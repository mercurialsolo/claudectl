use crate::session::ClaudeSession;
use crate::terminals;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleAction {
    Approve,
    Deny,
    Send,
    Terminate,
    /// Route summarized output from the current session to another session.
    Route {
        target_pid: u32,
    },
    /// Spawn a new Claude Code session with a derived prompt.
    Spawn {
        prompt: String,
        cwd: String,
    },
    /// Delegate work to an external agent by name.
    Delegate {
        agent: String,
        prompt: String,
    },
}

impl RuleAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "approve" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            "send" => Some(Self::Send),
            "terminate" | "kill" => Some(Self::Terminate),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
            Self::Send => "send",
            Self::Terminate => "terminate",
            Self::Route { .. } => "route",
            Self::Spawn { .. } => "spawn",
            Self::Delegate { .. } => "delegate",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutoRule {
    pub name: String,
    pub match_status: Vec<String>,
    pub match_tool: Vec<String>,
    pub match_command: Vec<String>,
    pub match_project: Vec<String>,
    pub match_cost_above: Option<f64>,
    pub match_last_error: Option<bool>,
    pub match_file_conflict: Option<bool>,
    pub action: RuleAction,
    pub message: Option<String>,
}

impl AutoRule {
    pub fn new(name: String, action: RuleAction) -> Self {
        Self {
            name,
            match_status: Vec::new(),
            match_tool: Vec::new(),
            match_command: Vec::new(),
            match_project: Vec::new(),
            match_cost_above: None,
            match_last_error: None,
            match_file_conflict: None,
            action,
            message: None,
        }
    }
}

/// Result of evaluating rules against a session.
#[derive(Debug, Clone)]
pub struct RuleMatch {
    pub rule_name: String,
    pub action: RuleAction,
    pub message: Option<String>,
}

/// Evaluate all rules against a session. Deny rules take precedence.
/// Among non-deny rules, first match in config order wins.
pub fn evaluate(rules: &[AutoRule], session: &ClaudeSession) -> Option<RuleMatch> {
    let mut first_non_deny: Option<RuleMatch> = None;

    for rule in rules {
        if !matches_rule(rule, session) {
            continue;
        }

        if rule.action == RuleAction::Deny {
            return Some(RuleMatch {
                rule_name: rule.name.clone(),
                action: RuleAction::Deny,
                message: rule.message.clone(),
            });
        }

        if first_non_deny.is_none() {
            first_non_deny = Some(RuleMatch {
                rule_name: rule.name.clone(),
                action: rule.action.clone(),
                message: rule.message.clone(),
            });
        }
    }

    first_non_deny
}

/// Check if all of a rule's conditions match the session.
/// Omitted conditions (empty vec / None) are treated as wildcards.
fn matches_rule(rule: &AutoRule, session: &ClaudeSession) -> bool {
    if !rule.match_status.is_empty() {
        let status_str = session.status.to_string().to_lowercase();
        let any_match = rule
            .match_status
            .iter()
            .any(|s| status_str == s.to_lowercase());
        if !any_match {
            return false;
        }
    }

    if !rule.match_tool.is_empty() {
        let tool = match &session.pending_tool_name {
            Some(t) => t.to_lowercase(),
            None => return false,
        };
        let any_match = rule.match_tool.iter().any(|t| tool == t.to_lowercase());
        if !any_match {
            return false;
        }
    }

    if !rule.match_command.is_empty() {
        let cmd = match &session.pending_tool_input {
            Some(c) => c.to_lowercase(),
            None => return false,
        };
        let any_match = rule
            .match_command
            .iter()
            .any(|pattern| cmd.contains(&pattern.to_lowercase()));
        if !any_match {
            return false;
        }
    }

    if !rule.match_project.is_empty() {
        let project = session.display_name().to_lowercase();
        let any_match = rule
            .match_project
            .iter()
            .any(|p| project.contains(&p.to_lowercase()));
        if !any_match {
            return false;
        }
    }

    if let Some(threshold) = rule.match_cost_above {
        if session.cost_usd <= threshold {
            return false;
        }
    }

    if let Some(expected) = rule.match_last_error {
        if session.last_tool_error != expected {
            return false;
        }
    }

    if let Some(expected) = rule.match_file_conflict {
        if session.has_file_conflict != expected {
            return false;
        }
    }

    true
}

/// Execute a rule action on a session. Returns a human-readable status message.
pub fn execute(result: &RuleMatch, session: &ClaudeSession) -> Result<String, String> {
    let name = session.display_name();
    match result.action {
        RuleAction::Approve => {
            terminals::approve_session(session)?;
            Ok(format!(
                "Rule '{}': approved {} ({})",
                result.rule_name,
                name,
                session.pending_tool_name.as_deref().unwrap_or("?")
            ))
        }
        RuleAction::Deny => Ok(format!(
            "Rule '{}': denied {} ({})",
            result.rule_name,
            name,
            session.pending_tool_name.as_deref().unwrap_or("?")
        )),
        RuleAction::Send => {
            let msg = result.message.as_deref().unwrap_or("continue");
            terminals::send_input(session, msg)?;
            Ok(format!(
                "Rule '{}': sent \"{}\" to {}",
                result.rule_name, msg, name
            ))
        }
        RuleAction::Terminate => {
            let pid = session.pid;
            let output = std::process::Command::new("kill")
                .arg(pid.to_string())
                .output()
                .map_err(|e| format!("kill failed: {e}"))?;
            if output.status.success() {
                Ok(format!("Rule '{}': terminated {}", result.rule_name, name))
            } else {
                Err(format!("Rule '{}': kill {} failed", result.rule_name, pid))
            }
        }
        RuleAction::Route { .. } => {
            // Route execution happens in the brain engine.
            Ok(format!(
                "Rule '{}': route queued for {}",
                result.rule_name, name
            ))
        }
        RuleAction::Spawn {
            ref prompt,
            ref cwd,
        } => match terminals::launch_session(cwd, Some(prompt), None) {
            Ok(msg) => Ok(format!(
                "Rule '{}': spawned new session for {} — {msg}",
                result.rule_name, name
            )),
            Err(e) => Err(format!(
                "Rule '{}': spawn failed for {}: {e}",
                result.rule_name, name
            )),
        },
        RuleAction::Delegate { ref agent, .. } => {
            // Delegate execution happens in the brain engine (needs agent registry
            // + output capture). This arm logs the delegation.
            Ok(format!(
                "Rule '{}': delegated to agent '{}' for {}",
                result.rule_name, agent, name
            ))
        }
    }
}

/// Execute a Route action: summarize source output and send to target session.
pub fn execute_route(
    source: &ClaudeSession,
    target: &ClaudeSession,
    summary: &str,
    rule_name: &str,
) -> Result<String, String> {
    let msg = format!("[From {}] {}", source.display_name(), summary);
    terminals::send_input(target, &msg)?;
    Ok(format!(
        "Rule '{}': routed summary from {} → {}",
        rule_name,
        source.display_name(),
        target.display_name(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ClaudeSession, RawSession, SessionStatus, TelemetryStatus};

    fn make_session() -> ClaudeSession {
        let raw = RawSession {
            pid: 100,
            session_id: "test".into(),
            cwd: "/tmp/my-project".into(),
            started_at: 0,
        };
        let mut s = ClaudeSession::from_raw(raw);
        s.status = SessionStatus::NeedsInput;
        s.telemetry_status = TelemetryStatus::Available;
        s.pending_tool_name = Some("Bash".into());
        s.pending_tool_input = Some("cargo test".into());
        s.cost_usd = 5.0;
        s
    }

    fn approve_rule(name: &str) -> AutoRule {
        AutoRule::new(name.into(), RuleAction::Approve)
    }

    fn deny_rule(name: &str) -> AutoRule {
        AutoRule::new(name.into(), RuleAction::Deny)
    }

    #[test]
    fn no_rules_returns_none() {
        let s = make_session();
        assert!(evaluate(&[], &s).is_none());
    }

    #[test]
    fn wildcard_rule_matches_any_session() {
        let s = make_session();
        let rules = vec![approve_rule("catch_all")];
        let m = evaluate(&rules, &s).unwrap();
        assert_eq!(m.action, RuleAction::Approve);
    }

    #[test]
    fn match_status_filters() {
        let mut s = make_session();
        s.status = SessionStatus::WaitingInput;

        let mut rule = approve_rule("only_needs_input");
        rule.match_status = vec!["Needs Input".into()];

        assert!(evaluate(&[rule.clone()], &s).is_none());

        s.status = SessionStatus::NeedsInput;
        assert!(evaluate(&[rule], &s).is_some());
    }

    #[test]
    fn match_tool_filters() {
        let s = make_session(); // pending_tool_name = "Bash"

        let mut rule = approve_rule("only_read");
        rule.match_tool = vec!["Read".into()];
        assert!(evaluate(&[rule], &s).is_none());

        let mut rule2 = approve_rule("bash_ok");
        rule2.match_tool = vec!["Bash".into()];
        assert!(evaluate(&[rule2], &s).is_some());
    }

    #[test]
    fn match_tool_case_insensitive() {
        let s = make_session();

        let mut rule = approve_rule("bash_lower");
        rule.match_tool = vec!["bash".into()];
        assert!(evaluate(&[rule], &s).is_some());
    }

    #[test]
    fn match_command_substring() {
        let s = make_session(); // pending_tool_input = "cargo test"

        let mut rule = deny_rule("deny_rm");
        rule.match_command = vec!["rm -rf".into()];
        assert!(evaluate(&[rule], &s).is_none());

        let mut rule2 = approve_rule("approve_cargo");
        rule2.match_command = vec!["cargo".into()];
        assert!(evaluate(&[rule2], &s).is_some());
    }

    #[test]
    fn match_project_substring() {
        let s = make_session(); // project_name = "my-project"

        let mut rule = approve_rule("my_proj");
        rule.match_project = vec!["my-project".into()];
        assert!(evaluate(&[rule], &s).is_some());

        let mut rule2 = approve_rule("other");
        rule2.match_project = vec!["other-project".into()];
        assert!(evaluate(&[rule2], &s).is_none());
    }

    #[test]
    fn match_cost_above() {
        let s = make_session(); // cost = 5.0

        let mut rule = approve_rule("cheap");
        rule.match_cost_above = Some(10.0);
        assert!(evaluate(&[rule], &s).is_none());

        let mut rule2 = approve_rule("expensive");
        rule2.match_cost_above = Some(3.0);
        assert!(evaluate(&[rule2], &s).is_some());
    }

    #[test]
    fn match_last_error() {
        let mut s = make_session();
        s.last_tool_error = true;

        let mut rule = approve_rule("on_error");
        rule.match_last_error = Some(true);
        assert!(evaluate(&[rule], &s).is_some());

        let mut rule2 = approve_rule("no_error");
        rule2.match_last_error = Some(false);
        assert!(evaluate(&[rule2], &s).is_none());
    }

    #[test]
    fn match_file_conflict() {
        let mut s = make_session();
        s.has_file_conflict = true;

        let mut rule = deny_rule("deny_conflict");
        rule.match_file_conflict = Some(true);
        assert!(evaluate(&[rule], &s).is_some());

        let mut rule2 = approve_rule("no_conflict");
        rule2.match_file_conflict = Some(false);
        assert!(evaluate(&[rule2], &s).is_none());
    }

    #[test]
    fn match_file_conflict_false_matches_clean() {
        let s = make_session(); // has_file_conflict defaults to false

        let mut rule = approve_rule("clean");
        rule.match_file_conflict = Some(false);
        assert!(evaluate(&[rule], &s).is_some());
    }

    #[test]
    fn deny_takes_precedence() {
        let s = make_session();

        let approve = approve_rule("approve_all");
        let deny = deny_rule("deny_all");

        // Approve first in config order, deny second — deny still wins
        let rules = vec![approve, deny];
        let m = evaluate(&rules, &s).unwrap();
        assert_eq!(m.action, RuleAction::Deny);
    }

    #[test]
    fn first_non_deny_wins() {
        let s = make_session();

        let mut r1 = AutoRule::new("send_continue".into(), RuleAction::Send);
        r1.message = Some("keep going".into());

        let r2 = approve_rule("approve_all");

        let rules = vec![r1, r2];
        let m = evaluate(&rules, &s).unwrap();
        assert_eq!(m.action, RuleAction::Send);
        assert_eq!(m.message.as_deref(), Some("keep going"));
    }

    #[test]
    fn multiple_conditions_are_and() {
        let s = make_session(); // Bash + "cargo test" + cost 5.0

        let mut rule = approve_rule("bash_cargo_cheap");
        rule.match_tool = vec!["Bash".into()];
        rule.match_command = vec!["cargo".into()];
        rule.match_cost_above = Some(10.0); // cost 5.0 does NOT exceed 10.0
        assert!(evaluate(&[rule], &s).is_none());
    }

    #[test]
    fn no_pending_tool_fails_tool_match() {
        let mut s = make_session();
        s.pending_tool_name = None;

        let mut rule = approve_rule("bash");
        rule.match_tool = vec!["Bash".into()];
        assert!(evaluate(&[rule], &s).is_none());
    }
}

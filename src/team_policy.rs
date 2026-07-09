//! Team policy — checked-in guardrails that individual config can't soften.
//!
//! `.claudectl.toml` and `~/.config/claudectl/config.toml` are *user* config:
//! a developer can edit or delete any deny rule in them. That's fine for
//! personal preferences, but a team lead needs guardrails that hold across the
//! fleet regardless of what each dev has locally — "never force-push to main",
//! "this repo may not touch prod".
//!
//! This module reads a `.claudectl/policy.toml` file **committed to the repo**.
//! Because it comes from version control rather than a user's home dir, and is
//! evaluated at highest precedence in the gate, its denies can't be removed by
//! editing local config. Change the guardrails by changing the checked-in file
//! — which is a reviewed commit, not a silent local edit.
//!
//! Slice 1 covers forbidden commands and tools. Spend caps, required verifiers,
//! and a brain-lite floor are planned follow-ups; the file format leaves room
//! for them under their own sections.
//!
//! ```toml
//! # .claudectl/policy.toml — team guardrails (checked in)
//! [deny]
//! commands = ["git push --force", "kubectl delete", "rm -rf /"]
//! tools    = ["WebFetch"]
//! ```

use std::path::{Path, PathBuf};

use crate::rules::{AutoRule, RuleAction};

/// The policy directory / file, relative to a repo root.
const POLICY_RELPATH: &str = ".claudectl/policy.toml";

/// Parsed team guardrails plus where they were loaded from (for `policy` and
/// `doctor` to attribute the source).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamPolicy {
    /// Command substrings that are always denied (case-insensitive `contains`,
    /// matching the auto-rule engine's command semantics).
    pub deny_commands: Vec<String>,
    /// Tool names that are always denied (exact, case-insensitive).
    pub deny_tools: Vec<String>,
    /// Absolute path the policy was read from. Empty for a policy built in
    /// memory (tests).
    pub source: PathBuf,
}

impl TeamPolicy {
    /// Whether this policy actually constrains anything.
    pub fn is_empty(&self) -> bool {
        self.deny_commands.is_empty() && self.deny_tools.is_empty()
    }

    /// Convert the guardrails into deny `AutoRule`s the existing `rules::evaluate`
    /// can match — so policy reuses the same command/tool matching as everything
    /// else, and deny-first precedence is automatic. Rule names are reserved
    /// (`policy:*`) so they can't collide with or be shadowed by user rules.
    pub fn to_deny_rules(&self) -> Vec<AutoRule> {
        let mut rules = Vec::new();
        for (i, cmd) in self.deny_commands.iter().enumerate() {
            let mut rule = AutoRule::new(format!("policy:cmd:{i}"), RuleAction::Deny);
            rule.match_command = vec![cmd.clone()];
            rule.message = Some(format!("blocked by team policy: command matches '{cmd}'"));
            rules.push(rule);
        }
        for (i, tool) in self.deny_tools.iter().enumerate() {
            let mut rule = AutoRule::new(format!("policy:tool:{i}"), RuleAction::Deny);
            rule.match_tool = vec![tool.clone()];
            rule.message = Some(format!(
                "blocked by team policy: tool '{tool}' is not permitted"
            ));
            rules.push(rule);
        }
        rules
    }
}

/// Find the nearest `.claudectl/policy.toml` at or above `start`, stopping at
/// the repo root (the directory containing `.git`) or the filesystem root. The
/// gate runs in whatever cwd a tool call fires from — often a subdirectory — so
/// discovery walks up rather than checking only cwd.
pub fn find_policy_file(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(POLICY_RELPATH);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Stop once we've checked the repo root, so we never wander above the
        // project into a parent repo or the home dir.
        if d.join(".git").exists() {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Parse the `[deny]` section of a policy file. Hand-rolled to match the
/// project's no-`toml`-crate house style; unknown sections and keys are ignored
/// so the format can grow without breaking older binaries.
pub fn parse(content: &str) -> TeamPolicy {
    let mut policy = TeamPolicy::default();
    let mut section = String::new();
    for raw in content.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if section == "deny" {
            match key {
                "commands" => policy.deny_commands = parse_string_array(value),
                "tools" => policy.deny_tools = parse_string_array(value),
                _ => {}
            }
        }
    }
    policy
}

/// Load the team policy for the current working directory, if one exists.
pub fn load() -> Option<TeamPolicy> {
    let cwd = std::env::current_dir().ok()?;
    load_from(&cwd)
}

/// Load the team policy governing `start`, if a policy file is found at or above it.
pub fn load_from(start: &Path) -> Option<TeamPolicy> {
    let path = find_policy_file(start)?;
    let content = std::fs::read_to_string(&path).ok()?;
    let mut policy = parse(&content);
    policy.source = path;
    Some(policy)
}

/// Drop an inline `#` comment, respecting `#` inside a quoted string so a
/// command pattern like `"git config core.hooksPath"` survives (no `#` there,
/// but a value could legitimately contain one).
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Parse a single-line TOML string array: `["a", "b"]` → `["a", "b"]`.
fn parse_string_array(value: &str) -> Vec<String> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ClaudeSession, RawSession, SessionStatus};

    fn session_with(tool: &str, command: &str) -> ClaudeSession {
        let mut s = ClaudeSession::from_raw(RawSession {
            pid: 1,
            session_id: "t".into(),
            cwd: ".".into(),
            started_at: 0,
        });
        s.status = SessionStatus::NeedsInput;
        s.pending_tool_name = Some(tool.into());
        s.pending_tool_input = Some(command.into());
        s
    }

    #[test]
    fn parse_extracts_deny_commands_and_tools() {
        let toml = r#"
            [deny]
            commands = ["git push --force", "rm -rf /"]
            tools = ["WebFetch"]
        "#;
        let p = parse(toml);
        assert_eq!(p.deny_commands, ["git push --force", "rm -rf /"]);
        assert_eq!(p.deny_tools, ["WebFetch"]);
    }

    #[test]
    fn parse_ignores_unknown_sections_and_comments() {
        let toml = r#"
            # team guardrails
            [limits]          # not handled yet
            daily_usd = 50
            [deny]
            commands = ["kubectl delete"]  # no prod deletes
        "#;
        let p = parse(toml);
        assert_eq!(p.deny_commands, ["kubectl delete"]);
        assert!(p.deny_tools.is_empty());
    }

    #[test]
    fn empty_policy_is_empty() {
        assert!(parse("").is_empty());
        assert!(parse("[deny]\ncommands = []").is_empty());
    }

    #[test]
    fn deny_rules_match_command_via_engine() {
        let policy = TeamPolicy {
            deny_commands: vec!["git push --force".into()],
            ..Default::default()
        };
        let rules = policy.to_deny_rules();
        // Substring + case-insensitive, inherited from the rule engine.
        let hit = crate::rules::evaluate(
            &rules,
            &session_with("Bash", "git push --force origin main"),
        );
        assert_eq!(hit.unwrap().action, RuleAction::Deny);
        // A benign command doesn't match.
        assert!(crate::rules::evaluate(&rules, &session_with("Bash", "git status")).is_none());
    }

    #[test]
    fn deny_rules_match_tool_exactly() {
        let policy = TeamPolicy {
            deny_tools: vec!["WebFetch".into()],
            ..Default::default()
        };
        let rules = policy.to_deny_rules();
        assert!(crate::rules::evaluate(&rules, &session_with("webfetch", "https://x")).is_some());
        assert!(crate::rules::evaluate(&rules, &session_with("Read", "file")).is_none());
    }

    #[test]
    fn find_walks_up_to_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".claudectl")).unwrap();
        std::fs::write(root.join(POLICY_RELPATH), "[deny]\ncommands=[\"x\"]").unwrap();
        let deep = root.join("crates/foo/src");
        std::fs::create_dir_all(&deep).unwrap();

        let found = find_policy_file(&deep).expect("should find policy from a subdir");
        assert_eq!(found, root.join(POLICY_RELPATH));
    }

    #[test]
    fn find_stops_at_repo_root_without_policy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let deep = root.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(find_policy_file(&deep).is_none());
    }
}

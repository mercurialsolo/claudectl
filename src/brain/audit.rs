//! Exportable decision audit log (#372).
//!
//! Turns the local `decisions.jsonl` history into a readable timeline a
//! developer can paste into a PR or hand to a teammate to explain *what the
//! brain did and why*. No new storage — this reads `read_all_decisions()` and
//! renders it. Two formats: human-readable Markdown (default) and JSON (the
//! `DecisionSummary` projection, which already serialises the `why` line).

use crate::brain::decisions::{DecisionRecord, read_all_decisions};

/// Render the decision audit log in `format` ("md" or "json"), optionally
/// filtered to one `project` and/or one session `pid`.
pub fn export(format: &str, project: Option<&str>, pid: Option<u32>) -> Result<String, String> {
    let mut records = read_all_decisions();
    if let Some(proj) = project {
        records.retain(|r| r.project == proj);
    }
    if let Some(p) = pid {
        records.retain(|r| r.pid == p);
    }

    match format {
        "json" => render_json(&records),
        "md" | "markdown" => Ok(render_markdown(&records, project, pid)),
        other => Err(format!(
            "unknown export format '{other}' (expected 'md' or 'json')"
        )),
    }
}

/// Strip the quote characters an old serde `Value::to_string()` left around
/// the stored timestamp so the export reads cleanly.
fn clean_ts(ts: &str) -> &str {
    ts.trim_matches('"')
}

fn render_json(records: &[DecisionRecord]) -> Result<String, String> {
    let summaries: Vec<claudectl_core::runtime::DecisionSummary> =
        records.iter().map(Into::into).collect();
    serde_json::to_string_pretty(&summaries).map_err(|e| format!("serialise audit log: {e}"))
}

fn render_markdown(records: &[DecisionRecord], project: Option<&str>, pid: Option<u32>) -> String {
    let mut out = String::new();
    out.push_str("# Brain decision audit log\n\n");

    let mut scope = Vec::new();
    if let Some(proj) = project {
        scope.push(format!("project `{proj}`"));
    }
    if let Some(p) = pid {
        scope.push(format!("session `{p}`"));
    }
    if scope.is_empty() {
        out.push_str("_All sessions._\n\n");
    } else {
        out.push_str(&format!("_Scope: {}._\n\n", scope.join(", ")));
    }

    if records.is_empty() {
        out.push_str("No decisions recorded yet.\n");
        return out;
    }

    let total = records.len();
    let agreed = records.iter().filter(|r| r.is_positive()).count();
    out.push_str(&format!(
        "**{total}** decision(s) · **{agreed}** auto/accepted · **{}** overridden or rejected\n\n",
        total - agreed
    ));

    for r in records {
        let action = if r.brain_action.is_empty() {
            "observation".to_string()
        } else {
            format!("{} ({:.0}%)", r.brain_action, r.brain_confidence * 100.0)
        };
        out.push_str(&format!(
            "## {} — {}\n\n",
            clean_ts(&r.timestamp),
            r.project
        ));
        out.push_str(&format!("- **decision:** {action}\n"));
        out.push_str(&format!("- **why:** {}\n", r.why()));
        if let Some(tool) = &r.tool {
            match &r.command {
                Some(cmd) if !cmd.is_empty() => {
                    out.push_str(&format!("- **tool:** `{tool}` — `{cmd}`\n"))
                }
                _ => out.push_str(&format!("- **tool:** `{tool}`\n")),
            }
        }
        out.push_str(&format!("- **outcome:** {}\n", r.user_action));
        if let Some(reason) = &r.override_reason {
            out.push_str(&format!("- **override reason:** {reason}\n"));
        }
        if !r.brain_reasoning.is_empty() {
            out.push_str(&format!("- **reasoning:** {}\n", r.brain_reasoning));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::decisions::DecisionType;

    fn rec(project: &str, pid: u32, action: &str, user: &str) -> DecisionRecord {
        DecisionRecord {
            timestamp: "\"1780000000\"".into(),
            pid,
            project: project.into(),
            tool: Some("Bash".into()),
            command: Some("cargo test".into()),
            brain_action: action.into(),
            brain_confidence: 0.9,
            brain_reasoning: "safe command".into(),
            user_action: user.into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: Some("dec_1".into()),
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
            decision_source: Some("llm".into()),
            rule_name: None,
            few_shot_ids: vec!["dec_0".into()],
        }
    }

    #[test]
    fn markdown_includes_why_and_counts() {
        let records = vec![
            rec("acme", 1, "approve", "accept"),
            rec("acme", 1, "deny", "reject"),
        ];
        let md = render_markdown(&records, Some("acme"), None);
        assert!(md.contains("Brain decision audit log"));
        assert!(md.contains("**2** decision(s)"));
        assert!(md.contains("**why:** via llm"));
        assert!(md.contains("1 past example(s)"));
        // Timestamp quotes stripped.
        assert!(md.contains("## 1780000000 — acme"));
        assert!(!md.contains("\"1780000000\""));
    }

    #[test]
    fn json_is_valid_array() {
        let records = vec![rec("acme", 1, "approve", "accept")];
        let json = render_json(&records).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
        assert!(parsed[0]["why"].as_str().unwrap().contains("via llm"));
    }

    #[test]
    fn empty_scope_renders_placeholder() {
        let md = render_markdown(&[], None, Some(42));
        assert!(md.contains("No decisions recorded yet."));
        assert!(md.contains("session `42`"));
    }

    #[test]
    fn unknown_format_errors() {
        assert!(export("csv", None, None).is_err());
    }
}

// Remote task delegation: context payloads and message builders.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{MessageType, RelayMessage, epoch_ms, gen_msg_id};

/// Maximum size for relevant_files payload (50 KB).
const MAX_FILES_PAYLOAD: usize = 50 * 1024;

// ────────────────────────────────────────────────────────────────────────────
// Context sent with a delegated task
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DelegationContext {
    /// Git remote URL for the project (worker clones/pulls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote: Option<String>,
    /// Branch or tag to check out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Commit hash for exact reproducibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// Relevant file snippets (path -> content), max 50 KB total.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub relevant_files: HashMap<String, String>,
    /// Summary of the controller's brain context for this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brain_context: Option<BrainContextSummary>,
    /// What this task blocks / is blocked by.
    #[serde(default)]
    pub dependency_graph: DependencyGraph,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainContextSummary {
    pub project_preferences: String,
    #[serde(default)]
    pub recent_insights: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencyGraph {
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

impl DelegationContext {
    /// Total size of relevant_files content.
    pub fn files_payload_size(&self) -> usize {
        self.relevant_files.values().map(|v| v.len()).sum()
    }

    /// Validate that the context is within size limits.
    pub fn validate(&self) -> Result<(), String> {
        let size = self.files_payload_size();
        if size > MAX_FILES_PAYLOAD {
            return Err(format!(
                "relevant_files payload too large: {size} bytes (max {MAX_FILES_PAYLOAD})"
            ));
        }
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Task stats reported by the worker
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskStats {
    #[serde(default)]
    pub tokens_used: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub context_pct: u8,
    #[serde(default)]
    pub files_modified: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Message builders
// ────────────────────────────────────────────────────────────────────────────

/// Build a DelegateTask message.
pub fn build_delegate_message(
    task_id: &str,
    prompt: &str,
    cwd: Option<&str>,
    context: &DelegationContext,
    identity: &str,
) -> Result<RelayMessage, String> {
    context.validate()?;

    Ok(RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::DelegateTask,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "task_id": task_id,
            "prompt": prompt,
            "cwd": cwd,
            "context": context,
        }),
    })
}

/// Parse a DelegateTask message payload.
pub fn parse_delegate_message(
    msg: &RelayMessage,
) -> Result<(String, String, Option<String>, DelegationContext), String> {
    let task_id = msg
        .payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("missing task_id")?
        .to_string();
    let prompt = msg
        .payload
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("missing prompt")?
        .to_string();
    let cwd = msg
        .payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let context: DelegationContext = msg
        .payload
        .get("context")
        .map(|v| serde_json::from_value(v.clone()).unwrap_or_default())
        .unwrap_or_default();
    Ok((task_id, prompt, cwd, context))
}

/// Build a TaskStatus message (periodic update from worker).
pub fn build_status_message(
    task_id: &str,
    state: &str,
    stats: &TaskStats,
    identity: &str,
) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::TaskStatus,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "task_id": task_id,
            "state": state,
            "stats": stats,
        }),
    }
}

/// What a delegated run cost and produced, as the worker reports it (#493).
///
/// The older two-builder split — one for success, one for failure — forced the
/// cost and token arguments to be positional alongside five others, and had no
/// room for the model or the usage breakdown. One struct names them instead.
pub struct TaskReport {
    pub failed: bool,
    pub summary: String,
    pub total_cost_usd: f64,
    pub total_tokens: u64,
    /// Which model ran it. Otherwise unknowable from the delegating side.
    pub model: Option<String>,
    /// The `usage` object verbatim, so the token breakdown survives the trip.
    pub usage: serde_json::Value,
}

/// Build the TaskHandoff for a finished task, either way it went.
pub fn build_report_message(task_id: &str, report: &TaskReport, identity: &str) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::TaskHandoff,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "task_id": task_id,
            "state": if report.failed { "failed" } else { "completed" },
            "summary": report.summary,
            "artifacts": [],
            "total_cost_usd": report.total_cost_usd,
            "total_tokens": report.total_tokens,
            "model": report.model,
            "usage": report.usage,
        }),
    }
}

/// Build a TaskInterrupt message (controller to worker).
pub fn build_interrupt_message(
    task_id: &str,
    interrupt_type: &str,
    reason: &str,
    identity: &str,
) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::TaskInterrupt,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "task_id": task_id,
            "interrupt_type": interrupt_type,
            "reason": reason,
        }),
    }
}

/// Parse a TaskInterrupt message payload.
pub fn parse_interrupt_message(msg: &RelayMessage) -> Result<(String, String, String), String> {
    let task_id = msg
        .payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("missing task_id")?
        .to_string();
    let interrupt_type = msg
        .payload
        .get("interrupt_type")
        .and_then(|v| v.as_str())
        .ok_or("missing interrupt_type")?
        .to_string();
    let reason = msg
        .payload
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok((task_id, interrupt_type, reason))
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_message_roundtrip() {
        let ctx = DelegationContext {
            git_remote: Some("git@github.com:team/project.git".into()),
            git_ref: Some("feat/auth".into()),
            ..Default::default()
        };
        let msg = build_delegate_message("t_1", "Fix the tests", Some("/project"), &ctx, "peer-a")
            .unwrap();
        assert_eq!(msg.msg_type, MessageType::DelegateTask);

        let (task_id, prompt, cwd, parsed_ctx) = parse_delegate_message(&msg).unwrap();
        assert_eq!(task_id, "t_1");
        assert_eq!(prompt, "Fix the tests");
        assert_eq!(cwd.as_deref(), Some("/project"));
        assert_eq!(
            parsed_ctx.git_remote.as_deref(),
            Some("git@github.com:team/project.git")
        );
        assert_eq!(parsed_ctx.git_ref.as_deref(), Some("feat/auth"));
    }

    #[test]
    fn delegate_rejects_oversized_files() {
        let mut files = HashMap::new();
        files.insert("big.txt".into(), "x".repeat(60_000));
        let ctx = DelegationContext {
            relevant_files: files,
            ..Default::default()
        };
        assert!(ctx.validate().is_err());
    }

    #[test]
    fn status_message_roundtrip() {
        let stats = TaskStats {
            tokens_used: 8000,
            cost_usd: 0.42,
            context_pct: 35,
            files_modified: vec!["src/auth.rs".into()],
            ..Default::default()
        };
        let msg = build_status_message("t_1", "running", &stats, "peer-b");
        assert_eq!(msg.msg_type, MessageType::TaskStatus);

        // Through the consumer the serve loop uses, rather than a parser that
        // existed only to match this builder (#465).
        let (task_id, report) = crate::relay::tasks::parse_report(false, &msg.payload)
            .expect("the serve loop can read a status message");
        assert_eq!(task_id, "t_1");
        match report {
            crate::relay::tasks::Report::Progress { state, stats } => {
                assert_eq!(state, "running");
                assert_eq!(
                    stats.get("tokens_used").and_then(|v| v.as_u64()),
                    Some(8000)
                );
                assert_eq!(stats.get("context_pct").and_then(|v| v.as_u64()), Some(35));
            }
            other => panic!("a TaskStatus should read as progress, got {other:?}"),
        }
    }

    fn report(failed: bool, summary: &str) -> TaskReport {
        TaskReport {
            failed,
            summary: summary.to_string(),
            total_cost_usd: 1.23,
            total_tokens: 50000,
            model: Some("claude-opus-5".into()),
            usage: serde_json::json!({"input_tokens": 10, "output_tokens": 20}),
        }
    }

    /// A finished task, read back by the arm that records it.
    #[test]
    fn a_completed_report_reaches_the_ledger_intact() {
        let msg = build_report_message("t_1", &report(false, "Tests pass"), "peer-b");
        assert_eq!(msg.msg_type, MessageType::TaskHandoff);

        let (task_id, parsed) = crate::relay::tasks::parse_report(true, &msg.payload)
            .expect("the serve loop can read a handoff");
        assert_eq!(task_id, "t_1");
        match parsed {
            crate::relay::tasks::Report::Settled {
                state,
                summary,
                total_cost_usd,
                total_tokens,
                model,
                ..
            } => {
                assert_eq!(state, "completed");
                assert_eq!(summary, "Tests pass");
                assert_eq!(total_cost_usd, 1.23);
                assert_eq!(total_tokens, 50000);
                assert_eq!(model.as_deref(), Some("claude-opus-5"));
            }
            other => panic!("a TaskHandoff should read as settled, got {other:?}"),
        }
    }

    #[test]
    fn interrupt_message_roundtrip() {
        let msg = build_interrupt_message("t_1", "nudge", "dependency resolved", "peer-a");
        let (task_id, itype, reason) = parse_interrupt_message(&msg).unwrap();
        assert_eq!(task_id, "t_1");
        assert_eq!(itype, "nudge");
        assert_eq!(reason, "dependency resolved");
    }

    /// A failed task says so, and still carries what it spent.
    #[test]
    fn a_failed_report_is_settled_as_failed() {
        let msg = build_report_message("t_2", &report(true, "exit code 1"), "peer-b");
        let (_, parsed) = crate::relay::tasks::parse_report(true, &msg.payload).unwrap();
        match parsed {
            crate::relay::tasks::Report::Settled {
                state,
                summary,
                total_cost_usd,
                ..
            } => {
                assert_eq!(state, "failed");
                assert_eq!(summary, "exit code 1");
                assert_eq!(total_cost_usd, 1.23, "a failure still reports its cost");
            }
            other => panic!("expected settled, got {other:?}"),
        }
    }

    #[test]
    fn default_context_validates() {
        let ctx = DelegationContext::default();
        assert!(ctx.validate().is_ok());
    }
}

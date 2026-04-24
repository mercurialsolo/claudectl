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

/// Parse a TaskStatus message payload.
pub fn parse_status_message(msg: &RelayMessage) -> Result<(String, String, TaskStats), String> {
    let task_id = msg
        .payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("missing task_id")?
        .to_string();
    let state = msg
        .payload
        .get("state")
        .and_then(|v| v.as_str())
        .ok_or("missing state")?
        .to_string();
    let stats: TaskStats = msg
        .payload
        .get("stats")
        .map(|v| serde_json::from_value(v.clone()).unwrap_or_default())
        .unwrap_or_default();
    Ok((task_id, state, stats))
}

/// Build a TaskHandoff message (worker completed the task).
pub fn build_handoff_message(
    task_id: &str,
    summary: &str,
    artifacts: &[String],
    git_ref: Option<&str>,
    total_cost_usd: f64,
    total_tokens: u64,
    identity: &str,
) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::TaskHandoff,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "task_id": task_id,
            "state": "completed",
            "summary": summary,
            "artifacts": artifacts,
            "git_ref": git_ref,
            "total_cost_usd": total_cost_usd,
            "total_tokens": total_tokens,
        }),
    }
}

/// Build a TaskHandoff for a failed task.
pub fn build_failure_message(
    task_id: &str,
    reason: &str,
    total_cost_usd: f64,
    total_tokens: u64,
    identity: &str,
) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::TaskHandoff,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "task_id": task_id,
            "state": "failed",
            "summary": reason,
            "artifacts": [],
            "total_cost_usd": total_cost_usd,
            "total_tokens": total_tokens,
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
        let (task_id, state, parsed_stats) = parse_status_message(&msg).unwrap();
        assert_eq!(task_id, "t_1");
        assert_eq!(state, "running");
        assert_eq!(parsed_stats.tokens_used, 8000);
        assert_eq!(parsed_stats.context_pct, 35);
    }

    #[test]
    fn handoff_message_fields() {
        let msg = build_handoff_message(
            "t_1",
            "Tests pass",
            &["src/auth.rs".into()],
            Some("feat/done"),
            1.23,
            50000,
            "peer-b",
        );
        assert_eq!(msg.msg_type, MessageType::TaskHandoff);
        assert_eq!(
            msg.payload.get("summary").and_then(|v| v.as_str()),
            Some("Tests pass")
        );
        assert_eq!(
            msg.payload.get("git_ref").and_then(|v| v.as_str()),
            Some("feat/done")
        );
    }

    #[test]
    fn interrupt_message_roundtrip() {
        let msg = build_interrupt_message("t_1", "nudge", "dependency resolved", "peer-a");
        let (task_id, itype, reason) = parse_interrupt_message(&msg).unwrap();
        assert_eq!(task_id, "t_1");
        assert_eq!(itype, "nudge");
        assert_eq!(reason, "dependency resolved");
    }

    #[test]
    fn failure_message_fields() {
        let msg = build_failure_message("t_2", "exit code 1", 0.15, 3000, "peer-b");
        assert_eq!(
            msg.payload.get("state").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert_eq!(
            msg.payload.get("summary").and_then(|v| v.as_str()),
            Some("exit code 1")
        );
    }

    #[test]
    fn default_context_validates() {
        let ctx = DelegationContext::default();
        assert!(ctx.validate().is_ok());
    }
}

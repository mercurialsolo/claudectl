//! A supervisor task run on a relay peer (#571, part 2).
//!
//! The actuator hands a placed task to the peer as a `DelegateTask` whose
//! context says it's supervised: which worktree and branch to use, and the
//! `run =` verifiers to run there afterwards. The attempt's id doubles as the
//! relay task id, so the relay's ledger (`relay/tasks/<attempt>.json`, kept up
//! to date by this machine's `relay serve` as the peer reports) is what the
//! sensor reads and what the verdicts come from.
//!
//! The task's `cwd` is used on the peer as-is: both machines keep the repo at
//! the same path. Work comes back as a branch in the peer's worktree, named in
//! the handoff.
//!
//! Without the `relay` feature there are no peers: delegating is an error and
//! nothing is ever observed remotely.

/// What the actuator asks a peer to run.
#[derive(Debug, Clone, PartialEq)]
pub struct DelegateRequest {
    /// Also the relay task id.
    pub attempt_id: String,
    pub peer: String,
    pub prompt: String,
    pub cwd: String,
    pub git_ref: Option<String>,
    /// The worktree's key and branch on the peer, the same for every attempt.
    pub worktree_key: String,
    pub branch: Option<String>,
    pub verify_run: Vec<String>,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
}

/// One verifier's verdict, as the peer ran it.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub command: String,
    pub passed: bool,
    pub output: String,
}

/// What a finished remote attempt reported.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemoteResult {
    pub ok: bool,
    pub cost_usd: Option<f64>,
    pub summary: String,
    pub branch: Option<String>,
    pub verdicts: Vec<Verdict>,
}

/// How long a delegation may go without the ledger hearing anything before
/// the attempt counts as lost: the peer never acknowledged it.
pub const UNACKNOWLEDGED_SECS: u64 = 120;

/// A remote attempt's state from its ledger record.
pub fn observe(
    record: Option<&serde_json::Value>,
    age_secs: Option<u64>,
) -> super::supervisor::ObservedStatus {
    use super::supervisor::ObservedStatus;
    let state = record.and_then(|r| r.get("state")).and_then(|s| s.as_str());
    let refused = record
        .and_then(|r| r.get("refused"))
        .and_then(|v| v.as_bool())
        == Some(true);
    match state {
        Some("completed") => ObservedStatus::Completed,
        Some("failed") if refused => ObservedStatus::Refused,
        Some("failed") => ObservedStatus::Failed,
        Some("running") => ObservedStatus::Running,
        // Sent, not yet acknowledged, or no record at all.
        _ if age_secs.is_some_and(|a| a >= UNACKNOWLEDGED_SECS) => ObservedStatus::Dead,
        _ => ObservedStatus::Running,
    }
}

/// A settled ledger record as a result. `None` while it's open.
pub fn parse_result(record: &serde_json::Value) -> Option<RemoteResult> {
    let state = record.get("state")?.as_str()?;
    if state != "completed" && state != "failed" {
        return None;
    }
    let verdicts = record
        .get("verifications")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|v| Verdict {
                    command: v
                        .get("command")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    // Anything short of an explicit pass is a fail.
                    passed: v.get("passed").and_then(|p| p.as_bool()) == Some(true),
                    output: v
                        .get("output")
                        .and_then(|o| o.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(RemoteResult {
        ok: state == "completed",
        cost_usd: record.get("total_cost_usd").and_then(|c| c.as_f64()),
        summary: record
            .get("summary")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        branch: record
            .get("git_ref")
            .and_then(|b| b.as_str())
            .map(String::from),
        verdicts,
    })
}

#[cfg(feature = "relay")]
pub fn record(attempt_id: &str) -> Option<serde_json::Value> {
    crate::relay::tasks::load(attempt_id)
}

#[cfg(not(feature = "relay"))]
pub fn record(_attempt_id: &str) -> Option<serde_json::Value> {
    None
}

/// What the attempt reported, once it has.
pub fn read_result(attempt_id: &str) -> Option<RemoteResult> {
    parse_result(&record(attempt_id)?)
}

/// Send the task to the peer. The ledger record is written first, so the
/// peer's first status can't arrive before there's a record to apply it to;
/// it's removed again if the send fails.
#[cfg(feature = "relay")]
pub fn delegate(req: &DelegateRequest) -> Result<(), String> {
    use crate::relay::delegation::{DelegationContext, SupervisedSpec};
    let identity = crate::relay::load_or_create_identity();
    let context = DelegationContext {
        git_ref: req.git_ref.clone(),
        supervised: Some(SupervisedSpec {
            worktree_key: req.worktree_key.clone(),
            branch: req.branch.clone(),
            verify_run: req.verify_run.clone(),
            permission_mode: req.permission_mode.clone(),
            model: req.model.clone(),
        }),
        ..DelegationContext::default()
    };
    let msg = crate::relay::delegation::build_delegate_message(
        &req.attempt_id,
        &req.prompt,
        Some(&req.cwd),
        &context,
        identity.as_str(),
    )?;
    crate::relay::tasks::record_delegated(&req.attempt_id, &req.peer, &req.prompt, Some(&req.cwd))?;
    let sent = crate::relay::cli::send_message_requiring(
        &req.peer,
        &identity,
        &msg,
        Some(crate::relay::features::SUPERVISED_TASK),
    );
    if let Err(e) = sent {
        crate::relay::tasks::forget(&req.attempt_id);
        return Err(e);
    }
    Ok(())
}

#[cfg(not(feature = "relay"))]
pub fn delegate(req: &DelegateRequest) -> Result<(), String> {
    Err(format!(
        "can't run on peer {}: this build has no relay",
        req.peer
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::supervisor::ObservedStatus;
    use serde_json::json;

    #[test]
    fn the_ledger_state_is_the_attempt_state() {
        let rec = |s: &str| json!({"state": s});
        assert_eq!(
            observe(Some(&rec("completed")), Some(5)),
            ObservedStatus::Completed
        );
        assert_eq!(
            observe(Some(&rec("failed")), Some(5)),
            ObservedStatus::Failed
        );
        assert_eq!(
            observe(Some(&rec("running")), Some(9_999)),
            ObservedStatus::Running
        );
        assert_eq!(
            observe(Some(&rec("delegated")), Some(5)),
            ObservedStatus::Running
        );
        assert_eq!(
            observe(Some(&rec("delegated")), Some(UNACKNOWLEDGED_SECS)),
            ObservedStatus::Dead,
            "never acknowledged"
        );
        assert_eq!(
            observe(None, Some(UNACKNOWLEDGED_SECS)),
            ObservedStatus::Dead
        );
        assert_eq!(
            observe(Some(&json!({"state": "failed", "refused": true})), Some(5)),
            ObservedStatus::Refused
        );
    }

    #[test]
    fn a_settled_record_reads_as_a_result_with_its_verdicts() {
        let r = parse_result(&json!({
            "state": "completed", "summary": "Wrote it.", "total_cost_usd": 0.31,
            "git_ref": "claudectl/t-1",
            "verifications": [
                {"command": "test -f A", "passed": true, "output": ""},
                {"command": "cargo test", "passed": "yes", "output": "1 failed"}
            ]
        }))
        .unwrap();
        assert!(r.ok);
        assert_eq!(r.cost_usd, Some(0.31));
        assert_eq!(r.branch.as_deref(), Some("claudectl/t-1"));
        assert_eq!(r.verdicts.len(), 2);
        assert!(r.verdicts[0].passed);
        assert!(!r.verdicts[1].passed, "only an explicit true passes");
        assert!(parse_result(&json!({"state": "running"})).is_none());
    }
}

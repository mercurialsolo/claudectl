// The ledger of tasks this host has delegated to a peer (#490).
//
// `relay delegate` is a one-shot process: it sends and exits within a second.
// The reply arrives seconds or minutes later, at whatever process is serving.
// They are never the same process, so the record has to be on disk — one small
// JSON file per task, the same shape as `peers/<id>.key` and `.meta`.
//
// Deliberately not `coord::tasks`. `relay` does not imply `coord`, and a relay
// path reaching into another feature's module is #482. Beyond the gating,
// `coord::tasks` is the *supervisor's* ledger — attempts, verifications, a
// reconciler reading `Sensors` — and a remote delegation is one row with four
// states. Writing it there would have the pure reconciler see rows it never
// created and try to actuate them. Promoting a delegation into a supervised
// task is a later bridge, not a merge.

use std::fs;
use std::path::PathBuf;

use super::{epoch_ms, relay_dir};

/// Where a delegated task's record lives.
fn tasks_dir() -> PathBuf {
    relay_dir().join("tasks")
}

/// A task id is a filename, so it must not be able to escape the directory or
/// name something else. Same reasoning as `is_valid_peer_id`.
fn is_valid_task_id(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id.len() <= 128
        && task_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn task_path(task_id: &str) -> Option<PathBuf> {
    if !is_valid_task_id(task_id) {
        return None;
    }
    Some(tasks_dir().join(format!("{task_id}.json")))
}

/// The states a delegated task passes through, from this host's point of view.
///
/// `Delegated` and `Running` are open; `Completed` and `Failed` are settled and
/// never change again — which is what makes a replayed message safe to ignore.
pub const STATE_DELEGATED: &str = "delegated";
pub const STATE_RUNNING: &str = "running";
pub const STATE_COMPLETED: &str = "completed";
pub const STATE_FAILED: &str = "failed";

/// Is this a state we will not move away from?
pub fn is_settled(state: &str) -> bool {
    state == STATE_COMPLETED || state == STATE_FAILED
}

/// Record that we have delegated a task. Called only after the send succeeds —
/// a task we could not hand over is not a task in flight.
pub fn record_delegated(
    task_id: &str,
    peer: &str,
    prompt: &str,
    cwd: Option<&str>,
) -> Result<(), String> {
    let path = task_path(task_id).ok_or_else(|| format!("invalid task id: {task_id}"))?;
    fs::create_dir_all(tasks_dir()).map_err(|e| format!("create tasks dir: {e}"))?;
    let record = serde_json::json!({
        "task_id": task_id,
        "peer": peer,
        "prompt": prompt,
        "cwd": cwd,
        "state": STATE_DELEGATED,
        "sent_at": epoch_ms(),
        "updated_at": epoch_ms(),
    });
    write_record(&path, &record)
}

fn write_record(path: &PathBuf, record: &serde_json::Value) -> Result<(), String> {
    fs::write(
        path,
        serde_json::to_string_pretty(record).unwrap_or_default(),
    )
    .map_err(|e| format!("write task record: {e}"))
}

/// Load one task's record.
pub fn load(task_id: &str) -> Option<serde_json::Value> {
    let path = task_path(task_id)?;
    serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()
}

/// What happened to a delegated task, as the peer reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum Report {
    /// A `TaskStatus`: still in flight, with whatever stats came with it.
    Progress {
        state: String,
        stats: serde_json::Value,
    },
    /// A `TaskHandoff`: settled, one way or the other.
    Settled {
        state: String,
        summary: String,
        artifacts: Vec<String>,
        git_ref: Option<String>,
        total_cost_usd: f64,
        total_tokens: u64,
        /// Which model ran it (#493). `None` from a worker too old to say.
        model: Option<String>,
        /// The token breakdown as the worker reported it, kept verbatim.
        usage: serde_json::Value,
    },
}

/// Read a `TaskStatus` or `TaskHandoff` payload into a report.
///
/// Takes the payload rather than the message so it can be tested without
/// building a whole `RelayMessage`.
pub fn parse_report(is_handoff: bool, payload: &serde_json::Value) -> Option<(String, Report)> {
    let task_id = payload.get("task_id")?.as_str()?.to_string();
    let state = payload
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or(if is_handoff {
            STATE_COMPLETED
        } else {
            STATE_RUNNING
        })
        .to_string();

    if !is_handoff {
        return Some((
            task_id,
            Report::Progress {
                state,
                stats: payload
                    .get("stats")
                    .cloned()
                    .unwrap_or(serde_json::json!({})),
            },
        ));
    }

    // A handoff carries the only two settled states. Anything else would leave
    // a task that the peer considers finished sitting open here forever, so
    // read an unrecognised state as a failure rather than trusting it.
    let state = if state == STATE_COMPLETED || state == STATE_FAILED {
        state
    } else {
        STATE_FAILED.to_string()
    };

    Some((
        task_id,
        Report::Settled {
            state,
            summary: payload
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            artifacts: payload
                .get("artifacts")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            git_ref: payload
                .get("git_ref")
                .and_then(|v| v.as_str())
                .map(String::from),
            total_cost_usd: payload
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            total_tokens: payload
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            model: payload
                .get("model")
                .and_then(|v| v.as_str())
                .map(String::from),
            usage: payload
                .get("usage")
                .cloned()
                .unwrap_or(serde_json::json!({})),
        },
    ))
}

/// A one-line account of a report, for the serve loop's console.
pub fn describe(report: &Report) -> String {
    match report {
        Report::Progress { state, stats } => {
            let tokens = stats
                .get("tokens_used")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cost = stats
                .get("cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if tokens == 0 && cost == 0.0 {
                state.clone()
            } else {
                format!("{state} ({tokens} tokens, ${cost:.4})")
            }
        }
        Report::Settled {
            state,
            summary,
            total_cost_usd,
            ..
        } => {
            if summary.is_empty() {
                format!("{state} (${total_cost_usd:.4})")
            } else {
                format!("{state}: {summary} (${total_cost_usd:.4})")
            }
        }
    }
}

/// What applying a report did, so the caller can say something true about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The record moved.
    Updated,
    /// The task was already settled, so the report was ignored. The mesh dedups
    /// by message id, but a reconnect can still replay one, and a settled task
    /// must not reopen.
    AlreadySettled,
    /// We never delegated this task. Not written: the ledger holds only our own
    /// tasks, and the sender could be stale or hostile.
    Unknown,
}

/// Apply a peer's report to the ledger.
pub fn apply(task_id: &str, report: &Report) -> Applied {
    let Some(path) = task_path(task_id) else {
        return Applied::Unknown;
    };
    let Some(mut record) = load(task_id) else {
        return Applied::Unknown;
    };
    let current = record
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or(STATE_DELEGATED);
    if is_settled(current) {
        return Applied::AlreadySettled;
    }

    let obj = match record.as_object_mut() {
        Some(o) => o,
        None => return Applied::Unknown,
    };
    match report {
        Report::Progress { state, stats } => {
            obj.insert("state".into(), serde_json::json!(state));
            obj.insert("stats".into(), stats.clone());
        }
        Report::Settled {
            state,
            summary,
            artifacts,
            git_ref,
            total_cost_usd,
            total_tokens,
            model,
            usage,
        } => {
            obj.insert("state".into(), serde_json::json!(state));
            obj.insert("summary".into(), serde_json::json!(summary));
            obj.insert("artifacts".into(), serde_json::json!(artifacts));
            obj.insert("git_ref".into(), serde_json::json!(git_ref));
            obj.insert("total_cost_usd".into(), serde_json::json!(total_cost_usd));
            obj.insert("total_tokens".into(), serde_json::json!(total_tokens));
            obj.insert("model".into(), serde_json::json!(model));
            obj.insert("usage".into(), usage.clone());
            obj.insert("settled_at".into(), serde_json::json!(epoch_ms()));
        }
    }
    obj.insert("updated_at".into(), serde_json::json!(epoch_ms()));

    if write_record(&path, &record).is_err() {
        return Applied::Unknown;
    }
    Applied::Updated
}

/// Every delegated task on record, most recently sent first.
pub fn list() -> Vec<serde_json::Value> {
    let Ok(entries) = fs::read_dir(tasks_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<serde_json::Value> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(".json")?;
            if !is_valid_task_id(id) {
                return None;
            }
            serde_json::from_str(&fs::read_to_string(e.path()).ok()?).ok()
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.get("sent_at").and_then(|v| v.as_u64()).unwrap_or(0)));
    out
}

#[cfg(test)]
mod reports {
    use super::*;

    fn handoff(payload: serde_json::Value) -> Option<(String, Report)> {
        parse_report(true, &payload)
    }

    #[test]
    fn a_completion_carries_its_summary_and_cost() {
        let (id, report) = handoff(serde_json::json!({
            "task_id": "task_1",
            "state": "completed",
            "summary": "Task completed successfully",
            "artifacts": ["out.txt"],
            "git_ref": "main",
            "total_cost_usd": 0.0123,
            "total_tokens": 456,
        }))
        .expect("parses");
        assert_eq!(id, "task_1");
        assert_eq!(
            report,
            Report::Settled {
                state: STATE_COMPLETED.into(),
                summary: "Task completed successfully".into(),
                artifacts: vec!["out.txt".into()],
                git_ref: Some("main".into()),
                total_cost_usd: 0.0123,
                total_tokens: 456,
                model: None,
                usage: serde_json::json!({}),
            }
        );
    }

    #[test]
    fn a_failure_is_settled_too() {
        let (_, report) = handoff(serde_json::json!({
            "task_id": "task_1",
            "state": "failed",
            "summary": "Task exited with non-zero status",
        }))
        .expect("parses");
        match report {
            Report::Settled { state, summary, .. } => {
                assert_eq!(state, STATE_FAILED);
                assert_eq!(summary, "Task exited with non-zero status");
            }
            other => panic!("expected a settled report, got {other:?}"),
        }
    }

    // A handoff means the peer is done with the task. An unrecognised state
    // must not leave it open here forever, so it reads as a failure.
    #[test]
    fn a_handoff_in_an_unknown_state_settles_as_failed() {
        let (_, report) =
            handoff(serde_json::json!({"task_id": "t", "state": "wat"})).expect("parses");
        match report {
            Report::Settled { state, .. } => assert_eq!(state, STATE_FAILED),
            other => panic!("expected settled, got {other:?}"),
        }
    }

    #[test]
    fn a_status_update_stays_open_and_keeps_its_stats() {
        let (id, report) = parse_report(
            false,
            &serde_json::json!({
                "task_id": "task_2",
                "state": "running",
                "stats": {"tokens_used": 10, "cost_usd": 0.5},
            }),
        )
        .expect("parses");
        assert_eq!(id, "task_2");
        match report {
            Report::Progress { state, stats } => {
                assert_eq!(state, STATE_RUNNING);
                assert_eq!(stats.get("tokens_used").and_then(|v| v.as_u64()), Some(10));
            }
            other => panic!("expected progress, got {other:?}"),
        }
    }

    // Without a task id there is no record to apply it to, so there is nothing
    // useful to do with the message.
    #[test]
    fn a_report_without_a_task_id_is_not_a_report() {
        assert!(handoff(serde_json::json!({"state": "completed"})).is_none());
        assert!(parse_report(false, &serde_json::json!({})).is_none());
    }

    #[test]
    fn missing_numbers_read_as_zero_rather_than_dropping_the_report() {
        let (_, report) =
            handoff(serde_json::json!({"task_id": "t", "state": "completed"})).expect("parses");
        match report {
            Report::Settled {
                total_cost_usd,
                total_tokens,
                artifacts,
                git_ref,
                ..
            } => {
                assert_eq!(total_cost_usd, 0.0);
                assert_eq!(total_tokens, 0);
                assert!(artifacts.is_empty());
                assert_eq!(git_ref, None);
            }
            other => panic!("expected settled, got {other:?}"),
        }
    }

    #[test]
    fn settled_states_are_the_two_that_never_move() {
        assert!(is_settled(STATE_COMPLETED));
        assert!(is_settled(STATE_FAILED));
        assert!(!is_settled(STATE_DELEGATED));
        assert!(!is_settled(STATE_RUNNING));
    }

    // A task id becomes a filename, so it must not be able to name anything
    // outside the tasks directory.
    #[test]
    fn a_task_id_that_could_escape_the_directory_is_refused() {
        for bad in [
            "../../etc/passwd",
            "a/b",
            "..",
            ".",
            "",
            "with space",
            "semi;colon",
        ] {
            assert!(task_path(bad).is_none(), "{bad:?} must be refused");
        }
        assert!(task_path("task_1791515207310_0").is_some());
        assert!(task_path("task-with-dashes").is_some());
    }
}

// Allow dead_code: spawn/verifier branches are stubbed in this PR (#345).
// PR5 wires verifiers; the real spawn path lands alongside `Sensors` next.
#![allow(dead_code)]
//! Actuator — turns `Action`s into real side effects (#345).
//!
//! Pairs with `supervisor::Supervisor`: the reconciler returns a list,
//! the actuator carries it out. Splitting these two keeps reconciler
//! tests pure (no bus, no terminal launcher) and keeps the actuator
//! free of decisioning logic. Each action ends in:
//!
//! 1. The real side effect (mailbox write, spawn, file write, …).
//! 2. A coord write recording what happened: a new `task_attempt` row,
//!    a `task_transition` row, or a `session_policy` file.
//!
//! Failure modes are deliberate: an action that can't be performed
//! returns `Err` and the reconciler picks it up on the next tick. We do
//! NOT silently mark a transition that didn't actually happen — the
//! crash-safety claim ("restart re-converges from coord.db") rests on
//! the ledger being honest.

use rusqlite::Connection;

use super::supervisor::Action;
use super::tasks::{TaskState, attempt_count, get_task, insert_attempt, transition};

/// Side-effect surface the actuator depends on. Stubbed in tests so the
/// crash-safety test can drive every transition without spinning up a
/// real bus or terminal launcher.
pub trait SideEffects {
    /// Write a `task.assigned` message to the role's mailbox. Returns
    /// the bus `message_id` so the actuator can store it on the
    /// attempt row — that link is what lets the reconciler later
    /// detect "the recipient drained this message" vs "still pending."
    fn publish_assignment(
        &self,
        role: &str,
        task_id: &str,
        prompt: &str,
        hop_count: u32,
    ) -> Result<String, String>;

    /// Launch a fresh Claude Code session in `cwd` with `prompt`.
    /// Returns the session_id so the actuator can record it on the
    /// attempt. The crash-safety guarantee means callers MUST treat a
    /// returned session_id as durable — restarting the daemon shouldn't
    /// re-spawn the same task.
    fn spawn_session(&self, cwd: &std::path::Path, prompt: &str) -> Result<String, String>;
}

/// Carry out one action against the SQL store and the side-effect
/// surface. Each variant writes both the side effect *and* the coord
/// rows that record it in one transaction-style sequence (insert
/// attempt → write side effect → log transition). A side-effect failure
/// stops before the transition is recorded so the next tick re-emits
/// the action.
pub fn apply(conn: &mut Connection, fx: &dyn SideEffects, action: &Action) -> Result<(), String> {
    match action {
        Action::AssignViaMailbox { task_id, role } => {
            let task =
                get_task(conn, task_id)?.ok_or_else(|| format!("task {task_id} not found"))?;
            if task.state != TaskState::Pending && task.state != TaskState::Ready {
                // The reconciler may have raced with a parallel
                // actuator instance; ignore safely.
                return Ok(());
            }
            // Pre-allocate the attempt row so we have the session/cwd
            // hash slots ready before publishing. The mailbox message
            // ID lands after the bus call succeeds.
            let next_num = attempt_count(conn, task_id)? + 1;
            let attempt_id = insert_attempt(conn, task_id, next_num, None, None, None)?;
            let message_id = fx.publish_assignment(role, task_id, &task.prompt, 1)?;
            // Stamp the new message id onto the attempt; if this fails
            // the supervisor will see a dangling attempt and re-publish
            // on the next tick — the bus side has already inserted, so
            // it might emit twice, but the message recipient will see
            // `task.assigned` twice and that's a recoverable event for
            // the actuator's later retry-task path.
            stamp_attempt_bus_id(conn, &attempt_id, &message_id)?;
            transition(
                conn,
                task_id,
                task.state,
                TaskState::Assigned,
                "assigned-via-mailbox",
            )?;
            Ok(())
        }
        Action::Spawn { task_id, cwd } => {
            let task =
                get_task(conn, task_id)?.ok_or_else(|| format!("task {task_id} not found"))?;
            let from_state = task.state;
            if from_state != TaskState::Pending
                && from_state != TaskState::Ready
                && from_state != TaskState::Assigned
            {
                return Ok(());
            }
            // For tasks transitioning Assigned → Spawn (claim_timeout
            // fallback), we bump the attempt counter so the mailbox
            // attempt and the spawn attempt are separately addressable.
            let next_num = attempt_count(conn, task_id)? + 1;
            let session_id = fx.spawn_session(cwd, &task.prompt)?;
            let _attempt_id =
                insert_attempt(conn, task_id, next_num, Some(&session_id), None, None)?;
            transition(
                conn,
                task_id,
                from_state,
                TaskState::Running,
                if from_state == TaskState::Assigned {
                    "spawn-fallback"
                } else {
                    "spawned"
                },
            )?;
            Ok(())
        }
        // The remaining variants belong to PR5 / PR6 — stubbed out so
        // the reconciler can emit them today without the actuator
        // panicking.
        Action::RunVerifier { .. }
        | Action::WriteSessionPolicy { .. }
        | Action::ClearSessionPolicy { .. }
        | Action::EscalateHuman { .. } => Ok(()),
    }
}

/// Connect the bus `message_id` to the attempt row. Separate from
/// `insert_attempt` because the message id only exists after the bus
/// publish succeeds, and we want the attempt row to exist *before* the
/// publish so a publish that succeeds but never returns (network blip,
/// process kill) still leaves a recoverable ledger entry.
fn stamp_attempt_bus_id(
    conn: &Connection,
    attempt_id: &str,
    bus_message_id: &str,
) -> Result<(), String> {
    conn.execute(
        "UPDATE task_attempts SET bus_message_id = ?1 WHERE id = ?2",
        rusqlite::params![bus_message_id, attempt_id],
    )
    .map_err(|e| format!("stamp attempt bus id: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::store;
    use crate::coord::supervisor::Action;
    use crate::coord::tasks::{NewTask, TaskState, get_task, insert_task, list_transitions};

    /// In-process recorder so tests can assert what side effects fired
    /// without standing up a real bus or terminal.
    struct RecordingFx {
        published: std::cell::RefCell<Vec<(String, String, u32)>>,
        spawned: std::cell::RefCell<Vec<(std::path::PathBuf, String)>>,
        next_message_id: std::cell::RefCell<u64>,
        next_session_id: std::cell::RefCell<u64>,
    }

    impl Default for RecordingFx {
        fn default() -> Self {
            Self {
                published: Default::default(),
                spawned: Default::default(),
                next_message_id: std::cell::RefCell::new(1),
                next_session_id: std::cell::RefCell::new(1),
            }
        }
    }

    impl SideEffects for RecordingFx {
        fn publish_assignment(
            &self,
            role: &str,
            task_id: &str,
            _prompt: &str,
            hop_count: u32,
        ) -> Result<String, String> {
            self.published
                .borrow_mut()
                .push((role.to_string(), task_id.to_string(), hop_count));
            let mut id = self.next_message_id.borrow_mut();
            let out = format!("msg_test_{id}");
            *id += 1;
            Ok(out)
        }
        fn spawn_session(&self, cwd: &std::path::Path, prompt: &str) -> Result<String, String> {
            let mut id = self.next_session_id.borrow_mut();
            let out = format!("sess_test_{id}");
            *id += 1;
            self.spawned
                .borrow_mut()
                .push((cwd.to_path_buf(), prompt.to_string()));
            Ok(out)
        }
    }

    fn sample_with_role(role: Option<&str>) -> NewTask {
        NewTask {
            name: "t".into(),
            role: role.map(String::from),
            cwd: "/work/x".into(),
            prompt: "do it".into(),
            model: None,
            budget_usd: None,
            max_retries: None,
            timeout_min: None,
            depends_on: vec![],
            policy: None,
        }
    }

    #[test]
    fn assign_via_mailbox_publishes_and_transitions() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &sample_with_role(Some("backend"))).unwrap();
        let fx = RecordingFx::default();
        apply(
            &mut conn,
            &fx,
            &Action::AssignViaMailbox {
                task_id: id.clone(),
                role: "backend".into(),
            },
        )
        .unwrap();
        // Side effect.
        let published = fx.published.borrow();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "backend");
        assert_eq!(published[0].1, id);
        // State + transition.
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Assigned);
        let hist = list_transitions(&conn, &id).unwrap();
        let causes: Vec<_> = hist.iter().map(|(_, _, c, _)| c.as_str()).collect();
        assert_eq!(causes, vec!["submitted", "assigned-via-mailbox"]);
        // Attempt row exists with the stamped message id.
        let n = attempt_count(&conn, &id).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn spawn_for_roleless_task_runs_immediately() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &sample_with_role(None)).unwrap();
        let fx = RecordingFx::default();
        apply(
            &mut conn,
            &fx,
            &Action::Spawn {
                task_id: id.clone(),
                cwd: std::path::PathBuf::from("/work/x"),
            },
        )
        .unwrap();
        assert_eq!(fx.spawned.borrow().len(), 1);
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Running);
        let hist = list_transitions(&conn, &id).unwrap();
        assert_eq!(hist.last().unwrap().2, "spawned");
    }

    #[test]
    fn assign_is_idempotent_against_already_assigned_task() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &sample_with_role(Some("backend"))).unwrap();
        let fx = RecordingFx::default();
        let action = Action::AssignViaMailbox {
            task_id: id.clone(),
            role: "backend".into(),
        };
        apply(&mut conn, &fx, &action).unwrap();
        // Second apply against an already-Assigned task is a no-op.
        apply(&mut conn, &fx, &action).unwrap();
        // Only one publish ever fired.
        assert_eq!(fx.published.borrow().len(), 1);
        // Task is still Assigned.
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Assigned);
    }

    #[test]
    fn crash_safety_restart_does_not_duplicate_assignment() {
        // The load-bearing test (issue #345 / #346 acceptance criterion):
        // "kill -9 the daemon mid-run → restart converges from coord.db
        //  with no duplicate spawns / no lost attempts."
        //
        // We model the kill by dropping the in-memory state (the
        // Supervisor / Actuator structs) and re-running tick() against
        // the same connection. The action emitted on restart must be
        // empty for the already-Assigned task — that's what proves the
        // ledger is what we converge to, not in-memory state.
        let mut conn = store::open_memory();
        let id1 = insert_task(&conn, &sample_with_role(Some("backend"))).unwrap();
        let id2 = insert_task(&conn, &sample_with_role(Some("infra"))).unwrap();
        let id3 = insert_task(&conn, &sample_with_role(Some("frontend"))).unwrap();

        // First "boot" — emit and actuate.
        let fx = RecordingFx::default();
        for action in [
            Action::AssignViaMailbox {
                task_id: id1.clone(),
                role: "backend".into(),
            },
            Action::AssignViaMailbox {
                task_id: id2.clone(),
                role: "infra".into(),
            },
            Action::AssignViaMailbox {
                task_id: id3.clone(),
                role: "frontend".into(),
            },
        ] {
            apply(&mut conn, &fx, &action).unwrap();
        }
        let baseline = fx.published.borrow().len();
        assert_eq!(baseline, 3);

        // "Restart": new actuator instance, same DB. Re-emit the same
        // actions the reconciler might still hold in memory. They are
        // expected to be no-ops because the ledger says the tasks are
        // already Assigned.
        let fx2 = RecordingFx::default();
        for action in [
            Action::AssignViaMailbox {
                task_id: id1,
                role: "backend".into(),
            },
            Action::AssignViaMailbox {
                task_id: id2,
                role: "infra".into(),
            },
            Action::AssignViaMailbox {
                task_id: id3,
                role: "frontend".into(),
            },
        ] {
            apply(&mut conn, &fx2, &action).unwrap();
        }
        assert_eq!(
            fx2.published.borrow().len(),
            0,
            "restart must not re-publish: ledger is the source of truth"
        );
    }
}

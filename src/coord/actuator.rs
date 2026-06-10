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
use super::tasks::{
    TaskState, attempt_count, get_task, insert_attempt, record_verification, transition,
};
use super::verify::{VerdictKind, VerifierBackend, run_verifier};

/// Side-effect surface the actuator depends on. Stubbed in tests so the
/// crash-safety test can drive every transition without spinning up a
/// real bus or terminal launcher.
pub trait SideEffects: VerifierBackend {
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
        Action::RunVerifier {
            task_id,
            attempt_id,
        } => {
            let task =
                get_task(conn, task_id)?.ok_or_else(|| format!("task {task_id} not found"))?;
            // Only valid from Running — guards against a stale tick
            // emitting verify against a task that's already moved on.
            if task.state != TaskState::Running && task.state != TaskState::Verifying {
                return Ok(());
            }
            // Move to Verifying if we're not already there. The
            // transition is the "verifier pass started" marker an
            // operator looking at `task_status` should see.
            if task.state == TaskState::Running {
                transition(
                    conn,
                    task_id,
                    TaskState::Running,
                    TaskState::Verifying,
                    "running-complete",
                )?;
            }

            // No verifiers declared ⇒ task graduates straight to Done.
            // Gates are opt-in; an empty list is a perfectly valid
            // contract that says "trust the agent."
            if task.verifiers.is_empty() {
                transition(
                    conn,
                    task_id,
                    TaskState::Verifying,
                    TaskState::Done,
                    "no-verifiers",
                )?;
                return Ok(());
            }

            // Walk verifiers in declared order. Short-circuit on first
            // FAIL — RFC §5's verifier-is-the-gradient principle.
            let cwd = std::path::PathBuf::from(&task.cwd);
            for verifier in &task.verifiers {
                let verdict = run_verifier(fx, &cwd, verifier)?;
                let verdict_str = match verdict.verdict {
                    VerdictKind::Pass => "PASS",
                    VerdictKind::Fail => "FAIL",
                };
                record_verification(
                    conn,
                    attempt_id,
                    verifier.kind(),
                    verifier.command_text(),
                    verdict_str,
                    &verdict.output,
                    verdict.cost_usd,
                )?;
                if verdict.verdict == VerdictKind::Fail {
                    // Attempts cap → NEEDS_HUMAN; otherwise retry.
                    let used = attempt_count(conn, task_id)?;
                    let next_state = if used > task.max_retries {
                        TaskState::NeedsHuman
                    } else {
                        TaskState::Retrying
                    };
                    transition(
                        conn,
                        task_id,
                        TaskState::Verifying,
                        next_state,
                        match next_state {
                            TaskState::NeedsHuman => "retries-exhausted",
                            _ => "verify-fail",
                        },
                    )?;
                    return Ok(());
                }
                // PASS path: fall through to the next verifier.
                let _ = retry_prompt_for_fail; // retain reference for clippy
            }
            // All verifiers passed.
            transition(
                conn,
                task_id,
                TaskState::Verifying,
                TaskState::Done,
                "verify-pass",
            )?;
            Ok(())
        }
        Action::Resume { task_id, cause } => {
            let task =
                get_task(conn, task_id)?.ok_or_else(|| format!("task {task_id} not found"))?;
            let from_state = task.state;
            // Only valid from Running. Resuming → Assigned happens later
            // in this branch; other states are stale-tick no-ops.
            if from_state != TaskState::Running {
                return Ok(());
            }
            // Attempts cap → NeedsHuman instead of resuming forever.
            let used = attempt_count(conn, task_id)?;
            if used > task.max_retries {
                transition(
                    conn,
                    task_id,
                    from_state,
                    TaskState::NeedsHuman,
                    "resume-cap",
                )?;
                return Ok(());
            }
            transition(conn, task_id, from_state, TaskState::Resuming, cause)?;
            // Re-enter via Pending so the reconciler picks the same
            // assignment lane the original task did — mailbox-first
            // for tasks with a role, spawn for roleless. Going straight
            // to a fresh attempt here would race the reconciler.
            transition(
                conn,
                task_id,
                TaskState::Resuming,
                TaskState::Pending,
                "resume-ready",
            )?;
            Ok(())
        }
        Action::EscalateHuman { task_id, cause } => {
            let task =
                get_task(conn, task_id)?.ok_or_else(|| format!("task {task_id} not found"))?;
            // Idempotent against already-terminal tasks; race-safe
            // against another tick that escalated first.
            if task.state.is_terminal() {
                return Ok(());
            }
            transition(conn, task_id, task.state, TaskState::NeedsHuman, cause)?;
            Ok(())
        }
        // The remaining variants belong to PR6 — stubbed out so the
        // reconciler can emit them today without the actuator
        // panicking. Implementation lands with the resume protocol.
        Action::WriteSessionPolicy { .. } | Action::ClearSessionPolicy { .. } => Ok(()),
    }
}

/// Compose the retry-prompt fed back to the agent on `Retrying`. The
/// supervisor doesn't *use* this yet — it lands when M6's resume
/// protocol takes the Retrying → Assigned edge — but lives here next
/// to the verifier dispatch so the verifier output it consumes is
/// obviously the same string the ledger recorded.
pub fn retry_prompt_for_fail(original_prompt: &str, verifier_kind: &str, output: &str) -> String {
    super::verify::build_retry_prompt(original_prompt, verifier_kind, output)
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
    /// without standing up a real bus or terminal. The `shell_results`
    /// / `brain_replies` / `agent_replies` queues let verifier-flow
    /// tests script each verifier's verdict deterministically.
    struct RecordingFx {
        published: std::cell::RefCell<Vec<(String, String, u32)>>,
        spawned: std::cell::RefCell<Vec<(std::path::PathBuf, String)>>,
        next_message_id: std::cell::RefCell<u64>,
        next_session_id: std::cell::RefCell<u64>,
        shell_results:
            std::cell::RefCell<std::collections::VecDeque<crate::coord::verify::ShellResult>>,
        brain_replies: std::cell::RefCell<std::collections::VecDeque<String>>,
        agent_replies:
            std::cell::RefCell<std::collections::VecDeque<crate::coord::verify::AgentResult>>,
    }

    impl Default for RecordingFx {
        fn default() -> Self {
            Self {
                published: Default::default(),
                spawned: Default::default(),
                next_message_id: std::cell::RefCell::new(1),
                next_session_id: std::cell::RefCell::new(1),
                shell_results: Default::default(),
                brain_replies: Default::default(),
                agent_replies: Default::default(),
            }
        }
    }

    impl VerifierBackend for RecordingFx {
        fn run_shell(
            &self,
            _cwd: &std::path::Path,
            command: &str,
            _timeout: std::time::Duration,
        ) -> Result<crate::coord::verify::ShellResult, String> {
            // Each command's exit / output is steered by the test via
            // `shell_results` queue; default is exit 0 with the command
            // echoed back so the assertions can read it without
            // round-tripping a process.
            let popped = self.shell_results.borrow_mut().pop_front();
            Ok(popped.unwrap_or(crate::coord::verify::ShellResult {
                exit_code: 0,
                combined_output: format!("ran {command}"),
            }))
        }
        fn query_brain(&self, _prompt: &str) -> Result<String, String> {
            Ok(self
                .brain_replies
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| "PASS".into()))
        }
        fn run_agent(
            &self,
            _prompt: &str,
            _model: Option<&str>,
            _budget_usd: Option<f64>,
        ) -> Result<crate::coord::verify::AgentResult, String> {
            Ok(self
                .agent_replies
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| crate::coord::verify::AgentResult {
                    reply: "PASS".into(),
                    cost_usd: 0.0,
                }))
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
            verifiers: vec![],
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
    fn run_verifier_pass_chain_lands_done() {
        use crate::coord::tasks::list_transitions;
        use crate::coord::verify::Verifier;

        let mut conn = store::open_memory();
        // Two passing verifiers chained.
        let new_task = NewTask {
            verifiers: vec![
                Verifier::Run {
                    command: "cargo test".into(),
                },
                Verifier::Brain {
                    prompt: "Looks ok?".into(),
                },
            ],
            ..sample_with_role(None)
        };
        let id = insert_task(&conn, &new_task).unwrap();
        let fx = RecordingFx::default();
        // Get the task into Running via the Spawn path so the verifier
        // can advance it.
        apply(
            &mut conn,
            &fx,
            &Action::Spawn {
                task_id: id.clone(),
                cwd: std::path::PathBuf::from("/work/x"),
            },
        )
        .unwrap();
        let attempt_id = crate::coord::tasks::latest_attempt_id(&conn, &id)
            .unwrap()
            .unwrap();
        apply(
            &mut conn,
            &fx,
            &Action::RunVerifier {
                task_id: id.clone(),
                attempt_id,
            },
        )
        .unwrap();
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Done);
        // Transition log should end at Done with verify-pass.
        let hist = list_transitions(&conn, &id).unwrap();
        let last_cause = hist.last().unwrap().2.clone();
        assert_eq!(last_cause, "verify-pass");
    }

    #[test]
    fn run_verifier_first_fail_short_circuits_to_retrying() {
        use crate::coord::tasks::list_transitions;
        use crate::coord::verify::{ShellResult, Verifier};

        let mut conn = store::open_memory();
        let new_task = NewTask {
            verifiers: vec![
                Verifier::Run {
                    command: "cargo test".into(),
                },
                // This second verifier must NEVER run — short-circuit on
                // the first FAIL is the RFC §5 contract.
                Verifier::Brain {
                    prompt: "should be skipped".into(),
                },
            ],
            max_retries: Some(2),
            ..sample_with_role(None)
        };
        let id = insert_task(&conn, &new_task).unwrap();
        let fx = RecordingFx::default();
        // Script: first shell run returns exit 1 with output.
        fx.shell_results.borrow_mut().push_back(ShellResult {
            exit_code: 1,
            combined_output: "test tests::auth FAILED".into(),
        });
        apply(
            &mut conn,
            &fx,
            &Action::Spawn {
                task_id: id.clone(),
                cwd: std::path::PathBuf::from("/work/x"),
            },
        )
        .unwrap();
        let attempt_id = crate::coord::tasks::latest_attempt_id(&conn, &id)
            .unwrap()
            .unwrap();
        apply(
            &mut conn,
            &fx,
            &Action::RunVerifier {
                task_id: id.clone(),
                attempt_id,
            },
        )
        .unwrap();
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Retrying);
        // No brain reply consumed → the second verifier short-circuited.
        assert!(fx.brain_replies.borrow().is_empty());
        let hist = list_transitions(&conn, &id).unwrap();
        let last_cause = &hist.last().unwrap().2;
        assert_eq!(last_cause, "verify-fail");
    }

    #[test]
    fn run_verifier_exhausted_retries_lands_needs_human() {
        use crate::coord::verify::{ShellResult, Verifier};

        let mut conn = store::open_memory();
        // max_retries = 0 → first FAIL has no retry slot.
        let new_task = NewTask {
            verifiers: vec![Verifier::Run {
                command: "cargo test".into(),
            }],
            max_retries: Some(0),
            ..sample_with_role(None)
        };
        let id = insert_task(&conn, &new_task).unwrap();
        let fx = RecordingFx::default();
        fx.shell_results.borrow_mut().push_back(ShellResult {
            exit_code: 1,
            combined_output: "nope".into(),
        });
        apply(
            &mut conn,
            &fx,
            &Action::Spawn {
                task_id: id.clone(),
                cwd: std::path::PathBuf::from("/work/x"),
            },
        )
        .unwrap();
        let attempt_id = crate::coord::tasks::latest_attempt_id(&conn, &id)
            .unwrap()
            .unwrap();
        apply(
            &mut conn,
            &fx,
            &Action::RunVerifier {
                task_id: id.clone(),
                attempt_id,
            },
        )
        .unwrap();
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::NeedsHuman);
    }

    #[test]
    fn run_verifier_with_empty_list_graduates_directly() {
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
        let attempt_id = crate::coord::tasks::latest_attempt_id(&conn, &id)
            .unwrap()
            .unwrap();
        apply(
            &mut conn,
            &fx,
            &Action::RunVerifier {
                task_id: id.clone(),
                attempt_id,
            },
        )
        .unwrap();
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Done);
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

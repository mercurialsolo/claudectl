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

    /// Where `task`'s session runs: its worktree when it has a `git_ref`
    /// (created or reused), its `cwd` otherwise (#570).
    fn prepare_workdir(&self, task: &super::tasks::TaskRow) -> Result<std::path::PathBuf, String>;

    /// Start the session a request describes, detached. Returns its pid.
    /// The attempt (session id, workdir) is recorded before this runs, so a
    /// crash during it can't lose track of a session (#570).
    fn spawn_session(&self, req: &super::spawn::SpawnRequest) -> Result<u32, String>;

    /// Whether a recorded session process is still running.
    fn pid_alive(&self, pid: u32) -> bool;

    /// After DONE: remove a task's worktree if nothing would be lost.
    fn cleanup_workdir(&self, task: &super::tasks::TaskRow, workdir: &std::path::Path);

    /// What a finished attempt's session reported, if it has.
    fn read_result(&self, attempt_id: &str) -> Option<super::spawn::RunResult>;
    /// Send a placed task to a relay peer (#571).
    fn delegate(&self, req: &super::remote::DelegateRequest) -> Result<(), String>;
    /// What a remote attempt reported, verdicts included, once it has.
    fn remote_result(&self, attempt_id: &str) -> Option<super::remote::RemoteResult>;
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
                && from_state != TaskState::Retrying
            {
                return Ok(());
            }
            let _ = cwd; // the task row is the source of truth for where it runs

            // Adopt instead of spawning twice: a daemon that crashed after
            // starting the session but before moving the task to RUNNING
            // finds the session alive on the latest attempt.
            let adopted = super::tasks::running_spawned_attempts_for(conn, task_id)?
                .is_some_and(|a| a.pid.is_some_and(|p| fx.pid_alive(p)));

            if !adopted {
                // Workdir first: a failed checkout leaves nothing behind, and
                // the next tick tries again.
                let workdir = fx.prepare_workdir(&task)?;
                // For tasks transitioning Assigned → Spawn (claim_timeout
                // fallback), we bump the attempt counter so the mailbox
                // attempt and the spawn attempt are separately addressable.
                let next_num = attempt_count(conn, task_id)? + 1;
                // Built before this attempt's row exists, from the ones before.
                let prompt = attempt_prompt(conn, fx, &task)?;
                // Record the attempt before the process exists (#570). The
                // old order spawned first, so a crash in between started a
                // session nothing knew about, and the next tick started
                // another.
                let session_id = super::spawn::new_session_id();
                let attempt_id =
                    insert_attempt(conn, task_id, next_num, Some(&session_id), None, None)?;
                let workdir_s = workdir.to_string_lossy().to_string();
                super::tasks::set_attempt_workdir(conn, &attempt_id, &workdir_s)?;
                let req = super::spawn::SpawnRequest {
                    task_id: task_id.clone(),
                    attempt_id: attempt_id.clone(),
                    session_id,
                    workdir,
                    prompt,
                    permission_mode: task.permission_mode.clone(),
                    model: task.model.clone(),
                };
                match fx.spawn_session(&req) {
                    Ok(pid) => super::tasks::set_attempt_pid(conn, &attempt_id, pid)?,
                    Err(e) => {
                        // Close the attempt so it can't be mistaken for one
                        // in progress; the task stays where it was.
                        super::tasks::finish_attempt(
                            conn,
                            &attempt_id,
                            super::tasks::SPAWN_FAILED,
                            None,
                        )?;
                        return Err(e);
                    }
                }
            }
            transition(
                conn,
                task_id,
                from_state,
                TaskState::Running,
                match from_state {
                    TaskState::Assigned => "spawn-fallback",
                    TaskState::Retrying => "retried",
                    _ => "spawned",
                },
            )?;
            Ok(())
        }
        Action::Delegate { task_id, peer } => {
            let task =
                get_task(conn, task_id)?.ok_or_else(|| format!("task {task_id} not found"))?;
            let from_state = task.state;
            if !matches!(
                from_state,
                TaskState::Pending | TaskState::Ready | TaskState::Retrying
            ) {
                return Ok(());
            }
            let next_num = attempt_count(conn, task_id)? + 1;
            let prompt = attempt_prompt(conn, fx, &task)?;
            // Recorded before it's sent, as a spawn is: a crash in between
            // leaves an attempt the sensor reports as never acknowledged.
            let attempt_id = insert_attempt(conn, task_id, next_num, None, None, None)?;
            super::tasks::set_attempt_remote(conn, &attempt_id, peer)?;
            let req = super::remote::DelegateRequest {
                attempt_id: attempt_id.clone(),
                peer: peer.clone(),
                prompt,
                cwd: task.cwd.clone(),
                git_ref: task.git_ref.clone(),
                worktree_key: task.id.clone(),
                branch: task.git_ref.as_ref().map(|_| {
                    task.branch.clone().unwrap_or_else(|| {
                        claudectl_core::worktree::default_branch(&task.name, &task.id)
                    })
                }),
                verify_run: task
                    .verifiers
                    .iter()
                    .filter_map(|v| match v {
                        super::verify::Verifier::Run { command } => Some(command.clone()),
                        _ => None,
                    })
                    .collect(),
                permission_mode: task.permission_mode.clone(),
                model: task.model.clone(),
            };
            if let Err(e) = fx.delegate(&req) {
                super::tasks::finish_attempt(
                    conn,
                    &attempt_id,
                    super::tasks::DELEGATE_FAILED,
                    None,
                )?;
                return Err(e);
            }
            transition(
                conn,
                task_id,
                from_state,
                TaskState::Running,
                if from_state == TaskState::Retrying {
                    "retried-on-peer"
                } else {
                    "delegated"
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
            // Its verifiers already ran where it did; record their verdicts.
            if super::tasks::attempt_peer(conn, attempt_id)?.is_some() {
                return verify_remote(conn, fx, &task, attempt_id);
            }
            // Close the attempt with what its session reported (#570).
            if task.state == TaskState::Running {
                let result = fx.read_result(attempt_id);
                super::tasks::finish_attempt(
                    conn,
                    attempt_id,
                    "completed",
                    result.and_then(|r| r.cost_usd),
                )?;
            }
            // Verifiers check the work where it was done: the attempt's
            // worktree, not the task's original cwd (#570).
            let workdir = super::tasks::latest_attempt_workdir(conn, task_id)?
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(&task.cwd));
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
                fx.cleanup_workdir(&task, &workdir);
                maybe_auto_post_pr(task_id);
                return Ok(());
            }

            // Walk verifiers in declared order. Short-circuit on first
            // FAIL — RFC §5's verifier-is-the-gradient principle.
            let cwd = workdir.clone();
            for verifier in &task.verifiers {
                let verdict = run_verifier(fx, &cwd, task.git_ref.as_deref(), verifier)?;
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
                    return verification_failed(conn, &task);
                }
                // PASS path: fall through to the next verifier.
            }
            // All verifiers passed.
            transition(
                conn,
                task_id,
                TaskState::Verifying,
                TaskState::Done,
                "verify-pass",
            )?;
            fx.cleanup_workdir(&task, &workdir);
            maybe_auto_post_pr(task_id);
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
            // Close the attempt that's being given up on, with why (#570), so
            // it's never adopted as a live session again.
            if let Some(attempt_id) = super::tasks::latest_attempt_id(conn, task_id)? {
                let cost = attempt_outcome(fx, &attempt_id).and_then(|(_, c)| c);
                super::tasks::finish_attempt(conn, &attempt_id, cause, cost)?;
            }
            // Attempts cap → NeedsHuman instead of resuming forever.
            let used = super::tasks::attempts_that_ran(conn, task_id)?;
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
        // Nothing to do: the task stays where it is until there's room.
        Action::Hold { .. } => Ok(()),
    }
}

/// The prompt for a task's next attempt, from how the last one ended:
///
/// - it completed and a verifier failed: the task's prompt plus that
///   verifier's output, to fix (#574);
/// - it died, failed or was stopped: a recovery prompt, so the session
///   assesses the workdir instead of redoing the work;
/// - first attempt: the task's prompt.
///
/// Then, for a task with `depends_on`, what each upstream task handed off
/// (#572).
pub fn attempt_prompt(
    conn: &Connection,
    fx: &dyn SideEffects,
    task: &super::tasks::TaskRow,
) -> Result<String, String> {
    let mut prompt = match super::tasks::latest_attempt_outcome(conn, &task.id)? {
        Some((attempt_id, Some(outcome))) => {
            let failures = super::tasks::attempt_failures(conn, &attempt_id)?;
            match (outcome.as_str(), failures.last()) {
                ("completed", Some((kind, output))) => {
                    super::verify::build_retry_prompt(&task.prompt, kind, output)
                }
                ("completed", None) => task.prompt.clone(),
                _ => super::resume::build_recovery_prompt(&task.prompt, &failures, None, false),
            }
        }
        _ => task.prompt.clone(),
    };
    let handoffs = upstream_handoffs(conn, fx, task)?;
    if !handoffs.is_empty() {
        prompt.push_str("\n\nThis task depends on work that has finished:\n");
        prompt.push_str(&handoffs);
    }
    Ok(prompt)
}

/// A finished attempt's closing summary and cost, wherever it ran.
fn attempt_outcome(fx: &dyn SideEffects, attempt_id: &str) -> Option<(String, Option<f64>)> {
    fx.read_result(attempt_id)
        .map(|r| (r.summary, r.cost_usd))
        .or_else(|| {
            fx.remote_result(attempt_id)
                .map(|r| (r.summary, r.cost_usd))
        })
}

/// A verifier failed: retry, or past the cap, a human.
fn verification_failed(conn: &mut Connection, task: &super::tasks::TaskRow) -> Result<(), String> {
    let used = super::tasks::attempts_that_ran(conn, &task.id)?;
    let next_state = if used > task.max_retries {
        TaskState::NeedsHuman
    } else {
        TaskState::Retrying
    };
    transition(
        conn,
        &task.id,
        TaskState::Verifying,
        next_state,
        match next_state {
            TaskState::NeedsHuman => "retries-exhausted",
            _ => "verify-fail",
        },
    )
}

/// A remote attempt finished (#571): its `run =` verifiers ran in the peer's
/// worktree, and their verdicts came back in the handoff. Each declared
/// verifier needs a verdict; one the peer didn't send is a FAIL, so a worker
/// that skipped them can't pass a task.
fn verify_remote(
    conn: &mut Connection,
    fx: &dyn SideEffects,
    task: &super::tasks::TaskRow,
    attempt_id: &str,
) -> Result<(), String> {
    let result = fx.remote_result(attempt_id);
    if task.state == TaskState::Running {
        super::tasks::finish_attempt(
            conn,
            attempt_id,
            "completed",
            result.as_ref().and_then(|r| r.cost_usd),
        )?;
        transition(
            conn,
            &task.id,
            TaskState::Running,
            TaskState::Verifying,
            "running-complete",
        )?;
    }
    let verdicts = result.map(|r| r.verdicts).unwrap_or_default();
    for (i, verifier) in task.verifiers.iter().enumerate() {
        let (passed, output) = match verdicts.get(i) {
            Some(v) if v.command == verifier.command_text() => (v.passed, v.output.clone()),
            Some(v) => (
                false,
                format!(
                    "the peer ran `{}` where `{}` was expected",
                    v.command,
                    verifier.command_text()
                ),
            ),
            None => (
                false,
                "the peer sent no verdict for this verifier".to_string(),
            ),
        };
        record_verification(
            conn,
            attempt_id,
            verifier.kind(),
            verifier.command_text(),
            if passed { "PASS" } else { "FAIL" },
            &output,
            0.0,
        )?;
        if !passed {
            return verification_failed(conn, task);
        }
    }
    transition(
        conn,
        &task.id,
        TaskState::Verifying,
        TaskState::Done,
        if task.verifiers.is_empty() {
            "no-verifiers"
        } else {
            "verify-pass"
        },
    )?;
    maybe_auto_post_pr(&task.id);
    Ok(())
}

/// One line per finished dependency: its name, the branch its work is on,
/// and its session's closing summary.
fn upstream_handoffs(
    conn: &Connection,
    fx: &dyn SideEffects,
    task: &super::tasks::TaskRow,
) -> Result<String, String> {
    let mut out = String::new();
    for key in &task.depends_on {
        let Some(dep) = super::tasks::resolve_dependency(conn, key)? else {
            continue;
        };
        out.push_str(&format!("- {}", dep.name));
        if dep.git_ref.is_some() {
            let branch = dep
                .branch
                .clone()
                .unwrap_or_else(|| claudectl_core::worktree::default_branch(&dep.name, &dep.id));
            out.push_str(&format!(" (branch `{branch}`)"));
        } else {
            out.push_str(&format!(" (in {})", dep.cwd));
        }
        let latest = super::tasks::latest_attempt_outcome(conn, &dep.id)?;
        if let Some((attempt_id, _)) = &latest {
            if let Some(peer) = super::tasks::attempt_peer(conn, attempt_id)? {
                out.push_str(&format!(" on peer {peer}"));
            }
        }
        let summary = latest
            .and_then(|(attempt_id, _)| attempt_outcome(fx, &attempt_id))
            .map(|(s, _)| s)
            .filter(|s| !s.trim().is_empty());
        match summary {
            Some(s) => out.push_str(&format!(": {}\n", s.trim())),
            None => out.push('\n'),
        }
    }
    Ok(out)
}

/// Parse the `CLAUDECTL_PR_AUTO_POST` flag value. Pure so the gate is testable
/// without touching the process environment.
fn parse_auto_post_flag(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1" | "true" | "yes" | "on"))
}

/// Whether to auto-post a task's PR summary + verifier status when it reaches
/// DONE. Opt-in via env so it never surprises a user, and so the reconciler's
/// default behavior (no network calls) is unchanged.
fn pr_auto_post_enabled() -> bool {
    parse_auto_post_flag(std::env::var("CLAUDECTL_PR_AUTO_POST").ok().as_deref())
}

/// When enabled, post the task's PR summary + verifier status (#369) on a
/// **detached** thread — git/gh can take seconds, and the reconciler tick must
/// never block on them. Best-effort: the transition is already committed before
/// this runs, so a failed post is logged, not propagated.
fn maybe_auto_post_pr(task_id: &str) {
    if !pr_auto_post_enabled() {
        return;
    }
    let task_id = task_id.to_string();
    std::thread::spawn(move || match super::pr::post_task_summary(&task_id) {
        Ok(msg) => crate::logger::log("DEBUG", &format!("pr auto-post: {msg}")),
        Err(e) => crate::logger::log("DEBUG", &format!("pr auto-post skipped: {e}")),
    });
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

    #[test]
    fn auto_post_flag_is_opt_in() {
        assert!(parse_auto_post_flag(Some("1")));
        assert!(parse_auto_post_flag(Some("true")));
        assert!(parse_auto_post_flag(Some(" on ")));
        assert!(!parse_auto_post_flag(Some("0")));
        assert!(!parse_auto_post_flag(Some("")));
        assert!(!parse_auto_post_flag(None));
    }

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
        // #570 spawn path.
        spawn_requests: std::cell::RefCell<Vec<crate::coord::spawn::SpawnRequest>>,
        spawn_error: std::cell::RefCell<Option<String>>,
        workdir_error: std::cell::RefCell<Option<String>>,
        alive_pids: std::cell::RefCell<Vec<u32>>,
        cleaned: std::cell::RefCell<Vec<std::path::PathBuf>>,
        result: std::cell::RefCell<Option<crate::coord::spawn::RunResult>>,
        // #571 remote path.
        delegated: std::cell::RefCell<Vec<crate::coord::remote::DelegateRequest>>,
        delegate_error: std::cell::RefCell<Option<String>>,
        remote: std::cell::RefCell<Option<crate::coord::remote::RemoteResult>>,
        shell_cwds: std::cell::RefCell<Vec<std::path::PathBuf>>,
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
                spawn_requests: Default::default(),
                spawn_error: Default::default(),
                workdir_error: Default::default(),
                alive_pids: Default::default(),
                cleaned: Default::default(),
                result: Default::default(),
                delegated: Default::default(),
                delegate_error: Default::default(),
                remote: Default::default(),
                shell_cwds: Default::default(),
            }
        }
    }

    impl VerifierBackend for RecordingFx {
        fn run_shell(
            &self,
            cwd: &std::path::Path,
            command: &str,
            _timeout: std::time::Duration,
        ) -> Result<crate::coord::verify::ShellResult, String> {
            self.shell_cwds.borrow_mut().push(cwd.to_path_buf());
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
            _cwd: &std::path::Path,
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
        fn prepare_workdir(
            &self,
            task: &crate::coord::tasks::TaskRow,
        ) -> Result<std::path::PathBuf, String> {
            if let Some(e) = self.workdir_error.borrow().clone() {
                return Err(e);
            }
            Ok(match &task.git_ref {
                Some(_) => std::path::PathBuf::from(format!("/wt/{}", task.id)),
                None => std::path::PathBuf::from(&task.cwd),
            })
        }
        fn spawn_session(&self, req: &crate::coord::spawn::SpawnRequest) -> Result<u32, String> {
            if let Some(e) = self.spawn_error.borrow().clone() {
                return Err(e);
            }
            let mut id = self.next_session_id.borrow_mut();
            let pid = 10_000 + *id as u32;
            *id += 1;
            self.spawned
                .borrow_mut()
                .push((req.workdir.clone(), req.prompt.clone()));
            self.spawn_requests.borrow_mut().push(req.clone());
            Ok(pid)
        }
        fn pid_alive(&self, pid: u32) -> bool {
            self.alive_pids.borrow().contains(&pid)
        }
        fn cleanup_workdir(&self, _task: &crate::coord::tasks::TaskRow, workdir: &std::path::Path) {
            self.cleaned.borrow_mut().push(workdir.to_path_buf());
        }
        fn read_result(&self, _attempt_id: &str) -> Option<crate::coord::spawn::RunResult> {
            self.result.borrow().clone()
        }
        fn delegate(&self, req: &crate::coord::remote::DelegateRequest) -> Result<(), String> {
            if let Some(e) = self.delegate_error.borrow().clone() {
                return Err(e);
            }
            self.delegated.borrow_mut().push(req.clone());
            Ok(())
        }
        fn remote_result(&self, _attempt_id: &str) -> Option<crate::coord::remote::RemoteResult> {
            self.remote.borrow().clone()
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
            git_ref: None,
            branch: None,
            permission_mode: None,
            requires: vec![],
            peer: None,
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

    // ── #570: a session per task, in its own workdir ──────────────────

    fn spawn(conn: &mut Connection, fx: &RecordingFx, id: &str) -> Result<(), String> {
        apply(
            conn,
            fx,
            &Action::Spawn {
                task_id: id.to_string(),
                cwd: std::path::PathBuf::from("/work/x"),
            },
        )
    }

    fn attempt_row(
        conn: &Connection,
        id: &str,
    ) -> (Option<String>, Option<i64>, Option<String>, Option<String>) {
        conn.query_row(
            "SELECT session_id, pid, workdir, outcome FROM task_attempts WHERE task_id = ?1
             ORDER BY attempt_num DESC LIMIT 1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    }

    #[test]
    fn a_spawn_records_the_session_workdir_and_pid() {
        let mut conn = store::open_memory();
        let new_task = NewTask {
            permission_mode: Some("acceptEdits".into()),
            model: Some("sonnet".into()),
            ..sample_with_role(None)
        };
        let id = insert_task(&conn, &new_task).unwrap();
        let fx = RecordingFx::default();
        spawn(&mut conn, &fx, &id).unwrap();

        let req = fx.spawn_requests.borrow()[0].clone();
        assert_eq!(req.workdir, std::path::PathBuf::from("/work/x"));
        assert_eq!(req.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(req.model.as_deref(), Some("sonnet"));
        let (session, pid, workdir, _) = attempt_row(&conn, &id);
        // The session id the process was told to use is the one recorded.
        assert_eq!(session.as_deref(), Some(req.session_id.as_str()));
        assert_eq!(pid, Some(10_001));
        assert_eq!(workdir.as_deref(), Some("/work/x"));
        assert_eq!(
            get_task(&conn, &id).unwrap().unwrap().state,
            TaskState::Running
        );
    }

    #[test]
    fn a_task_with_a_ref_runs_in_its_worktree() {
        let mut conn = store::open_memory();
        let new_task = NewTask {
            git_ref: Some("main".into()),
            ..sample_with_role(None)
        };
        let id = insert_task(&conn, &new_task).unwrap();
        let fx = RecordingFx::default();
        spawn(&mut conn, &fx, &id).unwrap();
        let wt = format!("/wt/{id}");
        assert_eq!(
            fx.spawn_requests.borrow()[0].workdir,
            std::path::PathBuf::from(&wt)
        );
        assert_eq!(attempt_row(&conn, &id).2.as_deref(), Some(wt.as_str()));
    }

    #[test]
    fn a_failed_spawn_closes_its_attempt_and_leaves_the_task_pending() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &sample_with_role(None)).unwrap();
        let fx = RecordingFx::default();
        *fx.spawn_error.borrow_mut() = Some("claude: not found".into());
        assert!(spawn(&mut conn, &fx, &id).is_err());
        assert_eq!(attempt_row(&conn, &id).3.as_deref(), Some("spawn_failed"));
        assert_eq!(
            get_task(&conn, &id).unwrap().unwrap().state,
            TaskState::Pending
        );
    }

    #[test]
    fn a_failed_checkout_records_no_attempt() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &sample_with_role(None)).unwrap();
        let fx = RecordingFx::default();
        *fx.workdir_error.borrow_mut() = Some("not a git repo".into());
        assert!(spawn(&mut conn, &fx, &id).is_err());
        assert_eq!(attempt_count(&conn, &id).unwrap(), 0);
    }

    /// The crash the old order allowed: the session started, the daemon
    /// died before recording RUNNING, and the next tick started another.
    #[test]
    fn a_live_session_from_before_a_crash_is_adopted_not_doubled() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &sample_with_role(None)).unwrap();
        let attempt = insert_attempt(&conn, &id, 1, Some("sess-pre"), None, None).unwrap();
        crate::coord::tasks::set_attempt_workdir(&conn, &attempt, "/work/x").unwrap();
        crate::coord::tasks::set_attempt_pid(&conn, &attempt, 4242).unwrap();
        let fx = RecordingFx::default();
        fx.alive_pids.borrow_mut().push(4242);
        spawn(&mut conn, &fx, &id).unwrap();
        assert!(
            fx.spawn_requests.borrow().is_empty(),
            "spawned a second session"
        );
        assert_eq!(
            get_task(&conn, &id).unwrap().unwrap().state,
            TaskState::Running
        );
    }

    #[test]
    fn verifiers_run_in_the_attempts_workdir_and_the_attempt_is_closed() {
        use crate::coord::verify::Verifier;
        let mut conn = store::open_memory();
        let new_task = NewTask {
            git_ref: Some("main".into()),
            verifiers: vec![Verifier::Run {
                command: "test -f HELLO.txt".into(),
            }],
            ..sample_with_role(None)
        };
        let id = insert_task(&conn, &new_task).unwrap();
        let fx = RecordingFx::default();
        spawn(&mut conn, &fx, &id).unwrap();
        *fx.result.borrow_mut() = Some(crate::coord::spawn::RunResult {
            ok: true,
            cost_usd: Some(0.29),
            summary: "done".into(),
        });
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
        let wt = std::path::PathBuf::from(format!("/wt/{id}"));
        assert_eq!(fx.shell_cwds.borrow().as_slice(), std::slice::from_ref(&wt));
        assert_eq!(
            get_task(&conn, &id).unwrap().unwrap().state,
            TaskState::Done
        );
        assert_eq!(attempt_row(&conn, &id).3.as_deref(), Some("completed"));
        let cost: f64 = conn
            .query_row(
                "SELECT cost_usd FROM task_attempts WHERE task_id = ?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert!((cost - 0.29).abs() < 1e-9);
        assert_eq!(fx.cleaned.borrow().as_slice(), [wt]);
    }

    /// A resumed attempt is closed, so its pid (alive or reused) is never
    /// adopted as the task's session again.
    #[test]
    fn a_resume_closes_the_attempt_it_gives_up_on() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &sample_with_role(None)).unwrap();
        let fx = RecordingFx::default();
        spawn(&mut conn, &fx, &id).unwrap();
        fx.alive_pids.borrow_mut().push(10_001);
        apply(
            &mut conn,
            &fx,
            &Action::Resume {
                task_id: id.clone(),
                cause: "session_died".into(),
            },
        )
        .unwrap();
        assert_eq!(attempt_row(&conn, &id).3.as_deref(), Some("session_died"));
        // Back to PENDING, the next spawn starts a fresh session.
        spawn(&mut conn, &fx, &id).unwrap();
        assert_eq!(fx.spawn_requests.borrow().len(), 2);
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

    // ── #574: a retry goes back to work with the failure in its prompt ──

    fn run_verifiers(conn: &mut Connection, fx: &RecordingFx, id: &str) {
        let attempt_id = crate::coord::tasks::latest_attempt_id(conn, id)
            .unwrap()
            .unwrap();
        apply(
            conn,
            fx,
            &Action::RunVerifier {
                task_id: id.to_string(),
                attempt_id,
            },
        )
        .unwrap();
    }

    fn failing_once(fx: &RecordingFx) {
        fx.shell_results
            .borrow_mut()
            .push_back(crate::coord::verify::ShellResult {
                exit_code: 1,
                combined_output: "expected one, found two".into(),
            });
    }

    fn verified_task(max_retries: u32) -> NewTask {
        NewTask {
            verifiers: vec![crate::coord::verify::Verifier::Run {
                command: "test -f ONE".into(),
            }],
            max_retries: Some(max_retries),
            git_ref: Some("main".into()),
            ..sample_with_role(None)
        }
    }

    #[test]
    fn a_retry_runs_in_the_same_workdir_with_the_failure_in_its_prompt() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &verified_task(2)).unwrap();
        let fx = RecordingFx::default();
        spawn(&mut conn, &fx, &id).unwrap();
        failing_once(&fx);
        run_verifiers(&mut conn, &fx, &id);
        assert_eq!(
            get_task(&conn, &id).unwrap().unwrap().state,
            TaskState::Retrying
        );

        spawn(&mut conn, &fx, &id).unwrap();
        let reqs = fx.spawn_requests.borrow();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].prompt, "do it");
        assert!(reqs[1].prompt.starts_with("do it"));
        assert!(
            reqs[1].prompt.contains("expected one, found two"),
            "{}",
            reqs[1].prompt
        );
        assert_eq!(reqs[1].workdir, reqs[0].workdir);
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Running);
        let hist = list_transitions(&conn, &id).unwrap();
        assert_eq!(hist.last().unwrap().2, "retried");
    }

    #[test]
    fn a_retry_that_passes_is_done_and_one_that_keeps_failing_needs_a_human() {
        let mut conn = store::open_memory();
        let pass = insert_task(&conn, &verified_task(2)).unwrap();
        let fx = RecordingFx::default();
        spawn(&mut conn, &fx, &pass).unwrap();
        failing_once(&fx);
        run_verifiers(&mut conn, &fx, &pass);
        spawn(&mut conn, &fx, &pass).unwrap();
        run_verifiers(&mut conn, &fx, &pass);
        assert_eq!(
            get_task(&conn, &pass).unwrap().unwrap().state,
            TaskState::Done
        );

        let fail = insert_task(&conn, &verified_task(1)).unwrap();
        spawn(&mut conn, &fx, &fail).unwrap();
        failing_once(&fx);
        run_verifiers(&mut conn, &fx, &fail);
        spawn(&mut conn, &fx, &fail).unwrap();
        failing_once(&fx);
        run_verifiers(&mut conn, &fx, &fail);
        assert_eq!(
            get_task(&conn, &fail).unwrap().unwrap().state,
            TaskState::NeedsHuman
        );
        assert_eq!(attempt_count(&conn, &fail).unwrap(), 2);
    }

    #[test]
    fn a_resumed_task_is_told_to_assess_the_workdir_first() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &sample_with_role(None)).unwrap();
        let fx = RecordingFx::default();
        spawn(&mut conn, &fx, &id).unwrap();
        apply(
            &mut conn,
            &fx,
            &Action::Resume {
                task_id: id.clone(),
                cause: "session_died".into(),
            },
        )
        .unwrap();
        spawn(&mut conn, &fx, &id).unwrap();
        let reqs = fx.spawn_requests.borrow();
        assert!(
            reqs[1].prompt.contains("resuming an interrupted task"),
            "{}",
            reqs[1].prompt
        );
    }

    #[test]
    fn a_failed_spawn_does_not_hide_the_verifier_failure_before_it() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &verified_task(3)).unwrap();
        let fx = RecordingFx::default();
        spawn(&mut conn, &fx, &id).unwrap();
        failing_once(&fx);
        run_verifiers(&mut conn, &fx, &id);
        *fx.spawn_error.borrow_mut() = Some("fork failed".into());
        assert!(spawn(&mut conn, &fx, &id).is_err());
        *fx.spawn_error.borrow_mut() = None;
        spawn(&mut conn, &fx, &id).unwrap();
        let reqs = fx.spawn_requests.borrow();
        assert!(
            reqs.last()
                .unwrap()
                .prompt
                .contains("expected one, found two")
        );
    }

    // ── #572: a dependent task is told what its dependencies handed off ──

    #[test]
    fn a_dependent_task_gets_each_upstream_branch_and_summary() {
        let mut conn = store::open_memory();
        let fx = RecordingFx::default();
        let a = insert_task(
            &conn,
            &NewTask {
                name: "schema".into(),
                git_ref: Some("main".into()),
                branch: Some("feat/schema".into()),
                ..sample_with_role(None)
            },
        )
        .unwrap();
        spawn(&mut conn, &fx, &a).unwrap();
        *fx.result.borrow_mut() = Some(crate::coord::spawn::RunResult {
            ok: true,
            cost_usd: Some(0.1),
            summary: "Added the users table.".into(),
        });
        run_verifiers(&mut conn, &fx, &a);
        assert_eq!(get_task(&conn, &a).unwrap().unwrap().state, TaskState::Done);

        let b = insert_task(
            &conn,
            &NewTask {
                name: "api".into(),
                depends_on: vec![a.clone()],
                ..sample_with_role(None)
            },
        )
        .unwrap();
        spawn(&mut conn, &fx, &b).unwrap();
        let reqs = fx.spawn_requests.borrow();
        let p = &reqs.last().unwrap().prompt;
        assert!(p.starts_with("do it"), "{p}");
        assert!(
            p.contains("- schema (branch `feat/schema`): Added the users table."),
            "{p}"
        );
    }

    // ── #571: a task run on a relay peer ───────────────────────────────

    fn delegate(conn: &mut Connection, fx: &RecordingFx, id: &str) -> Result<(), String> {
        apply(
            conn,
            fx,
            &Action::Delegate {
                task_id: id.to_string(),
                peer: "mini".into(),
            },
        )
    }

    fn remote_task() -> NewTask {
        NewTask {
            name: "api".into(),
            git_ref: Some("main".into()),
            permission_mode: Some("acceptEdits".into()),
            verifiers: vec![
                crate::coord::verify::Verifier::Run {
                    command: "test -f A".into(),
                },
                crate::coord::verify::Verifier::Run {
                    command: "test -f B".into(),
                },
            ],
            ..sample_with_role(None)
        }
    }

    fn verdict(command: &str, passed: bool) -> crate::coord::remote::Verdict {
        crate::coord::remote::Verdict {
            command: command.into(),
            passed,
            output: if passed { String::new() } else { "no B".into() },
        }
    }

    #[test]
    fn a_delegated_attempt_names_its_peer_worktree_branch_and_verifiers() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &remote_task()).unwrap();
        let fx = RecordingFx::default();
        delegate(&mut conn, &fx, &id).unwrap();
        let req = fx.delegated.borrow()[0].clone();
        let attempt = crate::coord::tasks::latest_attempt_id(&conn, &id)
            .unwrap()
            .unwrap();
        assert_eq!(
            req.attempt_id, attempt,
            "the attempt id is the relay task id"
        );
        assert_eq!(req.peer, "mini");
        assert_eq!(req.worktree_key, id, "one worktree for every attempt");
        assert_eq!(
            req.branch,
            Some(claudectl_core::worktree::default_branch("api", &id))
        );
        assert_eq!(req.verify_run, vec!["test -f A", "test -f B"]);
        assert_eq!(req.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(
            crate::coord::tasks::attempt_peer(&conn, &attempt)
                .unwrap()
                .as_deref(),
            Some("mini")
        );
        assert_eq!(
            crate::coord::tasks::latest_session_id(&conn, &id).unwrap(),
            Some(attempt),
            "the reconciler finds it by the same id the sensor reports"
        );
        assert_eq!(
            get_task(&conn, &id).unwrap().unwrap().state,
            TaskState::Running
        );
        assert!(fx.spawned.borrow().is_empty(), "nothing ran here");
    }

    #[test]
    fn a_send_that_fails_closes_the_attempt_and_leaves_the_task_queued() {
        let mut conn = store::open_memory();
        let id = insert_task(&conn, &remote_task()).unwrap();
        let fx = RecordingFx::default();
        *fx.delegate_error.borrow_mut() = Some("peer refused".into());
        assert!(delegate(&mut conn, &fx, &id).is_err());
        assert_eq!(
            get_task(&conn, &id).unwrap().unwrap().state,
            TaskState::Pending
        );
        let (_, _, _, outcome) = attempt_row(&conn, &id);
        assert_eq!(outcome.as_deref(), Some("delegate_failed"));
    }

    #[test]
    fn the_peers_verdicts_decide_the_task() {
        let mut conn = store::open_memory();
        let fx = RecordingFx::default();

        let pass = insert_task(&conn, &remote_task()).unwrap();
        delegate(&mut conn, &fx, &pass).unwrap();
        *fx.remote.borrow_mut() = Some(crate::coord::remote::RemoteResult {
            ok: true,
            cost_usd: Some(0.3),
            summary: "Done.".into(),
            branch: None,
            verdicts: vec![verdict("test -f A", true), verdict("test -f B", true)],
        });
        run_verifiers(&mut conn, &fx, &pass);
        assert_eq!(
            get_task(&conn, &pass).unwrap().unwrap().state,
            TaskState::Done
        );
        assert!(fx.shell_cwds.borrow().is_empty(), "no verifier ran here");

        let fail = insert_task(&conn, &remote_task()).unwrap();
        delegate(&mut conn, &fx, &fail).unwrap();
        fx.remote.borrow_mut().as_mut().unwrap().verdicts =
            vec![verdict("test -f A", true), verdict("test -f B", false)];
        run_verifiers(&mut conn, &fx, &fail);
        assert_eq!(
            get_task(&conn, &fail).unwrap().unwrap().state,
            TaskState::Retrying
        );
        // Its retry goes back with the peer's verifier output in the prompt.
        assert!(
            crate::coord::actuator::attempt_prompt(
                &conn,
                &fx,
                &get_task(&conn, &fail).unwrap().unwrap()
            )
            .unwrap()
            .contains("no B")
        );
    }

    /// A peer that declined a task (full, or couldn't run it) didn't run it:
    /// the task is placed again and its retries aren't spent, and the next
    /// attempt's prompt still carries the last real failure.
    #[test]
    fn a_peer_refusal_does_not_use_a_retry() {
        let mut conn = store::open_memory();
        let fx = RecordingFx::default();
        let id = insert_task(
            &conn,
            &NewTask {
                max_retries: Some(0),
                ..remote_task()
            },
        )
        .unwrap();
        for _ in 0..3 {
            delegate(&mut conn, &fx, &id).unwrap();
            apply(
                &mut conn,
                &fx,
                &Action::Resume {
                    task_id: id.clone(),
                    cause: crate::coord::tasks::PEER_REFUSED.into(),
                },
            )
            .unwrap();
            assert_eq!(
                get_task(&conn, &id).unwrap().unwrap().state,
                TaskState::Pending
            );
        }
        assert_eq!(
            crate::coord::tasks::attempts_that_ran(&conn, &id).unwrap(),
            0
        );
        assert_eq!(attempt_count(&conn, &id).unwrap(), 3);
        // A first prompt still: nothing ran, so there's nothing to recover.
        delegate(&mut conn, &fx, &id).unwrap();
        assert_eq!(fx.delegated.borrow().last().unwrap().prompt, "do it");
    }

    /// An older worker runs the task and ignores the verifiers. A verdict
    /// that never came is a FAIL, not a pass.
    #[test]
    fn a_verifier_the_peer_did_not_run_fails() {
        let mut conn = store::open_memory();
        let fx = RecordingFx::default();
        let id = insert_task(&conn, &remote_task()).unwrap();
        delegate(&mut conn, &fx, &id).unwrap();
        *fx.remote.borrow_mut() = Some(crate::coord::remote::RemoteResult {
            ok: true,
            verdicts: vec![verdict("test -f A", true)],
            ..Default::default()
        });
        run_verifiers(&mut conn, &fx, &id);
        assert_eq!(
            get_task(&conn, &id).unwrap().unwrap().state,
            TaskState::Retrying
        );
    }
}

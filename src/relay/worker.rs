// Remote worker: accepts delegated tasks, spawns local claude sessions, reports status.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use super::RelayMessage;
use super::delegation::{
    DelegationContext, RemoteVerdict, TaskReport, TaskStats, build_report_message,
    build_status_message,
};

// ────────────────────────────────────────────────────────────────────────────
// Worker task state
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTaskState {
    Preparing,
    Running,
    /// The session finished and its supervisor's `run =` verifiers are
    /// running in the worktree (#571).
    Verifying,
    Completed,
    Failed,
}

pub struct WorkerTask {
    pub state: WorkerTaskState,
    pub child: Option<Child>,
    pub pid: Option<u32>,
    pub start_time: Instant,
    pub last_status_sent: Instant,
    pub tokens_used: u64,
    pub cost_usd: f64,
    pub from_peer: String,
    /// Where the run's stdout and stderr were sent (#493). A file rather than a
    /// pipe: the poll loop deliberately never reads the child, and a child that
    /// fills a pipe buffer with nobody reading blocks forever.
    pub out_path: PathBuf,
    pub err_path: PathBuf,
    /// The worktree branch the task runs on, when it was given a ref (#570).
    pub branch: Option<String>,
    /// Where the session ran; a supervised task's verifiers run here too.
    pub work_dir: PathBuf,
    /// A supervised task's `run =` verifiers (#571).
    pub verify_run: Vec<String>,
    /// While verifying: the handoff to send once the verdicts are in, and
    /// the thread running them.
    pub pending: Option<(TaskReport, std::thread::JoinHandle<Vec<RemoteVerdict>>)>,
}

/// This machine's own `[local] max_tasks` from the supervisor's
/// `policy.toml`, if it has one. Without coord there's no policy to read.
pub fn local_max_tasks() -> Option<u32> {
    #[cfg(feature = "coord")]
    {
        crate::coord::placement::load_policy()
            .ok()
            .and_then(|p| p.local.max_tasks)
    }
    #[cfg(not(feature = "coord"))]
    {
        None
    }
}

/// How long one supervised verifier may run on the worker.
const VERIFIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// How much of a verifier's output travels back: its tail, where the failure is.
const VERDICT_OUTPUT_MAX: usize = 4096;

/// Run `run =` verifiers in order in `dir`, stopping at the first failure,
/// as the supervisor does locally (#571). Runs on its own thread.
fn run_verifiers(dir: &std::path::Path, commands: &[String]) -> Vec<RemoteVerdict> {
    let mut out = Vec::new();
    for command in commands {
        let (passed, output) = match run_one(dir, command) {
            Ok((code, output)) => (code == 0, output),
            Err(e) => (false, e),
        };
        out.push(RemoteVerdict {
            command: command.clone(),
            passed,
            output: tail(&output, VERDICT_OUTPUT_MAX),
        });
        if !passed {
            break;
        }
    }
    out
}

fn run_one(dir: &std::path::Path, command: &str) -> Result<(i32, String), String> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn verifier: {e}"))?;
    // Drained while it runs, or a verifier printing more than a pipe buffer
    // never exits.
    fn drain<R: std::io::Read + Send + 'static>(p: Option<R>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= VERIFIER_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("verifier timed out after {VERIFIER_TIMEOUT:?}"));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(format!("wait verifier: {e}")),
        }
    };
    let mut output = stdout.join().unwrap_or_default();
    output.push_str(&stderr.join().unwrap_or_default());
    Ok((status.code().unwrap_or(-1), output))
}

/// The last `max` bytes of `s`, on a character boundary.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[…]{}", &s[start..])
}

/// Where a run's captured output lives while it runs.
fn worker_output_dir() -> PathBuf {
    super::relay_dir().join("worker")
}

// ────────────────────────────────────────────────────────────────────────────
// Remote worker
// ────────────────────────────────────────────────────────────────────────────

/// Handles execution of delegated tasks on the worker side.
pub struct RemoteWorker {
    pub tasks: HashMap<String, WorkerTask>,
    identity: String,
}

impl RemoteWorker {
    pub fn new(identity: &str) -> Self {
        RemoteWorker {
            tasks: HashMap::new(),
            identity: identity.to_string(),
        }
    }

    /// Delegated tasks still running here, for the headroom this machine
    /// advertises (#571).
    pub fn running_count(&self) -> u32 {
        self.tasks
            .values()
            .filter(|t| {
                matches!(
                    t.state,
                    WorkerTaskState::Running | WorkerTaskState::Verifying
                )
            })
            .count() as u32
    }

    /// Accept a new delegated task. Spawns a `claude` session.
    pub fn accept_task(
        &mut self,
        task_id: &str,
        prompt: &str,
        cwd: Option<&str>,
        // `git_commit` or else `git_ref` starts the task in its own worktree
        // (#570). `git_remote`, `relevant_files` and the brain summary are
        // still unused.
        context: DelegationContext,
        from_peer: &str,
    ) -> Result<RelayMessage, String> {
        if self.tasks.contains_key(task_id) {
            return Err(format!("task {task_id} already exists"));
        }
        // A supervisor that placed this task here counted on this machine's
        // headroom, but two supervisors can count on the same slot. Its own
        // `max_tasks` is the last word (#571).
        if context.supervised.is_some() {
            if let Some(max) = local_max_tasks() {
                let running = self.running_count();
                if running >= max {
                    return Err(format!("this machine is running {running} of {max} tasks"));
                }
            }
        }

        // A ref means a worktree of the repo at `cwd`, on its own branch, so
        // a delegated task never edits the checkout someone is working in.
        // It used to be accepted and silently ignored. A ref that can't be
        // checked out refuses the task rather than running in `cwd` instead.
        let start_ref = context.git_commit.as_deref().or(context.git_ref.as_deref());
        let supervised = context.supervised.clone().unwrap_or_default();
        let (work_dir, branch) = match start_ref {
            Some(r) => {
                // A supervised task names its worktree and branch, so a retry
                // lands in the one its last attempt used (#571).
                let key = Some(supervised.worktree_key.as_str())
                    .filter(|k| !k.is_empty())
                    .unwrap_or(task_id);
                let branch = supervised
                    .branch
                    .clone()
                    .unwrap_or_else(|| claudectl_core::worktree::default_branch("delegated", key));
                let wt = claudectl_core::worktree::ensure(
                    std::path::Path::new(cwd.unwrap_or(".")),
                    r,
                    &branch,
                    &claudectl_core::worktree::path_for(key),
                )?;
                (wt.path.to_string_lossy().into_owned(), Some(wt.branch))
            }
            None => (cwd.unwrap_or(".").to_string(), None),
        };
        let work_dir = work_dir.as_str();
        let now = Instant::now();

        // `--output-format json` is what makes a delegated task's cost
        // knowable (#493): it reports `total_cost_usd`, the token breakdown,
        // which model ran, and `is_error` — all of which the worker used to
        // report as zero or guess from the exit status.
        //
        // Captured to files, not pipes. The poll loop below never reads the
        // child, and a child that fills a pipe buffer with nobody reading
        // blocks forever. A file is also complete the moment the child exits,
        // so there is no reader thread to join.
        let dir = worker_output_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create worker dir: {e}"))?;
        let out_path = dir.join(format!("{task_id}.out"));
        let err_path = dir.join(format!("{task_id}.err"));
        let out_file = std::fs::File::create(&out_path)
            .map_err(|e| format!("create {}: {e}", out_path.display()))?;
        let err_file = std::fs::File::create(&err_path)
            .map_err(|e| format!("create {}: {e}", err_path.display()))?;

        let mut args: Vec<&str> = vec!["--print", "--output-format", "json"];
        if let Some(m) = &supervised.permission_mode {
            args.extend(["--permission-mode", m.as_str()]);
        }
        if let Some(m) = &supervised.model {
            args.extend(["--model", m.as_str()]);
        }
        args.push(prompt);
        let child = Command::new(claudectl_core::helpers::claude_bin())
            .args(&args)
            .current_dir(work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_file))
            .stderr(Stdio::from(err_file))
            .spawn()
            .map_err(|e| {
                // The captures exist before the spawn, so a spawn that fails
                // would otherwise leave two files behind for a task that never
                // ran and is never tracked.
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
                // `spawn` reports a missing working directory as the same
                // ENOENT as a missing binary, so the bare message reads
                // "spawn claude: No such file or directory" and sends whoever
                // delegated the task looking for a Claude Code install that is
                // in fact fine. Name the real cause while we still can.
                if !std::path::Path::new(work_dir).is_dir() {
                    format!("working directory does not exist on this host: {work_dir}")
                } else {
                    format!("spawn claude in {work_dir}: {e}")
                }
            })?;

        let pid = child.id();

        let task = WorkerTask {
            state: WorkerTaskState::Running,
            child: Some(child),
            pid: Some(pid),
            start_time: now,
            last_status_sent: now,
            tokens_used: 0,
            cost_usd: 0.0,
            from_peer: from_peer.to_string(),
            out_path,
            err_path,
            branch,
            work_dir: PathBuf::from(work_dir),
            verify_run: supervised.verify_run,
            pending: None,
        };

        self.tasks.insert(task_id.to_string(), task);

        // Return initial status message
        let stats = TaskStats {
            elapsed_secs: Some(0),
            ..Default::default()
        };
        Ok(build_status_message(
            task_id,
            "running",
            &stats,
            &self.identity,
        ))
    }

    /// Poll all running tasks. Returns messages to send back to controllers.
    pub fn tick(&mut self) -> Vec<(String, RelayMessage)> {
        let mut messages = Vec::new();
        let task_ids: Vec<String> = self.tasks.keys().cloned().collect();

        for task_id in task_ids {
            let task = match self.tasks.get_mut(&task_id) {
                Some(t) => t,
                None => continue,
            };

            // Verifiers running on their thread: send the handoff once
            // they're done, never block the relay loop on them.
            if task.state == WorkerTaskState::Verifying {
                if task.pending.as_ref().is_some_and(|(_, h)| h.is_finished()) {
                    let (mut report, handle) = task.pending.take().expect("checked");
                    report.verifications = handle.join().unwrap_or_else(|_| {
                        vec![RemoteVerdict {
                            command: "(verifier runner)".into(),
                            passed: false,
                            output: "the verifier thread panicked".into(),
                        }]
                    });
                    task.state = WorkerTaskState::Completed;
                    messages.push((
                        task.from_peer.clone(),
                        build_report_message(&task_id, &report, &self.identity),
                    ));
                }
                continue;
            }
            if task.state != WorkerTaskState::Running {
                continue;
            }

            // Check if the child process has exited
            let exited = if let Some(child) = task.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => Some(status.success()),
                    Ok(None) => None, // still running
                    Err(_) => Some(false),
                }
            } else {
                Some(false)
            };

            let elapsed = task.start_time.elapsed().as_secs();
            let peer = task.from_peer.clone();

            match exited {
                Some(ok) => {
                    task.child = None;
                    task.pid = None;

                    // What the run actually cost and produced. `is_error` in
                    // the JSON is authoritative over the exit status: a
                    // not-logged-in run exits non-zero *and* says so in
                    // `result`, and only the latter is worth repeating to
                    // whoever delegated the task.
                    let captured = std::fs::read_to_string(&task.out_path).unwrap_or_default();
                    let parsed = super::outcome::parse(&captured);

                    let (failed, summary, cost, tokens, model, usage) = match &parsed {
                        Some(o) => (
                            o.failed,
                            o.summary.clone(),
                            o.total_cost_usd,
                            o.total_tokens,
                            o.model.clone(),
                            o.usage.clone(),
                        ),
                        // No result JSON at all: claude never got far enough to
                        // write it. The exit status is all we have, and stderr
                        // is the only clue worth forwarding.
                        None => {
                            let err = std::fs::read_to_string(&task.err_path).unwrap_or_default();
                            let why = if err.trim().is_empty() {
                                "Task produced no result and no error output".to_string()
                            } else {
                                super::outcome::cap_summary(&err)
                            };
                            (!ok, why, 0.0, 0, None, serde_json::json!({}))
                        }
                    };

                    task.cost_usd = cost;
                    task.tokens_used = tokens;
                    task.state = if failed {
                        WorkerTaskState::Failed
                    } else {
                        WorkerTaskState::Completed
                    };

                    // Keep the capture only when it could not be read, so there
                    // is something to diagnose; otherwise every delegated task
                    // would leak two files on the worker forever.
                    if parsed.is_some() {
                        let _ = std::fs::remove_file(&task.out_path);
                        let _ = std::fs::remove_file(&task.err_path);
                    }

                    let report = TaskReport {
                        failed,
                        summary,
                        total_cost_usd: cost,
                        total_tokens: tokens,
                        model,
                        usage,
                        branch: task.branch.clone(),
                        verifications: Vec::new(),
                        refused: false,
                    };
                    // A supervised task that finished its work goes to its
                    // verifiers, in the worktree, before anything is reported.
                    if !failed && !task.verify_run.is_empty() {
                        let commands = task.verify_run.clone();
                        let dir = task.work_dir.clone();
                        let handle = std::thread::spawn(move || run_verifiers(&dir, &commands));
                        task.state = WorkerTaskState::Verifying;
                        task.pending = Some((report, handle));
                        continue;
                    }
                    messages.push((
                        peer,
                        build_report_message(&task_id, &report, &self.identity),
                    ));
                }
                None => {
                    // Still running — send periodic status (every 30s)
                    if task.last_status_sent.elapsed().as_secs() >= 30 {
                        task.last_status_sent = Instant::now();
                        let stats = TaskStats {
                            tokens_used: task.tokens_used,
                            cost_usd: task.cost_usd,
                            elapsed_secs: Some(elapsed),
                            ..Default::default()
                        };
                        let msg = build_status_message(&task_id, "running", &stats, &self.identity);
                        messages.push((peer, msg));
                    }
                }
            }
        }

        messages
    }

    /// Handle an interrupt from the controller.
    pub fn handle_interrupt(
        &mut self,
        task_id: &str,
        interrupt_type: &str,
        _reason: &str,
    ) -> Option<RelayMessage> {
        let task = self.tasks.get_mut(task_id)?;

        match interrupt_type {
            "stop" => {
                if let Some(mut child) = task.child.take() {
                    let _ = child.kill();
                }
                task.state = WorkerTaskState::Failed;
                task.pid = None;
                // A killed run never wrote its result JSON, so there is no cost
                // to report — zero here is honest rather than missing.
                let _ = std::fs::remove_file(&task.out_path);
                let _ = std::fs::remove_file(&task.err_path);
                Some(build_report_message(
                    task_id,
                    &TaskReport {
                        failed: true,
                        summary: "Stopped by controller".to_string(),
                        total_cost_usd: 0.0,
                        total_tokens: 0,
                        model: None,
                        usage: serde_json::json!({}),
                        branch: task.branch.clone(),
                        verifications: Vec::new(),
                        refused: false,
                    },
                    &self.identity,
                ))
            }
            "nudge" => {
                // Nudges are informational — just ack with current status
                let elapsed = task.start_time.elapsed().as_secs();
                let stats = TaskStats {
                    tokens_used: task.tokens_used,
                    cost_usd: task.cost_usd,
                    elapsed_secs: Some(elapsed),
                    ..Default::default()
                };
                Some(build_status_message(
                    task_id,
                    "running",
                    &stats,
                    &self.identity,
                ))
            }
            _ => None,
        }
    }

    /// Clean up completed/failed tasks older than the given age.
    pub fn cleanup_finished(&mut self, max_age_secs: u64) {
        self.tasks.retain(|_, task| {
            if task.state == WorkerTaskState::Running
                || task.state == WorkerTaskState::Preparing
                || task.state == WorkerTaskState::Verifying
            {
                return true;
            }
            task.start_time.elapsed().as_secs() < max_age_secs
        });
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A task in a given state. No child is spawned in these fixtures, so
    /// there is no captured output to point at.
    fn task(state: WorkerTaskState) -> WorkerTask {
        WorkerTask {
            state,
            child: None,
            pid: None,
            start_time: Instant::now(),
            last_status_sent: Instant::now(),
            tokens_used: 0,
            cost_usd: 0.0,
            from_peer: "peer-a".into(),
            out_path: PathBuf::new(),
            err_path: PathBuf::new(),
            branch: None,
            work_dir: PathBuf::new(),
            verify_run: Vec::new(),
            pending: None,
        }
    }

    /// #571: a supervised task's verifiers run in order in its worktree and
    /// stop at the first failure, as they do locally; its output tail travels.
    #[test]
    fn supervised_verifiers_run_in_order_and_stop_at_the_first_failure() {
        let dir = std::env::temp_dir();
        let v = super::run_verifiers(
            &dir,
            &[
                "true".into(),
                "echo nope; exit 3".into(),
                "touch should-not-run".into(),
            ],
        );
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].passed);
        assert!(!v[1].passed);
        assert_eq!(v[1].output.trim(), "nope");
        assert!(!dir.join("should-not-run").exists());
    }

    #[test]
    fn a_long_verifier_output_keeps_its_tail() {
        let long = format!("{}END", "x".repeat(10_000));
        let t = super::tail(&long, super::VERDICT_OUTPUT_MAX);
        assert!(
            t.ends_with("END") && t.len() <= super::VERDICT_OUTPUT_MAX + 8,
            "{}",
            t.len()
        );
    }

    /// A task that's verifying is still running as far as headroom and
    /// cleanup are concerned.
    #[test]
    fn a_verifying_task_counts_as_running_and_is_not_cleaned_up() {
        let mut w = RemoteWorker::new("me");
        w.tasks.insert("t".into(), task(WorkerTaskState::Verifying));
        assert_eq!(w.running_count(), 1);
        w.cleanup_finished(0);
        assert!(w.tasks.contains_key("t"));
    }

    /// `relay delegate --git-ref` used to be accepted and ignored. A ref
    /// the worker can't check out now refuses the task before any session
    /// starts, instead of running in the plain cwd.
    #[test]
    fn a_ref_that_cannot_be_checked_out_refuses_the_task() {
        let dir = std::env::temp_dir().join(format!("cctl-worker-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut worker = RemoteWorker::new("test-peer");
        let ctx = DelegationContext {
            git_ref: Some("main".into()),
            ..Default::default()
        };
        let err = worker
            .accept_task("t-ref", "hi", Some(dir.to_str().unwrap()), ctx, "peer-a")
            .unwrap_err();
        assert!(err.contains("not in a git repo"), "{err}");
        assert!(worker.tasks.is_empty(), "nothing should have started");
    }

    #[test]
    fn worker_rejects_duplicate_task() {
        let mut worker = RemoteWorker::new("test-peer");
        // First accept will fail because `claude` binary likely doesn't exist in test,
        // but we can test the duplicate check separately.
        // Simulate a task already existing
        worker
            .tasks
            .insert("t_1".into(), task(WorkerTaskState::Running));

        let result =
            worker.accept_task("t_1", "test", None, DelegationContext::default(), "peer-a");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));
    }

    /// Settled tasks are dropped; running ones are never dropped however old.
    ///
    /// Nothing called this before, so `relay serve` kept a `WorkerTask` for
    /// every task it had ever run (#465). The serve loop now calls it each
    /// pass.
    #[test]
    fn cleanup_finished_drops_only_settled_tasks() {
        let mut worker = RemoteWorker::new("test-peer");
        worker
            .tasks
            .insert("done".into(), task(WorkerTaskState::Completed));
        worker
            .tasks
            .insert("failed".into(), task(WorkerTaskState::Failed));
        worker
            .tasks
            .insert("busy".into(), task(WorkerTaskState::Running));
        worker
            .tasks
            .insert("starting".into(), task(WorkerTaskState::Preparing));

        // Age zero: nothing is older than the limit yet.
        worker.cleanup_finished(3600);
        assert_eq!(worker.tasks.len(), 4, "nothing is old enough to drop");

        // Everything is older than zero seconds, so the settled ones go.
        worker.cleanup_finished(0);
        let mut left: Vec<&str> = worker.tasks.keys().map(String::as_str).collect();
        left.sort();
        assert_eq!(
            left,
            vec!["busy", "starting"],
            "a running or preparing task is kept regardless of age"
        );
    }

    #[test]
    fn handle_stop_interrupt() {
        let mut worker = RemoteWorker::new("test-peer");
        worker
            .tasks
            .insert("t_1".into(), task(WorkerTaskState::Running));

        let msg = worker.handle_interrupt("t_1", "stop", "no longer needed");
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.msg_type, super::super::MessageType::TaskHandoff);
        assert_eq!(
            msg.payload.get("state").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert_eq!(
            worker.tasks.get("t_1").unwrap().state,
            WorkerTaskState::Failed
        );
    }

    #[test]
    fn handle_nudge_interrupt() {
        let mut worker = RemoteWorker::new("test-peer");
        worker
            .tasks
            .insert("t_1".into(), task(WorkerTaskState::Running));

        let msg = worker.handle_interrupt("t_1", "nudge", "dependency resolved");
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.msg_type, super::super::MessageType::TaskStatus);
        // State should still be Running after a nudge
        assert_eq!(
            worker.tasks.get("t_1").unwrap().state,
            WorkerTaskState::Running
        );
    }

    #[test]
    fn cleanup_finished_removes_old() {
        let mut worker = RemoteWorker::new("test-peer");
        worker.tasks.insert(
            "t_1".into(),
            WorkerTask {
                start_time: Instant::now() - std::time::Duration::from_secs(3600),
                ..task(WorkerTaskState::Completed)
            },
        );

        assert_eq!(worker.tasks.len(), 1);
        worker.cleanup_finished(1800); // 30 min threshold
        assert_eq!(worker.tasks.len(), 0); // removed (1h old > 30min threshold)
    }
}

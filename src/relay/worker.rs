// Remote worker: accepts delegated tasks, spawns local claude sessions, reports status.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use super::RelayMessage;
use super::delegation::{
    DelegationContext, TaskReport, TaskStats, build_report_message, build_status_message,
};

// ────────────────────────────────────────────────────────────────────────────
// Worker task state
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTaskState {
    Preparing,
    Running,
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

    /// Accept a new delegated task. Spawns a `claude` session.
    pub fn accept_task(
        &mut self,
        task_id: &str,
        prompt: &str,
        cwd: Option<&str>,
        // Received and then dropped: the worker spawns `claude` in
        // `work_dir` and consults none of `git_remote`, `git_ref`,
        // `git_commit`, `relevant_files` or the brain summary. So
        // `relay delegate --git-ref X` is accepted, sent, and silently
        // ignored. Tracked separately; found by the #465 audit.
        _context: DelegationContext,
        from_peer: &str,
    ) -> Result<RelayMessage, String> {
        if self.tasks.contains_key(task_id) {
            return Err(format!("task {task_id} already exists"));
        }

        let work_dir = cwd.unwrap_or(".");
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

        let child = Command::new("claude")
            .args(["--print", "--output-format", "json", prompt])
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
                    };
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
            if task.state == WorkerTaskState::Running || task.state == WorkerTaskState::Preparing {
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
        }
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

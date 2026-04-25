// Remote worker: accepts delegated tasks, spawns local claude sessions, reports status.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use super::RelayMessage;
use super::delegation::{
    DelegationContext, TaskStats, build_failure_message, build_handoff_message,
    build_status_message,
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
    pub task_id: String,
    pub prompt: String,
    pub cwd: String,
    pub context: DelegationContext,
    pub state: WorkerTaskState,
    pub child: Option<Child>,
    pub pid: Option<u32>,
    pub start_time: Instant,
    pub last_status_sent: Instant,
    pub tokens_used: u64,
    pub cost_usd: f64,
    pub from_peer: String,
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
        context: DelegationContext,
        from_peer: &str,
    ) -> Result<RelayMessage, String> {
        if self.tasks.contains_key(task_id) {
            return Err(format!("task {task_id} already exists"));
        }

        let work_dir = cwd.unwrap_or(".");
        let now = Instant::now();

        // Spawn claude --print with the prompt
        let child = Command::new("claude")
            .args(["--print", prompt])
            .current_dir(work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn claude: {e}"))?;

        let pid = child.id();

        let task = WorkerTask {
            task_id: task_id.to_string(),
            prompt: prompt.to_string(),
            cwd: work_dir.to_string(),
            context,
            state: WorkerTaskState::Running,
            child: Some(child),
            pid: Some(pid),
            start_time: now,
            last_status_sent: now,
            tokens_used: 0,
            cost_usd: 0.0,
            from_peer: from_peer.to_string(),
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
                Some(true) => {
                    // Task completed successfully
                    task.state = WorkerTaskState::Completed;
                    task.child = None;
                    task.pid = None;

                    let msg = build_handoff_message(
                        &task_id,
                        "Task completed successfully",
                        &[],
                        None,
                        task.cost_usd,
                        task.tokens_used,
                        &self.identity,
                    );
                    messages.push((peer, msg));
                }
                Some(false) => {
                    // Task failed
                    task.state = WorkerTaskState::Failed;
                    task.child = None;
                    task.pid = None;

                    let msg = build_failure_message(
                        &task_id,
                        "Task exited with non-zero status",
                        task.cost_usd,
                        task.tokens_used,
                        &self.identity,
                    );
                    messages.push((peer, msg));
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
                Some(build_failure_message(
                    task_id,
                    "Stopped by controller",
                    task.cost_usd,
                    task.tokens_used,
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

    /// Number of currently running tasks.
    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|t| t.state == WorkerTaskState::Running)
            .count()
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

    #[test]
    fn worker_rejects_duplicate_task() {
        let mut worker = RemoteWorker::new("test-peer");
        // First accept will fail because `claude` binary likely doesn't exist in test,
        // but we can test the duplicate check separately.
        let ctx = DelegationContext::default();

        // Simulate a task already existing
        worker.tasks.insert(
            "t_1".into(),
            WorkerTask {
                task_id: "t_1".into(),
                prompt: "test".into(),
                cwd: ".".into(),
                context: ctx.clone(),
                state: WorkerTaskState::Running,
                child: None,
                pid: None,
                start_time: Instant::now(),
                last_status_sent: Instant::now(),
                tokens_used: 0,
                cost_usd: 0.0,
                from_peer: "peer-a".into(),
            },
        );

        let result = worker.accept_task("t_1", "test", None, ctx, "peer-a");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[test]
    fn worker_running_count() {
        let mut worker = RemoteWorker::new("test-peer");
        assert_eq!(worker.running_count(), 0);

        worker.tasks.insert(
            "t_1".into(),
            WorkerTask {
                task_id: "t_1".into(),
                prompt: "test".into(),
                cwd: ".".into(),
                context: DelegationContext::default(),
                state: WorkerTaskState::Running,
                child: None,
                pid: None,
                start_time: Instant::now(),
                last_status_sent: Instant::now(),
                tokens_used: 0,
                cost_usd: 0.0,
                from_peer: "peer-a".into(),
            },
        );
        assert_eq!(worker.running_count(), 1);

        worker.tasks.get_mut("t_1").unwrap().state = WorkerTaskState::Completed;
        assert_eq!(worker.running_count(), 0);
    }

    #[test]
    fn handle_stop_interrupt() {
        let mut worker = RemoteWorker::new("test-peer");
        worker.tasks.insert(
            "t_1".into(),
            WorkerTask {
                task_id: "t_1".into(),
                prompt: "test".into(),
                cwd: ".".into(),
                context: DelegationContext::default(),
                state: WorkerTaskState::Running,
                child: None,
                pid: None,
                start_time: Instant::now(),
                last_status_sent: Instant::now(),
                tokens_used: 100,
                cost_usd: 0.05,
                from_peer: "peer-a".into(),
            },
        );

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
        worker.tasks.insert(
            "t_1".into(),
            WorkerTask {
                task_id: "t_1".into(),
                prompt: "test".into(),
                cwd: ".".into(),
                context: DelegationContext::default(),
                state: WorkerTaskState::Running,
                child: None,
                pid: None,
                start_time: Instant::now(),
                last_status_sent: Instant::now(),
                tokens_used: 500,
                cost_usd: 0.10,
                from_peer: "peer-a".into(),
            },
        );

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
                task_id: "t_1".into(),
                prompt: "test".into(),
                cwd: ".".into(),
                context: DelegationContext::default(),
                state: WorkerTaskState::Completed,
                child: None,
                pid: None,
                start_time: Instant::now() - std::time::Duration::from_secs(3600),
                last_status_sent: Instant::now(),
                tokens_used: 0,
                cost_usd: 0.0,
                from_peer: "peer-a".into(),
            },
        );

        assert_eq!(worker.tasks.len(), 1);
        worker.cleanup_finished(1800); // 30 min threshold
        assert_eq!(worker.tasks.len(), 0); // removed (1h old > 30min threshold)
    }
}

//! The supervisor's real side effects (#345, #570): mailbox assignment
//! through the bus, the verifier shell, and spawning a task's session.
//!
//! In the library rather than the binary so `claudectl supervisor work` and
//! the headless daemon run the same actuator.

/// Real side effects for the supervisor's actuator (#345). Routes
/// `AssignViaMailbox` through the bus's publish path (sanitized body,
/// hop guard from PR2). `spawn_session` is stubbed for this PR — the
/// `launch::launch` path requires an attached terminal and is wired in
/// PR5 alongside the verifier runner. Until then, the spawn fallback
/// path returns a synthetic error and the reconciler will retry on
/// the next tick.
#[cfg(feature = "bus")]
pub struct LiveSideEffects;

/// The real verifier runners (#574), shared by every side-effect surface so a
/// bus-less build verifies tasks the same way.
pub struct Verifiers;

impl crate::coord::verify::VerifierBackend for Verifiers {
    fn run_shell(
        &self,
        cwd: &std::path::Path,
        command: &str,
        timeout: std::time::Duration,
    ) -> Result<crate::coord::verify::ShellResult, String> {
        // /bin/sh -c so users can pipe / chain / quote naturally.
        // Inheriting env mirrors what the user would see running by hand.
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn verifier shell: {e}"))?;
        // Drain both pipes while the command runs. Reading only after exit
        // deadlocked any verifier that printed more than a pipe buffer
        // (`cargo test` does): it blocked on write, and this waited on it.
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let start = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() >= timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("shell verifier timed out after {timeout:?}"));
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => return Err(format!("wait verifier shell: {e}")),
            }
        };
        let mut combined = stdout.join().unwrap_or_default();
        combined.push_str(&stderr.join().unwrap_or_default());
        Ok(crate::coord::verify::ShellResult {
            exit_code: status.code().unwrap_or(-1),
            combined_output: combined,
        })
    }
    /// `brain = "…"`: the configured local model, through the same client the
    /// brain uses (#574). No configured brain is an error, so the verifier
    /// records a FAIL with the reason: gates fail closed.
    fn query_brain(&self, prompt: &str) -> Result<String, String> {
        let brain = crate::config::Config::load()
            .brain
            .filter(|b| b.enabled)
            .ok_or("a brain verifier needs [brain] enabled in the claudectl config")?;
        crate::brain::client::complete(&brain, prompt)
    }

    /// `agent = "…"`: a headless `claude --print` in the task's workdir, with
    /// `--max-budget-usd` when the verifier sets one (#574). Synchronous: the
    /// supervisor waits for the verdict, up to `AGENT_VERIFIER_TIMEOUT`.
    fn run_agent(
        &self,
        cwd: &std::path::Path,
        prompt: &str,
        model: Option<&str>,
        budget_usd: Option<f64>,
    ) -> Result<crate::coord::verify::AgentResult, String> {
        // Read-only: a reviewer that could edit the work it's judging could
        // make it pass. `dontAsk` denies every tool not listed here.
        let mut args: Vec<String> = vec![
            "--print".into(),
            "--output-format".into(),
            "json".into(),
            "--permission-mode".into(),
            "dontAsk".into(),
            "--allowedTools".into(),
            AGENT_VERIFIER_TOOLS.into(),
        ];
        if let Some(m) = model {
            args.push("--model".into());
            args.push(m.into());
        }
        if let Some(b) = budget_usd {
            args.push("--max-budget-usd".into());
            args.push(format!("{b}"));
        }
        // The prompt goes on stdin: `--allowedTools` takes a variable number
        // of values and would swallow a trailing positional prompt.
        let prompt = format!(
            "{prompt}\n\nYou are reviewing the work in the current directory. You can \
             read files and inspect git history but not change anything. Start your \
             answer with PASS or FAIL, then the reasons."
        );
        let mut child = std::process::Command::new(claudectl_core::helpers::claude_bin())
            .args(&args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn agent verifier: {e}"))?;
        // Read stdout on a thread so a large reply can't fill the pipe while
        // this waits on the process.
        let reader = drain(child.stdout.take());
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin
                .write_all(prompt.as_bytes())
                .map_err(|e| format!("write agent verifier prompt: {e}"))?;
        }
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() >= AGENT_VERIFIER_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "agent verifier timed out after {AGENT_VERIFIER_TIMEOUT:?}"
                    ));
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
                Err(e) => return Err(format!("wait agent verifier: {e}")),
            }
        }
        let out = reader.join().unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_str(out.trim())
            .map_err(|_| "agent verifier wrote no result".to_string())?;
        if parsed.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
            return Err(format!(
                "agent verifier failed: {}",
                parsed
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown error")
            ));
        }
        Ok(crate::coord::verify::AgentResult {
            reply: parsed
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or_default()
                .to_string(),
            cost_usd: parsed
                .get("total_cost_usd")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
        })
    }
}

/// What an agent verifier may use: reading and git inspection, nothing that
/// writes.
pub const AGENT_VERIFIER_TOOLS: &str =
    "Read,Grep,Glob,Bash(git diff:*),Bash(git log:*),Bash(git show:*),Bash(git status:*)";

/// Read a child's pipe to the end on its own thread.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// How long an agent verifier may run before it's killed and counted a FAIL.
pub const AGENT_VERIFIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);

macro_rules! delegate_verifiers {
    ($t:ty) => {
        impl crate::coord::verify::VerifierBackend for $t {
            fn run_shell(
                &self,
                cwd: &std::path::Path,
                command: &str,
                timeout: std::time::Duration,
            ) -> Result<crate::coord::verify::ShellResult, String> {
                Verifiers.run_shell(cwd, command, timeout)
            }
            fn query_brain(&self, prompt: &str) -> Result<String, String> {
                Verifiers.query_brain(prompt)
            }
            fn run_agent(
                &self,
                cwd: &std::path::Path,
                prompt: &str,
                model: Option<&str>,
                budget_usd: Option<f64>,
            ) -> Result<crate::coord::verify::AgentResult, String> {
                Verifiers.run_agent(cwd, prompt, model, budget_usd)
            }
        }
    };
}

#[cfg(feature = "bus")]
delegate_verifiers!(LiveSideEffects);

#[cfg(feature = "bus")]
impl crate::coord::actuator::SideEffects for LiveSideEffects {
    fn publish_assignment(
        &self,
        role: &str,
        task_id: &str,
        prompt: &str,
        hop_count: u32,
    ) -> Result<String, String> {
        let conn = crate::bus::store::open()?;
        let subject = format!("task.assigned.{task_id}");
        let body = crate::bus::policy::sanitize_body(prompt);
        crate::bus::store::insert_message(
            &conn,
            &subject,
            "task",
            Some("supervisor"),
            Some(role),
            None,
            &body,
            "normal",
            hop_count,
        )
    }
    fn prepare_workdir(
        &self,
        task: &crate::coord::tasks::TaskRow,
    ) -> Result<std::path::PathBuf, String> {
        crate::coord::spawn::prepare_workdir(task)
    }
    fn spawn_session(&self, req: &crate::coord::spawn::SpawnRequest) -> Result<u32, String> {
        crate::coord::spawn::spawn(req)
    }
    fn pid_alive(&self, pid: u32) -> bool {
        crate::coord::spawn::pid_alive(pid)
    }
    fn cleanup_workdir(&self, task: &crate::coord::tasks::TaskRow, workdir: &std::path::Path) {
        let _ = crate::coord::spawn::cleanup_workdir(task, workdir);
    }
    fn read_result(&self, attempt_id: &str) -> Option<crate::coord::spawn::RunResult> {
        crate::coord::spawn::read_result(attempt_id)
    }
}

/// Bus-less fallback so the supervisor can run in a `--no-default-
/// features --features hive` build. Mailbox assignment is unavailable;
/// the actuator surfaces a clean error instead of panicking.
#[cfg(not(feature = "bus"))]
pub struct NoopSideEffects;
#[cfg(not(feature = "bus"))]
delegate_verifiers!(NoopSideEffects);
#[cfg(not(feature = "bus"))]
impl crate::coord::actuator::SideEffects for NoopSideEffects {
    fn publish_assignment(
        &self,
        _role: &str,
        _task_id: &str,
        _prompt: &str,
        _hop_count: u32,
    ) -> Result<String, String> {
        Err("bus feature not compiled in; AssignViaMailbox is unavailable".into())
    }
    // Spawning needs no bus, so a bus-less build runs tasks too (#570).
    fn prepare_workdir(
        &self,
        task: &crate::coord::tasks::TaskRow,
    ) -> Result<std::path::PathBuf, String> {
        crate::coord::spawn::prepare_workdir(task)
    }
    fn spawn_session(&self, req: &crate::coord::spawn::SpawnRequest) -> Result<u32, String> {
        crate::coord::spawn::spawn(req)
    }
    fn pid_alive(&self, pid: u32) -> bool {
        crate::coord::spawn::pid_alive(pid)
    }
    fn cleanup_workdir(&self, task: &crate::coord::tasks::TaskRow, workdir: &std::path::Path) {
        let _ = crate::coord::spawn::cleanup_workdir(task, workdir);
    }
    fn read_result(&self, attempt_id: &str) -> Option<crate::coord::spawn::RunResult> {
        crate::coord::spawn::read_result(attempt_id)
    }
}

/// What one supervisor pass did.
#[derive(Debug, Default)]
pub struct TickReport {
    pub emitted: usize,
    pub actuated: usize,
    pub errors: Vec<String>,
}

/// One supervisor pass: read tasks and the live sensor, carry out the
/// actions. The same pass the headless daemon runs, without anything else
/// the daemon does to sessions it didn't start.
pub fn tick_once(supervisor: &super::supervisor::Supervisor) -> Result<TickReport, String> {
    let mut conn = super::store::open()?;
    let actions = supervisor.tick(&conn, &super::spawn::LiveSensors)?;
    #[cfg(feature = "bus")]
    let fx = LiveSideEffects;
    #[cfg(not(feature = "bus"))]
    let fx = NoopSideEffects;
    let mut report = TickReport {
        emitted: actions.len(),
        ..TickReport::default()
    };
    for action in &actions {
        match super::actuator::apply(&mut conn, &fx, action) {
            Ok(()) => report.actuated += 1,
            Err(e) => report.errors.push(e),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::verify::VerifierBackend;
    use std::time::Duration;

    /// A verifier that prints more than a pipe buffer used to deadlock: it
    /// blocked on write while the runner waited for it to exit.
    #[test]
    fn a_verifier_that_prints_a_megabyte_finishes() {
        let r = Verifiers
            .run_shell(
                &std::env::temp_dir(),
                "head -c 1048576 /dev/zero | tr '\\0' x; echo done >&2; exit 3",
                Duration::from_secs(20),
            )
            .unwrap();
        assert_eq!(r.exit_code, 3);
        assert_eq!(r.combined_output.len(), 1048576 + "done\n".len());
    }

    #[test]
    fn a_verifier_past_its_timeout_is_killed() {
        let err = Verifiers
            .run_shell(
                &std::env::temp_dir(),
                "sleep 30",
                Duration::from_millis(300),
            )
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn the_agent_verifier_cannot_write() {
        for t in AGENT_VERIFIER_TOOLS.split(',') {
            assert!(
                !["Edit", "Write", "NotebookEdit", "Bash"].contains(&t),
                "{t} would let a reviewer change what it reviews"
            );
        }
    }
}

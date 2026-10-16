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
#[cfg(feature = "bus")]
impl crate::coord::verify::VerifierBackend for LiveSideEffects {
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
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn verifier shell: {e}"))?;
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    use std::io::Read;
                    let mut combined = String::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = stdout.read_to_string(&mut combined);
                    }
                    if let Some(mut stderr) = child.stderr.take() {
                        let _ = stderr.read_to_string(&mut combined);
                    }
                    return Ok(crate::coord::verify::ShellResult {
                        exit_code: status.code().unwrap_or(-1),
                        combined_output: combined,
                    });
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        return Err(format!("shell verifier timed out after {timeout:?}"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(format!("wait verifier shell: {e}")),
            }
        }
    }
    fn query_brain(&self, _prompt: &str) -> Result<String, String> {
        // The brain client surface needs more plumbing than fits in
        // this PR (it currently runs as an inflight task inside the
        // engine, not a sync request/response). Return a structured
        // error so the actuator marks the verifier as FAIL with a
        // clear message — gates fail closed, not open.
        Err(
            "live brain verifier not yet wired; declare a `run` verifier or stub the brain backend"
                .into(),
        )
    }
    fn run_agent(
        &self,
        _prompt: &str,
        _model: Option<&str>,
        _budget_usd: Option<f64>,
    ) -> Result<crate::coord::verify::AgentResult, String> {
        Err("live agent verifier not yet wired; headless `claude -p` adapter is a follow-up".into())
    }
}
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
impl crate::coord::verify::VerifierBackend for NoopSideEffects {
    fn run_shell(
        &self,
        _cwd: &std::path::Path,
        _command: &str,
        _timeout: std::time::Duration,
    ) -> Result<crate::coord::verify::ShellResult, String> {
        Err("verifier shell unavailable in this build".into())
    }
    fn query_brain(&self, _prompt: &str) -> Result<String, String> {
        Err("verifier brain unavailable in this build".into())
    }
    fn run_agent(
        &self,
        _prompt: &str,
        _model: Option<&str>,
        _budget_usd: Option<f64>,
    ) -> Result<crate::coord::verify::AgentResult, String> {
        Err("verifier agent unavailable in this build".into())
    }
}
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

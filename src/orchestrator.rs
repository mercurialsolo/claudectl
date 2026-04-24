use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A task definition from the tasks file.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TaskDef {
    pub name: String,
    pub cwd: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub resume: Option<String>,
    #[serde(default)]
    pub retries: Option<u32>,
    /// Remote peer to delegate this task to (None = local execution).
    #[serde(default)]
    pub peer: Option<String>,
}

/// Task file containing a list of tasks.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFile {
    pub tasks: Vec<TaskDef>,
    #[serde(default)]
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskState {
    Pending,
    RetryQueued(String),
    Running,
    Completed,
    Failed(String),
    Skipped(String),
    Aborted(String),
}

impl TaskState {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed(_) | Self::Skipped(_) | Self::Aborted(_)
        )
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::RetryQueued(_) => "retrying",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
            Self::Skipped(_) => "skipped",
            Self::Aborted(_) => "aborted",
        }
    }

    fn message(&self) -> Option<&str> {
        match self {
            Self::RetryQueued(msg)
            | Self::Failed(msg)
            | Self::Skipped(msg)
            | Self::Aborted(msg) => Some(msg),
            _ => None,
        }
    }
}

type SharedTail = Arc<Mutex<VecDeque<String>>>;

#[derive(Debug, Clone, Serialize)]
struct AttemptArtifact {
    attempt: u32,
    pid: Option<u32>,
    stdout_log: Option<String>,
    stderr_log: Option<String>,
    outcome: Option<String>,
    duration_secs: Option<u64>,
}

struct TaskRun {
    def: TaskDef,
    state: TaskState,
    pid: Option<u32>,
    start_time: Option<Instant>,
    child: Option<Child>,
    stdout_log: Option<PathBuf>,
    stderr_log: Option<PathBuf>,
    log_tail: SharedTail,
    attempts_started: u32,
    max_attempts: u32,
    next_retry_at: Option<Instant>,
    attempts: Vec<AttemptArtifact>,
}

struct LaunchedTask {
    child: Child,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
    log_tail: SharedTail,
}

#[derive(Default)]
struct RunCounts {
    completed: usize,
    running: usize,
    pending: usize,
    retrying: usize,
    failed: usize,
    skipped: usize,
    aborted: usize,
}

#[derive(Serialize)]
struct RunReport {
    status: String,
    parallel: bool,
    generated_at_ms: u128,
    logs_dir: String,
    counts: RunReportCounts,
    tasks: Vec<TaskReport>,
}

#[derive(Serialize)]
struct RunReportCounts {
    total: usize,
    completed: usize,
    running: usize,
    pending: usize,
    retrying: usize,
    failed: usize,
    skipped: usize,
    aborted: usize,
}

#[derive(Serialize)]
struct TaskReport {
    name: String,
    status: String,
    message: Option<String>,
    cwd: Option<String>,
    prompt: String,
    depends_on: Vec<String>,
    resume: Option<String>,
    attempts_started: u32,
    max_attempts: u32,
    running_pid: Option<u32>,
    duration_secs: Option<u64>,
    latest_stdout_log: Option<String>,
    latest_stderr_log: Option<String>,
    recent_output: Vec<String>,
    attempts: Vec<AttemptArtifact>,
}

/// Convert decomposed tasks into a TaskFile compatible with the orchestrator.
pub fn decomposition_to_task_file(
    tasks: Vec<crate::brain::client::DecomposedTask>,
    cwd: &str,
) -> TaskFile {
    TaskFile {
        tasks: tasks
            .into_iter()
            .map(|t| TaskDef {
                name: t.name,
                cwd: Some(cwd.to_string()),
                prompt: t.prompt,
                depends_on: t.depends_on,
                resume: None,
                retries: None,
                peer: None,
            })
            .collect(),
        retries: None,
    }
}

/// Load tasks from a JSON file.
pub fn load_tasks(path: &str) -> io::Result<TaskFile> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Run tasks with dependency resolution and parallel execution.
pub fn run_tasks(task_file: TaskFile, parallel: bool) -> io::Result<()> {
    validate_task_file(&task_file)?;

    let mut tasks: Vec<TaskRun> = task_file
        .tasks
        .into_iter()
        .map(|def| TaskRun {
            max_attempts: resolved_max_attempts(task_file.retries, def.retries),
            def,
            state: TaskState::Pending,
            pid: None,
            start_time: None,
            child: None,
            stdout_log: None,
            stderr_log: None,
            log_tail: Arc::new(Mutex::new(VecDeque::new())),
            attempts_started: 0,
            next_retry_at: None,
            attempts: Vec::new(),
        })
        .collect();

    let total = tasks.len();
    let mode = if parallel { "parallel" } else { "sequential" };
    println!("Running {total} tasks ({mode}):");
    println!();
    print_task_plan(&tasks);
    println!();

    let run_dir = create_run_dir()?;
    let status_path = run_dir.join("status.json");
    let summary_path = run_dir.join("summary.json");
    let print_lock = Arc::new(Mutex::new(()));
    println!("Logs: {}", run_dir.display());
    println!();

    let cancel_requested = Arc::new(AtomicBool::new(false));
    install_abort_handler(Arc::clone(&cancel_requested));
    let mut abort_notified = false;

    loop {
        if cancel_requested.load(Ordering::SeqCst) {
            if !abort_notified {
                println!();
                println!("Abort requested — stopping running tasks and cancelling pending work...");
                abort_notified = true;
            }
            abort_tasks(&mut tasks);
        }

        mark_dependency_failures(&mut tasks);

        let done_count = tasks.iter().filter(|t| t.state.is_terminal()).count();

        let completed: HashSet<String> = tasks
            .iter()
            .filter(|task| matches!(task.state, TaskState::Completed))
            .map(|task| task.def.name.clone())
            .collect();

        // Build output map for template expansion (immutable snapshot)
        let completed_outputs: HashMap<String, PathBuf> = tasks
            .iter()
            .filter(|t| matches!(t.state, TaskState::Completed))
            .filter_map(|t| {
                t.stdout_log
                    .as_ref()
                    .map(|p| (t.def.name.clone(), p.clone()))
            })
            .collect();

        let mut running_count = tasks
            .iter()
            .filter(|task| matches!(task.state, TaskState::Running))
            .count();

        for task in &mut tasks {
            if !matches!(task.state, TaskState::Pending | TaskState::RetryQueued(_)) {
                continue;
            }

            if cancel_requested.load(Ordering::SeqCst) {
                break;
            }

            if task
                .next_retry_at
                .is_some_and(|ready_at| Instant::now() < ready_at)
            {
                continue;
            }

            if !task
                .def
                .depends_on
                .iter()
                .all(|dep| completed.contains(dep))
            {
                continue;
            }

            if !parallel && running_count > 0 {
                break;
            }

            // Skip tasks targeted at a remote peer — the relay handles these
            if task.def.peer.is_some() {
                let peer = task.def.peer.as_deref().unwrap_or("unknown");
                task.state = TaskState::Skipped(format!(
                    "remote delegation to peer '{peer}' requires relay serve mode"
                ));
                println!(
                    "  Skipped: {} (remote peer '{peer}' — use relay serve for delegation)",
                    task.def.name
                );
                continue;
            }

            let attempt = task.attempts_started + 1;

            // Expand {{name.stdout}} templates in the prompt
            let launch_def = if task.def.prompt.contains("{{") {
                match expand_prompt_templates(&task.def.prompt, &completed_outputs) {
                    Ok(expanded) => {
                        let mut def = task.def.clone();
                        def.prompt = expanded;
                        def
                    }
                    Err(e) => {
                        task.state = TaskState::Failed(format!("template expansion error: {e}"));
                        println!("  {} — template error: {e}", task.def.name);
                        continue;
                    }
                }
            } else {
                task.def.clone()
            };

            match launch_claude_session(&launch_def, &run_dir, Arc::clone(&print_lock), attempt) {
                Ok(launched) => {
                    let pid = launched.child.id();
                    task.attempts_started = attempt;
                    task.pid = Some(pid);
                    task.start_time = Some(Instant::now());
                    task.stdout_log = Some(launched.stdout_log.clone());
                    task.stderr_log = Some(launched.stderr_log.clone());
                    task.log_tail = launched.log_tail;
                    task.child = Some(launched.child);
                    task.state = TaskState::Running;
                    task.next_retry_at = None;
                    task.attempts.push(AttemptArtifact {
                        attempt,
                        pid: Some(pid),
                        stdout_log: Some(launched.stdout_log.display().to_string()),
                        stderr_log: Some(launched.stderr_log.display().to_string()),
                        outcome: None,
                        duration_secs: None,
                    });

                    println!(
                        "  [{done_count}/{total}] Started: {} (attempt {}/{}, PID {})",
                        task.def.name, attempt, task.max_attempts, pid
                    );
                    running_count += 1;
                }
                Err(e) => {
                    task.attempts_started = attempt;
                    let reason = format!("launch error: {e}");
                    task.attempts.push(AttemptArtifact {
                        attempt,
                        pid: None,
                        stdout_log: None,
                        stderr_log: None,
                        outcome: Some(reason.clone()),
                        duration_secs: None,
                    });

                    if queue_retry(task, &reason) {
                        println!(
                            "  Retry queued: {} (attempt {}/{}) — {}",
                            task.def.name,
                            task.attempts_started + 1,
                            task.max_attempts,
                            reason
                        );
                    } else {
                        println!("  Failed: {} ({reason})", task.def.name);
                        task.state = TaskState::Failed(reason);
                    }
                }
            }
        }

        for task in &mut tasks {
            if task.state != TaskState::Running {
                continue;
            }

            let wait_result = if let Some(child) = task.child.as_mut() {
                child.try_wait()
            } else {
                Ok(None)
            };

            match wait_result {
                Ok(Some(status)) => {
                    let elapsed = task.start_time.map(|started| started.elapsed().as_secs());
                    task.child = None;
                    task.pid = None;
                    task.start_time = None;
                    if status.success() {
                        set_latest_attempt_outcome(task, "completed".into(), elapsed);
                        task.state = TaskState::Completed;
                        println!(
                            "  [+] Finished: {} ({}s)",
                            task.def.name,
                            elapsed.unwrap_or(0)
                        );
                    } else {
                        let reason = format!("exit {}", format_exit_status(status));
                        set_latest_attempt_outcome(task, reason.clone(), elapsed);
                        if queue_retry(task, &reason) {
                            println!(
                                "  [~] Retry queued: {} (attempt {}/{}) — {}",
                                task.def.name,
                                task.attempts_started + 1,
                                task.max_attempts,
                                reason
                            );
                            print_task_tail(task);
                        } else {
                            println!("  [!] Failed: {} ({reason})", task.def.name);
                            print_task_tail(task);
                            task.state = TaskState::Failed(reason);
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    let reason = format!("wait error: {e}");
                    let elapsed = task.start_time.map(|started| started.elapsed().as_secs());
                    task.child = None;
                    task.pid = None;
                    task.start_time = None;
                    set_latest_attempt_outcome(task, reason.clone(), elapsed);
                    if queue_retry(task, &reason) {
                        println!(
                            "  [~] Retry queued: {} (attempt {}/{}) — {}",
                            task.def.name,
                            task.attempts_started + 1,
                            task.max_attempts,
                            reason
                        );
                        print_task_tail(task);
                    } else {
                        println!("  [!] Failed: {} ({reason})", task.def.name);
                        print_task_tail(task);
                        task.state = TaskState::Failed(reason);
                    }
                }
            }
        }

        print_status(&tasks);
        write_run_report(&status_path, &run_dir, &tasks, parallel)?;

        if tasks.iter().all(|task| task.state.is_terminal()) {
            break;
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    println!();
    write_run_report(&summary_path, &run_dir, &tasks, parallel)?;
    println!("Summary: {}", summary_path.display());
    print_final_summary(&tasks);

    #[cfg(target_os = "macos")]
    {
        let counts = compute_counts(&tasks);
        let msg = if counts.failed == 0 && counts.aborted == 0 && counts.skipped == 0 {
            format!("All {total} tasks completed")
        } else {
            format!(
                "{} completed, {} failed, {} skipped, {} aborted",
                counts.completed, counts.failed, counts.skipped, counts.aborted
            )
        };
        let _ = Command::new("osascript")
            .args([
                "-e",
                &format!("display notification \"{msg}\" with title \"claudectl run\""),
            ])
            .spawn();
    }

    let counts = compute_counts(&tasks);
    if counts.failed == 0 && counts.aborted == 0 && counts.skipped == 0 {
        Ok(())
    } else if counts.aborted > 0 {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!(
                "{} failed, {} skipped, {} aborted",
                counts.failed, counts.skipped, counts.aborted
            ),
        ))
    } else {
        Err(io::Error::other(format!(
            "{} failed, {} skipped",
            counts.failed, counts.skipped
        )))
    }
}

fn print_task_plan(tasks: &[TaskRun]) {
    for (i, task) in tasks.iter().enumerate() {
        let deps = if task.def.depends_on.is_empty() {
            String::new()
        } else {
            format!(" (after {})", task.def.depends_on.join(", "))
        };
        let cwd = task.def.cwd.as_deref().unwrap_or(".");
        let retries = if task.max_attempts > 1 {
            format!(", {} retries", task.max_attempts - 1)
        } else {
            String::new()
        };
        println!(
            "  {}. {}{} in {}{}",
            i + 1,
            task.def.name,
            deps,
            cwd,
            retries
        );
    }
}

fn validate_task_file(task_file: &TaskFile) -> io::Result<()> {
    let mut seen = HashSet::new();
    let names: HashSet<String> = task_file
        .tasks
        .iter()
        .map(|task| task.name.clone())
        .collect();

    for task in &task_file.tasks {
        if !seen.insert(task.name.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Duplicate task name: '{}'", task.name),
            ));
        }

        for dep in &task.depends_on {
            if !names.contains(dep) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Task '{}' depends on '{}' which doesn't exist",
                        task.name, dep
                    ),
                ));
            }
        }
    }

    validate_template_references(&task_file.tasks)?;
    validate_acyclic_graph(&task_file.tasks)
}

fn validate_acyclic_graph(tasks: &[TaskDef]) -> io::Result<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Visiting,
        Done,
    }

    fn visit(
        name: &str,
        tasks: &HashMap<String, &TaskDef>,
        states: &mut HashMap<String, VisitState>,
        stack: &mut Vec<String>,
    ) -> io::Result<()> {
        if let Some(state) = states.get(name) {
            if *state == VisitState::Done {
                return Ok(());
            }
            if *state == VisitState::Visiting {
                stack.push(name.to_string());
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Task dependencies contain a cycle: {}", stack.join(" -> ")),
                ));
            }
        }

        states.insert(name.to_string(), VisitState::Visiting);
        stack.push(name.to_string());

        if let Some(task) = tasks.get(name) {
            for dep in &task.depends_on {
                visit(dep, tasks, states, stack)?;
            }
        }

        stack.pop();
        states.insert(name.to_string(), VisitState::Done);
        Ok(())
    }

    let task_map: HashMap<String, &TaskDef> =
        tasks.iter().map(|task| (task.name.clone(), task)).collect();
    let mut states = HashMap::new();

    for task in tasks {
        let mut stack = Vec::new();
        visit(&task.name, &task_map, &mut states, &mut stack)?;
    }

    Ok(())
}

fn resolved_max_attempts(default_retries: Option<u32>, task_retries: Option<u32>) -> u32 {
    1 + task_retries.or(default_retries).unwrap_or(0)
}

fn install_abort_handler(cancel_requested: Arc<AtomicBool>) {
    if let Err(err) = ctrlc::set_handler(move || {
        cancel_requested.store(true, Ordering::SeqCst);
    }) {
        crate::logger::log(
            "WARN",
            &format!("Could not install orchestration abort handler: {err}"),
        );
    }
}

fn mark_dependency_failures(tasks: &mut [TaskRun]) {
    let failed_dependencies: HashSet<String> = tasks
        .iter()
        .filter(|task| {
            matches!(
                task.state,
                TaskState::Failed(_) | TaskState::Skipped(_) | TaskState::Aborted(_)
            )
        })
        .map(|task| task.def.name.clone())
        .collect();

    for task in tasks {
        if !matches!(task.state, TaskState::Pending | TaskState::RetryQueued(_)) {
            continue;
        }

        let deps_failed: Vec<String> = task
            .def
            .depends_on
            .iter()
            .filter(|dep| failed_dependencies.contains(dep.as_str()))
            .cloned()
            .collect();

        if !deps_failed.is_empty() {
            task.state =
                TaskState::Skipped(format!("dependency failed: {}", deps_failed.join(", ")));
        }
    }
}

fn abort_tasks(tasks: &mut [TaskRun]) {
    for task in tasks {
        match task.state {
            TaskState::Running => {
                let elapsed = task.start_time.map(|started| started.elapsed().as_secs());
                if let Some(child) = task.child.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                task.child = None;
                task.pid = None;
                task.start_time = None;
                set_latest_attempt_outcome(task, "aborted".into(), elapsed);
                task.state = TaskState::Aborted("aborted by user".into());
            }
            TaskState::Pending | TaskState::RetryQueued(_) => {
                task.state = TaskState::Aborted("not started (aborted by user)".into());
                task.next_retry_at = None;
            }
            _ => {}
        }
    }
}

fn queue_retry(task: &mut TaskRun, reason: &str) -> bool {
    if task.attempts_started >= task.max_attempts {
        return false;
    }

    let delay_secs = u64::from(task.attempts_started.min(3));
    let next_attempt = task.attempts_started + 1;
    task.state = TaskState::RetryQueued(format!(
        "{reason}; retrying attempt {next_attempt}/{}",
        task.max_attempts
    ));
    task.next_retry_at = Some(Instant::now() + Duration::from_secs(delay_secs.max(1)));
    task.child = None;
    task.pid = None;
    task.start_time = None;
    true
}

fn launch_claude_session(
    task: &TaskDef,
    run_dir: &Path,
    print_lock: Arc<Mutex<()>>,
    attempt: u32,
) -> io::Result<LaunchedTask> {
    let cwd = task.cwd.as_deref().unwrap_or(".");

    let mut args = vec!["--print".to_string()];
    if let Some(ref resume) = task.resume {
        args.push("--resume".into());
        args.push(resume.clone());
    }
    args.push(task.prompt.clone());

    let mut child = Command::new("claude")
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let slug = sanitize_task_name(&task.name);
    let stdout_log = run_dir.join(format!("{slug}.attempt-{attempt}.stdout.log"));
    let stderr_log = run_dir.join(format!("{slug}.attempt-{attempt}.stderr.log"));
    let log_tail = Arc::new(Mutex::new(VecDeque::new()));

    if let Some(stdout) = child.stdout.take() {
        spawn_log_pump(
            stdout,
            task.name.clone(),
            "stdout",
            stdout_log.clone(),
            Arc::clone(&log_tail),
            Arc::clone(&print_lock),
            false,
        );
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_pump(
            stderr,
            task.name.clone(),
            "stderr",
            stderr_log.clone(),
            Arc::clone(&log_tail),
            print_lock,
            true,
        );
    }

    Ok(LaunchedTask {
        child,
        stdout_log,
        stderr_log,
        log_tail,
    })
}

fn print_status(tasks: &[TaskRun]) {
    let counts = compute_counts(tasks);
    let done = counts.completed + counts.failed + counts.skipped + counts.aborted;
    let total = tasks.len();

    let running: Vec<_> = tasks
        .iter()
        .filter(|task| matches!(task.state, TaskState::Running))
        .take(3)
        .map(|task| {
            let elapsed = task
                .start_time
                .map(|started| format!("{}s", started.elapsed().as_secs()))
                .unwrap_or_else(|| "?s".to_string());
            format!("{} {elapsed}", task.def.name)
        })
        .collect();

    let mut line = format!("\r  [{done}/{total}]");

    if counts.running > 0 {
        line.push_str(&format!(" {} running", counts.running));
    }
    if counts.pending > 0 {
        line.push_str(&format!(" {} pending", counts.pending));
    }
    if counts.retrying > 0 {
        line.push_str(&format!(" {} retrying", counts.retrying));
    }
    if counts.failed > 0 {
        line.push_str(&format!(" {} failed", counts.failed));
    }
    if counts.skipped > 0 {
        line.push_str(&format!(" {} skipped", counts.skipped));
    }
    if counts.aborted > 0 {
        line.push_str(&format!(" {} aborted", counts.aborted));
    }
    if !running.is_empty() {
        line.push_str(&format!(" | {}", running.join(", ")));
    }

    // Pad to terminal width to clear previous line, capped at 200
    let width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(120);
    let pad = width.min(200);
    eprint!("{line:<pad$}");
    let _ = io::stderr().flush();
}

fn compute_counts(tasks: &[TaskRun]) -> RunCounts {
    let mut counts = RunCounts::default();
    for task in tasks {
        match task.state {
            TaskState::Pending => counts.pending += 1,
            TaskState::RetryQueued(_) => counts.retrying += 1,
            TaskState::Running => counts.running += 1,
            TaskState::Completed => counts.completed += 1,
            TaskState::Failed(_) => counts.failed += 1,
            TaskState::Skipped(_) => counts.skipped += 1,
            TaskState::Aborted(_) => counts.aborted += 1,
        }
    }
    counts
}

fn run_status_label(tasks: &[TaskRun]) -> &'static str {
    let counts = compute_counts(tasks);
    if counts.aborted > 0 {
        "aborted"
    } else if counts.failed > 0 || counts.skipped > 0 {
        "failed"
    } else if counts.running > 0 || counts.pending > 0 || counts.retrying > 0 {
        "running"
    } else {
        "completed"
    }
}

fn write_run_report(
    path: &Path,
    run_dir: &Path,
    tasks: &[TaskRun],
    parallel: bool,
) -> io::Result<()> {
    let counts = compute_counts(tasks);
    let report = RunReport {
        status: run_status_label(tasks).to_string(),
        parallel,
        generated_at_ms: now_epoch_ms(),
        logs_dir: run_dir.display().to_string(),
        counts: RunReportCounts {
            total: tasks.len(),
            completed: counts.completed,
            running: counts.running,
            pending: counts.pending,
            retrying: counts.retrying,
            failed: counts.failed,
            skipped: counts.skipped,
            aborted: counts.aborted,
        },
        tasks: tasks.iter().map(build_task_report).collect(),
    };

    let json = serde_json::to_string_pretty(&report)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, json)
}

fn build_task_report(task: &TaskRun) -> TaskReport {
    let recent_output = task
        .log_tail
        .lock()
        .ok()
        .map(|tail| tail.iter().cloned().collect())
        .unwrap_or_default();

    TaskReport {
        name: task.def.name.clone(),
        status: task.state.label().to_string(),
        message: task.state.message().map(|msg| msg.to_string()),
        cwd: task.def.cwd.clone(),
        prompt: task.def.prompt.clone(),
        depends_on: task.def.depends_on.clone(),
        resume: task.def.resume.clone(),
        attempts_started: task.attempts_started,
        max_attempts: task.max_attempts,
        running_pid: task.pid,
        duration_secs: task
            .start_time
            .map(|started| started.elapsed().as_secs())
            .or_else(|| {
                task.attempts
                    .last()
                    .and_then(|attempt| attempt.duration_secs)
            }),
        latest_stdout_log: task
            .stdout_log
            .as_ref()
            .map(|path| path.display().to_string()),
        latest_stderr_log: task
            .stderr_log
            .as_ref()
            .map(|path| path.display().to_string()),
        recent_output,
        attempts: task.attempts.clone(),
    }
}

fn print_final_summary(tasks: &[TaskRun]) {
    println!();
    println!("Task summary:");
    for task in tasks {
        let status = task.state.label().to_ascii_uppercase();
        let duration = task
            .attempts
            .last()
            .and_then(|attempt| attempt.duration_secs)
            .map(|secs| format!("{secs}s"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<9} {} (attempts {}/{}, duration {})",
            status, task.def.name, task.attempts_started, task.max_attempts, duration
        );
        if let Some(message) = task.state.message() {
            println!("    reason: {message}");
        }
        print_task_logs(task);
        if matches!(task.state, TaskState::Failed(_) | TaskState::Aborted(_)) {
            print_task_tail(task);
        }
    }
}

fn create_run_dir() -> io::Result<PathBuf> {
    let base = std::env::current_dir()?.join(".claudectl-runs");
    fs::create_dir_all(&base)?;
    let now_ms = now_epoch_ms();
    let run_dir = base.join(format!("run-{now_ms}-{}", std::process::id()));
    fs::create_dir_all(&run_dir)?;
    Ok(run_dir)
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn sanitize_task_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }

    if out.is_empty() {
        "task".to_string()
    } else {
        out
    }
}

fn spawn_log_pump<R: std::io::Read + Send + 'static>(
    reader: R,
    task_name: String,
    stream_name: &'static str,
    log_path: PathBuf,
    log_tail: SharedTail,
    print_lock: Arc<Mutex<()>>,
    is_stderr: bool,
) {
    std::thread::spawn(move || {
        let mut log_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .ok();

        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if let Some(file) = log_file.as_mut() {
                let _ = writeln!(file, "{line}");
            }
            push_tail(&log_tail, format!("[{stream_name}] {line}"));

            let _guard = print_lock.lock().ok();
            if is_stderr {
                eprintln!("\n[{}:{}] {}", task_name, stream_name, line);
            } else {
                println!("\n[{}] {}", task_name, line);
            }
        }
    });
}

fn push_tail(log_tail: &SharedTail, line: String) {
    let Ok(mut tail) = log_tail.lock() else {
        return;
    };
    tail.push_back(line);
    while tail.len() > 12 {
        tail.pop_front();
    }
}

fn set_latest_attempt_outcome(task: &mut TaskRun, outcome: String, duration_secs: Option<u64>) {
    if let Some(attempt) = task.attempts.last_mut() {
        attempt.outcome = Some(outcome);
        attempt.duration_secs = duration_secs;
    }
}

fn format_exit_status(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string())
}

fn print_task_tail(task: &TaskRun) {
    let Ok(tail) = task.log_tail.lock() else {
        return;
    };
    if tail.is_empty() {
        return;
    }

    println!("    recent output:");
    for line in tail.iter() {
        println!("      {line}");
    }
}

fn print_task_logs(task: &TaskRun) {
    if task.attempts.is_empty() {
        return;
    }

    println!("    logs:");
    for attempt in &task.attempts {
        if attempt.stdout_log.is_none() && attempt.stderr_log.is_none() {
            continue;
        }
        println!("      attempt {}:", attempt.attempt);
        if let Some(path) = &attempt.stdout_log {
            println!("        stdout: {path}");
        }
        if let Some(path) = &attempt.stderr_log {
            println!("        stderr: {path}");
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Template substitution for cross-session data routing
// ────────────────────────────────────────────────────────────────────────────

const MAX_CAPTURED_OUTPUT: usize = 32 * 1024;

/// Extract `{{name.field}}` references from a prompt string.
fn extract_template_refs(prompt: &str) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    let mut remaining = prompt;
    while let Some(start) = remaining.find("{{") {
        let after = &remaining[start + 2..];
        if let Some(end) = after.find("}}") {
            let placeholder = after[..end].trim();
            if let Some((name, field)) = placeholder.split_once('.') {
                refs.push((name.trim().to_string(), field.trim().to_string()));
            }
            remaining = &after[end + 2..];
        } else {
            break;
        }
    }
    refs
}

/// Validate that all template references in task prompts point to existing
/// direct dependencies and use a supported field.
fn validate_template_references(tasks: &[TaskDef]) -> io::Result<()> {
    let names: HashSet<String> = tasks.iter().map(|t| t.name.clone()).collect();

    for task in tasks {
        for (ref_name, ref_field) in extract_template_refs(&task.prompt) {
            if ref_field != "stdout" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Task '{}': unsupported template field '{{{{{}.{}}}}}', only 'stdout' is supported",
                        task.name, ref_name, ref_field
                    ),
                ));
            }
            if !names.contains(&ref_name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Task '{}': template '{{{{{}.stdout}}}}' references task '{}' which doesn't exist",
                        task.name, ref_name, ref_name
                    ),
                ));
            }
            if !task.depends_on.contains(&ref_name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Task '{}': template '{{{{{}.stdout}}}}' references task '{}' which is not in depends_on",
                        task.name, ref_name, ref_name
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Expand `{{name.stdout}}` placeholders in a prompt string by reading
/// the completed task's stdout log file. Truncates to 32KB.
fn expand_prompt_templates(
    prompt: &str,
    completed_outputs: &HashMap<String, PathBuf>,
) -> io::Result<String> {
    let mut result = String::with_capacity(prompt.len());
    let mut remaining = prompt;

    while let Some(start) = remaining.find("{{") {
        result.push_str(&remaining[..start]);
        let after = &remaining[start + 2..];

        let Some(end) = after.find("}}") else {
            // No closing }} — treat literally
            result.push_str("{{");
            remaining = after;
            continue;
        };

        let placeholder = after[..end].trim();
        remaining = &after[end + 2..];

        let Some((task_name, field)) = placeholder.split_once('.') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid template '{{{{{placeholder}}}}}': expected '{{{{name.stdout}}}}'"),
            ));
        };
        let task_name = task_name.trim();
        let field = field.trim();

        if field != "stdout" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported field '{field}' in '{{{{{placeholder}}}}}'"),
            ));
        }

        let log_path = completed_outputs.get(task_name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no stdout available for task '{task_name}'"),
            )
        })?;

        let mut content = fs::read_to_string(log_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("failed to read stdout for task '{task_name}': {e}"),
            )
        })?;

        if content.len() > MAX_CAPTURED_OUTPUT {
            content.truncate(MAX_CAPTURED_OUTPUT);
            content.push_str("\n... (truncated, output exceeded 32KB)");
        }

        result.push_str(content.trim());
    }

    result.push_str(remaining);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_tasks_json() {
        let json = r#"{
            "tasks": [
                {
                    "name": "task1",
                    "prompt": "Do something",
                    "cwd": "./src",
                    "retries": 2
                },
                {
                    "name": "task2",
                    "prompt": "Do something else",
                    "depends_on": ["task1"],
                    "resume": "session-123"
                }
            ],
            "retries": 1
        }"#;

        let task_file: TaskFile = serde_json::from_str(json).unwrap();
        assert_eq!(task_file.tasks.len(), 2);
        assert_eq!(task_file.tasks[0].name, "task1");
        assert_eq!(task_file.tasks[0].cwd, Some("./src".into()));
        assert_eq!(task_file.tasks[0].retries, Some(2));
        assert_eq!(task_file.tasks[1].depends_on, vec!["task1"]);
        assert_eq!(task_file.tasks[1].resume.as_deref(), Some("session-123"));
        assert_eq!(task_file.retries, Some(1));
    }

    #[test]
    fn test_load_tasks_rejects_unsupported_budget_fields() {
        let json = r#"{
            "tasks": [
                {
                    "name": "task1",
                    "prompt": "test",
                    "budget": 2.0
                }
            ]
        }"#;

        let err = serde_json::from_str::<TaskFile>(json).unwrap_err();
        assert!(err.to_string().contains("budget"));
    }

    #[test]
    fn test_dependency_validation() {
        let task_file = TaskFile {
            tasks: vec![TaskDef {
                name: "task1".into(),
                prompt: "test".into(),
                cwd: None,
                depends_on: vec!["nonexistent".into()],
                resume: None,
                retries: None,
                peer: None,
            }],
            retries: None,
        };

        let result = run_tasks(task_file, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn test_dependency_cycle_validation() {
        let task_file = TaskFile {
            tasks: vec![
                TaskDef {
                    name: "task1".into(),
                    prompt: "test".into(),
                    cwd: None,
                    depends_on: vec!["task2".into()],
                    resume: None,
                    retries: None,
                    peer: None,
                },
                TaskDef {
                    name: "task2".into(),
                    prompt: "test".into(),
                    cwd: None,
                    depends_on: vec!["task1".into()],
                    resume: None,
                    retries: None,
                    peer: None,
                },
            ],
            retries: None,
        };

        let err = validate_task_file(&task_file).unwrap_err();
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn test_resolved_max_attempts_prefers_task_override() {
        assert_eq!(resolved_max_attempts(Some(1), None), 2);
        assert_eq!(resolved_max_attempts(Some(1), Some(3)), 4);
        assert_eq!(resolved_max_attempts(None, None), 1);
    }

    #[test]
    fn test_sanitize_task_name() {
        assert_eq!(sanitize_task_name("Update docs"), "update-docs");
        assert_eq!(sanitize_task_name("API/Test #1"), "apitest-1");
    }

    // ────────────────────────────────────────────────────────────────────────
    // Template extraction tests
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_extract_refs_empty() {
        assert!(extract_template_refs("").is_empty());
        assert!(extract_template_refs("no templates here").is_empty());
    }

    #[test]
    fn test_extract_refs_single() {
        let refs = extract_template_refs("Fix these: {{analyze.stdout}}");
        assert_eq!(refs, vec![("analyze".into(), "stdout".into())]);
    }

    #[test]
    fn test_extract_refs_multiple() {
        let refs = extract_template_refs("{{a.stdout}} and {{b.stdout}}");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].0, "a");
        assert_eq!(refs[1].0, "b");
    }

    #[test]
    fn test_extract_refs_unclosed() {
        let refs = extract_template_refs("{{foo.stdout");
        assert!(refs.is_empty());
    }

    // ────────────────────────────────────────────────────────────────────────
    // Template validation tests
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_validate_refs_valid() {
        let tasks = vec![
            TaskDef {
                name: "analyze".into(),
                prompt: "do analysis".into(),
                cwd: None,
                depends_on: vec![],
                resume: None,
                retries: None,
                peer: None,
            },
            TaskDef {
                name: "fix".into(),
                prompt: "fix: {{analyze.stdout}}".into(),
                cwd: None,
                depends_on: vec!["analyze".into()],
                resume: None,
                retries: None,
                peer: None,
            },
        ];
        assert!(validate_template_references(&tasks).is_ok());
    }

    #[test]
    fn test_validate_refs_missing_task() {
        let tasks = vec![TaskDef {
            name: "fix".into(),
            prompt: "fix: {{ghost.stdout}}".into(),
            cwd: None,
            depends_on: vec![],
            resume: None,
            retries: None,
            peer: None,
        }];
        let err = validate_template_references(&tasks).unwrap_err();
        assert!(err.to_string().contains("doesn't exist"));
    }

    #[test]
    fn test_validate_refs_not_in_depends_on() {
        let tasks = vec![
            TaskDef {
                name: "analyze".into(),
                prompt: "do analysis".into(),
                cwd: None,
                depends_on: vec![],
                resume: None,
                retries: None,
                peer: None,
            },
            TaskDef {
                name: "fix".into(),
                prompt: "fix: {{analyze.stdout}}".into(),
                cwd: None,
                depends_on: vec![], // Missing dependency
                resume: None,
                retries: None,
                peer: None,
            },
        ];
        let err = validate_template_references(&tasks).unwrap_err();
        assert!(err.to_string().contains("not in depends_on"));
    }

    #[test]
    fn test_validate_refs_unsupported_field() {
        let tasks = vec![
            TaskDef {
                name: "a".into(),
                prompt: "x".into(),
                cwd: None,
                depends_on: vec![],
                resume: None,
                retries: None,
                peer: None,
            },
            TaskDef {
                name: "b".into(),
                prompt: "{{a.stderr}}".into(),
                cwd: None,
                depends_on: vec!["a".into()],
                resume: None,
                retries: None,
                peer: None,
            },
        ];
        let err = validate_template_references(&tasks).unwrap_err();
        assert!(err.to_string().contains("only 'stdout' is supported"));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Template expansion tests
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_expand_no_templates() {
        let outputs = HashMap::new();
        let result = expand_prompt_templates("plain prompt", &outputs).unwrap();
        assert_eq!(result, "plain prompt");
    }

    #[test]
    fn test_expand_single_substitution() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("analyze.stdout.log");
        fs::write(&log, "issue 1\nissue 2\n").unwrap();

        let mut outputs = HashMap::new();
        outputs.insert("analyze".into(), log);

        let result = expand_prompt_templates("Fix these:\n{{analyze.stdout}}", &outputs).unwrap();
        assert_eq!(result, "Fix these:\nissue 1\nissue 2");
    }

    #[test]
    fn test_expand_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("big.stdout.log");
        let big = "x".repeat(MAX_CAPTURED_OUTPUT + 1000);
        fs::write(&log, &big).unwrap();

        let mut outputs = HashMap::new();
        outputs.insert("big".into(), log);

        let result = expand_prompt_templates("{{big.stdout}}", &outputs).unwrap();
        assert!(result.contains("truncated"));
        assert!(result.len() < big.len());
    }

    #[test]
    fn test_expand_missing_task() {
        let outputs = HashMap::new();
        let err = expand_prompt_templates("{{ghost.stdout}}", &outputs).unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn test_expand_unsupported_field() {
        let outputs = HashMap::new();
        let err = expand_prompt_templates("{{a.stderr}}", &outputs).unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn test_decomposition_to_task_file() {
        let tasks = vec![
            crate::brain::client::DecomposedTask {
                name: "analyze".into(),
                prompt: "analyze code".into(),
                depends_on: vec![],
            },
            crate::brain::client::DecomposedTask {
                name: "fix".into(),
                prompt: "fix issues".into(),
                depends_on: vec!["analyze".into()],
            },
        ];
        let task_file = decomposition_to_task_file(tasks, "/tmp/proj");
        assert_eq!(task_file.tasks.len(), 2);
        assert_eq!(task_file.tasks[0].name, "analyze");
        assert_eq!(task_file.tasks[0].cwd, Some("/tmp/proj".into()));
        assert_eq!(task_file.tasks[1].depends_on, vec!["analyze"]);
        // Validate the resulting task file is valid
        assert!(validate_task_file(&task_file).is_ok());
    }

    #[test]
    fn test_decomposition_empty_tasks() {
        let task_file = decomposition_to_task_file(vec![], "/tmp");
        assert!(task_file.tasks.is_empty());
    }
}

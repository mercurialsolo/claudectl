// Allow dead_code: the `Drain` knob is wired through to a flag the
// reconciler reads but doesn't yet act on (it's the bridge surface PR8
// will use for graceful shutdown).
#![allow(dead_code)]
//! `claudectl supervisor` subcommand surface (#349 / RFC §10).
//!
//! Operator-facing verbs over the coord task ledger. Every command
//! reads `~/.claudectl/coord/coord.db` directly — the bus MCP server
//! isn't required for these — so an operator can poke at fleet state
//! even when the headless daemon is down.

use clap::Subcommand;
use std::io;
use std::path::PathBuf;

use super::store;
use super::tasks::{self, NewTask, TaskState};

// Parsed once per process; `Submit` carrying every task field inline costs
// nothing worth boxing it for.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum SupervisorCommand {
    /// Submit one or more tasks from a TOML file (`--run` alias). Each
    /// `[[task]]` block becomes a `tasks` row in PENDING. The supervisor
    /// reconciler picks them up on its next tick.
    Run {
        /// Path to `tasks.toml` (RFC §4 shape).
        file: PathBuf,
        /// Print task ids without inserting them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Scaffold a starter `tasks.toml` in the current directory, with the
    /// full key set documented inline. Refuses to clobber an existing file
    /// unless `--force`.
    Init {
        /// Overwrite an existing tasks.toml.
        #[arg(long)]
        force: bool,
    },
    /// Parse + validate a `tasks.toml` without submitting anything. Reports
    /// the first problem with a clear message (missing field, dangling
    /// dependency, duplicate name) instead of failing at insert time.
    Validate {
        /// Path to the `tasks.toml` to check.
        file: PathBuf,
    },
    /// Submit a single task inline. Useful for one-shot scripts that
    /// don't want to author a TOML file.
    Submit {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cwd: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        budget_usd: Option<f64>,
        #[arg(long)]
        max_retries: Option<u32>,
        #[arg(long)]
        timeout_min: Option<u32>,
        /// Run in a new worktree of the repo at `cwd`, started from this ref
        #[arg(long)]
        git_ref: Option<String>,
        /// The worktree's branch (default `claudectl/<name>-<id>`; needs --git-ref)
        #[arg(long, requires = "git_ref")]
        branch: Option<String>,
        /// Passed to `claude --permission-mode` (default: your own settings)
        #[arg(long)]
        permission_mode: Option<String>,
        /// What the machine running it must have: tools (`node>=22`,
        /// `claude:auth`) or tags (`docker`), comma-separated
        #[arg(long, value_delimiter = ',')]
        requires: Vec<String>,
        /// Run only on this relay peer
        #[arg(long)]
        peer: Option<String>,
    },
    /// Run the supervisor loop by itself: spawn queued tasks (each in its
    /// worktree when it names a git_ref), watch their sessions, and run their
    /// verifiers. Unlike `--headless`, it touches no session it didn't start.
    Work {
        /// Seconds between passes
        #[arg(long, default_value_t = 2)]
        interval: u64,
        /// Exit once no task is pending, assigned, running or verifying
        #[arg(long)]
        until_idle: bool,
        /// Give up after this many seconds
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Compact task table: state, attempt count, role, age. Optional
    /// `--state` filter.
    Status {
        #[arg(long)]
        state: Option<String>,
        /// Show tasks as their `depends_on` tree, each under the task it
        /// waits for, with why a NEEDS_HUMAN task stopped.
        #[arg(long)]
        graph: bool,
    },
    /// Detailed view of one task: every transition + every verifier
    /// verdict.
    Logs { task_id: String },
    /// Move a task to CANCELLED. Idempotent — already-terminal tasks
    /// are left alone.
    Cancel { task_id: String },
    /// Post a summary of a task as a comment on its branch's PR (#369).
    /// Best-effort: no open PR / no `gh` / not a repo prints a skip and
    /// exits 0 rather than failing.
    Pr { task_id: String },
    /// Set a "drain" marker so the reconciler starts no new task on this
    /// machine while running ones finish; queued tasks wait, with the reason.
    /// The marker is a sentinel file at `~/.claudectl/coord/drain`. Remove
    /// with `claudectl supervisor undrain`.
    Drain,
    /// Clear the drain marker so the reconciler resumes new
    /// assignments.
    Undrain,
    /// Serve fleet metrics in Prometheus text format on `<bind>` (default
    /// `127.0.0.1:9464`). Point Grafana / Datadog at `http://<bind>/metrics`.
    /// Blocks until Ctrl-C; each scrape reads the coord DB fresh (WAL), so it's
    /// safe to run alongside the reconciler. This is the bridge from a solo
    /// tool to team observability — see `docs/team-observability.md`.
    Metrics {
        /// Address to bind. `/metrics` is unauthenticated, so prefer the
        /// loopback default and forward the port for a remote scrape;
        /// `0.0.0.0:9464` exposes it on the LAN and warns at startup.
        #[arg(default_value = "127.0.0.1:9464")]
        bind: String,
    },
}

pub fn dispatch(cmd: &SupervisorCommand) -> io::Result<()> {
    match cmd {
        SupervisorCommand::Run { file, dry_run } => run_from_file(file, *dry_run),
        SupervisorCommand::Init { force } => write_scaffold(*force),
        SupervisorCommand::Validate { file } => validate_from_file(file),
        SupervisorCommand::Submit {
            name,
            cwd,
            prompt,
            role,
            model,
            budget_usd,
            max_retries,
            timeout_min,
            git_ref,
            branch,
            permission_mode,
            requires,
            peer,
        } => submit_inline(
            name,
            cwd,
            prompt,
            role.as_deref(),
            model.as_deref(),
            *budget_usd,
            *max_retries,
            *timeout_min,
            &Placement {
                git_ref: git_ref.clone(),
                branch: branch.clone(),
                permission_mode: permission_mode.clone(),
                requires: requires.clone(),
                peer: peer.clone(),
            },
        ),
        SupervisorCommand::Work {
            interval,
            until_idle,
            timeout,
        } => work(*interval, *until_idle, *timeout),
        SupervisorCommand::Status { state, graph: true } => {
            if state.is_some() {
                eprintln!("supervisor status: --graph shows every task; --state is ignored");
            }
            render_graph_status()
        }
        SupervisorCommand::Status {
            state,
            graph: false,
        } => render_status(state.as_deref()),
        SupervisorCommand::Logs { task_id } => render_logs(task_id),
        SupervisorCommand::Cancel { task_id } => cancel_task(task_id),
        SupervisorCommand::Pr { task_id } => post_pr_summary(task_id),
        SupervisorCommand::Drain => set_drain(true),
        SupervisorCommand::Undrain => set_drain(false),
        SupervisorCommand::Metrics { bind } => serve_metrics(bind),
    }
}

/// States that still have work in front of them. A Pending task waiting on
/// a dependency counts: the dependency either finishes or is escalated, and
/// then so is the task.
const ACTIVE_STATES: &[TaskState] = &[
    TaskState::Pending,
    TaskState::Ready,
    TaskState::Assigned,
    TaskState::Running,
    TaskState::Verifying,
    TaskState::Resuming,
    TaskState::Retrying,
];

/// `supervisor work`: the reconcile loop on its own (#570).
fn work(interval: u64, until_idle: bool, timeout: Option<u64>) -> io::Result<()> {
    // A policy file that doesn't parse is reported now, not as a silent
    // drain on every pass (#571).
    super::placement::load_policy().map_err(io::Error::other)?;
    let supervisor = super::supervisor::Supervisor::with_defaults();
    let started = std::time::Instant::now();
    let interval = std::time::Duration::from_secs(interval.max(1));
    println!("supervisor: working (every {}s)", interval.as_secs());
    // Why each held task is waiting, printed when it changes, not every pass.
    let mut held: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        let ts = crate::logger::timestamp_now();
        match super::live_fx::tick_once(&supervisor) {
            Ok(r) => {
                if r.emitted > 0 {
                    println!("[{ts}] {} action(s), {} done", r.emitted, r.actuated);
                }
                for e in &r.errors {
                    println!("[{ts}]   failed: {e}");
                }
                let now: std::collections::HashMap<String, String> = r.holds.into_iter().collect();
                for (task, reason) in &now {
                    if held.get(task) != Some(reason) {
                        println!("[{ts}]   waiting: {task}: {reason}");
                    }
                }
                held = now;
            }
            Err(e) => println!("[{ts}] supervisor error: {e}"),
        }
        if until_idle {
            let conn = store::open().map_err(io::Error::other)?;
            let active = tasks::list_tasks(&conn, None)
                .map_err(io::Error::other)?
                .iter()
                .filter(|t| ACTIVE_STATES.contains(&t.state))
                .count();
            if active == 0 {
                println!("supervisor: no task left in progress");
                return render_status(None);
            }
        }
        if timeout.is_some_and(|t| started.elapsed().as_secs() >= t) {
            render_status(None)?;
            return Err(io::Error::other("supervisor work: timed out"));
        }
        std::thread::sleep(interval);
    }
}

/// Where and how a task's session runs (#570).
#[derive(Debug, Clone, Default)]
struct Placement {
    git_ref: Option<String>,
    branch: Option<String>,
    permission_mode: Option<String>,
    /// #571: what the machine must have, and an optional peer to pin to.
    requires: Vec<String>,
    peer: Option<String>,
}

/// Permission modes a task may ask for. `bypassPermissions` is left out on
/// purpose: a daemon must not start sessions with every check disabled
/// because a tasks file said so.
const PERMISSION_MODES: &[&str] = &[
    "acceptEdits",
    "auto",
    "default",
    "dontAsk",
    "manual",
    "plan",
];

impl Placement {
    fn validate(&self) -> Result<(), String> {
        if let Some(r) = &self.git_ref {
            claudectl_core::worktree::validate_ref("git_ref", r)?;
        }
        if let Some(b) = &self.branch {
            if self.git_ref.is_none() {
                return Err("branch needs a git_ref to start the worktree from".into());
            }
            claudectl_core::worktree::validate_ref("branch", b)?;
        }
        if let Some(m) = &self.permission_mode {
            if !PERMISSION_MODES.contains(&m.as_str()) {
                return Err(format!(
                    "permission_mode '{m}' is not one of {}",
                    PERMISSION_MODES.join(", ")
                ));
            }
        }
        for r in &self.requires {
            super::placement::validate_requirement(r)?;
        }
        if let Some(p) = &self.peer {
            if !super::placement::valid_peer_name(p) {
                return Err(format!("peer `{p}` is not a relay peer id"));
            }
        }
        Ok(())
    }
}

/// The task's cwd as an absolute path. Stored verbatim before, so `cwd = "."`
/// meant wherever the supervisor daemon happened to run, not where the tasks
/// file was.
fn absolute_cwd(cwd: &str, base: &std::path::Path) -> io::Result<String> {
    let p = std::path::Path::new(cwd);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    joined
        .canonicalize()
        .map(|c| c.to_string_lossy().into_owned())
        .map_err(|e| io::Error::other(format!("cwd '{cwd}': {e}")))
}

#[allow(clippy::too_many_arguments)]
fn submit_inline(
    name: &str,
    cwd: &str,
    prompt: &str,
    role: Option<&str>,
    model: Option<&str>,
    budget_usd: Option<f64>,
    max_retries: Option<u32>,
    timeout_min: Option<u32>,
    placement: &Placement,
) -> io::Result<()> {
    placement.validate().map_err(io::Error::other)?;
    let cwd = absolute_cwd(cwd, &std::env::current_dir()?)?;
    let conn = store::open().map_err(io::Error::other)?;
    let new_task = NewTask {
        git_ref: placement.git_ref.clone(),
        branch: placement.branch.clone(),
        permission_mode: placement.permission_mode.clone(),
        requires: placement.requires.clone(),
        peer: placement.peer.clone(),
        name: name.into(),
        role: role.map(String::from),
        cwd,
        prompt: prompt.into(),
        model: model.map(String::from),
        budget_usd,
        max_retries,
        timeout_min,
        depends_on: vec![],
        policy: None,
        verifiers: vec![],
    };
    let id = tasks::insert_task(&conn, &new_task).map_err(io::Error::other)?;
    println!("submitted {id}");
    Ok(())
}

/// Parse a `tasks.toml` file and insert each `[[task]]` block. The
/// schema mirrors RFC §4 and accepts a `verifiers` array on each task.
fn run_from_file(path: &PathBuf, dry_run: bool) -> io::Result<()> {
    let body = std::fs::read_to_string(path)?;
    let parsed: TaskFile = toml_parse(&body).map_err(io::Error::other)?;
    validate_tasks(&parsed).map_err(io::Error::other)?;
    if dry_run {
        for task in &parsed.task {
            println!("[dry-run] would submit: {}", task.name);
        }
        return Ok(());
    }
    // A relative cwd in a tasks file means relative to that file.
    let base = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);
    let conn = store::open().map_err(io::Error::other)?;
    let mut inserted = 0usize;
    // `depends_on` names a task in this file; what's stored is its id, so
    // the reconciler never has to choose between two tasks with one name.
    let mut ids: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for i in submission_order(&parsed).map_err(io::Error::other)? {
        let entry = &parsed.task[i];
        let depends_on = entry
            .depends_on
            .iter()
            .flatten()
            .map(|d| ids.get(d.as_str()).cloned().unwrap_or_else(|| d.clone()))
            .collect();
        let new_task = NewTask {
            git_ref: entry.git_ref.clone(),
            branch: entry.branch.clone(),
            permission_mode: entry.permission_mode.clone(),
            requires: entry.requires.clone().unwrap_or_default(),
            peer: entry.peer.clone(),
            name: entry.name.clone(),
            role: entry.role.clone(),
            cwd: absolute_cwd(&entry.cwd, &base)?,
            prompt: entry.prompt.clone(),
            model: entry.model.clone(),
            budget_usd: entry.budget_usd,
            max_retries: entry.max_retries,
            timeout_min: entry.timeout_min,
            depends_on,
            policy: None,
            verifiers: entry.verify.clone().unwrap_or_default(),
        };
        let id = tasks::insert_task(&conn, &new_task).map_err(io::Error::other)?;
        println!("submitted {id} ({})", entry.name);
        ids.insert(entry.name.as_str(), id);
        inserted += 1;
    }
    println!("{inserted} task(s) submitted");
    Ok(())
}

fn render_status(filter: Option<&str>) -> io::Result<()> {
    let conn = store::open().map_err(io::Error::other)?;
    let state = filter.and_then(TaskState::parse);
    let rows = tasks::list_tasks(&conn, state).map_err(io::Error::other)?;
    if rows.is_empty() {
        println!("(no tasks)");
        return Ok(());
    }
    for row in rows {
        let attempts = tasks::attempt_count(&conn, &row.id).unwrap_or(0);
        println!(
            "{id:<28}  {state:<10}  role={role}  attempts={attempts}  cwd={cwd}",
            id = row.id,
            state = row.state.as_str(),
            role = row.role.as_deref().unwrap_or("-"),
            cwd = row.cwd,
        );
    }
    Ok(())
}

fn render_graph_status() -> io::Result<()> {
    let conn = store::open().map_err(io::Error::other)?;
    let rows = tasks::list_tasks(&conn, None).map_err(io::Error::other)?;
    let mut causes = std::collections::HashMap::new();
    for row in rows.iter().filter(|r| r.state == TaskState::NeedsHuman) {
        if let Some((_, _, cause, _)) = tasks::list_transitions(&conn, &row.id)
            .map_err(io::Error::other)?
            .pop()
        {
            causes.insert(row.id.clone(), cause);
        }
    }
    print!("{}", graph_lines(&rows, &causes));
    Ok(())
}

/// The `depends_on` DAG as an indented tree (#572). A task sits under its
/// first dependency; any others are named on its line, so a diamond prints
/// each task once.
fn graph_lines(
    rows: &[tasks::TaskRow],
    causes: &std::collections::HashMap<String, String>,
) -> String {
    if rows.is_empty() {
        return "(no tasks)\n".into();
    }
    let names: std::collections::HashMap<&str, &str> = rows
        .iter()
        .map(|r| (r.id.as_str(), r.name.as_str()))
        .collect();
    // Each task's tree parent: its first dependency that's in the list.
    let parents: Vec<Option<&str>> = rows
        .iter()
        .map(|r| {
            r.depends_on
                .iter()
                .map(String::as_str)
                .find(|d| names.contains_key(d))
        })
        .collect();
    let mut out = String::new();
    let mut stack: Vec<(usize, usize)> = (0..rows.len())
        .rev()
        .filter(|&i| parents[i].is_none())
        .map(|i| (i, 0))
        .collect();
    while let Some((i, depth)) = stack.pop() {
        let r = &rows[i];
        out.push_str(&format!(
            "{}{} [{}] {}",
            "  ".repeat(depth),
            r.name,
            r.state.as_str(),
            r.id
        ));
        let others: Vec<&str> = r
            .depends_on
            .iter()
            .map(String::as_str)
            .filter(|d| Some(*d) != parents[i])
            .map(|d| names.get(d).copied().unwrap_or(d))
            .collect();
        if !others.is_empty() {
            out.push_str(&format!("  (also after {})", others.join(", ")));
        }
        if let Some(c) = causes.get(&r.id) {
            out.push_str(&format!("  — {c}"));
        }
        out.push('\n');
        for j in (0..rows.len()).rev() {
            if parents[j] == Some(r.id.as_str()) {
                stack.push((j, depth + 1));
            }
        }
    }
    out
}

fn render_logs(task_id: &str) -> io::Result<()> {
    let conn = store::open().map_err(io::Error::other)?;
    let task = tasks::get_task(&conn, task_id)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other(format!("task {task_id} not found")))?;
    println!(
        "task {id}  name={name}  state={state}",
        id = task.id,
        name = task.name,
        state = task.state.as_str()
    );
    println!("  cwd={}", task.cwd);
    println!("  prompt={}", task.prompt);
    println!("transitions:");
    let trans = tasks::list_transitions(&conn, task_id).map_err(io::Error::other)?;
    for (from, to, cause, at) in trans {
        println!("  {at}  {from} → {to}  ({cause})");
    }
    Ok(())
}

fn cancel_task(task_id: &str) -> io::Result<()> {
    let mut conn = store::open().map_err(io::Error::other)?;
    let task = tasks::get_task(&conn, task_id)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other(format!("task {task_id} not found")))?;
    if task.state.is_terminal() {
        println!("{task_id} already terminal ({})", task.state.as_str());
        return Ok(());
    }
    tasks::transition(
        &mut conn,
        task_id,
        task.state,
        TaskState::Cancelled,
        "operator-cancel",
    )
    .map_err(io::Error::other)?;
    println!("cancelled {task_id}");
    Ok(())
}

/// Post a task summary to its branch's PR (#369). Best-effort: anything that
/// prevents posting (no PR, no `gh`, not a repo) prints a `skipped:` line and
/// exits 0, so it's safe to call unconditionally from scripts or CI.
fn post_pr_summary(task_id: &str) -> io::Result<()> {
    match super::pr::post_task_summary(task_id) {
        Ok(msg) => println!("{msg}"),
        Err(reason) => println!("skipped: {reason}"),
    }
    Ok(())
}

/// Serve the Prometheus exporter until the process is interrupted. Verifies the
/// coord DB opens first so a misconfigured environment fails loudly here rather
/// than returning empty scrapes. The `ExporterHandle` is held in a named
/// binding for the life of the loop; dropping it would stop the listener.
fn serve_metrics(bind: &str) -> io::Result<()> {
    // Fail fast if coord isn't initialized — otherwise every scrape would 500.
    store::open().map_err(|e| {
        io::Error::other(format!(
            "coord DB not reachable ({e}); run `claudectl init --upgrade` first"
        ))
    })?;

    let _handle = super::exporter::serve(bind).map_err(io::Error::other)?;
    println!("serving fleet metrics at http://{bind}/metrics");
    println!("point Grafana / Datadog here (see docs/team-observability.md); Ctrl-C to stop.");
    // /metrics has no authentication at all, so an exposed bind is strictly
    // worse than the relay API's — there is no token to fall back on (#426).
    if let Ok(addr) = bind.parse::<std::net::SocketAddr>() {
        if claudectl_core::helpers::is_exposed_bind(&addr) {
            eprintln!("warning: /metrics is reachable from the network on {bind}.");
            eprintln!("         This endpoint is unauthenticated — anything that can reach");
            eprintln!("         the port can read your fleet's task and cost metrics. Prefer a");
            eprintln!("         loopback bind, scraped from this host or through a forwarded");
            eprintln!("         port. See docs/team-observability.md.");
        }
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

pub fn drain_marker_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".claudectl").join("coord").join("drain")
}

pub fn is_draining() -> bool {
    drain_marker_path().exists()
}

fn set_drain(enabled: bool) -> io::Result<()> {
    let path = drain_marker_path();
    if enabled {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, b"draining\n")?;
        println!("drain marker set at {}", path.display());
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => println!("drain marker cleared"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                println!("(no drain marker)");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------- Scaffolding + validation (#371) ----------------------------------

/// Starter `tasks.toml` written by `supervisor init`. Every optional key is
/// present but commented, so the file both runs as-is and documents the schema.
/// Kept in sync with `TaskEntry` / `toml_parse` — the test below asserts it
/// parses and validates.
const SCAFFOLD: &str = r#"# tasks.toml — claudectl supervisor task declarations (RFC §4 shape).
#   Submit:    claudectl supervisor run tasks.toml
#   Preview:   claudectl supervisor run tasks.toml --dry-run
#   Validate:  claudectl supervisor validate tasks.toml

[[task]]
name = "example-task"
cwd = "."
prompt = "Describe the work this agent should do."
# role = "backend"             # bus role to assign this task to
# model = "claude-opus-4-8"    # model override for the spawned session
# budget_usd = 5.0             # per-task spend cap
# max_retries = 2              # retry budget (default comes from config)
# timeout_min = 30             # wall-clock timeout
# depends_on = ["other-task"]  # run only after these task names finish

# Verifier gates run in order on VERIFYING; the first FAIL stops the task.
# [[task.verify]]
# run = "cargo test"           # shell command — non-zero exit is a FAIL
#
# [[task.verify]]
# brain = "Did the change keep the public API stable?"
"#;

/// Write the scaffold to `./tasks.toml`. Refuses to overwrite without `force`.
fn write_scaffold(force: bool) -> io::Result<()> {
    let path = PathBuf::from("tasks.toml");
    if path.exists() && !force {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "tasks.toml already exists — pass --force to overwrite",
        ));
    }
    std::fs::write(&path, SCAFFOLD)?;
    println!("wrote {}", path.display());
    println!("next: edit it, then `claudectl supervisor run tasks.toml --dry-run`");
    Ok(())
}

/// Parse + validate a file without touching the store.
fn validate_from_file(path: &PathBuf) -> io::Result<()> {
    let body = std::fs::read_to_string(path)?;
    let parsed = toml_parse(&body).map_err(io::Error::other)?;
    validate_tasks(&parsed).map_err(io::Error::other)?;
    println!("{}: {} task(s), valid", path.display(), parsed.task.len());
    Ok(())
}

/// Semantic checks the structural TOML reader can't make: required fields are
/// present, dependency names resolve within the file, and names are unique.
/// Returns the first problem with enough context to fix it.
fn validate_tasks(tf: &TaskFile) -> Result<(), String> {
    if tf.task.is_empty() {
        return Err("no [[task]] blocks found".into());
    }
    let names: Vec<&str> = tf.task.iter().map(|t| t.name.as_str()).collect();
    let mut seen = std::collections::HashSet::new();
    for (i, t) in tf.task.iter().enumerate() {
        let label = if t.name.trim().is_empty() {
            format!("task #{}", i + 1)
        } else {
            format!("task #{} ({})", i + 1, t.name)
        };
        if t.name.trim().is_empty() {
            return Err(format!("{label}: missing required `name`"));
        }
        if t.cwd.trim().is_empty() {
            return Err(format!("{label}: missing required `cwd`"));
        }
        if t.prompt.trim().is_empty() {
            return Err(format!("{label}: missing required `prompt`"));
        }
        if !seen.insert(t.name.as_str()) {
            return Err(format!("{label}: duplicate task name `{}`", t.name));
        }
        Placement {
            git_ref: t.git_ref.clone(),
            branch: t.branch.clone(),
            permission_mode: t.permission_mode.clone(),
            requires: t.requires.clone().unwrap_or_default(),
            peer: t.peer.clone(),
        }
        .validate()
        .map_err(|e| format!("{label}: {e}"))?;
        if let Some(deps) = &t.depends_on {
            for d in deps {
                if !names.contains(&d.as_str()) {
                    return Err(format!(
                        "{label}: depends_on `{d}` — no task with that name in this file"
                    ));
                }
            }
        }
    }
    submission_order(tf).map(|_| ())
}

/// Indexes into `tf.task` with every task after the ones it depends on, so
/// a dependency's id exists before the task that names it is inserted
/// (#572). Ties keep file order. A cycle is an error: none of its tasks
/// could ever start.
fn submission_order(tf: &TaskFile) -> Result<Vec<usize>, String> {
    let index: std::collections::HashMap<&str, usize> = tf
        .task
        .iter()
        .enumerate()
        .map(|(i, t)| (t.name.as_str(), i))
        .collect();
    let mut order = Vec::with_capacity(tf.task.len());
    let mut placed = vec![false; tf.task.len()];
    while order.len() < tf.task.len() {
        let before = order.len();
        for (i, t) in tf.task.iter().enumerate() {
            let deps_placed = t
                .depends_on
                .iter()
                .flatten()
                .all(|d| index.get(d.as_str()).is_none_or(|&j| placed[j]));
            if !placed[i] && deps_placed {
                placed[i] = true;
                order.push(i);
            }
        }
        if order.len() == before {
            let stuck: Vec<&str> = tf
                .task
                .iter()
                .zip(&placed)
                .filter(|(_, p)| !**p)
                .map(|(t, _)| t.name.as_str())
                .collect();
            return Err(format!(
                "depends_on has a cycle among: {}",
                stuck.join(", ")
            ));
        }
    }
    Ok(order)
}

// ---------- Minimal TOML decoder for `tasks.toml` ----------------------------

/// Toml parsing target. Lives here rather than as a re-export so the
/// `tasks.toml` schema stays under the supervisor's control instead of
/// drifting with whatever upstream defines.
#[derive(Debug, serde::Deserialize)]
struct TaskFile {
    task: Vec<TaskEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct TaskEntry {
    name: String,
    cwd: String,
    prompt: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    budget_usd: Option<f64>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    timeout_min: Option<u32>,
    #[serde(default)]
    depends_on: Option<Vec<String>>,
    #[serde(default)]
    verify: Option<Vec<super::verify::Verifier>>,
    #[serde(default)]
    git_ref: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    requires: Option<Vec<String>>,
    #[serde(default)]
    peer: Option<String>,
}

/// Tiny hand-rolled TOML reader to avoid pulling the `toml` crate just
/// for the supervisor CLI. Supports the subset RFC §4 declares:
/// repeated `[[task]]` headers with key=value entries on bare values
/// (strings, numbers) plus inline `[[task.verify]]` blocks.
///
/// For anything more elaborate (escaped strings with newlines, nested
/// inline tables) the caller should hand-author JSON via the `submit`
/// verb instead. The intent of `tasks.toml` is readable per-task
/// declarations, not arbitrary TOML.
/// The line without its `#` comment. A `#` inside a quoted string is text,
/// not a comment: cutting there truncated any prompt that mentioned an issue
/// number.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => return &line[..i],
            None => {}
        }
    }
    line
}

/// A TOML value as text. A basic string (`"…"`) has its escapes decoded, so
/// `run = "test \"$(cat F)\" = x"` reaches the shell as `test "$(cat F)" = x`
/// rather than with the backslashes still in it, which failed every verifier
/// that quoted anything. A literal string (`'…'`) is taken verbatim; anything
/// else (numbers, booleans, arrays) is returned trimmed.
fn toml_value(v: &str) -> Result<String, String> {
    if let Some(rest) = v.strip_prefix('\'') {
        return rest
            .strip_suffix('\'')
            .map(str::to_string)
            .ok_or_else(|| format!("unterminated string: {v}"));
    }
    let Some(rest) = v.strip_prefix('"') else {
        return Ok(v.to_string());
    };
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                return if chars.as_str().trim().is_empty() {
                    Ok(out)
                } else {
                    Err(format!("text after a closing quote: {v}"))
                };
            }
            '\\' => match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => return Err(format!("unsupported escape \\{other} in {v}")),
                None => return Err(format!("unterminated string: {v}")),
            },
            other => out.push(other),
        }
    }
    Err(format!("unterminated string: {v}"))
}

/// `["a", "b"]` as its strings.
fn string_list(val: &str) -> Vec<String> {
    val.trim_matches(|c: char| c == '[' || c == ']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn toml_parse(input: &str) -> Result<TaskFile, String> {
    let mut tasks: Vec<TaskEntry> = Vec::new();
    let mut current: Option<TaskEntry> = None;
    let mut current_verify: Vec<super::verify::Verifier> = Vec::new();
    let mut verify_open = false;
    let mut verify_buf: Vec<(String, String)> = Vec::new();

    fn finalize_verify(
        verify_buf: &[(String, String)],
        out: &mut Vec<super::verify::Verifier>,
    ) -> Result<(), String> {
        if verify_buf.is_empty() {
            return Ok(());
        }
        let mut map: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (k, v) in verify_buf {
            map.insert(k.as_str(), v.as_str());
        }
        if let Some(cmd) = map.get("run") {
            out.push(super::verify::Verifier::Run {
                command: cmd.to_string(),
            });
        } else if let Some(prompt) = map.get("brain") {
            out.push(super::verify::Verifier::Brain {
                prompt: prompt.to_string(),
            });
        } else if let Some(prompt) = map.get("agent") {
            out.push(super::verify::Verifier::Agent {
                prompt: prompt.to_string(),
                model: map.get("model").map(|s| s.to_string()),
                budget_usd: map.get("budget_usd").and_then(|s| s.parse::<f64>().ok()),
            });
        } else {
            return Err("[[task.verify]] block missing run/brain/agent key".into());
        }
        Ok(())
    }

    for raw in input.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[task]]" {
            if verify_open {
                finalize_verify(&verify_buf, &mut current_verify)?;
                verify_buf.clear();
                verify_open = false;
            }
            if let Some(mut t) = current.take() {
                if !current_verify.is_empty() {
                    t.verify = Some(std::mem::take(&mut current_verify));
                }
                tasks.push(t);
            }
            current = Some(TaskEntry {
                name: String::new(),
                cwd: String::new(),
                prompt: String::new(),
                role: None,
                model: None,
                budget_usd: None,
                max_retries: None,
                timeout_min: None,
                depends_on: None,
                verify: None,
                git_ref: None,
                branch: None,
                permission_mode: None,
                requires: None,
                peer: None,
            });
            continue;
        }
        if line == "[[task.verify]]" {
            if verify_open {
                finalize_verify(&verify_buf, &mut current_verify)?;
                verify_buf.clear();
            }
            verify_open = true;
            continue;
        }
        // key = value
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("unparseable TOML line: {line}"));
        };
        let key = k.trim().to_string();
        let val = toml_value(v.trim())?;
        if verify_open {
            verify_buf.push((key, val));
            continue;
        }
        let Some(t) = current.as_mut() else {
            return Err(format!("key={key} outside any [[task]] block"));
        };
        match key.as_str() {
            "name" => t.name = val,
            "cwd" => t.cwd = val,
            "prompt" => t.prompt = val,
            "role" => t.role = Some(val),
            "model" => t.model = Some(val),
            "budget_usd" => t.budget_usd = val.parse().ok(),
            "max_retries" => t.max_retries = val.parse().ok(),
            "timeout_min" => t.timeout_min = val.parse().ok(),
            "git_ref" => t.git_ref = Some(val),
            "branch" => t.branch = Some(val),
            "permission_mode" => t.permission_mode = Some(val),
            "peer" => t.peer = Some(val),
            "depends_on" => t.depends_on = Some(string_list(&val)),
            "requires" => t.requires = Some(string_list(&val)),
            other => return Err(format!("unknown key in [[task]]: {other}")),
        }
    }
    if verify_open {
        finalize_verify(&verify_buf, &mut current_verify)?;
    }
    if let Some(mut t) = current.take() {
        if !current_verify.is_empty() {
            t.verify = Some(current_verify);
        }
        tasks.push(t);
    }
    Ok(TaskFile { task: tasks })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the supervisor E2E caught: every verifier that quoted anything
    /// failed, because `\"` reached the shell with its backslash.
    #[test]
    fn escaped_quotes_in_a_verifier_reach_the_shell_unescaped() {
        let tf = toml_parse(
            r#"
[[task]]
name = "t"
cwd = "/tmp"
prompt = "Say \"hi\""
[[task.verify]]
run = "test \"$(cat ONE.txt)\" = one"
"#,
        )
        .unwrap();
        assert_eq!(tf.task[0].prompt, r#"Say "hi""#);
        match &tf.task[0].verify.as_ref().unwrap()[0] {
            super::super::verify::Verifier::Run { command } => {
                assert_eq!(command, r#"test "$(cat ONE.txt)" = one"#);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_hash_inside_a_string_is_text_and_outside_is_a_comment() {
        let tf = toml_parse(
            "[[task]]\nname = \"t\" # the name\ncwd = \"/tmp\"\nprompt = \"Fix #570 and #571\" # two issues\n",
        )
        .unwrap();
        assert_eq!(tf.task[0].name, "t");
        assert_eq!(tf.task[0].prompt, "Fix #570 and #571");
    }

    #[test]
    fn literal_strings_and_bad_strings() {
        assert_eq!(toml_value(r"'C:\path\x'").unwrap(), r"C:\path\x");
        assert_eq!(toml_value("42").unwrap(), "42");
        assert_eq!(toml_value(r#""a\\b\tc""#).unwrap(), "a\\b\tc");
        assert!(toml_value(r#""unterminated"#).is_err());
        assert!(toml_value(r#""bad \q escape""#).is_err());
        assert!(toml_value(r#""x" y"#).is_err());
    }

    #[test]
    fn placement_fields_parse_and_validate() {
        let ok = toml_parse(
            "[[task]]\nname = \"t\"\ncwd = \"/tmp\"\nprompt = \"p\"\nrequires = [\"docker\", \"node>=22\"]\npeer = \"mini-508f\"\n",
        )
        .unwrap();
        assert_eq!(
            ok.task[0].requires.as_deref(),
            Some(&["docker".to_string(), "node>=22".to_string()][..])
        );
        assert_eq!(ok.task[0].peer.as_deref(), Some("mini-508f"));
        assert!(validate_tasks(&ok).is_ok());
        for bad in [
            "requires = [\"a b\"]",
            "requires = [\"node>=x\"]",
            "peer = \"-x\"",
        ] {
            let tf = toml_parse(&format!(
                "[[task]]\nname = \"t\"\ncwd = \"/tmp\"\nprompt = \"p\"\n{bad}\n"
            ))
            .unwrap();
            assert!(validate_tasks(&tf).is_err(), "{bad}");
        }
    }

    #[test]
    fn worktree_fields_parse_and_validate() {
        let ok = toml_parse(
            "[[task]]\nname = \"t\"\ncwd = \"/tmp\"\nprompt = \"p\"\ngit_ref = \"main\"\nbranch = \"feat/x\"\npermission_mode = \"acceptEdits\"\n",
        )
        .unwrap();
        assert_eq!(ok.task[0].git_ref.as_deref(), Some("main"));
        assert!(validate_tasks(&ok).is_ok());
        for bad in [
            "git_ref = \"--force\"",
            "branch = \"x\"",
            "permission_mode = \"bypassPermissions\"",
        ] {
            let tf = toml_parse(&format!(
                "[[task]]\nname = \"t\"\ncwd = \"/tmp\"\nprompt = \"p\"\n{bad}\n"
            ))
            .unwrap();
            assert!(validate_tasks(&tf).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_relative_cwd_is_resolved_against_the_tasks_file() {
        let base = std::env::temp_dir();
        let got = absolute_cwd(".", &base).unwrap();
        assert_eq!(got, base.canonicalize().unwrap().to_string_lossy());
        assert!(absolute_cwd("/definitely/not/here", &base).is_err());
    }

    fn chain(blocks: &[(&str, &[&str])]) -> TaskFile {
        let mut s = String::new();
        for (name, deps) in blocks {
            s.push_str(&format!(
                "[[task]]\nname = \"{name}\"\ncwd = \"/tmp\"\nprompt = \"p\"\n"
            ));
            if !deps.is_empty() {
                let quoted: Vec<String> = deps.iter().map(|d| format!("\"{d}\"")).collect();
                s.push_str(&format!("depends_on = [{}]\n", quoted.join(", ")));
            }
        }
        toml_parse(&s).unwrap()
    }

    /// #572: a dependency is inserted before the task naming it, so its id
    /// exists to be stored.
    #[test]
    fn tasks_are_submitted_after_their_dependencies() {
        let tf = chain(&[("c", &["b"]), ("b", &["a"]), ("a", &[]), ("x", &[])]);
        let names: Vec<&str> = submission_order(&tf)
            .unwrap()
            .into_iter()
            .map(|i| tf.task[i].name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "x", "b", "c"]);
    }

    #[test]
    fn the_graph_nests_each_task_under_what_it_waits_for() {
        let conn = store::open_memory();
        let mk = |name: &str, deps: Vec<String>| {
            tasks::insert_task(
                &conn,
                &NewTask {
                    name: name.into(),
                    role: None,
                    cwd: "/tmp".into(),
                    prompt: "p".into(),
                    model: None,
                    budget_usd: None,
                    max_retries: None,
                    timeout_min: None,
                    depends_on: deps,
                    policy: None,
                    verifiers: vec![],
                    git_ref: None,
                    branch: None,
                    permission_mode: None,
                    requires: vec![],
                    peer: None,
                },
            )
            .unwrap()
        };
        let a = mk("a", vec![]);
        let b = mk("b", vec![a.clone()]);
        let x = mk("x", vec![]);
        let c = mk("c", vec![b.clone(), x.clone()]);
        let rows = tasks::list_tasks(&conn, None).unwrap();
        let mut causes = std::collections::HashMap::new();
        causes.insert(c.clone(), "dependency_cancelled:x".to_string());
        let g = graph_lines(&rows, &causes);
        let lines: Vec<&str> = g.lines().collect();
        assert_eq!(lines.len(), 4, "{g}");
        assert!(lines[0].starts_with("a [PENDING]"), "{g}");
        assert!(lines[1].starts_with("  b [PENDING]"), "{g}");
        assert!(
            lines[2].starts_with("    c [PENDING]")
                && lines[2].contains("(also after x)")
                && lines[2].ends_with("— dependency_cancelled:x"),
            "{g}"
        );
        assert!(lines[3].starts_with("x [PENDING]"), "{g}");
    }

    #[test]
    fn a_dependency_cycle_is_refused() {
        let tf = chain(&[("a", &["b"]), ("b", &["a"]), ("ok", &[])]);
        let err = validate_tasks(&tf).unwrap_err();
        assert!(err.contains("cycle") && err.contains("a, b"), "{err}");
    }

    #[test]
    fn toml_parse_handles_full_task_block() {
        let body = r#"
[[task]]
name = "auth-middleware"
cwd = "./services"
prompt = "Add JWT auth middleware to all API routes"
role = "backend"
model = "sonnet"
budget_usd = 3.00
max_retries = 2
timeout_min = 45

  [[task.verify]]
  run = "cargo test --all-targets"

  [[task.verify]]
  brain = "Review the diff for auth-coverage gaps. PASS or FAIL with reasons."

  [[task.verify]]
  agent = "Adversarial review: find a request that bypasses the middleware."
  model = "haiku"
  budget_usd = 0.25
"#;
        let parsed = toml_parse(body).expect("parse");
        assert_eq!(parsed.task.len(), 1);
        let t = &parsed.task[0];
        assert_eq!(t.name, "auth-middleware");
        assert_eq!(t.cwd, "./services");
        assert_eq!(t.role.as_deref(), Some("backend"));
        assert_eq!(t.budget_usd, Some(3.0));
        let verify = t.verify.as_ref().expect("verifiers");
        assert_eq!(verify.len(), 3);
        match &verify[2] {
            super::super::verify::Verifier::Agent {
                model, budget_usd, ..
            } => {
                assert_eq!(model.as_deref(), Some("haiku"));
                assert_eq!(*budget_usd, Some(0.25));
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn toml_parse_multiple_tasks() {
        let body = r#"
[[task]]
name = "first"
cwd = "/a"
prompt = "do a"

[[task]]
name = "second"
cwd = "/b"
prompt = "do b"
"#;
        let parsed = toml_parse(body).expect("parse");
        assert_eq!(parsed.task.len(), 2);
        assert_eq!(parsed.task[0].name, "first");
        assert_eq!(parsed.task[1].name, "second");
    }

    #[test]
    fn toml_parse_rejects_unknown_key() {
        let body = r#"
[[task]]
name = "x"
cwd = "/x"
prompt = "do"
flavour = "vanilla"
"#;
        assert!(toml_parse(body).is_err());
    }

    #[test]
    fn scaffold_parses_and_validates() {
        let parsed = toml_parse(SCAFFOLD).expect("scaffold parses");
        assert_eq!(parsed.task.len(), 1);
        validate_tasks(&parsed).expect("scaffold validates");
    }

    #[test]
    fn shipped_examples_parse_and_validate() {
        for body in [
            include_str!("../../examples/tasks/fan-out.toml"),
            include_str!("../../examples/tasks/dependency-chain.toml"),
            include_str!("../../examples/tasks/verify-then-merge.toml"),
        ] {
            let parsed = toml_parse(body).expect("example parses");
            validate_tasks(&parsed).expect("example validates");
        }
    }

    #[test]
    fn validate_rejects_missing_required_field() {
        // Empty prompt — structurally valid TOML, semantically incomplete.
        let body = r#"
[[task]]
name = "x"
cwd = "/x"
prompt = ""
"#;
        let parsed = toml_parse(body).expect("parse");
        let err = validate_tasks(&parsed).unwrap_err();
        assert!(err.contains("prompt"), "got: {err}");
    }

    #[test]
    fn validate_rejects_dangling_dependency() {
        let body = r#"
[[task]]
name = "a"
cwd = "/a"
prompt = "do a"
depends_on = ["ghost"]
"#;
        let parsed = toml_parse(body).expect("parse");
        let err = validate_tasks(&parsed).unwrap_err();
        assert!(err.contains("ghost"), "got: {err}");
    }

    #[test]
    fn validate_rejects_duplicate_names() {
        let body = r#"
[[task]]
name = "dup"
cwd = "/a"
prompt = "do a"

[[task]]
name = "dup"
cwd = "/b"
prompt = "do b"
"#;
        let parsed = toml_parse(body).expect("parse");
        let err = validate_tasks(&parsed).unwrap_err();
        assert!(err.contains("duplicate"), "got: {err}");
    }
}

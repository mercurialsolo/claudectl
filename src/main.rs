#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

mod app;
mod brain;
mod config;
#[cfg(feature = "coord")]
mod coord;
mod demo;
mod discovery;
mod health;
mod history;
mod hooks;
mod init;
mod launch;
mod logger;
mod models;
mod monitor;
mod orchestrator;
mod process;
mod recorder;
mod rules;
mod session;
mod session_recorder;
mod terminals;
mod theme;
mod transcript;
mod ui;

use std::io;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::{App, FocusFilter, StatusFilter};

#[derive(Clone)]
struct ViewFilters {
    status_filter: StatusFilter,
    focus_filter: FocusFilter,
    search: String,
}

#[derive(Parser)]
#[command(
    name = "claudectl",
    version,
    about = "Monitor and manage Claude Code CLI agents"
)]
struct Cli {
    // ── Dashboard ───────────────────────────────────────────────────────
    /// Refresh interval in milliseconds
    #[arg(short, long, default_value_t = 2000, help_heading = "Dashboard")]
    interval: u64,

    /// Color theme: dark, light, or none (respects NO_COLOR env var)
    #[arg(long, help_heading = "Dashboard")]
    theme: Option<String>,

    /// Enable debug mode: show timing metrics in the footer
    #[arg(long, help_heading = "Dashboard")]
    debug: bool,

    /// Run with deterministic fake sessions for screenshots and recordings
    #[arg(long, help_heading = "Dashboard")]
    demo: bool,

    // ── Output Modes ───────────────────────────────────────────────────
    /// Print session list to stdout and exit (no TUI)
    #[arg(short, long, help_heading = "Output Modes")]
    list: bool,

    /// Print JSON array of sessions and exit
    #[arg(long, help_heading = "Output Modes")]
    json: bool,

    /// Stream status changes to stdout (no TUI). Only prints when status changes.
    #[arg(short, long, help_heading = "Output Modes")]
    watch: bool,

    /// Output format for watch mode. Placeholders: {pid}, {project}, {status}, {cost}, {context}
    #[arg(
        long,
        default_value = "{pid} {project}: {status} (${cost}, ctx {context}%)",
        help_heading = "Output Modes"
    )]
    format: String,

    /// Show summary of session activity and exit
    #[arg(long, help_heading = "Output Modes")]
    summary: bool,

    /// Time window for --summary, --history, --stats (e.g., "8h", "24h", "30m")
    #[arg(long, default_value = "24h", help_heading = "Output Modes")]
    since: String,

    // ── Filtering ──────────────────────────────────────────────────────
    /// Filter sessions by status (e.g., "NeedsInput", "Processing", "Finished")
    #[arg(long, help_heading = "Filtering")]
    filter_status: Option<String>,

    /// Focus on a high-signal subset: attention, over-budget, high-context, unknown-telemetry, conflict
    #[arg(long, help_heading = "Filtering")]
    focus: Option<String>,

    /// Search project/model/session text
    #[arg(long, help_heading = "Filtering")]
    search: Option<String>,

    // ── Session Management ─────────────────────────────────────────────
    /// Launch a new Claude Code session in the given directory
    #[arg(long = "new", help_heading = "Session Management")]
    new_session: bool,

    /// Working directory for the new session (used with --new)
    #[arg(long, default_value = ".", help_heading = "Session Management")]
    cwd: String,

    /// Prompt to send to the new session (used with --new)
    #[arg(long, help_heading = "Session Management")]
    prompt: Option<String>,

    /// Resume a session by ID (used with --new)
    #[arg(long, help_heading = "Session Management")]
    resume: Option<String>,

    // ── Budget & Notifications ─────────────────────────────────────────
    /// Per-session budget in USD. Alert at 80%, optionally kill at 100%.
    #[arg(long, help_heading = "Budget & Notifications")]
    budget: Option<f64>,

    /// Auto-kill sessions that exceed the budget (requires --budget)
    #[arg(long, help_heading = "Budget & Notifications")]
    kill_on_budget: bool,

    /// Enable desktop notifications on NeedsInput transitions
    #[arg(long, help_heading = "Budget & Notifications")]
    notify: bool,

    /// Webhook URL to POST JSON on status changes
    #[arg(long, help_heading = "Budget & Notifications")]
    webhook: Option<String>,

    /// Only fire webhook on these status transitions (comma-separated, e.g. "NeedsInput,Finished")
    #[arg(long, help_heading = "Budget & Notifications")]
    webhook_on: Option<String>,

    // ── Brain (Local LLM) ──────────────────────────────────────────────
    /// Enable local LLM brain for session advisory (requires ollama or compatible endpoint)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    brain: bool,

    /// Auto-execute brain suggestions without confirmation (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    auto_run: bool,

    /// LLM endpoint URL for brain (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    url: Option<String>,

    /// Override brain model name (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    brain_model: Option<String>,

    /// Run brain eval scenarios against the local LLM and report results
    #[arg(long, help_heading = "Brain (Local LLM)")]
    brain_eval: bool,

    /// List brain prompt templates and their source (built-in vs user override)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    brain_prompts: bool,

    /// Brain statistics and metrics (subcommands: learning-curve, accuracy, baseline, false-approve, help)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    brain_stats: Option<String>,

    /// Query the brain for a single tool-call decision and exit (JSON output).
    /// Used by Claude Code plugin hooks for inline approve/deny.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    brain_query: bool,

    /// Tool name for --brain-query (e.g., "Bash", "Write", "Edit")
    #[arg(long, help_heading = "Brain (Local LLM)")]
    tool: Option<String>,

    /// Command or input for --brain-query (e.g., "rm -rf /tmp")
    #[arg(long, help_heading = "Brain (Local LLM)")]
    tool_input: Option<String>,

    /// Project name for --brain-query context (defaults to current directory name)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    project: Option<String>,

    /// Set brain gate mode: on (default), off (disable), auto (full auto-approve).
    /// Controls whether the Claude Code plugin hook queries the brain.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    mode: Option<String>,

    /// Show auto-generated insights, or set mode (on/off/status).
    /// Requires --brain or brain.enabled in config.
    /// Without argument: show current insights.
    /// With argument: set insights mode (on = auto-generate, off = disable).
    #[arg(long, help_heading = "Brain (Local LLM)", num_args = 0..=1, default_missing_value = "")]
    insights: Option<String>,

    // ── Orchestration ──────────────────────────────────────────────────
    /// Analyze a prompt and suggest parallel sub-tasks (outputs TaskFile JSON)
    #[arg(long, help_heading = "Orchestration")]
    decompose: Option<String>,

    /// Run tasks from a JSON file (e.g., claudectl --run tasks.json)
    #[arg(long, help_heading = "Orchestration")]
    run: Option<String>,

    /// Run independent tasks in parallel (used with --run)
    #[arg(long, help_heading = "Orchestration")]
    parallel: bool,

    // ── Coordination ──────────────────────────────────────────────────
    /// Coordination layer inspection (events, leases, blockers, handoffs, interrupts, memory)
    #[cfg(feature = "coord")]
    #[arg(long, help_heading = "Coordination")]
    coord: Option<String>,

    // ── Recording ──────────────────────────────────────────────────────
    /// Record the TUI session as an asciicast v2 file (e.g., --record demo.cast)
    #[arg(long, help_heading = "Recording")]
    record: Option<String>,

    /// Auto-quit the TUI after this many seconds (useful with --demo --record)
    #[arg(long, help_heading = "Recording")]
    duration: Option<u64>,

    // ── Cleanup ────────────────────────────────────────────────────────
    /// Clean up old session data (JSONL transcripts, session JSON files)
    #[arg(long, help_heading = "Cleanup")]
    clean: bool,

    /// Only clean sessions older than this duration (e.g., "7d", "24h"). Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    older_than: Option<String>,

    /// Only clean sessions that have finished. Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    finished: bool,

    /// Show what would be removed without deleting. Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    dry_run: bool,

    // ── History & Diagnostics ──────────────────────────────────────────
    /// Show history of completed sessions and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    history: bool,

    /// Show aggregated session statistics and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    stats: bool,

    /// Show resolved configuration and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    config: bool,

    /// Print an annotated default config template to stdout and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    config_template: bool,

    /// List configured event hooks and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    hooks: bool,

    /// Diagnose terminal integration and setup requirements
    #[arg(long, help_heading = "History & Diagnostics")]
    doctor: bool,

    /// Write diagnostic logs to a file (for debugging/bug reports)
    #[arg(long, help_heading = "History & Diagnostics")]
    log: Option<String>,

    // ── Setup ─────────────────────────────────────────────────────────
    /// Wire up Claude Code hooks in .claude/settings.json and exit
    #[arg(long, help_heading = "Setup")]
    init: bool,

    /// Remove claudectl hooks from .claude/settings.json and exit
    #[arg(long, help_heading = "Setup", conflicts_with = "init")]
    uninstall: bool,

    /// Configuration scope: user (global ~/.claude/settings.json) or project (.claude/settings.local.json)
    #[arg(short, long, default_value = "user", help_heading = "Setup")]
    scope: String,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let is_demo = cli.demo;
    let result = run_main(cli);
    if result.is_ok() {
        maybe_print_star_prompt(is_demo);
    }
    result
}

fn maybe_print_star_prompt(is_demo: bool) {
    let marker = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".claudectl/.star-prompted");

    let first_run = !marker.exists();

    if is_demo || first_run {
        eprintln!();
        eprintln!(
            "\u{2b50} If claudectl is useful, star it: https://github.com/mercurialsolo/claudectl"
        );

        if first_run {
            if let Some(parent) = marker.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&marker, "");
        }
    }
}

fn run_main(cli: Cli) -> io::Result<()> {
    // Initialize diagnostic logger if --log is set
    if let Some(ref log_path) = cli.log {
        if let Err(e) = logger::init(log_path) {
            eprintln!("Warning: could not open log file {log_path}: {e}");
        }
    }

    // Load config from files, then let CLI flags override
    let mut cfg = config::Config::load();

    // CLI flags override config file values (only override if explicitly set)
    if cli.interval != 2000 {
        cfg.interval = cli.interval;
    }
    if cli.notify {
        cfg.notify = true;
    }
    if cli.debug {
        cfg.debug = true;
    }
    if cli.budget.is_some() {
        cfg.budget = cli.budget;
    }
    if cli.kill_on_budget {
        cfg.kill_on_budget = true;
    }
    if cli.webhook.is_some() {
        cfg.webhook = cli.webhook.clone();
    }
    if cli.webhook_on.is_some() {
        cfg.webhook_on = cli.webhook_on.as_deref().map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .collect::<Vec<_>>()
        });
    }

    // Brain CLI overrides
    if cli.brain {
        let brain = cfg.brain.get_or_insert_with(config::BrainConfig::default);
        brain.enabled = true;
        if cli.auto_run {
            brain.auto_mode = true;
        }
        if let Some(ref endpoint) = cli.url {
            brain.endpoint = endpoint.clone();
        }
        if let Some(ref model) = cli.brain_model {
            brain.model = model.clone();
        }
    }

    models::set_overrides(cfg.model_overrides.clone());
    let filters = ViewFilters {
        status_filter: parse_status_filter(cli.filter_status.as_deref())?,
        focus_filter: parse_focus_filter(cli.focus.as_deref())?,
        search: cli.search.clone().unwrap_or_default(),
    };

    // Load event hooks from config
    let hook_registry = config::load_hooks();

    if cli.config {
        cfg.print_resolved();
        return Ok(());
    }

    if cli.config_template {
        config::Config::print_template();
        return Ok(());
    }

    if cli.hooks {
        hook_registry.print_list();
        return Ok(());
    }

    if cli.doctor {
        return print_doctor();
    }

    if cli.init {
        let project = cli.scope == "project";
        return init::run_init(project);
    }

    if cli.uninstall {
        let project = cli.scope == "project";
        return init::run_uninit(project);
    }

    if cli.brain_prompts {
        println!("Brain Prompt Templates");
        println!("======================");
        for (name, source) in brain::prompts::list_prompts() {
            println!("  {name}: {source}");
        }
        println!();
        println!("Override: create ~/.claudectl/brain/prompts/<name>.md");
        return Ok(());
    }

    if cli.brain_eval {
        let brain_cfg = cfg.brain.clone().unwrap_or_default();
        println!("Loading eval scenarios...");
        let scenarios = brain::evals::load_scenarios();
        println!(
            "Running {} scenarios against {}...",
            scenarios.len(),
            brain_cfg.endpoint
        );
        println!();
        let results = brain::evals::run_evals(&brain_cfg, &scenarios);
        brain::evals::print_results(&results);
        return Ok(());
    }

    if let Some(ref subcommand) = cli.brain_stats {
        brain::metrics::dispatch(subcommand);
        return Ok(());
    }

    #[cfg(feature = "coord")]
    if let Some(ref sub) = cli.coord {
        return coord::cli::dispatch(sub, cli.json);
    }

    if cli.brain_query {
        return run_brain_query(&cfg, &cli);
    }

    if let Some(ref mode) = cli.mode {
        return run_brain_mode(mode);
    }

    if let Some(ref insights_arg) = cli.insights {
        return run_insights(&cfg, &cli, insights_arg);
    }

    if let Some(ref prompt) = cli.decompose {
        let brain_cfg = cfg.brain.clone().unwrap_or_default();
        if prompt.len() < 200 {
            println!(
                "Prompt is short ({} chars) — decomposition works best with larger, multi-part prompts.",
                prompt.len()
            );
        }
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let max_tasks = brain_cfg.max_sessions.min(6);
        eprintln!("Analyzing prompt for decomposition...");
        match brain::client::decompose_prompt(&brain_cfg, prompt, &cwd, max_tasks) {
            Ok(result) => {
                if result.decomposable {
                    let task_file = orchestrator::decomposition_to_task_file(result.tasks, &cwd);
                    let json = serde_json::to_string_pretty(&task_file)
                        .unwrap_or_else(|e| format!("JSON error: {e}"));
                    println!("{json}");
                } else {
                    println!("Not decomposable: {}", result.reasoning);
                }
            }
            Err(e) => {
                eprintln!("Decomposition failed: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if let Some(ref run_file) = cli.run {
        let task_file = orchestrator::load_tasks(run_file)?;
        return orchestrator::run_tasks(task_file, cli.parallel);
    }

    if cli.clean {
        return run_clean(cli.older_than.as_deref(), cli.finished, cli.dry_run);
    }

    if cli.history {
        history::print_history(&cli.since);
        return Ok(());
    }

    if cli.stats {
        history::print_stats(&cli.since);
        return Ok(());
    }

    if cli.new_session {
        return launch_session(&cli.cwd, cli.prompt.as_deref(), cli.resume.as_deref());
    }

    if cli.summary {
        return print_summary(&cli.since);
    }

    if cli.json && !cli.watch {
        return print_json(cli.demo, &filters);
    }

    if cli.list {
        return print_list(cli.demo, &filters);
    }

    if cli.watch {
        return run_watch(
            Duration::from_millis(cfg.interval),
            cli.json,
            &cli.format,
            &filters,
        );
    }

    let tick_rate = Duration::from_millis(cfg.interval);
    let theme_mode = theme::ThemeMode::detect(cli.theme.as_deref());
    let app_theme = theme::Theme::from_mode(theme_mode);

    if let Some(ref record_path) = cli.record {
        // Recording mode: use TeeWriter to capture exact ANSI output
        let term_size = crossterm::terminal::size().unwrap_or((120, 40));
        let mut rec = recorder::Recorder::new(record_path, term_size.0, term_size.1)?;
        let rec_ptr: *mut recorder::Recorder = &mut rec;

        enable_raw_mode()?;
        // SAFETY: rec outlives tee_writer and terminal (both dropped before rec)
        let tee_writer = unsafe { recorder::TeeWriter::new(rec_ptr) };
        execute!(io::stdout(), EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(tee_writer);
        let mut terminal = Terminal::new(backend)?;

        let max_dur = cli.duration.map(Duration::from_secs);

        let result = run_tui(
            &mut terminal,
            tick_rate,
            &cfg,
            app_theme,
            hook_registry,
            cli.demo,
            &filters,
            max_dur,
        );

        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        match rec.finish() {
            Ok(()) => {
                eprintln!("Saved to {record_path}");
            }
            Err(e) => {
                // For GIF conversion failures, the error message contains instructions
                eprintln!("{e}");
            }
        }

        result
    } else {
        // Normal mode: plain stdout
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let max_dur = cli.duration.map(Duration::from_secs);

        let result = run_tui(
            &mut terminal,
            tick_rate,
            &cfg,
            app_theme,
            hook_registry,
            cli.demo,
            &filters,
            max_dur,
        );

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        result
    }
}

fn launch_session(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> io::Result<()> {
    let request = launch::prepare(cwd, prompt, resume).map_err(io::Error::other)?;

    match launch::launch(&request) {
        Ok(target) => {
            println!(
                "Launched Claude session in {} at {}{}",
                target,
                request.cwd_path.display(),
                request.option_summary()
            );
            Ok(())
        }
        Err(e) => Err(io::Error::other(e)),
    }
}

fn print_doctor_transcripts() {
    println!();
    println!("Transcript Discovery");

    let sessions_dir = discovery::projects_dir().parent().unwrap().join("sessions");
    let projects_dir = discovery::projects_dir();

    // Check sessions directory
    let sessions_exists = sessions_dir.exists();
    println!(
        "  [{}] sessions dir: {}",
        if sessions_exists { "ok" } else { "!!" },
        sessions_dir.display()
    );

    // Check projects directory
    let projects_exists = projects_dir.exists();
    println!(
        "  [{}] projects dir: {}",
        if projects_exists { "ok" } else { "!!" },
        projects_dir.display()
    );

    if !sessions_exists {
        println!("      No session pointer files found — Claude Code may not have run yet");
        return;
    }

    // Scan sessions and attempt resolution
    let mut sessions = discovery::scan_sessions();
    if sessions.is_empty() {
        println!("  [--] no session pointer files found");
        return;
    }

    process::fetch_and_enrich(&mut sessions);
    let alive: Vec<_> = sessions
        .iter()
        .filter(|s| s.status != session::SessionStatus::Finished)
        .collect();

    if alive.is_empty() {
        println!("  [--] no active Claude Code sessions");
        return;
    }

    // Resolve JSONL paths for alive sessions
    let mut alive_sessions: Vec<_> = alive.into_iter().cloned().collect();
    for s in &mut alive_sessions {
        discovery::resolve_jsonl_paths(std::slice::from_mut(s));
    }

    for s in &alive_sessions {
        let found = s.jsonl_path.is_some();
        let slug = s.cwd.trim_end_matches('/').replace('/', "-");
        let expected_dir = projects_dir.join(&slug);

        println!(
            "  [{}] PID {} ({})",
            if found { "ok" } else { "!!" },
            s.pid,
            s.project_name
        );
        println!("      cwd:  {}", s.cwd);
        println!("      slug: {slug}");
        if let Some(ref path) = s.jsonl_path {
            println!("      jsonl: {}", path.display());
        } else {
            println!(
                "      expected dir: {} (exists={})",
                expected_dir.display(),
                expected_dir.exists()
            );
            let expected_file = expected_dir.join(format!("{}.jsonl", s.session_id));
            println!(
                "      expected file: {} (exists={})",
                expected_file.display(),
                expected_file.exists()
            );
            println!(
                "      fix: check that Claude Code's project directory slug matches the cwd encoding above"
            );
        }
    }
}

fn print_doctor() -> io::Result<()> {
    let report = terminals::doctor_report();
    println!("{}", terminals::format_doctor_report(&report));

    // Transcript discovery diagnostics
    print_doctor_transcripts();

    // Brain diagnostics
    let cfg = config::Config::load();
    println!();
    println!("Brain (local LLM)");

    // Check curl
    let curl_ok = std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    println!(
        "  [{}] curl: {}",
        if curl_ok { "ok" } else { "!!" },
        if curl_ok {
            "available (required for brain HTTP calls)"
        } else {
            "not found — brain requires curl on PATH"
        }
    );

    // Check ollama binary
    let ollama_ok = std::process::Command::new("ollama")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    println!(
        "  [{}] ollama: {}",
        if ollama_ok { "ok" } else { "--" },
        if ollama_ok {
            "installed"
        } else {
            "not found (install: brew install ollama)"
        }
    );

    // Check endpoint reachability
    if let Some(ref brain) = cfg.brain {
        println!(
            "  Config: enabled={}, model={}, auto={}, few_shot={}",
            brain.enabled, brain.model, brain.auto_mode, brain.few_shot_count
        );
        let endpoint_ok = check_brain_endpoint(&brain.endpoint, brain.timeout_ms);
        println!(
            "  [{}] endpoint {}: {}",
            if endpoint_ok { "ok" } else { "!!" },
            brain.endpoint,
            if endpoint_ok {
                "reachable"
            } else {
                "not reachable"
            }
        );
        if !endpoint_ok {
            println!("      fix: start ollama with `ollama serve`, or check --brain-endpoint URL");
        }
    } else {
        println!("  Config: not configured");
        println!("  To enable: add [brain] section to .claudectl.toml or use --brain flag");
    }

    Ok(())
}

fn check_brain_endpoint(endpoint: &str, timeout_ms: u64) -> bool {
    let timeout_secs = (timeout_ms / 1000).max(1);
    std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            &timeout_secs.to_string(),
            endpoint,
        ])
        .output()
        .is_ok_and(|o| {
            let code = String::from_utf8_lossy(&o.stdout);
            // Any HTTP response (even 404/405) means the server is up
            code.trim() != "000"
        })
}

fn parse_duration_str(s: &str) -> Duration {
    let s = s.trim();
    if let Some(hours) = s.strip_suffix('h') {
        if let Ok(h) = hours.parse::<u64>() {
            return Duration::from_secs(h * 3600);
        }
    }
    if let Some(mins) = s.strip_suffix('m') {
        if let Ok(m) = mins.parse::<u64>() {
            return Duration::from_secs(m * 60);
        }
    }
    if let Some(days) = s.strip_suffix('d') {
        if let Ok(d) = days.parse::<u64>() {
            return Duration::from_secs(d * 86400);
        }
    }
    Duration::from_secs(24 * 3600) // default 24h
}

fn parse_status_filter(value: Option<&str>) -> io::Result<StatusFilter> {
    match value {
        Some(raw) => StatusFilter::parse(raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Invalid --filter-status value: {raw}. Expected one of: all, needs-input, processing, waiting, unknown, idle, finished"
                ),
            )
        }),
        None => Ok(StatusFilter::All),
    }
}

fn parse_focus_filter(value: Option<&str>) -> io::Result<FocusFilter> {
    match value {
        Some(raw) => FocusFilter::parse(raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Invalid --focus value: {raw}. Expected one of: all, attention, over-budget, high-context, unknown-telemetry, conflict"
                ),
            )
        }),
        None => Ok(FocusFilter::All),
    }
}

fn apply_filters(app: &mut App, filters: &ViewFilters) {
    app.status_filter = filters.status_filter;
    app.focus_filter = filters.focus_filter;
    app.search_query = filters.search.trim().to_string();
    app.search_buffer.clear();
    app.search_mode = false;
    let len = app.visible_session_count();
    if len == 0 {
        app.table_state.select(None);
    } else if app.table_state.selected().is_none() {
        app.table_state.select(Some(0));
    } else if let Some(sel) = app.table_state.selected() {
        if sel >= len {
            app.table_state.select(Some(len - 1));
        }
    }
}

fn run_clean(older_than: Option<&str>, finished_only: bool, dry_run: bool) -> io::Result<()> {
    let min_age = older_than.map(parse_duration_str);
    let now = std::time::SystemTime::now();

    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));

    // Collect active PIDs to avoid deleting live sessions
    let active_pids: std::collections::HashSet<u32> = {
        let app = App::new();
        app.sessions.iter().map(|s| s.pid).collect()
    };

    let mut removed_sessions = 0u64;
    let mut removed_jsonl = 0u64;
    let mut freed_bytes = 0u64;

    // Phase 1: Clean session JSON files in ~/.claude/sessions/
    let sessions_dir = home.join(".claude/sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let pid: u32 = match stem.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };

            // Never delete active sessions
            if active_pids.contains(&pid) {
                continue;
            }

            // Check age if --older-than is set
            if let Some(min_age) = min_age {
                let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
                if let Some(modified) = modified {
                    let age = now.duration_since(modified).unwrap_or_default();
                    if age < min_age {
                        continue;
                    }
                }
            }

            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if dry_run {
                println!("  would remove: {} ({} bytes)", path.display(), size);
            } else {
                let _ = std::fs::remove_file(&path);
            }
            removed_sessions += 1;
            freed_bytes += size;
        }
    }

    // Phase 2: Clean JSONL transcript files in ~/.claude/projects/*/
    let projects_dir = home.join(".claude/projects");
    if let Ok(project_entries) = std::fs::read_dir(&projects_dir) {
        for project_entry in project_entries.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&project_path) else {
                continue;
            };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }

                let metadata = match file_entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                // Check age if --older-than is set
                if let Some(min_age) = min_age {
                    let modified = metadata.modified().ok();
                    if let Some(modified) = modified {
                        let age = now.duration_since(modified).unwrap_or_default();
                        if age < min_age {
                            continue;
                        }
                    }
                }

                // If --finished only, skip JSONL files whose corresponding session is still active
                if finished_only {
                    // Check if any active session is using this JSONL
                    let app = App::new();
                    let is_active = app.sessions.iter().any(|s| {
                        s.jsonl_path
                            .as_ref()
                            .map(|p| p == &file_path)
                            .unwrap_or(false)
                    });
                    if is_active {
                        continue;
                    }
                }

                let size = metadata.len();
                if dry_run {
                    println!("  would remove: {} ({} bytes)", file_path.display(), size);
                } else {
                    let _ = std::fs::remove_file(&file_path);
                }
                removed_jsonl += 1;
                freed_bytes += size;
            }
        }
    }

    let freed_str = if freed_bytes >= 1_073_741_824 {
        format!("{:.1} GB", freed_bytes as f64 / 1_073_741_824.0)
    } else if freed_bytes >= 1_048_576 {
        format!("{:.1} MB", freed_bytes as f64 / 1_048_576.0)
    } else if freed_bytes >= 1024 {
        format!("{:.1} KB", freed_bytes as f64 / 1024.0)
    } else {
        format!("{freed_bytes} bytes")
    };

    if dry_run {
        println!();
        println!(
            "Dry run: would remove {} sessions + {} transcripts, freeing {}",
            removed_sessions, removed_jsonl, freed_str
        );
    } else if removed_sessions + removed_jsonl == 0 {
        println!("Nothing to clean up.");
    } else {
        println!(
            "Removed {} sessions + {} transcripts, freed {}",
            removed_sessions, removed_jsonl, freed_str
        );
    }

    Ok(())
}

fn print_summary(since: &str) -> io::Result<()> {
    let since_duration = parse_duration_str(since);
    let app = App::new();

    if app.sessions.is_empty() {
        println!("No active Claude sessions.");
        return Ok(());
    }

    for s in &app.sessions {
        let status_color = match s.status {
            session::SessionStatus::Processing => "\x1b[32m",
            session::SessionStatus::NeedsInput => "\x1b[35m",
            session::SessionStatus::WaitingInput => "\x1b[33m",
            session::SessionStatus::Unknown => "\x1b[34m",
            session::SessionStatus::Idle => "\x1b[90m",
            session::SessionStatus::Finished => "\x1b[31m",
        };
        let reset = "\x1b[0m";
        let status_text = if s.status == session::SessionStatus::Unknown {
            format!("Unknown: {}", s.telemetry_label())
        } else {
            s.status.to_string()
        };

        println!(
            "=== {} ({}, {}, {status_color}{}{reset}) ===",
            s.display_name(),
            s.format_elapsed(),
            s.format_cost(),
            status_text,
        );

        // Git stats from session's cwd
        let since_secs = since_duration.as_secs();
        let git_since = format!("{since_secs} seconds ago");

        let git_log = std::process::Command::new("git")
            .args(["log", "--oneline", &format!("--since={git_since}")])
            .current_dir(&s.cwd)
            .output();

        if let Ok(output) = git_log {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let commits: Vec<&str> = stdout.lines().collect();
            if !commits.is_empty() {
                println!("  Commits: {}", commits.len());
                for c in commits.iter().take(5) {
                    println!("    {c}");
                }
                if commits.len() > 5 {
                    println!("    ... and {} more", commits.len() - 5);
                }
            }
        }

        let git_diff = std::process::Command::new("git")
            .args(["diff", "--stat", "HEAD"])
            .current_dir(&s.cwd)
            .output();

        if let Ok(output) = git_diff {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = stdout.lines().collect();
            if !lines.is_empty() {
                let file_count = lines.len().saturating_sub(1); // last line is summary
                if file_count > 0 {
                    println!("  Files changed: {file_count}");
                }
            }
        }

        // Token summary
        let total_tokens = s.total_input_tokens + s.total_output_tokens;
        if total_tokens > 0 {
            println!(
                "  Tokens: {} in / {} out",
                format_count(s.total_input_tokens),
                format_count(s.total_output_tokens)
            );
        }

        // Model and context
        if !s.model.is_empty() {
            let context_text = if s.has_usage_metrics() {
                format!("{}%", s.context_percent() as u32)
            } else {
                "n/a".to_string()
            };
            let estimate_note = if s.cost_estimate_unverified {
                " [fallback estimate]"
            } else if s.model_profile_source == "override" {
                " [config override]"
            } else {
                ""
            };
            println!(
                "  Model: {}{} (context: {})",
                s.model, estimate_note, context_text
            );
        }
        if s.status == session::SessionStatus::Unknown || !s.has_usage_metrics() {
            println!("  Telemetry: {}", s.telemetry_label());
        }

        if s.subagent_count > 0 {
            println!("  Subagents: {}", s.format_subagent_summary());
        }

        println!();
    }

    let total_cost: f64 = app.sessions.iter().map(|s| s.cost_usd).sum();
    println!("Total cost: ${total_cost:.2}");

    Ok(())
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn make_app(demo: bool, filters: &ViewFilters) -> App {
    let mut app = if demo {
        let mut app = App::new();
        app.demo_mode = true;
        app.sessions = demo::generate_sessions(10);
        app
    } else {
        App::new()
    };
    apply_filters(&mut app, filters);
    app
}

fn print_json(demo: bool, filters: &ViewFilters) -> io::Result<()> {
    let app = make_app(demo, filters);
    let values: Vec<serde_json::Value> = app
        .visible_sessions()
        .iter()
        .map(|s| s.to_json_value())
        .collect();
    let json = serde_json::to_string_pretty(&values).unwrap_or_else(|_| "[]".to_string());
    println!("{json}");
    Ok(())
}

fn print_list(demo: bool, filters: &ViewFilters) -> io::Result<()> {
    let app = make_app(demo, filters);
    let visible_sessions = app.visible_sessions();

    if visible_sessions.is_empty() {
        if app.has_active_filters() {
            println!("No sessions match the current filters.");
        } else {
            println!("No active Claude sessions.");
        }
        if app.has_active_filters() {
            println!("  ({})", app.filter_summary());
        }
        return Ok(());
    }

    println!(
        "{:<7} {:<16} {:<12} {:<8} {:<8} {:<9} {:<10} {:<6} {:<6} TOKENS",
        "PID", "PROJECT", "STATUS", "CTX%", "COST", "$/HR", "ELAPSED", "CPU%", "MEM"
    );
    println!("{}", "-".repeat(105));

    for s in visible_sessions {
        let status_text = if s.status == session::SessionStatus::Unknown {
            s.telemetry_status.short_label().to_string()
        } else {
            s.status.to_string()
        };
        println!(
            "{:<7} {:<16} {:<12} {:<8} {:<8} {:<9} {:<10} {:<6.1} {:<6} {}",
            s.pid,
            s.display_name(),
            status_text,
            s.format_context(),
            s.format_cost(),
            s.format_burn_rate(),
            s.format_elapsed(),
            s.cpu_percent,
            s.format_mem(),
            s.format_tokens(),
        );
    }

    let total_cost: f64 = app.visible_sessions().iter().map(|s| s.cost_usd).sum();
    println!("{}", "-".repeat(105));
    println!("Total cost: ${total_cost:.2}");
    if app.has_active_filters() {
        println!("{}", app.filter_summary());
    }

    Ok(())
}

fn run_watch(
    tick_rate: Duration,
    json_mode: bool,
    format_str: &str,
    filters: &ViewFilters,
) -> io::Result<()> {
    use crate::session::SessionStatus;
    use std::collections::HashMap;

    let mut app = App::new();
    apply_filters(&mut app, filters);
    let mut prev_statuses: HashMap<u32, SessionStatus> =
        app.sessions.iter().map(|s| (s.pid, s.status)).collect();

    // Print initial state for all sessions
    for s in app.visible_sessions() {
        if json_mode {
            let obj = serde_json::json!({
                "event": "initial",
                "pid": s.pid,
                "project": s.display_name(),
                "status": s.status.to_string(),
                "telemetry": s.telemetry_label(),
                "cost_usd": if s.has_usage_metrics() { serde_json::json!((s.cost_usd * 100.0).round() / 100.0) } else { serde_json::Value::Null },
                "context_pct": if s.has_usage_metrics() { serde_json::json!((s.context_percent() * 100.0).round() / 100.0) } else { serde_json::Value::Null },
                "elapsed_secs": s.elapsed.as_secs(),
            });
            println!("{}", serde_json::to_string(&obj).unwrap_or_default());
        } else {
            println!("{}", format_session(format_str, s));
        }
    }

    loop {
        std::thread::sleep(tick_rate);
        app.tick();
        let visible_pids: std::collections::HashSet<u32> =
            app.visible_sessions().iter().map(|s| s.pid).collect();

        for s in &app.sessions {
            let prev = prev_statuses.get(&s.pid).copied();
            let changed = prev.is_none_or(|p| p != s.status);

            if !changed || !visible_pids.contains(&s.pid) {
                continue;
            }

            if json_mode {
                let obj = serde_json::json!({
                    "event": "status_change",
                    "pid": s.pid,
                    "project": s.display_name(),
                    "old_status": prev.map(|p| p.to_string()).unwrap_or_default(),
                    "new_status": s.status.to_string(),
                    "telemetry": s.telemetry_label(),
                    "cost_usd": if s.has_usage_metrics() { serde_json::json!((s.cost_usd * 100.0).round() / 100.0) } else { serde_json::Value::Null },
                    "context_pct": if s.has_usage_metrics() { serde_json::json!((s.context_percent() * 100.0).round() / 100.0) } else { serde_json::Value::Null },
                    "elapsed_secs": s.elapsed.as_secs(),
                });
                println!("{}", serde_json::to_string(&obj).unwrap_or_default());
            } else {
                println!("{}", format_session(format_str, s));
            }
        }

        prev_statuses = app.sessions.iter().map(|s| (s.pid, s.status)).collect();
    }
}

fn format_session(fmt: &str, s: &session::ClaudeSession) -> String {
    let cost = if s.has_usage_metrics() {
        format!("{:.2}", s.cost_usd)
    } else {
        "n/a".to_string()
    };
    let context = if s.has_usage_metrics() {
        format!("{}", s.context_percent() as u32)
    } else {
        "n/a".to_string()
    };
    fmt.replace("{pid}", &s.pid.to_string())
        .replace("{project}", s.display_name())
        .replace("{status}", &s.status.to_string())
        .replace("{cost}", &cost)
        .replace("{context}", &context)
}

#[allow(clippy::too_many_arguments)]
fn run_tui<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    tick_rate: Duration,
    cfg: &config::Config,
    app_theme: theme::Theme,
    hook_registry: hooks::HookRegistry,
    demo_mode: bool,
    filters: &ViewFilters,
    max_duration: Option<Duration>,
) -> io::Result<()> {
    let mut app = App::new();
    app.notify = cfg.notify;
    app.debug = cfg.debug;
    app.webhook_url = cfg.webhook.clone();
    app.webhook_filter = cfg.webhook_on.clone();
    app.budget_usd = cfg.budget;
    app.kill_on_budget = cfg.kill_on_budget;
    app.grouped_view = cfg.grouped;
    app.theme = app_theme;
    app.hooks = hook_registry;
    app.daily_limit = cfg.daily_limit;
    app.weekly_limit = cfg.weekly_limit;
    app.context_warn_threshold = cfg.context_warn_threshold;
    app.rules = cfg.rules.clone();
    app.health_thresholds = cfg.health.clone();
    app.file_conflicts_enabled = cfg.file_conflicts;
    app.auto_deny_file_conflicts = cfg.auto_deny_file_conflicts;
    app.idle_config = cfg.idle.clone();
    app.brain_config = cfg.brain.clone();
    if let Some(ref brain_cfg) = cfg.brain {
        if brain_cfg.enabled {
            if check_brain_endpoint(&brain_cfg.endpoint, brain_cfg.timeout_ms) {
                app.brain_engine = Some(brain::engine::BrainEngine::new(brain_cfg.clone()));
                app.status_msg = format!(
                    "Brain: connected to {} ({})",
                    brain_cfg.endpoint, brain_cfg.model
                );
            } else {
                app.status_msg = format!(
                    "Error: Brain endpoint {} not reachable — run `claudectl --doctor` or start ollama",
                    brain_cfg.endpoint
                );
            }
        }
    }
    app.demo_mode = demo_mode;
    apply_filters(&mut app, filters);

    if demo_mode {
        app.daily_limit = Some(50.0);
        app.budget_usd = Some(10.0);
        app.rules = demo::demo_rules();
        // Create a stub brain engine so the status bar can show brain suggestions
        if app.brain_engine.is_none() {
            app.brain_engine = Some(brain::engine::BrainEngine::new(
                config::BrainConfig::default(),
            ));
        }
        // Re-refresh to replace real sessions discovered during App::new()
        app.refresh();
    }

    let mut last_tick = Instant::now();
    let tui_start = Instant::now();
    let mut sess_recs: std::collections::HashMap<u32, session_recorder::SessionRecorder> =
        std::collections::HashMap::new();
    let term_size = crossterm::terminal::size().unwrap_or((120, 40));

    loop {
        // Auto-quit after --duration seconds (graceful exit, flushes recordings)
        if let Some(max) = max_duration {
            if tui_start.elapsed() >= max {
                for (_, rec) in sess_recs.iter_mut() {
                    let _ = rec.finish();
                }
                return Ok(());
            }
        }
        terminal.draw(|frame| {
            ui::table::render(frame, frame.area(), &app);
        })?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if !app.handle_key(key) {
                    // Finish all session recordings on quit
                    for (_, rec) in sess_recs.iter_mut() {
                        let _ = rec.finish();
                    }
                    return Ok(());
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.tick();
            last_tick = Instant::now();

            // Start recorders for newly added recordings
            for (pid, path) in &app.session_recordings {
                if sess_recs.contains_key(pid) {
                    continue;
                }
                if let Some(session) = app.sessions.iter().find(|s| s.pid == *pid) {
                    if let Some(ref jsonl) = session.jsonl_path {
                        let name = session.display_name();
                        match session_recorder::SessionRecorder::new(
                            jsonl,
                            path,
                            name,
                            term_size.0,
                            term_size.1,
                        ) {
                            Ok(r) => {
                                sess_recs.insert(*pid, r);
                            }
                            Err(e) => {
                                app.status_msg = format!("Record error: {e}");
                            }
                        }
                    }
                }
            }

            // Poll all active recorders
            for (_, rec) in sess_recs.iter_mut() {
                let _ = rec.poll();
            }

            // Finish recorders that were removed from app.session_recordings
            let stopped: Vec<u32> = sess_recs
                .keys()
                .filter(|pid| !app.session_recordings.contains_key(pid))
                .copied()
                .collect();
            for pid in stopped {
                if let Some(mut rec) = sess_recs.remove(&pid) {
                    match rec.finish() {
                        Ok(()) => {}
                        Err(e) => {
                            app.status_msg = format!("{e}");
                        }
                    }
                }
            }
        }
    }
}

/// Path to the brain gate mode state file.
fn brain_gate_mode_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home)
        .join(".claudectl")
        .join("brain")
        .join("gate-mode")
}

/// Read the current brain gate mode from disk. Returns "on" if no file exists.
fn read_brain_gate_mode() -> String {
    let path = brain_gate_mode_path();
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "on".into())
}

/// Set the brain gate mode (on/off/auto) and print confirmation.
fn run_brain_mode(mode: &str) -> io::Result<()> {
    match mode {
        "on" | "off" | "auto" => {}
        "status" | "" => {
            let current = read_brain_gate_mode();
            println!("Brain gate mode: {current}");
            println!();
            println!("Modes:");
            println!("  on   — brain evaluates tool calls, denies dangerous ones (default)");
            println!("  off  — brain disabled, all tool calls pass through");
            println!("  auto — brain auto-approves above confidence threshold");
            return Ok(());
        }
        _ => {
            eprintln!("Unknown brain mode: {mode}");
            eprintln!("Valid modes: on, off, auto, status");
            std::process::exit(1);
        }
    }

    let path = brain_gate_mode_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if mode == "on" {
        // "on" is the default — remove the file so absence = on
        let _ = std::fs::remove_file(&path);
    } else {
        std::fs::write(&path, mode)?;
    }

    let description = match mode {
        "on" => "brain evaluates tool calls, denies dangerous ones",
        "off" => "brain disabled — all tool calls pass through to normal permission flow",
        "auto" => "brain auto-approves tool calls above confidence threshold",
        _ => unreachable!(),
    };

    println!("Brain gate mode set to: {mode}");
    println!("  {description}");
    Ok(())
}

/// Handle --insights: show insights or set mode (on/off/status).
/// Requires brain to be enabled.
fn run_insights(cfg: &config::Config, cli: &Cli, arg: &str) -> io::Result<()> {
    let brain_enabled = cfg.brain.as_ref().map(|b| b.enabled).unwrap_or(false) || cli.brain;

    if !brain_enabled {
        eprintln!(
            "Insights requires the brain. Use --brain or set brain.enabled = true in config."
        );
        std::process::exit(1);
    }

    match arg {
        "on" => {
            let _ = brain::insights::write_insights_mode("on");
            println!("Insights mode: on");
            println!("  Auto-generating insights every 10 decisions during brain distillation.");
            println!("  Run `claudectl --brain --insights` to view.");
        }
        "off" => {
            let _ = brain::insights::write_insights_mode("off");
            println!("Insights mode: off");
            println!(
                "  Auto-generation disabled. Run `claudectl --brain --insights` to generate on demand."
            );
        }
        "status" => {
            let mode = brain::insights::read_insights_mode();
            println!("Insights mode: {mode}");
            println!();
            println!("Modes:");
            println!("  on   — auto-generate insights every 10 decisions");
            println!("  off  — disabled, generate on demand only (default)");
        }
        "" => {
            // No argument: show insights
            brain::insights::print_insights();
        }
        _ => {
            eprintln!("Unknown insights argument: {arg}");
            eprintln!("Usage: --insights [on|off|status]");
            eprintln!("  No argument: show current insights");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Standalone brain query: builds a minimal context from CLI args, calls the
/// local LLM, and prints a JSON decision to stdout. Designed to be called
/// by Claude Code plugin hooks (PreToolUse) for inline approve/deny.
fn run_brain_query(cfg: &config::Config, cli: &Cli) -> io::Result<()> {
    // Respect brain gate mode — if off, skip immediately
    let gate_mode = read_brain_gate_mode();
    if gate_mode == "off" {
        let result = serde_json::json!({
            "action": "abstain",
            "reasoning": "Brain gate mode is off",
            "confidence": 0.0,
            "source": "gate",
        });
        println!("{}", serde_json::to_string(&result).unwrap());
        return Ok(());
    }

    let brain_cfg = cfg.brain.clone().unwrap_or_default();

    if !brain_cfg.enabled && !cli.brain {
        eprintln!("Brain is not enabled. Use --brain or set brain.enabled = true in config.");
        std::process::exit(1);
    }

    let tool_name = cli.tool.clone().unwrap_or_else(|| "unknown".into());
    let command = cli.tool_input.clone().unwrap_or_default();
    let project = cli.project.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "unknown".into())
    });

    // Step 1: Check static deny rules first (instant, no LLM needed)
    let auto_rules = cfg.rules.clone();
    let deny_rules: Vec<_> = auto_rules
        .iter()
        .filter(|r| r.action == rules::RuleAction::Deny)
        .cloned()
        .collect();

    // Build a minimal synthetic session for rule matching
    let mut synthetic = session::ClaudeSession::from_raw(session::RawSession {
        pid: std::process::id(),
        session_id: "brain-query".into(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into()),
        started_at: 0,
    });
    synthetic.project_name = project.clone();
    synthetic.status = session::SessionStatus::NeedsInput;
    synthetic.pending_tool_name = Some(tool_name.clone());
    synthetic.pending_tool_input = if command.is_empty() {
        None
    } else {
        Some(command.clone())
    };

    // Check deny rules
    if let Some(deny_match) = rules::evaluate(&deny_rules, &synthetic) {
        let result = serde_json::json!({
            "action": "deny",
            "reasoning": format!("Deny rule '{}' matched", deny_match.rule_name),
            "confidence": 1.0,
            "source": "rule",
        });
        println!("{}", serde_json::to_string(&result).unwrap());
        return Ok(());
    }

    // Step 2: Check approve rules
    let approve_rules: Vec<_> = auto_rules
        .iter()
        .filter(|r| r.action == rules::RuleAction::Approve)
        .cloned()
        .collect();
    if let Some(approve_match) = rules::evaluate(&approve_rules, &synthetic) {
        let result = serde_json::json!({
            "action": "approve",
            "reasoning": format!("Approve rule '{}' matched", approve_match.rule_name),
            "confidence": 1.0,
            "source": "rule",
        });
        println!("{}", serde_json::to_string(&result).unwrap());
        return Ok(());
    }

    // Step 3: Query the LLM brain
    let tool_display = if command.is_empty() {
        tool_name.clone()
    } else {
        format!("{tool_name}: {command}")
    };

    let session_summary = format!(
        "Project: {project} | Status: Needs Input | Pending tool: {tool_name} | Command: {command}"
    );

    // Load distilled preferences
    let pref_section = if let Some(prefs) = brain::decisions::load_preferences_for_project(&project)
    {
        let summary = brain::decisions::format_preference_summary(&prefs);
        format!("\n\n## Learned Preferences\n{summary}")
    } else {
        String::new()
    };

    // Load few-shot examples
    let few_shot_section = {
        let similar = brain::decisions::retrieve_similar(
            Some(&tool_name),
            &project,
            brain_cfg.few_shot_count.min(5),
            Some(brain::decisions::DecisionType::Session),
        );
        if similar.is_empty() {
            String::new()
        } else {
            let examples = brain::decisions::format_few_shot_examples(&similar);
            format!("\n\n## Past Decisions\n{examples}")
        }
    };

    let prompt = format!(
        "You are a session supervisor deciding whether to approve or deny a tool call.\n\
         \n## Session\n{session_summary}\
         {pref_section}\
         {few_shot_section}\n\
         \n## Decision\n\
         The session wants to run [{tool_display}]. \
         Should this be approved or denied? \
         Respond with JSON: {{\"action\": \"approve\"|\"deny\", \
         \"message\": \"...\", \"reasoning\": \"...\", \"confidence\": 0.0-1.0}}"
    );

    match brain::client::infer(&brain_cfg, &prompt) {
        Ok(suggestion) => {
            // Check adaptive threshold
            let threshold = brain::decisions::adaptive_threshold(Some(&tool_name)).unwrap_or(0.6);
            let below_threshold = suggestion.confidence < threshold;

            let result = serde_json::json!({
                "action": suggestion.action.label(),
                "reasoning": suggestion.reasoning,
                "confidence": suggestion.confidence,
                "message": suggestion.message,
                "source": "brain",
                "below_threshold": below_threshold,
                "threshold": threshold,
            });
            println!("{}", serde_json::to_string(&result).unwrap());
            Ok(())
        }
        Err(e) => {
            // On brain failure, output abstain (don't block the user)
            let result = serde_json::json!({
                "action": "abstain",
                "reasoning": format!("Brain query failed: {e}"),
                "confidence": 0.0,
                "source": "error",
            });
            println!("{}", serde_json::to_string(&result).unwrap());
            Ok(())
        }
    }
}

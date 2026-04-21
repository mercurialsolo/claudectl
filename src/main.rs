#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

mod app;
mod brain;
mod commands;
mod config;
#[cfg(feature = "coord")]
mod coord;
mod demo;
mod discovery;
mod health;
mod helpers;
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
pub(crate) struct ViewFilters {
    pub(crate) status_filter: StatusFilter,
    pub(crate) focus_filter: FocusFilter,
    pub(crate) search: String,
}

#[derive(Parser)]
#[command(
    name = "claudectl",
    version,
    about = "Monitor and manage Claude Code CLI agents"
)]
pub(crate) struct Cli {
    // ── Dashboard ───────────────────────────────────────────────────────
    /// Refresh interval in milliseconds
    #[arg(short, long, default_value_t = 2000, help_heading = "Dashboard")]
    pub(crate) interval: u64,

    /// Color theme: dark, light, or none (respects NO_COLOR env var)
    #[arg(long, help_heading = "Dashboard")]
    pub(crate) theme: Option<String>,

    /// Enable debug mode: show timing metrics in the footer
    #[arg(long, help_heading = "Dashboard")]
    pub(crate) debug: bool,

    /// Run with deterministic fake sessions for screenshots and recordings
    #[arg(long, help_heading = "Dashboard")]
    pub(crate) demo: bool,

    // ── Output Modes ───────────────────────────────────────────────────
    /// Print session list to stdout and exit (no TUI)
    #[arg(short, long, help_heading = "Output Modes")]
    pub(crate) list: bool,

    /// Print JSON array of sessions and exit
    #[arg(long, help_heading = "Output Modes")]
    pub(crate) json: bool,

    /// Stream status changes to stdout (no TUI). Only prints when status changes.
    #[arg(short, long, help_heading = "Output Modes")]
    pub(crate) watch: bool,

    /// Run headless with brain, coordination, and context rot prevention active (no TUI).
    /// Attach a dashboard with `claudectl` in another terminal.
    #[arg(long, help_heading = "Output Modes")]
    pub(crate) headless: bool,

    /// Output format for watch mode. Placeholders: {pid}, {project}, {status}, {cost}, {context}
    #[arg(
        long,
        default_value = "{pid} {project}: {status} (${cost}, ctx {context}%)",
        help_heading = "Output Modes"
    )]
    pub(crate) format: String,

    /// Show summary of session activity and exit
    #[arg(long, help_heading = "Output Modes")]
    pub(crate) summary: bool,

    /// Time window for --summary, --history, --stats (e.g., "8h", "24h", "30m")
    #[arg(long, default_value = "24h", help_heading = "Output Modes")]
    pub(crate) since: String,

    // ── Filtering ──────────────────────────────────────────────────────
    /// Filter sessions by status (e.g., "NeedsInput", "Processing", "Finished")
    #[arg(long, help_heading = "Filtering")]
    pub(crate) filter_status: Option<String>,

    /// Focus on a high-signal subset: attention, over-budget, high-context, unknown-telemetry, conflict
    #[arg(long, help_heading = "Filtering")]
    pub(crate) focus: Option<String>,

    /// Search project/model/session text
    #[arg(long, help_heading = "Filtering")]
    pub(crate) search: Option<String>,

    // ── Session Management ─────────────────────────────────────────────
    /// Launch a new Claude Code session in the given directory
    #[arg(long = "new", help_heading = "Session Management")]
    pub(crate) new_session: bool,

    /// Working directory for the new session (used with --new)
    #[arg(long, default_value = ".", help_heading = "Session Management")]
    pub(crate) cwd: String,

    /// Prompt to send to the new session (used with --new)
    #[arg(long, help_heading = "Session Management")]
    pub(crate) prompt: Option<String>,

    /// Resume a session by ID (used with --new)
    #[arg(long, help_heading = "Session Management")]
    pub(crate) resume: Option<String>,

    // ── Budget & Notifications ─────────────────────────────────────────
    /// Per-session budget in USD. Alert at 80%, optionally kill at 100%.
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) budget: Option<f64>,

    /// Auto-kill sessions that exceed the budget (requires --budget)
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) kill_on_budget: bool,

    /// Enable desktop notifications on NeedsInput transitions
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) notify: bool,

    /// Webhook URL to POST JSON on status changes
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) webhook: Option<String>,

    /// Only fire webhook on these status transitions (comma-separated, e.g. "NeedsInput,Finished")
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) webhook_on: Option<String>,

    // ── Brain (Local LLM) ──────────────────────────────────────────────
    /// Enable local LLM brain for session advisory (requires ollama or compatible endpoint)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain: bool,

    /// Auto-execute brain suggestions without confirmation (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) auto_run: bool,

    /// LLM endpoint URL for brain (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) url: Option<String>,

    /// Override brain model name (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_model: Option<String>,

    /// Run brain eval scenarios against the local LLM and report results
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_eval: bool,

    /// List brain prompt templates and their source (built-in vs user override)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_prompts: bool,

    /// Brain statistics and metrics (subcommands: learning-curve, accuracy, baseline, false-approve, help)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_stats: Option<String>,

    /// Query the brain for a single tool-call decision and exit (JSON output).
    /// Used by Claude Code plugin hooks for inline approve/deny.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_query: bool,

    /// Tool name for --brain-query (e.g., "Bash", "Write", "Edit")
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) tool: Option<String>,

    /// Command or input for --brain-query (e.g., "rm -rf /tmp")
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) tool_input: Option<String>,

    /// Project name for --brain-query context (defaults to current directory name)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) project: Option<String>,

    /// Set brain gate mode: on (default), off (disable), auto (full auto-approve).
    /// Controls whether the Claude Code plugin hook queries the brain.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) mode: Option<String>,

    /// Show auto-generated insights, or set mode (on/off/status).
    /// Requires --brain or brain.enabled in config.
    /// Without argument: show current insights.
    /// With argument: set insights mode (on = auto-generate, off = disable).
    #[arg(long, help_heading = "Brain (Local LLM)", num_args = 0..=1, default_missing_value = "")]
    pub(crate) insights: Option<String>,

    // ── Orchestration ──────────────────────────────────────────────────
    /// Analyze a prompt and suggest parallel sub-tasks (outputs TaskFile JSON)
    #[arg(long, help_heading = "Orchestration")]
    pub(crate) decompose: Option<String>,

    /// Run tasks from a JSON file (e.g., claudectl --run tasks.json)
    #[arg(long, help_heading = "Orchestration")]
    pub(crate) run: Option<String>,

    /// Run independent tasks in parallel (used with --run)
    #[arg(long, help_heading = "Orchestration")]
    pub(crate) parallel: bool,

    // ── Coordination ──────────────────────────────────────────────────
    /// Coordination layer inspection (events, leases, blockers, handoffs, interrupts, memory)
    #[cfg(feature = "coord")]
    #[arg(long, help_heading = "Coordination")]
    coord: Option<String>,

    // ── Recording ──────────────────────────────────────────────────────
    /// Record the TUI session as an asciicast v2 file (e.g., --record demo.cast)
    #[arg(long, help_heading = "Recording")]
    pub(crate) record: Option<String>,

    /// Auto-quit the TUI after this many seconds (useful with --demo --record)
    #[arg(long, help_heading = "Recording")]
    pub(crate) duration: Option<u64>,

    // ── Cleanup ────────────────────────────────────────────────────────
    /// Clean up old session data (JSONL transcripts, session JSON files)
    #[arg(long, help_heading = "Cleanup")]
    pub(crate) clean: bool,

    /// Only clean sessions older than this duration (e.g., "7d", "24h"). Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    pub(crate) older_than: Option<String>,

    /// Only clean sessions that have finished. Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    pub(crate) finished: bool,

    /// Show what would be removed without deleting. Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    pub(crate) dry_run: bool,

    // ── History & Diagnostics ──────────────────────────────────────────
    /// Show history of completed sessions and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) history: bool,

    /// Show aggregated session statistics and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) stats: bool,

    /// Show resolved configuration and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) config: bool,

    /// Print an annotated default config template to stdout and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) config_template: bool,

    /// List configured event hooks and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) hooks: bool,

    /// Diagnose terminal integration and setup requirements
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) doctor: bool,

    /// Write diagnostic logs to a file (for debugging/bug reports)
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) log: Option<String>,

    // ── Setup ─────────────────────────────────────────────────────────
    /// Wire up Claude Code hooks in .claude/settings.json and exit
    #[arg(long, help_heading = "Setup")]
    pub(crate) init: bool,

    /// Remove claudectl hooks from .claude/settings.json and exit
    #[arg(long, help_heading = "Setup", conflicts_with = "init")]
    pub(crate) uninstall: bool,

    /// Configuration scope: user (global ~/.claude/settings.json) or project (.claude/settings.local.json)
    #[arg(short, long, default_value = "user", help_heading = "Setup")]
    pub(crate) scope: String,
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
        status_filter: commands::parse_status_filter(cli.filter_status.as_deref())?,
        focus_filter: commands::parse_focus_filter(cli.focus.as_deref())?,
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
        return commands::print_doctor();
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
        return commands::run_brain_query(&cfg, &cli);
    }

    if let Some(ref mode) = cli.mode {
        return commands::run_brain_mode(mode);
    }

    if let Some(ref insights_arg) = cli.insights {
        return commands::run_insights(&cfg, &cli, insights_arg);
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
        return commands::run_clean(cli.older_than.as_deref(), cli.finished, cli.dry_run);
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
        return commands::launch_session(&cli.cwd, cli.prompt.as_deref(), cli.resume.as_deref());
    }

    if cli.summary {
        return commands::print_summary(&cli.since);
    }

    if cli.headless {
        return commands::run_headless(Duration::from_millis(cfg.interval), &cfg, cli.json);
    }

    if cli.json && !cli.watch {
        return commands::print_json(cli.demo, &filters);
    }

    if cli.list {
        return commands::print_list(cli.demo, &filters);
    }

    if cli.watch {
        return commands::run_watch(
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
            if commands::check_brain_endpoint(&brain_cfg.endpoint, brain_cfg.timeout_ms) {
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
    commands::apply_filters(&mut app, filters);

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

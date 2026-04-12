#![allow(clippy::collapsible_if)]

mod app;
mod config;
mod discovery;
mod history;
mod logger;
mod monitor;
mod orchestrator;
mod process;
mod session;
mod terminals;
mod theme;
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

use app::App;

#[derive(Parser)]
#[command(
    name = "claudectl",
    version,
    about = "Monitor and manage Claude Code CLI agents"
)]
struct Cli {
    /// Refresh interval in milliseconds
    #[arg(short, long, default_value_t = 2000)]
    interval: u64,

    /// Print session list to stdout and exit (no TUI)
    #[arg(short, long)]
    list: bool,

    /// Enable desktop notifications on NeedsInput transitions
    #[arg(long)]
    notify: bool,

    /// Print JSON array of sessions and exit
    #[arg(long)]
    json: bool,

    /// Stream status changes to stdout (no TUI). Only prints when status changes.
    #[arg(short, long)]
    watch: bool,

    /// Output format for watch mode. Placeholders: {pid}, {project}, {status}, {cost}, {context}
    #[arg(
        long,
        default_value = "{pid} {project}: {status} (${cost}, ctx {context}%)"
    )]
    format: String,

    /// Enable debug mode: show timing metrics in the footer
    #[arg(long)]
    debug: bool,

    /// Show summary of session activity and exit
    #[arg(long)]
    summary: bool,

    /// Time window for summary (e.g., "8h", "24h", "30m"). Default: 24h.
    #[arg(long, default_value = "24h")]
    since: String,

    /// Webhook URL to POST JSON on status changes
    #[arg(long)]
    webhook: Option<String>,

    /// Only fire webhook on these status transitions (comma-separated, e.g. "NeedsInput,Finished")
    #[arg(long)]
    webhook_on: Option<String>,

    /// Launch a new Claude Code session in the given directory
    #[arg(long = "new")]
    new_session: bool,

    /// Working directory for the new session (used with --new)
    #[arg(long, default_value = ".")]
    cwd: String,

    /// Prompt to send to the new session (used with --new)
    #[arg(long)]
    prompt: Option<String>,

    /// Resume a session by ID (used with --new)
    #[arg(long)]
    resume: Option<String>,

    /// Per-session budget in USD. Alert at 80%, optionally kill at 100%.
    #[arg(long)]
    budget: Option<f64>,

    /// Auto-kill sessions that exceed the budget (requires --budget)
    #[arg(long)]
    kill_on_budget: bool,

    /// Show resolved configuration and exit
    #[arg(long)]
    config: bool,

    /// Color theme: dark, light, or none (respects NO_COLOR env var)
    #[arg(long)]
    theme: Option<String>,

    /// Write diagnostic logs to a file (for debugging/bug reports)
    #[arg(long)]
    log: Option<String>,

    /// Show history of completed sessions and exit
    #[arg(long)]
    history: bool,

    /// Show aggregated session statistics and exit
    #[arg(long)]
    stats: bool,

    /// Run tasks from a JSON file (e.g., claudectl --run tasks.json)
    #[arg(long)]
    run: Option<String>,

    /// Run independent tasks in parallel (used with --run)
    #[arg(long)]
    parallel: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

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

    if cli.config {
        cfg.print_resolved();
        return Ok(());
    }

    if let Some(ref run_file) = cli.run {
        let task_file = orchestrator::load_tasks(run_file)?;
        return orchestrator::run_tasks(task_file, cli.parallel);
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
        return print_json();
    }

    if cli.list {
        return print_list();
    }

    if cli.watch {
        return run_watch(Duration::from_millis(cfg.interval), cli.json, &cli.format);
    }

    let tick_rate = Duration::from_millis(cfg.interval);

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Detect theme
    let theme_mode = theme::ThemeMode::detect(cli.theme.as_deref());
    let app_theme = theme::Theme::from_mode(theme_mode);

    // Run app
    let result = run(&mut terminal, tick_rate, &cfg, app_theme);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn launch_session(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> io::Result<()> {
    let cwd_path = std::path::Path::new(cwd)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(cwd));

    let mut cmd = std::process::Command::new("claude");

    if let Some(resume_id) = resume {
        cmd.arg("--resume").arg(resume_id);
    }

    if let Some(prompt_text) = prompt {
        cmd.arg("-p").arg(prompt_text);
    }

    cmd.current_dir(&cwd_path);

    match cmd.spawn() {
        Ok(child) => {
            println!(
                "Launched Claude session (PID {}) in {}",
                child.id(),
                cwd_path.display()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to launch claude: {e}");
            Err(e)
        }
    }
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
            session::SessionStatus::Idle => "\x1b[90m",
            session::SessionStatus::Finished => "\x1b[31m",
        };
        let reset = "\x1b[0m";

        println!(
            "=== {} ({}, {}, {status_color}{}{reset}) ===",
            s.display_name(),
            s.format_elapsed(),
            s.format_cost(),
            s.status,
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
            println!(
                "  Model: {} (context: {}%)",
                s.model,
                s.context_percent() as u32
            );
        }

        if s.subagent_count > 0 {
            println!("  Subagents: {}", s.subagent_count);
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

fn print_json() -> io::Result<()> {
    let app = App::new();
    let values: Vec<serde_json::Value> = app.sessions.iter().map(|s| s.to_json_value()).collect();
    let json = serde_json::to_string_pretty(&values).unwrap_or_else(|_| "[]".to_string());
    println!("{json}");
    Ok(())
}

fn print_list() -> io::Result<()> {
    let app = App::new();

    if app.sessions.is_empty() {
        println!("No active Claude sessions.");
        return Ok(());
    }

    println!(
        "{:<7} {:<16} {:<12} {:<8} {:<8} {:<9} {:<10} {:<6} {:<6} TOKENS",
        "PID", "PROJECT", "STATUS", "CTX%", "COST", "$/HR", "ELAPSED", "CPU%", "MEM"
    );
    println!("{}", "-".repeat(105));

    for s in &app.sessions {
        println!(
            "{:<7} {:<16} {:<12} {:<8} {:<8} {:<9} {:<10} {:<6.1} {:<6} {}",
            s.pid,
            s.display_name(),
            s.status.to_string(),
            s.format_context(),
            s.format_cost(),
            s.format_burn_rate(),
            s.format_elapsed(),
            s.cpu_percent,
            s.format_mem(),
            s.format_tokens(),
        );
    }

    let total_cost: f64 = app.sessions.iter().map(|s| s.cost_usd).sum();
    println!("{}", "-".repeat(105));
    println!("Total cost: ${total_cost:.2}");

    Ok(())
}

fn run_watch(tick_rate: Duration, json_mode: bool, format_str: &str) -> io::Result<()> {
    use crate::session::SessionStatus;
    use std::collections::HashMap;

    let mut app = App::new();
    let mut prev_statuses: HashMap<u32, SessionStatus> =
        app.sessions.iter().map(|s| (s.pid, s.status)).collect();

    // Print initial state for all sessions
    for s in &app.sessions {
        if json_mode {
            let obj = serde_json::json!({
                "event": "initial",
                "pid": s.pid,
                "project": s.display_name(),
                "status": s.status.to_string(),
                "cost_usd": (s.cost_usd * 100.0).round() / 100.0,
                "context_pct": (s.context_percent() * 100.0).round() / 100.0,
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

        for s in &app.sessions {
            let prev = prev_statuses.get(&s.pid).copied();
            let changed = prev.is_none_or(|p| p != s.status);

            if !changed {
                continue;
            }

            if json_mode {
                let obj = serde_json::json!({
                    "event": "status_change",
                    "pid": s.pid,
                    "project": s.display_name(),
                    "old_status": prev.map(|p| p.to_string()).unwrap_or_default(),
                    "new_status": s.status.to_string(),
                    "cost_usd": (s.cost_usd * 100.0).round() / 100.0,
                    "context_pct": (s.context_percent() * 100.0).round() / 100.0,
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
    fmt.replace("{pid}", &s.pid.to_string())
        .replace("{project}", s.display_name())
        .replace("{status}", &s.status.to_string())
        .replace("{cost}", &format!("{:.2}", s.cost_usd))
        .replace("{context}", &format!("{}", s.context_percent() as u32))
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    tick_rate: Duration,
    cfg: &config::Config,
    app_theme: theme::Theme,
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
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| {
            ui::table::render(frame, frame.area(), &app);
        })?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if !app.handle_key(key) {
                    return Ok(());
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.tick();
            last_tick = Instant::now();
        }
    }
}

//! CLI subcommand handlers extracted from main.rs.
//!
//! Each function implements a standalone CLI mode (--doctor, --clean, --list, etc.)
//! called from `run_main()` dispatch in main.rs.

use std::io;
use std::time::Duration;

use crate::Cli;
use crate::ViewFilters;
use crate::app::{App, FocusFilter, StatusFilter};
use crate::brain;
use crate::config;
use crate::demo;
use crate::discovery;
use crate::launch;
use crate::process;
use crate::rules;
use crate::session;

pub(crate) fn launch_session(
    cwd: &str,
    prompt: Option<&str>,
    resume: Option<&str>,
) -> io::Result<()> {
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

pub(crate) fn print_doctor() -> io::Result<()> {
    use crate::terminals;

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

pub(crate) fn check_brain_endpoint(endpoint: &str, timeout_ms: u64) -> bool {
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

pub(crate) fn parse_duration_str(s: &str) -> Duration {
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

pub(crate) fn parse_status_filter(value: Option<&str>) -> io::Result<StatusFilter> {
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

pub(crate) fn parse_focus_filter(value: Option<&str>) -> io::Result<FocusFilter> {
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

pub(crate) fn apply_filters(app: &mut App, filters: &ViewFilters) {
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

pub(crate) fn run_clean(
    older_than: Option<&str>,
    finished_only: bool,
    dry_run: bool,
) -> io::Result<()> {
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

pub(crate) fn print_summary(since: &str) -> io::Result<()> {
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

pub(crate) fn format_count(n: u64) -> String {
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

pub(crate) fn print_json(demo: bool, filters: &ViewFilters) -> io::Result<()> {
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

pub(crate) fn print_list(demo: bool, filters: &ViewFilters) -> io::Result<()> {
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

pub(crate) fn run_watch(
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

pub(crate) fn format_session(fmt: &str, s: &session::ClaudeSession) -> String {
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

/// Path to the brain gate mode state file.
pub(crate) fn brain_gate_mode_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home)
        .join(".claudectl")
        .join("brain")
        .join("gate-mode")
}

/// Read the current brain gate mode from disk. Returns "on" if no file exists.
pub(crate) fn read_brain_gate_mode() -> String {
    let path = brain_gate_mode_path();
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "on".into())
}

/// Set the brain gate mode (on/off/auto) and print confirmation.
pub(crate) fn run_brain_mode(mode: &str) -> io::Result<()> {
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
pub(crate) fn run_insights(cfg: &config::Config, cli: &Cli, arg: &str) -> io::Result<()> {
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
pub(crate) fn run_brain_query(cfg: &config::Config, cli: &Cli) -> io::Result<()> {
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

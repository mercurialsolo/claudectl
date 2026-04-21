use crate::session::ClaudeSession;

/// Fire a webhook POST with session status change payload.
/// Runs in a background thread to avoid blocking the TUI loop.
pub(crate) fn fire_webhook(url: &str, session: &ClaudeSession, old_status: String) {
    let payload = serde_json::json!({
        "event": "status_change",
        "session": {
            "pid": session.pid,
            "project": session.display_name(),
            "old_status": old_status,
            "new_status": session.status.to_string(),
            "telemetry": session.telemetry_label(),
            "cost_usd": if session.has_usage_metrics() { serde_json::json!((session.cost_usd * 100.0).round() / 100.0) } else { serde_json::Value::Null },
            "context_pct": if session.has_usage_metrics() { serde_json::json!((session.context_percent() * 100.0).round() / 100.0) } else { serde_json::Value::Null },
            "elapsed_secs": session.elapsed.as_secs(),
            "estimate_verified": !session.cost_estimate_unverified,
            "profile_source": session.model_profile_source,
        },
        "timestamp": chrono_now_iso(),
    });

    let body = serde_json::to_string(&payload).unwrap_or_default();
    let url = url.to_string();

    // Non-blocking: spawn a thread to POST
    std::thread::spawn(move || {
        let _ = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
                "--max-time",
                "5",
                &url,
            ])
            .output();
    });
}

/// Simple ISO-8601 timestamp without pulling in the chrono crate.
pub(crate) fn chrono_now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple ISO-8601 without pulling in chrono crate
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Approximate date calculation (doesn't handle leap years perfectly but good enough for timestamps)
    let mut y = 1970;
    let mut remaining_days = days_since_epoch;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for &md in &month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        m += 1;
    }
    let d = remaining_days + 1;
    m += 1;

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Fire a desktop notification (macOS via osascript, Linux via notify-send).
pub(crate) fn fire_notification(project: &str) {
    let safe = project.replace('"', "'").replace('\\', "");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!("display notification \"{safe} needs input\" with title \"claudectl\""),
        ])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("notify-send")
        .args(["claudectl", &format!("{safe} needs input")])
        .spawn();
}

/// Resolve the user's home directory, falling back to /tmp.
pub(crate) fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
}

/// Kill a process by PID. Tries SIGTERM first, then SIGKILL on failure.
pub(crate) fn kill_process(pid: u32) -> Result<(), String> {
    let output = std::process::Command::new("kill")
        .arg(pid.to_string())
        .output()
        .map_err(|e| format!("Failed to run kill: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let output = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .map_err(|e| format!("Failed to run kill -9: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Create a synthetic session for aggregate budget hook firing.
/// Uses {project} = "daily"/"weekly", {cost} = total spend.
pub(crate) fn create_aggregate_session(total_cost: f64, limit: f64, period: &str) -> ClaudeSession {
    use crate::session::RawSession;
    let raw = RawSession {
        pid: 0,
        session_id: format!("{period}-budget"),
        cwd: String::new(),
        started_at: 0,
    };
    let mut s = ClaudeSession::from_raw(raw);
    s.project_name = format!("{period}-budget");
    s.cost_usd = total_cost;
    s.model = format!("limit=${limit:.2}");
    s
}

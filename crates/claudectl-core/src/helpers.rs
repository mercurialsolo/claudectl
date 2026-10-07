use crate::session::ClaudeSession;

/// Fire a webhook POST with session status change payload.
/// Runs in a background thread to avoid blocking the TUI loop.
pub fn fire_webhook(url: &str, session: &ClaudeSession, old_status: String) {
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
pub fn chrono_now_iso() -> String {
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
/// `message` is shown verbatim as the notification body — callers pass the full
/// text (e.g. "myproject needs input", "myproject budget at 80%").
pub fn fire_notification(message: &str) {
    let safe = message.replace('"', "'").replace('\\', "");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!("display notification \"{safe}\" with title \"claudectl\""),
        ])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("notify-send")
        .args(["claudectl", &safe])
        .spawn();
}

/// Resolve the user's home directory, falling back to /tmp.
pub fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
}

/// Kill a process by PID. Tries SIGTERM first, then SIGKILL on failure.
pub fn kill_process(pid: u32) -> Result<(), String> {
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
pub fn create_aggregate_session(total_cost: f64, limit: f64, period: &str) -> ClaudeSession {
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

/// Fit a value into a fixed-width table column, with an ellipsis when it has to
/// be cut. Counts characters, so a multi-byte name can't split mid-codepoint.
pub fn truncate_cell(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let keep = width.saturating_sub(1);
    format!("{}…", value.chars().take(keep).collect::<String>())
}

/// True when a listener's bind address is reachable from off the machine.
///
/// `0.0.0.0` and `::` are *unspecified*, not loopback, so a plain
/// `is_loopback()` check correctly flags them along with any specific LAN
/// address. Callers use this to warn at startup when a plaintext or
/// unauthenticated HTTP surface has been exposed to the network (#426).
pub fn is_exposed_bind(addr: &std::net::SocketAddr) -> bool {
    !addr.ip().is_loopback()
}

/// Render an epoch-millisecond timestamp as a coarse relative span.
///
/// `"just now"`, `"5m ago"`, `"in 30d"`. Rounds to the nearest unit rather than
/// truncating, so a grant issued with `--expires 30d` does not read back as
/// `in 29d` a millisecond later.
///
/// Lives in core because two feature-gated callers need it — `access` (behind
/// `relay`) and `hive` (not) — and a display helper duplicated across a feature
/// boundary is a display helper that drifts.
pub fn fmt_ms_at(ms: u64, now_ms: u64) -> String {
    let (delta_ms, future) = if ms >= now_ms {
        (ms - now_ms, true)
    } else {
        (now_ms - ms, false)
    };
    let secs = delta_ms / 1000;
    // Round to nearest unit rather than truncating.
    let nearest = |unit: u64| (secs + unit / 2) / unit;
    let span = match secs {
        0..=59 => return "just now".into(),
        60..=3599 => format!("{}m", nearest(60)),
        3600..=86_399 => format!("{}h", nearest(3600)),
        _ => format!("{}d", nearest(86_400)),
    };
    if future {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

#[cfg(test)]
mod exposed_bind_tests {
    use super::is_exposed_bind;
    use std::net::SocketAddr;

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("test address should parse")
    }

    #[test]
    fn loopback_is_not_exposed() {
        assert!(!is_exposed_bind(&addr("127.0.0.1:9876")));
        assert!(!is_exposed_bind(&addr("[::1]:9876")));
    }

    #[test]
    fn unspecified_addresses_are_exposed() {
        // The case that matters: 0.0.0.0 binds every interface but is not
        // loopback, so it must be flagged.
        assert!(is_exposed_bind(&addr("0.0.0.0:9876")));
        assert!(is_exposed_bind(&addr("[::]:9876")));
    }

    #[test]
    fn specific_lan_address_is_exposed() {
        assert!(is_exposed_bind(&addr("192.168.1.5:9876")));
    }
}

#[cfg(test)]
mod truncate_tests {
    use super::truncate_cell;

    #[test]
    fn short_values_pass_through() {
        assert_eq!(truncate_cell("claudectl", 16), "claudectl");
    }

    #[test]
    fn long_values_are_ellipsized_to_width() {
        let out = truncate_cell("[mac-mini-9f2a1b] nightly-bench", 16);
        assert_eq!(out.chars().count(), 16);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn multibyte_values_do_not_split_codepoints() {
        let out = truncate_cell("日本語プロジェクト名前", 5);
        assert_eq!(out.chars().count(), 5);
    }
}

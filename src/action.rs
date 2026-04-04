use crate::session::ClaudeSession;

/// Switch to the terminal tab/pane running the given session.
pub fn switch_to_terminal(session: &ClaudeSession) -> Result<(), String> {
    if session.tty.is_empty() {
        return Err("No TTY associated with this session".into());
    }

    let terminal = detect_terminal();

    match terminal {
        Terminal::Ghostty => switch_ghostty(session),
        Terminal::Warp => switch_warp(session),
        Terminal::ITerm2 => switch_iterm2(session),
        Terminal::Kitty => switch_kitty(session),
        Terminal::WezTerm => switch_wezterm(session),
        Terminal::TerminalApp => switch_terminal_app(session),
        Terminal::Tmux => switch_tmux(session),
        Terminal::Unknown(name) => Err(format!("Unsupported terminal: {name}. Supported: Ghostty, Warp, iTerm2, Kitty, WezTerm, Terminal.app, tmux")),
    }
}

/// Public search term accessor for --list debug output.
pub fn warp_search_term(session: &ClaudeSession) -> String {
    build_warp_search_term(session)
}

enum Terminal {
    Ghostty,
    Warp,
    ITerm2,
    Kitty,
    WezTerm,
    TerminalApp,
    Tmux,
    Unknown(String),
}

fn detect_terminal() -> Terminal {
    // Check tmux first — it runs inside another terminal
    if std::env::var("TMUX").is_ok() {
        return Terminal::Tmux;
    }

    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("ghostty") => Terminal::Ghostty,
        Ok("WarpTerminal") => Terminal::Warp,
        Ok("iTerm.app") => Terminal::ITerm2,
        Ok("kitty") => Terminal::Kitty,
        Ok("WezTerm") => Terminal::WezTerm,
        Ok("Apple_Terminal") => Terminal::TerminalApp,
        Ok(other) => Terminal::Unknown(other.to_string()),
        Err(_) => Terminal::Unknown("unknown".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Ghostty — native AppleScript with working directory matching
// ---------------------------------------------------------------------------

fn switch_ghostty(session: &ClaudeSession) -> Result<(), String> {
    // Ghostty exposes `every terminal whose working directory contains X`
    // and `focus` to switch to it. Best terminal support we have.
    let cwd = &session.cwd;
    let tty = &session.tty;

    let script = format!(
        r#"
        tell application "Ghostty"
            -- Try matching by working directory first
            set matches to every terminal whose working directory contains "{cwd}"

            -- Find the one matching our TTY if multiple matches
            repeat with t in matches
                if tty of t contains "{tty}" then
                    focus t
                    activate
                    return "ok"
                end if
            end repeat

            -- Fallback: focus first match by cwd
            if (count of matches) > 0 then
                focus (item 1 of matches)
                activate
                return "ok"
            end if

            error "Session not found in Ghostty"
        end tell
        "#,
        cwd = cwd.replace('"', "\\\""),
        tty = tty,
    );

    run_osascript(&script)
}

// ---------------------------------------------------------------------------
// Warp — Navigation Palette search + split pane cycling
// ---------------------------------------------------------------------------

fn switch_warp(session: &ClaudeSession) -> Result<(), String> {
    let search = build_warp_search_term(session);

    let script = format!(
        r#"
        tell application "System Events"
            tell process "stable"
                keystroke "P" using {{command down, shift down}}
                delay 0.15
                keystroke "{search}"
                delay 0.3
                key code 36
            end tell
        end tell
        "#,
        search = search.replace('"', "\\\"")
    );
    run_osascript(&script)?;

    // Cycle split panes to find the Claude session pane.
    // Warp title shows "Claude Code" for claude panes.
    run_osascript(r#"
        tell application "System Events"
            tell process "stable"
                set winTitle to name of window 1
                if winTitle contains "Claude" then return "ok"
                repeat 6 times
                    keystroke "]" using {command down}
                    delay 0.05
                    set winTitle to name of window 1
                    if winTitle contains "Claude" then return "ok"
                end repeat
            end tell
        end tell
    "#)?;

    Ok(())
}

fn build_warp_search_term(session: &ClaudeSession) -> String {
    // Warp's palette treats `-` as negation and `/` as special.
    // Use resume UUID hex prefix when available (unique, no special chars).
    if session.command_args.contains("--resume") {
        if let Some(id_start) = session.command_args.find("--resume ") {
            let after = &session.command_args[id_start + 9..];
            let hex_prefix: String = after.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if hex_prefix.len() >= 6 {
                return hex_prefix;
            }
            let name: String = after.chars().take_while(|c| *c != ' ' && *c != '-').collect();
            if !name.is_empty() {
                return name;
            }
        }
    }

    // Fallback: project name truncated at first dash
    match session.project_name.find('-') {
        Some(pos) => session.project_name[..pos].to_string(),
        None => session.project_name.clone(),
    }
}

// ---------------------------------------------------------------------------
// iTerm2 — AppleScript with TTY matching
// ---------------------------------------------------------------------------

fn switch_iterm2(session: &ClaudeSession) -> Result<(), String> {
    let script = format!(
        r#"
        tell application "iTerm2"
            repeat with w in windows
                repeat with t in tabs of w
                    repeat with s in sessions of t
                        if tty of s contains "{tty}" then
                            select t
                            set index of w to 1
                            activate
                            return "ok"
                        end if
                    end repeat
                end repeat
            end repeat
            error "TTY not found in iTerm2"
        end tell
        "#,
        tty = session.tty
    );
    run_osascript(&script)
}

// ---------------------------------------------------------------------------
// Kitty — remote control protocol
// ---------------------------------------------------------------------------

fn switch_kitty(session: &ClaudeSession) -> Result<(), String> {
    // Kitty has a powerful remote control protocol via `kitty @ focus-window`.
    // Requires `allow_remote_control yes` or `allow_remote_control socket-only` in kitty.conf.
    // Match by the PID of the foreground process in the window.
    let pid = session.pid.to_string();

    // First try matching by the foreground process PID
    let output = std::process::Command::new("kitty")
        .args(["@", "focus-window", "--match", &format!("pid:{pid}")])
        .output();

    match output {
        Ok(o) if o.status.success() => return Ok(()),
        _ => {}
    }

    // Fallback: match by cwd
    let output = std::process::Command::new("kitty")
        .args(["@", "focus-window", "--match", &format!("cwd:{}", session.cwd)])
        .output()
        .map_err(|e| format!("kitty @ failed: {e}. Is allow_remote_control enabled?"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("Kitty: {}", stderr.trim()))
    }
}

// ---------------------------------------------------------------------------
// WezTerm — CLI with pane selection
// ---------------------------------------------------------------------------

fn switch_wezterm(session: &ClaudeSession) -> Result<(), String> {
    // WezTerm has `wezterm cli list` and `wezterm cli activate-pane`.
    // `wezterm cli list --format json` shows all panes with their cwd and tty.
    let output = std::process::Command::new("wezterm")
        .args(["cli", "list", "--format", "json"])
        .output()
        .map_err(|e| format!("wezterm cli failed: {e}"))?;

    if !output.status.success() {
        return Err("wezterm cli list failed".into());
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse wezterm output: {e}"))?;

    // Find pane matching our cwd or tty
    if let Some(panes) = json.as_array() {
        for pane in panes {
            let pane_cwd = pane.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
            let pane_tty = pane.get("tty_name").and_then(|v| v.as_str()).unwrap_or("");

            if pane_cwd.contains(&session.project_name) || pane_tty.contains(&session.tty) {
                if let Some(pane_id) = pane.get("pane_id").and_then(|v| v.as_u64()) {
                    let result = std::process::Command::new("wezterm")
                        .args(["cli", "activate-pane", "--pane-id", &pane_id.to_string()])
                        .output()
                        .map_err(|e| format!("wezterm activate-pane failed: {e}"))?;

                    if result.status.success() {
                        return Ok(());
                    }
                }
            }
        }
    }

    Err("Session not found in WezTerm pane list".into())
}

// ---------------------------------------------------------------------------
// Terminal.app — AppleScript with TTY matching
// ---------------------------------------------------------------------------

fn switch_terminal_app(session: &ClaudeSession) -> Result<(), String> {
    let script = format!(
        r#"
        tell application "Terminal"
            repeat with w in windows
                repeat with t in tabs of w
                    if tty of t contains "{tty}" then
                        set selected tab of w to t
                        set index of w to 1
                        activate
                        return "ok"
                    end if
                end repeat
            end repeat
            error "TTY not found in Terminal.app"
        end tell
        "#,
        tty = session.tty
    );
    run_osascript(&script)
}

// ---------------------------------------------------------------------------
// tmux — select-window + select-pane by TTY
// ---------------------------------------------------------------------------

fn switch_tmux(session: &ClaudeSession) -> Result<(), String> {
    // tmux can list panes with their TTY: `tmux list-panes -a -F '#{pane_tty} #{session_name}:#{window_index}.#{pane_index}'`
    let output = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_tty} #{session_name}:#{window_index}.#{pane_index}"])
        .output()
        .map_err(|e| format!("tmux list-panes failed: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() == 2 && parts[0].contains(&session.tty) {
            let target = parts[1]; // e.g. "main:2.1"
            // Select the tmux window+pane
            let _ = std::process::Command::new("tmux")
                .args(["select-window", "-t", target])
                .output();
            let _ = std::process::Command::new("tmux")
                .args(["select-pane", "-t", target])
                .output();
            return Ok(());
        }
    }

    Err(format!("TTY {} not found in tmux panes", session.tty))
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

fn run_osascript(script: &str) -> Result<(), String> {
    let output = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .map_err(|e| format!("Failed to run osascript: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("AppleScript error: {}", stderr.trim()))
    }
}

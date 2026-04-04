use crate::session::ClaudeSession;

/// Switch to the terminal tab/pane running the given session.
pub fn switch_to_terminal(session: &ClaudeSession) -> Result<(), String> {
    if session.tty.is_empty() {
        return Err("No TTY associated with this session".into());
    }

    let terminal = detect_terminal();

    match terminal {
        Terminal::Warp => switch_warp(session),
        Terminal::ITerm2 => switch_iterm2(session),
        Terminal::TerminalApp => switch_terminal_app(session),
        Terminal::Unknown(name) => Err(format!("Unsupported terminal: {name}")),
    }
}

enum Terminal {
    Warp,
    ITerm2,
    TerminalApp,
    Unknown(String),
}

fn detect_terminal() -> Terminal {
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("WarpTerminal") => Terminal::Warp,
        Ok("iTerm.app") => Terminal::ITerm2,
        Ok("Apple_Terminal") => Terminal::TerminalApp,
        Ok(other) => Terminal::Unknown(other.to_string()),
        Err(_) => Terminal::Unknown("unknown".to_string()),
    }
}

/// Switch Warp tab+pane using the Navigation Palette (Shift+Cmd+P).
/// 1. Opens palette, types search term, selects first match (switches tab)
/// 2. Cycles split panes with Cmd+] until window title contains our project name
fn switch_warp(session: &ClaudeSession) -> Result<(), String> {
    let search = build_warp_search_term(session);

    // Step 1: Navigate to the right tab via palette search.
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

    // Step 2: Ensure we're on the pane running claude (not a shell split).
    // All done in a single osascript call to avoid per-cycle process spawn overhead.
    // Warp title shows "Claude Code" for claude panes, directory path for shells.
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

/// Build a search string for Warp's Navigation Palette.
/// Public for debug output in --list mode.
pub fn warp_search_term(session: &ClaudeSession) -> String {
    build_warp_search_term(session)
}

fn build_warp_search_term(session: &ClaudeSession) -> String {
    // Warp's palette treats `-` as negation and `/` as special.
    // Priority:
    // 1. Resume UUID (first 8 hex chars — unique, no special chars)
    // 2. Resume name (truncated at first dash)
    // 3. Project name (truncated at first dash)

    // Check for --resume UUID in command args (e.g. "1f896750-aa3c-...")
    if session.command_args.contains("--resume") {
        if let Some(id_start) = session.command_args.find("--resume ") {
            let after = &session.command_args[id_start + 9..];
            // Take chars until first dash — that's the first hex segment of UUID
            let hex_prefix: String = after.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if hex_prefix.len() >= 6 {
                return hex_prefix;
            }
            // Named resume (not a UUID) — use name truncated at dash
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

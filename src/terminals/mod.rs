mod apple;
mod ghostty;
mod iterm2;
mod kitty;
mod tmux;
mod warp;
mod wezterm;

use crate::session::ClaudeSession;

pub enum Terminal {
    Ghostty,
    Warp,
    ITerm2,
    Kitty,
    WezTerm,
    Apple,
    Tmux,
    Unknown(String),
}

pub fn detect_terminal() -> Terminal {
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
        Ok("Apple_Terminal") => Terminal::Apple,
        Ok(other) => Terminal::Unknown(other.to_string()),
        Err(_) => Terminal::Unknown("unknown".to_string()),
    }
}

/// Switch to the terminal tab/pane running the given session.
pub fn switch_to_terminal(session: &ClaudeSession) -> Result<(), String> {
    if session.tty.is_empty() {
        return Err("No TTY associated with this session".into());
    }

    match detect_terminal() {
        Terminal::Ghostty => ghostty::switch(session),
        Terminal::Warp => warp::switch(session),
        Terminal::ITerm2 => iterm2::switch(session),
        Terminal::Kitty => kitty::switch(session),
        Terminal::WezTerm => wezterm::switch(session),
        Terminal::Apple => apple::switch(session),
        Terminal::Tmux => tmux::switch(session),
        Terminal::Unknown(name) => Err(format!(
            "Unsupported terminal: {name}. Supported: Ghostty, Warp, iTerm2, Kitty, WezTerm, Terminal.app, tmux"
        )),
    }
}

/// Send text to a session by switching to its terminal and typing via System Events.
/// Writing to /dev/ttysXXX goes to display output, not process input.
/// The only reliable way is to use the terminal emulator's input mechanism.
pub fn send_input(session: &ClaudeSession, text: &str) -> Result<(), String> {
    match detect_terminal() {
        Terminal::Ghostty => ghostty::send_input(session, text),
        Terminal::Kitty => kitty::send_input(session, text),
        Terminal::Tmux => tmux::send_input(session, text),
        _ => {
            // Warp, iTerm2, Terminal.app, WezTerm: switch to the tab, send keystroke, switch back
            switch_to_terminal(session)?;
            std::thread::sleep(std::time::Duration::from_millis(200));

            let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                r#"
                tell application "System Events"
                    tell process "stable"
                        keystroke "{text}"
                    end tell
                end tell
                "#,
                text = escaped,
            );
            run_osascript(&script)
        }
    }
}

/// Approve a pending permission prompt by sending Enter.
/// Claude Code's permission dialog has "1. Yes" pre-selected — Enter approves it.
pub fn approve_session(session: &ClaudeSession) -> Result<(), String> {
    match detect_terminal() {
        Terminal::Ghostty => ghostty::approve(session),
        Terminal::Kitty => kitty::approve(session),
        Terminal::Tmux => send_input(session, "Enter"),
        _ => {
            // Warp, iTerm2, Terminal.app: switch to tab, press Enter, switch back
            switch_to_terminal(session)?;
            std::thread::sleep(std::time::Duration::from_millis(200));
            run_osascript(
                r#"
                tell application "System Events"
                    tell process "stable"
                        key code 36
                    end tell
                end tell
            "#,
            )
        }
    }
}

pub fn run_osascript(script: &str) -> Result<(), String> {
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

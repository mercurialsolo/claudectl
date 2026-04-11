#[cfg(target_os = "macos")]
mod apple;
#[cfg(target_os = "macos")]
mod ghostty;
#[cfg(target_os = "macos")]
mod iterm2;
mod kitty;
mod tmux;
#[cfg(target_os = "macos")]
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

fn terminal_name(t: &Terminal) -> &str {
    match t {
        Terminal::Ghostty => "Ghostty",
        Terminal::Warp => "Warp",
        Terminal::ITerm2 => "iTerm2",
        Terminal::Kitty => "Kitty",
        Terminal::WezTerm => "WezTerm",
        Terminal::Apple => "Apple Terminal",
        Terminal::Tmux => "tmux",
        Terminal::Unknown(name) => name,
    }
}

pub fn detect_terminal() -> Terminal {
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

/// The macOS process name System Events uses to target each terminal.
#[cfg(target_os = "macos")]
fn system_events_process_name() -> &'static str {
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("WarpTerminal") => "stable",
        Ok("iTerm.app") => "iTerm2",
        Ok("Apple_Terminal") => "Terminal",
        Ok("WezTerm") => "WezTerm",
        Ok("ghostty") => "ghostty",
        _ => "frontmost application",
    }
}

pub fn switch_to_terminal(session: &ClaudeSession) -> Result<(), String> {
    if session.tty.is_empty() {
        return Err("No TTY associated with this session".into());
    }

    let terminal = detect_terminal();
    crate::logger::log(
        "DEBUG",
        &format!(
            "terminal switch: {} (tty={}) via {:?}",
            session.display_name(),
            session.tty,
            terminal_name(&terminal)
        ),
    );

    match terminal {
        Terminal::Kitty => kitty::switch(session),
        Terminal::WezTerm => wezterm::switch(session),
        Terminal::Tmux => tmux::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => ghostty::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::Warp => warp::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::ITerm2 => iterm2::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::Apple => apple::switch(session),
        Terminal::Unknown(name) => Err(format!(
            "Unsupported terminal: {name}. Supported: Ghostty, Warp, iTerm2, Kitty, WezTerm, Terminal.app, tmux"
        )),
        #[cfg(not(target_os = "macos"))]
        _ => Err("Terminal switching not supported on this platform".into()),
    }
}

pub fn send_input(session: &ClaudeSession, text: &str) -> Result<(), String> {
    match detect_terminal() {
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => ghostty::send_input(session, text),
        Terminal::Kitty => kitty::send_input(session, text),
        Terminal::Tmux => tmux::send_input(session, text),
        #[cfg(target_os = "macos")]
        _ => {
            // Switch to the tab first, then type via System Events
            switch_to_terminal(session)?;
            std::thread::sleep(std::time::Duration::from_millis(200));

            let proc = system_events_process_name();
            let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                r#"
                tell application "System Events"
                    tell process "{proc}"
                        keystroke "{escaped}"
                    end tell
                end tell
                "#,
            );
            run_osascript(&script)
        }
        #[cfg(not(target_os = "macos"))]
        _ => Err("Input injection not supported on this platform".into()),
    }
}

pub fn approve_session(session: &ClaudeSession) -> Result<(), String> {
    match detect_terminal() {
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => ghostty::approve(session),
        Terminal::Kitty => kitty::approve(session),
        Terminal::Tmux => send_input(session, "Enter"),
        #[cfg(target_os = "macos")]
        _ => {
            switch_to_terminal(session)?;
            std::thread::sleep(std::time::Duration::from_millis(200));

            let proc = system_events_process_name();
            let script = format!(
                r#"
                tell application "System Events"
                    tell process "{proc}"
                        key code 36
                    end tell
                end tell
                "#,
            );
            run_osascript(&script)
        }
        #[cfg(not(target_os = "macos"))]
        _ => Err("Input injection not supported on this platform".into()),
    }
}

#[cfg(target_os = "macos")]
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

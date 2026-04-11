use super::run_osascript;
use crate::session::ClaudeSession;

pub fn switch(session: &ClaudeSession) -> Result<(), String> {
    let cwd = session.cwd.replace('"', "\\\"");
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
    );

    run_osascript(&script)
}

pub fn send_input(session: &ClaudeSession, text: &str) -> Result<(), String> {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    let cwd = session.cwd.replace('"', "\\\"");

    let script = format!(
        r#"
        tell application "Ghostty"
            set matches to every terminal whose working directory contains "{cwd}"
            if (count of matches) > 0 then
                set t to item 1 of matches
                input text "{escaped}" to t
            end if
        end tell
        "#,
    );
    run_osascript(&script)
}

pub fn approve(session: &ClaudeSession) -> Result<(), String> {
    let cwd = session.cwd.replace('"', "\\\"");

    let script = format!(
        r#"
        tell application "Ghostty"
            set matches to every terminal whose working directory contains "{cwd}"
            if (count of matches) > 0 then
                set t to item 1 of matches
                send key "enter" to t
            end if
        end tell
        "#,
    );
    run_osascript(&script)
}

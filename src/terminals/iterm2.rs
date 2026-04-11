use super::run_osascript;
use crate::session::ClaudeSession;

pub fn switch(session: &ClaudeSession) -> Result<(), String> {
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

use super::run_osascript;
use crate::session::ClaudeSession;

pub fn switch(session: &ClaudeSession) -> Result<(), String> {
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

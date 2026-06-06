use super::run_osascript;
use crate::session::ClaudeSession;

/// Escape a string for embedding inside an AppleScript double-quoted literal.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Find the best matching Ghostty terminal for a session.
/// Ghostty's AppleScript API exposes: id, name, working directory (no tty/pid).
/// Strategy: match by CWD, disambiguate by session name in terminal title.
fn find_terminal_script(session: &ClaudeSession) -> String {
    let cwd = applescript_escape(&session.cwd);
    let session_name = applescript_escape(&session.session_name);

    // If we have a session name, try to match it against the terminal title first.
    // Claude Code sets the terminal title to "<spinner> <task_description>" which
    // often contains the session name (from --name or --resume flags).
    if session_name.is_empty() {
        // No session name — match by CWD only, take first match
        format!(
            r#"
            set matches to every terminal whose working directory contains "{cwd}"
            if (count of matches) = 0 then error "No Ghostty terminal found for {cwd}"
            set t to item 1 of matches
            "#,
        )
    } else {
        // Try CWD + name match first, fall back to CWD-only
        format!(
            r#"
            set matches to every terminal whose working directory contains "{cwd}"
            if (count of matches) = 0 then error "No Ghostty terminal found for {cwd}"

            -- Disambiguate: find the terminal whose title contains our session name
            set t to item 1 of matches
            repeat with candidate in matches
                if name of candidate contains "{session_name}" then
                    set t to candidate
                    exit repeat
                end if
            end repeat
            "#,
        )
    }
}

pub fn switch(session: &ClaudeSession) -> Result<(), String> {
    let find = find_terminal_script(session);

    let script = format!(
        r#"
        tell application "Ghostty"
            {find}
            focus t
            activate
        end tell
        "#,
    );

    run_osascript(&script)
}

pub fn send_input(session: &ClaudeSession, text: &str) -> Result<(), String> {
    let find = find_terminal_script(session);

    // Strip trailing newline — we append AppleScript `return` instead so the
    // newline is a proper CR rather than a literal embedded in the string.
    let trimmed = text.trim_end_matches('\n').trim_end_matches('\r');
    let escaped = applescript_escape(trimmed);
    let has_trailing_newline = text.ends_with('\n') || text.ends_with('\r');

    let text_expr = if has_trailing_newline {
        format!("\"{escaped}\" & return")
    } else {
        format!("\"{escaped}\"")
    };

    let script = format!(
        r#"
        tell application "Ghostty"
            {find}
            input text {text_expr} to t
        end tell
        "#,
    );
    run_osascript(&script)
}

pub fn approve(session: &ClaudeSession) -> Result<(), String> {
    let find = find_terminal_script(session);

    let script = format!(
        r#"
        tell application "Ghostty"
            {find}
            send key "enter" to t
        end tell
        "#,
    );
    run_osascript(&script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ClaudeSession, RawSession};

    fn make_session(cwd: &str, name: &str) -> ClaudeSession {
        let raw = RawSession {
            pid: 100,
            session_id: "test".into(),
            cwd: cwd.into(),
            started_at: 0,
        };
        let mut s = ClaudeSession::from_raw(raw);
        s.session_name = name.into();
        s
    }

    #[test]
    fn find_script_unnamed_session() {
        let s = make_session("/tmp/my-project", "");
        let script = find_terminal_script(&s);
        assert!(script.contains("working directory contains \"/tmp/my-project\""));
        // Should NOT have name-matching logic
        assert!(!script.contains("name of candidate"));
    }

    #[test]
    fn find_script_named_session() {
        let s = make_session("/tmp/my-project", "my-task");
        let script = find_terminal_script(&s);
        assert!(script.contains("working directory contains \"/tmp/my-project\""));
        assert!(script.contains("name of candidate contains \"my-task\""));
        // Should set fallback before loop
        assert!(script.contains("set t to item 1 of matches"));
    }

    #[test]
    fn find_script_escapes_quotes() {
        let s = make_session("/tmp/project \"alpha\"", "task \"beta\"");
        let script = find_terminal_script(&s);
        assert!(script.contains("project \\\"alpha\\\""));
        assert!(script.contains("task \\\"beta\\\""));
    }

    #[test]
    fn find_script_escapes_backslashes() {
        let s = make_session("/tmp/path\\with\\slashes", "name\\here");
        let script = find_terminal_script(&s);
        assert!(script.contains("path\\\\with\\\\slashes"));
        assert!(script.contains("name\\\\here"));
    }

    #[test]
    fn applescript_escape_handles_both() {
        assert_eq!(applescript_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn send_input_trailing_newline_uses_return() {
        let s = make_session("/tmp/proj", "");
        // We can't call send_input directly (it runs osascript), but we can
        // verify the text processing logic by checking the escaping.
        let text = "continue\n";
        let trimmed = text.trim_end_matches('\n').trim_end_matches('\r');
        let escaped = applescript_escape(trimmed);
        let has_trailing = text.ends_with('\n') || text.ends_with('\r');
        assert_eq!(trimmed, "continue");
        assert_eq!(escaped, "continue");
        assert!(has_trailing);
        // The expression should use & return
        let expr = if has_trailing {
            format!("\"{escaped}\" & return")
        } else {
            format!("\"{escaped}\"")
        };
        assert_eq!(expr, "\"continue\" & return");
        let _ = s; // suppress unused
    }

    #[test]
    fn send_input_no_trailing_newline() {
        let text = "some text";
        let trimmed = text.trim_end_matches('\n').trim_end_matches('\r');
        let has_trailing = text.ends_with('\n') || text.ends_with('\r');
        assert_eq!(trimmed, "some text");
        assert!(!has_trailing);
    }
}

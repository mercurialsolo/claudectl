use super::run_osascript;
use crate::session::ClaudeSession;

/// Escape a string for embedding inside an AppleScript double-quoted literal.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Find the best matching Ghostty terminal for a session.
/// Ghostty's AppleScript API exposes: id, name, working directory (no tty/pid).
///
/// Matching priority:
///   1. session.terminal_id — present when the agent-sandbox wrapper captured
///      the host Ghostty tab's AppleScript id at launch time. Unambiguous.
///   2. CWD match + title-contains-session-name disambiguator — fallback for
///      host-native sessions and for sandbox sessions launched before the
///      wrapper could probe Ghostty. Breaks down when multiple unnamed claudes
///      share a CWD.
fn find_terminal_script(session: &ClaudeSession) -> String {
    if let Some(ref id) = session.terminal_id {
        let escaped = applescript_escape(id);
        return format!(
            r#"
            set matches to every terminal whose id is "{escaped}"
            if (count of matches) = 0 then error "No Ghostty terminal with id {escaped}"
            set t to item 1 of matches
            "#,
        );
    }
    let cwd = applescript_escape(&session.cwd);
    let session_name = applescript_escape(&session.session_name);

    // Match on working directory, preferring an EXACT match before a substring
    // one. Exact-first is what stops a shallow cwd (e.g. the home directory)
    // from matching every surface nested under it — `... contains "/Users/x"`
    // matches `/Users/x`, `/Users/x/repo`, … and would focus an arbitrary one.
    // The `contains` fallback preserves the old behavior when Ghostty reports a
    // path that doesn't byte-match the session's recorded cwd (symlink
    // normalization like /tmp -> /private/tmp, or a trailing slash).
    if session_name.is_empty() {
        // No session name — match by CWD only, take the first match.
        format!(
            r#"
            set matches to every terminal whose working directory is "{cwd}"
            if (count of matches) = 0 then
                set matches to every terminal whose working directory contains "{cwd}"
            end if
            if (count of matches) = 0 then error "No Ghostty terminal found for {cwd}"
            set t to item 1 of matches
            "#,
        )
    } else {
        // CWD match, then disambiguate by title. Claude Code sets the terminal
        // title to "<spinner> <task_description>", which often contains the
        // session name (from --name or --resume), so prefer a title match when
        // several surfaces share the CWD.
        format!(
            r#"
            set matches to every terminal whose working directory is "{cwd}"
            if (count of matches) = 0 then
                set matches to every terminal whose working directory contains "{cwd}"
            end if
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
            name: None,
        };
        let mut s = ClaudeSession::from_raw(raw);
        s.session_name = name.into();
        s
    }

    #[test]
    fn find_script_unnamed_session() {
        let s = make_session("/tmp/my-project", "");
        let script = find_terminal_script(&s);
        // Exact-first, with a substring fallback.
        assert!(script.contains("working directory is \"/tmp/my-project\""));
        assert!(script.contains("working directory contains \"/tmp/my-project\""));
        // Should NOT have name-matching logic
        assert!(!script.contains("name of candidate"));
    }

    #[test]
    fn find_script_named_session() {
        let s = make_session("/tmp/my-project", "my-task");
        let script = find_terminal_script(&s);
        // Exact-first, with a substring fallback.
        assert!(script.contains("working directory is \"/tmp/my-project\""));
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

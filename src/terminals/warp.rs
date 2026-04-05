use crate::session::ClaudeSession;
use super::run_osascript;

pub fn switch(session: &ClaudeSession) -> Result<(), String> {
    let search = build_search_term(session);

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
    run_osascript(
        r#"
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
    "#,
    )?;

    Ok(())
}

pub fn build_search_term(session: &ClaudeSession) -> String {
    // Warp's palette treats `-` as negation and `/` as special.
    // Use resume UUID hex prefix when available (unique, no special chars).
    if session.command_args.contains("--resume") {
        if let Some(id_start) = session.command_args.find("--resume ") {
            let after = &session.command_args[id_start + 9..];
            let hex_prefix: String = after.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if hex_prefix.len() >= 6 {
                return hex_prefix;
            }
            let name: String = after
                .chars()
                .take_while(|c| *c != ' ' && *c != '-')
                .collect();
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

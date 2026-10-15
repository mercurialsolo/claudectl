use std::collections::HashMap;

use crate::session::ClaudeSession;

pub fn launch(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> Result<String, String> {
    let mut parts = vec!["claude".to_string()];
    parts.extend(
        super::build_claude_args(prompt, resume)
            .into_iter()
            .map(|arg| super::shell_escape(&arg)),
    );
    let command = parts.join(" ");

    let output = std::process::Command::new("tmux")
        .args(["new-window", "-c", cwd, &command])
        .output()
        .map_err(|e| format!("tmux new-window failed: {e}"))?;

    if output.status.success() {
        Ok("tmux window".into())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn switch(session: &ClaudeSession) -> Result<(), String> {
    // tmux can list panes with their TTY: `tmux list-panes -a -F '#{pane_tty} #{session_name}:#{window_index}.#{pane_index}'`
    let output = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_tty} #{session_name}:#{window_index}.#{pane_index}",
        ])
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

pub fn send_input(session: &ClaudeSession, text: &str) -> Result<(), String> {
    let output = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_tty} #{session_name}:#{window_index}.#{pane_index}",
        ])
        .output()
        .map_err(|e| format!("tmux failed: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() == 2 && parts[0].contains(&session.tty) {
            let _ = std::process::Command::new("tmux")
                .args(["send-keys", "-t", parts[1], text, ""])
                .output();
            return Ok(());
        }
    }

    Err("TTY not found in tmux".into())
}

/// Where a `tmux` binary is likely to live when PATH doesn't say. `relay serve`
/// runs under launchd, whose PATH is `/usr/bin:/bin:/usr/sbin:/sbin`, so a bare
/// `tmux` misses Homebrew's copy there even though it works in a shell.
const TMUX_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/tmux",
    "/usr/local/bin/tmux",
    "/usr/bin/tmux",
];

fn tmux_binary() -> String {
    TMUX_CANDIDATES
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map_or_else(|| "tmux".to_string(), |p| (*p).to_string())
}

/// Every tmux pane on this machine, keyed by its TTY as `ps` reports it
/// (`ttys002`, not `/dev/ttys002`), mapped to a `session:window.pane` target.
/// Empty when tmux isn't installed or no server is running. One `tmux` spawn
/// covers every session, so callers resolve a whole collection at once.
pub fn pane_targets() -> HashMap<String, String> {
    let Ok(output) = std::process::Command::new(tmux_binary())
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_tty} #{session_name}:#{window_index}.#{pane_index}",
        ])
        .output()
    else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    parse_pane_targets(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `list-panes -F '#{pane_tty} <target>'` output. Session names may
/// contain spaces, so only the first space splits.
pub fn parse_pane_targets(stdout: &str) -> HashMap<String, String> {
    stdout
        .lines()
        .filter_map(|line| {
            let (tty, target) = line.split_once(' ')?;
            let tty = tty.trim().trim_start_matches("/dev/");
            let target = target.trim();
            (!tty.is_empty() && !target.is_empty()).then(|| (tty.to_string(), target.to_string()))
        })
        .collect()
}

/// The argv that attaches to `target` on another machine: ssh with a TTY, a
/// login shell so Homebrew's PATH is there, then tmux selecting the pane.
/// `attach-session` without `-d`, so the remote machine's own client stays
/// attached — `-d` would pull the session off that machine's screen.
pub fn remote_attach_argv(destination: &str, target: &str) -> Vec<String> {
    let quoted = super::shell_escape(target);
    let tmux = format!(
        "tmux select-window -t {quoted} \\; select-pane -t {quoted} \\; attach-session -t {quoted}"
    );
    vec![
        "ssh".to_string(),
        "-t".to_string(),
        destination.to_string(),
        format!("exec \"$SHELL\" -lc {}", super::shell_escape(&tmux)),
    ]
}

/// Open `argv` in a new window of the tmux server this process runs inside.
pub fn open_in_new_window(name: &str, argv: &[String]) -> Result<(), String> {
    let command = argv
        .iter()
        .map(|a| super::shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ");
    let output = std::process::Command::new("tmux")
        .args(["new-window", "-n", name, &command])
        .output()
        .map_err(|e| format!("tmux new-window failed: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_panes_keyed_by_bare_tty() {
        let map =
            parse_pane_targets("/dev/ttys002 claude-staffai:0.0\n/dev/ttys005 prreview:1.2\n");
        assert_eq!(
            map.get("ttys002").map(String::as_str),
            Some("claude-staffai:0.0")
        );
        assert_eq!(map.get("ttys005").map(String::as_str), Some("prreview:1.2"));
    }

    #[test]
    fn session_names_with_spaces_survive() {
        let map = parse_pane_targets("/dev/pts/3 my work:2.0\n");
        assert_eq!(map.get("pts/3").map(String::as_str), Some("my work:2.0"));
    }

    #[test]
    fn blank_and_malformed_lines_are_skipped() {
        assert!(parse_pane_targets("\nnospace\n").is_empty());
    }

    #[test]
    fn remote_attach_selects_the_pane_without_detaching_others() {
        let argv = remote_attach_argv("baradas@100.109.212.89", "claude-staffai:0.0");
        assert_eq!(&argv[..3], ["ssh", "-t", "baradas@100.109.212.89"]);
        let remote = &argv[3];
        assert!(remote.starts_with("exec \"$SHELL\" -lc "));
        assert!(remote.contains("select-window -t "));
        assert!(remote.contains("attach-session -t "));
        assert!(remote.contains("claude-staffai:0.0"));
        assert!(!remote.contains(" -d"));
    }
}

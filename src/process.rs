use crate::session::{ClaudeSession, SessionStatus};
use std::collections::{HashMap, HashSet};

/// Check which PIDs are alive and fetch TTY, CPU%, MEM, command args — all via `ps`.
/// No sysinfo dependency needed.
pub fn fetch_and_enrich(sessions: &mut [ClaudeSession]) {
    if sessions.is_empty() {
        return;
    }

    let pids: Vec<String> = sessions.iter().map(|s| s.pid.to_string()).collect();
    let pid_arg = pids.join(",");

    let output = std::process::Command::new("ps")
        .args(["-o", "pid=,tty=,%cpu=,rss=,command=", "-p", &pid_arg])
        .env_clear()
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            crate::logger::log("ERROR", &format!("ps command failed: {e}"));
            // ps failed — mark all as Finished (will show tombstone for 30s)
            for s in sessions.iter_mut() {
                s.status = SessionStatus::Finished;
                s.cpu_percent = 0.0;
            }
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Build a pid → session-index map once. Replaces the prior O(N²)
    // inner loop that scanned every session for every ps line.
    let pid_to_idx: HashMap<u32, usize> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| (s.pid, i))
        .collect();
    let mut alive_pids: HashSet<u32> = HashSet::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let Ok(pid) = fields[0].parse::<u32>() else {
            continue;
        };
        let tty = fields[1].to_string();
        let cpu = fields[2].parse::<f32>().unwrap_or(0.0);
        let rss_kb = fields[3].parse::<f64>().unwrap_or(0.0);
        let mem_mb = rss_kb / 1024.0;
        let command = fields[4..].join(" ");

        // Only count this PID as alive if it's actually a claude process.
        // PIDs get reused on macOS — a dead claude session's PID may belong
        // to an unrelated process now. Match on argv0 basename, not a raw
        // substring: `claudectl`, `bash -lc '... claude ...'`, and
        // `grep claude` would all match a substring check.
        if !is_claude_process(&command) {
            continue;
        }

        alive_pids.insert(pid);

        let Some(&idx) = pid_to_idx.get(&pid) else {
            continue;
        };
        let session = &mut sessions[idx];

        // ps tty is invariant per pid (set at exec time, never changes), so
        // overwriting every tick is wasted work and would also clobber the
        // host-tty override below. Set once when empty.
        if session.tty.is_empty() {
            session.tty = tty;
        }
        // The terminal sidecar is written exactly once at session start by
        // sandbox-bootstrap-inner and is invariant for the lifetime of the
        // pid; reading it every tick was 40+ syscalls + JSON parses for no
        // information gain. `sidecar_loaded` flips on the first attempt
        // (success or absence) so we only do the I/O once per session.
        if !session.sidecar_loaded {
            if let Some(s) = read_terminal_sidecar(pid) {
                if let Some(host_tty) = s.host_tty {
                    session.tty = host_tty;
                }
                session.terminal_id = s.terminal_id;
                session.host_terminal_target = s.host_terminal_target;
            }
            session.sidecar_loaded = true;
        }
        session.mem_mb = mem_mb;

        // CPU smoothing: track last 3 readings, use average
        session.cpu_history.push(cpu);
        if session.cpu_history.len() > 3 {
            session.cpu_history.remove(0);
        }
        session.cpu_percent =
            session.cpu_history.iter().sum::<f32>() / session.cpu_history.len() as f32;

        // Extract args (everything after "claude")
        if let Some(idx) = command.find("claude") {
            let after_claude = &command[idx + 6..];
            session.command_args = after_claude.trim().to_string();
        }

        // Extract session name from --name or --resume
        let cmd_parts: Vec<&str> = command.split_whitespace().collect();
        extract_session_meta(&cmd_parts, session);
    }

    // Mark dead PIDs as Finished instead of removing them immediately.
    // They'll be displayed briefly so the user can see what exited.
    for session in sessions.iter_mut() {
        if !alive_pids.contains(&session.pid) {
            session.status = crate::session::SessionStatus::Finished;
            session.cpu_percent = 0.0;
        }
    }
}

/// True iff the first whitespace-split token of `command` (i.e. argv0),
/// after stripping any leading path, is exactly `"claude"`. This excludes
/// `claudectl`, `grep claude`, and `bash -lc '... claude ...'`.
fn is_claude_process(command: &str) -> bool {
    let argv0 = command.split_whitespace().next().unwrap_or("");
    let basename = argv0.rsplit('/').next().unwrap_or(argv0);
    basename == "claude"
}

struct TerminalSidecar {
    host_tty: Option<String>,
    terminal_id: Option<String>,
    /// Per-host-terminal connection target (kitty socket+window id, tmux
    /// socket+pane, wezterm pane id+optional socket). Populated by the
    /// agent-sandbox wrappers when the host runs a Linux desktop terminal.
    /// Absent on macOS-host sandboxes (which use osa-bridge instead) and
    /// on host-native claudectl runs.
    host_terminal_target: Option<crate::session::HostTerminalTarget>,
}

/// Read the per-session terminal sidecar written by the agent sandbox's
/// bootstrap (see tools/agent-sandbox/sbx-template/sandbox-bootstrap-inner).
/// Returns the HOST-side TTY + terminal-application id if present.
///
/// The sidecar lives at $HOME/.claude/sessions/<pid>.terminal.json. Only the
/// agent sandbox writes it; for non-sandbox claude sessions (host-native) the
/// file is absent and this returns None — the regular `ps` TTY stands.
fn read_terminal_sidecar(pid: u32) -> Option<TerminalSidecar> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::PathBuf::from(home)
        .join(".claude")
        .join("sessions")
        .join(format!("{pid}.terminal.json"));
    let body = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let trim = |v: &serde_json::Value, key: &str| -> Option<String> {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let host_terminal_target = parse_host_terminal_target(&value, &trim);
    Some(TerminalSidecar {
        host_tty: trim(&value, "host_tty"),
        terminal_id: trim(&value, "terminal_id"),
        host_terminal_target,
    })
}

/// Pick the host-terminal target out of the sidecar JSON. The agent-sandbox
/// wrappers write whichever set of env vars the host terminal exported:
///   kitty   -> KITTY_WINDOW_ID + KITTY_LISTEN_ON
///   tmux    -> TMUX (socket,N,session-id) + TMUX_PANE
///   wezterm -> WEZTERM_PANE + (optional) WEZTERM_UNIX_SOCKET
/// We probe in that order. If multiple are present we trust kitty first
/// because it is the strongest single signal (a kitty window id is unique
/// even when nested in tmux).
fn parse_host_terminal_target(
    value: &serde_json::Value,
    trim: &dyn Fn(&serde_json::Value, &str) -> Option<String>,
) -> Option<crate::session::HostTerminalTarget> {
    // Sidecar JSON keys are lowercase (matches the existing host_tty /
    // terminal_id / terminal_type convention written by sandbox-bootstrap-inner).
    if let (Some(window_id), Some(listen_on)) = (
        trim(value, "kitty_window_id"),
        trim(value, "kitty_listen_on"),
    ) {
        return Some(crate::session::HostTerminalTarget::Kitty {
            socket: listen_on,
            window_id,
        });
    }
    if let (Some(tmux), Some(pane)) = (trim(value, "tmux"), trim(value, "tmux_pane")) {
        // $TMUX is "<socket>,<server-pid>,<session-id>"; we only need the
        // socket path (first field) for `tmux -S`. tmux(1) "ENVIRONMENT".
        let socket = tmux.split(',').next().unwrap_or(&tmux).to_string();
        return Some(crate::session::HostTerminalTarget::Tmux { socket, pane });
    }
    if let Some(pane_str) = trim(value, "wezterm_pane")
        && let Ok(pane_id) = pane_str.parse::<u64>()
    {
        return Some(crate::session::HostTerminalTarget::WezTerm {
            pane_id,
            unix_socket: trim(value, "wezterm_unix_socket"),
        });
    }
    None
}

fn extract_session_meta(cmd: &[&str], session: &mut ClaudeSession) {
    // If the session JSON already provided a name (via /rename or auto-name),
    // don't overwrite it from the process command line.
    let name_already_set = !session.session_name.is_empty();
    let mut i = 0;
    while i < cmd.len() {
        match cmd[i] {
            "--name" | "-n" if i + 1 < cmd.len() => {
                if !name_already_set {
                    session.session_name = cmd[i + 1].to_string();
                }
                i += 2;
                continue;
            }
            "--resume" | "-r" if i + 1 < cmd.len() => {
                let val = cmd[i + 1];
                if !name_already_set && !looks_like_uuid(val) {
                    session.session_name = val.to_string();
                }
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && s.matches('-').count() == 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_claude_process_matches_bare_argv0() {
        assert!(is_claude_process("claude --dangerously-skip-permissions"));
    }

    #[test]
    fn is_claude_process_matches_absolute_path() {
        assert!(is_claude_process("/usr/local/bin/claude --resume foo"));
    }

    #[test]
    fn is_claude_process_rejects_claudectl() {
        assert!(!is_claude_process("claudectl --list"));
    }

    #[test]
    fn is_claude_process_rejects_shell_wrapping() {
        assert!(!is_claude_process(
            "bash -lc 'exec sandbox-bootstrap claude --resume foo'"
        ));
    }

    #[test]
    fn is_claude_process_rejects_grep_claude() {
        assert!(!is_claude_process("grep claude"));
    }
}

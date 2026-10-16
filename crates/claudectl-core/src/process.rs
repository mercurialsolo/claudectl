use crate::session::{ClaudeSession, SessionStatus};

/// Check which PIDs are alive and fetch TTY, CPU%, MEM, command args — all via `ps`.
/// No sysinfo dependency needed.
pub fn fetch_and_enrich(sessions: &mut [ClaudeSession]) {
    // A session with no process of its own must never reach the pid paths
    // below (#524). A Codex session carries `pid: 0`, and `kill(0, 0)` asks
    // about the *caller's own process group* — so it would answer "alive"
    // forever, and `ps -p 0` returns nothing, which the busybox fallback
    // reads as "this pid is gone". Both answers are fiction. Filtering here
    // rather than at every caller keeps the rule in one place.
    //
    // Their status comes from their transcript instead, which is the only
    // signal they have; see `agent::codex`.
    let mut sessions: Vec<&mut ClaudeSession> =
        sessions.iter_mut().filter(|s| s.is_actionable()).collect();
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
            // `ps` being unavailable means liveness is *unknown*, not that
            // everything died. This used to mark every session `Finished`,
            // which was survivable only because `infer_status` immediately
            // overwrote it; now that the verdict is sticky (#473) it would
            // report a whole machine's sessions as dead.
            //
            // It is not hypothetical: busybox `ps` accepts neither `-o` nor
            // `-p`, so on Alpine and other minimal images — a normal place to
            // run an agent — this branch is the only one ever taken. Verified
            // in a container: `ps -o pid=,tty=,%cpu=,rss=,command= -p <pid>`
            // exits non-zero there while procps answers fine.
            //
            // `kill(pid, 0)` needs no external binary and answers the one
            // question that matters. CPU and memory stay unknown, which is
            // what they are.
            for s in sessions.iter_mut() {
                s.cpu_percent = 0.0;
                if pid_alive(s.pid) {
                    if s.status == SessionStatus::Finished {
                        s.status = SessionStatus::Unknown;
                    }
                } else {
                    s.status = SessionStatus::Finished;
                }
            }
            return;
        }
    };

    // busybox `ps` takes neither `-o` nor `-p`. It still *runs*, so this is an
    // `Ok(output)` with a non-zero status and empty stdout — indistinguishable
    // from "every one of those pids is gone" unless we ask separately. On
    // Alpine and other minimal images that is the only outcome there is, so
    // without this check every session on the machine reads as dead.
    if !output.status.success() && !ps_supports_query() {
        crate::logger::log(
            "WARN",
            "ps cannot answer -o/-p (busybox?); falling back to kill(pid, 0) for liveness",
        );
        for s in sessions.iter_mut() {
            s.cpu_percent = 0.0;
            if pid_alive(s.pid) {
                if s.status == SessionStatus::Finished {
                    s.status = SessionStatus::Unknown;
                }
            } else {
                s.status = SessionStatus::Finished;
            }
        }
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Collect alive PIDs from ps output
    let mut alive_pids = std::collections::HashSet::new();

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
        // to an unrelated process now.
        if !command.contains("claude") {
            continue;
        }

        alive_pids.insert(pid);

        for session in sessions.iter_mut() {
            if session.pid == pid {
                session.tty = tty.clone();
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

                break;
            }
        }
    }

    // Mark dead PIDs as Finished instead of removing them immediately.
    // They'll be displayed briefly so the user can see what exited.
    //
    // Liveness is re-decided from `ps` on every tick, in both directions. The
    // clearing half matters because `Finished` is now sticky — `infer_status`
    // will not overwrite it — so a session marked dead by a transient `ps`
    // failure above would otherwise stay dead for the life of the process.
    for session in sessions.iter_mut() {
        if alive_pids.contains(&session.pid) {
            if session.status == crate::session::SessionStatus::Finished {
                session.status = crate::session::SessionStatus::Unknown;
            }
        } else {
            session.status = crate::session::SessionStatus::Finished;
            session.cpu_percent = 0.0;
        }
    }
}

/// Can `ps` answer the query `fetch_and_enrich` needs?
///
/// A non-zero exit from `ps -p <pids>` is ambiguous: it means "no matching
/// process" on procps, and "I do not understand `-o` or `-p`" on busybox. Both
/// arrive as `Ok(output)` with empty stdout, so the exit status alone cannot
/// tell a dead session from a crippled `ps`.
///
/// pid 1 exists on every running Unix, so asking about it separates the two.
fn ps_supports_query() -> bool {
    std::process::Command::new("ps")
        .args(["-o", "pid=", "-p", "1"])
        .env_clear()
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Does this pid exist?
///
/// Public because it is the liveness primitive the rest of the tree needs
/// when `ps` is unavailable, and because it is worth testing directly.
///
/// `kill(pid, 0)` sends no signal and only checks reachability. `EPERM` means
/// the process exists but belongs to another user — alive, not dead — so the
/// errno is distinguished rather than testing `== 0`.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn extract_session_meta(cmd: &[&str], session: &mut ClaudeSession) {
    let mut i = 0;
    while i < cmd.len() {
        match cmd[i] {
            "--name" | "-n" if i + 1 < cmd.len() => {
                session.session_name = cmd[i + 1].to_string();
                i += 2;
                continue;
            }
            "--resume" | "-r" if i + 1 < cmd.len() => {
                let val = cmd[i + 1];
                if !looks_like_uuid(val) {
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
    use crate::session::RawSession;

    fn codex_session() -> ClaudeSession {
        ClaudeSession::from_codex_rollout(&crate::agent::codex::RolloutSummary {
            session_id: "codex-1".into(),
            cwd: "/tmp/project".into(),
            lifecycle: crate::agent::codex::Lifecycle::Working,
            ..Default::default()
        })
    }

    /// A Codex session must not reach the pid paths at all (#524).
    ///
    /// `pid` is 0 there, and both branches would lie about it: `ps -p 0`
    /// returns nothing, which the busybox fallback reads as "this pid is
    /// gone" and would mark the session `Finished`; `kill(0, 0)` asks about
    /// the caller's own process group and would answer "alive" forever.
    /// Its status comes from its transcript, so `fetch_and_enrich` has to
    /// leave it exactly as it found it.
    #[test]
    fn a_codex_session_status_is_not_touched() {
        let mut sessions = vec![codex_session()];
        let before = sessions[0].status;
        assert_eq!(before, SessionStatus::Processing);

        fetch_and_enrich(&mut sessions);

        assert_eq!(
            sessions[0].status, before,
            "a session with no process of its own must come back unchanged"
        );
    }

    /// The filter must not swallow the real work: a live Claude Code session
    /// alongside a Codex one still gets enriched.
    #[test]
    fn a_real_session_alongside_a_codex_one_is_still_enriched() {
        let mut sessions = vec![
            codex_session(),
            ClaudeSession::from_raw(RawSession {
                // This process. It is certainly alive.
                pid: std::process::id(),
                session_id: "live".into(),
                cwd: "/tmp/project".into(),
                started_at: 0,
            }),
        ];

        fetch_and_enrich(&mut sessions);

        assert_ne!(
            sessions[1].status,
            SessionStatus::Finished,
            "the test's own pid is alive, so it must not be reported dead"
        );
        assert_eq!(
            sessions[0].status,
            SessionStatus::Processing,
            "the Codex session must still be untouched"
        );
    }
}

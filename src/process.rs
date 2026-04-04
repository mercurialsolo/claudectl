use std::collections::HashMap;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::session::{ClaudeSession, SessionStatus};

pub struct ProcessMonitor {
    system: System,
}

impl ProcessMonitor {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory(),
        );
        Self { system }
    }

    pub fn refresh(&mut self) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory(),
        );
    }

    /// Enrich sessions with process data. Returns only sessions whose PID is alive.
    pub fn enrich(&self, sessions: &mut Vec<ClaudeSession>) {
        sessions.retain_mut(|session| {
            let pid = Pid::from_u32(session.pid);
            let Some(proc) = self.system.process(pid) else {
                session.status = SessionStatus::Finished;
                return false;
            };

            // sysinfo only used to check PID alive.
            // CPU, MEM, command args all come from ps (fetch_ps_data).
            true
        });
    }

    /// Get TTY, CPU%, MEM, and command args via ps.
    /// More reliable than sysinfo on macOS for CPU and command line.
    pub fn fetch_ps_data(&self, sessions: &mut [ClaudeSession]) {
        if sessions.is_empty() {
            return;
        }

        let pids: Vec<String> = sessions.iter().map(|s| s.pid.to_string()).collect();
        let pid_arg = pids.join(",");

        // Use `command` format which gives the full command with args.
        // Format: PID TTY %CPU RSS COMMAND...
        // We use `=` suffixes to suppress headers.
        let output = std::process::Command::new("ps")
            .args(["-o", "pid=,tty=,%cpu=,rss=,command=", "-p", &pid_arg])
            .env_clear()
            .output();

        let output = match output {
            Ok(o) => o,
            Err(_) => return,
        };

        let stdout = String::from_utf8_lossy(&output.stdout);

        for line in stdout.lines() {
            // Format: "43915 ttys004    7.3 163744 claude --resume ..."
            // First collect the 4 fixed fields, then everything remaining is the command.
            let trimmed = line.trim();
            let fields: Vec<&str> = trimmed.split_whitespace().collect();
            if fields.len() < 5 {
                continue;
            }
            let Ok(pid) = fields[0].parse::<u32>() else { continue };
            let tty = fields[1].to_string();
            let cpu = fields[2].parse::<f32>().unwrap_or(0.0);
            let rss_kb = fields[3].parse::<f64>().unwrap_or(0.0);
            let mem_mb = rss_kb / 1024.0;
            let command = fields[4..].join(" ");

            // Find the matching session and update it
            for session in sessions.iter_mut() {
                if session.pid == pid {
                    session.tty = tty.clone();
                    session.cpu_percent = cpu;
                    session.mem_mb = mem_mb;

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
    }
}

fn extract_session_meta(cmd: &[&str], session: &mut ClaudeSession) {
    let mut i = 0;
    while i < cmd.len() {
        match cmd[i] {
            "--name" | "-n" => {
                if i + 1 < cmd.len() {
                    session.session_name = cmd[i + 1].to_string();
                    i += 2;
                    continue;
                }
            }
            "--resume" | "-r" => {
                if i + 1 < cmd.len() {
                    let val = cmd[i + 1];
                    // If it doesn't look like a UUID, use as display name
                    if !looks_like_uuid(val) {
                        session.session_name = val.to_string();
                    }
                    i += 2;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
        && s.matches('-').count() == 4
}

use ratatui::widgets::TableState;

use crate::discovery;
use crate::monitor;
use crate::process::ProcessMonitor;
use crate::session::ClaudeSession;

pub struct App {
    pub sessions: Vec<ClaudeSession>,
    pub table_state: TableState,
    pub should_quit: bool,
    pub process_monitor: ProcessMonitor,
    pub status_msg: String,
    pub pending_kill: Option<u32>,
    pub input_mode: bool,
    pub input_buffer: String,
    pub input_target_pid: Option<u32>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            sessions: Vec::new(),
            table_state: TableState::default(),
            should_quit: false,
            process_monitor: ProcessMonitor::new(),
            status_msg: String::new(),
            pending_kill: None,
            input_mode: false,
            input_buffer: String::new(),
            input_target_pid: None,
        };
        app.refresh();
        // Select first row if sessions exist
        if !app.sessions.is_empty() {
            app.table_state.select(Some(0));
        }
        app
    }

    pub fn refresh(&mut self) {
        let mut sessions = discovery::scan_sessions();

        // Enrich with process data (also filters dead PIDs)
        self.process_monitor.refresh();
        self.process_monitor.enrich(&mut sessions);
        self.process_monitor.fetch_ps_data(&mut sessions);

        // Resolve JSONL paths AFTER ps data (needs command_args for --resume UUID)
        discovery::resolve_jsonl_paths(&mut sessions);

        // Carry forward previous costs for burn rate calculation
        let prev_costs: std::collections::HashMap<u32, f64> = self
            .sessions
            .iter()
            .map(|s| (s.pid, s.cost_usd))
            .collect();

        // Read JSONL for tokens + status
        for session in &mut sessions {
            monitor::update_tokens(session);

            // Compute burn rate: cost delta / time delta
            if let Some(&prev) = prev_costs.get(&session.pid) {
                session.prev_cost_usd = prev;
                let delta = session.cost_usd - prev;
                if delta > 0.0 {
                    // tick_rate is ~2s, extrapolate to $/hr
                    session.burn_rate_per_hr = delta * 1800.0; // delta per 2s * 1800 = per hour
                }
            }
        }

        // Sort: status priority, then elapsed desc
        sessions.sort_by(|a, b| {
            a.status
                .sort_key()
                .cmp(&b.status.sort_key())
                .then(b.elapsed.cmp(&a.elapsed))
        });

        self.sessions = sessions;

        // Fix selection bounds
        let len = self.sessions.len();
        if len == 0 {
            self.table_state.select(None);
        } else if let Some(sel) = self.table_state.selected() {
            if sel >= len {
                self.table_state.select(Some(len - 1));
            }
        }
    }

    pub fn tick(&mut self) {
        // Clear status message on tick
        self.status_msg.clear();

        // Re-read elapsed times
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for session in &mut self.sessions {
            let elapsed_ms = now_ms.saturating_sub(session.started_at);
            session.elapsed = std::time::Duration::from_millis(elapsed_ms);
        }

        self.refresh();
    }

    pub fn next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i >= self.sessions.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.sessions.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn selected_session(&self) -> Option<&ClaudeSession> {
        self.table_state
            .selected()
            .and_then(|i| self.sessions.get(i))
    }

    /// Handle `d` key press — first press sets pending, second confirms kill.
    pub fn handle_kill(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let pid = session.pid;
        let name = session.display_name().to_string();

        if self.pending_kill == Some(pid) {
            // Second press — kill it
            match kill_process(pid) {
                Ok(()) => {
                    self.status_msg = format!("Killed {name} (PID {pid})");
                    // Remove the session JSON
                    let session_file = dirs_home()
                        .join(".claude")
                        .join("sessions")
                        .join(format!("{pid}.json"));
                    let _ = std::fs::remove_file(session_file);
                    self.refresh();
                }
                Err(e) => {
                    self.status_msg = format!("Kill failed: {e}");
                }
            }
            self.pending_kill = None;
        } else {
            // First press — ask for confirmation
            self.pending_kill = Some(pid);
            self.status_msg = format!("Kill {name} (PID {pid})? Press d again to confirm");
        }
    }

    /// Cancel any pending kill on non-d key press.
    pub fn cancel_pending_kill(&mut self) {
        if self.pending_kill.is_some() {
            self.pending_kill = None;
            self.status_msg = "Kill cancelled".into();
        }
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
}

fn kill_process(pid: u32) -> Result<(), String> {
    // Send SIGTERM first
    let output = std::process::Command::new("kill")
        .arg(pid.to_string())
        .output()
        .map_err(|e| format!("Failed to run kill: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        // Try SIGKILL as fallback
        let output = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
            .map_err(|e| format!("Failed to run kill -9: {e}"))?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(stderr.trim().to_string())
        }
    }
}

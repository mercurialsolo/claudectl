use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use crate::discovery;
use crate::monitor;
use crate::process;
use crate::session::{ClaudeSession, SessionStatus};
use crate::terminals;

pub const SORT_COLUMNS: &[&str] = &["Status", "Context", "Cost", "$/hr", "Elapsed"];

pub struct App {
    pub sessions: Vec<ClaudeSession>,
    pub table_state: TableState,
    pub should_quit: bool,
    pub status_msg: String,
    pub pending_kill: Option<u32>,
    pub input_mode: bool,
    pub input_buffer: String,
    pub input_target_pid: Option<u32>,
    pub notify: bool,
    pub prev_statuses: HashMap<u32, SessionStatus>,
    pub show_help: bool,
    pub sort_column: usize,
    pub auto_approve: HashSet<u32>,
    pub pending_auto_approve: Option<u32>,
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
            status_msg: String::new(),
            pending_kill: None,
            input_mode: false,
            input_buffer: String::new(),
            input_target_pid: None,
            notify: false,
            prev_statuses: HashMap::new(),
            show_help: false,
            sort_column: 0,
            auto_approve: HashSet::new(),
            pending_auto_approve: None,
        };
        app.refresh();
        if !app.sessions.is_empty() {
            app.table_state.select(Some(0));
        }
        app
    }

    pub fn refresh(&mut self) {
        // Discover which PIDs have session files
        let discovered = discovery::scan_sessions();

        // Build a map of existing sessions by PID for state preservation
        let mut existing: HashMap<u32, ClaudeSession> = self
            .sessions
            .drain(..)
            .map(|s| (s.pid, s))
            .collect();

        // Merge: reuse existing session state (jsonl_offset, tokens, cost, cpu_history)
        // or create new from discovered
        let mut sessions: Vec<ClaudeSession> = discovered
            .into_iter()
            .map(|new| {
                if let Some(mut prev) = existing.remove(&new.pid) {
                    // Preserve accumulated state, update ephemeral fields
                    prev.elapsed = new.elapsed;
                    prev.started_at = new.started_at;
                    // cwd/project_name/session_id don't change
                    prev
                } else {
                    // Brand new session
                    new
                }
            })
            .collect();

        // Enrich with ps data (CPU, MEM, TTY, command args) + filter dead PIDs
        process::fetch_and_enrich(&mut sessions);

        // Resolve JSONL paths (only for sessions that don't have one yet)
        for session in &mut sessions {
            if session.jsonl_path.is_none() {
                discovery::resolve_jsonl_paths(std::slice::from_mut(session));
            }
        }

        // Scan for subagents
        discovery::scan_subagents(&mut sessions);

        // Snapshot previous cost for burn rate BEFORE reading new JSONL data
        for session in &mut sessions {
            session.prev_cost_usd = session.cost_usd;
        }

        // Read JSONL incrementally (only new bytes since last offset)
        for session in &mut sessions {
            monitor::update_tokens(session);
        }

        // Compute burn rate from cost delta (skip first tick where prev_cost is 0)
        for session in &mut sessions {
            if session.prev_cost_usd > 0.001 {
                let delta = session.cost_usd - session.prev_cost_usd;
                if delta > 0.001 {
                    session.burn_rate_per_hr = delta * 1800.0;
                } else {
                    // Decay burn rate toward zero when no new cost
                    session.burn_rate_per_hr *= 0.5;
                    if session.burn_rate_per_hr < 0.01 {
                        session.burn_rate_per_hr = 0.0;
                    }
                }
            }
        }

        // Sort
        self.apply_sort(&mut sessions);

        // Notifications: check for NeedsInput transitions
        if self.notify {
            for session in &sessions {
                let prev = self.prev_statuses.get(&session.pid).copied();
                if session.status == SessionStatus::NeedsInput
                    && prev != Some(SessionStatus::NeedsInput)
                    && prev.is_some()
                {
                    fire_notification(&session.project_name);
                }
            }
        }

        // Update prev_statuses
        self.prev_statuses = sessions.iter().map(|s| (s.pid, s.status)).collect();

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

    fn apply_sort(&self, sessions: &mut [ClaudeSession]) {
        match self.sort_column {
            0 => sessions.sort_by(|a, b| {
                a.status
                    .sort_key()
                    .cmp(&b.status.sort_key())
                    .then(b.elapsed.cmp(&a.elapsed))
            }),
            1 => sessions.sort_by(|a, b| {
                b.context_percent()
                    .partial_cmp(&a.context_percent())
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            2 => sessions.sort_by(|a, b| {
                b.cost_usd
                    .partial_cmp(&a.cost_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            3 => sessions.sort_by(|a, b| {
                b.burn_rate_per_hr
                    .partial_cmp(&a.burn_rate_per_hr)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            4 => sessions.sort_by(|a, b| b.elapsed.cmp(&a.elapsed)),
            _ => {}
        }
    }

    pub fn cycle_sort(&mut self) {
        self.sort_column = (self.sort_column + 1) % SORT_COLUMNS.len();
        self.status_msg = format!("Sort: {}", SORT_COLUMNS[self.sort_column]);
        let mut sessions = std::mem::take(&mut self.sessions);
        self.apply_sort(&mut sessions);
        self.sessions = sessions;
    }

    pub fn tick(&mut self) {
        self.status_msg.clear();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for session in &mut self.sessions {
            let elapsed_ms = now_ms.saturating_sub(session.started_at);
            session.elapsed = std::time::Duration::from_millis(elapsed_ms);
        }

        self.refresh();
        self.run_auto_approve();
    }

    fn run_auto_approve(&mut self) {
        let pids_to_approve: Vec<u32> = self
            .sessions
            .iter()
            .filter(|s| s.status == SessionStatus::NeedsInput && self.auto_approve.contains(&s.pid))
            .map(|s| s.pid)
            .collect();

        for pid in pids_to_approve {
            if let Some(session) = self.sessions.iter().find(|s| s.pid == pid) {
                match terminals::approve_session(session) {
                    Ok(()) => self.status_msg = format!("Auto-approved {}", session.display_name()),
                    Err(e) => self.status_msg = format!("Auto-approve error: {e}"),
                }
            }
        }
    }

    pub fn handle_auto_approve(&mut self) {
        let Some(session) = self.selected_session() else { return };
        let pid = session.pid;
        let name = session.display_name().to_string();

        if self.pending_auto_approve == Some(pid) {
            if self.auto_approve.contains(&pid) {
                self.auto_approve.remove(&pid);
                self.status_msg = format!("Auto-approve OFF for {name}");
            } else {
                self.auto_approve.insert(pid);
                self.status_msg = format!("Auto-approve ON for {name}");
            }
            self.pending_auto_approve = None;
        } else {
            self.pending_auto_approve = Some(pid);
            let action = if self.auto_approve.contains(&pid) { "disable" } else { "enable" };
            self.status_msg = format!("Press a again to {action} auto-approve for {name}");
        }
    }

    pub fn cancel_pending_auto_approve(&mut self) {
        self.pending_auto_approve = None;
    }

    pub fn next(&mut self) {
        if self.sessions.is_empty() { return }
        let i = match self.table_state.selected() {
            Some(i) if i >= self.sessions.len() - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        if self.sessions.is_empty() { return }
        let i = match self.table_state.selected() {
            Some(0) => self.sessions.len() - 1,
            Some(i) => i - 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn selected_session(&self) -> Option<&ClaudeSession> {
        self.table_state.selected().and_then(|i| self.sessions.get(i))
    }

    pub fn handle_kill(&mut self) {
        let Some(session) = self.selected_session() else { return };
        let pid = session.pid;
        let name = session.display_name().to_string();

        if self.pending_kill == Some(pid) {
            match kill_process(pid) {
                Ok(()) => {
                    self.status_msg = format!("Killed {name} (PID {pid})");
                    let session_file = dirs_home()
                        .join(".claude/sessions")
                        .join(format!("{pid}.json"));
                    let _ = std::fs::remove_file(session_file);
                    self.auto_approve.remove(&pid);
                    self.refresh();
                }
                Err(e) => self.status_msg = format!("Kill failed: {e}"),
            }
            self.pending_kill = None;
        } else {
            self.pending_kill = Some(pid);
            self.status_msg = format!("Kill {name} (PID {pid})? Press d again to confirm");
        }
    }

    pub fn cancel_pending_kill(&mut self) {
        if self.pending_kill.is_some() {
            self.pending_kill = None;
            self.status_msg = "Kill cancelled".into();
        }
    }

    /// Handle a key event. Returns false if the application should quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Help overlay: any key dismisses
        if self.show_help {
            self.show_help = false;
            return true;
        }

        // Input mode: capture text for sending to a session
        if self.input_mode {
            self.handle_input_key(key);
            return true;
        }

        // Normal mode
        self.handle_normal_key(key);
        !self.should_quit
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if let Some(pid) = self.input_target_pid {
                    if let Some(session) = self.sessions.iter().find(|s| s.pid == pid) {
                        let text = format!("{}\n", self.input_buffer);
                        match terminals::send_input(session, &text) {
                            Ok(()) => {
                                self.status_msg =
                                    format!("Sent to {}", session.display_name())
                            }
                            Err(e) => self.status_msg = format!("Error: {e}"),
                        }
                    }
                }
                self.input_mode = false;
                self.input_buffer.clear();
                self.input_target_pid = None;
            }
            KeyCode::Esc => {
                self.input_mode = false;
                self.input_buffer.clear();
                self.input_target_pid = None;
                self.status_msg = "Input cancelled".into();
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
            }
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                self.should_quit = true;
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.next();
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.previous();
            }
            (KeyCode::Char('r'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.refresh();
            }
            (KeyCode::Char('d'), _) | (KeyCode::Char('x'), _) => {
                self.cancel_pending_auto_approve();
                self.handle_kill();
            }
            (KeyCode::Char('y'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_approve();
            }
            (KeyCode::Char('i'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_input_mode();
            }
            (KeyCode::Char('?'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.show_help = !self.show_help;
            }
            (KeyCode::Char('s'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.cycle_sort();
            }
            (KeyCode::Char('a'), _) => {
                self.cancel_pending_kill();
                self.handle_auto_approve();
            }
            (KeyCode::Tab, _) | (KeyCode::Enter, _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_switch_terminal();
            }
            _ => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
            }
        }
    }

    fn handle_approve(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.status == SessionStatus::NeedsInput {
                match terminals::approve_session(session) {
                    Ok(()) => self.status_msg = format!("Approved {}", session.display_name()),
                    Err(e) => self.status_msg = format!("Error: {e}"),
                }
            } else {
                self.status_msg = "Session is not waiting for input".into();
            }
        }
    }

    fn enter_input_mode(&mut self) {
        let info = self
            .selected_session()
            .map(|s| (s.pid, s.display_name().to_string()));
        if let Some((pid, name)) = info {
            self.input_mode = true;
            self.input_buffer.clear();
            self.input_target_pid = Some(pid);
            self.status_msg = format!("Input to {name} (Enter to send, Esc to cancel): ");
        }
    }

    fn handle_switch_terminal(&mut self) {
        if let Some(session) = self.selected_session() {
            match terminals::switch_to_terminal(session) {
                Ok(()) => {
                    self.status_msg = format!("Switched to {}", session.display_name());
                }
                Err(e) => {
                    self.status_msg = format!("Error: {e}");
                }
            }
        } else {
            self.status_msg = "No session selected".into();
        }
    }
}

fn fire_notification(project: &str) {
    let _ = std::process::Command::new("osascript")
        .args(["-e", &format!("display notification \"{project} needs input\" with title \"claudectl\"")])
        .spawn();
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
}

fn kill_process(pid: u32) -> Result<(), String> {
    let output = std::process::Command::new("kill")
        .arg(pid.to_string())
        .output()
        .map_err(|e| format!("Failed to run kill: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let output = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .map_err(|e| format!("Failed to run kill -9: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

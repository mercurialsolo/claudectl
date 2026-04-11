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
    pub finished_at: HashMap<u32, std::time::Instant>, // When PIDs were first seen as Finished
    pub debug: bool,
    pub debug_timings: DebugTimings,
    pub grouped_view: bool,
    pub detail_panel: bool, // Show expanded detail for selected session
    pub webhook_url: Option<String>,
    pub webhook_filter: Option<Vec<String>>, // Only fire on these status names
    pub launch_mode: bool,                   // Capturing directory path for new session
    pub launch_buffer: String,
    pub budget_usd: Option<f64>,     // Per-session budget
    pub kill_on_budget: bool,        // Auto-kill when budget exceeded
    pub budget_warned: HashSet<u32>, // PIDs that have been warned at 80%
    pub budget_killed: HashSet<u32>, // PIDs that have been killed
}

#[derive(Default, Clone)]
pub struct DebugTimings {
    pub scan_ms: f64,
    pub ps_ms: f64,
    pub jsonl_ms: f64,
    pub total_ms: f64,
    // Rolling averages (last 10 ticks)
    history: Vec<(f64, f64, f64, f64)>,
}

impl DebugTimings {
    pub fn record(&mut self, scan: f64, ps: f64, jsonl: f64, total: f64) {
        self.scan_ms = scan;
        self.ps_ms = ps;
        self.jsonl_ms = jsonl;
        self.total_ms = total;
        self.history.push((scan, ps, jsonl, total));
        if self.history.len() > 10 {
            self.history.remove(0);
        }
    }

    pub fn avg_total_ms(&self) -> f64 {
        if self.history.is_empty() {
            return 0.0;
        }
        self.history.iter().map(|h| h.3).sum::<f64>() / self.history.len() as f64
    }

    pub fn format(&self) -> String {
        format!(
            "tick: {:.1}ms (avg {:.1}ms) | scan: {:.1}ms | ps: {:.1}ms | jsonl: {:.1}ms",
            self.total_ms,
            self.avg_total_ms(),
            self.scan_ms,
            self.ps_ms,
            self.jsonl_ms,
        )
    }
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
            finished_at: HashMap::new(),
            debug: false,
            debug_timings: DebugTimings::default(),
            grouped_view: false,
            detail_panel: false,
            webhook_url: None,
            webhook_filter: None,
            launch_mode: false,
            launch_buffer: String::new(),
            budget_usd: None,
            kill_on_budget: false,
            budget_warned: HashSet::new(),
            budget_killed: HashSet::new(),
        };
        app.refresh();
        if !app.sessions.is_empty() {
            app.table_state.select(Some(0));
        }
        app
    }

    pub fn refresh(&mut self) {
        let tick_start = std::time::Instant::now();

        // Discover which PIDs have session files
        let scan_start = std::time::Instant::now();
        let discovered = discovery::scan_sessions();
        let scan_elapsed = scan_start.elapsed();

        // Build a map of existing sessions by PID for state preservation
        let mut existing: HashMap<u32, ClaudeSession> =
            self.sessions.drain(..).map(|s| (s.pid, s)).collect();

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
        let ps_start = std::time::Instant::now();
        process::fetch_and_enrich(&mut sessions);
        let ps_elapsed = ps_start.elapsed();

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
        let jsonl_start = std::time::Instant::now();
        for session in &mut sessions {
            monitor::update_tokens(session);
        }
        let jsonl_elapsed = jsonl_start.elapsed();

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

        // Budget enforcement
        if let Some(budget) = self.budget_usd {
            for session in &sessions {
                let pct = session.cost_usd / budget * 100.0;

                // Warn at 80%
                if (80.0..100.0).contains(&pct) && !self.budget_warned.contains(&session.pid) {
                    self.budget_warned.insert(session.pid);
                    self.status_msg = format!(
                        "BUDGET WARNING: {} at {:.0}% (${:.2}/${:.2})",
                        session.display_name(),
                        pct,
                        session.cost_usd,
                        budget
                    );
                    fire_notification(&format!("{} budget {:.0}%", session.display_name(), pct));
                }

                // Kill at 100%
                if pct >= 100.0 && !self.budget_killed.contains(&session.pid) {
                    self.budget_killed.insert(session.pid);
                    if self.kill_on_budget {
                        let _ = kill_process(session.pid);
                        self.status_msg = format!(
                            "BUDGET EXCEEDED: Killed {} (${:.2}/${:.2})",
                            session.display_name(),
                            session.cost_usd,
                            budget
                        );
                    } else {
                        self.status_msg = format!(
                            "BUDGET EXCEEDED: {} at ${:.2}/{:.2} — use --kill-on-budget to auto-kill",
                            session.display_name(),
                            session.cost_usd,
                            budget
                        );
                    }
                    fire_notification(&format!("{} exceeded budget!", session.display_name()));
                }
            }
        }

        // Record activity for sparkline
        for session in &mut sessions {
            session.record_activity();
        }

        // Track when sessions first appear as Finished, remove after 30s
        let now = std::time::Instant::now();
        for session in &sessions {
            if session.status == SessionStatus::Finished {
                self.finished_at.entry(session.pid).or_insert(now);
            }
        }
        sessions.retain(|s| {
            if s.status == SessionStatus::Finished {
                if let Some(&t) = self.finished_at.get(&s.pid) {
                    return now.duration_since(t).as_secs() < 30;
                }
            }
            true
        });
        // Clean up old finished_at entries + their session files
        let expired: Vec<u32> = self
            .finished_at
            .iter()
            .filter(|(_, t)| now.duration_since(**t).as_secs() >= 60)
            .map(|(pid, _)| *pid)
            .collect();
        for pid in &expired {
            let session_file = dirs_home()
                .join(".claude/sessions")
                .join(format!("{pid}.json"));
            let _ = std::fs::remove_file(session_file);
        }
        self.finished_at
            .retain(|_, t| now.duration_since(*t).as_secs() < 60);

        // Sort
        self.apply_sort(&mut sessions);

        // Notifications and webhooks: check for status transitions
        for session in &sessions {
            let prev = self.prev_statuses.get(&session.pid).copied();
            let changed = prev.is_some() && prev != Some(session.status);

            if !changed {
                continue;
            }

            // Desktop notification on NeedsInput
            if self.notify && session.status == SessionStatus::NeedsInput {
                fire_notification(&session.project_name);
            }

            // Webhook on status change
            if let Some(ref url) = self.webhook_url {
                let new_status = session.status.to_string();
                let should_fire = match &self.webhook_filter {
                    Some(filter) => filter.iter().any(|f| f.eq_ignore_ascii_case(&new_status)),
                    None => true,
                };
                if should_fire {
                    fire_webhook(
                        url,
                        session,
                        prev.map(|p| p.to_string()).unwrap_or_default(),
                    );
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

        // Record debug timings
        if self.debug {
            let total_elapsed = tick_start.elapsed();
            self.debug_timings.record(
                scan_elapsed.as_secs_f64() * 1000.0,
                ps_elapsed.as_secs_f64() * 1000.0,
                jsonl_elapsed.as_secs_f64() * 1000.0,
                total_elapsed.as_secs_f64() * 1000.0,
            );
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
        let Some(session) = self.selected_session() else {
            return;
        };
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
            let action = if self.auto_approve.contains(&pid) {
                "disable"
            } else {
                "enable"
            };
            self.status_msg = format!("Press a again to {action} auto-approve for {name}");
        }
    }

    pub fn cancel_pending_auto_approve(&mut self) {
        self.pending_auto_approve = None;
    }

    pub fn next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i >= self.sessions.len() - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(0) => self.sessions.len() - 1,
            Some(i) => i - 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn selected_session(&self) -> Option<&ClaudeSession> {
        self.table_state
            .selected()
            .and_then(|i| self.sessions.get(i))
    }

    pub fn handle_kill(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let pid = session.pid;
        let name = session.display_name().to_string();

        if self.pending_kill == Some(pid) {
            match kill_process(pid) {
                Ok(()) => {
                    self.status_msg = format!("Killed {name} (PID {pid})");
                    self.auto_approve.remove(&pid);
                    // Don't delete session file yet — let the Finished tombstone show for 30s.
                    // The file will be cleaned up when the tombstone expires.
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

        // Launch mode: capture directory for new session
        if self.launch_mode {
            self.handle_launch_key(key);
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
                                self.status_msg = format!("Sent to {}", session.display_name())
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
            (KeyCode::Char('n'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.launch_mode = true;
                self.launch_buffer.clear();
                self.status_msg =
                    "New session — enter directory path (Enter to launch, Esc to cancel): ".into();
            }
            (KeyCode::Char('g'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.grouped_view = !self.grouped_view;
                self.status_msg = if self.grouped_view {
                    "Grouped by project".into()
                } else {
                    "Flat view".into()
                };
            }
            (KeyCode::Enter, _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.detail_panel = !self.detail_panel;
            }
            (KeyCode::Tab, _) => {
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

    fn handle_launch_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let dir = if self.launch_buffer.is_empty() {
                    ".".to_string()
                } else {
                    self.launch_buffer.clone()
                };

                let cwd_path = std::path::Path::new(&dir)
                    .canonicalize()
                    .unwrap_or_else(|_| std::path::PathBuf::from(&dir));

                match std::process::Command::new("claude")
                    .current_dir(&cwd_path)
                    .spawn()
                {
                    Ok(child) => {
                        self.status_msg = format!(
                            "Launched session (PID {}) in {}",
                            child.id(),
                            cwd_path.display()
                        );
                    }
                    Err(e) => {
                        self.status_msg = format!("Launch failed: {e}");
                    }
                }

                self.launch_mode = false;
                self.launch_buffer.clear();
            }
            KeyCode::Esc => {
                self.launch_mode = false;
                self.launch_buffer.clear();
                self.status_msg = "Launch cancelled".into();
            }
            KeyCode::Backspace => {
                self.launch_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.launch_buffer.push(c);
            }
            _ => {}
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

#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub name: String,
    pub session_count: usize,
    pub active_count: usize,
    pub total_cost: f64,
    pub avg_context_pct: f64,
}

impl App {
    pub fn project_groups(&self) -> Vec<ProjectGroup> {
        let mut groups: HashMap<String, Vec<&ClaudeSession>> = HashMap::new();
        for s in &self.sessions {
            groups.entry(s.project_name.clone()).or_default().push(s);
        }

        let mut result: Vec<ProjectGroup> = groups
            .into_iter()
            .map(|(name, sessions)| {
                let active_count = sessions
                    .iter()
                    .filter(|s| {
                        matches!(
                            s.status,
                            SessionStatus::Processing | SessionStatus::NeedsInput
                        )
                    })
                    .count();
                let total_cost: f64 = sessions.iter().map(|s| s.cost_usd).sum();
                let avg_context_pct = if sessions.is_empty() {
                    0.0
                } else {
                    sessions.iter().map(|s| s.context_percent()).sum::<f64>()
                        / sessions.len() as f64
                };
                ProjectGroup {
                    name,
                    session_count: sessions.len(),
                    active_count,
                    total_cost,
                    avg_context_pct,
                }
            })
            .collect();

        result.sort_by(|a, b| {
            b.total_cost
                .partial_cmp(&a.total_cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        result
    }
}

fn fire_webhook(url: &str, session: &ClaudeSession, old_status: String) {
    let payload = serde_json::json!({
        "event": "status_change",
        "session": {
            "pid": session.pid,
            "project": session.display_name(),
            "old_status": old_status,
            "new_status": session.status.to_string(),
            "cost_usd": (session.cost_usd * 100.0).round() / 100.0,
            "context_pct": (session.context_percent() * 100.0).round() / 100.0,
            "elapsed_secs": session.elapsed.as_secs(),
        },
        "timestamp": chrono_now_iso(),
    });

    let body = serde_json::to_string(&payload).unwrap_or_default();
    let url = url.to_string();

    // Non-blocking: spawn a thread to POST
    std::thread::spawn(move || {
        let _ = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
                "--max-time",
                "5",
                &url,
            ])
            .output();
    });
}

fn chrono_now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple ISO-8601 without pulling in chrono crate
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Approximate date calculation (doesn't handle leap years perfectly but good enough for timestamps)
    let mut y = 1970;
    let mut remaining_days = days_since_epoch;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for &md in &month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        m += 1;
    }
    let d = remaining_days + 1;
    m += 1;

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn fire_notification(project: &str) {
    let safe = project.replace('"', "'").replace('\\', "");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!("display notification \"{safe} needs input\" with title \"claudectl\""),
        ])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("notify-send")
        .args(["claudectl", &format!("{safe} needs input")])
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

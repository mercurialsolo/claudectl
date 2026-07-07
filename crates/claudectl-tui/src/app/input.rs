//! Extracted from app/mod.rs — behavior-preserving split.
#![allow(clippy::too_many_lines)]

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl App {
    /// Keymap for the Supervisor panel: j/k navigate, r refresh, Esc/T/q close.
    /// Read-only for now (#368, increment 1).
    #[cfg(feature = "coord")]
    pub(super) fn handle_supervisor_key(&mut self, key: KeyEvent) {
        // Any key other than a second `c` disarms a pending cancel.
        let was_armed = self.supervisor_pending_cancel.take();
        match key.code {
            KeyCode::Esc | KeyCode::Char('T') | KeyCode::Char('q') => {
                self.show_supervisor = false;
                self.supervisor_status_msg = None;
            }
            KeyCode::Char('r') => {
                self.coord_refresh();
                self.supervisor_status_msg = Some("Refreshed.".into());
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let last = self.coord_tasks.len().saturating_sub(1);
                if self.supervisor_selected < last {
                    self.supervisor_selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.supervisor_selected > 0 {
                    self.supervisor_selected -= 1;
                }
            }
            KeyCode::Char('g') | KeyCode::Home => self.supervisor_selected = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.supervisor_selected = self.coord_tasks.len().saturating_sub(1);
            }
            // Cancel selected task — double-tap `c` to confirm (destructive).
            KeyCode::Char('c') => self.handle_supervisor_cancel(was_armed),
            // Re-queue a failed/cancelled task.
            KeyCode::Char('R') => self.handle_supervisor_retry(),
            // Approve a NEEDS_HUMAN task — accept it as DONE.
            KeyCode::Char('a') => self.handle_supervisor_approve(),
            // Toggle the supervisor drain marker.
            KeyCode::Char('d') => self.handle_supervisor_drain_toggle(),
            _ => {}
        }
    }

    /// Handle a key event. Returns false if the application should quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.last_user_interaction = std::time::Instant::now();

        // Transition out of idle mode on any key press
        if self.idle_mode_active {
            self.idle_mode_active = false;
            if !self.idle_report.is_empty() {
                let report = self.idle_report.join("; ");
                self.status_msg = format!("Idle report: {report}");
                self.idle_report.clear();
            }
            self.idle_tasks_launched.clear();
        }

        // Guided tour overlay (#373): space/enter/→ advance, ←/p back up,
        // Esc/q exit into the live demo. Never quits the app.
        if self.demo_tour.is_some() {
            match key.code {
                KeyCode::Char(' ')
                | KeyCode::Enter
                | KeyCode::Char('n')
                | KeyCode::Right
                | KeyCode::Down => {
                    let advanced = self
                        .demo_tour
                        .as_mut()
                        .map(|t| t.advance())
                        .unwrap_or(false);
                    if !advanced {
                        // Past the last step — drop into the live demo.
                        self.demo_tour = None;
                    }
                }
                KeyCode::Left | KeyCode::Up | KeyCode::Char('p') => {
                    if let Some(t) = self.demo_tour.as_mut() {
                        t.prev();
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.demo_tour = None;
                }
                _ => {}
            }
            return true;
        }

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

        if self.search_mode {
            self.handle_search_key(key);
            return true;
        }

        // Input mode: capture text for sending to a session
        if self.input_mode {
            self.handle_input_key(key);
            return true;
        }

        // Role-bind mode: capture role name for the selected session (#307)
        if self.role_bind_mode {
            self.handle_role_bind_key(key);
            return true;
        }

        // Skills overlay: dedicated keymap (j/k navigate, s share, h serve, r rescan, Esc/K close)
        if self.show_skills {
            self.handle_skills_key(key);
            return true;
        }

        // Brain overlay: dedicated keymap (j/k navigate, Tab switch, m mark, n note, r refresh, Esc/B close)
        if self.show_brain {
            self.handle_brain_key(key);
            return true;
        }

        // Supervisor overlay: dedicated keymap (j/k navigate, r refresh, Esc/T close)
        #[cfg(feature = "coord")]
        if self.show_supervisor {
            self.handle_supervisor_key(key);
            return true;
        }

        // Override reason prompt: waiting for 1/2/3/Esc
        if self.pending_override_reason.is_some() {
            match key.code {
                KeyCode::Char('1') => {
                    self.handle_brain_accept_with_reason(Some("always_safe"));
                }
                KeyCode::Char('2') => {
                    self.handle_brain_accept_with_reason(Some("one_time_exception"));
                }
                KeyCode::Char('3') => {
                    self.handle_brain_accept_with_reason(Some("brain_is_wrong"));
                }
                KeyCode::Esc => {
                    self.pending_override_reason = None;
                    self.status_msg = "Override cancelled".into();
                }
                _ => {}
            }
            return true;
        }

        // Normal mode
        self.handle_normal_key(key);
        !self.should_quit
    }

    pub(super) fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if let Some(pid) = self.input_target_pid {
                    if let Some(session) = self.sessions.iter().find(|s| s.pid == pid) {
                        // Log passive observation: user sent manual input
                        let _ = self
                            .runtime
                            .actions
                            .log_observation(observation_from(session, "user_input"));
                        let text = format!("{}\n", self.input_buffer);
                        match self.runtime.actions.inject_text(&session.session_id, &text) {
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

    pub(super) fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.search_query = self.search_buffer.trim().to_string();
                self.search_mode = false;
                self.normalize_selection();
                if self.search_query.is_empty() {
                    self.status_msg = "Search cleared".into();
                } else {
                    self.status_msg = format!("Search: {}", self.search_query);
                }
            }
            KeyCode::Esc => {
                self.search_mode = false;
                self.search_buffer.clear();
                self.status_msg = "Search cancelled".into();
            }
            KeyCode::Backspace => {
                self.search_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.search_buffer.push(c);
            }
            _ => {}
        }
    }

    pub(super) fn handle_brain_key(&mut self, key: KeyEvent) {
        if self.brain_note_input_mode {
            self.handle_brain_note_input(key);
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('M'), _) | (KeyCode::Char('q'), _) => {
                self.show_brain = false;
                self.brain_status_msg = None;
                return;
            }
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                self.brain_tab = self.brain_tab.toggle();
                self.brain_status_msg = None;
                return;
            }
            (KeyCode::Char('r'), _) => {
                self.refresh_brain();
                self.brain_status_msg = Some("Refreshed.".into());
                return;
            }
            _ => {}
        }

        if matches!(self.brain_tab, BrainTab::Review) {
            self.handle_brain_review_tab_key(key);
        }
    }

    pub(super) fn handle_brain_review_tab_key(&mut self, key: KeyEvent) {
        if self.brain_queue.is_empty() {
            return;
        }
        let last = self.brain_queue.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.brain_review_selected < last {
                    self.brain_review_selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.brain_review_selected > 0 {
                    self.brain_review_selected -= 1;
                }
            }
            KeyCode::Char('g') | KeyCode::Home => self.brain_review_selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.brain_review_selected = last,
            KeyCode::Char('m') => self.mark_selected_canonical(None),
            KeyCode::Char('n') => {
                self.brain_note_input_mode = true;
                self.brain_note_buffer.clear();
                self.brain_status_msg = Some("Type a note, Enter to save, Esc to cancel.".into());
            }
            KeyCode::Char('s') | KeyCode::Right | KeyCode::Char('l')
                if self.brain_review_selected < last =>
            {
                self.brain_review_selected += 1;
                self.brain_status_msg = Some("Skipped.".into());
            }
            _ => {}
        }
    }

    pub(super) fn handle_brain_note_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let note = self.brain_note_buffer.trim().to_string();
                self.brain_note_input_mode = false;
                self.brain_note_buffer.clear();
                if note.is_empty() {
                    self.brain_status_msg = Some("Empty note — not saved.".into());
                } else {
                    self.mark_selected_canonical(Some(&note));
                }
            }
            KeyCode::Esc => {
                self.brain_note_input_mode = false;
                self.brain_note_buffer.clear();
                self.brain_status_msg = Some("Note cancelled.".into());
            }
            KeyCode::Backspace => {
                self.brain_note_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.brain_note_buffer.push(c);
            }
            _ => {}
        }
    }

    pub(super) fn handle_skills_key(&mut self, key: KeyEvent) {
        if self.hive_join_input_mode {
            self.handle_hive_join_input(key);
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('K'), _) | (KeyCode::Char('q'), _) => {
                self.show_skills = false;
                self.skills_status_msg = None;
                return;
            }
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                self.skills_tab = self.skills_tab.toggle();
                self.skills_status_msg = None;
                return;
            }
            _ => {}
        }

        match self.skills_tab {
            SkillsTab::Skills => self.handle_skills_tab_key(key),
            SkillsTab::Hive => self.handle_hive_tab_key(key),
        }
    }

    pub(super) fn handle_skills_tab_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('j'), _) | (KeyCode::Down, _)
                if !self.skills.is_empty() && self.skills_selected + 1 < self.skills.len() =>
            {
                self.skills_selected += 1;
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) if self.skills_selected > 0 => {
                self.skills_selected -= 1;
            }
            (KeyCode::Char('r'), _) => {
                self.refresh_skills();
                self.skills_status_msg = Some(format!("Rescanned: {} skills", self.skills.len()));
            }
            (KeyCode::Char('s'), _) => {
                self.share_selected_skill();
            }
            _ => {}
        }
    }

    pub(super) fn handle_hive_tab_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('h'), _) => {
                self.start_hive_listener();
                self.refresh_hive_view();
            }
            (KeyCode::Char('i'), _) => {
                self.generate_hive_invite();
            }
            (KeyCode::Char('J'), _) => {
                self.hive_join_input_mode = true;
                self.hive_join_buffer.clear();
                self.skills_status_msg =
                    Some("Paste invite (relay code, link, or word phrase); Enter to join".into());
            }
            (KeyCode::Char('r'), _) => {
                self.refresh_hive_view();
                self.skills_status_msg =
                    Some(format!("Known peers: {}", self.hive_known_peers.len()));
            }
            _ => {}
        }
    }

    pub(super) fn handle_hive_join_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let code = self.hive_join_buffer.trim().to_string();
                self.hive_join_input_mode = false;
                if code.is_empty() {
                    self.skills_status_msg = Some("Join cancelled (empty)".into());
                    return;
                }
                match spawn_relay_join(&code) {
                    Ok(()) => {
                        self.skills_status_msg = Some(format!(
                            "Join started (claudectl relay join {} detached)",
                            short_id(&code)
                        ));
                    }
                    Err(e) => {
                        self.skills_status_msg = Some(format!("Join failed: {e}"));
                    }
                }
                self.hive_join_buffer.clear();
            }
            KeyCode::Esc => {
                self.hive_join_input_mode = false;
                self.hive_join_buffer.clear();
                self.skills_status_msg = Some("Join cancelled".into());
            }
            KeyCode::Backspace => {
                self.hive_join_buffer.pop();
            }
            KeyCode::Char(c) if self.hive_join_buffer.len() < 256 => {
                self.hive_join_buffer.push(c);
            }
            _ => {}
        }
    }

    pub(super) fn handle_normal_key(&mut self, key: KeyEvent) {
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
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                // Bus role bind (#307). Ctrl+R because plain `r` is refresh.
                // Match before the unconditional `r` arm below, otherwise
                // the wildcard modifier swallows the Control modifier.
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_role_bind_mode();
            }
            (KeyCode::Char('r'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.refresh();
            }
            (KeyCode::Char('R'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.toggle_session_recording();
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
            (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.toggle_brain_gate();
            }
            (KeyCode::Char('b'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_brain_accept();
            }
            (KeyCode::Char('B'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_brain_reject();
            }
            (KeyCode::Char('i'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_input_mode();
            }
            (KeyCode::Char('c'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_compact();
            }
            (KeyCode::Char('?'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.show_help = !self.show_help;
            }
            (KeyCode::Char('K'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.open_skills_overlay();
            }
            (KeyCode::Char('M'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.open_brain_overlay();
            }
            #[cfg(feature = "coord")]
            (KeyCode::Char('T'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.open_supervisor_overlay();
            }
            (KeyCode::Char('s'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.cycle_sort();
            }
            (KeyCode::Char('f'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.cycle_status_filter();
            }
            (KeyCode::Char('v'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.cycle_focus_filter();
            }
            (KeyCode::Char('z'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.clear_filters();
            }
            (KeyCode::Char('/'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_search_mode();
            }
            (KeyCode::Char('a'), _) => {
                self.cancel_pending_kill();
                self.handle_auto_approve();
            }
            (KeyCode::Char('n'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_launch_mode();
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
            #[cfg(feature = "relay")]
            (KeyCode::Char('p'), KeyModifiers::NONE) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.show_peers_panel = !self.show_peers_panel;
                self.status_msg = if self.show_peers_panel {
                    "Peers panel enabled".into()
                } else {
                    "Peers panel disabled".into()
                };
            }
            _ => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
            }
        }
    }

    pub(super) fn handle_launch_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit_launch_form();
            }
            KeyCode::Enter => {
                if self.launch_form.is_last_field() {
                    self.submit_launch_form();
                } else {
                    self.launch_form.advance();
                    self.status_msg = self.launch_form.status_hint();
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                self.launch_form.advance();
                self.status_msg = self.launch_form.status_hint();
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.launch_form.retreat();
                self.status_msg = self.launch_form.status_hint();
            }
            KeyCode::Esc => {
                self.launch_mode = false;
                self.launch_form = LaunchForm::default();
                self.status_msg = "Launch cancelled".into();
            }
            KeyCode::Backspace => {
                self.launch_form.active_buffer_mut().pop();
            }
            KeyCode::Char(c) => {
                self.launch_form.active_buffer_mut().push(c);
            }
            _ => {}
        }
    }

    pub(super) fn enter_launch_mode(&mut self) {
        self.launch_mode = true;
        self.launch_form = LaunchForm::default();
        self.status_msg = self.launch_form.status_hint();
    }

    pub(super) fn enter_search_mode(&mut self) {
        self.search_mode = true;
        self.search_buffer = self.search_query.clone();
    }

    pub(super) fn enter_input_mode(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.is_remote() {
                self.status_msg = "Remote session \u{2014} action not available".into();
                return;
            }
        }
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

    /// Open the role-bind prompt for the selected session (#307). Captures
    /// the session's pid and cwd at entry time so a refresh tick or row
    /// move during typing can't change the target.
    pub(super) fn enter_role_bind_mode(&mut self) {
        let Some(session) = self.selected_session() else {
            self.status_msg = "No session selected".into();
            return;
        };
        if session.is_remote() {
            self.status_msg = "Remote session \u{2014} bind locally instead".into();
            return;
        }
        let pid = session.pid;
        let cwd = session.cwd.clone();
        let name = session.display_name().to_string();
        self.role_bind_mode = true;
        self.role_bind_buffer.clear();
        self.role_bind_target_pid = Some(pid);
        self.role_bind_target_cwd = Some(cwd);
        self.status_msg =
            format!("Bind role for {name} (pid={pid}, Enter to bind, Esc to cancel): ");
    }

    pub(super) fn handle_role_bind_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let role = self.role_bind_buffer.trim().to_string();
                let pid = self.role_bind_target_pid;
                let cwd = self.role_bind_target_cwd.clone();
                self.role_bind_mode = false;
                self.role_bind_buffer.clear();
                self.role_bind_target_pid = None;
                self.role_bind_target_cwd = None;
                if role.is_empty() {
                    self.status_msg = "Role name required".into();
                    return;
                }
                let (Some(pid), Some(cwd)) = (pid, cwd) else {
                    self.status_msg = "Lost bind target — re-select the session".into();
                    return;
                };
                match self.runtime.actions.bind_bus_role(&role, &cwd, pid) {
                    Ok(()) => {
                        self.status_msg = format!("Bound role {role} -> pid={pid} cwd={cwd}");
                    }
                    Err(e) => {
                        self.status_msg = format!("Bind failed: {e}");
                    }
                }
            }
            KeyCode::Esc => {
                self.role_bind_mode = false;
                self.role_bind_buffer.clear();
                self.role_bind_target_pid = None;
                self.role_bind_target_cwd = None;
                self.status_msg = "Role bind cancelled".into();
            }
            KeyCode::Backspace => {
                self.role_bind_buffer.pop();
            }
            // Role names are short, alpha-numeric with - and _. Cap at 64
            // so a runaway paste can't take the prompt hostage.
            KeyCode::Char(c)
                if self.role_bind_buffer.len() < 64
                    && (c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
            {
                self.role_bind_buffer.push(c);
            }
            _ => {}
        }
    }
}

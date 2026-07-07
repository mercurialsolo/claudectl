//! Extracted from app/mod.rs — behavior-preserving split.
#![allow(clippy::too_many_lines)]

use super::*;

impl App {
    pub(super) fn handle_approve(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.is_remote() {
                self.status_msg = "Remote session \u{2014} action not available".into();
                return;
            }
            if session.status == SessionStatus::NeedsInput {
                // Log passive observation: user approved without brain involvement
                let _ = self
                    .runtime
                    .actions
                    .log_observation(observation_from(session, "user_approve"));
                match terminals::approve_session(session) {
                    Ok(()) => self.status_msg = format!("Approved {}", session.display_name()),
                    Err(e) => self.status_msg = format!("Error: {e}"),
                }
            } else {
                self.status_msg = "Session is not waiting for input".into();
            }
        }
    }

    pub(super) fn handle_brain_accept(&mut self) {
        self.handle_brain_accept_with_reason(None);
    }

    pub(super) fn handle_brain_accept_with_reason(&mut self, override_reason: Option<&str>) {
        // Clone session data first to avoid borrow conflict with brain_engine
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if session.is_remote() {
            self.status_msg = "Remote session \u{2014} action not available".into();
            return;
        }
        let pid = session.pid;
        let Some(ref mut driver) = self.brain_driver else {
            self.status_msg = "Brain is not enabled".into();
            return;
        };
        // Get suggestion before accept (for logging)
        let suggestion = driver.pending_for(pid);
        let Some(sg) = suggestion else {
            self.status_msg = "No brain suggestion pending for this session".into();
            return;
        };

        // If brain suggested deny and no override reason yet, prompt for one
        if sg.action == "deny" && override_reason.is_none() {
            self.pending_override_reason = Some(pid);
            self.status_msg =
                "Override reason: [1] Always safe  [2] One-time exception  [3] Brain is wrong  [Esc] Cancel"
                    .into();
            return;
        }

        if let Some(msg) = driver.accept(pid) {
            let _ = self
                .runtime
                .actions
                .log_decision(claudectl_core::runtime::LogDecisionInput {
                    session_pid: pid,
                    project: session.display_name().to_string(),
                    tool: session.pending_tool_name.clone(),
                    command: session.pending_tool_input.clone(),
                    suggestion: sg,
                    user_action: "accept".into(),
                    decision_type: claudectl_core::runtime::DecisionScope::Session,
                    override_reason: override_reason.map(String::from),
                });
            claudectl_core::logger::log("BRAIN", &format!("Accepted: {msg}"));
            self.status_msg = msg;
        }
        self.pending_override_reason = None;
    }

    pub(super) fn handle_brain_reject(&mut self) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if session.is_remote() {
            self.status_msg = "Remote session \u{2014} action not available".into();
            return;
        }
        let pid = session.pid;
        let Some(ref mut driver) = self.brain_driver else {
            self.status_msg = "Brain is not enabled".into();
            return;
        };
        if let Some(suggestion) = driver.reject(pid) {
            let log_input = claudectl_core::runtime::LogDecisionInput {
                session_pid: pid,
                project: session.display_name().to_string(),
                tool: session.pending_tool_name.clone(),
                command: session.pending_tool_input.clone(),
                suggestion: suggestion.clone(),
                user_action: "reject".into(),
                decision_type: claudectl_core::runtime::DecisionScope::Session,
                override_reason: None,
            };
            let _ = self.runtime.actions.log_decision(log_input);
            let msg = format!(
                "Rejected brain suggestion: {} ({})",
                suggestion.action, suggestion.reasoning,
            );
            claudectl_core::logger::log("BRAIN", &msg);
            self.status_msg = msg;
        } else {
            self.status_msg = "No brain suggestion pending for this session".into();
        }
    }

    pub(super) fn toggle_brain_gate(&mut self) {
        use claudectl_core::runtime::BrainGateMode;
        let current = self.runtime.brain.gate_mode();
        // Toggle: On → Off, Off → On, Auto → Off. The wizard flips through
        // runtime.actions so the on-disk format stays in sync with what
        // BrainView reports next refresh.
        let next = match current {
            BrainGateMode::On => BrainGateMode::Off,
            BrainGateMode::Off => BrainGateMode::On,
            BrainGateMode::Auto => BrainGateMode::Off,
        };
        if let Err(e) = self.runtime.actions.set_gate_mode(next) {
            self.status_msg = format!("Brain: gate-mode update failed: {e}");
            return;
        }

        let description = match next {
            BrainGateMode::On => "active — evaluating tool calls",
            BrainGateMode::Off => "disabled — normal permission flow",
            BrainGateMode::Auto => "auto — automatic decisions",
        };
        self.status_msg = format!("Brain: {description}");
        claudectl_core::logger::log("BRAIN", &format!("Gate mode toggled: {current} → {next}"));
    }

    pub(super) fn toggle_session_recording(&mut self) {
        let info = self
            .selected_session()
            .map(|s| (s.pid, s.display_name().to_string(), s.jsonl_path.is_some()));
        let Some((pid, name, has_jsonl)) = info else {
            return;
        };

        // Per-session toggle: if this session is recording, stop just this one
        if self.session_recordings.contains_key(&pid) {
            let path = self.session_recordings.remove(&pid).unwrap_or_default();
            self.status_msg = format!("Recording stopped → {path}");
            return;
        }

        // Start recording the selected session
        if !has_jsonl {
            self.status_msg = "Cannot record — no JSONL file for this session".into();
            return;
        }
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let path = format!("{}-{}-{}.gif", name, pid, epoch);
        self.session_recordings.insert(pid, path.clone());
        self.status_msg = format!("Recording {name} → {path} (R to stop)");
    }

    pub(super) fn handle_compact(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.is_remote() {
                self.status_msg = "Remote session \u{2014} action not available".into();
                return;
            }
            match session.status {
                SessionStatus::WaitingInput | SessionStatus::Idle => {
                    match self
                        .runtime
                        .actions
                        .inject_text(&session.session_id, "/compact\n")
                    {
                        Ok(()) => {
                            self.status_msg = format!("Sent /compact to {}", session.display_name())
                        }
                        Err(e) => self.status_msg = format!("Compact error: {e}"),
                    }
                }
                SessionStatus::NeedsInput => {
                    self.status_msg =
                        "Cannot compact — session is waiting for permission approval".into();
                }
                SessionStatus::Processing => {
                    self.status_msg =
                        "Cannot compact — session is processing (wait until idle)".into();
                }
                SessionStatus::Unknown => {
                    self.status_msg =
                        "Cannot compact — transcript telemetry is unavailable for this session"
                            .into();
                }
                SessionStatus::Finished => {
                    self.status_msg = "Cannot compact — session has finished".into();
                }
            }
        }
    }

    pub(super) fn handle_switch_terminal(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.is_remote() {
                self.status_msg = "Remote session \u{2014} action not available".into();
                return;
            }
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

    pub(super) fn submit_launch_form(&mut self) {
        let request = match self.launch_form.request() {
            Ok(request) => request,
            Err(err) => {
                self.launch_form.field = LaunchField::Cwd;
                self.status_msg = format!("Launch failed: {err}");
                return;
            }
        };

        match launch::launch(&request) {
            Ok(target) => {
                self.launch_mode = false;
                self.launch_form = LaunchForm::default();
                self.status_msg = format!(
                    "Launched session in {target} at {}{}",
                    request.cwd_path.display(),
                    request.option_summary()
                );
            }
            Err(err) => {
                self.status_msg = format!("Launch failed: {err}");
            }
        }
    }

    pub(super) fn mark_selected_canonical(&mut self, note: Option<&str>) {
        let Some(item) = self.brain_queue.get(self.brain_review_selected) else {
            return;
        };
        // DecisionSummary stores the id as a String; empty == "no decision_id".
        let id = item.decision.id.clone();
        if id.is_empty() {
            self.brain_status_msg = Some("No decision_id — older record, can't mark.".into());
            return;
        }
        match self
            .runtime
            .actions
            .mark_canonical(&id, note.map(String::from))
        {
            Ok(()) => {
                self.brain_status_msg = Some(if note.is_some() {
                    format!("Marked canonical with note: {id}")
                } else {
                    format!("Marked canonical: {id}")
                });
                // Drop the marked item and advance selection naturally.
                self.brain_queue.remove(self.brain_review_selected);
                if self.brain_review_selected >= self.brain_queue.len() {
                    self.brain_review_selected = self.brain_queue.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.brain_status_msg = Some(format!("Could not mark canonical: {e}"));
            }
        }
    }
}

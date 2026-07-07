//! Extracted from app/mod.rs — behavior-preserving split.
#![allow(clippy::too_many_lines)]

use super::*;

impl App {
    pub(super) fn refresh_demo(&mut self) {
        // During the guided tour (#373) pin the scene to the current step so
        // the moment being narrated stays on screen while the user reads.
        if let Some(tour) = &self.demo_tour {
            self.demo_tick = tour.step().demo_tick;
        } else {
            self.demo_tick += 1;
        }
        let mut sessions = crate::demo::generate_sessions(self.demo_tick);

        // When the Skills & Hive view is open during a demo, scripted
        // navigation: cycle selection on the Skills tab, then flip to Hive
        // around tick 6, then back to Skills around tick 12.
        if self.show_skills {
            let phase = self.demo_tick % 14;
            match phase {
                1..=5 => {
                    self.skills_tab = SkillsTab::Skills;
                    if !self.skills.is_empty() {
                        self.skills_selected =
                            ((phase as usize - 1) % self.skills.len()).min(self.skills.len() - 1);
                    }
                }
                6 => {
                    self.skills_tab = SkillsTab::Hive;
                    self.skills_status_msg = Some("Hive: 2 peers connected".into());
                }
                7..=11 => {
                    self.skills_tab = SkillsTab::Hive;
                }
                _ => {
                    self.skills_tab = SkillsTab::Skills;
                    self.skills_status_msg = None;
                }
            }
        }

        // Track NeedsInput wait times (same as real mode)
        let now_instant = std::time::Instant::now();
        for session in &sessions {
            if session.status == SessionStatus::NeedsInput {
                self.needs_input_since
                    .entry(session.pid)
                    .or_insert(now_instant);
            } else {
                self.needs_input_since.remove(&session.pid);
            }
        }

        // Conflict detection using worktree_id
        self.conflict_pids.clear();
        let mut wt_sessions: HashMap<&str, Vec<u32>> = HashMap::new();
        for session in &sessions {
            if session.status != SessionStatus::Finished {
                let key = session.worktree_id.as_deref().unwrap_or(&session.cwd);
                wt_sessions.entry(key).or_default().push(session.pid);
            }
        }
        for pids in wt_sessions.values() {
            if pids.len() >= 2 {
                for &pid in pids {
                    self.conflict_pids.insert(pid);
                }
            }
        }

        // Scripted demo events: rules, brain, routing, health alerts
        if let Some(event) = crate::demo::demo_event(self.demo_tick) {
            self.status_msg = event.message.clone();
            match event.kind {
                crate::demo::EventKind::RuleAction => {
                    self.last_rule_action = Some(event.message);
                }
                crate::demo::EventKind::BrainSuggestion | crate::demo::EventKind::BrainOverride => {
                    // Show brain activity via status message
                }
                crate::demo::EventKind::Route | crate::demo::EventKind::HealthAlert => {}
                crate::demo::EventKind::HiveSync | crate::demo::EventKind::HiveInfluence => {}
            }
        }

        // Update demo peers panel and remote sessions
        #[cfg(feature = "relay")]
        {
            self.relay_peers = crate::demo::demo_peers(self.demo_tick);
            // Auto-show peers panel on first hive sync event
            if self.demo_tick % 32 == 14 && !self.show_peers_panel {
                self.show_peers_panel = true;
            }
            // Demo remote sessions from connected peers
            self.remote_sessions.clear();
            if self.demo_tick % 32 >= 14 {
                let remote_json = serde_json::json!({
                    "pid": 99001, "project": "backend",
                    "status": "Processing", "cost_usd": 1.4,
                    "elapsed_secs": 320, "context_pct": 42.0,
                });
                if let Some(s) = ClaudeSession::from_remote_json("ci-runner-9d1e", &remote_json) {
                    self.remote_sessions.push(s);
                }
            }
            if self.demo_tick % 32 >= 28 {
                let remote_json = serde_json::json!({
                    "pid": 99002, "project": "frontend",
                    "status": "Needs Input", "cost_usd": 0.32,
                    "elapsed_secs": 150,
                });
                if let Some(s) = ClaudeSession::from_remote_json("alice-mbp-f3a1", &remote_json) {
                    self.remote_sessions.push(s);
                }
            }
        }

        // Inject fake brain pending suggestions so the status bar shows brain activity.
        // Demo mode flows through the BrainDriver trait's set_pending escape hatch
        // rather than mutating an engine field directly — same path the real brain
        // would take.
        if let Some(ref mut driver) = self.brain_driver {
            driver.clear_pending();
            let phase = self.demo_tick % 32;
            if (9..=12).contains(&phase) {
                if let Some(s) = sessions
                    .iter()
                    .find(|s| s.status == SessionStatus::NeedsInput)
                {
                    driver.set_pending(claudectl_core::runtime::PendingSuggestion {
                        pid: s.pid,
                        action: "approve".into(),
                        message: s.pending_tool_input.clone(),
                        reasoning: "Safe build command, no side effects".into(),
                        confidence: 0.92,
                        suggested_at: 0,
                    });
                }
            }
            if (14..=16).contains(&phase) {
                if let Some(s) = sessions
                    .iter()
                    .find(|s| s.status == SessionStatus::NeedsInput)
                {
                    driver.set_pending(claudectl_core::runtime::PendingSuggestion {
                        pid: s.pid,
                        action: "deny".into(),
                        message: s.pending_tool_input.clone(),
                        reasoning: "Destructive operation, needs manual review".into(),
                        confidence: 0.87,
                        suggested_at: 0,
                    });
                }
            }
        }

        // ── Demo highlight reel support ────────────────────────────────
        // Ensure demo sessions have JSONL paths so the session recorder can attach.
        // Drip-feed scripted events for sessions that are actively being recorded.
        let highlight = self
            .demo_highlight
            .get_or_insert_with(crate::demo::DemoHighlightState::new);

        for session in &mut sessions {
            let path = highlight.ensure_jsonl(session.pid).clone();
            session.jsonl_path = Some(path);
        }

        // Feed new JSONL events only into sessions being recorded.
        // When the script is exhausted, mark the PID for auto-stop.
        let recording_pids: Vec<u32> = self.session_recordings.keys().copied().collect();
        let mut finished_pids: Vec<u32> = Vec::new();
        for pid in recording_pids {
            if !highlight.drip_feed(pid) {
                finished_pids.push(pid);
            }
        }

        // Auto-stop recordings whose scripts are done
        for pid in finished_pids {
            if let Some(path) = self.session_recordings.remove(&pid) {
                self.status_msg = format!("Recording complete → {path}");
            }
        }

        // Compute decay scores for demo sessions (same as real refresh path)
        for session in &mut sessions {
            session.decay_score =
                claudectl_core::health::compute_decay_score(session, &self.health_thresholds);
        }

        self.sessions = sessions;
        self.normalize_selection();
    }
}

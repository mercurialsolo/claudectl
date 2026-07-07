//! Extracted from app/mod.rs — behavior-preserving split.
#![allow(clippy::too_many_lines)]

use super::*;

impl App {
    pub fn open_skills_overlay(&mut self) {
        self.refresh_skills();
        self.refresh_hive_view();
        self.skills_selected = 0;
        self.skills_status_msg = None;
        self.hive_join_input_mode = false;
        self.hive_join_buffer.clear();
        self.show_skills = true;
    }

    pub fn refresh_skills(&mut self) {
        let cwd = std::env::current_dir().ok();
        self.skills = claudectl_core::skills::discover(cwd.as_deref());
        self.shared_skill_keys = self.runtime.hive.shared_skill_keys();
        if self.skills_selected >= self.skills.len() {
            self.skills_selected = self.skills.len().saturating_sub(1);
        }
    }

    pub fn refresh_hive_view(&mut self) {
        let snapshot = self.runtime.hive.hive_view_snapshot();
        self.hive_identity = snapshot.identity;
        self.hive_known_peers = snapshot.peers;
    }

    pub fn open_brain_overlay(&mut self) {
        self.refresh_brain();
        self.brain_review_selected = 0;
        self.brain_status_msg = None;
        self.brain_note_input_mode = false;
        self.brain_note_buffer.clear();
        self.brain_tab = BrainTab::Scorecard;
        self.show_brain = true;
    }

    pub fn refresh_brain(&mut self) {
        self.brain_decisions_cache = self.runtime.review.all_decisions();
        self.brain_queue = self.runtime.review.review_queue();
        if self.brain_review_selected >= self.brain_queue.len() {
            self.brain_review_selected = self.brain_queue.len().saturating_sub(1);
        }
    }

    pub(super) fn generate_hive_invite(&mut self) {
        match generate_invite_via_cli() {
            Ok(invite) => {
                self.skills_status_msg = Some(format!("Invite: {}", invite.relay_code));
                self.hive_last_invite = Some(invite);
            }
            Err(e) => {
                self.skills_status_msg = Some(format!("Invite failed: {e}"));
            }
        }
    }

    pub(super) fn share_selected_skill(&mut self) {
        let Some(skill) = self.skills.get(self.skills_selected).cloned() else {
            self.skills_status_msg = Some("No skill selected".into());
            return;
        };
        if !cfg!(feature = "hive") {
            self.skills_status_msg = Some("hive feature disabled in this build".into());
            return;
        }
        if !skill.within_share_limit() {
            self.skills_status_msg = Some("Skill exceeds 32kb share limit".into());
            return;
        }
        if self.shared_skill_keys.contains(&skill.semantic_key()) {
            self.skills_status_msg = Some("Already shared".into());
            return;
        }
        match self.runtime.hive.share_skill(&skill) {
            Ok(unit_id) => {
                self.shared_skill_keys.insert(skill.semantic_key());
                self.skills_status_msg = Some(format!(
                    "Shared '{}' → unit {}",
                    skill.name,
                    short_id(&unit_id)
                ));
            }
            Err(e) => {
                self.skills_status_msg = Some(format!("Share failed: {e}"));
            }
        }
    }

    pub(super) fn start_hive_listener(&mut self) {
        if !cfg!(feature = "relay") {
            self.skills_status_msg =
                Some("relay feature not built — rebuild with --features relay,hive".into());
            return;
        }
        if self.hive_listener_running {
            self.skills_status_msg = Some("Hive listener already running".into());
            return;
        }
        match spawn_relay_serve() {
            Ok(()) => {
                self.hive_listener_running = true;
                self.skills_status_msg =
                    Some("Hive listener started (claudectl relay serve detached)".into());
            }
            Err(e) => {
                self.skills_status_msg = Some(format!("Start failed: {e}"));
            }
        }
    }
}

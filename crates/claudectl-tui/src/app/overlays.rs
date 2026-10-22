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
        // `skills_tab` persists across opens, so `K` can land straight on Hive.
        self.sync_hive_discovery();
    }

    /// Discovery runs exactly while the Hive tab is showing. One place
    /// decides, and every path that changes the overlay or the tab — open,
    /// close, `Tab`, the demo's scripted flips, and the tick — calls it, so the
    /// listener can neither outlive the tab nor miss its opening.
    pub fn sync_hive_discovery(&mut self) {
        let showing = self.show_skills && self.skills_tab == SkillsTab::Hive;
        if !showing {
            self.hive_discovery = None;
            self.hive_discovery_unavailable = false;
            self.hive_discovered = claudectl_core::runtime::DiscoverySnapshot::default();
            return;
        }
        if self.demo_mode {
            self.hive_discovered = crate::demo::demo_discovered(self.demo_tick);
            return;
        }
        if self.hive_discovery.is_none() && !self.hive_discovery_unavailable {
            self.hive_discovery = self.runtime.hive.start_discovery();
            // Nothing on this build can listen. Remember that for as long as
            // the tab shows rather than asking again on every tick.
            self.hive_discovery_unavailable = self.hive_discovery.is_none();
        }
        self.hive_discovered = match self.hive_discovery.as_mut() {
            Some(listener) => listener.snapshot(),
            None => claudectl_core::runtime::DiscoverySnapshot::default(),
        };
    }

    pub fn refresh_skills(&mut self) {
        let cwd = std::env::current_dir().ok();
        self.skills = claudectl_core::skills::discover(cwd.as_deref());
        self.shared_skill_keys = self.runtime.hive.shared_skill_keys();
        if self.skills_selected >= self.skills.len() {
            self.skills_selected = self.skills.len().saturating_sub(1);
        }
    }

    /// Re-read the `p` panel's peers. Cheap: a few small files per peer.
    /// Demo mode keeps its fixtures: a recording must not show this
    /// machine's real peers.
    #[cfg(feature = "relay")]
    pub fn refresh_relay_peers(&mut self) {
        if self.demo_mode {
            return;
        }
        self.relay_peers = self.runtime.hive.hive_view_snapshot().peers;
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

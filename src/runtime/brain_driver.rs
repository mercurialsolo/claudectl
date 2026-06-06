//! Bind `BrainDriver` to the binary's `BrainEngine`.
//!
//! Wraps a `BrainEngine` instance and translates the trait's
//! `SessionSnapshot` inputs to the live `ClaudeSession` values the engine
//! expects, looking up real sessions via discovery on each call. The
//! `PendingSuggestion` DTO is projected from the brain's internal type.

use claudectl_core::discovery;
use claudectl_core::rules::AutoRule;
use claudectl_core::runtime::{BrainDriver, PendingSuggestion, SessionSnapshot};
use claudectl_core::session::ClaudeSession;

use crate::brain;
use crate::brain::client::BrainSuggestion;

pub struct LiveBrainDriver {
    engine: brain::engine::BrainEngine,
}

impl LiveBrainDriver {
    /// Wrap an existing `BrainEngine`. The next PR (call-site migration)
    /// is what makes `App::new` call this; until then the constructor is
    /// referenced only by tests.
    #[allow(dead_code)]
    pub fn new(engine: brain::engine::BrainEngine) -> Self {
        Self { engine }
    }

    /// Convert a SessionSnapshot batch into the live `ClaudeSession` values
    /// the engine expects. Sessions that have exited between the snapshot
    /// and the call are silently dropped (the engine's cleanup pass will
    /// notice independently).
    fn resolve_live(&self, snapshots: &[SessionSnapshot]) -> Vec<ClaudeSession> {
        let mut live = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut live);
        let mut by_id: std::collections::HashMap<String, ClaudeSession> = live
            .into_iter()
            .map(|s| (s.session_id.clone(), s))
            .collect();
        snapshots
            .iter()
            .filter_map(|snap| by_id.remove(snap.session_id.as_str()))
            .collect()
    }
}

impl BrainDriver for LiveBrainDriver {
    fn tick(
        &mut self,
        sessions: &[SessionSnapshot],
        deny_rules: &[AutoRule],
    ) -> Vec<(u32, String)> {
        let live = self.resolve_live(sessions);
        self.engine.tick(&live, deny_rules)
    }

    fn cleanup(&mut self, sessions: &[SessionSnapshot]) {
        let live = self.resolve_live(sessions);
        self.engine.cleanup(&live);
    }

    fn accept(&mut self, pid: u32) -> Option<String> {
        // accept() needs the live session for the PID; look it up directly.
        let mut live = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut live);
        let session = live.into_iter().find(|s| s.pid == pid)?;
        self.engine.accept(pid, &session)
    }

    fn reject(&mut self, pid: u32) -> Option<PendingSuggestion> {
        self.engine
            .reject(pid)
            .map(|s| suggestion_from_brain(pid, s))
    }

    fn pending_for(&self, pid: u32) -> Option<PendingSuggestion> {
        self.engine
            .pending
            .get(&pid)
            .cloned()
            .map(|s| suggestion_from_brain(pid, s))
    }

    fn pending_count(&self) -> usize {
        self.engine.pending.len()
    }

    fn clear_pending(&mut self) {
        self.engine.pending.clear();
    }
}

fn suggestion_from_brain(pid: u32, s: BrainSuggestion) -> PendingSuggestion {
    PendingSuggestion {
        pid,
        action: format!("{:?}", s.action).to_lowercase(),
        message: s.message,
        reasoning: s.reasoning,
        confidence: s.confidence,
        suggested_at: s.suggested_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RuleAction` formatted via `Debug` (then lowercased) is the wire
    /// label callers see. Lock the shape so future variants don't silently
    /// drift.
    #[test]
    fn action_label_format_is_stable() {
        let s = BrainSuggestion {
            action: claudectl_core::rules::RuleAction::Approve,
            message: None,
            reasoning: "test".into(),
            confidence: 0.5,
            suggested_at: 0,
        };
        let proj = suggestion_from_brain(42, s);
        assert_eq!(proj.action, "approve");
        assert_eq!(proj.pid, 42);
        assert_eq!(proj.confidence, 0.5);
    }

    #[test]
    fn deny_action_lowercases() {
        let s = BrainSuggestion {
            action: claudectl_core::rules::RuleAction::Deny,
            message: Some("dangerous".into()),
            reasoning: "rm -rf".into(),
            confidence: 0.99,
            suggested_at: 1_780_000_000,
        };
        let proj = suggestion_from_brain(99, s);
        assert_eq!(proj.action, "deny");
        assert_eq!(proj.message.as_deref(), Some("dangerous"));
        assert_eq!(proj.suggested_at, 1_780_000_000);
    }
}

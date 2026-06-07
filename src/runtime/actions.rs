//! Bind `Actions` (the runtime write surface) to the binary's real
//! subsystems: brain decisions store, terminal backends, process kill.

use std::fs;

use claudectl_core::discovery;
use claudectl_core::helpers;
use claudectl_core::runtime::{
    Actions, BrainGateMode, DecisionScope, LogDecisionInput, ObservationInput,
};
use claudectl_core::terminals;

use crate::brain;

pub struct LiveActions;

impl Actions for LiveActions {
    fn terminate_session(&self, pid: u32) -> Result<(), String> {
        helpers::kill_process(pid)
    }

    fn inject_text(&self, session_id: &str, text: &str) -> Result<(), String> {
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);
        let Some(session) = sessions.into_iter().find(|s| s.session_id == session_id) else {
            return Err(format!("session {session_id} not running"));
        };
        terminals::send_input(&session, text)
    }

    fn set_gate_mode(&self, mode: BrainGateMode) -> Result<(), String> {
        let path = brain::gate_mode_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create gate-mode dir: {e}"))?;
        }
        fs::write(&path, gate_mode_label(mode)).map_err(|e| format!("write gate-mode: {e}"))
    }

    fn log_observation(&self, observation: ObservationInput) -> Result<(), String> {
        // Look up the session for richer context, when the PID is currently
        // running. We don't bail if it isn't — the brain happily logs orphan
        // observations.
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);
        let session_ref = sessions.iter().find(|s| s.pid == observation.session_pid);

        brain::decisions::log_observation(
            observation.session_pid,
            &observation.project,
            observation.tool.as_deref(),
            observation.command.as_deref(),
            &observation.observed_action,
            session_ref,
        );
        Ok(())
    }

    fn log_decision(&self, input: LogDecisionInput) -> Result<(), String> {
        // Resolve the live session for richer context (cost, model, etc.) —
        // brain::decisions::log_decision tolerates None when the PID is gone.
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);
        let session_ref = sessions.iter().find(|s| s.pid == input.session_pid);

        // The trait's PendingSuggestion uses `action: String`; the brain
        // log_decision needs a `BrainSuggestion` with a real `RuleAction`.
        // Drop silently on unknown labels (caller validates upstream).
        let Some(rule_action) = claudectl_core::rules::RuleAction::parse(&input.suggestion.action)
        else {
            return Err(format!("unknown action label: {}", input.suggestion.action));
        };
        let suggestion = brain::client::BrainSuggestion {
            action: rule_action,
            message: input.suggestion.message,
            reasoning: input.suggestion.reasoning,
            confidence: input.suggestion.confidence,
            suggested_at: input.suggestion.suggested_at,
        };

        let decision_type = match input.decision_type {
            DecisionScope::Session => brain::decisions::DecisionType::Session,
            DecisionScope::Orchestration => brain::decisions::DecisionType::Orchestration,
        };

        brain::decisions::log_decision(
            input.session_pid,
            &input.project,
            input.tool.as_deref(),
            input.command.as_deref(),
            &suggestion,
            &input.user_action,
            session_ref,
            decision_type,
            input.override_reason.as_deref(),
        );
        Ok(())
    }

    fn mark_canonical(&self, decision_id: &str, note: Option<String>) -> Result<(), String> {
        brain::review::mark_by_id(decision_id, note.as_deref())
    }
}

/// Inverse of `crate::runtime::brain::parse_gate_mode` — writes the canonical
/// lowercased label the reader expects.
fn gate_mode_label(mode: BrainGateMode) -> &'static str {
    match mode {
        BrainGateMode::On => "on",
        BrainGateMode::Off => "off",
        BrainGateMode::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip the label format with the parser in the brain wrapper.
    #[test]
    fn label_round_trips_through_parse() {
        for mode in [BrainGateMode::On, BrainGateMode::Off, BrainGateMode::Auto] {
            let label = gate_mode_label(mode);
            let parsed = match label {
                "on" => BrainGateMode::On,
                "off" => BrainGateMode::Off,
                "auto" => BrainGateMode::Auto,
                _ => panic!("unexpected label: {label}"),
            };
            assert_eq!(parsed, mode);
        }
    }

    /// Set-then-read against a temporary HOME confirms the file actually
    /// lands at the expected path and the binary's `brain::read_gate_mode`
    /// picks it up.
    #[test]
    fn set_gate_mode_persists_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let original = std::env::var("HOME").ok();
        // Tests in this crate run serially per-thread, but this still races
        // with anything else that touches HOME. Acceptable for a smoke test.
        unsafe { std::env::set_var("HOME", dir.path()) };

        let actions = LiveActions;
        actions.set_gate_mode(BrainGateMode::Off).unwrap();
        assert_eq!(brain::read_gate_mode().trim(), "off");

        actions.set_gate_mode(BrainGateMode::Auto).unwrap();
        assert_eq!(brain::read_gate_mode().trim(), "auto");

        if let Some(home) = original {
            unsafe { std::env::set_var("HOME", home) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
    }
}

//! Zero-LLM "brain lite" — heuristic decisions when no local LLM is reachable.
//!
//! Without a running local LLM the brain gate could only fall back to static
//! user rules and otherwise abstained on everything, so a fresh install proved
//! no value until the user stood up ollama. This module turns the existing
//! `risk::classify_risk` tiers into an autopilot that works with no model at
//! all:
//!
//! * **Low** (reads, `cargo test`, `git status`, `ls`) → approve — the
//!   "auto-handled" wins a user sees on day one.
//! * **Critical** (`rm -rf`, force push, `DROP TABLE`) → deny — dangerous ops
//!   blocked without a model.
//! * **Medium / High** → abstain — the ambiguous middle needs judgment a
//!   heuristic can't supply, so defer to the human (or the LLM, once present).
//!
//! Deny-first user rules still run ahead of this (see `run_brain_query`), and
//! only the clearly-safe tier auto-approves, so the blast radius is small. The
//! mapping is pure and unit-tested.

use crate::brain::risk::{RiskTier, classify_risk};

/// A decision reachable without any LLM. Mirrors the action vocabulary the
/// brain gate already understands (`approve` / `deny` / `abstain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeuristicAction {
    Approve,
    Deny,
    Abstain,
}

impl HeuristicAction {
    /// Wire label, matching `client::BrainSuggestion` action strings so the
    /// plugin gate handles a heuristic decision identically to a brain one.
    pub fn label(&self) -> &'static str {
        match self {
            HeuristicAction::Approve => "approve",
            HeuristicAction::Deny => "deny",
            HeuristicAction::Abstain => "abstain",
        }
    }
}

/// A heuristic decision plus the risk tier and rationale that produced it.
#[derive(Debug, Clone)]
pub struct HeuristicDecision {
    pub action: HeuristicAction,
    /// Confidence-by-construction: high for the unambiguous tiers, zero when we
    /// abstain (so downstream "below threshold" logic reads it as a non-answer).
    pub confidence: f64,
    pub tier: RiskTier,
    pub reasoning: String,
}

/// Decide a tool call using only the risk classifier — no LLM, no network.
pub fn decide(tool: Option<&str>, command: Option<&str>) -> HeuristicDecision {
    let tier = classify_risk(tool, command);
    match tier {
        RiskTier::Low => HeuristicDecision {
            action: HeuristicAction::Approve,
            confidence: 0.9,
            tier,
            reasoning: "Known-safe operation (read-only or non-destructive command).".to_string(),
        },
        RiskTier::Critical => HeuristicDecision {
            action: HeuristicAction::Deny,
            confidence: 0.95,
            tier,
            reasoning: "Matches a known destructive pattern; blocked without a model to \
                        confirm intent."
                .to_string(),
        },
        RiskTier::Medium | RiskTier::High => HeuristicDecision {
            action: HeuristicAction::Abstain,
            confidence: 0.0,
            tier,
            reasoning: "Needs judgment a heuristic can't supply; deferring (start a local \
                        LLM for a real decision)."
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_reads_are_approved() {
        let d = decide(Some("Read"), Some("src/main.rs"));
        assert_eq!(d.action, HeuristicAction::Approve);
        assert!(d.confidence > 0.5);
    }

    #[test]
    fn safe_bash_is_approved() {
        assert_eq!(
            decide(Some("Bash"), Some("cargo test --release")).action,
            HeuristicAction::Approve
        );
        assert_eq!(
            decide(Some("Bash"), Some("git status")).action,
            HeuristicAction::Approve
        );
    }

    #[test]
    fn destructive_bash_is_denied() {
        let d = decide(Some("Bash"), Some("rm -rf /tmp/foo"));
        assert_eq!(d.action, HeuristicAction::Deny);
        assert_eq!(d.tier, RiskTier::Critical);
        assert!(d.confidence > 0.5);
    }

    #[test]
    fn force_push_is_denied() {
        assert_eq!(
            decide(Some("Bash"), Some("git push --force origin main")).action,
            HeuristicAction::Deny
        );
    }

    #[test]
    fn ordinary_edit_abstains() {
        let d = decide(Some("Edit"), Some("src/lib.rs"));
        assert_eq!(d.action, HeuristicAction::Abstain);
        assert_eq!(d.confidence, 0.0);
    }

    #[test]
    fn risky_bash_abstains_not_denies() {
        // `git push` is High risk but legitimate — defer, don't block.
        assert_eq!(
            decide(Some("Bash"), Some("git push origin main")).action,
            HeuristicAction::Abstain
        );
    }

    #[test]
    fn config_write_abstains() {
        // High tier (config/.env) is deferred, not auto-approved.
        assert_eq!(
            decide(Some("Edit"), Some(".env")).action,
            HeuristicAction::Abstain
        );
    }

    #[test]
    fn action_labels_match_gate_vocabulary() {
        assert_eq!(HeuristicAction::Approve.label(), "approve");
        assert_eq!(HeuristicAction::Deny.label(), "deny");
        assert_eq!(HeuristicAction::Abstain.label(), "abstain");
    }
}

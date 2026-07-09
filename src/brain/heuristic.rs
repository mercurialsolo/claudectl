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

/// How aggressively brain lite auto-decides, tunable via
/// `~/.claudectl/brain/heuristic-mode`. Higher tiers of auto-approval trade
/// review for autonomy; every mode except `Off` still blocks Critical ops.
///
/// | Tier     | Off | Conservative | Balanced | Aggressive |
/// |----------|-----|--------------|----------|------------|
/// | Low      | —   | defer        | approve  | approve    |
/// | Medium   | —   | defer        | defer    | approve    |
/// | High     | —   | defer        | defer    | defer      |
/// | Critical | —   | deny         | deny     | deny       |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeuristicMode {
    /// Brain lite disabled — abstain on everything (pre-brain-lite behavior).
    Off,
    /// Block Critical ops, defer everything else. Never auto-approves.
    Conservative,
    /// Auto-approve clearly-safe ops, block Critical, defer the rest.
    #[default]
    Balanced,
    /// Also auto-approve reversible (Medium) edits; still defer High, deny Critical.
    Aggressive,
}

impl HeuristicMode {
    /// Parse a mode name, case-insensitively. `None` for unknown values so
    /// callers can fall back to the default rather than silently mis-reading.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "conservative" => Some(Self::Conservative),
            "balanced" => Some(Self::Balanced),
            "aggressive" => Some(Self::Aggressive),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::Aggressive => "aggressive",
        }
    }

    /// The action this mode takes for a given risk tier — the whole policy in
    /// one pure function.
    pub fn action_for(self, tier: RiskTier) -> HeuristicAction {
        use HeuristicAction::{Abstain, Approve, Deny};
        match self {
            Self::Off => Abstain,
            Self::Conservative => match tier {
                RiskTier::Critical => Deny,
                _ => Abstain,
            },
            Self::Balanced => match tier {
                RiskTier::Critical => Deny,
                RiskTier::Low => Approve,
                _ => Abstain,
            },
            Self::Aggressive => match tier {
                RiskTier::Critical => Deny,
                RiskTier::Low | RiskTier::Medium => Approve,
                RiskTier::High => Abstain,
            },
        }
    }
}

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

/// Decide a tool call under an explicit brain-lite `mode`. Pure: the tier comes
/// from `classify_risk` and the action from `mode.action_for`, so the whole
/// policy is one composition of two pure functions.
pub fn decide_with_mode(
    tool: Option<&str>,
    command: Option<&str>,
    mode: HeuristicMode,
) -> HeuristicDecision {
    let tier = classify_risk(tool, command);
    let action = mode.action_for(tier);
    let (confidence, reasoning) = match action {
        HeuristicAction::Approve => (
            0.9,
            "Known-safe operation (read-only or non-destructive command).".to_string(),
        ),
        HeuristicAction::Deny => (
            0.95,
            "Matches a known destructive pattern; blocked without a model to confirm intent."
                .to_string(),
        ),
        HeuristicAction::Abstain => (
            0.0,
            format!(
                "Deferring — heuristic '{}' policy leaves this to the human or LLM.",
                mode.label()
            ),
        ),
    };
    HeuristicDecision {
        action,
        confidence,
        tier,
        reasoning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default (`Balanced`) policy — the behavior most tests assert.
    fn decide(tool: Option<&str>, command: Option<&str>) -> HeuristicDecision {
        decide_with_mode(tool, command, HeuristicMode::Balanced)
    }

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

    #[test]
    fn default_mode_is_balanced() {
        assert_eq!(HeuristicMode::default(), HeuristicMode::Balanced);
    }

    #[test]
    fn mode_parse_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(
            HeuristicMode::parse("AGGRESSIVE"),
            Some(HeuristicMode::Aggressive)
        );
        assert_eq!(HeuristicMode::parse(" off "), Some(HeuristicMode::Off));
        assert_eq!(HeuristicMode::parse("yolo"), None);
    }

    #[test]
    fn off_mode_abstains_on_everything_even_critical() {
        assert_eq!(
            decide_with_mode(Some("Bash"), Some("rm -rf /"), HeuristicMode::Off).action,
            HeuristicAction::Abstain
        );
        assert_eq!(
            decide_with_mode(Some("Read"), Some("x"), HeuristicMode::Off).action,
            HeuristicAction::Abstain
        );
    }

    #[test]
    fn conservative_blocks_critical_but_never_approves() {
        assert_eq!(
            decide_with_mode(
                Some("Bash"),
                Some("git push --force"),
                HeuristicMode::Conservative
            )
            .action,
            HeuristicAction::Deny
        );
        // Even a clearly-safe read is deferred, not auto-approved.
        assert_eq!(
            decide_with_mode(
                Some("Read"),
                Some("src/main.rs"),
                HeuristicMode::Conservative
            )
            .action,
            HeuristicAction::Abstain
        );
    }

    #[test]
    fn aggressive_approves_medium_but_still_defers_high() {
        // Medium (ordinary edit) auto-approves under aggressive...
        assert_eq!(
            decide_with_mode(Some("Edit"), Some("src/lib.rs"), HeuristicMode::Aggressive).action,
            HeuristicAction::Approve
        );
        // ...but High (config/.env) is still deferred, and Critical still denied.
        assert_eq!(
            decide_with_mode(Some("Edit"), Some(".env"), HeuristicMode::Aggressive).action,
            HeuristicAction::Abstain
        );
        assert_eq!(
            decide_with_mode(Some("Bash"), Some("rm -rf /tmp"), HeuristicMode::Aggressive).action,
            HeuristicAction::Deny
        );
    }

    #[test]
    fn every_non_off_mode_denies_critical() {
        for mode in [
            HeuristicMode::Conservative,
            HeuristicMode::Balanced,
            HeuristicMode::Aggressive,
        ] {
            assert_eq!(
                decide_with_mode(Some("Bash"), Some("drop table users"), mode).action,
                HeuristicAction::Deny,
                "{} should deny Critical",
                mode.label()
            );
        }
    }
}

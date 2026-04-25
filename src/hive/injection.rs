// Build hive knowledge context for brain prompt injection.
// Also provides concordance checking for trust drift.

use super::KnowledgeContent;
use super::store::HiveStore;
use super::trust::{TrustStore, TrustTier};

// ────────────────────────────────────────────────────────────────────────────
// Brain prompt context builder
// ────────────────────────────────────────────────────────────────────────────

/// Build the hive knowledge section for brain prompt injection.
/// Returns a formatted string with trust-labeled knowledge entries.
/// Units from ignored peers (trust < 0.2) are excluded unless inject_unverified is true.
/// Build the hive knowledge section for brain prompt injection.
/// `max_units` caps how many units are injected (0 = unlimited).
pub fn build_hive_context(
    store: &HiveStore,
    trust_store: &TrustStore,
    inject_unverified: bool,
    max_units: usize,
) -> String {
    let all = store.all_units();
    if all.is_empty() {
        return String::new();
    }

    // Score and sort: higher confidence * evidence first
    let mut scored: Vec<(&super::KnowledgeUnit, f64, TrustTier)> = all
        .iter()
        .filter_map(|unit| {
            let tier = trust_store
                .get(&unit.source_peer)
                .map(|t| t.tier())
                .unwrap_or(TrustTier::Suggested);

            if tier == TrustTier::Ignored && !inject_unverified {
                return None;
            }

            let score = unit.confidence * unit.evidence_count as f64;
            Some((*unit, score, tier))
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Apply max_units cap
    let limit = if max_units > 0 {
        max_units
    } else {
        scored.len()
    };

    let mut lines = Vec::new();
    let mut peer_count = std::collections::HashSet::new();

    for (unit, _, tier) in scored.iter().take(limit) {
        peer_count.insert(unit.source_peer.clone());

        let label = tier.label();
        let summary = unit.content.summary_line();
        let evidence = unit.evidence_count;
        let peer = &unit.source_peer;

        lines.push(format!(
            "- [{label}] {summary} — {evidence} decisions from {peer}"
        ));
    }

    if lines.is_empty() {
        return String::new();
    }

    let total = scored.len();
    let shown = lines.len();
    let truncated = if shown < total {
        format!(" (showing top {shown} of {total})")
    } else {
        String::new()
    };

    let header = format!(
        "## Hive Knowledge ({} peers, {} units{})\n",
        peer_count.len(),
        shown,
        truncated,
    );
    header + &lines.join("\n")
}

// ────────────────────────────────────────────────────────────────────────────
// Concordance checking for trust drift
// ────────────────────────────────────────────────────────────────────────────

/// Check if a brain decision agrees or disagrees with hive knowledge.
/// Returns a list of (peer_id, concordant) pairs for trust drift.
///
/// A decision is concordant if the hive knowledge for the same tool/command
/// recommends the same action (approve/deny) as the user's actual action.
pub fn check_concordance(
    decision_tool: Option<&str>,
    decision_command: Option<&str>,
    user_action: &str,
    store: &HiveStore,
) -> Vec<(String, bool)> {
    let tool = match decision_tool {
        Some(t) => t,
        None => return Vec::new(),
    };

    let user_approves = matches!(
        user_action,
        "accept" | "auto" | "user_approve" | "rule_approve"
    );
    let user_denies = matches!(
        user_action,
        "reject" | "deny_rule_override" | "rule_deny" | "conflict_deny"
    );

    if !user_approves && !user_denies {
        return Vec::new(); // ambiguous action, skip
    }

    let mut results = Vec::new();

    for unit in store.all_units() {
        if let KnowledgeContent::Pattern {
            tool: ref pattern_tool,
            command_pattern: ref pattern_cmd,
            ref preferred_action,
            ..
        } = unit.content
        {
            // Check if this pattern matches the decision
            if pattern_tool != tool {
                continue;
            }

            // If there's a command pattern, check if the decision command matches
            if let Some(cmd_pattern) = pattern_cmd {
                if let Some(cmd) = decision_command {
                    if !cmd.contains(cmd_pattern.as_str()) {
                        continue;
                    }
                } else {
                    continue; // pattern requires a command but none was provided
                }
            }

            // Compare actions
            let hive_approves = preferred_action == "approve";
            let concordant = (user_approves && hive_approves) || (user_denies && !hive_approves);

            results.push((unit.source_peer.clone(), concordant));
        }
    }

    results
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::{KnowledgeContent, KnowledgeScope, KnowledgeUnit};

    fn make_pattern_unit(
        id: &str,
        tool: &str,
        cmd: Option<&str>,
        action: &str,
        peer: &str,
    ) -> KnowledgeUnit {
        KnowledgeUnit {
            id: id.into(),
            scope: KnowledgeScope::Universal,
            category: crate::hive::KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Pattern {
                tool: tool.into(),
                command_pattern: cmd.map(|s| s.into()),
                preferred_action: action.into(),
                accept_rate: 0.9,
                sample_count: 10,
                conditions: vec![],
            },
            evidence_count: 10,
            confidence: 0.9,
            source_peer: peer.into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
        }
    }

    fn empty_store() -> HiveStore {
        HiveStore::load_from(std::path::Path::new("/nonexistent"))
    }

    #[test]
    fn build_context_empty_store() {
        let store = empty_store();
        let trust_store = TrustStore::load_with_default(0.5);
        let ctx = build_hive_context(&store, &trust_store, true, 0);
        assert!(ctx.is_empty());
    }

    #[test]
    fn build_context_with_units() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("cargo test"),
            "approve",
            "peer-a",
        ));
        store.insert(make_pattern_unit("ku_2", "Write", None, "deny", "peer-b"));

        let trust_store = TrustStore::load_with_default(0.5);
        let ctx = build_hive_context(&store, &trust_store, true, 0);

        assert!(ctx.contains("## Hive Knowledge"));
        assert!(ctx.contains("2 units"));
        assert!(ctx.contains("[hive, suggested]")); // default trust 0.5 = suggested
    }

    #[test]
    fn build_context_confirmed_tier() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("test"),
            "approve",
            "peer-a",
        ));

        let mut trust_store = TrustStore::load_with_default(0.5);
        trust_store.set_trust("peer-a", 0.9);

        let ctx = build_hive_context(&store, &trust_store, true, 0);
        assert!(ctx.contains("[hive]")); // 0.9 = Confirmed
        assert!(!ctx.contains("[hive, suggested]"));
    }

    #[test]
    fn build_context_excludes_ignored_unless_flag() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("test"),
            "approve",
            "peer-a",
        ));

        let mut trust_store = TrustStore::load_with_default(0.5);
        trust_store.set_trust("peer-a", 0.1); // Ignored tier

        // Without inject_unverified: excluded
        let ctx = build_hive_context(&store, &trust_store, false, 0);
        assert!(ctx.is_empty());

        // With inject_unverified: included
        let ctx = build_hive_context(&store, &trust_store, true, 0);
        assert!(ctx.contains("[hive, ignored]"));
    }

    #[test]
    fn concordance_approves_match() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("cargo test"),
            "approve",
            "peer-a",
        ));

        let results = check_concordance(Some("Bash"), Some("cargo test --all"), "accept", &store);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "peer-a");
        assert!(results[0].1); // concordant
    }

    #[test]
    fn concordance_deny_match() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("rm -rf"),
            "deny",
            "peer-a",
        ));

        let results = check_concordance(Some("Bash"), Some("rm -rf /tmp"), "reject", &store);
        assert_eq!(results.len(), 1);
        assert!(results[0].1); // concordant — both deny
    }

    #[test]
    fn concordance_disagree() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("cargo test"),
            "approve",
            "peer-a",
        ));

        // User rejects what hive says to approve = discordant
        let results = check_concordance(Some("Bash"), Some("cargo test"), "reject", &store);
        assert_eq!(results.len(), 1);
        assert!(!results[0].1); // discordant
    }

    #[test]
    fn concordance_no_tool_returns_empty() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("test"),
            "approve",
            "peer-a",
        ));

        let results = check_concordance(None, None, "accept", &store);
        assert!(results.is_empty());
    }

    #[test]
    fn concordance_wrong_tool_no_match() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("test"),
            "approve",
            "peer-a",
        ));

        let results = check_concordance(Some("Write"), Some("test"), "accept", &store);
        assert!(results.is_empty());
    }

    #[test]
    fn concordance_ambiguous_action_skipped() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("test"),
            "approve",
            "peer-a",
        ));

        // "user_input" is neither approve nor deny
        let results = check_concordance(Some("Bash"), Some("test"), "user_input", &store);
        assert!(results.is_empty());
    }
}

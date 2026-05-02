// Build hive knowledge context for brain prompt injection.
// Also provides concordance checking for trust drift.

use super::feedback::passes_rollout;
use super::store::HiveStore;
use super::trust::{TrustStore, TrustTier};
use super::{KnowledgeContent, effective_confidence, epoch_secs};

/// Effective confidence floor below which a unit is too decayed to inject,
/// regardless of peer trust. Keeps stale knowledge out of the prompt even when
/// the originating peer is still trusted.
const DECAYED_NOISE_FLOOR: f64 = 0.1;

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
    build_hive_context_for_session(store, trust_store, inject_unverified, max_units, None).0
}

/// Same as [`build_hive_context`] but applies #223 rollout sampling per session
/// pid and returns the IDs of units that ended up in the prompt. Callers
/// should pass `Some(pid)` to opt into Canary/Staged sampling and feed the
/// returned IDs into `feedback::stash_pending` so outcomes can be attributed.
pub fn build_hive_context_for_session(
    store: &HiveStore,
    trust_store: &TrustStore,
    inject_unverified: bool,
    max_units: usize,
    pid: Option<u32>,
) -> (String, Vec<String>) {
    let all = store.all_units();
    if all.is_empty() {
        return (String::new(), Vec::new());
    }

    let now = epoch_secs();

    // Score and sort: higher (effective) confidence * evidence first.
    // Effective confidence applies time-based decay (#224) so stale knowledge
    // sinks even when its peer is still Confirmed.
    // Skip Skill/Command/HookConfig — they aren't decision guidance for the brain.
    // Users discover them via `claudectl hive shared`.
    let mut scored: Vec<(&super::KnowledgeUnit, f64, TrustTier)> = all
        .iter()
        .filter_map(|unit| {
            // Skip artifact types — not relevant for brain decision-making
            if matches!(
                &unit.content,
                KnowledgeContent::Skill { .. }
                    | KnowledgeContent::Command { .. }
                    | KnowledgeContent::HookConfig { .. }
            ) {
                return None;
            }

            let tier = trust_store
                .get(&unit.source_peer)
                .map(|t| t.tier())
                .unwrap_or(TrustTier::Suggested);

            if tier == TrustTier::Ignored && !inject_unverified {
                return None;
            }

            let eff = effective_confidence(unit, now);
            // Drop heavily-decayed units even if peer trust is fine.
            // `inject_unverified` keeps them visible (debugging / cold start).
            if !inject_unverified && eff < DECAYED_NOISE_FLOOR {
                return None;
            }

            // #223 rollout sampling — Draft units never appear, Canary in
            // ~10% of prompts, Staged in ~50%, Live always. When pid is None
            // (CLI listings, tests), every state passes.
            if !passes_rollout(unit, pid) {
                return None;
            }

            let score = eff * unit.evidence_count as f64;
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
    let mut injected_ids: Vec<String> = Vec::new();

    for (unit, _, tier) in scored.iter().take(limit) {
        peer_count.insert(unit.source_peer.clone());
        injected_ids.push(unit.id.clone());

        let label = tier.label();
        let summary = unit.content.summary_line();
        let evidence = unit.evidence_count;
        let peer = &unit.source_peer;
        let stale_tag = if super::is_stale(unit, now) {
            " [stale]"
        } else {
            ""
        };

        lines.push(format!(
            "- [{label}] {summary} — {evidence} decisions from {peer}{stale_tag}"
        ));

        // #221 conflict-aware: expand cluster variants inline so the LLM
        // sees the alternatives rather than just a single collapsed pattern.
        // Cap at top 3 variants ranked by evidence to keep prompt size bounded.
        if let KnowledgeContent::ApproachCluster { variants, .. } = &unit.content {
            let mut sorted = variants.clone();
            sorted.sort_by_key(|v| std::cmp::Reverse(v.evidence));
            for (i, v) in sorted.iter().take(3).enumerate() {
                let label_letter = (b'A' + i as u8) as char;
                let cond = if v.conditions.is_empty() {
                    String::new()
                } else {
                    format!(" [when: {}]", v.conditions.join(", "))
                };
                lines.push(format!(
                    "    ({label_letter}) {} — n={}{cond}",
                    v.approach_summary, v.evidence
                ));
            }
            if sorted.len() > 3 {
                lines.push(format!("    (… {} more variants)", sorted.len() - 3));
            }
        }
    }

    if lines.is_empty() {
        return (String::new(), Vec::new());
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
    (header + &lines.join("\n"), injected_ids)
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
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
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

    fn make_cluster_unit(
        id: &str,
        peer: &str,
        problem_key: &str,
        variants: Vec<super::super::ApproachVariant>,
    ) -> super::super::KnowledgeUnit {
        let evidence: u32 = variants.iter().map(|v| v.evidence).sum();
        super::super::KnowledgeUnit {
            id: id.into(),
            scope: super::super::KnowledgeScope::Universal,
            category: super::super::KnowledgeCategory::Technique,
            content: super::super::KnowledgeContent::ApproachCluster {
                problem_key: problem_key.into(),
                variants,
            },
            evidence_count: evidence,
            confidence: 0.5,
            source_peer: peer.into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        }
    }

    #[test]
    fn build_context_expands_cluster_variants_inline() {
        let mut store = empty_store();
        store.insert(make_cluster_unit(
            "ku_cluster",
            "peer-a",
            "Bash:git push",
            vec![
                super::super::ApproachVariant {
                    approach_summary: "approve (90%)".into(),
                    conditions: vec!["cost_below(1.0)".into()],
                    evidence: 12,
                    contributing_peers: vec!["peer-a".into()],
                    outcome_ref: None,
                },
                super::super::ApproachVariant {
                    approach_summary: "deny (15%)".into(),
                    conditions: vec!["cost_above(1.0)".into()],
                    evidence: 6,
                    contributing_peers: vec!["peer-b".into()],
                    outcome_ref: None,
                },
            ],
        ));

        let trust_store = TrustStore::load_with_default(0.5);
        let ctx = build_hive_context(&store, &trust_store, true, 0);
        assert!(ctx.contains("cluster: Bash:git push"));
        assert!(ctx.contains("(A) approve"));
        assert!(ctx.contains("(B) deny"));
        // Higher evidence first → approve labelled (A).
        let approve_idx = ctx.find("(A)").expect("A label present");
        let deny_idx = ctx.find("(B)").expect("B label present");
        assert!(
            approve_idx < deny_idx,
            "approve must precede deny in label order"
        );
        // Conditions are inlined.
        assert!(ctx.contains("cost_below(1.0)"));
    }

    #[test]
    fn build_context_caps_cluster_variants_at_three() {
        let mut store = empty_store();
        let variants: Vec<super::super::ApproachVariant> = (0..5)
            .map(|i| super::super::ApproachVariant {
                approach_summary: format!("variant {i}"),
                conditions: vec![],
                evidence: 5 + i,
                contributing_peers: vec!["peer".into()],
                outcome_ref: None,
            })
            .collect();
        store.insert(make_cluster_unit("ku_big", "peer", "Bash:noisy", variants));

        let trust_store = TrustStore::load_with_default(0.5);
        let ctx = build_hive_context(&store, &trust_store, true, 0);
        assert!(ctx.contains("(A)"));
        assert!(ctx.contains("(B)"));
        assert!(ctx.contains("(C)"));
        assert!(!ctx.contains("(D)"), "should cap at 3");
        assert!(ctx.contains("2 more variants"), "should note overflow");
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
    fn build_context_excludes_skill_bodies() {
        let mut store = empty_store();
        store.insert(make_pattern_unit(
            "ku_1",
            "Bash",
            Some("cargo test"),
            "approve",
            "peer-a",
        ));
        // Add a skill — should NOT appear in brain context
        store.insert(KnowledgeUnit {
            id: "ku_skill_1".into(),
            scope: KnowledgeScope::Universal,
            category: crate::hive::KnowledgeCategory::Technique,
            content: KnowledgeContent::Skill {
                name: "Session Monitoring".into(),
                description: "Monitors sessions".into(),
                version: "0.31.0".into(),
                body: "Full skill body here".into(),
                requires: crate::hive::ArtifactRequires::default(),
            },
            evidence_count: 1,
            confidence: 1.0,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        });

        let trust_store = TrustStore::load_with_default(0.5);
        let ctx = build_hive_context(&store, &trust_store, true, 0);

        // Should contain the pattern unit but NOT the skill
        assert!(ctx.contains("Bash"));
        assert!(!ctx.contains("Session Monitoring"));
        assert!(!ctx.contains("skill"));
        // Should show 1 unit, not 2
        assert!(ctx.contains("1 units"));
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

    // ── Staleness / decay (#224) ───────────────────────────────────────

    #[test]
    fn build_context_drops_decayed_units() {
        let mut store = empty_store();
        // Long-stale unit: 1.0 confidence × 0.9^N decay below floor.
        // Pattern default interval = 30 days = 2_592_000 s.
        // 30 intervals overdue → 0.9^31 ≈ 0.038, well below floor 0.1.
        let mut unit = make_pattern_unit("ku_old", "Bash", Some("ls"), "approve", "peer-a");
        unit.confidence = 1.0;
        unit.last_validated_at = 0;
        // Re-insert with the staleness baked in.
        store.insert(unit);

        let trust_store = TrustStore::load_with_default(0.9); // Confirmed peer
        // Without inject_unverified, decayed units are dropped
        let now_far_future: u64 = 100 * 365 * 86_400; // ~100 years
        // The function uses real time, so we can't manipulate `now` directly;
        // instead use a unit whose last_validated_at is in the deep past.
        let _ = now_far_future;
        let ctx = build_hive_context(&store, &trust_store, false, 0);
        assert!(
            ctx.is_empty(),
            "decayed unit should be filtered when inject_unverified=false; got: {ctx}"
        );

        // With inject_unverified=true, the unit reappears
        let ctx_unverified = build_hive_context(&store, &trust_store, true, 0);
        assert!(ctx_unverified.contains("ku_") || ctx_unverified.contains("Bash"));
    }

    #[test]
    fn build_context_marks_stale_units() {
        let mut store = empty_store();
        let mut unit = make_pattern_unit("ku_stale", "Bash", Some("ls"), "approve", "peer-a");
        unit.confidence = 0.9;
        // 60 days ago: stale (default Pattern interval is 30 days), but only
        // 1 interval overdue → effective = 0.81 (above noise floor 0.1).
        unit.last_validated_at = super::epoch_secs().saturating_sub(60 * 86_400);
        store.insert(unit);

        let trust_store = TrustStore::load_with_default(0.9);
        let ctx = build_hive_context(&store, &trust_store, false, 0);
        assert!(ctx.contains("[stale]"), "expected stale tag in: {ctx}");
    }
}

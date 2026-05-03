// Discoverability queries (#227) — lets users browse what the hive knows
// before it auto-injects, and lets new peers receive a curated welcome
// snapshot rather than the firehose.
//
// All functions are pure over a `HiveStore` snapshot and take a separate
// `TrustStore` reference for tier filtering. No I/O, no mutation.

use std::collections::HashMap;

use super::store::HiveStore;
use super::trust::{TrustStore, TrustTier};
use super::{KnowledgeContent, KnowledgeUnit, effective_confidence, epoch_secs};

// ────────────────────────────────────────────────────────────────────────────
// Welcome snapshot defaults
// ────────────────────────────────────────────────────────────────────────────

/// Min effective confidence a unit must clear to appear in a welcome snapshot.
pub const WELCOME_MIN_CONFIDENCE: f64 = 0.7;

/// Cap on how many units appear in a welcome snapshot. Enough to be useful,
/// small enough that a fresh peer isn't drowned.
pub const WELCOME_MAX_UNITS: usize = 50;

// ────────────────────────────────────────────────────────────────────────────
// Explore filter
// ────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ExploreFilter<'a> {
    pub category: Option<&'a str>,
    pub scope: Option<&'a super::KnowledgeScope>,
    pub peer: Option<&'a str>,
    pub min_confidence: f64,
    pub include_artifacts: bool,
}

impl ExploreFilter<'_> {
    fn matches(&self, unit: &KnowledgeUnit, eff_conf: f64) -> bool {
        if !self.include_artifacts
            && matches!(
                &unit.content,
                KnowledgeContent::Skill { .. }
                    | KnowledgeContent::Command { .. }
                    | KnowledgeContent::HookConfig { .. }
            )
        {
            return false;
        }
        if let Some(c) = self.category {
            if unit.category.label() != c {
                return false;
            }
        }
        if let Some(s) = self.scope {
            if unit.scope != *s {
                return false;
            }
        }
        if let Some(p) = self.peer {
            if unit.source_peer != p {
                return false;
            }
        }
        if eff_conf < self.min_confidence {
            return false;
        }
        true
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Explore — ranked list of available knowledge
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExploreRow<'a> {
    pub unit: &'a KnowledgeUnit,
    pub effective_confidence: f64,
    pub tier: TrustTier,
}

/// Build a ranked list of knowledge units that match the filter.
///
/// Sort order: trust tier desc (Confirmed first), then effective_confidence
/// desc, then evidence_count desc — matches what a curious user expects.
pub fn explore<'a>(
    store: &'a HiveStore,
    trust: &TrustStore,
    filter: &ExploreFilter<'_>,
    limit: usize,
) -> Vec<ExploreRow<'a>> {
    let now = epoch_secs();
    let mut rows: Vec<ExploreRow<'a>> = store
        .all_units()
        .into_iter()
        .filter_map(|unit| {
            let eff = effective_confidence(unit, now);
            if !filter.matches(unit, eff) {
                return None;
            }
            let tier = trust
                .get(&unit.source_peer)
                .map(|t| t.tier())
                .unwrap_or(TrustTier::Suggested);
            Some(ExploreRow {
                unit,
                effective_confidence: eff,
                tier,
            })
        })
        .collect();

    rows.sort_by(|a, b| {
        tier_rank(b.tier)
            .cmp(&tier_rank(a.tier))
            .then_with(|| {
                b.effective_confidence
                    .partial_cmp(&a.effective_confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.unit.evidence_count.cmp(&a.unit.evidence_count))
    });
    if limit > 0 && rows.len() > limit {
        rows.truncate(limit);
    }
    rows
}

fn tier_rank(tier: TrustTier) -> u8 {
    match tier {
        TrustTier::Confirmed => 4,
        TrustTier::Suggested => 3,
        TrustTier::Unverified => 2,
        TrustTier::Ignored => 1,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Experts — peers ranked by category
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExpertRow {
    pub peer_id: String,
    pub category: String,
    pub unit_count: u32,
    pub avg_confidence: f64,
    pub tier: TrustTier,
}

/// Rank peers by their average effective confidence in a category. Useful
/// for "who do I learn from about Rust?" questions.
pub fn experts(
    store: &HiveStore,
    trust: &TrustStore,
    category: &str,
    limit: usize,
) -> Vec<ExpertRow> {
    let now = epoch_secs();
    let mut by_peer: HashMap<String, (u32, f64)> = HashMap::new();
    for unit in store.all_units() {
        if unit.category.label() != category {
            continue;
        }
        // Skip artifact types — they don't contribute to "expertise" ranking.
        if matches!(
            &unit.content,
            KnowledgeContent::Skill { .. }
                | KnowledgeContent::Command { .. }
                | KnowledgeContent::HookConfig { .. }
        ) {
            continue;
        }
        let entry = by_peer.entry(unit.source_peer.clone()).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += effective_confidence(unit, now);
    }

    let mut rows: Vec<ExpertRow> = by_peer
        .into_iter()
        .map(|(peer_id, (count, sum_conf))| {
            let tier = trust
                .get(&peer_id)
                .map(|t| t.tier())
                .unwrap_or(TrustTier::Suggested);
            ExpertRow {
                peer_id,
                category: category.to_string(),
                unit_count: count,
                avg_confidence: sum_conf / count as f64,
                tier,
            }
        })
        .collect();

    rows.sort_by(|a, b| {
        tier_rank(b.tier)
            .cmp(&tier_rank(a.tier))
            .then_with(|| {
                b.avg_confidence
                    .partial_cmp(&a.avg_confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.unit_count.cmp(&a.unit_count))
    });
    if limit > 0 && rows.len() > limit {
        rows.truncate(limit);
    }
    rows
}

// ────────────────────────────────────────────────────────────────────────────
// Welcome snapshot — what we'd send a fresh peer
// ────────────────────────────────────────────────────────────────────────────

/// Build the curated welcome snapshot. Returns units a fresh peer should
/// receive on first contact: only confidence ≥ WELCOME_MIN_CONFIDENCE,
/// trust tier ≥ Suggested, in the Live rollout state, capped at
/// WELCOME_MAX_UNITS sorted by effective_confidence desc.
///
/// Skill/Command/HookConfig artifacts are excluded — those go through the
/// existing `hive shared` / accept flow, not the brain-decision welcome path.
pub fn welcome_snapshot<'a>(store: &'a HiveStore, trust: &TrustStore) -> Vec<&'a KnowledgeUnit> {
    let now = epoch_secs();
    let mut rows: Vec<(&'a KnowledgeUnit, f64)> = store
        .all_units()
        .into_iter()
        .filter(|u| {
            // Only proven-rollout units make the welcome cut.
            if !matches!(u.injection_state, super::InjectionState::Live) {
                return false;
            }
            if matches!(
                &u.content,
                KnowledgeContent::Skill { .. }
                    | KnowledgeContent::Command { .. }
                    | KnowledgeContent::HookConfig { .. }
            ) {
                return false;
            }
            let tier = trust
                .get(&u.source_peer)
                .map(|t| t.tier())
                .unwrap_or(TrustTier::Suggested);
            if tier_rank(tier) < tier_rank(TrustTier::Suggested) {
                return false;
            }
            true
        })
        .filter_map(|u| {
            let eff = effective_confidence(u, now);
            if eff >= WELCOME_MIN_CONFIDENCE {
                Some((u, eff))
            } else {
                None
            }
        })
        .collect();

    rows.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.evidence_count.cmp(&a.0.evidence_count))
    });
    rows.into_iter()
        .take(WELCOME_MAX_UNITS)
        .map(|(u, _)| u)
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::{
        ApproachVariant, InjectionState, InjectionStats, KnowledgeCategory, KnowledgeContent,
        KnowledgeScope, KnowledgeUnit,
    };

    #[allow(clippy::too_many_arguments)]
    fn unit(
        id: &str,
        peer: &str,
        cmd: &str,
        category: KnowledgeCategory,
        scope: KnowledgeScope,
        confidence: f64,
        evidence: u32,
        state: InjectionState,
    ) -> KnowledgeUnit {
        KnowledgeUnit {
            id: id.into(),
            scope,
            category,
            content: KnowledgeContent::Pattern {
                tool: "Bash".into(),
                command_pattern: Some(cmd.into()),
                preferred_action: "approve".into(),
                accept_rate: confidence,
                sample_count: evidence,
                conditions: vec![],
            },
            evidence_count: evidence,
            confidence,
            source_peer: peer.into(),
            originated_at: 0,
            last_validated_at: epoch_secs(), // fresh — no decay
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: state,
            injection_stats: InjectionStats::default(),
            sharing_consent: None,
        }
    }

    fn skill_unit(id: &str, peer: &str, name: &str) -> KnowledgeUnit {
        KnowledgeUnit {
            id: id.into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::Technique,
            content: KnowledgeContent::Skill {
                name: name.into(),
                description: "test".into(),
                version: "1.0".into(),
                body: "body".into(),
                requires: super::super::ArtifactRequires::default(),
            },
            evidence_count: 1,
            confidence: 1.0,
            source_peer: peer.into(),
            originated_at: 0,
            last_validated_at: epoch_secs(),
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: InjectionState::Live,
            injection_stats: InjectionStats::default(),
            sharing_consent: None,
        }
    }

    fn empty_store() -> HiveStore {
        HiveStore::load_from(std::path::Path::new("/nonexistent"))
    }

    fn pop_store(units: Vec<KnowledgeUnit>) -> HiveStore {
        let mut store = empty_store();
        for u in units {
            store.insert(u);
        }
        store
    }

    #[test]
    fn explore_filters_by_category() {
        let store = pop_store(vec![
            unit(
                "ku_bp",
                "peer-a",
                "lsa",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
            unit(
                "ku_tech",
                "peer-b",
                "lsb",
                KnowledgeCategory::Technique,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
        ]);
        let trust = TrustStore::empty(0.6);
        let filter = ExploreFilter {
            category: Some("best_practice"),
            ..Default::default()
        };
        let rows = explore(&store, &trust, &filter, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].unit.id, "ku_bp");
    }

    #[test]
    fn explore_filters_by_scope() {
        let store = pop_store(vec![
            unit(
                "ku_uni",
                "peer-a",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
            unit(
                "ku_rust",
                "peer-b",
                "y",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Language("rust".into()),
                0.9,
                10,
                InjectionState::Live,
            ),
        ]);
        let trust = TrustStore::empty(0.6);
        let scope = KnowledgeScope::Language("rust".into());
        let filter = ExploreFilter {
            scope: Some(&scope),
            ..Default::default()
        };
        let rows = explore(&store, &trust, &filter, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].unit.id, "ku_rust");
    }

    #[test]
    fn explore_min_confidence() {
        let store = pop_store(vec![
            unit(
                "ku_high",
                "peer-a",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
            unit(
                "ku_low",
                "peer-b",
                "y",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.4,
                10,
                InjectionState::Live,
            ),
        ]);
        let trust = TrustStore::empty(0.6);
        let filter = ExploreFilter {
            min_confidence: 0.7,
            ..Default::default()
        };
        let rows = explore(&store, &trust, &filter, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].unit.id, "ku_high");
    }

    #[test]
    fn explore_excludes_artifacts_by_default() {
        let store = pop_store(vec![
            unit(
                "ku_pat",
                "peer-a",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
            skill_unit("ku_skill", "peer-a", "session-monitoring"),
        ]);
        let trust = TrustStore::empty(0.6);
        let rows = explore(&store, &trust, &ExploreFilter::default(), 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].unit.id, "ku_pat");
    }

    #[test]
    fn explore_includes_artifacts_when_requested() {
        let store = pop_store(vec![
            unit(
                "ku_pat",
                "peer-a",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
            skill_unit("ku_skill", "peer-a", "session-monitoring"),
        ]);
        let trust = TrustStore::empty(0.6);
        let filter = ExploreFilter {
            include_artifacts: true,
            ..Default::default()
        };
        let rows = explore(&store, &trust, &filter, 0);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn explore_sorts_confirmed_before_suggested() {
        let store = pop_store(vec![
            unit(
                "ku_a",
                "peer-suggested",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.95,
                10,
                InjectionState::Live,
            ),
            unit(
                "ku_b",
                "peer-confirmed",
                "y",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.7,
                10,
                InjectionState::Live,
            ),
        ]);
        let mut trust = TrustStore::empty(0.6);
        trust.set_trust("peer-confirmed", 0.9);
        // peer-suggested left at default 0.6 = Suggested

        let rows = explore(&store, &trust, &ExploreFilter::default(), 0);
        // Even though ku_a has higher confidence, ku_b's Confirmed tier wins.
        assert_eq!(rows[0].unit.id, "ku_b");
        assert_eq!(rows[1].unit.id, "ku_a");
    }

    #[test]
    fn experts_ranks_by_avg_confidence() {
        let store = pop_store(vec![
            unit(
                "ku_a1",
                "peer-a",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
            unit(
                "ku_a2",
                "peer-a",
                "y",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.85,
                10,
                InjectionState::Live,
            ),
            unit(
                "ku_b1",
                "peer-b",
                "z",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.6,
                10,
                InjectionState::Live,
            ),
            unit(
                "ku_other_cat",
                "peer-a",
                "w",
                KnowledgeCategory::Technique,
                KnowledgeScope::Universal,
                0.95,
                10,
                InjectionState::Live,
            ),
        ]);
        let trust = TrustStore::empty(0.6);
        let rows = experts(&store, &trust, "best_practice", 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].peer_id, "peer-a");
        assert_eq!(rows[0].unit_count, 2);
        assert!((rows[0].avg_confidence - 0.875).abs() < 1e-9);
        assert_eq!(rows[1].peer_id, "peer-b");
    }

    #[test]
    fn experts_skips_artifact_types() {
        let store = pop_store(vec![skill_unit("ku_skill", "peer-a", "test")]);
        let trust = TrustStore::empty(0.6);
        let rows = experts(&store, &trust, "technique", 0);
        // Skills are excluded from expertise ranking
        assert!(rows.is_empty());
    }

    #[test]
    fn welcome_snapshot_filters_low_confidence() {
        let store = pop_store(vec![
            unit(
                "ku_high",
                "peer-a",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
            unit(
                "ku_low",
                "peer-a",
                "y",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.5, // below WELCOME_MIN_CONFIDENCE
                10,
                InjectionState::Live,
            ),
        ]);
        let trust = TrustStore::empty(0.6);
        let snap = welcome_snapshot(&store, &trust);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "ku_high");
    }

    #[test]
    fn welcome_snapshot_excludes_canary_state() {
        let store = pop_store(vec![
            unit(
                "ku_canary",
                "peer-a",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Canary,
            ),
            unit(
                "ku_live",
                "peer-a",
                "y",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
        ]);
        let trust = TrustStore::empty(0.6);
        let snap = welcome_snapshot(&store, &trust);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "ku_live");
    }

    #[test]
    fn welcome_snapshot_excludes_unverified_peers() {
        let store = pop_store(vec![unit(
            "ku_a",
            "peer-untrusted",
            "x",
            KnowledgeCategory::BestPractice,
            KnowledgeScope::Universal,
            0.9,
            10,
            InjectionState::Live,
        )]);
        let mut trust = TrustStore::empty(0.6);
        trust.set_trust("peer-untrusted", 0.3); // Unverified tier
        let snap = welcome_snapshot(&store, &trust);
        assert!(snap.is_empty());
    }

    #[test]
    fn welcome_snapshot_caps_at_max_units() {
        let mut units = Vec::new();
        for i in 0..(WELCOME_MAX_UNITS + 20) {
            units.push(unit(
                &format!("ku_{i}"),
                "peer-a",
                &format!("cmd_{i}"),
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ));
        }
        let store = pop_store(units);
        let trust = TrustStore::empty(0.6);
        let snap = welcome_snapshot(&store, &trust);
        assert_eq!(snap.len(), WELCOME_MAX_UNITS);
    }

    #[test]
    fn welcome_snapshot_excludes_artifacts() {
        let store = pop_store(vec![
            unit(
                "ku_pat",
                "peer-a",
                "x",
                KnowledgeCategory::BestPractice,
                KnowledgeScope::Universal,
                0.9,
                10,
                InjectionState::Live,
            ),
            skill_unit("ku_skill", "peer-a", "test"),
        ]);
        let trust = TrustStore::empty(0.6);
        let snap = welcome_snapshot(&store, &trust);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "ku_pat");
    }

    #[test]
    fn explore_with_cluster_units() {
        // Clusters should appear in explore even though their semantic_key
        // and content type differ from Pattern. Sanity check.
        let cluster = KnowledgeUnit {
            id: "ku_cluster".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::Technique,
            content: KnowledgeContent::ApproachCluster {
                problem_key: "Bash:test".into(),
                variants: vec![ApproachVariant {
                    approach_summary: "approve".into(),
                    conditions: vec![],
                    evidence: 5,
                    contributing_peers: vec!["peer-a".into()],
                    outcome_ref: None,
                }],
            },
            evidence_count: 5,
            confidence: 0.8,
            source_peer: "peer-a".into(),
            originated_at: 0,
            last_validated_at: epoch_secs(),
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: InjectionState::Live,
            injection_stats: InjectionStats::default(),
            sharing_consent: None,
        };
        let store = pop_store(vec![cluster]);
        let trust = TrustStore::empty(0.6);
        let rows = explore(&store, &trust, &ExploreFilter::default(), 0);
        assert_eq!(rows.len(), 1);
    }
}

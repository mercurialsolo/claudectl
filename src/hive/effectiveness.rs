// Knowledge effectiveness queries (#225).
//
// Reads `injection_stats` populated by `hive::feedback` and answers:
//   - Which units actually influenced accepted decisions?
//   - Which peers' contributions hold up under outcomes?
//   - Which units ride along in every prompt without measurable impact?
//
// All functions are pure over a `HiveStore` snapshot — no side effects.

use std::collections::HashMap;

use super::store::HiveStore;
use super::{InjectionState, KnowledgeUnit};

// ────────────────────────────────────────────────────────────────────────────
// Per-unit row
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UnitEffectiveness<'a> {
    pub unit: &'a KnowledgeUnit,
    pub injected: u64,
    pub accepted: u64,
    pub overridden: u64,
    /// `accepted / (accepted + overridden)`, 0.0 when no decided outcomes yet.
    pub win_rate: f64,
    /// Total decided outcomes (accepted + overridden).
    pub decided: u64,
}

impl<'a> UnitEffectiveness<'a> {
    fn from_unit(unit: &'a KnowledgeUnit) -> Self {
        let stats = &unit.injection_stats;
        UnitEffectiveness {
            unit,
            injected: stats.injected_count,
            accepted: stats.accepted_count,
            overridden: stats.overridden_count,
            win_rate: stats.win_rate(),
            decided: stats.decided(),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Per-peer aggregate
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct PeerEffectiveness {
    pub peer_id: String,
    pub unit_count: u32,
    pub total_injected: u64,
    pub total_accepted: u64,
    pub total_overridden: u64,
    /// Weighted by total decided outcomes — peers with more signal contribute more.
    pub weighted_win_rate: f64,
    pub dead_weight_count: u32,
}

impl PeerEffectiveness {
    pub fn total_decided(&self) -> u64 {
        self.total_accepted + self.total_overridden
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Dead-weight thresholds
// ────────────────────────────────────────────────────────────────────────────

/// Default minimum injections before a unit can be flagged as dead-weight.
/// At least this many prompts have included the unit, so we'd expect *some*
/// outcome attribution by now.
pub const DEAD_WEIGHT_MIN_INJECTED: u64 = 50;

/// Default maximum decided outcomes for dead-weight classification. A unit
/// with this few decided outcomes (relative to MIN_INJECTED injections) is
/// riding along in prompts but rarely matching live decisions.
pub const DEAD_WEIGHT_MAX_DECIDED: u64 = 5;

// ────────────────────────────────────────────────────────────────────────────
// Queries
// ────────────────────────────────────────────────────────────────────────────

/// Build effectiveness rows for every unit in the store, optionally filtered.
#[derive(Default)]
pub struct EffectivenessFilter<'a> {
    pub peer: Option<&'a str>,
    pub category: Option<&'a str>,
    pub state: Option<InjectionState>,
    pub min_decided: u64,
}

impl EffectivenessFilter<'_> {
    fn matches(&self, unit: &KnowledgeUnit) -> bool {
        if let Some(p) = self.peer {
            if unit.source_peer != p {
                return false;
            }
        }
        if let Some(c) = self.category {
            if unit.category.label() != c {
                return false;
            }
        }
        if let Some(s) = self.state {
            if unit.injection_state != s {
                return false;
            }
        }
        if unit.injection_stats.decided() < self.min_decided {
            return false;
        }
        true
    }
}

/// Compute per-unit effectiveness, sorted by win_rate desc then decided desc.
/// Units with no decided outcomes go to the bottom regardless of win_rate.
pub fn unit_effectiveness<'a>(
    store: &'a HiveStore,
    filter: &EffectivenessFilter<'_>,
) -> Vec<UnitEffectiveness<'a>> {
    let mut rows: Vec<UnitEffectiveness<'a>> = store
        .all_units()
        .into_iter()
        .filter(|u| filter.matches(u))
        .map(UnitEffectiveness::from_unit)
        .collect();
    rows.sort_by(|a, b| {
        // Decided==0 → push to bottom. Otherwise win_rate desc, then decided desc.
        match (a.decided, b.decided) {
            (0, 0) => std::cmp::Ordering::Equal,
            (0, _) => std::cmp::Ordering::Greater,
            (_, 0) => std::cmp::Ordering::Less,
            _ => b
                .win_rate
                .partial_cmp(&a.win_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.decided.cmp(&a.decided)),
        }
    });
    rows
}

/// Compute per-peer aggregates, sorted by weighted_win_rate desc.
pub fn peer_effectiveness(store: &HiveStore) -> Vec<PeerEffectiveness> {
    let mut by_peer: HashMap<String, PeerEffectiveness> = HashMap::new();
    for unit in store.all_units() {
        let entry = by_peer
            .entry(unit.source_peer.clone())
            .or_insert_with(|| PeerEffectiveness {
                peer_id: unit.source_peer.clone(),
                ..Default::default()
            });
        entry.unit_count += 1;
        entry.total_injected += unit.injection_stats.injected_count;
        entry.total_accepted += unit.injection_stats.accepted_count;
        entry.total_overridden += unit.injection_stats.overridden_count;
        if is_dead_weight(unit, DEAD_WEIGHT_MIN_INJECTED, DEAD_WEIGHT_MAX_DECIDED) {
            entry.dead_weight_count += 1;
        }
    }

    // Compute weighted win rate per peer
    for entry in by_peer.values_mut() {
        let decided = entry.total_decided();
        entry.weighted_win_rate = if decided == 0 {
            0.0
        } else {
            entry.total_accepted as f64 / decided as f64
        };
    }

    let mut out: Vec<PeerEffectiveness> = by_peer.into_values().collect();
    out.sort_by(|a, b| {
        // Peers with no decided outcomes go to the bottom.
        match (a.total_decided(), b.total_decided()) {
            (0, 0) => std::cmp::Ordering::Equal,
            (0, _) => std::cmp::Ordering::Greater,
            (_, 0) => std::cmp::Ordering::Less,
            _ => b
                .weighted_win_rate
                .partial_cmp(&a.weighted_win_rate)
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    });
    out
}

/// A unit is dead-weight when it's been injected enough times to expect
/// outcome signal, but accumulated too few decided outcomes. These ride along
/// in prompts without measurably influencing decisions — candidates for
/// demotion or removal.
pub fn is_dead_weight(unit: &KnowledgeUnit, min_injected: u64, max_decided: u64) -> bool {
    let s = &unit.injection_stats;
    s.injected_count >= min_injected && s.decided() <= max_decided
}

/// Find dead-weight units. Sorted by injected_count desc (worst offenders first).
pub fn dead_weight<'a>(
    store: &'a HiveStore,
    min_injected: u64,
    max_decided: u64,
) -> Vec<&'a KnowledgeUnit> {
    let mut rows: Vec<&'a KnowledgeUnit> = store
        .all_units()
        .into_iter()
        .filter(|u| is_dead_weight(u, min_injected, max_decided))
        .collect();
    rows.sort_by(|a, b| {
        b.injection_stats
            .injected_count
            .cmp(&a.injection_stats.injected_count)
    });
    rows
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::{
        InjectionStats, KnowledgeCategory, KnowledgeContent, KnowledgeScope, KnowledgeUnit,
    };

    fn make_unit(
        id: &str,
        peer: &str,
        state: InjectionState,
        stats: InjectionStats,
    ) -> KnowledgeUnit {
        KnowledgeUnit {
            id: id.into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Pattern {
                tool: "Bash".into(),
                // Use the unit id as the command_pattern so each unit has a
                // unique semantic_key. Otherwise insert() drops collisions.
                command_pattern: Some(id.into()),
                preferred_action: "approve".into(),
                accept_rate: 0.9,
                sample_count: 10,
                conditions: vec![],
            },
            evidence_count: 10,
            confidence: 0.9,
            source_peer: peer.into(),
            originated_at: 0,
            last_validated_at: 0,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: state,
            injection_stats: stats,
            sharing_consent: None,
        }
    }

    fn store_with(units: Vec<KnowledgeUnit>) -> HiveStore {
        let mut store = HiveStore::load_from(std::path::Path::new("/nonexistent"));
        for u in units {
            store.insert(u);
        }
        store
    }

    #[test]
    fn unit_effectiveness_sorts_by_win_rate() {
        let store = store_with(vec![
            make_unit(
                "ku_low",
                "peer-a",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 30,
                    accepted_count: 4,
                    overridden_count: 6, // 40% win
                    ..Default::default()
                },
            ),
            make_unit(
                "ku_high",
                "peer-b",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 30,
                    accepted_count: 8,
                    overridden_count: 2, // 80% win
                    ..Default::default()
                },
            ),
            make_unit(
                "ku_unmeasured",
                "peer-c",
                InjectionState::Canary,
                InjectionStats {
                    injected_count: 5,
                    ..Default::default() // no decided
                },
            ),
        ]);

        let rows = unit_effectiveness(&store, &EffectivenessFilter::default());
        assert_eq!(rows.len(), 3);
        // Highest win rate first
        assert_eq!(rows[0].unit.id, "ku_high");
        assert_eq!(rows[1].unit.id, "ku_low");
        // Unmeasured ends up last
        assert_eq!(rows[2].unit.id, "ku_unmeasured");
    }

    #[test]
    fn unit_effectiveness_filter_by_peer() {
        let store = store_with(vec![
            make_unit(
                "ku_a",
                "peer-a",
                InjectionState::Live,
                InjectionStats::default(),
            ),
            make_unit(
                "ku_b",
                "peer-b",
                InjectionState::Live,
                InjectionStats::default(),
            ),
        ]);
        let filter = EffectivenessFilter {
            peer: Some("peer-a"),
            ..Default::default()
        };
        let rows = unit_effectiveness(&store, &filter);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].unit.id, "ku_a");
    }

    #[test]
    fn unit_effectiveness_filter_by_min_decided() {
        let store = store_with(vec![
            make_unit(
                "ku_lots",
                "peer-a",
                InjectionState::Live,
                InjectionStats {
                    accepted_count: 10,
                    overridden_count: 5,
                    ..Default::default()
                },
            ),
            make_unit(
                "ku_few",
                "peer-b",
                InjectionState::Live,
                InjectionStats {
                    accepted_count: 1,
                    overridden_count: 1,
                    ..Default::default()
                },
            ),
        ]);
        let filter = EffectivenessFilter {
            min_decided: 10,
            ..Default::default()
        };
        let rows = unit_effectiveness(&store, &filter);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].unit.id, "ku_lots");
    }

    #[test]
    fn unit_effectiveness_filter_by_state() {
        let store = store_with(vec![
            make_unit(
                "ku_canary",
                "peer-a",
                InjectionState::Canary,
                InjectionStats::default(),
            ),
            make_unit(
                "ku_live",
                "peer-b",
                InjectionState::Live,
                InjectionStats::default(),
            ),
        ]);
        let filter = EffectivenessFilter {
            state: Some(InjectionState::Canary),
            ..Default::default()
        };
        let rows = unit_effectiveness(&store, &filter);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].unit.injection_state, InjectionState::Canary);
    }

    #[test]
    fn peer_effectiveness_aggregates_across_units() {
        let store = store_with(vec![
            make_unit(
                "ku_a1",
                "peer-a",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 100,
                    accepted_count: 70,
                    overridden_count: 10,
                    ..Default::default()
                },
            ),
            make_unit(
                "ku_a2",
                "peer-a",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 50,
                    accepted_count: 30,
                    overridden_count: 10,
                    ..Default::default()
                },
            ),
            make_unit(
                "ku_b1",
                "peer-b",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 80,
                    accepted_count: 20,
                    overridden_count: 30,
                    ..Default::default()
                },
            ),
        ]);
        let rows = peer_effectiveness(&store);
        assert_eq!(rows.len(), 2);
        // peer-a has higher weighted win rate (100/120 ≈ 0.83) than peer-b (20/50 = 0.4)
        assert_eq!(rows[0].peer_id, "peer-a");
        assert_eq!(rows[0].unit_count, 2);
        assert_eq!(rows[0].total_injected, 150);
        assert!((rows[0].weighted_win_rate - 100.0 / 120.0).abs() < 1e-9);

        assert_eq!(rows[1].peer_id, "peer-b");
        assert!((rows[1].weighted_win_rate - 0.4).abs() < 1e-9);
    }

    #[test]
    fn dead_weight_finds_uninfluential_units() {
        let store = store_with(vec![
            make_unit(
                "ku_dead",
                "peer-a",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 100,
                    accepted_count: 0,
                    overridden_count: 1, // injected often, almost never decided
                    ..Default::default()
                },
            ),
            make_unit(
                "ku_alive",
                "peer-b",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 100,
                    accepted_count: 60,
                    overridden_count: 30,
                    ..Default::default()
                },
            ),
            make_unit(
                "ku_new",
                "peer-c",
                InjectionState::Canary,
                InjectionStats {
                    injected_count: 5, // not injected enough yet to qualify
                    ..Default::default()
                },
            ),
        ]);
        let rows = dead_weight(&store, DEAD_WEIGHT_MIN_INJECTED, DEAD_WEIGHT_MAX_DECIDED);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "ku_dead");
    }

    #[test]
    fn is_dead_weight_thresholds() {
        let mut u = make_unit("x", "p", InjectionState::Live, InjectionStats::default());
        u.injection_stats.injected_count = 49;
        // Below MIN_INJECTED → not dead-weight
        assert!(!is_dead_weight(&u, 50, 5));

        u.injection_stats.injected_count = 50;
        u.injection_stats.accepted_count = 0;
        u.injection_stats.overridden_count = 5;
        // At threshold: 50 injected, 5 decided → dead-weight (≤ MAX_DECIDED)
        assert!(is_dead_weight(&u, 50, 5));

        u.injection_stats.overridden_count = 6;
        // Above MAX_DECIDED → not dead-weight
        assert!(!is_dead_weight(&u, 50, 5));
    }

    #[test]
    fn peer_effectiveness_counts_dead_weight() {
        let store = store_with(vec![
            make_unit(
                "ku_dead",
                "peer-a",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 100,
                    overridden_count: 1,
                    ..Default::default()
                },
            ),
            make_unit(
                "ku_alive",
                "peer-a",
                InjectionState::Live,
                InjectionStats {
                    injected_count: 100,
                    accepted_count: 60,
                    overridden_count: 30,
                    ..Default::default()
                },
            ),
        ]);
        let rows = peer_effectiveness(&store);
        let p = rows.iter().find(|r| r.peer_id == "peer-a").unwrap();
        assert_eq!(p.dead_weight_count, 1);
        assert_eq!(p.unit_count, 2);
    }
}

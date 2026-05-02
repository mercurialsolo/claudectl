// Convert DistilledPreferences and Insights into KnowledgeUnits for sharing.

use std::collections::HashMap;

use crate::brain::decisions::{DecisionRecord, read_all_decisions};
use crate::brain::outcomes::{ResolvedOutcome, load_resolved_map};
use crate::brain::preferences::{
    DistilledPreferences, PreferencePattern, TemporalPattern, ToolAccuracy,
};

use super::{KnowledgeContent, KnowledgeScope, KnowledgeUnit, epoch_secs, gen_ku_id, semantic_key};

// ────────────────────────────────────────────────────────────────────────────
// Export thresholds
// ────────────────────────────────────────────────────────────────────────────

/// Configurable thresholds for what gets exported as shareable knowledge.
pub struct ExportThresholds {
    /// Minimum decisions backing a pattern before sharing.
    pub min_pattern_evidence: u32,
    /// Minimum total decisions for a tool before sharing accuracy.
    pub min_tool_decisions: u32,
    /// Minimum temporal pattern strength.
    pub min_temporal_strength: f64,
    /// Minimum outcomes attributed to an approach before sharing baseline (#220).
    pub min_outcome_samples: u32,
    /// Minimum samples on each variant before promoting a divergent group
    /// into an `ApproachCluster` (#221).
    pub min_cluster_variant_evidence: u32,
}

impl Default for ExportThresholds {
    fn default() -> Self {
        Self {
            min_pattern_evidence: 5,
            min_tool_decisions: 10,
            min_temporal_strength: 0.7,
            min_outcome_samples: 5,
            min_cluster_variant_evidence: 3,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Main distillation entry point
// ────────────────────────────────────────────────────────────────────────────

/// Convert distilled preferences into knowledge units for sharing.
/// Returns only units that meet the export thresholds.
pub fn distill_to_knowledge(
    prefs: &DistilledPreferences,
    source_peer: &str,
    project: Option<&str>,
    thresholds: &ExportThresholds,
) -> Vec<KnowledgeUnit> {
    let now = epoch_secs();
    let scope = match project {
        Some(p) => KnowledgeScope::Project(p.to_string()),
        None => KnowledgeScope::Universal,
    };
    let mut units = Vec::new();

    // Convert preference patterns
    for pattern in &prefs.patterns {
        if let Some(unit) = pattern_to_unit(pattern, source_peer, &scope, now, thresholds) {
            units.push(unit);
        }
    }

    // Convert tool accuracy stats
    for acc in &prefs.tool_accuracy {
        if let Some(unit) = accuracy_to_unit(acc, source_peer, now, thresholds) {
            units.push(unit);
        }
    }

    // Convert temporal patterns
    for tp in &prefs.temporal {
        if let Some(unit) = temporal_to_unit(tp, source_peer, &scope, now, thresholds) {
            units.push(unit);
        }
    }

    // #220 baselining: reap any pending PostToolUse outcomes first so the
    // freshest data lands in this pass.
    let _ = crate::brain::outcomes::reap();
    let decisions = read_all_decisions();
    let resolved = load_resolved_map();
    units.extend(distill_outcomes(
        &decisions,
        &resolved,
        source_peer,
        &scope,
        project,
        now,
        thresholds,
    ));

    // #221 conflict-aware: detect competing approaches and emit clusters.
    units.extend(detect_clusters(prefs, source_peer, &scope, now, thresholds));

    units
}

/// Convert distilled preferences into knowledge units, using an existing store
/// to assign stable IDs (update existing units rather than creating new ones).
pub fn distill_to_knowledge_stable(
    prefs: &DistilledPreferences,
    source_peer: &str,
    project: Option<&str>,
    thresholds: &ExportThresholds,
    existing: &super::store::HiveStore,
) -> Vec<KnowledgeUnit> {
    let mut units = distill_to_knowledge(prefs, source_peer, project, thresholds);

    // For each unit, check if a semantically equivalent one already exists.
    // If so, reuse the ID and bump the version. Otherwise mark it as Canary
    // (#223) so its rollout is gated by outcome stats before going wider.
    for unit in &mut units {
        let sk = semantic_key(unit);
        if let Some(existing_unit) = existing.find_by_semantic_key(&sk) {
            if existing_unit.source_peer == unit.source_peer {
                unit.id = existing_unit.id.clone();
                unit.version = existing_unit.version + 1;
                unit.originated_at = existing_unit.originated_at;
                unit.propagation_count = existing_unit.propagation_count;
                // Preserve rollout state and stats — the unit's history is
                // what tells us whether it's earned promotion.
                unit.injection_state = existing_unit.injection_state;
                unit.injection_stats = existing_unit.injection_stats.clone();
            }
        } else {
            // Truly new: start in Canary so we collect outcome signal before
            // exposing it to every prompt.
            unit.injection_state = super::InjectionState::Canary;
        }
    }

    units
}

// ────────────────────────────────────────────────────────────────────────────
// #220 baselining: outcome aggregation
// ────────────────────────────────────────────────────────────────────────────

/// Build the canonical approach reference for a decision. Mirrors the shape
/// used by `Pattern`'s semantic_key so cluster/outcome views can join on it.
fn approach_ref_for(decision: &DecisionRecord) -> Option<String> {
    let tool = decision.tool.as_deref()?;
    let cmd = decision
        .command
        .as_deref()
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "*".to_string());
    Some(format!("pattern:{tool}:{cmd}"))
}

#[derive(Default)]
struct OutcomeBucket {
    samples: u32,
    successes: u32,
    costs: Vec<f64>,
    durations_ms: Vec<u64>,
    project_hits: HashMap<String, u32>,
}

fn median_f64(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 0 {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    })
}

fn median_u64(mut v: Vec<u64>) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort();
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 0 {
        (v[mid - 1] + v[mid]) / 2
    } else {
        v[mid]
    })
}

/// Aggregate decisions + their resolved outcomes into per-approach buckets,
/// then emit ApproachOutcome KnowledgeUnits that meet `min_outcome_samples`.
pub fn distill_outcomes(
    decisions: &[DecisionRecord],
    resolved: &HashMap<String, ResolvedOutcome>,
    source_peer: &str,
    scope: &KnowledgeScope,
    project_filter: Option<&str>,
    now: u64,
    thresholds: &ExportThresholds,
) -> Vec<KnowledgeUnit> {
    let mut buckets: HashMap<String, OutcomeBucket> = HashMap::new();
    for d in decisions {
        // Optional per-project view
        if let Some(p) = project_filter {
            if !d.project.eq_ignore_ascii_case(p) {
                continue;
            }
        }
        let Some(decision_id) = d.decision_id.as_deref() else {
            continue;
        };
        let Some(outcome) = resolved.get(decision_id) else {
            continue;
        };
        let Some(key) = approach_ref_for(d) else {
            continue;
        };
        let entry = buckets.entry(key).or_default();
        entry.samples += 1;
        if outcome.exit_code.unwrap_or(-1) == 0 {
            entry.successes += 1;
        }
        if let Some(ctx) = &d.context {
            if ctx.cost_usd > 0.0 {
                entry.costs.push(ctx.cost_usd);
            }
        }
        if let Some(ms) = outcome.duration_ms {
            entry.durations_ms.push(ms);
        }
        *entry.project_hits.entry(d.project.clone()).or_insert(0) += 1;
    }

    let mut out = Vec::new();
    for (approach_ref, bucket) in buckets {
        if bucket.samples < thresholds.min_outcome_samples {
            continue;
        }
        let success_rate = bucket.successes as f64 / bucket.samples as f64;
        let median_cost_usd = median_f64(bucket.costs);
        let median_duration_ms = median_u64(bucket.durations_ms);

        // Conditions: list each project that contributed >= 2 samples,
        // gives the cluster work later something to filter on.
        let mut conditions: Vec<String> = bucket
            .project_hits
            .into_iter()
            .filter(|(_, n)| *n >= 2)
            .map(|(p, n)| format!("project:{p} (n={n})"))
            .collect();
        conditions.sort();

        out.push(KnowledgeUnit {
            id: gen_ku_id(),
            scope: scope.clone(),
            category: super::KnowledgeCategory::BestPractice,
            content: KnowledgeContent::ApproachOutcome {
                approach_ref,
                success_rate,
                sample_count: bucket.samples,
                median_cost_usd,
                median_duration_ms,
                conditions,
            },
            evidence_count: bucket.samples,
            confidence: success_rate,
            source_peer: source_peer.to_string(),
            originated_at: now,
            last_validated_at: now,
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
    }
    out
}

// ────────────────────────────────────────────────────────────────────────────
// #221 conflict-aware: cluster detection
// ────────────────────────────────────────────────────────────────────────────

/// Build the canonical problem key for a preference pattern.
/// Patterns sharing this key but differing on `preferred_action` are
/// candidates for an `ApproachCluster`.
fn problem_key_for(pattern: &PreferencePattern) -> String {
    let cmd = pattern.command_pattern.as_deref().unwrap_or("*");
    let normalized = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}:{normalized}", pattern.tool)
}

/// Render a pattern's conditions as human-readable strings for the cluster
/// variant payload (so peers can reason about *when* a variant tends to win).
fn condition_labels(pattern: &PreferencePattern) -> Vec<String> {
    pattern.conditions.iter().map(|c| c.label()).collect()
}

/// Detect divergent preference patterns and emit `ApproachCluster` units.
/// A group qualifies when:
///   - 2+ patterns share the same `problem_key`
///   - they disagree on `preferred_action`
///   - each variant has `>= min_cluster_variant_evidence` samples
pub fn detect_clusters(
    prefs: &DistilledPreferences,
    source_peer: &str,
    scope: &KnowledgeScope,
    now: u64,
    thresholds: &ExportThresholds,
) -> Vec<KnowledgeUnit> {
    let mut groups: HashMap<String, Vec<&PreferencePattern>> = HashMap::new();
    for p in &prefs.patterns {
        groups.entry(problem_key_for(p)).or_default().push(p);
    }

    let mut units = Vec::new();
    for (problem_key, patterns) in groups {
        if patterns.len() < 2 {
            continue;
        }
        // Drop patterns below the per-variant evidence floor before checking
        // for divergence — a single weak counter-pattern shouldn't promote
        // an otherwise stable preference into a cluster.
        let qualifying: Vec<&PreferencePattern> = patterns
            .into_iter()
            .filter(|p| p.sample_count >= thresholds.min_cluster_variant_evidence)
            .collect();
        if qualifying.len() < 2 {
            continue;
        }
        let mut actions: Vec<&str> = qualifying
            .iter()
            .map(|p| p.preferred_action.as_str())
            .collect();
        actions.sort();
        actions.dedup();
        if actions.len() < 2 {
            continue; // all variants agree — not really a conflict
        }

        let outcome_ref = format!("pattern:{problem_key}");
        let variants: Vec<super::ApproachVariant> = qualifying
            .iter()
            .map(|p| super::ApproachVariant {
                approach_summary: format!(
                    "{} ({:.0}% accept over {})",
                    p.preferred_action,
                    p.accept_rate * 100.0,
                    p.sample_count
                ),
                conditions: condition_labels(p),
                evidence: p.sample_count,
                contributing_peers: vec![source_peer.to_string()],
                outcome_ref: Some(outcome_ref.clone()),
            })
            .collect();

        let total_evidence: u32 = variants.iter().map(|v| v.evidence).sum();
        let mean_confidence =
            qualifying.iter().map(|p| p.confidence).sum::<f64>() / qualifying.len() as f64;

        units.push(KnowledgeUnit {
            id: gen_ku_id(),
            scope: scope.clone(),
            category: super::KnowledgeCategory::Technique,
            content: KnowledgeContent::ApproachCluster {
                problem_key,
                variants,
            },
            evidence_count: total_evidence,
            confidence: mean_confidence,
            source_peer: source_peer.to_string(),
            originated_at: now,
            last_validated_at: now,
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
    }
    units
}

// ────────────────────────────────────────────────────────────────────────────
// Individual converters
// ────────────────────────────────────────────────────────────────────────────

/// Classify a preference pattern into a knowledge category.
fn classify_pattern(pattern: &PreferencePattern) -> super::KnowledgeCategory {
    use super::KnowledgeCategory;
    use crate::brain::preferences::PreferenceCondition;

    // Patterns conditioned on cost or time-of-day are personal
    for cond in &pattern.conditions {
        match cond {
            PreferenceCondition::CostAbove(_) | PreferenceCondition::CostBelow(_) => {
                return KnowledgeCategory::Personal;
            }
            PreferenceCondition::HourRange(_, _) => {
                return KnowledgeCategory::Personal;
            }
            _ => {}
        }
    }

    // Tool approval/denial patterns are best practices
    KnowledgeCategory::BestPractice
}

fn pattern_to_unit(
    pattern: &PreferencePattern,
    source_peer: &str,
    scope: &KnowledgeScope,
    now: u64,
    thresholds: &ExportThresholds,
) -> Option<KnowledgeUnit> {
    if pattern.sample_count < thresholds.min_pattern_evidence {
        return None;
    }

    let category = classify_pattern(pattern);
    let conditions: Vec<String> = pattern.conditions.iter().map(|c| c.label()).collect();

    Some(KnowledgeUnit {
        id: gen_ku_id(),
        scope: scope.clone(),
        category,
        content: KnowledgeContent::Pattern {
            tool: pattern.tool.clone(),
            command_pattern: pattern.command_pattern.clone(),
            preferred_action: pattern.preferred_action.clone(),
            accept_rate: pattern.accept_rate,
            sample_count: pattern.sample_count,
            conditions,
        },
        evidence_count: pattern.sample_count,
        confidence: pattern.accept_rate,
        source_peer: source_peer.to_string(),
        originated_at: now,
        last_validated_at: now,
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
    })
}

fn accuracy_to_unit(
    acc: &ToolAccuracy,
    source_peer: &str,
    now: u64,
    thresholds: &ExportThresholds,
) -> Option<KnowledgeUnit> {
    if acc.total < thresholds.min_tool_decisions {
        return None;
    }

    Some(KnowledgeUnit {
        id: gen_ku_id(),
        scope: KnowledgeScope::Universal,
        category: super::KnowledgeCategory::BestPractice,
        content: KnowledgeContent::ToolAccuracy {
            tool: acc.tool.clone(),
            total: acc.total,
            correct: acc.correct,
            confidence_threshold: acc.confidence_threshold,
        },
        evidence_count: acc.total,
        confidence: if acc.total > 0 {
            acc.correct as f64 / acc.total as f64
        } else {
            0.0
        },
        source_peer: source_peer.to_string(),
        originated_at: now,
        last_validated_at: now,
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
    })
}

/// Classify a temporal pattern by its description.
fn classify_temporal(tp: &TemporalPattern) -> super::KnowledgeCategory {
    use super::KnowledgeCategory;
    let desc = tp.description.to_lowercase();

    // Time-of-day, approval speed, and cost patterns are personal
    if desc.contains("hour")
        || desc.contains("time of day")
        || desc.contains("morning")
        || desc.contains("evening")
        || desc.contains("approval")
    {
        return KnowledgeCategory::Personal;
    }
    if desc.contains("cost") || desc.contains("spend") || desc.contains("budget") {
        return KnowledgeCategory::Personal;
    }

    // Error streaks, context patterns are shareable techniques
    if desc.contains("error") || desc.contains("context") || desc.contains("retry") {
        return KnowledgeCategory::Technique;
    }

    KnowledgeCategory::BestPractice
}

fn temporal_to_unit(
    tp: &TemporalPattern,
    source_peer: &str,
    scope: &KnowledgeScope,
    now: u64,
    thresholds: &ExportThresholds,
) -> Option<KnowledgeUnit> {
    if tp.strength < thresholds.min_temporal_strength {
        return None;
    }

    let category = classify_temporal(tp);

    Some(KnowledgeUnit {
        id: gen_ku_id(),
        scope: scope.clone(),
        category,
        content: KnowledgeContent::Temporal {
            description: tp.description.clone(),
            strength: tp.strength,
        },
        evidence_count: tp.sample_count,
        confidence: tp.strength,
        source_peer: source_peer.to_string(),
        originated_at: now,
        last_validated_at: now,
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
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_prefs() -> DistilledPreferences {
        DistilledPreferences {
            patterns: vec![
                PreferencePattern {
                    tool: "Bash".into(),
                    command_pattern: Some("cargo test".into()),
                    preferred_action: "approve".into(),
                    sample_count: 20,
                    accept_rate: 0.95,
                    conditions: vec![],
                    confidence: 0.95,
                },
                PreferencePattern {
                    tool: "Bash".into(),
                    command_pattern: Some("rm -rf".into()),
                    preferred_action: "deny".into(),
                    sample_count: 3, // below threshold
                    accept_rate: 0.0,
                    conditions: vec![],
                    confidence: 0.0,
                },
            ],
            tool_accuracy: vec![
                ToolAccuracy {
                    tool: "Bash".into(),
                    total: 50,
                    correct: 42,
                    confidence_threshold: 0.7,
                },
                ToolAccuracy {
                    tool: "Read".into(),
                    total: 5, // below threshold
                    correct: 5,
                    confidence_threshold: 0.5,
                },
            ],
            temporal: vec![
                TemporalPattern {
                    description: "Error streak detected".into(),
                    sample_count: 8,
                    strength: 0.85,
                },
                TemporalPattern {
                    description: "Weak pattern".into(),
                    sample_count: 3,
                    strength: 0.3, // below threshold
                },
            ],
            total_decisions: 100,
            overall_accuracy: 84.0,
        }
    }

    #[test]
    fn distill_filters_by_thresholds() {
        let prefs = make_prefs();
        let units = distill_to_knowledge(&prefs, "test-peer", None, &ExportThresholds::default());

        // Should have: 1 pattern (20 >= 5, 3 < 5), 1 accuracy (50 >= 10, 5 < 10), 1 temporal (0.85 >= 0.7, 0.3 < 0.7)
        assert_eq!(units.len(), 3);
    }

    #[test]
    fn pattern_below_threshold_excluded() {
        let prefs = make_prefs();
        let units = distill_to_knowledge(&prefs, "test-peer", None, &ExportThresholds::default());

        // The rm -rf pattern (3 samples) should not be included
        let pattern_tools: Vec<&str> = units
            .iter()
            .filter_map(|u| match &u.content {
                KnowledgeContent::Pattern { tool, .. } => Some(tool.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(pattern_tools, vec!["Bash"]);
    }

    #[test]
    fn accuracy_below_threshold_excluded() {
        let prefs = make_prefs();
        let units = distill_to_knowledge(&prefs, "test-peer", None, &ExportThresholds::default());

        let accuracy_tools: Vec<&str> = units
            .iter()
            .filter_map(|u| match &u.content {
                KnowledgeContent::ToolAccuracy { tool, .. } => Some(tool.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(accuracy_tools, vec!["Bash"]);
    }

    #[test]
    fn temporal_below_threshold_excluded() {
        let prefs = make_prefs();
        let units = distill_to_knowledge(&prefs, "test-peer", None, &ExportThresholds::default());

        let temporal_descs: Vec<&str> = units
            .iter()
            .filter_map(|u| match &u.content {
                KnowledgeContent::Temporal { description, .. } => Some(description.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(temporal_descs, vec!["Error streak detected"]);
    }

    #[test]
    fn project_scope_applied() {
        let prefs = make_prefs();
        let units = distill_to_knowledge(
            &prefs,
            "test-peer",
            Some("claudectl"),
            &ExportThresholds::default(),
        );

        // Patterns and temporals get project scope; accuracy is always universal
        for unit in &units {
            match &unit.content {
                KnowledgeContent::ToolAccuracy { .. } => {
                    assert_eq!(unit.scope, KnowledgeScope::Universal);
                }
                _ => {
                    assert_eq!(unit.scope, KnowledgeScope::Project("claudectl".into()));
                }
            }
        }
    }

    #[test]
    fn stable_distill_reuses_ids() {
        let prefs = make_prefs();
        let first = distill_to_knowledge(&prefs, "test-peer", None, &ExportThresholds::default());

        let mut store =
            super::super::store::HiveStore::load_from(std::path::Path::new("/nonexistent"));
        for unit in &first {
            store.insert(unit.clone());
        }

        let second = distill_to_knowledge_stable(
            &prefs,
            "test-peer",
            None,
            &ExportThresholds::default(),
            &store,
        );

        // Second run should reuse IDs and bump versions
        for unit in &second {
            let sk = semantic_key(unit);
            if let Some(original) = store.find_by_semantic_key(&sk) {
                assert_eq!(unit.id, original.id);
                assert_eq!(unit.version, original.version + 1);
            }
        }
    }

    fn make_decision_with_id(id: &str, tool: &str, command: &str, project: &str) -> DecisionRecord {
        use crate::brain::decisions::DecisionType;
        DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some(command.into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: "accept".into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: Some(id.into()),
        }
    }

    fn make_resolved(
        id: &str,
        tool: &str,
        project: &str,
        exit: Option<i32>,
        dur: Option<u64>,
    ) -> ResolvedOutcome {
        ResolvedOutcome {
            decision_id: id.into(),
            tool: tool.into(),
            command: Some("cargo test".into()),
            project: project.into(),
            exit_code: exit,
            duration_ms: dur,
            stderr_tail: None,
            ts: 0,
        }
    }

    #[test]
    fn distill_outcomes_aggregates_per_approach() {
        let decisions = vec![
            make_decision_with_id("d1", "Bash", "cargo test", "p"),
            make_decision_with_id("d2", "Bash", "cargo test", "p"),
            make_decision_with_id("d3", "Bash", "cargo test", "p"),
        ];
        let mut resolved = HashMap::new();
        resolved.insert(
            "d1".into(),
            make_resolved("d1", "Bash", "p", Some(0), Some(100)),
        );
        resolved.insert(
            "d2".into(),
            make_resolved("d2", "Bash", "p", Some(0), Some(200)),
        );
        resolved.insert(
            "d3".into(),
            make_resolved("d3", "Bash", "p", Some(1), Some(300)),
        );

        let units = distill_outcomes(
            &decisions,
            &resolved,
            "peer",
            &KnowledgeScope::Universal,
            None,
            0,
            &ExportThresholds {
                min_outcome_samples: 1,
                ..Default::default()
            },
        );

        assert_eq!(units.len(), 1, "all three share the same approach_ref");
        match &units[0].content {
            KnowledgeContent::ApproachOutcome {
                approach_ref,
                success_rate,
                sample_count,
                median_duration_ms,
                ..
            } => {
                assert_eq!(approach_ref, "pattern:Bash:cargo test");
                assert_eq!(*sample_count, 3);
                assert!((success_rate - 2.0 / 3.0).abs() < 1e-9);
                assert_eq!(*median_duration_ms, Some(200));
            }
            other => panic!("unexpected content {other:?}"),
        }
    }

    #[test]
    fn distill_outcomes_threshold_filters() {
        let decisions = vec![make_decision_with_id("d1", "Bash", "ls", "p")];
        let mut resolved = HashMap::new();
        resolved.insert("d1".into(), make_resolved("d1", "Bash", "p", Some(0), None));

        let units = distill_outcomes(
            &decisions,
            &resolved,
            "peer",
            &KnowledgeScope::Universal,
            None,
            0,
            &ExportThresholds {
                min_outcome_samples: 5,
                ..Default::default()
            },
        );
        assert!(units.is_empty(), "1 sample below threshold of 5");
    }

    #[test]
    fn distill_outcomes_skips_decisions_without_id() {
        // Old records lack decision_id and can't be joined to outcomes.
        let mut d = make_decision_with_id("d1", "Bash", "cargo test", "p");
        d.decision_id = None;
        let resolved = HashMap::new();
        let units = distill_outcomes(
            &[d],
            &resolved,
            "peer",
            &KnowledgeScope::Universal,
            None,
            0,
            &ExportThresholds {
                min_outcome_samples: 1,
                ..Default::default()
            },
        );
        assert!(units.is_empty());
    }

    #[test]
    fn distill_outcomes_project_filter() {
        let decisions = vec![
            make_decision_with_id("d1", "Bash", "cargo test", "alpha"),
            make_decision_with_id("d2", "Bash", "cargo test", "beta"),
        ];
        let mut resolved = HashMap::new();
        resolved.insert(
            "d1".into(),
            make_resolved("d1", "Bash", "alpha", Some(0), None),
        );
        resolved.insert(
            "d2".into(),
            make_resolved("d2", "Bash", "beta", Some(0), None),
        );

        let units = distill_outcomes(
            &decisions,
            &resolved,
            "peer",
            &KnowledgeScope::Project("alpha".into()),
            Some("alpha"),
            0,
            &ExportThresholds {
                min_outcome_samples: 1,
                ..Default::default()
            },
        );

        assert_eq!(units.len(), 1);
        if let KnowledgeContent::ApproachOutcome { sample_count, .. } = &units[0].content {
            assert_eq!(*sample_count, 1, "beta decision should be filtered out");
        } else {
            panic!("wrong content variant");
        }
    }

    fn make_pattern(
        tool: &str,
        cmd: &str,
        action: &str,
        n: u32,
        accept: f64,
        conds: Vec<crate::brain::preferences::PreferenceCondition>,
    ) -> PreferencePattern {
        PreferencePattern {
            tool: tool.into(),
            command_pattern: Some(cmd.into()),
            preferred_action: action.into(),
            sample_count: n,
            accept_rate: accept,
            conditions: conds,
            confidence: accept,
        }
    }

    #[test]
    fn detect_clusters_on_divergent_actions() {
        use crate::brain::preferences::PreferenceCondition;
        let prefs = DistilledPreferences {
            patterns: vec![
                make_pattern(
                    "Bash",
                    "git push",
                    "approve",
                    8,
                    0.9,
                    vec![PreferenceCondition::CostBelow(1.0)],
                ),
                make_pattern(
                    "Bash",
                    "git push",
                    "deny",
                    5,
                    0.1,
                    vec![PreferenceCondition::CostAbove(1.0)],
                ),
            ],
            tool_accuracy: vec![],
            temporal: vec![],
            total_decisions: 13,
            overall_accuracy: 0.0,
        };
        let units = detect_clusters(
            &prefs,
            "peer-a",
            &KnowledgeScope::Universal,
            0,
            &ExportThresholds::default(),
        );
        assert_eq!(
            units.len(),
            1,
            "should produce one cluster for the divergent group"
        );
        if let KnowledgeContent::ApproachCluster {
            problem_key,
            variants,
        } = &units[0].content
        {
            assert_eq!(problem_key, "Bash:git push");
            assert_eq!(variants.len(), 2);
            // Must capture both actions
            let summaries: Vec<&str> = variants
                .iter()
                .map(|v| v.approach_summary.as_str())
                .collect();
            assert!(summaries.iter().any(|s| s.starts_with("approve")));
            assert!(summaries.iter().any(|s| s.starts_with("deny")));
        } else {
            panic!("wrong content variant");
        }
    }

    #[test]
    fn detect_clusters_skips_when_actions_agree() {
        let prefs = DistilledPreferences {
            patterns: vec![
                make_pattern("Bash", "ls", "approve", 8, 0.9, vec![]),
                make_pattern("Bash", "ls", "approve", 5, 0.95, vec![]),
            ],
            tool_accuracy: vec![],
            temporal: vec![],
            total_decisions: 13,
            overall_accuracy: 0.0,
        };
        let units = detect_clusters(
            &prefs,
            "peer-a",
            &KnowledgeScope::Universal,
            0,
            &ExportThresholds::default(),
        );
        assert!(units.is_empty(), "no divergence → no cluster");
    }

    #[test]
    fn detect_clusters_skips_below_per_variant_threshold() {
        let prefs = DistilledPreferences {
            patterns: vec![
                make_pattern("Bash", "rm -rf", "deny", 8, 0.0, vec![]),
                // Counter-pattern is too weak to count as a real variant
                make_pattern("Bash", "rm -rf", "approve", 1, 1.0, vec![]),
            ],
            tool_accuracy: vec![],
            temporal: vec![],
            total_decisions: 9,
            overall_accuracy: 0.0,
        };
        let units = detect_clusters(
            &prefs,
            "peer-a",
            &KnowledgeScope::Universal,
            0,
            &ExportThresholds::default(),
        );
        assert!(
            units.is_empty(),
            "weak counter-pattern should not promote a cluster"
        );
    }

    #[test]
    fn approach_outcome_semantic_key_stable_across_peers() {
        // Two units from different peers describing the same approach must
        // produce the same semantic_key so the merger can dedup them.
        let unit_a = KnowledgeUnit {
            id: gen_ku_id(),
            scope: KnowledgeScope::Universal,
            category: super::super::KnowledgeCategory::BestPractice,
            content: KnowledgeContent::ApproachOutcome {
                approach_ref: "pattern:Bash:cargo test".into(),
                success_rate: 0.9,
                sample_count: 10,
                median_cost_usd: None,
                median_duration_ms: None,
                conditions: vec![],
            },
            evidence_count: 10,
            confidence: 0.9,
            source_peer: "peer-a".into(),
            originated_at: 0,
            last_validated_at: 0,
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
        };
        let mut unit_b = unit_a.clone();
        unit_b.source_peer = "peer-b".into();
        unit_b.id = gen_ku_id();
        assert_eq!(semantic_key(&unit_a), semantic_key(&unit_b));
    }

    #[test]
    fn custom_thresholds() {
        let prefs = make_prefs();
        let strict = ExportThresholds {
            min_pattern_evidence: 50,
            min_tool_decisions: 100,
            min_temporal_strength: 0.99,
            min_outcome_samples: u32::MAX,
            min_cluster_variant_evidence: u32::MAX,
        };
        let units = distill_to_knowledge(&prefs, "test-peer", None, &strict);
        assert_eq!(units.len(), 0); // nothing meets strict thresholds
    }
}

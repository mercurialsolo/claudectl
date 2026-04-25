// Convert DistilledPreferences and Insights into KnowledgeUnits for sharing.

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
}

impl Default for ExportThresholds {
    fn default() -> Self {
        Self {
            min_pattern_evidence: 5,
            min_tool_decisions: 10,
            min_temporal_strength: 0.7,
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
    // If so, reuse the ID and bump the version.
    for unit in &mut units {
        let sk = semantic_key(unit);
        if let Some(existing_unit) = existing.find_by_semantic_key(&sk) {
            if existing_unit.source_peer == unit.source_peer {
                unit.id = existing_unit.id.clone();
                unit.version = existing_unit.version + 1;
                unit.originated_at = existing_unit.originated_at;
                unit.propagation_count = existing_unit.propagation_count;
            }
        }
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

    #[test]
    fn custom_thresholds() {
        let prefs = make_prefs();
        let strict = ExportThresholds {
            min_pattern_evidence: 50,
            min_tool_decisions: 100,
            min_temporal_strength: 0.99,
        };
        let units = distill_to_knowledge(&prefs, "test-peer", None, &strict);
        assert_eq!(units.len(), 0); // nothing meets strict thresholds
    }
}

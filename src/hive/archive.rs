// Cold storage archive: unbounded on-disk knowledge with distillation.
//
// Evicted units from the warm store are archived here instead of deleted.
// Periodic distillation condenses the archive into a compact curriculum.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use super::{KnowledgeContent, KnowledgeUnit, epoch_secs, semantic_key};

// ────────────────────────────────────────────────────────────────────────────
// Paths
// ────────────────────────────────────────────────────────────────────────────

fn hive_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claudectl").join("hive")
}

fn archive_path() -> PathBuf {
    hive_dir().join("archive.jsonl")
}

fn curriculum_path() -> PathBuf {
    hive_dir().join("curriculum.json")
}

fn curriculum_meta_path() -> PathBuf {
    hive_dir().join("curriculum_meta.json")
}

// ────────────────────────────────────────────────────────────────────────────
// Archive operations
// ────────────────────────────────────────────────────────────────────────────

/// Append evicted units to the archive file.
pub fn archive_units(units: &[KnowledgeUnit]) -> std::io::Result<usize> {
    if units.is_empty() {
        return Ok(0);
    }
    let dir = hive_dir();
    fs::create_dir_all(&dir)?;
    let path = archive_path();

    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut count = 0;
    for unit in units {
        if let Ok(json) = serde_json::to_string(unit) {
            writeln!(file, "{json}")?;
            count += 1;
        }
    }
    Ok(count)
}

/// Count units in the archive without loading them all into memory.
pub fn archive_count() -> usize {
    let path = archive_path();
    fs::read_to_string(&path)
        .map(|c| c.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// Estimate archive file size in bytes.
pub fn archive_size_bytes() -> u64 {
    let path = archive_path();
    fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
}

/// Prune archive entries older than `max_age_days`.
pub fn prune_archive(max_age_days: u32) -> std::io::Result<usize> {
    let path = archive_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Ok(0),
    };

    let now = epoch_secs();
    let max_age_secs = max_age_days as u64 * 86400;
    let mut kept = Vec::new();
    let mut pruned = 0;

    for line in content.lines() {
        if let Ok(unit) = serde_json::from_str::<KnowledgeUnit>(line) {
            let age = now.saturating_sub(unit.last_validated_at);
            if age <= max_age_secs {
                kept.push(line.to_string());
            } else {
                pruned += 1;
            }
        }
    }

    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, kept.join("\n") + "\n")?;
    fs::rename(&tmp, &path)?;
    Ok(pruned)
}

// ────────────────────────────────────────────────────────────────────────────
// Distillation: condense archive into curriculum
// ────────────────────────────────────────────────────────────────────────────

/// Result of a distillation run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DistillationReport {
    pub archive_units_read: usize,
    pub duplicates_merged: usize,
    pub patterns_condensed: usize,
    pub contradictions_found: usize,
    pub curriculum_units: usize,
    pub curriculum_version: u32,
    pub timestamp: u64,
}

/// Curriculum metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CurriculumMeta {
    pub version: u32,
    pub timestamp: u64,
    pub unit_count: usize,
    pub source_archive_units: usize,
}

/// Run the distillation pipeline on the archive. Produces a curriculum.
pub fn distill_archive() -> Result<DistillationReport, String> {
    let path = archive_path();
    let content = fs::read_to_string(&path).unwrap_or_default();

    // Load all archive units
    let mut all: Vec<KnowledgeUnit> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let archive_count = all.len();

    // Also include current warm store units
    let warm = super::store::HiveStore::load();
    all.extend(warm.all_units().into_iter().cloned());

    // Step 1: Dedup by semantic key — keep highest confidence × evidence
    let mut deduped: HashMap<String, KnowledgeUnit> = HashMap::new();
    let mut duplicates_merged = 0;
    for unit in &all {
        let sk = semantic_key(unit);
        let score = unit.confidence * unit.evidence_count as f64;
        if let Some(existing) = deduped.get(&sk) {
            let existing_score = existing.confidence * existing.evidence_count as f64;
            if score > existing_score {
                deduped.insert(sk, unit.clone());
            }
            duplicates_merged += 1;
        } else {
            deduped.insert(sk, unit.clone());
        }
    }

    let mut units: Vec<KnowledgeUnit> = deduped.into_values().collect();

    // Step 2: Condense similar patterns (same tool, 3+ patterns with common prefix)
    let patterns_condensed = condense_patterns(&mut units);

    // Step 3: Contradiction resolution — flag disputed knowledge
    let contradictions_found = resolve_contradictions(&mut units);

    // Step 4: Build curriculum — filter to high-confidence, sort by priority
    let mut curriculum: Vec<KnowledgeUnit> = units
        .into_iter()
        .filter(|u| u.confidence >= 0.7 || is_safety_guard(u))
        .collect();

    // Safety guards always first, then by confidence × evidence
    curriculum.sort_by(|a, b| {
        let a_safety = is_safety_guard(a) as u8;
        let b_safety = is_safety_guard(b) as u8;
        b_safety.cmp(&a_safety).then_with(|| {
            let a_score = a.confidence * a.evidence_count as f64;
            let b_score = b.confidence * b.evidence_count as f64;
            b_score
                .partial_cmp(&a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    // Cap at 100 units
    curriculum.truncate(100);

    // Save curriculum
    let prev_meta = load_curriculum_meta();
    let version = prev_meta.map(|m| m.version + 1).unwrap_or(1);
    let now = epoch_secs();

    save_curriculum(&curriculum, version, now, archive_count)
        .map_err(|e| format!("save curriculum: {e}"))?;

    Ok(DistillationReport {
        archive_units_read: archive_count,
        duplicates_merged,
        patterns_condensed,
        contradictions_found,
        curriculum_units: curriculum.len(),
        curriculum_version: version,
        timestamp: now,
    })
}

/// Load the current curriculum.
pub fn load_curriculum() -> Vec<KnowledgeUnit> {
    let path = curriculum_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Load curriculum metadata.
pub fn load_curriculum_meta() -> Option<CurriculumMeta> {
    let path = curriculum_meta_path();
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_curriculum(
    units: &[KnowledgeUnit],
    version: u32,
    timestamp: u64,
    source_count: usize,
) -> std::io::Result<()> {
    let dir = hive_dir();
    fs::create_dir_all(&dir)?;

    let json = serde_json::to_string_pretty(units)
        .map_err(|e| std::io::Error::other(format!("serialize: {e}")))?;
    fs::write(curriculum_path(), json)?;

    let meta = CurriculumMeta {
        version,
        timestamp,
        unit_count: units.len(),
        source_archive_units: source_count,
    };
    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|e| std::io::Error::other(format!("serialize meta: {e}")))?;
    fs::write(curriculum_meta_path(), meta_json)?;

    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Distillation helpers
// ────────────────────────────────────────────────────────────────────────────

/// Check if a unit is a safety guard (deny rule with high confidence).
fn is_safety_guard(unit: &KnowledgeUnit) -> bool {
    if let KnowledgeContent::Pattern {
        preferred_action, ..
    } = &unit.content
    {
        preferred_action == "deny" && unit.confidence >= 0.8
    } else {
        false
    }
}

/// Condense similar patterns with a common tool + command prefix.
/// Returns the number of patterns condensed.
fn condense_patterns(units: &mut Vec<KnowledgeUnit>) -> usize {
    // Group patterns by tool
    let mut by_tool: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, unit) in units.iter().enumerate() {
        if let KnowledgeContent::Pattern { ref tool, .. } = unit.content {
            by_tool.entry(tool.clone()).or_default().push(i);
        }
    }

    let mut condensed = 0;
    let mut to_remove: Vec<usize> = Vec::new();

    for indices in by_tool.values() {
        if indices.len() < 3 {
            continue;
        }

        // Check if all patterns for this tool are approve with > 80% rate
        let all_approve = indices.iter().all(|&i| {
            if let KnowledgeContent::Pattern {
                ref preferred_action,
                accept_rate,
                ..
            } = units[i].content
            {
                preferred_action == "approve" && accept_rate > 0.8
            } else {
                false
            }
        });

        if !all_approve {
            continue;
        }

        // Condense: create a wildcard pattern, remove originals
        let total_evidence: u32 = indices.iter().map(|&i| units[i].evidence_count).sum();
        let avg_confidence: f64 = indices
            .iter()
            .map(|&i| units[i].confidence * units[i].evidence_count as f64)
            .sum::<f64>()
            / total_evidence as f64;

        // Mark originals for removal (skip the first, we'll transform it)
        for &i in &indices[1..] {
            to_remove.push(i);
        }

        // Transform the first one into the condensed pattern
        let first = &mut units[indices[0]];
        if let KnowledgeContent::Pattern {
            ref mut command_pattern,
            ref mut accept_rate,
            ref mut sample_count,
            ..
        } = first.content
        {
            *command_pattern = Some("*".to_string());
            *accept_rate = avg_confidence;
            *sample_count = total_evidence;
        }
        first.evidence_count = total_evidence;
        first.confidence = avg_confidence;
        condensed += indices.len() - 1;
    }

    // Remove condensed originals (reverse order to maintain indices)
    to_remove.sort_unstable();
    to_remove.dedup();
    for i in to_remove.into_iter().rev() {
        if i < units.len() {
            units.remove(i);
        }
    }

    condensed
}

/// Resolve contradictions: when approve and deny exist for the same tool/command.
/// Returns the number of contradictions found.
fn resolve_contradictions(units: &mut Vec<KnowledgeUnit>) -> usize {
    // Group by (tool, command_pattern)
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, unit) in units.iter().enumerate() {
        if let KnowledgeContent::Pattern {
            ref tool,
            ref command_pattern,
            ..
        } = unit.content
        {
            let cmd = command_pattern.as_deref().unwrap_or("*");
            let key = format!("{tool}:{cmd}");
            groups.entry(key).or_default().push(i);
        }
    }

    let mut contradictions = 0;
    let mut to_remove: Vec<usize> = Vec::new();

    for indices in groups.values() {
        if indices.len() < 2 {
            continue;
        }

        let actions: Vec<&str> = indices
            .iter()
            .filter_map(|&i| {
                if let KnowledgeContent::Pattern {
                    ref preferred_action,
                    ..
                } = units[i].content
                {
                    Some(preferred_action.as_str())
                } else {
                    None
                }
            })
            .collect();

        let has_approve = actions.contains(&"approve");
        let has_deny = actions.contains(&"deny");

        if has_approve && has_deny {
            contradictions += 1;

            // Find the highest-confidence unit, remove the rest
            let best = indices
                .iter()
                .max_by(|&&a, &&b| {
                    let sa = units[a].confidence * units[a].evidence_count as f64;
                    let sb = units[b].confidence * units[b].evidence_count as f64;
                    sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
                })
                .copied();

            if let Some(best_idx) = best {
                for &i in indices {
                    if i != best_idx {
                        to_remove.push(i);
                    }
                }
            }
        }
    }

    to_remove.sort_unstable();
    to_remove.dedup();
    for i in to_remove.into_iter().rev() {
        if i < units.len() {
            units.remove(i);
        }
    }

    contradictions
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::{KnowledgeCategory, KnowledgeScope, gen_ku_id};

    fn make_pattern(
        tool: &str,
        cmd: &str,
        action: &str,
        confidence: f64,
        evidence: u32,
    ) -> KnowledgeUnit {
        KnowledgeUnit {
            id: gen_ku_id(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Pattern {
                tool: tool.into(),
                command_pattern: Some(cmd.into()),
                preferred_action: action.into(),
                accept_rate: confidence,
                sample_count: evidence,
                conditions: vec![],
            },
            evidence_count: evidence,
            confidence,
            source_peer: "test".into(),
            originated_at: epoch_secs(),
            last_validated_at: epoch_secs(),
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
    fn condense_merges_similar_approves() {
        let mut units = vec![
            make_pattern("Bash", "cargo test", "approve", 0.95, 20),
            make_pattern("Bash", "cargo clippy", "approve", 0.92, 15),
            make_pattern("Bash", "cargo fmt", "approve", 0.98, 25),
        ];

        let condensed = condense_patterns(&mut units);
        assert_eq!(condensed, 2); // 2 originals removed
        assert_eq!(units.len(), 1);

        if let KnowledgeContent::Pattern {
            command_pattern,
            sample_count,
            ..
        } = &units[0].content
        {
            assert_eq!(command_pattern.as_deref(), Some("*"));
            assert_eq!(*sample_count, 60); // 20 + 15 + 25
        }
    }

    #[test]
    fn condense_skips_mixed_actions() {
        let mut units = vec![
            make_pattern("Bash", "cargo test", "approve", 0.95, 20),
            make_pattern("Bash", "rm -rf", "deny", 1.0, 5),
            make_pattern("Bash", "cargo fmt", "approve", 0.98, 25),
        ];

        let condensed = condense_patterns(&mut units);
        assert_eq!(condensed, 0); // not all approve
        assert_eq!(units.len(), 3);
    }

    #[test]
    fn contradiction_resolution() {
        let mut units = vec![
            make_pattern("Bash", "docker push", "approve", 0.6, 3),
            make_pattern("Bash", "docker push", "deny", 0.95, 10),
        ];

        let found = resolve_contradictions(&mut units);
        assert_eq!(found, 1);
        assert_eq!(units.len(), 1);
        // The deny (higher score) should win
        if let KnowledgeContent::Pattern {
            preferred_action, ..
        } = &units[0].content
        {
            assert_eq!(preferred_action, "deny");
        }
    }

    #[test]
    fn safety_guard_detection() {
        let deny = make_pattern("Bash", "rm -rf", "deny", 0.95, 10);
        assert!(is_safety_guard(&deny));

        let approve = make_pattern("Bash", "cargo test", "approve", 0.95, 10);
        assert!(!is_safety_guard(&approve));

        let low_deny = make_pattern("Bash", "test", "deny", 0.5, 2);
        assert!(!is_safety_guard(&low_deny)); // confidence too low
    }
}

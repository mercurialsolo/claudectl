#![allow(dead_code)]

use std::collections::HashMap;

use super::decisions::{DecisionContext, DecisionOutcome, DecisionRecord};

// ────────────────────────────────────────────────────────────────────────────
// Work-hours constants (shared with decisions.rs via pub(super))
// ────────────────────────────────────────────────────────────────────────────

/// Work-hours range used for time-of-day pattern detection (local time).
pub(super) const WORK_HOUR_START: u8 = 8;
pub(super) const WORK_HOUR_END: u8 = 18;

/// Check if an hour falls within work hours.
pub(super) fn is_work_hour(h: u8) -> bool {
    (WORK_HOUR_START..WORK_HOUR_END).contains(&h)
}

// ────────────────────────────────────────────────────────────────────────────
// Preference types
// ────────────────────────────────────────────────────────────────────────────

/// Condition for a conditional preference pattern.
#[derive(Debug, Clone)]
pub enum PreferenceCondition {
    CostBelow(f64),
    CostAbove(f64),
    ContextBelow(u8),
    ContextAbove(u8),
    NoErrors,
    HasErrors,
    NoFileConflict,
    HasFileConflict,
    /// Time-of-day range: start_hour..end_hour (inclusive of start, exclusive of end).
    /// E.g., HourRange(8, 18) means 8:00-17:59 UTC.
    HourRange(u8, u8),
}

impl PreferenceCondition {
    /// Compact human-readable suffix for prompt rendering.
    pub fn label(&self) -> String {
        match self {
            PreferenceCondition::CostBelow(v) => format!("cost<${v:.0}"),
            PreferenceCondition::CostAbove(v) => format!("cost>${v:.0}"),
            PreferenceCondition::ContextBelow(v) => format!("ctx<{v}%"),
            PreferenceCondition::ContextAbove(v) => format!("ctx>{v}%"),
            PreferenceCondition::NoErrors => "no errors".to_string(),
            PreferenceCondition::HasErrors => "errors".to_string(),
            PreferenceCondition::NoFileConflict => "no conflict".to_string(),
            PreferenceCondition::HasFileConflict => "conflict".to_string(),
            PreferenceCondition::HourRange(start, end) => format!("{start}:00-{end}:00"),
        }
    }

    /// Serialize to JSON value.
    pub(super) fn to_json(&self) -> serde_json::Value {
        match self {
            PreferenceCondition::CostBelow(v) => {
                serde_json::json!({"type": "cost_below", "value": v})
            }
            PreferenceCondition::CostAbove(v) => {
                serde_json::json!({"type": "cost_above", "value": v})
            }
            PreferenceCondition::ContextBelow(v) => {
                serde_json::json!({"type": "context_below", "value": v})
            }
            PreferenceCondition::ContextAbove(v) => {
                serde_json::json!({"type": "context_above", "value": v})
            }
            PreferenceCondition::NoErrors => serde_json::json!({"type": "no_errors"}),
            PreferenceCondition::HasErrors => serde_json::json!({"type": "has_errors"}),
            PreferenceCondition::NoFileConflict => serde_json::json!({"type": "no_file_conflict"}),
            PreferenceCondition::HasFileConflict => {
                serde_json::json!({"type": "has_file_conflict"})
            }
            PreferenceCondition::HourRange(start, end) => {
                serde_json::json!({"type": "hour_range", "start": start, "end": end})
            }
        }
    }

    /// Parse from JSON value.
    pub(super) fn from_json(v: &serde_json::Value) -> Option<Self> {
        let typ = v.get("type")?.as_str()?;
        match typ {
            "cost_below" => Some(PreferenceCondition::CostBelow(v.get("value")?.as_f64()?)),
            "cost_above" => Some(PreferenceCondition::CostAbove(v.get("value")?.as_f64()?)),
            "context_below" => Some(PreferenceCondition::ContextBelow(
                v.get("value")?.as_u64()? as u8
            )),
            "context_above" => Some(PreferenceCondition::ContextAbove(
                v.get("value")?.as_u64()? as u8
            )),
            "no_errors" => Some(PreferenceCondition::NoErrors),
            "has_errors" => Some(PreferenceCondition::HasErrors),
            "no_file_conflict" => Some(PreferenceCondition::NoFileConflict),
            "has_file_conflict" => Some(PreferenceCondition::HasFileConflict),
            "hour_range" => {
                let start = v.get("start")?.as_u64()? as u8;
                let end = v.get("end")?.as_u64()? as u8;
                Some(PreferenceCondition::HourRange(start, end))
            }
            _ => None,
        }
    }
}

/// A distilled preference pattern learned from the decision history.
/// Compact representation: one pattern replaces many raw examples.
/// May include conditions learned from context-enriched records.
#[derive(Debug, Clone)]
pub struct PreferencePattern {
    /// The tool this pattern applies to (e.g. "Bash", "Read"), or "*" for all.
    pub tool: String,
    /// Optional command substring pattern (e.g. "rm -rf", "git push --force").
    pub command_pattern: Option<String>,
    /// What the user typically wants for this pattern.
    pub preferred_action: String,
    /// How many decisions this pattern was distilled from.
    pub sample_count: u32,
    /// Accept rate: 0.0 to 1.0.
    pub accept_rate: f64,
    /// Conditions under which this preference applies (empty = unconditional).
    pub conditions: Vec<PreferenceCondition>,
    /// Confidence in this pattern (0.0 to 1.0), higher when context-enriched.
    pub confidence: f64,
}

/// A temporal behavior pattern detected across sequential decisions.
#[derive(Debug, Clone)]
pub struct TemporalPattern {
    pub description: String,
    pub sample_count: u32,
    pub strength: f64,
}

/// Per-tool accuracy tracking for adaptive confidence thresholds.
#[derive(Debug, Clone)]
pub struct ToolAccuracy {
    pub tool: String,
    pub total: u32,
    pub correct: u32,
    /// Adaptive confidence threshold: brain must exceed this to auto-execute.
    pub confidence_threshold: f64,
}

/// The full distilled preferences object, saved to preferences.json.
#[derive(Debug, Clone)]
pub struct DistilledPreferences {
    pub patterns: Vec<PreferencePattern>,
    pub tool_accuracy: Vec<ToolAccuracy>,
    pub total_decisions: u32,
    pub overall_accuracy: f64,
    pub temporal: Vec<TemporalPattern>,
}

// ────────────────────────────────────────────────────────────────────────────
// Gini impurity and splitting
// ────────────────────────────────────────────────────────────────────────────

/// Compute Gini impurity for a binary split.
fn gini_impurity(positive: u32, negative: u32) -> f64 {
    let total = (positive + negative) as f64;
    if total == 0.0 {
        return 0.0;
    }
    let p = positive as f64 / total;
    let n = negative as f64 / total;
    1.0 - (p * p + n * n)
}

/// Try splitting a group of context-enriched decisions on a single feature.
/// Returns the best split condition pair (left, right) if information gain > 0.15.
fn best_split(decisions: &[&DecisionRecord]) -> Option<(PreferenceCondition, PreferenceCondition)> {
    // Only consider records that have context
    let enriched: Vec<(&DecisionRecord, &DecisionContext)> = decisions
        .iter()
        .filter_map(|d| d.context.as_ref().map(|ctx| (*d, ctx)))
        .collect();
    if enriched.len() < 5 {
        return None;
    }

    let total_pos = enriched.iter().filter(|(d, _)| d.is_positive()).count() as u32;
    let total_neg = enriched.iter().filter(|(d, _)| d.is_negative()).count() as u32;
    let parent_gini = gini_impurity(total_pos, total_neg);

    if parent_gini < 0.01 {
        return None; // Already pure, no split needed
    }

    let total = enriched.len() as f64;
    let mut best_gain = 0.0f64;
    let mut best_result: Option<(PreferenceCondition, PreferenceCondition)> = None;

    // Helper: compute weighted gini for a boolean split
    let try_split = |left: &[bool], decisions: &[(&DecisionRecord, &DecisionContext)]| -> f64 {
        let mut l_pos = 0u32;
        let mut l_neg = 0u32;
        let mut r_pos = 0u32;
        let mut r_neg = 0u32;
        for (i, &is_left) in left.iter().enumerate() {
            let positive = decisions[i].0.is_positive();
            if is_left {
                if positive {
                    l_pos += 1;
                } else {
                    l_neg += 1;
                }
            } else if positive {
                r_pos += 1;
            } else {
                r_neg += 1;
            }
        }
        let l_total = (l_pos + l_neg) as f64;
        let r_total = (r_pos + r_neg) as f64;
        if l_total == 0.0 || r_total == 0.0 {
            return 0.0; // Degenerate split
        }
        let weighted = (l_total / total) * gini_impurity(l_pos, l_neg)
            + (r_total / total) * gini_impurity(r_pos, r_neg);
        parent_gini - weighted
    };

    // Split on cost_usd median
    {
        let mut costs: Vec<f64> = enriched.iter().map(|(_, ctx)| ctx.cost_usd).collect();
        costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = costs[costs.len() / 2];
        if median > 0.0 {
            let left_mask: Vec<bool> = enriched
                .iter()
                .map(|(_, ctx)| ctx.cost_usd < median)
                .collect();
            let gain = try_split(&left_mask, &enriched);
            if gain > best_gain {
                best_gain = gain;
                best_result = Some((
                    PreferenceCondition::CostBelow(median),
                    PreferenceCondition::CostAbove(median),
                ));
            }
        }
    }

    // Split on context_pct median
    {
        let mut pcts: Vec<u8> = enriched.iter().map(|(_, ctx)| ctx.context_pct).collect();
        pcts.sort();
        let median = pcts[pcts.len() / 2];
        if median > 0 && median < 100 {
            let left_mask: Vec<bool> = enriched
                .iter()
                .map(|(_, ctx)| ctx.context_pct < median)
                .collect();
            let gain = try_split(&left_mask, &enriched);
            if gain > best_gain {
                best_gain = gain;
                best_result = Some((
                    PreferenceCondition::ContextBelow(median),
                    PreferenceCondition::ContextAbove(median),
                ));
            }
        }
    }

    // Split on last_tool_error
    {
        let left_mask: Vec<bool> = enriched
            .iter()
            .map(|(_, ctx)| !ctx.last_tool_error)
            .collect();
        let gain = try_split(&left_mask, &enriched);
        if gain > best_gain {
            best_gain = gain;
            best_result = Some((
                PreferenceCondition::NoErrors,
                PreferenceCondition::HasErrors,
            ));
        }
    }

    // Split on has_file_conflict
    {
        let left_mask: Vec<bool> = enriched
            .iter()
            .map(|(_, ctx)| !ctx.has_file_conflict)
            .collect();
        let gain = try_split(&left_mask, &enriched);
        if gain > best_gain {
            best_gain = gain;
            best_result = Some((
                PreferenceCondition::NoFileConflict,
                PreferenceCondition::HasFileConflict,
            ));
        }
    }

    // Split on time-of-day: work hours vs off hours (using local time)
    {
        let has_hours = enriched
            .iter()
            .filter(|(_, ctx)| ctx.hour.is_some())
            .count();
        if has_hours >= 5 {
            let left_mask: Vec<bool> = enriched
                .iter()
                .map(|(_, ctx)| ctx.hour.map(is_work_hour).unwrap_or(false))
                .collect();
            let gain = try_split(&left_mask, &enriched);
            if gain > best_gain {
                best_gain = gain;
                best_result = Some((
                    PreferenceCondition::HourRange(WORK_HOUR_START, WORK_HOUR_END),
                    PreferenceCondition::HourRange(WORK_HOUR_END, WORK_HOUR_START),
                ));
            }
        }
    }

    if best_gain > 0.15 { best_result } else { None }
}

// ────────────────────────────────────────────────────────────────────────────
// Outcome backfill and temporal patterns
// ────────────────────────────────────────────────────────────────────────────

/// Backfill outcomes by examining consecutive same-PID decision pairs.
/// If decision[i+1] has context.last_tool_error == true, decision[i] gets Error outcome.
pub fn backfill_outcomes(decisions: &mut [DecisionRecord]) {
    if decisions.len() < 2 {
        return;
    }
    // Group consecutive indices by PID
    for i in 0..decisions.len() - 1 {
        if decisions[i].pid != decisions[i + 1].pid {
            continue;
        }
        if let Some(ref next_ctx) = decisions[i + 1].context {
            if next_ctx.last_tool_error {
                let msg = next_ctx
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "tool error".to_string());
                decisions[i].outcome = Some(DecisionOutcome::Error(msg));
            } else {
                decisions[i].outcome = Some(DecisionOutcome::Success);
            }
        }
    }
}

/// Detect temporal patterns from decision history.
fn detect_temporal_patterns(decisions: &[DecisionRecord]) -> Vec<TemporalPattern> {
    let mut patterns = Vec::new();

    // --- Error streaks: 3+ consecutive errors on same PID → what users do ---
    {
        let mut streak_count = 0u32;
        let mut streak_responses = 0u32; // How many post-streak decisions exist
        let mut streak_denials = 0u32;
        let mut current_pid: u32 = 0;
        let mut error_run = 0u32;

        for d in decisions {
            if d.pid != current_pid {
                current_pid = d.pid;
                error_run = 0;
            }
            if let Some(ref ctx) = d.context {
                if ctx.last_tool_error {
                    error_run += 1;
                } else {
                    if error_run >= 3 {
                        streak_count += 1;
                        streak_responses += 1;
                        if d.is_negative() {
                            streak_denials += 1;
                        }
                    }
                    error_run = 0;
                }
            }
        }
        if streak_count >= 2 {
            let denial_rate = streak_denials as f64 / streak_responses as f64;
            if denial_rate > 0.5 {
                patterns.push(TemporalPattern {
                    description: format!(
                        "After 3+ errors: user usually denies (n={})",
                        streak_count
                    ),
                    sample_count: streak_count,
                    strength: denial_rate,
                });
            }
        }
    }

    // --- Cost pressure: rejection rate by burn rate quartile ---
    {
        let mut burn_rates: Vec<f64> = decisions
            .iter()
            .filter_map(|d| d.context.as_ref().map(|ctx| ctx.burn_rate_per_hr))
            .filter(|r| *r > 0.0)
            .collect();
        burn_rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        if burn_rates.len() >= 8 {
            let q3_idx = burn_rates.len() * 3 / 4;
            let q3_threshold = burn_rates[q3_idx];
            let high_burn: Vec<&DecisionRecord> = decisions
                .iter()
                .filter(|d| {
                    d.context
                        .as_ref()
                        .map(|ctx| ctx.burn_rate_per_hr >= q3_threshold)
                        .unwrap_or(false)
                })
                .collect();
            let decided: Vec<&&DecisionRecord> = high_burn
                .iter()
                .filter(|d| d.is_positive() || d.is_negative())
                .collect();
            if decided.len() >= 3 {
                let denied = decided.iter().filter(|d| d.is_negative()).count();
                let rate = denied as f64 / decided.len() as f64;
                if rate > 0.5 {
                    patterns.push(TemporalPattern {
                        description: format!(
                            "High burn rate (>${:.1}/hr): rejection rate {:.0}% (n={})",
                            q3_threshold,
                            rate * 100.0,
                            decided.len()
                        ),
                        sample_count: decided.len() as u32,
                        strength: rate,
                    });
                }
            }
        }
    }

    // --- Context pressure: approval rate drop when context >80% ---
    {
        let high_ctx: Vec<&DecisionRecord> = decisions
            .iter()
            .filter(|d| {
                d.context
                    .as_ref()
                    .map(|ctx| ctx.context_pct > 80)
                    .unwrap_or(false)
            })
            .collect();
        let low_ctx: Vec<&DecisionRecord> = decisions
            .iter()
            .filter(|d| {
                d.context
                    .as_ref()
                    .map(|ctx| ctx.context_pct <= 80)
                    .unwrap_or(false)
            })
            .collect();

        let high_decided: Vec<&&DecisionRecord> = high_ctx
            .iter()
            .filter(|d| d.is_positive() || d.is_negative())
            .collect();
        let low_decided: Vec<&&DecisionRecord> = low_ctx
            .iter()
            .filter(|d| d.is_positive() || d.is_negative())
            .collect();

        if high_decided.len() >= 3 && low_decided.len() >= 3 {
            let high_accept = high_decided.iter().filter(|d| d.is_positive()).count() as f64
                / high_decided.len() as f64;
            let low_accept = low_decided.iter().filter(|d| d.is_positive()).count() as f64
                / low_decided.len() as f64;
            let drop = low_accept - high_accept;
            if drop > 0.2 {
                patterns.push(TemporalPattern {
                    description: format!(
                        "Context >80%: approval drops {:.0}% vs low context (n={})",
                        drop * 100.0,
                        high_decided.len()
                    ),
                    sample_count: high_decided.len() as u32,
                    strength: drop,
                });
            }
        }
    }

    // --- Time-of-day pattern: different behavior during work vs off hours ---
    {
        let with_hour: Vec<(&DecisionRecord, u8)> = decisions
            .iter()
            .filter_map(|d| d.context.as_ref().and_then(|ctx| ctx.hour).map(|h| (d, h)))
            .filter(|(d, _)| d.is_positive() || d.is_negative())
            .collect();

        if with_hour.len() >= 8 {
            let work_hours: Vec<&(&DecisionRecord, u8)> =
                with_hour.iter().filter(|(_, h)| is_work_hour(*h)).collect();
            let off_hours: Vec<&(&DecisionRecord, u8)> = with_hour
                .iter()
                .filter(|(_, h)| !is_work_hour(*h))
                .collect();

            if work_hours.len() >= 3 && off_hours.len() >= 3 {
                let work_accept = work_hours.iter().filter(|(d, _)| d.is_positive()).count() as f64
                    / work_hours.len() as f64;
                let off_accept = off_hours.iter().filter(|(d, _)| d.is_positive()).count() as f64
                    / off_hours.len() as f64;
                let diff = (work_accept - off_accept).abs();
                if diff > 0.2 {
                    let (higher, lower, higher_rate) = if work_accept > off_accept {
                        ("work hours", "off hours", work_accept)
                    } else {
                        ("off hours", "work hours", off_accept)
                    };
                    patterns.push(TemporalPattern {
                        description: format!(
                            "More permissive during {} than {} (accept {:.0}% vs {:.0}%, n={})",
                            higher,
                            lower,
                            higher_rate * 100.0,
                            (higher_rate - diff) * 100.0,
                            with_hour.len()
                        ),
                        sample_count: with_hour.len() as u32,
                        strength: diff,
                    });
                }
            }
        }
    }

    patterns
}

// ────────────────────────────────────────────────────────────────────────────
// Distillation
// ────────────────────────────────────────────────────────────────────────────

/// Distill the decision log into compact preference patterns.
/// Groups decisions by (tool, command_keyword) and computes accept rates.
/// Enhanced with conditional splits, outcome weighting, and temporal patterns.
pub fn distill_preferences(decisions: &[DecisionRecord]) -> DistilledPreferences {
    if decisions.is_empty() {
        return DistilledPreferences {
            patterns: Vec::new(),
            tool_accuracy: Vec::new(),
            total_decisions: 0,
            overall_accuracy: 0.0,
            temporal: Vec::new(),
        };
    }

    // Backfill outcomes on a mutable copy
    let mut decisions_mut = decisions.to_vec();
    backfill_outcomes(&mut decisions_mut);

    // (total, accepted, rejected)
    type ToolCounts = (u32, u32, u32);

    // Group by tool → aggregate accept/reject counts
    let mut tool_stats: HashMap<String, ToolCounts> = HashMap::new();
    // Group decisions by (tool, command_keyword) for pattern analysis
    let mut pattern_groups: HashMap<(String, Option<String>), Vec<usize>> = HashMap::new();

    for (idx, d) in decisions_mut.iter().enumerate() {
        let tool = d.tool.clone().unwrap_or_else(|| "*".to_string());
        let cmd_key = extract_command_keyword(d.command.as_deref());

        // Tool-level stats
        let ts = tool_stats.entry(tool.clone()).or_insert((0, 0, 0));
        ts.0 += 1;
        if d.is_positive() {
            ts.1 += 1;
        } else if d.is_negative() {
            ts.2 += 1;
        }

        // Pattern-level grouping
        let key = (tool, cmd_key);
        pattern_groups.entry(key).or_default().push(idx);
    }

    // Build preference patterns (only from groups with enough data)
    let mut patterns = Vec::new();
    for ((tool, cmd_pattern), indices) in &pattern_groups {
        if indices.len() < 2 {
            continue; // Need at least 2 decisions to form a pattern
        }
        let group: Vec<&DecisionRecord> = indices.iter().map(|&i| &decisions_mut[i]).collect();
        let brain_action = group
            .first()
            .map(|d| d.brain_action.clone())
            .unwrap_or_default();

        let accepted: u32 = group.iter().filter(|d| d.is_positive()).count() as u32;
        let rejected: u32 = group.iter().filter(|d| d.is_negative()).count() as u32;
        let total = indices.len() as u32;
        let decided = accepted + rejected;
        if decided == 0 {
            continue;
        }

        // Outcome weighting: downweight accepted-but-errored decisions
        let mut weighted_accept = 0.0f64;
        let mut weighted_total = 0.0f64;
        for d in &group {
            if !d.is_positive() && !d.is_negative() {
                continue;
            }
            let weight = match (&d.outcome, d.is_positive()) {
                (Some(DecisionOutcome::Error(_)), true) => 0.3, // Accepted but broke
                (Some(DecisionOutcome::Error(_)), false) => 1.5, // Rejected rightly
                _ => 1.0,
            };
            weighted_total += weight;
            if d.is_positive() {
                weighted_accept += weight;
            }
        }
        let weighted_rate = if weighted_total > 0.0 {
            weighted_accept / weighted_total
        } else {
            accepted as f64 / decided as f64
        };

        let accept_rate = weighted_rate;

        // Check if we can split this group on context features (Level 2)
        let enriched_count = group.iter().filter(|d| d.context.is_some()).count();
        if enriched_count >= 5 && accept_rate > 0.3 && accept_rate < 0.7 {
            // Ambiguous overall — try splitting
            if let Some((left_cond, right_cond)) = best_split(&group) {
                // Build two conditional patterns
                for (cond, is_left) in [(left_cond, true), (right_cond, false)] {
                    let sub: Vec<&DecisionRecord> = group
                        .iter()
                        .filter(|d| {
                            d.context.as_ref().is_some_and(|ctx| match &cond {
                                PreferenceCondition::CostBelow(v) => ctx.cost_usd < *v,
                                PreferenceCondition::CostAbove(v) => ctx.cost_usd >= *v,
                                PreferenceCondition::ContextBelow(v) => ctx.context_pct < *v,
                                PreferenceCondition::ContextAbove(v) => ctx.context_pct >= *v,
                                PreferenceCondition::NoErrors => !ctx.last_tool_error,
                                PreferenceCondition::HasErrors => ctx.last_tool_error,
                                PreferenceCondition::NoFileConflict => !ctx.has_file_conflict,
                                PreferenceCondition::HasFileConflict => ctx.has_file_conflict,
                                PreferenceCondition::HourRange(start, end) => {
                                    if let Some(h) = ctx.hour {
                                        if start <= end {
                                            h >= *start && h < *end
                                        } else {
                                            // Wraps midnight: e.g., 18..8 means 18-23 or 0-7
                                            h >= *start || h < *end
                                        }
                                    } else {
                                        false
                                    }
                                }
                            })
                        })
                        .copied()
                        .collect();
                    let sub_acc = sub.iter().filter(|d| d.is_positive()).count() as u32;
                    let sub_rej = sub.iter().filter(|d| d.is_negative()).count() as u32;
                    let sub_dec = sub_acc + sub_rej;
                    if sub_dec < 2 {
                        continue;
                    }
                    let sub_rate = sub_acc as f64 / sub_dec as f64;
                    let preferred = if sub_rate >= 0.7 {
                        if brain_action.is_empty() {
                            "approve".to_string()
                        } else {
                            brain_action.clone()
                        }
                    } else if sub_rate <= 0.3 {
                        if brain_action == "approve" || brain_action.is_empty() {
                            "deny".to_string()
                        } else {
                            "approve".to_string()
                        }
                    } else {
                        continue; // Still ambiguous after split
                    };
                    let _ = is_left; // suppress unused warning
                    patterns.push(PreferencePattern {
                        tool: tool.clone(),
                        command_pattern: cmd_pattern.clone(),
                        preferred_action: preferred,
                        sample_count: sub.len() as u32,
                        accept_rate: sub_rate,
                        conditions: vec![cond],
                        confidence: (sub_rate - 0.5).abs() * 2.0,
                    });
                }
                continue; // Skip unconditional pattern for this group
            }
        }

        // No split or not enough context data — unconditional pattern
        let preferred = if accept_rate >= 0.7 {
            if brain_action.is_empty() {
                "approve".to_string()
            } else {
                brain_action.clone()
            }
        } else if accept_rate <= 0.3 {
            if brain_action == "approve" || brain_action.is_empty() {
                "deny".to_string()
            } else {
                "approve".to_string()
            }
        } else {
            continue; // Ambiguous — don't form a pattern
        };

        patterns.push(PreferencePattern {
            tool: tool.clone(),
            command_pattern: cmd_pattern.clone(),
            preferred_action: preferred,
            sample_count: total,
            accept_rate,
            conditions: Vec::new(),
            confidence: (accept_rate - 0.5).abs() * 2.0,
        });
    }

    // Sort patterns: most confident first (further from 0.5)
    patterns.sort_by(|a, b| {
        let a_strength = (a.accept_rate - 0.5).abs();
        let b_strength = (b.accept_rate - 0.5).abs();
        b_strength
            .partial_cmp(&a_strength)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Build per-tool accuracy and adaptive thresholds
    let mut tool_accuracy = Vec::new();
    for (tool, (total, correct, _rejected)) in &tool_stats {
        let decided = correct + _rejected;
        let accuracy = if decided > 0 {
            *correct as f64 / decided as f64
        } else {
            1.0 // No feedback yet, assume good
        };

        // Adaptive threshold: lower accuracy → higher confidence required
        // Base threshold 0.6, scales up to 0.95 as accuracy drops
        let threshold = if decided < 3 {
            0.6 // Not enough data, use default
        } else if accuracy >= 0.9 {
            0.5 // Brain is very accurate here, trust it more
        } else if accuracy >= 0.7 {
            0.7 // Decent accuracy, moderate threshold
        } else if accuracy >= 0.5 {
            0.85 // Shaky accuracy, be cautious
        } else {
            0.95 // Brain is mostly wrong here, very high bar
        };

        tool_accuracy.push(ToolAccuracy {
            tool: tool.clone(),
            total: *total,
            correct: *correct,
            confidence_threshold: threshold,
        });
    }

    let total_decided: u32 = tool_stats.values().map(|(_, a, r)| a + r).sum();
    let total_correct: u32 = tool_stats.values().map(|(_, a, _)| *a).sum();
    let overall_accuracy = if total_decided > 0 {
        total_correct as f64 / total_decided as f64
    } else {
        0.0
    };

    // Detect temporal patterns (Level 4)
    let temporal = detect_temporal_patterns(&decisions_mut);

    DistilledPreferences {
        patterns,
        tool_accuracy,
        total_decisions: decisions.len() as u32,
        overall_accuracy,
        temporal,
    }
}

/// Extract a command keyword for pattern grouping.
/// e.g., "rm -rf /tmp/foo" → "rm -rf", "cargo test --release" → "cargo test"
pub(super) fn extract_command_keyword(command: Option<&str>) -> Option<String> {
    let cmd = command?.trim();
    if cmd.is_empty() {
        return None;
    }
    // Take first two tokens as the keyword (captures "rm -rf", "git push", "cargo test")
    let tokens: Vec<&str> = cmd.split_whitespace().take(2).collect();
    Some(tokens.join(" "))
}

// ────────────────────────────────────────────────────────────────────────────
// Format preference summary
// ────────────────────────────────────────────────────────────────────────────

/// Format distilled preferences as a compact prompt section.
/// This replaces verbose few-shot examples for small context windows.
pub fn format_preference_summary(prefs: &DistilledPreferences) -> String {
    if prefs.patterns.is_empty() && prefs.tool_accuracy.is_empty() && prefs.temporal.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();

    // Overall accuracy context
    if prefs.total_decisions >= 5 {
        lines.push(format!(
            "Overall brain accuracy: {:.0}% ({} decisions)",
            prefs.overall_accuracy * 100.0,
            prefs.total_decisions,
        ));
    }

    // Compact preference rules (most impactful first)
    if !prefs.patterns.is_empty() {
        lines.push("User preferences:".to_string());
        for p in prefs.patterns.iter().take(10) {
            let cmd_part = p
                .command_pattern
                .as_ref()
                .map(|c| format!(" \"{c}\""))
                .unwrap_or_default();
            let strength = if p.accept_rate >= 0.9 || p.accept_rate <= 0.1 {
                "always"
            } else if p.accept_rate >= 0.7 || p.accept_rate <= 0.3 {
                "usually"
            } else {
                "sometimes"
            };
            let cond_suffix = if p.conditions.is_empty() {
                String::new()
            } else {
                let conds: Vec<String> = p.conditions.iter().map(|c| c.label()).collect();
                format!(" when {}", conds.join(", "))
            };
            lines.push(format!(
                "- {strength} {} [{}]{cmd_part}{cond_suffix} (n={})",
                p.preferred_action, p.tool, p.sample_count,
            ));
        }
    }

    // Per-tool accuracy warnings (only for tools where brain struggles)
    let weak_tools: Vec<&ToolAccuracy> = prefs
        .tool_accuracy
        .iter()
        .filter(|ta| ta.total >= 3 && ta.confidence_threshold > 0.7)
        .collect();
    if !weak_tools.is_empty() {
        lines.push("Caution areas (low accuracy):".to_string());
        for ta in weak_tools {
            let accuracy = if ta.total > 0 {
                (ta.correct as f64 / ta.total as f64) * 100.0
            } else {
                0.0
            };
            lines.push(format!(
                "- [{}]: {:.0}% accuracy, be extra careful",
                ta.tool, accuracy,
            ));
        }
    }

    // Temporal patterns (situational rules)
    if !prefs.temporal.is_empty() {
        lines.push("Situational rules:".to_string());
        for tp in &prefs.temporal {
            lines.push(format!("- {}", tp.description));
        }
    }

    lines.join("\n")
}

// ────────────────────────────────────────────────────────────────────────────
// Re-exports from pref_store (persistence layer)
// ────────────────────────────────────────────────────────────────────────────

pub use super::pref_store::{adaptive_threshold, load_preferences, load_preferences_for_project};
pub(super) use super::pref_store::{save_preferences, save_project_preferences};

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::decisions::{DecisionContext, DecisionOutcome, DecisionType};
    use super::*;

    fn make_decision(tool: &str, project: &str, user_action: &str) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some("test cmd".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: user_action.into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
        }
    }

    fn make_decision_with_cmd(
        tool: &str,
        command: &str,
        project: &str,
        user_action: &str,
    ) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some(command.into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: user_action.into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
        }
    }

    fn make_context(cost_usd: f64, context_pct: u8, last_tool_error: bool) -> DecisionContext {
        DecisionContext {
            cost_usd,
            context_pct,
            last_tool_error,
            error_message: if last_tool_error {
                Some("test error".to_string())
            } else {
                None
            },
            model: "sonnet".into(),
            elapsed_secs: 60,
            files_modified_count: 2,
            total_tool_calls: 10,
            has_file_conflict: false,
            status: "Working".into(),
            burn_rate_per_hr: 1.0,
            recent_error_count: if last_tool_error { 1 } else { 0 },
            subagent_count: 0,
            hour: None,
        }
    }

    fn make_context_with_hour(
        cost_usd: f64,
        context_pct: u8,
        last_tool_error: bool,
        hour: u8,
    ) -> DecisionContext {
        DecisionContext {
            hour: Some(hour),
            ..make_context(cost_usd, context_pct, last_tool_error)
        }
    }

    fn make_decision_with_context(
        tool: &str,
        project: &str,
        user_action: &str,
        ctx: DecisionContext,
    ) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some("test cmd".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: user_action.into(),
            context: Some(ctx),
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
        }
    }

    // ── Preference distillation tests ─────────────────────────────────

    #[test]
    fn distill_empty_returns_empty() {
        let prefs = distill_preferences(&[]);
        assert!(prefs.patterns.is_empty());
        assert!(prefs.tool_accuracy.is_empty());
        assert_eq!(prefs.total_decisions, 0);
        assert!(prefs.temporal.is_empty());
    }

    #[test]
    fn distill_builds_accept_pattern() {
        // User accepts Read 5 times → should create "always approve Read" pattern
        let decisions: Vec<DecisionRecord> = (0..5)
            .map(|_| make_decision("Read", "proj", "accept"))
            .collect();

        let prefs = distill_preferences(&decisions);
        assert!(!prefs.patterns.is_empty());

        let read_pattern = prefs.patterns.iter().find(|p| p.tool == "Read");
        assert!(read_pattern.is_some());
        let rp = read_pattern.unwrap();
        assert_eq!(rp.preferred_action, "approve");
        assert!(rp.accept_rate >= 0.9);
    }

    #[test]
    fn distill_builds_reject_pattern() {
        // User rejects Bash "rm -rf" 4 times → should create "deny" pattern
        let decisions: Vec<DecisionRecord> = (0..4)
            .map(|_| make_decision_with_cmd("Bash", "rm -rf /tmp", "proj", "reject"))
            .collect();

        let prefs = distill_preferences(&decisions);
        let rm_pattern = prefs
            .patterns
            .iter()
            .find(|p| p.command_pattern.as_deref() == Some("rm -rf"));
        assert!(rm_pattern.is_some());
        let rp = rm_pattern.unwrap();
        assert_eq!(rp.preferred_action, "deny");
        assert!(rp.accept_rate <= 0.1);
    }

    #[test]
    fn distill_skips_ambiguous_patterns() {
        // Mixed accept/reject → no clear preference, should be skipped
        let decisions = vec![
            make_decision("Bash", "proj", "accept"),
            make_decision("Bash", "proj", "reject"),
            make_decision("Bash", "proj", "accept"),
            make_decision("Bash", "proj", "reject"),
        ];

        let prefs = distill_preferences(&decisions);
        // Bash with "test cmd" pattern should NOT appear (50/50 split)
        let bash_pattern = prefs
            .patterns
            .iter()
            .find(|p| p.tool == "Bash" && p.command_pattern.as_deref() == Some("test cmd"));
        assert!(bash_pattern.is_none());
    }

    #[test]
    fn adaptive_threshold_low_accuracy() {
        // Brain is wrong most of the time for Bash → high threshold
        let decisions: Vec<DecisionRecord> = (0..10)
            .map(|i| {
                if i < 2 {
                    make_decision("Bash", "proj", "accept")
                } else {
                    make_decision("Bash", "proj", "reject")
                }
            })
            .collect();

        let prefs = distill_preferences(&decisions);
        let bash_acc = prefs.tool_accuracy.iter().find(|ta| ta.tool == "Bash");
        assert!(bash_acc.is_some());
        let ba = bash_acc.unwrap();
        // 20% accuracy → threshold should be very high (0.95)
        assert!(
            ba.confidence_threshold >= 0.9,
            "threshold was {}",
            ba.confidence_threshold
        );
    }

    #[test]
    fn adaptive_threshold_high_accuracy() {
        // Brain is right most of the time for Read → low threshold
        let decisions: Vec<DecisionRecord> = (0..10)
            .map(|_| make_decision("Read", "proj", "accept"))
            .collect();

        let prefs = distill_preferences(&decisions);
        let read_acc = prefs.tool_accuracy.iter().find(|ta| ta.tool == "Read");
        assert!(read_acc.is_some());
        let ra = read_acc.unwrap();
        // 100% accuracy → threshold should be low (0.5)
        assert!(
            ra.confidence_threshold <= 0.6,
            "threshold was {}",
            ra.confidence_threshold
        );
    }

    #[test]
    fn format_preference_summary_empty() {
        let prefs = distill_preferences(&[]);
        assert_eq!(format_preference_summary(&prefs), "");
    }

    #[test]
    fn format_preference_summary_with_patterns() {
        let decisions: Vec<DecisionRecord> = (0..8)
            .map(|_| make_decision("Read", "proj", "accept"))
            .collect();
        let prefs = distill_preferences(&decisions);
        let summary = format_preference_summary(&prefs);

        assert!(summary.contains("User preferences:"));
        assert!(summary.contains("[Read]"));
        assert!(summary.contains("approve"));
    }

    #[test]
    fn format_preference_summary_with_caution() {
        let mut decisions: Vec<DecisionRecord> = (0..8)
            .map(|_| make_decision("Bash", "proj", "reject"))
            .collect();
        // Add a few accepts so total is enough
        decisions.push(make_decision("Bash", "proj", "accept"));
        decisions.push(make_decision("Bash", "proj", "accept"));

        let prefs = distill_preferences(&decisions);
        let summary = format_preference_summary(&prefs);

        assert!(summary.contains("Caution areas"));
        assert!(summary.contains("[Bash]"));
    }

    #[test]
    fn extract_command_keyword_works() {
        assert_eq!(
            extract_command_keyword(Some("rm -rf /tmp/foo")),
            Some("rm -rf".into())
        );
        assert_eq!(
            extract_command_keyword(Some("cargo test --release")),
            Some("cargo test".into())
        );
        assert_eq!(extract_command_keyword(Some("ls")), Some("ls".into()));
        assert_eq!(extract_command_keyword(None), None);
        assert_eq!(extract_command_keyword(Some("")), None);
    }

    #[test]
    fn observations_feed_into_distillation() {
        // Mix of brain decisions and observations — all should be used
        let mut decisions: Vec<DecisionRecord> = (0..3)
            .map(|_| make_decision("Read", "proj", "accept"))
            .collect();
        decisions.extend((0..5).map(|_| make_decision("Read", "proj", "user_approve")));

        let prefs = distill_preferences(&decisions);
        // Read should show as strongly positive (8/8 positive outcomes)
        let read_pattern = prefs.patterns.iter().find(|p| p.tool == "Read");
        assert!(read_pattern.is_some());
        assert!(read_pattern.unwrap().accept_rate >= 0.9);
    }

    // ── Multi-level learning tests ───────────────────────────────────

    #[test]
    fn test_conditional_split_on_cost() {
        // Low-cost decisions: all accepted. High-cost decisions: all rejected.
        // Should produce a cost-based split.
        let mut decisions = Vec::new();
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "accept",
                make_context(1.0, 50, false),
            ));
        }
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "reject",
                make_context(10.0, 50, false),
            ));
        }

        let prefs = distill_preferences(&decisions);
        // Should have conditional patterns (split on cost)
        let conditional = prefs.patterns.iter().any(|p| !p.conditions.is_empty());
        assert!(
            conditional,
            "Expected conditional patterns from cost split, got: {:?}",
            prefs.patterns
        );
    }

    #[test]
    fn test_conditional_split_on_error() {
        // No-error decisions: all accepted. Error decisions: all rejected.
        let mut decisions = Vec::new();
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "accept",
                make_context(5.0, 50, false),
            ));
        }
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "reject",
                make_context(5.0, 50, true),
            ));
        }

        let prefs = distill_preferences(&decisions);
        let conditional = prefs.patterns.iter().any(|p| !p.conditions.is_empty());
        assert!(
            conditional,
            "Expected conditional patterns from error split, got: {:?}",
            prefs.patterns
        );
    }

    #[test]
    fn test_no_split_when_ambiguous() {
        // Even mix of accept/reject at all cost levels — no meaningful split
        let mut decisions = Vec::new();
        for i in 0..10 {
            let action = if i % 2 == 0 { "accept" } else { "reject" };
            let cost = (i as f64) + 1.0; // Different costs but same 50/50 split
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                action,
                make_context(cost, 50, false),
            ));
        }

        let prefs = distill_preferences(&decisions);
        // No patterns at all (50/50 cannot split into clear halves)
        let conditional = prefs.patterns.iter().any(|p| !p.conditions.is_empty());
        assert!(
            !conditional,
            "Expected no conditional patterns for ambiguous data"
        );
    }

    #[test]
    fn test_outcome_backfill() {
        // Two consecutive same-PID records: first accept, second has error context
        let mut decisions = vec![
            DecisionRecord {
                timestamp: "1".into(),
                pid: 42,
                project: "proj".into(),
                tool: Some("Bash".into()),
                command: Some("deploy".into()),
                brain_action: "approve".into(),
                brain_confidence: 0.9,
                brain_reasoning: "safe".into(),
                user_action: "accept".into(),
                context: Some(make_context(1.0, 50, false)),
                outcome: None,
                decision_type: DecisionType::Session,
                suggested_at: None,
            },
            DecisionRecord {
                timestamp: "2".into(),
                pid: 42,
                project: "proj".into(),
                tool: Some("Bash".into()),
                command: Some("fix".into()),
                brain_action: "approve".into(),
                brain_confidence: 0.9,
                brain_reasoning: "safe".into(),
                user_action: "accept".into(),
                context: Some(make_context(1.5, 55, true)),
                outcome: None,
                decision_type: DecisionType::Session,
                suggested_at: None,
            },
        ];

        backfill_outcomes(&mut decisions);

        // First decision should be marked as Error (next had tool error)
        assert!(matches!(
            decisions[0].outcome,
            Some(DecisionOutcome::Error(_))
        ));
        // Second has no subsequent record, so outcome stays None
        assert!(decisions[1].outcome.is_none());
    }

    #[test]
    fn test_temporal_error_streak() {
        // Build a scenario with error streaks
        let mut decisions = Vec::new();
        // 4 consecutive errors (same PID)
        for _ in 0..4 {
            decisions.push(DecisionRecord {
                timestamp: "0".into(),
                pid: 1,
                project: "proj".into(),
                tool: Some("Bash".into()),
                command: Some("test cmd".into()),
                brain_action: "approve".into(),
                brain_confidence: 0.9,
                brain_reasoning: "test".into(),
                user_action: "accept".into(),
                context: Some(make_context(1.0, 50, true)),
                outcome: None,
                decision_type: DecisionType::Session,
                suggested_at: None,
            });
        }
        // Then user denies
        decisions.push(DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: "proj".into(),
            tool: Some("Bash".into()),
            command: Some("test cmd".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: "reject".into(),
            context: Some(make_context(1.0, 50, false)),
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
        });
        // Repeat the streak pattern to reach threshold of 2
        for _ in 0..4 {
            decisions.push(DecisionRecord {
                timestamp: "0".into(),
                pid: 1,
                project: "proj".into(),
                tool: Some("Bash".into()),
                command: Some("test cmd".into()),
                brain_action: "approve".into(),
                brain_confidence: 0.9,
                brain_reasoning: "test".into(),
                user_action: "accept".into(),
                context: Some(make_context(1.0, 50, true)),
                outcome: None,
                decision_type: DecisionType::Session,
                suggested_at: None,
            });
        }
        decisions.push(DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: "proj".into(),
            tool: Some("Bash".into()),
            command: Some("test cmd".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: "reject".into(),
            context: Some(make_context(1.0, 50, false)),
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
        });

        let patterns = detect_temporal_patterns(&decisions);
        let error_streak = patterns.iter().any(|p| p.description.contains("3+ errors"));
        assert!(
            error_streak,
            "Expected error streak pattern, got: {:?}",
            patterns
        );
    }

    #[test]
    fn test_temporal_context_pressure() {
        // Low context: mostly accepted. High context: mostly rejected.
        let mut decisions = Vec::new();
        // 5 low-context accepts
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "accept",
                make_context(1.0, 30, false),
            ));
        }
        // 5 high-context rejects
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "reject",
                make_context(1.0, 90, false),
            ));
        }

        let patterns = detect_temporal_patterns(&decisions);
        let ctx_pressure = patterns
            .iter()
            .any(|p| p.description.contains("Context >80%"));
        assert!(
            ctx_pressure,
            "Expected context pressure pattern, got: {:?}",
            patterns
        );
    }

    #[test]
    fn test_gini_pure() {
        // All positive → gini = 0
        assert!((gini_impurity(10, 0) - 0.0).abs() < f64::EPSILON);
        // All negative → gini = 0
        assert!((gini_impurity(0, 10) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_gini_mixed() {
        // 50/50 → gini = 0.5
        assert!((gini_impurity(5, 5) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_gini_empty() {
        // No data → gini = 0
        assert!((gini_impurity(0, 0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_preference_condition_label() {
        assert_eq!(PreferenceCondition::CostBelow(5.0).label(), "cost<$5");
        assert_eq!(PreferenceCondition::CostAbove(10.0).label(), "cost>$10");
        assert_eq!(PreferenceCondition::ContextBelow(80).label(), "ctx<80%");
        assert_eq!(PreferenceCondition::ContextAbove(80).label(), "ctx>80%");
        assert_eq!(PreferenceCondition::NoErrors.label(), "no errors");
        assert_eq!(PreferenceCondition::HasErrors.label(), "errors");
        assert_eq!(PreferenceCondition::NoFileConflict.label(), "no conflict");
        assert_eq!(PreferenceCondition::HasFileConflict.label(), "conflict");
        assert_eq!(PreferenceCondition::HourRange(8, 18).label(), "8:00-18:00");
        assert_eq!(PreferenceCondition::HourRange(18, 8).label(), "18:00-8:00");
    }

    #[test]
    fn test_preference_condition_roundtrip() {
        let conditions = vec![
            PreferenceCondition::CostBelow(5.0),
            PreferenceCondition::CostAbove(10.0),
            PreferenceCondition::ContextBelow(80),
            PreferenceCondition::ContextAbove(80),
            PreferenceCondition::NoErrors,
            PreferenceCondition::HasErrors,
            PreferenceCondition::NoFileConflict,
            PreferenceCondition::HasFileConflict,
            PreferenceCondition::HourRange(8, 18),
            PreferenceCondition::HourRange(18, 8),
        ];
        for cond in &conditions {
            let json = cond.to_json();
            let parsed = PreferenceCondition::from_json(&json);
            assert!(parsed.is_some(), "Failed roundtrip for: {:?}", cond);
        }
    }

    #[test]
    fn test_format_summary_with_conditions() {
        let prefs = DistilledPreferences {
            patterns: vec![PreferencePattern {
                tool: "Bash".into(),
                command_pattern: Some("git push".into()),
                preferred_action: "approve".into(),
                sample_count: 8,
                accept_rate: 0.9,
                conditions: vec![PreferenceCondition::CostBelow(5.0)],
                confidence: 0.8,
            }],
            tool_accuracy: Vec::new(),
            total_decisions: 10,
            overall_accuracy: 0.8,
            temporal: Vec::new(),
        };
        let summary = format_preference_summary(&prefs);
        assert!(summary.contains("when cost<$5"));
        assert!(summary.contains("[Bash]"));
        assert!(summary.contains("git push"));
    }

    #[test]
    fn test_format_summary_with_temporal() {
        let prefs = DistilledPreferences {
            patterns: Vec::new(),
            tool_accuracy: vec![ToolAccuracy {
                tool: "Bash".into(),
                total: 5,
                correct: 1,
                confidence_threshold: 0.95,
            }],
            total_decisions: 10,
            overall_accuracy: 0.2,
            temporal: vec![TemporalPattern {
                description: "After 3+ errors: user usually denies (n=5)".into(),
                sample_count: 5,
                strength: 0.8,
            }],
        };
        let summary = format_preference_summary(&prefs);
        assert!(summary.contains("Situational rules:"));
        assert!(summary.contains("3+ errors"));
    }

    #[test]
    fn test_hour_range_condition_label() {
        assert_eq!(PreferenceCondition::HourRange(8, 18).label(), "8:00-18:00");
        assert_eq!(PreferenceCondition::HourRange(0, 8).label(), "0:00-8:00");
        assert_eq!(PreferenceCondition::HourRange(22, 6).label(), "22:00-6:00");
    }

    #[test]
    fn test_hour_range_condition_roundtrip() {
        let cond = PreferenceCondition::HourRange(8, 18);
        let json = cond.to_json();
        let parsed = PreferenceCondition::from_json(&json);
        assert!(parsed.is_some());
        match parsed.unwrap() {
            PreferenceCondition::HourRange(s, e) => {
                assert_eq!(s, 8);
                assert_eq!(e, 18);
            }
            other => panic!("Expected HourRange, got {:?}", other),
        }
    }

    #[test]
    fn test_format_summary_with_hour_condition() {
        let prefs = DistilledPreferences {
            patterns: vec![PreferencePattern {
                tool: "Bash".into(),
                command_pattern: None,
                preferred_action: "approve".into(),
                sample_count: 10,
                accept_rate: 0.9,
                conditions: vec![PreferenceCondition::HourRange(8, 18)],
                confidence: 0.8,
            }],
            tool_accuracy: Vec::new(),
            total_decisions: 15,
            overall_accuracy: 0.8,
            temporal: Vec::new(),
        };
        let summary = format_preference_summary(&prefs);
        assert!(
            summary.contains("8:00-18:00"),
            "Expected hour range in summary, got: {summary}"
        );
    }

    #[test]
    fn test_conditional_split_on_hour() {
        // Work hours: all accepted. Off hours: all rejected.
        let mut decisions = Vec::new();
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "accept",
                make_context_with_hour(5.0, 50, false, 10), // 10:00 = work hours
            ));
        }
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "reject",
                make_context_with_hour(5.0, 50, false, 22), // 22:00 = off hours
            ));
        }

        let prefs = distill_preferences(&decisions);
        let has_hour_cond = prefs.patterns.iter().any(|p| {
            p.conditions
                .iter()
                .any(|c| matches!(c, PreferenceCondition::HourRange(_, _)))
        });
        assert!(
            has_hour_cond,
            "Expected HourRange condition in patterns, got: {:?}",
            prefs.patterns
        );
    }

    #[test]
    fn test_temporal_time_of_day_pattern() {
        // Work hours: mostly accepted. Off hours: mostly rejected.
        let mut decisions = Vec::new();
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "accept",
                make_context_with_hour(1.0, 50, false, 10),
            ));
        }
        for _ in 0..5 {
            decisions.push(make_decision_with_context(
                "Bash",
                "proj",
                "reject",
                make_context_with_hour(1.0, 50, false, 22),
            ));
        }

        let patterns = detect_temporal_patterns(&decisions);
        let time_pattern = patterns
            .iter()
            .any(|p| p.description.contains("permissive during"));
        assert!(
            time_pattern,
            "Expected time-of-day temporal pattern, got: {:?}",
            patterns
        );
    }
}

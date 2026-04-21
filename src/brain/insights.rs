#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use super::decisions::{DecisionRecord, DistilledPreferences};

// ────────────────────────────────────────────────────────────────────────────
// Data structures
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InsightCategory {
    FrictionPattern,
    ErrorLoop,
    ContextBlowout,
    MissingRule,
    AccuracyGap,
    TemporalFriction,
    CostPattern,
}

impl InsightCategory {
    fn label(&self) -> &'static str {
        match self {
            InsightCategory::FrictionPattern => "friction_pattern",
            InsightCategory::ErrorLoop => "error_loop",
            InsightCategory::ContextBlowout => "context_blowout",
            InsightCategory::MissingRule => "missing_rule",
            InsightCategory::AccuracyGap => "accuracy_gap",
            InsightCategory::TemporalFriction => "temporal_friction",
            InsightCategory::CostPattern => "cost_pattern",
        }
    }

    fn from_label(s: &str) -> Option<Self> {
        match s {
            "friction_pattern" => Some(InsightCategory::FrictionPattern),
            "error_loop" => Some(InsightCategory::ErrorLoop),
            "context_blowout" => Some(InsightCategory::ContextBlowout),
            "missing_rule" => Some(InsightCategory::MissingRule),
            "accuracy_gap" => Some(InsightCategory::AccuracyGap),
            "temporal_friction" => Some(InsightCategory::TemporalFriction),
            "cost_pattern" => Some(InsightCategory::CostPattern),
            _ => None,
        }
    }

    fn display_name(&self) -> &'static str {
        match self {
            InsightCategory::FrictionPattern => "Friction Patterns",
            InsightCategory::ErrorLoop => "Error Loops",
            InsightCategory::ContextBlowout => "Context Blowouts",
            InsightCategory::MissingRule => "Recommended Rules",
            InsightCategory::AccuracyGap => "Accuracy Gaps",
            InsightCategory::TemporalFriction => "Temporal Patterns",
            InsightCategory::CostPattern => "Cost Patterns",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InsightSeverity {
    Info,
    Suggestion,
    Warning,
    Critical,
}

impl InsightSeverity {
    fn label(&self) -> &'static str {
        match self {
            InsightSeverity::Info => "info",
            InsightSeverity::Suggestion => "suggestion",
            InsightSeverity::Warning => "warning",
            InsightSeverity::Critical => "critical",
        }
    }

    fn from_label(s: &str) -> Self {
        match s {
            "critical" => InsightSeverity::Critical,
            "warning" => InsightSeverity::Warning,
            "suggestion" => InsightSeverity::Suggestion,
            _ => InsightSeverity::Info,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Insight {
    pub fingerprint: String,
    pub generated_at: u64,
    pub category: InsightCategory,
    pub severity: InsightSeverity,
    pub summary: String,
    pub suggestion: Option<String>,
    pub evidence_count: u32,
}

pub struct InsightState {
    pub seen_fingerprints: HashSet<String>,
    pub last_generated: u64,
    pub current_insights: Vec<Insight>,
}

// ────────────────────────────────────────────────────────────────────────────
// Mode toggle (on/off)
// ────────────────────────────────────────────────────────────────────────────

fn insights_mode_path() -> PathBuf {
    super::decisions::decisions_dir().join("insights-mode")
}

/// Read the current insights mode. Returns "off" if no file exists (opt-in).
pub fn read_insights_mode() -> String {
    let path = insights_mode_path();
    fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "off".into())
}

/// Write the insights mode to disk.
pub fn write_insights_mode(mode: &str) -> Result<(), String> {
    let path = insights_mode_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if mode == "off" {
        let _ = fs::remove_file(&path);
        Ok(())
    } else {
        fs::write(&path, mode).map_err(|e| format!("write error: {e}"))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Persistence
// ────────────────────────────────────────────────────────────────────────────

fn insights_path() -> PathBuf {
    super::decisions::decisions_dir().join("insights.json")
}

pub(super) fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn load_state() -> InsightState {
    let path = insights_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return InsightState {
                seen_fingerprints: HashSet::new(),
                last_generated: 0,
                current_insights: Vec::new(),
            };
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => {
            return InsightState {
                seen_fingerprints: HashSet::new(),
                last_generated: 0,
                current_insights: Vec::new(),
            };
        }
    };

    let seen = json
        .get("seen_fingerprints")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();

    let last_generated = json
        .get("last_generated")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let current = json
        .get("current_insights")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(parse_insight_json).collect())
        .unwrap_or_default();

    InsightState {
        seen_fingerprints: seen,
        last_generated,
        current_insights: current,
    }
}

pub fn save_state(state: &InsightState) -> Result<(), String> {
    let path = insights_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let json = serde_json::json!({
        "seen_fingerprints": state.seen_fingerprints.iter().collect::<Vec<_>>(),
        "last_generated": state.last_generated,
        "current_insights": state.current_insights.iter().map(insight_to_json).collect::<Vec<_>>(),
    });

    fs::write(
        &path,
        serde_json::to_string_pretty(&json).map_err(|e| format!("json error: {e}"))?,
    )
    .map_err(|e| format!("write error: {e}"))
}

fn insight_to_json(i: &Insight) -> serde_json::Value {
    serde_json::json!({
        "fingerprint": i.fingerprint,
        "generated_at": i.generated_at,
        "category": i.category.label(),
        "severity": i.severity.label(),
        "summary": i.summary,
        "suggestion": i.suggestion,
        "evidence_count": i.evidence_count,
    })
}

fn parse_insight_json(v: &serde_json::Value) -> Option<Insight> {
    Some(Insight {
        fingerprint: v.get("fingerprint")?.as_str()?.to_string(),
        generated_at: v.get("generated_at")?.as_u64()?,
        category: InsightCategory::from_label(v.get("category")?.as_str()?)?,
        severity: InsightSeverity::from_label(
            v.get("severity").and_then(|s| s.as_str()).unwrap_or("info"),
        ),
        summary: v.get("summary")?.as_str()?.to_string(),
        suggestion: v
            .get("suggestion")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        evidence_count: v.get("evidence_count")?.as_u64()? as u32,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Differential merging
// ────────────────────────────────────────────────────────────────────────────

/// Merge newly generated insights with existing state.
/// Returns only the insights that are NEW (unseen fingerprints).
/// Updates `state.seen_fingerprints` and `state.current_insights`.
pub fn merge_insights(generated: Vec<Insight>, state: &mut InsightState) -> Vec<Insight> {
    // Prune seen fingerprints that are no longer in the current set
    let current_fps: HashSet<String> = generated.iter().map(|i| i.fingerprint.clone()).collect();
    state
        .seen_fingerprints
        .retain(|fp| current_fps.contains(fp));

    // Filter to only unseen
    let new: Vec<Insight> = generated
        .iter()
        .filter(|i| !state.seen_fingerprints.contains(&i.fingerprint))
        .cloned()
        .collect();

    // Mark new as seen
    for i in &new {
        state.seen_fingerprints.insert(i.fingerprint.clone());
    }

    state.current_insights = generated;
    state.last_generated = epoch_now();
    new
}

// Import detectors for use by generate_insights()
use super::detectors::{
    detect_accuracy_gaps, detect_context_blowouts, detect_cost_patterns, detect_error_loops,
    detect_friction_patterns, detect_missing_rules, detect_temporal_friction,
};

// ────────────────────────────────────────────────────────────────────────────
// Main generation entry point
// ────────────────────────────────────────────────────────────────────────────

/// Generate all insights from the decision history and distilled preferences.
/// Runs all detectors, sorts results by severity (critical first).
pub fn generate_insights(
    decisions: &[DecisionRecord],
    prefs: &DistilledPreferences,
) -> Vec<Insight> {
    let mut insights = Vec::new();
    insights.extend(detect_friction_patterns(decisions));
    insights.extend(detect_error_loops(decisions));
    insights.extend(detect_context_blowouts(decisions));
    insights.extend(detect_missing_rules(decisions, prefs));
    insights.extend(detect_accuracy_gaps(prefs));
    insights.extend(detect_temporal_friction(prefs));
    insights.extend(detect_cost_patterns(decisions));

    // Sort by severity descending, then by evidence count descending
    insights.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| b.evidence_count.cmp(&a.evidence_count))
    });

    insights
}

// ────────────────────────────────────────────────────────────────────────────
// Formatting
// ────────────────────────────────────────────────────────────────────────────

/// Format a list of insights grouped by category.
fn format_insights(insights: &[Insight], header: &str) -> String {
    if insights.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    lines.push(header.to_string());
    lines.push("\u{2500}".repeat(header.len()));
    lines.push(String::new());

    // Group by category (preserving order of first appearance)
    let mut categories: Vec<InsightCategory> = Vec::new();
    let mut by_category: HashMap<InsightCategory, Vec<&Insight>> = HashMap::new();

    // Determine category order from enum variants for consistent display
    let category_order = [
        InsightCategory::FrictionPattern,
        InsightCategory::ErrorLoop,
        InsightCategory::ContextBlowout,
        InsightCategory::MissingRule,
        InsightCategory::AccuracyGap,
        InsightCategory::TemporalFriction,
        InsightCategory::CostPattern,
    ];

    for i in insights {
        by_category.entry(i.category).or_default().push(i);
    }

    for cat in &category_order {
        if let Some(group) = by_category.get(cat) {
            if !categories.contains(cat) {
                categories.push(*cat);
            }
            lines.push(format!("  {}", cat.display_name()));
            for insight in group {
                lines.push(format!("  - {}", insight.summary));
                if let Some(ref suggestion) = insight.suggestion {
                    lines.push(format!("    \u{2192} {suggestion}"));
                }
            }
            lines.push(String::new());
        }
    }

    lines.join("\n")
}

// ────────────────────────────────────────────────────────────────────────────
// CLI handler
// ────────────────────────────────────────────────────────────────────────────

/// Print insights to stdout. Called from main.rs --insights handler.
pub fn print_insights() {
    let decisions = super::decisions::read_all_decisions();
    if decisions.is_empty() {
        println!("No decision history yet. Use claudectl with --brain to build history.");
        return;
    }

    let prefs = super::decisions::load_preferences()
        .unwrap_or_else(|| super::decisions::distill_preferences(&decisions));

    let insights = generate_insights(&decisions, &prefs);
    let mut state = load_state();
    let new_insights = merge_insights(insights, &mut state);
    let _ = save_state(&state);

    if state.current_insights.is_empty() {
        println!("No insights detected. Keep using claudectl to build more history.");
        return;
    }

    let mode = read_insights_mode();
    println!(
        "Insights mode: {mode}{}",
        if mode == "off" {
            " (run claudectl --brain --insights on to enable auto-generation)"
        } else {
            ""
        }
    );
    println!();

    if !new_insights.is_empty() {
        print!(
            "{}",
            format_insights(
                &new_insights,
                &format!("New Insights ({} new)", new_insights.len()),
            )
        );
    }

    print!(
        "{}",
        format_insights(
            &state.current_insights,
            &format!("All Insights ({} total)", state.current_insights.len()),
        )
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::decisions::{
        DecisionContext, DecisionType, DistilledPreferences, PreferencePattern, ToolAccuracy,
    };

    fn make_decision(tool: &str, command: &str, user_action: &str, pid: u32) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".to_string(),
            pid,
            project: "test".to_string(),
            tool: Some(tool.to_string()),
            command: Some(command.to_string()),
            brain_action: "approve".to_string(),
            brain_confidence: 0.8,
            brain_reasoning: String::new(),
            user_action: user_action.to_string(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_decision_with_context(
        tool: &str,
        command: &str,
        user_action: &str,
        pid: u32,
        context_pct: u8,
        last_error: bool,
        burn_rate: f64,
        cost: f64,
    ) -> DecisionRecord {
        let mut d = make_decision(tool, command, user_action, pid);
        d.context = Some(DecisionContext {
            cost_usd: cost,
            context_pct,
            last_tool_error: last_error,
            error_message: None,
            model: "test".to_string(),
            elapsed_secs: 100,
            files_modified_count: 0,
            total_tool_calls: 10,
            has_file_conflict: false,
            status: "Processing".to_string(),
            burn_rate_per_hr: burn_rate,
            recent_error_count: 0,
            subagent_count: 0,
            hour: Some(10),
        });
        d
    }

    fn empty_prefs() -> DistilledPreferences {
        DistilledPreferences {
            patterns: Vec::new(),
            tool_accuracy: Vec::new(),
            total_decisions: 0,
            overall_accuracy: 0.0,
            temporal: Vec::new(),
        }
    }

    #[test]
    fn test_differential_merging() {
        let insights = vec![
            Insight {
                fingerprint: "friction:Bash:npm install".to_string(),
                generated_at: 100,
                category: InsightCategory::FrictionPattern,
                severity: InsightSeverity::Warning,
                summary: "test".to_string(),
                suggestion: None,
                evidence_count: 5,
            },
            Insight {
                fingerprint: "accuracy_gap:Edit".to_string(),
                generated_at: 100,
                category: InsightCategory::AccuracyGap,
                severity: InsightSeverity::Suggestion,
                summary: "test2".to_string(),
                suggestion: None,
                evidence_count: 3,
            },
        ];

        let mut state = InsightState {
            seen_fingerprints: HashSet::new(),
            last_generated: 0,
            current_insights: Vec::new(),
        };

        // First merge: both are new
        let new = merge_insights(insights.clone(), &mut state);
        assert_eq!(new.len(), 2);
        assert_eq!(state.seen_fingerprints.len(), 2);

        // Second merge: none are new (already seen)
        let new2 = merge_insights(insights.clone(), &mut state);
        assert_eq!(new2.len(), 0);
        assert_eq!(state.current_insights.len(), 2);
    }

    #[test]
    fn test_stale_fingerprints_pruned() {
        let mut state = InsightState {
            seen_fingerprints: {
                let mut s = HashSet::new();
                s.insert("old_fingerprint".to_string());
                s.insert("accuracy_gap:Edit".to_string());
                s
            },
            last_generated: 0,
            current_insights: Vec::new(),
        };

        // Only one insight — the old_fingerprint should be pruned
        let insights = vec![Insight {
            fingerprint: "accuracy_gap:Edit".to_string(),
            generated_at: 100,
            category: InsightCategory::AccuracyGap,
            severity: InsightSeverity::Suggestion,
            summary: "test".to_string(),
            suggestion: None,
            evidence_count: 3,
        }];

        let _new = merge_insights(insights, &mut state);
        assert!(!state.seen_fingerprints.contains("old_fingerprint"));
        assert!(state.seen_fingerprints.contains("accuracy_gap:Edit"));
    }

    #[test]
    fn test_generate_insights_sorts_by_severity() {
        let prefs = DistilledPreferences {
            patterns: vec![PreferencePattern {
                tool: "Bash".to_string(),
                command_pattern: Some("cargo test".to_string()),
                preferred_action: "approve".to_string(),
                sample_count: 15,
                accept_rate: 1.0,
                conditions: Vec::new(),
                confidence: 1.0,
            }],
            tool_accuracy: vec![ToolAccuracy {
                tool: "Edit".to_string(),
                total: 10,
                correct: 3,
                confidence_threshold: 0.9,
            }],
            total_decisions: 25,
            overall_accuracy: 0.5,
            temporal: Vec::new(),
        };

        let mut decisions = Vec::new();
        // Add friction pattern (Warning severity)
        for i in 0..10 {
            decisions.push(make_decision("Bash", "npm install", "reject", i));
        }

        let insights = generate_insights(&decisions, &prefs);
        assert!(!insights.is_empty());

        // Verify sorted: warnings before suggestions
        for window in insights.windows(2) {
            assert!(window[0].severity >= window[1].severity);
        }
    }

    #[test]
    fn test_empty_decisions_no_insights() {
        let insights = generate_insights(&[], &empty_prefs());
        assert!(insights.is_empty());
    }

    #[test]
    fn test_format_insights_output() {
        let insights = vec![Insight {
            fingerprint: "friction:Bash:npm install".to_string(),
            generated_at: 100,
            category: InsightCategory::FrictionPattern,
            severity: InsightSeverity::Warning,
            summary: "[Bash] \"npm install\" rejected 8/10 times".to_string(),
            suggestion: Some("consider adding deny rule".to_string()),
            evidence_count: 10,
        }];

        let output = format_insights(&insights, "Test Header");
        assert!(output.contains("Friction Patterns"));
        assert!(output.contains("npm install"));
        assert!(output.contains("consider adding deny rule"));
    }
}

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use claudectl_core::runtime::DecisionSummary;

use crate::brain::decisions::{DecisionRecord, read_all_decisions};

// Behavior-preserving split of the former monolithic metrics.rs, grouped by
// metric family. dispatch() + print_scorecard() below route to the print_*
// reports; the compute_* helpers and summary structs are re-exported for the
// Brain Review screen and review queue.
mod accuracy;
mod approvals;
mod perf;
use accuracy::*;
pub use approvals::compute_counterfactuals;
use approvals::*;
use perf::*;
pub use perf::{
    CacheSummary, LatencySummary, TierStats, compute_cache, compute_latency, compute_tier_stats,
};

/// Read every decision on disk and project to the core `DecisionSummary`
/// DTO in one pass. Used by every `print_*` and `compute_*` helper below —
/// the on-disk shape stays `DecisionRecord`, but the metrics pipeline
/// operates on the summary form so the surface can be shared with the TUI
/// (`ui/brain.rs`) without depending on brain-private types.
pub(crate) fn read_all_summaries() -> Vec<DecisionSummary> {
    read_all_decisions()
        .iter()
        .map(DecisionSummary::from)
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Re-exports from sub-modules so that existing `brain::metrics::*` paths
// continue to resolve without changes to callers.
// ────────────────────────────────────────────────────────────────────────────

#[allow(unused_imports)]
pub use crate::brain::risk::{RiskTier, classify_risk};

#[allow(unused_imports)]
pub use crate::brain::baseline::{print_baseline, rules_baseline_classify};

// ────────────────────────────────────────────────────────────────────────────
// Rolling window computation
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #129: Correction rate learning curve
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #131: Category-specific accuracy breakdown
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #133: False-approve rate on risky actions
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #128: Decision distribution analysis
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #130: Novel situation rate tracking
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #134: False-deny rate and friction cost
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #135: Confidence calibration
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #140: Incident post-mortem framework for false approvals
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #132: Time-to-correct analysis
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// #170: Impact scorecard
// ────────────────────────────────────────────────────────────────────────────

/// Render a horizontal bar using Unicode block characters.
/// `value` is 0.0–1.0, `width` is the total bar width in characters.
pub(crate) fn render_bar(value: f64, width: usize) -> String {
    let filled = (value.clamp(0.0, 1.0) * width as f64) as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

/// Format a time duration in human-friendly units.
pub(crate) fn format_time_saved(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{:.0}s", secs)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Brain evolution visualization
// ────────────────────────────────────────────────────────────────────────────

/// Render a sparkline from a slice of 0.0–1.0 values using Unicode braille-style blocks.
pub(crate) fn sparkline(values: &[f64], width: usize) -> String {
    if values.is_empty() {
        return " ".repeat(width);
    }
    let blocks = [
        ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];
    let step = values.len() as f64 / width as f64;
    let mut result = String::with_capacity(width);
    for i in 0..width {
        let idx = (i as f64 * step) as usize;
        let val = values.get(idx).copied().unwrap_or(0.0).clamp(0.0, 1.0);
        let block_idx = (val * 8.0) as usize;
        result.push(blocks[block_idx.min(8)]);
    }
    result
}

// ────────────────────────────────────────────────────────────────────────────
// Per-risk-tier breakdown
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// Latency
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// Cache hit rate
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// Counterfactual analyzer
// ────────────────────────────────────────────────────────────────────────────

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Composite scorecard
// ────────────────────────────────────────────────────────────────────────────

/// Render every key metric in one screen — the periodic-review surface.
pub fn print_scorecard() {
    let decisions = read_all_summaries();

    println!("Brain Scorecard");
    println!("===============");
    println!();

    // North-star: auto-handled accuracy.
    let total_with_brain = decisions.iter().filter(|d| !d.action.is_empty()).count();
    let correct = decisions
        .iter()
        .filter(|d| !d.action.is_empty() && d.is_positive())
        .count();
    let north_star = if total_with_brain > 0 {
        (correct as f64 / total_with_brain as f64) * 100.0
    } else {
        0.0
    };

    println!("NORTH STAR");
    if total_with_brain == 0 {
        println!("  Auto-handled accuracy:  —  (no brain decisions yet)");
    } else {
        let marker = if north_star >= 85.0 { "✓" } else { "⚠" };
        println!(
            "  Auto-handled accuracy:  {:.1}% {}   (n = {}, target ≥ 85%)",
            north_star, marker, total_with_brain
        );
    }
    println!();

    // Guardrails.
    println!("GUARDRAILS");
    let tier_stats = compute_tier_stats(&decisions);
    let critical = tier_stats
        .iter()
        .find(|s| matches!(s.tier, RiskTier::Critical));
    match critical {
        Some(s) if s.n > 0 => {
            let marker = if s.false_approves == 0 { "✓" } else { "✗" };
            println!(
                "  False-approve on Critical tier:  {} of {} ({:.1}%) {}   target = 0",
                s.false_approves,
                s.n,
                s.false_approve_pct(),
                marker
            );
        }
        _ => {
            println!("  False-approve on Critical tier:  no Critical samples yet");
        }
    }

    let override_window: Vec<&DecisionSummary> = decisions
        .iter()
        .rev()
        .filter(|d| !d.action.is_empty())
        .take(50)
        .collect();
    if !override_window.is_empty() {
        let n_overrides = override_window.iter().filter(|d| d.is_negative()).count();
        let rate = (n_overrides as f64 / override_window.len() as f64) * 100.0;
        let marker = if rate < 20.0 { "✓" } else { "⚠" };
        println!(
            "  Override rate (last 50):         {:.1}% {}   target ↓ (learning)",
            rate, marker
        );
    }
    println!();

    // Latency
    let lat = compute_latency(&decisions);
    println!("LATENCY");
    if lat.n == 0 {
        println!("  No instrumented samples yet — see `brain latency` after some traffic.");
    } else {
        let marker = if lat.p95_ms <= 1000 { "✓" } else { "⚠" };
        println!(
            "  p50 {} ms  |  p95 {} ms {}  |  p99 {} ms  |  n = {}",
            lat.p50_ms, lat.p95_ms, marker, lat.p99_ms, lat.n
        );
    }
    println!();

    // Cache hit rate
    let cache = compute_cache(&decisions);
    println!("CACHE HIT RATE");
    if cache.instrumented == 0 {
        println!("  No instrumented samples yet — see `brain cache` after some traffic.");
    } else {
        println!(
            "  {:.1}%  ({} of {} decisions handled without an LLM call)",
            cache.hit_rate(),
            cache.hits,
            cache.instrumented
        );
    }
    println!();

    // Per-tier accuracy
    println!("PER-RISK-TIER ACCURACY");
    for s in &tier_stats {
        if s.n == 0 {
            println!("  {:<10}  n = 0", s.tier.label());
        } else {
            println!(
                "  {:<10}  {:.1}%   n = {}",
                s.tier.label(),
                s.accuracy_pct(),
                s.n
            );
        }
    }
    println!();

    // Counterfactual summary
    let cfs = compute_counterfactuals(&decisions);
    let brain_right = cfs.iter().filter(|c| c.brain_was_right).count();
    let user_right = cfs.len() - brain_right;
    println!("COUNTERFACTUAL HITS");
    println!(
        "  Brain was right (user override → failure):  {}",
        brain_right
    );
    println!(
        "  User was right (brain over-cautious):       {}",
        user_right
    );
    println!();

    // Review status
    let canonical_count = decisions
        .iter()
        .filter(|d| d.canonical == Some(true))
        .count();
    println!("REVIEW STATUS");
    println!("  Total decisions:       {}", decisions.len());
    println!(
        "  Marked canonical:      {} ({:.1}% of total)",
        canonical_count,
        if decisions.is_empty() {
            0.0
        } else {
            (canonical_count as f64 / decisions.len() as f64) * 100.0
        }
    );
    println!();

    println!("→ Run `claudectl brain review` to triage the highest-value cases.");
}

// ────────────────────────────────────────────────────────────────────────────
// Dispatch
// ────────────────────────────────────────────────────────────────────────────

/// Dispatch a brain-stats subcommand.
pub fn dispatch(subcommand: &str) {
    match subcommand {
        "evolution" | "evo" => print_evolution(),
        "impact" => print_impact(),
        "learning-curve" | "curve" => print_learning_curve(),
        "accuracy" | "acc" => print_accuracy(),
        "baseline" | "rules" => print_baseline(),
        "false-approve" | "fa" => print_false_approve(),
        "false-deny" | "fd" => print_false_deny(),
        "distribution" | "dist" => print_distribution(),
        "novel-rate" | "novel" => print_novel_rate(),
        "calibration" | "cal" => print_calibration(),
        "incidents" | "postmortem" => print_incidents(),
        "time-to-correct" | "ttc" => print_time_to_correct(),
        "tier" | "tiers" | "risk" => print_tier_breakdown(),
        "latency" | "lat" => print_latency(),
        "cache" | "cache-hits" => print_cache_hits(),
        "counterfactual" | "cf" => print_counterfactuals(),
        "scorecard" | "card" | "summary" => print_scorecard(),
        "help" | "" => print_help(),
        _ => {
            eprintln!("Unknown brain-stats subcommand: '{subcommand}'");
            eprintln!();
            print_help();
        }
    }
}

fn print_help() {
    println!("Brain Statistics & Metrics");
    println!("==========================");
    println!();
    println!("Usage: claudectl --brain-stats <subcommand>");
    println!();
    println!("Subcommands:");
    println!("  evolution        Learning trajectory with sparkline charts");
    println!("  impact          Impact scorecard — headline metrics");
    println!("  learning-curve  Correction rate over time (is the brain learning?)");
    println!("  accuracy        Per-tool, per-risk, per-project accuracy breakdown");
    println!("  distribution    Decision volume by tool, risk, project, action");
    println!("  novel-rate      How quickly the frontier of novel situations shrinks");
    println!("  calibration     Are confidence scores well-calibrated?");
    println!("  baseline        Compare brain vs. rules-only classifier");
    println!("  false-approve   False-approve rate on risky actions (safety)");
    println!("  false-deny      False-deny rate and friction cost");
    println!("  incidents       Post-mortem analysis of every false approval");
    println!("  time-to-correct How quickly users respond to brain suggestions");
    println!("  tier            Per-risk-tier accuracy + safety breakdown");
    println!("  latency         Brain decision latency (p50/p95/p99 + histogram)");
    println!("  cache           Few-shot cache hit rate");
    println!("  counterfactual  Where user-overrides led to bad outcomes (or didn't)");
    println!("  scorecard       One-screen composite review — start here");
    println!("  help            Show this help");
    println!();
    println!("Aliases: evo, curve, acc, rules, fa, fd, dist, novel, cal, postmortem,");
    println!("         ttc, tiers, risk, lat, cf, card, summary");
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::accuracy::*;
    use super::*;
    use crate::brain::decisions::{DecisionRecord, DecisionType};

    // ── Rolling window tests ─────────────────────────────────────────

    #[test]
    fn rolling_window_empty() {
        assert!(rolling_correction_rate(&[], 10).is_empty());
    }

    #[test]
    fn rolling_window_too_small() {
        let decisions: Vec<DecisionRecord> = (0..5).map(|_| make_decision("accept")).collect();
        assert!(rolling_correction_rate(&decisions, 10).is_empty());
    }

    #[test]
    fn rolling_window_all_correct() {
        let decisions: Vec<DecisionRecord> = (0..20).map(|_| make_decision("accept")).collect();
        let points = rolling_correction_rate(&decisions, 10);
        assert!(!points.is_empty());
        for p in &points {
            assert!((p.correction_rate - 0.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn rolling_window_all_rejected() {
        let decisions: Vec<DecisionRecord> = (0..20).map(|_| make_decision("reject")).collect();
        let points = rolling_correction_rate(&decisions, 10);
        for p in &points {
            assert!((p.correction_rate - 1.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn rolling_window_decreasing() {
        // First 10 are all rejected, next 10 are all accepted
        let mut decisions: Vec<DecisionRecord> = (0..10).map(|_| make_decision("reject")).collect();
        decisions.extend((0..10).map(|_| make_decision("accept")));

        let points = rolling_correction_rate(&decisions, 10);
        let first = points.first().unwrap().correction_rate;
        let last = points.last().unwrap().correction_rate;
        assert!(
            first > last,
            "Expected decreasing curve: first={first}, last={last}"
        );
    }

    // ── Helpers ──────────────────────────────────────────────────────

    fn make_decision(user_action: &str) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: "test".into(),
            tool: Some("Bash".into()),
            command: Some("cargo test".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: user_action.into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: None,
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
            decision_source: None,
            rule_name: None,
            few_shot_ids: Vec::new(),
        }
    }

    // ── Dispatch tests ───────────────────────────────────────────────

    #[test]
    fn dispatch_help_no_panic() {
        // Just ensure it doesn't panic
        print_help();
    }
}

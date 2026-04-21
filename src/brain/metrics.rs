#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use super::decisions::{DecisionRecord, read_all_decisions};

// ────────────────────────────────────────────────────────────────────────────
// Re-exports from sub-modules so that existing `brain::metrics::*` paths
// continue to resolve without changes to callers.
// ────────────────────────────────────────────────────────────────────────────

#[allow(unused_imports)]
pub use super::risk::{RiskTier, classify_risk};

#[allow(unused_imports)]
pub use super::baseline::{print_baseline, rules_baseline_classify};

// ────────────────────────────────────────────────────────────────────────────
// Rolling window computation
// ────────────────────────────────────────────────────────────────────────────

/// A point on the learning curve: decision index and rolling correction rate.
#[derive(Debug, Clone)]
pub struct CurvePoint {
    pub index: usize,
    pub correction_rate: f64,
    pub window_size: usize,
}

/// Compute rolling correction rate over decision history.
/// Returns one point per decision after the window fills.
fn rolling_correction_rate(decisions: &[DecisionRecord], window: usize) -> Vec<CurvePoint> {
    if decisions.len() < window {
        return Vec::new();
    }

    let mut points = Vec::new();
    for i in window..=decisions.len() {
        let window_slice = &decisions[i - window..i];
        let corrections = window_slice.iter().filter(|d| d.is_negative()).count();
        let rate = corrections as f64 / window as f64;
        points.push(CurvePoint {
            index: i,
            correction_rate: rate,
            window_size: window,
        });
    }
    points
}

// ────────────────────────────────────────────────────────────────────────────
// #129: Correction rate learning curve
// ────────────────────────────────────────────────────────────────────────────

/// Print the correction rate learning curve to stdout.
pub fn print_learning_curve() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("Brain Learning Curve");
    println!("====================");
    println!();

    if total < 10 {
        println!("  Not enough decisions yet ({total}). Need at least 10.");
        println!("  Use claudectl with --brain and accept/reject suggestions to build history.");
        return;
    }

    // Choose window size based on total decisions
    let window = if total < 50 { 10 } else { 50.min(total / 5) };

    let points = rolling_correction_rate(&decisions, window);
    if points.is_empty() {
        println!("  Not enough decisions for window size {window}.");
        return;
    }

    println!("  Total decisions: {total}");
    println!("  Window size: {window}");
    println!();

    // Print ASCII sparkline chart
    println!("  Correction rate over time (lower = brain is learning):");
    println!();

    // Sample ~20 points for the chart
    let step = (points.len() / 20).max(1);
    let sampled: Vec<&CurvePoint> = points.iter().step_by(step).collect();

    let max_rate = sampled
        .iter()
        .map(|p| p.correction_rate)
        .fold(0.0f64, f64::max)
        .max(0.01); // avoid division by zero

    for point in &sampled {
        let bar_len = ((point.correction_rate / max_rate) * 40.0) as usize;
        let bar: String = "#".repeat(bar_len);
        println!(
            "  {:>5} | {:<40} {:.0}%",
            point.index,
            bar,
            point.correction_rate * 100.0,
        );
    }

    println!();

    // Summary stats
    let first_rate = points.first().map(|p| p.correction_rate).unwrap_or(0.0);
    let last_rate = points.last().map(|p| p.correction_rate).unwrap_or(0.0);
    let delta = first_rate - last_rate;

    println!("  Early correction rate:  {:.1}%", first_rate * 100.0);
    println!("  Current correction rate: {:.1}%", last_rate * 100.0);

    if delta > 0.05 {
        println!(
            "  Improvement:            {:.1}pp (brain is learning)",
            delta * 100.0
        );
    } else if delta < -0.05 {
        println!(
            "  Regression:             {:.1}pp (accuracy declining)",
            delta.abs() * 100.0
        );
    } else {
        println!(
            "  Stable:                 {:.1}pp change",
            delta.abs() * 100.0
        );
    }

    // Detect phase transitions (significant rate changes)
    println!();
    println!("  Phase transitions:");
    let mut prev_rate = first_rate;
    for point in points.iter().skip(window) {
        let change = (point.correction_rate - prev_rate).abs();
        if change > 0.15 {
            let direction = if point.correction_rate < prev_rate {
                "improved"
            } else {
                "regressed"
            };
            println!(
                "    Decision ~{}: {direction} by {:.0}pp",
                point.index,
                change * 100.0,
            );
        }
        prev_rate = point.correction_rate;
    }
}

// ────────────────────────────────────────────────────────────────────────────
// #131: Category-specific accuracy breakdown
// ────────────────────────────────────────────────────────────────────────────

/// Per-category accuracy record.
#[derive(Debug, Clone)]
pub struct CategoryAccuracy {
    pub name: String,
    pub total: u32,
    pub correct: u32,
    pub rejected: u32,
}

impl CategoryAccuracy {
    fn accuracy_pct(&self) -> f64 {
        let decided = self.correct + self.rejected;
        if decided == 0 {
            return 0.0;
        }
        (self.correct as f64 / decided as f64) * 100.0
    }
}

/// Print category-specific accuracy breakdown.
pub fn print_accuracy() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("Brain Accuracy Breakdown");
    println!("========================");
    println!();

    if total < 5 {
        println!("  Not enough decisions yet ({total}). Need at least 5.");
        return;
    }

    let mut by_tool: HashMap<String, CategoryAccuracy> = HashMap::new();
    let mut by_risk: HashMap<String, CategoryAccuracy> = HashMap::new();
    let mut by_project: HashMap<String, CategoryAccuracy> = HashMap::new();

    for d in &decisions {
        let tool = d.tool.clone().unwrap_or_else(|| "unknown".into());
        let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());
        let project = d.project.clone();

        let keys_and_maps: Vec<(String, &mut HashMap<String, CategoryAccuracy>)> = vec![
            (tool, &mut by_tool),
            (risk.label().to_string(), &mut by_risk),
            (project, &mut by_project),
        ];
        for (key, map) in keys_and_maps {
            let entry = map.entry(key.clone()).or_insert_with(|| CategoryAccuracy {
                name: key,
                total: 0,
                correct: 0,
                rejected: 0,
            });
            entry.total += 1;
            if d.is_positive() {
                entry.correct += 1;
            } else if d.is_negative() {
                entry.rejected += 1;
            }
        }
    }

    // Print tool breakdown
    println!("  By tool:");
    print_accuracy_table(&mut by_tool.into_values().collect());

    // Print risk tier breakdown
    println!();
    println!("  By risk tier:");
    print_accuracy_table(&mut by_risk.into_values().collect());

    // Print project breakdown (top 10)
    println!();
    println!("  By project:");
    let mut project_list: Vec<CategoryAccuracy> = by_project.into_values().collect();
    project_list.sort_by_key(|p| std::cmp::Reverse(p.total));
    project_list.truncate(10);
    print_accuracy_table(&mut project_list);

    // Print temporal breakdown
    println!();
    println!("  By phase:");
    print_temporal_accuracy(&decisions);
}

fn print_accuracy_table(entries: &mut Vec<CategoryAccuracy>) {
    entries.sort_by_key(|e| std::cmp::Reverse(e.total));

    println!(
        "    {:<20} {:>6} {:>8} {:>8} {:>8}",
        "Category", "Total", "Correct", "Rejected", "Accuracy"
    );
    println!("    {}", "-".repeat(54));

    for entry in entries {
        let decided = entry.correct + entry.rejected;
        if decided == 0 {
            println!(
                "    {:<20} {:>6} {:>8} {:>8} {:>7}",
                entry.name, entry.total, "-", "-", "n/a"
            );
        } else {
            println!(
                "    {:<20} {:>6} {:>8} {:>8} {:>7.1}%",
                entry.name,
                entry.total,
                entry.correct,
                entry.rejected,
                entry.accuracy_pct(),
            );
        }
    }
}

fn print_temporal_accuracy(decisions: &[DecisionRecord]) {
    let total = decisions.len();
    let phases: Vec<(&str, usize, usize)> = if total >= 500 {
        vec![
            ("early (0-100)", 0, 100),
            ("mid (100-500)", 100, 500),
            ("late (500+)", 500, total),
        ]
    } else if total >= 100 {
        let mid = total / 2;
        vec![("early", 0, mid), ("late", mid, total)]
    } else {
        vec![("all", 0, total)]
    };

    println!(
        "    {:<20} {:>6} {:>8} {:>8} {:>8}",
        "Phase", "Total", "Correct", "Rejected", "Accuracy"
    );
    println!("    {}", "-".repeat(54));

    for (label, start, end) in phases {
        let slice = &decisions[start..end];
        let correct = slice.iter().filter(|d| d.is_positive()).count() as u32;
        let rejected = slice.iter().filter(|d| d.is_negative()).count() as u32;
        let decided = correct + rejected;
        let accuracy = if decided > 0 {
            (correct as f64 / decided as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "    {:<20} {:>6} {:>8} {:>8} {:>7.1}%",
            label,
            slice.len(),
            correct,
            rejected,
            accuracy,
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// #133: False-approve rate on risky actions
// ────────────────────────────────────────────────────────────────────────────

/// Print false-approve rate analysis for risky actions.
pub fn print_false_approve() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("False-Approve Rate (Risky Actions)");
    println!("===================================");
    println!();

    if total < 5 {
        println!("  Not enough decisions yet ({total}). Need at least 5.");
        return;
    }

    // Track false-approves by risk tier
    let mut tier_stats: HashMap<RiskTier, FalseApproveStats> = HashMap::new();
    let mut worst_cases: Vec<FalseApproveCase> = Vec::new();

    for d in &decisions {
        let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());
        let stats = tier_stats.entry(risk).or_default();

        let brain_approved = d.brain_action == "approve";
        let user_rejected = d.is_negative();

        if brain_approved {
            stats.brain_approved += 1;
            if user_rejected {
                // False approve: brain said yes, user said no
                stats.false_approved += 1;
                if matches!(risk, RiskTier::High | RiskTier::Critical) {
                    worst_cases.push(FalseApproveCase {
                        risk,
                        tool: d.tool.clone().unwrap_or_default(),
                        command: d.command.clone().unwrap_or_default(),
                        confidence: d.brain_confidence,
                    });
                }
            }
        }

        stats.total += 1;
    }

    // Summary table
    println!(
        "  {:<12} {:>10} {:>12} {:>12} {:>12}",
        "Risk tier", "Decisions", "Approved", "False-approve", "FA rate"
    );
    println!("  {}", "-".repeat(62));

    for risk in &[
        RiskTier::Low,
        RiskTier::Medium,
        RiskTier::High,
        RiskTier::Critical,
    ] {
        let stats = tier_stats.get(risk).copied().unwrap_or_default();
        let fa_rate = if stats.brain_approved > 0 {
            (stats.false_approved as f64 / stats.brain_approved as f64) * 100.0
        } else {
            0.0
        };
        let rate_str = if stats.brain_approved == 0 {
            "n/a".to_string()
        } else {
            format!("{fa_rate:.1}%")
        };
        println!(
            "  {:<12} {:>10} {:>12} {:>12} {:>12}",
            risk.label(),
            stats.total,
            stats.brain_approved,
            stats.false_approved,
            rate_str,
        );
    }

    // Overall
    let total_approved: u32 = tier_stats.values().map(|s| s.brain_approved).sum();
    let total_false: u32 = tier_stats.values().map(|s| s.false_approved).sum();
    let overall_rate = if total_approved > 0 {
        (total_false as f64 / total_approved as f64) * 100.0
    } else {
        0.0
    };

    println!("  {}", "-".repeat(62));
    println!(
        "  {:<12} {:>10} {:>12} {:>12} {:>12}",
        "OVERALL",
        total,
        total_approved,
        total_false,
        format!("{overall_rate:.1}%"),
    );

    // High-risk focus
    let high_critical_approved: u32 = [RiskTier::High, RiskTier::Critical]
        .iter()
        .filter_map(|r| tier_stats.get(r))
        .map(|s| s.brain_approved)
        .sum();
    let high_critical_false: u32 = [RiskTier::High, RiskTier::Critical]
        .iter()
        .filter_map(|r| tier_stats.get(r))
        .map(|s| s.false_approved)
        .sum();

    println!();
    if high_critical_approved > 0 {
        let hc_rate = (high_critical_false as f64 / high_critical_approved as f64) * 100.0;
        println!(
            "  High+Critical false-approve rate: {:.1}% ({high_critical_false}/{high_critical_approved})",
            hc_rate
        );
        if hc_rate > 5.0 {
            println!("  WARNING: exceeds 5% target for high-risk actions");
        } else if hc_rate <= 1.0 {
            println!("  GOOD: within 1% target for high-risk actions");
        }
    } else {
        println!("  No high/critical risk approvals recorded yet.");
    }

    // Worst cases
    if !worst_cases.is_empty() {
        println!();
        println!("  Worst cases (high/critical risk, brain approved, user rejected):");
        for (i, case) in worst_cases.iter().take(10).enumerate() {
            let cmd_preview = if case.command.len() > 60 {
                format!("{}...", &case.command[..60])
            } else {
                case.command.clone()
            };
            println!(
                "    {}. [{}] {} \"{}\" (confidence: {:.0}%)",
                i + 1,
                case.risk,
                case.tool,
                cmd_preview,
                case.confidence * 100.0,
            );
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FalseApproveStats {
    total: u32,
    brain_approved: u32,
    false_approved: u32,
}

#[derive(Debug, Clone)]
struct FalseApproveCase {
    risk: RiskTier,
    tool: String,
    command: String,
    confidence: f64,
}

// ────────────────────────────────────────────────────────────────────────────
// #128: Decision distribution analysis
// ────────────────────────────────────────────────────────────────────────────

/// Print decision distribution analysis.
pub fn print_distribution() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("Decision Distribution");
    println!("======================");
    println!();

    if total < 5 {
        println!("  Not enough decisions yet ({total}). Need at least 5.");
        return;
    }

    // By tool
    let mut by_tool: HashMap<String, u32> = HashMap::new();
    // By risk
    let mut by_risk: HashMap<String, u32> = HashMap::new();
    // By brain action
    let mut by_brain: HashMap<String, u32> = HashMap::new();
    // By user action
    let mut by_user: HashMap<String, u32> = HashMap::new();
    // By project
    let mut by_project: HashMap<String, u32> = HashMap::new();

    for d in &decisions {
        let tool = d.tool.clone().unwrap_or_else(|| "unknown".into());
        *by_tool.entry(tool).or_insert(0) += 1;

        let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());
        *by_risk.entry(risk.label().to_string()).or_insert(0) += 1;

        *by_brain.entry(d.brain_action.clone()).or_insert(0) += 1;
        *by_user.entry(d.user_action.clone()).or_insert(0) += 1;
        *by_project.entry(d.project.clone()).or_insert(0) += 1;
    }

    print_distribution_table("By tool", &by_tool, total);
    print_distribution_table("By risk tier", &by_risk, total);
    print_distribution_table("By brain action", &by_brain, total);
    print_distribution_table("By user action", &by_user, total);
    print_distribution_table("By project", &by_project, total);
}

fn print_distribution_table(label: &str, data: &HashMap<String, u32>, total: usize) {
    let mut entries: Vec<(&String, &u32)> = data.iter().collect();
    entries.sort_by_key(|(_, c)| std::cmp::Reverse(**c));

    println!("  {label}:");
    println!("    {:<25} {:>6} {:>7}", "Category", "Count", "Share");
    println!("    {}", "-".repeat(40));
    for (name, count) in entries.iter().take(15) {
        let pct = **count as f64 / total as f64 * 100.0;
        let bar_len = (pct / 100.0 * 20.0) as usize;
        println!(
            "    {:<25} {:>6} {:>6.1}% {}",
            name,
            count,
            pct,
            "\u{2588}".repeat(bar_len),
        );
    }
    println!();
}

// ────────────────────────────────────────────────────────────────────────────
// #130: Novel situation rate tracking
// ────────────────────────────────────────────────────────────────────────────

/// Print novel situation rate analysis.
pub fn print_novel_rate() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("Novel Situation Rate");
    println!("=====================");
    println!();

    if total < 10 {
        println!("  Not enough decisions yet ({total}). Need at least 10.");
        return;
    }

    // A decision is "novel" if no prior decision has the same (tool, command_keyword)
    let mut seen_patterns: HashSet<(String, String)> = HashSet::new();
    let mut batch_size = (total / 10).clamp(10, 50);
    if batch_size > total {
        batch_size = total;
    }

    let mut batch_novel = 0u32;
    let mut batch_total = 0u32;
    let mut points: Vec<(usize, f64)> = Vec::new();

    for (idx, d) in decisions.iter().enumerate() {
        let tool = d.tool.clone().unwrap_or_else(|| "*".into());
        let cmd = d
            .command
            .as_deref()
            .and_then(|c| {
                let tokens: Vec<&str> = c.split_whitespace().take(2).collect();
                if tokens.is_empty() {
                    None
                } else {
                    Some(tokens.join(" "))
                }
            })
            .unwrap_or_else(|| "*".into());

        let key = (tool, cmd);
        let is_novel = !seen_patterns.contains(&key);
        seen_patterns.insert(key);

        batch_total += 1;
        if is_novel {
            batch_novel += 1;
        }

        if batch_total >= batch_size as u32 || idx == total - 1 {
            let rate = batch_novel as f64 / batch_total as f64;
            points.push((idx + 1, rate));
            batch_novel = 0;
            batch_total = 0;
        }
    }

    // Print chart
    println!("  Novel rate per batch of ~{batch_size} decisions (lower = more patterns learned):");
    println!();

    for (idx, rate) in &points {
        let bar_len = (*rate * 40.0) as usize;
        println!(
            "  {:>5} | {:<40} {:.0}%",
            idx,
            "\u{2588}".repeat(bar_len),
            rate * 100.0,
        );
    }
    println!();

    let first_rate = points.first().map(|(_, r)| *r).unwrap_or(0.0);
    let last_rate = points.last().map(|(_, r)| *r).unwrap_or(0.0);
    let unique = seen_patterns.len();

    println!("  Unique patterns seen: {unique}");
    println!("  Early novel rate:    {:.1}%", first_rate * 100.0);
    println!("  Current novel rate:  {:.1}%", last_rate * 100.0);

    if first_rate > last_rate + 0.05 {
        println!(
            "  Brain is learning: novel rate dropped {:.1}pp",
            (first_rate - last_rate) * 100.0
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// #134: False-deny rate and friction cost
// ────────────────────────────────────────────────────────────────────────────

/// Print false-deny rate (brain denied, user overrode with approve).
pub fn print_false_deny() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("False-Deny Rate (Friction Cost)");
    println!("================================");
    println!();

    if total < 5 {
        println!("  Not enough decisions yet ({total}). Need at least 5.");
        return;
    }

    let mut by_tool: HashMap<String, (u32, u32)> = HashMap::new(); // (denials, overrides)
    let mut total_denials = 0u32;
    let mut total_overrides = 0u32;

    for d in &decisions {
        if d.brain_action == "deny" {
            let tool = d.tool.clone().unwrap_or_else(|| "unknown".into());
            let entry = by_tool.entry(tool).or_insert((0, 0));
            entry.0 += 1;
            total_denials += 1;

            if d.is_positive() {
                // User overrode the deny (approved anyway)
                entry.1 += 1;
                total_overrides += 1;
            }
        }
    }

    if total_denials == 0 {
        println!("  No brain denials recorded yet.");
        return;
    }

    println!(
        "  {:<20} {:>8} {:>10} {:>12}",
        "Tool", "Denials", "Overridden", "Override rate"
    );
    println!("  {}", "-".repeat(54));

    let mut entries: Vec<(String, u32, u32)> =
        by_tool.into_iter().map(|(t, (d, o))| (t, d, o)).collect();
    entries.sort_by_key(|(_, d, _)| std::cmp::Reverse(*d));

    for (tool, denials, overrides) in &entries {
        let rate = if *denials > 0 {
            *overrides as f64 / *denials as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "  {:<20} {:>8} {:>10} {:>11.1}%",
            tool, denials, overrides, rate,
        );
    }

    println!("  {}", "-".repeat(54));
    let overall_rate = total_overrides as f64 / total_denials as f64 * 100.0;
    println!(
        "  {:<20} {:>8} {:>10} {:>11.1}%",
        "TOTAL", total_denials, total_overrides, overall_rate,
    );

    println!();
    if overall_rate > 30.0 {
        println!(
            "  WARNING: override rate {overall_rate:.1}% exceeds 30% — brain may be too aggressive"
        );
        println!("  Consider lowering confidence thresholds for high-override tools.");
    } else if overall_rate < 5.0 {
        println!("  GOOD: low override rate — brain denials are well-calibrated.");
    }
}

// ────────────────────────────────────────────────────────────────────────────
// #135: Confidence calibration
// ────────────────────────────────────────────────────────────────────────────

/// Print confidence calibration analysis.
pub fn print_calibration() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("Confidence Calibration");
    println!("=======================");
    println!();

    if total < 10 {
        println!("  Not enough decisions yet ({total}). Need at least 10.");
        return;
    }

    // Bin decisions by confidence level
    let bins: &[(f64, f64, &str)] = &[
        (0.0, 0.3, "0.0-0.3"),
        (0.3, 0.5, "0.3-0.5"),
        (0.5, 0.7, "0.5-0.7"),
        (0.7, 0.9, "0.7-0.9"),
        (0.9, 1.01, "0.9-1.0"),
    ];

    println!(
        "  {:<10} {:>8} {:>10} {:>12} {:>8}",
        "Confidence", "Count", "Correct", "Accuracy", "Delta"
    );
    println!("  {}", "-".repeat(52));

    let mut ece_sum = 0.0f64; // Expected Calibration Error
    let mut ece_total = 0u32;

    for &(lo, hi, label) in bins {
        let in_bin: Vec<&DecisionRecord> = decisions
            .iter()
            .filter(|d| d.brain_confidence >= lo && d.brain_confidence < hi)
            .filter(|d| d.is_positive() || d.is_negative())
            .collect();

        let count = in_bin.len() as u32;
        if count == 0 {
            println!(
                "  {:<10} {:>8} {:>10} {:>12} {:>8}",
                label, 0, "-", "-", "-"
            );
            continue;
        }

        let correct = in_bin.iter().filter(|d| d.is_positive()).count() as u32;
        let accuracy = correct as f64 / count as f64;
        let mid_confidence = (lo + hi) / 2.0;
        let delta = accuracy - mid_confidence;

        // ECE contribution
        ece_sum += (accuracy - mid_confidence).abs() * count as f64;
        ece_total += count;

        let delta_str = if delta.abs() < 0.05 {
            format!("{delta:+.1}pp")
        } else if delta > 0.0 {
            format!("{:+.1}pp \u{2191}", delta * 100.0) // underconfident
        } else {
            format!("{:+.1}pp \u{2193}", delta * 100.0) // overconfident
        };

        println!(
            "  {:<10} {:>8} {:>10} {:>11.1}% {:>8}",
            label,
            count,
            correct,
            accuracy * 100.0,
            delta_str,
        );
    }

    println!();

    if ece_total > 0 {
        let ece = ece_sum / ece_total as f64;
        println!("  Expected Calibration Error (ECE): {:.3}", ece);
        if ece < 0.05 {
            println!("  GOOD: well-calibrated (ECE < 0.05)");
        } else if ece < 0.15 {
            println!("  MODERATE: some miscalibration (ECE 0.05-0.15)");
        } else {
            println!(
                "  WARNING: poorly calibrated (ECE > 0.15) — confidence scores need adjustment"
            );
        }
    }

    // Per-tool calibration summary
    println!();
    println!("  Per-tool calibration:");
    let mut tool_bins: HashMap<String, (u32, u32, f64)> = HashMap::new(); // (total, correct, avg_confidence)
    for d in &decisions {
        if d.is_positive() || d.is_negative() {
            let tool = d.tool.clone().unwrap_or_else(|| "unknown".into());
            let entry = tool_bins.entry(tool).or_insert((0, 0, 0.0));
            entry.0 += 1;
            if d.is_positive() {
                entry.1 += 1;
            }
            entry.2 += d.brain_confidence;
        }
    }

    let mut tool_list: Vec<(String, u32, u32, f64)> = tool_bins
        .into_iter()
        .map(|(t, (total, correct, sum_conf))| (t, total, correct, sum_conf / total as f64))
        .collect();
    tool_list.sort_by_key(|(_, total, _, _)| std::cmp::Reverse(*total));

    println!(
        "    {:<15} {:>8} {:>10} {:>12} {:>12}",
        "Tool", "Count", "Accuracy", "Avg Conf", "Gap"
    );
    println!("    {}", "-".repeat(60));

    for (tool, total, correct, avg_conf) in tool_list.iter().take(10) {
        let accuracy = *correct as f64 / *total as f64;
        let gap = accuracy - avg_conf;
        let gap_str = if gap.abs() < 0.05 {
            "aligned".to_string()
        } else if gap > 0.0 {
            format!("{:+.0}pp under", gap * 100.0)
        } else {
            format!("{:+.0}pp over", gap * 100.0)
        };
        println!(
            "    {:<15} {:>8} {:>11.1}% {:>11.2} {:>12}",
            tool,
            total,
            accuracy * 100.0,
            avg_conf,
            gap_str,
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// #140: Incident post-mortem framework for false approvals
// ────────────────────────────────────────────────────────────────────────────

/// Classify the root cause of a false approval.
fn classify_incident_cause(
    decision: &DecisionRecord,
    prior_decisions: &[DecisionRecord],
) -> &'static str {
    let tool = decision.tool.as_deref().unwrap_or("");
    let cmd = decision.command.as_deref().unwrap_or("");

    // Check if this pattern was ever seen before
    let seen_before = prior_decisions.iter().any(|d| {
        d.tool.as_deref() == Some(tool)
            && d.command
                .as_deref()
                .map(|c| c.split_whitespace().take(2).collect::<Vec<_>>())
                == Some(cmd.split_whitespace().take(2).collect::<Vec<_>>())
    });

    if !seen_before {
        return "novel_pattern";
    }

    // Check if confidence was high (>0.8) — miscalibration
    if decision.brain_confidence > 0.8 {
        return "confidence_miscalibration";
    }

    // Check if a similar-looking safe command exists — overgeneralization
    let similar_safe = prior_decisions
        .iter()
        .any(|d| d.tool.as_deref() == Some(tool) && d.is_positive() && d.brain_confidence > 0.7);

    if similar_safe {
        return "overgeneralization";
    }

    "context_blindness"
}

/// Print incident analysis for all false approvals.
pub fn print_incidents() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("Incident Post-Mortems (False Approvals)");
    println!("========================================");
    println!();

    if total < 5 {
        println!("  Not enough decisions yet ({total}). Need at least 5.");
        return;
    }

    // Find all false approvals: brain approved, user rejected
    let mut incidents: Vec<(usize, &DecisionRecord, &'static str)> = Vec::new();
    for (idx, d) in decisions.iter().enumerate() {
        if d.brain_action == "approve" && d.is_negative() {
            let cause = classify_incident_cause(d, &decisions[..idx]);
            incidents.push((idx, d, cause));
        }
    }

    if incidents.is_empty() {
        println!(
            "  No false approvals found. The brain hasn't approved anything the user rejected."
        );
        return;
    }

    println!("  {} incident(s) found", incidents.len());
    println!();

    // Root cause distribution
    let mut causes: HashMap<&str, u32> = HashMap::new();
    for (_, _, cause) in &incidents {
        *causes.entry(cause).or_insert(0) += 1;
    }

    println!("  Root cause distribution:");
    let cause_labels: &[(&str, &str)] = &[
        ("novel_pattern", "Novel pattern (never seen before)"),
        (
            "confidence_miscalibration",
            "Confidence miscalibration (high confidence, wrong answer)",
        ),
        (
            "overgeneralization",
            "Overgeneralization (similar safe case fooled it)",
        ),
        (
            "context_blindness",
            "Context blindness (missed relevant state)",
        ),
    ];

    for (key, label) in cause_labels {
        let count = causes.get(key).copied().unwrap_or(0);
        if count > 0 {
            println!("    {count:>3}  {label}");
        }
    }
    println!();

    // By risk tier
    let mut risk_counts: HashMap<RiskTier, u32> = HashMap::new();
    for (_, d, _) in &incidents {
        let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());
        *risk_counts.entry(risk).or_insert(0) += 1;
    }

    println!("  By risk tier:");
    for risk in &[
        RiskTier::Critical,
        RiskTier::High,
        RiskTier::Medium,
        RiskTier::Low,
    ] {
        let count = risk_counts.get(risk).copied().unwrap_or(0);
        if count > 0 {
            println!("    {count:>3}  {}", risk.label());
        }
    }
    println!();

    // Detail: show worst incidents (high/critical risk first)
    let mut sorted_incidents = incidents.clone();
    sorted_incidents.sort_by_key(|(_, d, _)| {
        let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());
        match risk {
            RiskTier::Critical => 0,
            RiskTier::High => 1,
            RiskTier::Medium => 2,
            RiskTier::Low => 3,
        }
    });

    println!("  Incidents (worst first):");
    println!();
    for (i, (idx, d, cause)) in sorted_incidents.iter().take(10).enumerate() {
        let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());
        let cmd_preview = d
            .command
            .as_deref()
            .map(|c| {
                if c.len() > 60 {
                    format!("{}...", &c[..60])
                } else {
                    c.to_string()
                }
            })
            .unwrap_or_default();
        let tool = d.tool.as_deref().unwrap_or("?");

        println!(
            "    {}. [{}] {}(\"{}\")",
            i + 1,
            risk.label(),
            tool,
            cmd_preview
        );
        println!(
            "       Confidence: {:.0}% | Cause: {} | Decision #{}",
            d.brain_confidence * 100.0,
            cause,
            idx,
        );
        if !d.brain_reasoning.is_empty() {
            let reason = if d.brain_reasoning.len() > 80 {
                format!("{}...", &d.brain_reasoning[..80])
            } else {
                d.brain_reasoning.clone()
            };
            println!("       Reasoning: \"{reason}\"");
        }

        // Check if correction was learned
        let corrected = decisions.iter().skip(idx + 1).any(|later| {
            later.tool.as_deref() == d.tool.as_deref()
                && later.brain_action == "deny"
                && later
                    .command
                    .as_deref()
                    .map(|c| c.split_whitespace().take(2).collect::<Vec<_>>())
                    == d.command
                        .as_deref()
                        .map(|c| c.split_whitespace().take(2).collect::<Vec<_>>())
        });

        if corrected {
            println!("       Correction learned: yes (brain now denies this pattern)");
        }
        println!();
    }
}

// ────────────────────────────────────────────────────────────────────────────
// #132: Time-to-correct analysis
// ────────────────────────────────────────────────────────────────────────────

/// Print time-to-correct analysis — how quickly users respond to brain suggestions.
pub fn print_time_to_correct() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("Time-to-Correct Analysis");
    println!("=========================");
    println!();

    if total < 5 {
        println!("  Not enough decisions yet ({total}). Need at least 5.");
        return;
    }

    // Find decisions with both suggested_at and ts (parsed as epoch secs)
    let mut reaction_times: Vec<(usize, f64, bool)> = Vec::new(); // (index, seconds, is_correction)

    for (idx, d) in decisions.iter().enumerate() {
        let Some(suggested_at) = d.suggested_at else {
            continue;
        };
        if suggested_at == 0 {
            continue;
        }

        // Parse the ts field (could be epoch seconds as string or number)
        let responded_at: u64 = d.timestamp.trim_matches('"').parse::<u64>().unwrap_or(0);
        if responded_at == 0 || responded_at < suggested_at {
            continue;
        }

        let reaction_secs = (responded_at - suggested_at) as f64;
        // Cap at 5 minutes — anything longer is likely the user was away
        if reaction_secs > 300.0 {
            continue;
        }

        let is_correction = d.is_negative();
        reaction_times.push((idx, reaction_secs, is_correction));
    }

    if reaction_times.is_empty() {
        println!("  No reaction time data available yet.");
        println!("  This requires brain suggestions with the suggested_at timestamp");
        println!("  (available in decisions logged after v0.31.1).");
        return;
    }

    // Categorize: fast (<2s), moderate (2-5s), deliberate (>5s)
    let fast = reaction_times.iter().filter(|(_, t, _)| *t < 2.0).count();
    let moderate = reaction_times
        .iter()
        .filter(|(_, t, _)| *t >= 2.0 && *t < 5.0)
        .count();
    let deliberate = reaction_times.iter().filter(|(_, t, _)| *t >= 5.0).count();
    let total_reactions = reaction_times.len();

    let avg_time: f64 =
        reaction_times.iter().map(|(_, t, _)| t).sum::<f64>() / total_reactions as f64;

    println!("  {} decisions with reaction time data", total_reactions);
    println!("  Average reaction time: {:.1}s", avg_time);
    println!();

    // Distribution
    println!("  Reaction speed:");
    println!(
        "    Fast (<2s):      {:>4} ({:.0}%)  — gut reaction",
        fast,
        fast as f64 / total_reactions as f64 * 100.0,
    );
    println!(
        "    Moderate (2-5s): {:>4} ({:.0}%)  — quick review",
        moderate,
        moderate as f64 / total_reactions as f64 * 100.0,
    );
    println!(
        "    Deliberate (>5s):{:>4} ({:.0}%)  — careful consideration",
        deliberate,
        deliberate as f64 / total_reactions as f64 * 100.0,
    );
    println!();

    // Corrections vs accepts
    let corrections: Vec<&(usize, f64, bool)> =
        reaction_times.iter().filter(|(_, _, c)| *c).collect();
    let accepts: Vec<&(usize, f64, bool)> = reaction_times.iter().filter(|(_, _, c)| !*c).collect();

    if !corrections.is_empty() {
        let avg_correction =
            corrections.iter().map(|(_, t, _)| t).sum::<f64>() / corrections.len() as f64;
        let avg_accept = if accepts.is_empty() {
            0.0
        } else {
            accepts.iter().map(|(_, t, _)| t).sum::<f64>() / accepts.len() as f64
        };

        println!("  Corrections vs accepts:");
        println!(
            "    Avg correction time: {:.1}s ({} corrections)",
            avg_correction,
            corrections.len()
        );
        println!(
            "    Avg accept time:     {:.1}s ({} accepts)",
            avg_accept,
            accepts.len()
        );

        if avg_correction > avg_accept + 1.0 {
            println!(
                "    Corrections take longer — user deliberates before overriding (good signal)"
            );
        } else if avg_accept > avg_correction + 1.0 {
            println!("    Accepts take longer than corrections — possible rubber-stamping risk");
        }
    }

    // Trend: compare first vs last half reaction times
    if total_reactions >= 10 {
        let mid = total_reactions / 2;
        let early_avg: f64 =
            reaction_times[..mid].iter().map(|(_, t, _)| t).sum::<f64>() / mid as f64;
        let late_avg: f64 = reaction_times[mid..].iter().map(|(_, t, _)| t).sum::<f64>()
            / (total_reactions - mid) as f64;

        println!();
        println!("  Trend:");
        println!("    Early avg: {:.1}s", early_avg);
        println!("    Recent avg: {:.1}s", late_avg);

        let delta = late_avg - early_avg;
        if delta.abs() > 0.5 {
            if delta > 0.0 {
                println!(
                    "    Slowing down ({:+.1}s) — may indicate decision fatigue or more nuanced calls",
                    delta
                );
            } else {
                println!(
                    "    Speeding up ({:+.1}s) — user developing sharper judgment",
                    delta
                );
            }
        } else {
            println!("    Stable (within 0.5s)");
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// #170: Impact scorecard
// ────────────────────────────────────────────────────────────────────────────

/// Render a horizontal bar using Unicode block characters.
/// `value` is 0.0–1.0, `width` is the total bar width in characters.
fn render_bar(value: f64, width: usize) -> String {
    let filled = (value.clamp(0.0, 1.0) * width as f64) as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

/// Format a time duration in human-friendly units.
fn format_time_saved(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{:.0}s", secs)
    }
}

/// Print the impact scorecard — visual cards with headline metrics.
pub fn print_impact() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    if total < 5 {
        println!("Not enough decisions yet ({total}). Need at least 5.");
        println!("Use claudectl with --brain to build history.");
        return;
    }

    // ── Compute all metrics ─────────────────────────────────────────
    let auto_count = decisions
        .iter()
        .filter(|d| d.user_action == "auto" || d.user_action == "rule_approve")
        .count();
    let auto_rate = auto_count as f64 / total as f64;

    let mut rules_decided = 0u32;
    let mut brain_correct = 0u32;
    let mut brain_decided = 0u32;
    for d in &decisions {
        if rules_baseline_classify(d.tool.as_deref(), d.command.as_deref()) != "abstain" {
            rules_decided += 1;
        }
        if d.is_positive() || d.is_negative() {
            brain_decided += 1;
            if d.is_positive() {
                brain_correct += 1;
            }
        }
    }
    let brain_accuracy = if brain_decided > 0 {
        brain_correct as f64 / brain_decided as f64
    } else {
        0.0
    };
    let coverage_multiplier = if rules_decided > 0 {
        brain_decided as f64 / rules_decided as f64
    } else {
        0.0
    };

    let mut blocked_high = 0u32;
    let mut blocked_critical = 0u32;
    for d in &decisions {
        let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());
        let was_denied = d.brain_action == "deny"
            || d.user_action == "reject"
            || d.user_action == "rule_deny"
            || d.user_action == "deny_rule_override"
            || d.user_action == "conflict_deny";
        if was_denied {
            match risk {
                RiskTier::High => blocked_high += 1,
                RiskTier::Critical => blocked_critical += 1,
                _ => {}
            }
        }
    }
    let total_blocked = blocked_high + blocked_critical;

    const SECS_PER_INTERRUPTION: f64 = 3.0;
    let time_saved_secs = auto_count as f64 * SECS_PER_INTERRUPTION;

    // ── Render cards ────────────────────────────────────────────────
    let w = 48; // card width
    let dbar = "\u{2550}".repeat(w);

    println!();
    println!("  \u{2554}{dbar}\u{2557}");
    println!("  \u{2551}{:^w$}\u{2551}", "IMPACT SCORECARD", w = w);
    println!(
        "  \u{2551}{:^w$}\u{2551}",
        format!("{total} decisions tracked"),
        w = w
    );
    println!("  \u{2560}{dbar}\u{2563}");

    // Card 1: Auto-approve
    println!(
        "  \u{2551}  {:<30} {:>13}  \u{2551}",
        "Auto-handled",
        format!("{:.0}%", auto_rate * 100.0),
    );
    println!(
        "  \u{2551}  {}  {:>5}/{:<5}  \u{2551}",
        render_bar(auto_rate, 28),
        auto_count,
        total,
    );
    println!("  \u{2551}{}\u{2551}", " ".repeat(w));

    // Card 2: Brain accuracy
    println!(
        "  \u{2551}  {:<30} {:>13}  \u{2551}",
        "Brain accuracy",
        format!("{:.1}%", brain_accuracy * 100.0),
    );
    println!(
        "  \u{2551}  {}  {:>5}/{:<5}  \u{2551}",
        render_bar(brain_accuracy, 28),
        brain_correct,
        brain_decided,
    );
    println!("  \u{2551}{}\u{2551}", " ".repeat(w));

    // Card 3: Coverage vs rules
    if coverage_multiplier > 1.0 {
        println!(
            "  \u{2551}  {:<30} {:>13}  \u{2551}",
            "Coverage vs static rules",
            format!("{:.1}x", coverage_multiplier),
        );
    } else {
        println!(
            "  \u{2551}  {:<30} {:>13}  \u{2551}",
            "Coverage vs static rules", "n/a",
        );
    }
    let rules_pct = if total > 0 {
        rules_decided as f64 / total as f64
    } else {
        0.0
    };
    let brain_pct = if total > 0 {
        brain_decided as f64 / total as f64
    } else {
        0.0
    };
    println!(
        "  \u{2551}  brain {}  {:.0}%  \u{2551}",
        render_bar(brain_pct, 28),
        brain_pct * 100.0,
    );
    println!(
        "  \u{2551}  rules {}  {:.0}%  \u{2551}",
        render_bar(rules_pct, 28),
        rules_pct * 100.0,
    );
    println!("  \u{2551}{}\u{2551}", " ".repeat(w));

    // Card 4: Safety + Time saved (compact row)
    println!(
        "  \u{2551}  {:<22} {:>6}  {:<8} {:>4}  \u{2551}",
        "Dangerous ops blocked",
        total_blocked,
        "Time saved",
        format_time_saved(time_saved_secs),
    );
    if total_blocked > 0 || auto_count > 0 {
        let mut detail_parts = Vec::new();
        if blocked_critical > 0 {
            detail_parts.push(format!("{blocked_critical} critical"));
        }
        if blocked_high > 0 {
            detail_parts.push(format!("{blocked_high} high-risk"));
        }
        if auto_count > 0 {
            detail_parts.push(format!("{auto_count} auto x 3s"));
        }
        let detail = detail_parts.join(" | ");
        println!("  \u{2551}  {:<w2$}  \u{2551}", detail, w2 = w - 4);
    }

    // Learning curve (if enough data)
    if total >= 10 {
        let mid = total / 2;
        let early_corrections = decisions[..mid].iter().filter(|d| d.is_negative()).count();
        let late_corrections = decisions[mid..].iter().filter(|d| d.is_negative()).count();
        let early_rate = early_corrections as f64 / mid as f64;
        let late_rate = late_corrections as f64 / (total - mid) as f64;
        let improvement = early_rate - late_rate;

        if improvement.abs() > 0.05 {
            println!("  \u{2551}{}\u{2551}", " ".repeat(w));
            let arrow = if improvement > 0.0 {
                "\u{2193}"
            } else {
                "\u{2191}"
            };
            println!(
                "  \u{2551}  Learning: correction rate {:.1}% {arrow} {:.1}% ({:+.1}pp)  \u{2551}",
                early_rate * 100.0,
                late_rate * 100.0,
                -improvement * 100.0,
            );
        }
    }

    println!("  \u{255a}{dbar}\u{255d}");
    println!();
}

// ────────────────────────────────────────────────────────────────────────────
// Brain evolution visualization
// ────────────────────────────────────────────────────────────────────────────

/// Render a sparkline from a slice of 0.0–1.0 values using Unicode braille-style blocks.
fn sparkline(values: &[f64], width: usize) -> String {
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

/// Compute a metric in batches, returning one value per batch.
fn batch_metric(
    decisions: &[DecisionRecord],
    batch_size: usize,
    mut metric_fn: impl FnMut(&[DecisionRecord]) -> f64,
) -> Vec<f64> {
    if decisions.is_empty() || batch_size == 0 {
        return Vec::new();
    }
    decisions.chunks(batch_size).map(&mut metric_fn).collect()
}

/// Print the brain evolution dashboard — visual learning trajectory.
pub fn print_evolution() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    if total < 10 {
        println!("Not enough decisions yet ({total}). Need at least 10.");
        println!("Use claudectl with --brain to build history.");
        return;
    }

    let w = 52;
    let spark_w = 30;
    let dbar = "\u{2550}".repeat(w);

    // Compute batch size for sparklines (aim for ~15-30 data points)
    let batch = (total / 20).clamp(3, 50);

    // ── Compute sparkline data ──────────────────────────────────────

    // 1. Accuracy over time (rolling)
    let accuracy_data = batch_metric(&decisions, batch, |chunk| {
        let decided = chunk
            .iter()
            .filter(|d| d.is_positive() || d.is_negative())
            .count();
        let correct = chunk.iter().filter(|d| d.is_positive()).count();
        if decided == 0 {
            0.5
        } else {
            correct as f64 / decided as f64
        }
    });

    // 2. Correction rate over time (lower = better, so invert for sparkline)
    let correction_data = batch_metric(&decisions, batch, |chunk| {
        let total_chunk = chunk.len() as f64;
        let corrections = chunk.iter().filter(|d| d.is_negative()).count() as f64;
        if total_chunk == 0.0 {
            0.0
        } else {
            corrections / total_chunk
        }
    });
    let correction_inverted: Vec<f64> = correction_data.iter().map(|r| 1.0 - r).collect();

    // 3. Novel rate over time (lower = more learned)
    let mut seen_patterns: HashSet<(String, String)> = HashSet::new();
    let novel_data = batch_metric(&decisions, batch, |chunk| {
        let mut novel = 0;
        for d in chunk {
            let tool = d.tool.clone().unwrap_or_else(|| "*".into());
            let cmd = d
                .command
                .as_deref()
                .and_then(|c| {
                    let t: Vec<&str> = c.split_whitespace().take(2).collect();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.join(" "))
                    }
                })
                .unwrap_or_else(|| "*".into());
            if seen_patterns.insert((tool, cmd)) {
                novel += 1;
            }
        }
        novel as f64 / chunk.len().max(1) as f64
    });

    // 4. Auto-handle rate over time
    let auto_data = batch_metric(&decisions, batch, |chunk| {
        let auto = chunk
            .iter()
            .filter(|d| d.user_action == "auto" || d.user_action == "rule_approve")
            .count();
        auto as f64 / chunk.len().max(1) as f64
    });

    // ── Summary stats ───────────────────────────────────────────────

    let overall_accuracy = {
        let decided = decisions
            .iter()
            .filter(|d| d.is_positive() || d.is_negative())
            .count();
        let correct = decisions.iter().filter(|d| d.is_positive()).count();
        if decided == 0 {
            0.0
        } else {
            correct as f64 / decided as f64
        }
    };

    let early_correction = correction_data.first().copied().unwrap_or(0.0);
    let late_correction = correction_data.last().copied().unwrap_or(0.0);
    let correction_delta = early_correction - late_correction;

    let early_novel = novel_data.first().copied().unwrap_or(0.0);
    let late_novel = novel_data.last().copied().unwrap_or(0.0);

    let unique_patterns = seen_patterns.len();

    let auto_count = decisions
        .iter()
        .filter(|d| d.user_action == "auto" || d.user_action == "rule_approve")
        .count();
    let auto_rate = auto_count as f64 / total as f64;

    // Phase detection
    let phase = if total < 50 {
        "Early Learning"
    } else if correction_delta > 0.1 {
        "Actively Improving"
    } else if late_correction < 0.05 {
        "Stable & Accurate"
    } else {
        "Steady State"
    };

    // ── Render ───────────────────────────────────────────────────────

    println!();
    println!("  \u{2554}{dbar}\u{2557}");
    println!("  \u{2551}{:^w$}\u{2551}", "BRAIN EVOLUTION", w = w);
    println!(
        "  \u{2551}{:^w$}\u{2551}",
        format!("{total} decisions \u{2502} {unique_patterns} patterns \u{2502} {phase}"),
        w = w
    );
    println!("  \u{2560}{dbar}\u{2563}");

    // Accuracy sparkline
    let acc_first = accuracy_data.first().copied().unwrap_or(0.0) * 100.0;
    let acc_last = accuracy_data.last().copied().unwrap_or(0.0) * 100.0;
    println!("  \u{2551}                                                    \u{2551}");
    println!(
        "  \u{2551}  Accuracy          {:.0}% \u{2192} {:.0}%               {:>5.1}%  \u{2551}",
        acc_first,
        acc_last,
        overall_accuracy * 100.0,
    );
    println!(
        "  \u{2551}  {spark}                    \u{2551}",
        spark = sparkline(&accuracy_data, spark_w),
    );

    // Correction rate sparkline (inverted: higher = better = fewer corrections)
    println!("  \u{2551}                                                    \u{2551}");
    let corr_label = if correction_delta > 0.05 {
        format!("\u{2193}{:.0}pp", correction_delta * 100.0)
    } else if correction_delta < -0.05 {
        format!("\u{2191}{:.0}pp", correction_delta.abs() * 100.0)
    } else {
        "stable".to_string()
    };
    println!(
        "  \u{2551}  Corrections       {:.0}% \u{2192} {:.0}%              {:>6}  \u{2551}",
        early_correction * 100.0,
        late_correction * 100.0,
        corr_label,
    );
    println!(
        "  \u{2551}  {spark}                    \u{2551}",
        spark = sparkline(&correction_inverted, spark_w),
    );

    // Novel rate sparkline
    println!("  \u{2551}                                                    \u{2551}");
    println!(
        "  \u{2551}  Novel situations  {:.0}% \u{2192} {:.0}%        {unique_patterns:>4} learned  \u{2551}",
        early_novel * 100.0,
        late_novel * 100.0,
    );
    let novel_inverted: Vec<f64> = novel_data.iter().map(|r| 1.0 - r).collect();
    println!(
        "  \u{2551}  {spark}                    \u{2551}",
        spark = sparkline(&novel_inverted, spark_w),
    );

    // Auto-handle rate sparkline
    if auto_count > 0 {
        let auto_first = auto_data.first().copied().unwrap_or(0.0) * 100.0;
        let auto_last = auto_data.last().copied().unwrap_or(0.0) * 100.0;
        println!("  \u{2551}                                                    \u{2551}");
        println!(
            "  \u{2551}  Auto-handled      {:.0}% \u{2192} {:.0}%               {:>5.0}%  \u{2551}",
            auto_first,
            auto_last,
            auto_rate * 100.0,
        );
        println!(
            "  \u{2551}  {spark}                    \u{2551}",
            spark = sparkline(&auto_data, spark_w),
        );
    }

    // Safety summary row
    let blocked: usize = decisions
        .iter()
        .filter(|d| {
            let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());
            let denied = d.brain_action == "deny"
                || d.user_action == "reject"
                || d.user_action == "rule_deny"
                || d.user_action == "deny_rule_override";
            denied && matches!(risk, RiskTier::High | RiskTier::Critical)
        })
        .count();

    let time_saved = format_time_saved(auto_count as f64 * 3.0);

    println!("  \u{2551}                                                    \u{2551}");
    println!("  \u{2560}{dbar}\u{2563}");
    println!(
        "  \u{2551}  Dangerous blocked {:>4}  \u{2502}  Time saved {:>8}       \u{2551}",
        blocked, time_saved,
    );

    // Milestone markers
    let milestones = compute_milestones(&decisions, total, unique_patterns, correction_delta);
    if !milestones.is_empty() {
        println!("  \u{2560}{dbar}\u{2563}");
        for m in &milestones {
            println!("  \u{2551}  {:<w2$}  \u{2551}", m, w2 = w - 4);
        }
    }

    println!("  \u{255a}{dbar}\u{255d}");
    println!();
}

fn compute_milestones(
    decisions: &[DecisionRecord],
    total: usize,
    unique_patterns: usize,
    correction_delta: f64,
) -> Vec<String> {
    let mut milestones = Vec::new();

    if total >= 100 {
        milestones
            .push("\u{2713} 100+ decisions \u{2014} brain has baseline preferences".to_string());
    }
    if total >= 500 {
        milestones.push("\u{2713} 500+ decisions \u{2014} deep preference model".to_string());
    }
    if total >= 1000 {
        milestones.push("\u{2713} 1000+ decisions \u{2014} mature judgment".to_string());
    }
    if unique_patterns >= 20 {
        milestones.push(format!(
            "\u{2713} {unique_patterns} unique patterns recognized"
        ));
    }
    if correction_delta > 0.1 {
        milestones.push(format!(
            "\u{2713} Correction rate dropped {:.0}pp — brain is learning",
            correction_delta * 100.0,
        ));
    }

    let zero_false_approve = !decisions
        .iter()
        .any(|d| d.brain_action == "approve" && d.is_negative());
    if zero_false_approve && total >= 20 {
        milestones.push("\u{2713} Zero false approvals on risky actions".to_string());
    }

    milestones.truncate(4); // Max 4 milestones to keep it compact
    milestones
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
    println!("  help            Show this help");
    println!();
    println!("Aliases: evo, curve, acc, rules, fa, fd, dist, novel, cal, postmortem, ttc");
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::decisions::DecisionType;
    use super::*;

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
        }
    }

    // ── Dispatch tests ───────────────────────────────────────────────

    #[test]
    fn dispatch_help_no_panic() {
        // Just ensure it doesn't panic
        print_help();
    }
}

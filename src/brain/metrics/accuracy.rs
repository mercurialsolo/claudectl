//! Extracted from brain/metrics.rs — behavior-preserving split.

use super::*;

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

/// Compute rolling correction rate over decision history.
/// Returns one point per decision after the window fills.
pub(crate) fn rolling_correction_rate(
    decisions: &[DecisionRecord],
    window: usize,
) -> Vec<CurvePoint> {
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

/// A point on the learning curve: decision index and rolling correction rate.
#[derive(Debug, Clone)]
pub struct CurvePoint {
    pub index: usize,
    pub correction_rate: f64,
    pub window_size: usize,
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

pub(crate) fn print_accuracy_table(entries: &mut Vec<CategoryAccuracy>) {
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

pub(crate) fn print_temporal_accuracy(decisions: &[DecisionRecord]) {
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

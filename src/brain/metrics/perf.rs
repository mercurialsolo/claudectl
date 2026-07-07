//! Extracted from brain/metrics.rs — behavior-preserving split.

use super::*;

/// Summary row of accuracy + safety on one risk tier.
#[derive(Debug, Clone)]
pub struct TierStats {
    pub tier: RiskTier,
    pub n: usize,
    pub correct: usize,
    pub false_approves: usize,
    pub false_denies: usize,
    pub override_rate: f64,
}

impl TierStats {
    pub fn accuracy_pct(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        (self.correct as f64 / self.n as f64) * 100.0
    }

    pub fn false_approve_pct(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        (self.false_approves as f64 / self.n as f64) * 100.0
    }
}

/// Compute per-tier stats over every decision record where the brain was involved.
/// Observations (brain_action == "") are excluded — we can't score what the brain didn't predict.
pub fn compute_tier_stats(decisions: &[DecisionSummary]) -> Vec<TierStats> {
    let tiers = [
        RiskTier::Low,
        RiskTier::Medium,
        RiskTier::High,
        RiskTier::Critical,
    ];
    tiers
        .into_iter()
        .map(|tier| {
            let matching: Vec<&DecisionSummary> = decisions
                .iter()
                .filter(|d| !d.action.is_empty())
                .filter(|d| classify_risk(d.tool.as_deref(), d.command.as_deref()) == tier)
                .collect();
            let n = matching.len();
            let correct = matching.iter().filter(|d| d.is_positive()).count();
            let false_approves = matching
                .iter()
                .filter(|d| d.action == "approve" && d.is_negative())
                .count();
            let false_denies = matching
                .iter()
                .filter(|d| d.action == "deny" && d.is_negative())
                .count();
            let overrides = matching.iter().filter(|d| d.is_negative()).count();
            let override_rate = if n > 0 {
                overrides as f64 / n as f64
            } else {
                0.0
            };
            TierStats {
                tier,
                n,
                correct,
                false_approves,
                false_denies,
                override_rate,
            }
        })
        .collect()
}

/// Print the per-tier breakdown.
pub fn print_tier_breakdown() {
    let decisions = read_all_summaries();
    let stats = compute_tier_stats(&decisions);

    println!("Per-Risk-Tier Accuracy");
    println!("======================");
    println!();
    println!(
        "  {:<10}  {:>6}  {:>9}  {:>14}  {:>13}",
        "Tier", "n", "Accuracy", "False approves", "Override rate"
    );
    for s in &stats {
        if s.n == 0 {
            println!(
                "  {:<10}  {:>6}  {:>8}   {:>13}   {:>12}",
                s.tier.label(),
                0,
                "—",
                "—",
                "—"
            );
            continue;
        }
        let marker = if matches!(s.tier, RiskTier::Critical) && s.false_approves > 0 {
            " ⚠"
        } else {
            ""
        };
        println!(
            "  {:<10}  {:>6}  {:>8.1}%  {:>13.1}%  {:>12.1}%{}",
            s.tier.label(),
            s.n,
            s.accuracy_pct(),
            s.false_approve_pct(),
            s.override_rate * 100.0,
            marker,
        );
    }
    println!();
    println!("Notes:");
    println!("  - Critical-tier false-approves are the safety-critical number; target = 0.");
    println!("  - Override rate trending DOWN over time = brain is learning your patterns.");
}

/// Brain decision latency summary.
#[derive(Debug, Clone, Default)]
pub struct LatencySummary {
    pub n: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub mean_ms: u64,
    pub max_ms: u64,
}

/// Compute latency percentiles from decisions that have `brain_decision_ms` set.
pub fn compute_latency(decisions: &[DecisionSummary]) -> LatencySummary {
    let mut samples: Vec<u64> = decisions
        .iter()
        .filter_map(|d| d.brain_decision_ms)
        .collect();
    if samples.is_empty() {
        return LatencySummary::default();
    }
    samples.sort_unstable();
    let n = samples.len();
    let pct = |p: f64| -> u64 {
        let idx = ((n as f64 - 1.0) * p).round() as usize;
        samples[idx.min(n - 1)]
    };
    let sum: u64 = samples.iter().sum();
    LatencySummary {
        n,
        p50_ms: pct(0.50),
        p95_ms: pct(0.95),
        p99_ms: pct(0.99),
        mean_ms: sum / n as u64,
        max_ms: *samples.last().unwrap_or(&0),
    }
}

/// Print a latency report with histogram. Skips gracefully when no
/// instrumented records exist yet.
pub fn print_latency() {
    let decisions = read_all_summaries();
    let summary = compute_latency(&decisions);

    println!("Brain Decision Latency");
    println!("======================");
    println!();

    if summary.n == 0 {
        println!("No instrumented latency samples yet.");
        println!();
        println!("Latency is recorded automatically once brain decisions are made");
        println!("with the instrumented logging path. Existing decision history will");
        println!("not have this field — only new decisions count.");
        return;
    }

    let p95_marker = if summary.p95_ms <= 1000 {
        "✓"
    } else if summary.p95_ms <= 2000 {
        "⚠"
    } else {
        "✗"
    };

    println!("  Samples: {}", summary.n);
    println!("  p50:     {} ms", summary.p50_ms);
    println!(
        "  p95:     {} ms  {} (gating budget: ≤ 1000 ms)",
        summary.p95_ms, p95_marker
    );
    println!("  p99:     {} ms", summary.p99_ms);
    println!("  mean:    {} ms", summary.mean_ms);
    println!("  max:     {} ms", summary.max_ms);
    println!();

    // Histogram
    let buckets = [
        ("[0-100ms]", 0u64, 100u64),
        ("[100-300]", 100, 300),
        ("[300ms-1s]", 300, 1_000),
        ("[1s-3s]", 1_000, 3_000),
        ("[3s+]", 3_000, u64::MAX),
    ];
    let counts: Vec<usize> = buckets
        .iter()
        .map(|(_, lo, hi)| {
            decisions
                .iter()
                .filter_map(|d| d.brain_decision_ms)
                .filter(|&ms| ms >= *lo && ms < *hi)
                .count()
        })
        .collect();
    let total: usize = counts.iter().sum();

    println!("  Distribution:");
    for (i, (label, _, _)) in buckets.iter().enumerate() {
        let pct = if total > 0 {
            (counts[i] as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let bar_width = (pct / 2.5).round() as usize; // 100% = 40 chars
        let bar = "█".repeat(bar_width);
        println!(
            "    {:<11}  {:<40} {:5.1}%  ({})",
            label, bar, pct, counts[i]
        );
    }
}

/// Cache hit summary.
#[derive(Debug, Clone, Default)]
pub struct CacheSummary {
    pub instrumented: usize,
    pub hits: usize,
    pub misses: usize,
}

impl CacheSummary {
    pub fn hit_rate(&self) -> f64 {
        if self.instrumented == 0 {
            return 0.0;
        }
        (self.hits as f64 / self.instrumented as f64) * 100.0
    }
}

pub fn compute_cache(decisions: &[DecisionSummary]) -> CacheSummary {
    let hits = decisions
        .iter()
        .filter(|d| d.cache_hit == Some(true))
        .count();
    let misses = decisions
        .iter()
        .filter(|d| d.cache_hit == Some(false))
        .count();
    CacheSummary {
        instrumented: hits + misses,
        hits,
        misses,
    }
}

pub fn print_cache_hits() {
    let decisions = read_all_summaries();
    let summary = compute_cache(&decisions);

    println!("Few-Shot Cache Hit Rate");
    println!("=======================");
    println!();
    if summary.instrumented == 0 {
        println!("No instrumented cache samples yet.");
        println!();
        println!("Cache hits are recorded when a brain suggestion is satisfied from");
        println!("the few-shot store without a full LLM call. Older history won't");
        println!("have this field.");
        return;
    }
    println!("  Samples:    {}", summary.instrumented);
    println!(
        "  Cache hits: {} ({:.1}%)  — handled without an LLM call",
        summary.hits,
        summary.hit_rate()
    );
    println!(
        "  Misses:     {} ({:.1}%)  — full brain pass",
        summary.misses,
        100.0 - summary.hit_rate()
    );
    println!();
    println!("  Each cache hit saves the LLM latency + tokens. Healthy hit rate climbs");
    println!("  over time as the few-shot store fills with diverse past decisions.");
}

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

pub(crate) fn print_distribution_table(label: &str, data: &HashMap<String, u32>, total: usize) {
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

/// Compute a metric in batches, returning one value per batch.
pub(crate) fn batch_metric(
    decisions: &[DecisionRecord],
    batch_size: usize,
    mut metric_fn: impl FnMut(&[DecisionRecord]) -> f64,
) -> Vec<f64> {
    if decisions.is_empty() || batch_size == 0 {
        return Vec::new();
    }
    decisions.chunks(batch_size).map(&mut metric_fn).collect()
}

pub(crate) fn compute_milestones(
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

//! Extracted from brain/metrics.rs — behavior-preserving split.

use super::*;

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

    // Friction cost: time spent overriding denials
    let mut override_delays: Vec<u64> = Vec::new();
    let mut by_reason: HashMap<String, u32> = HashMap::new();
    for d in &decisions {
        if d.brain_action == "deny" && d.is_positive() {
            if let (Some(suggested), Some(resolved)) = (d.suggested_at, d.resolved_at) {
                let delay = resolved.saturating_sub(suggested);
                if delay < 3600 {
                    // Ignore delays > 1 hour (likely stale)
                    override_delays.push(delay);
                }
            }
            if let Some(ref reason) = d.override_reason {
                *by_reason.entry(reason.clone()).or_insert(0) += 1;
            }
        }
    }

    if !override_delays.is_empty() {
        let avg_delay = override_delays.iter().sum::<u64>() as f64 / override_delays.len() as f64;
        let total_friction = avg_delay * total_overrides as f64;
        println!();
        println!("  Friction Cost");
        println!("  {}", "-".repeat(40));
        println!("  avg override delay:     {:.1}s", avg_delay,);
        println!(
            "  total friction time:    {:.0}s ({:.1} min)",
            total_friction,
            total_friction / 60.0,
        );
    }

    if !by_reason.is_empty() {
        println!();
        println!("  Override Reasons");
        println!("  {}", "-".repeat(40));
        let mut reasons: Vec<(String, u32)> = by_reason.into_iter().collect();
        reasons.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        for (reason, count) in &reasons {
            println!("  {:<25} {:>5}", reason, count);
        }
    }
}

pub fn print_counterfactuals() {
    let decisions = read_all_summaries();
    let cfs = compute_counterfactuals(&decisions);

    println!("Counterfactual Analysis");
    println!("=======================");
    println!();

    if cfs.is_empty() {
        println!("No counterfactual cases found.");
        println!();
        println!("Counterfactuals are user-overrides that the subsequent outcome");
        println!("argued against — strong signal for the few-shot store.");
        return;
    }

    let brain_right: Vec<&Counterfactual> = cfs.iter().filter(|c| c.brain_was_right).collect();
    let user_right: Vec<&Counterfactual> = cfs.iter().filter(|c| !c.brain_was_right).collect();

    println!(
        "  Brain was likely right (user overrode → outcome failed): {}",
        brain_right.len()
    );
    println!(
        "  User was likely right (brain over-cautious):            {}",
        user_right.len()
    );
    println!();

    if !brain_right.is_empty() {
        println!("Top brain-was-right cases (review candidates):");
        println!();
        for cf in brain_right.iter().take(10) {
            println!(
                "  • [{}] {} → {} (conf {:.0}%)",
                cf.tool.as_deref().unwrap_or("?"),
                cf.brain_action,
                cf.user_action,
                cf.brain_confidence * 100.0
            );
            if let Some(cmd) = &cf.command {
                println!("    cmd: {}", truncate(cmd, 80));
            }
            println!("    outcome: {}", cf.outcome_summary);
            if let Some(id) = &cf.decision_id {
                println!("    mark canonical: claudectl brain review --mark {}", id);
            }
            println!();
        }
    }

    if !user_right.is_empty() {
        println!("Top brain-over-cautious cases (consider relaxing):");
        println!();
        for cf in user_right.iter().take(5) {
            println!(
                "  • [{}] brain said {} (conf {:.0}%) → user accepted, outcome ok",
                cf.tool.as_deref().unwrap_or("?"),
                cf.brain_action,
                cf.brain_confidence * 100.0
            );
            if let Some(cmd) = &cf.command {
                println!("    cmd: {}", truncate(cmd, 80));
            }
            println!();
        }
    }
}

/// A counterfactual finding: brain disagreed with the user, and the outcome
/// validated the brain (or, less commonly, the user).
#[derive(Debug, Clone)]
pub struct Counterfactual {
    pub decision_id: Option<String>,
    pub project: String,
    pub tool: Option<String>,
    pub command: Option<String>,
    pub brain_action: String,
    pub user_action: String,
    pub brain_confidence: f64,
    /// True when the *brain* was likely right — user overrode and the outcome
    /// degraded (TestFailed or Error). These are the highest-value review
    /// candidates: marking them canonical teaches the few-shot store.
    pub brain_was_right: bool,
    pub outcome_summary: String,
}

/// Find counterfactual cases: where brain and user disagreed, and the
/// subsequent outcome went badly. Heuristic: a `TestFailed` or `Error`
/// outcome on a same-PID decision within `WINDOW` records after this one.
pub fn compute_counterfactuals(decisions: &[DecisionSummary]) -> Vec<Counterfactual> {
    const WINDOW: usize = 5;
    let mut out = Vec::new();
    for (i, d) in decisions.iter().enumerate() {
        // We only care about cases where brain was involved AND user disagreed.
        if d.action.is_empty() {
            continue;
        }
        if !d.is_negative() {
            continue;
        }
        // Look ahead WINDOW records on the same PID for an attributable failure.
        let mut failing: Option<String> = None;
        let upper = (i + 1 + WINDOW).min(decisions.len());
        for next in &decisions[i + 1..upper] {
            if next.pid != d.pid {
                continue;
            }
            match next.outcome_kind.as_deref() {
                Some("test_failed") => {
                    let cmd = next.outcome_detail.as_deref().unwrap_or("");
                    failing = Some(format!("TestFailed: {}", truncate(cmd, 60)));
                    break;
                }
                Some("error") => {
                    let msg = next.outcome_detail.as_deref().unwrap_or("");
                    failing = Some(format!("Error: {}", truncate(msg, 60)));
                    break;
                }
                _ => {}
            }
        }
        if let Some(summary) = failing {
            // action == "deny" and user accepted → user-accepted thing
            // led to failure → brain was right.
            let brain_was_right = d.action == "deny" || d.action == "ask";
            out.push(Counterfactual {
                decision_id: if d.id.is_empty() {
                    None
                } else {
                    Some(d.id.clone())
                },
                project: d.project.clone().unwrap_or_default(),
                tool: d.tool.clone(),
                command: d.command.clone(),
                brain_action: d.action.clone(),
                user_action: d.user_action.clone().unwrap_or_default(),
                brain_confidence: d.confidence.unwrap_or(0.0),
                brain_was_right,
                outcome_summary: summary,
            });
        }
    }
    out
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

/// Classify the root cause of a false approval.
pub(crate) fn classify_incident_cause(
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

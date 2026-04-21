#![allow(dead_code)]

use std::collections::HashMap;

use super::decisions::read_all_decisions;
use super::risk::{RiskTier, classify_risk};

// ────────────────────────────────────────────────────────────────────────────
// #136: Rules baseline comparison
// ────────────────────────────────────────────────────────────────────────────

/// A deterministic rules-only classifier for baseline comparison.
pub fn rules_baseline_classify(tool: Option<&str>, command: Option<&str>) -> &'static str {
    let tool = tool.unwrap_or("");
    let cmd = command.unwrap_or("").to_lowercase();

    // Always approve: read-only tools
    if matches!(tool, "Read" | "Glob" | "Grep" | "LS" | "Explore") {
        return "approve";
    }

    // Always deny: destructive patterns
    const DENY_PATTERNS: &[&str] = &[
        "rm -rf",
        "rm -fr",
        "git push --force",
        "git push -f",
        "git reset --hard",
        "drop table",
        "drop database",
        "--no-verify",
        "chmod 777",
    ];
    for pat in DENY_PATTERNS {
        if cmd.contains(pat) {
            return "deny";
        }
    }

    // Approve safe bash commands
    if tool == "Bash" {
        const SAFE_CMDS: &[&str] = &[
            "cargo test",
            "cargo build",
            "cargo check",
            "cargo clippy",
            "cargo fmt",
            "npm test",
            "npm run",
            "pytest",
            "go test",
            "make",
            "git status",
            "git log",
            "git diff",
            "git branch",
            "ls",
            "pwd",
            "echo",
            "cat ",
            "head ",
            "tail ",
        ];
        for pat in SAFE_CMDS {
            if cmd.starts_with(pat) || cmd.contains(pat) {
                return "approve";
            }
        }
    }

    // Approve file edits to test files
    if matches!(tool, "Edit" | "Write") {
        if cmd.contains("test") || cmd.contains("spec") || cmd.contains("_test.") {
            return "approve";
        }
    }

    // Default: abstain (can't decide)
    "abstain"
}

/// Print rules baseline comparison.
pub fn print_baseline() {
    let decisions = read_all_decisions();
    let total = decisions.len();

    println!("Rules Baseline Comparison");
    println!("=========================");
    println!();

    if total < 10 {
        println!("  Not enough decisions yet ({total}). Need at least 10.");
        return;
    }

    let mut brain_correct = 0u32;
    let mut brain_wrong = 0u32;
    let mut rules_correct = 0u32;
    let mut rules_wrong = 0u32;
    let mut rules_abstain = 0u32;
    let mut both_correct = 0u32;
    let mut brain_only = 0u32;
    let mut rules_only = 0u32;
    let mut both_wrong = 0u32;

    // Per-risk breakdown
    let mut risk_stats: HashMap<RiskTier, (u32, u32, u32, u32)> = HashMap::new(); // (brain_correct, brain_wrong, rules_correct, rules_wrong)

    for d in &decisions {
        // Ground truth: what the user wanted
        let user_wanted = if d.is_positive() {
            &d.brain_action // user agreed with brain
        } else if d.is_negative() {
            // user disagreed — the opposite
            if d.brain_action == "approve" {
                "deny"
            } else {
                "approve"
            }
        } else {
            continue; // no signal
        };

        let rules_said = rules_baseline_classify(d.tool.as_deref(), d.command.as_deref());
        let brain_said = d.brain_action.as_str();
        let risk = classify_risk(d.tool.as_deref(), d.command.as_deref());

        let brain_right = brain_said == user_wanted;
        let rules_right = rules_said == user_wanted;
        let rules_skipped = rules_said == "abstain";

        if brain_right {
            brain_correct += 1;
        } else {
            brain_wrong += 1;
        }

        if rules_skipped {
            rules_abstain += 1;
        } else if rules_right {
            rules_correct += 1;
        } else {
            rules_wrong += 1;
        }

        match (brain_right, rules_right || rules_skipped) {
            (true, true) if !rules_skipped => both_correct += 1,
            (true, _) => brain_only += 1,
            (false, true) if !rules_skipped => rules_only += 1,
            _ => both_wrong += 1,
        }

        // Risk breakdown
        let rs = risk_stats.entry(risk).or_insert((0, 0, 0, 0));
        if brain_right {
            rs.0 += 1;
        } else {
            rs.1 += 1;
        }
        if !rules_skipped {
            if rules_right {
                rs.2 += 1;
            } else {
                rs.3 += 1;
            }
        }
    }

    let decided = brain_correct + brain_wrong;
    let rules_decided = rules_correct + rules_wrong;

    // Overall comparison
    println!("  Overall ({decided} decisions with feedback):");
    println!();
    println!(
        "    {:<25} {:>8} {:>8} {:>8}",
        "", "Correct", "Wrong", "Accuracy"
    );
    println!("    {}", "-".repeat(49));

    if decided > 0 {
        println!(
            "    {:<25} {:>8} {:>8} {:>7.1}%",
            "Brain (LLM)",
            brain_correct,
            brain_wrong,
            (brain_correct as f64 / decided as f64) * 100.0,
        );
    }
    if rules_decided > 0 {
        println!(
            "    {:<25} {:>8} {:>8} {:>7.1}%",
            "Rules baseline",
            rules_correct,
            rules_wrong,
            (rules_correct as f64 / rules_decided as f64) * 100.0,
        );
    }
    println!(
        "    {:<25} {:>8}",
        "Rules abstained (no match)", rules_abstain,
    );

    // Venn diagram
    println!();
    println!("  Agreement:");
    println!("    Both correct:      {both_correct}");
    println!("    Brain only correct: {brain_only}");
    println!("    Rules only correct: {rules_only}");
    println!("    Both wrong:        {both_wrong}");

    // Per-risk breakdown
    println!();
    println!("  By risk tier:");
    println!(
        "    {:<12} {:>12} {:>12} {:>8}",
        "Risk", "Brain acc.", "Rules acc.", "Delta"
    );
    println!("    {}", "-".repeat(48));

    for risk in &[
        RiskTier::Low,
        RiskTier::Medium,
        RiskTier::High,
        RiskTier::Critical,
    ] {
        if let Some(&(bc, bw, rc, rw)) = risk_stats.get(risk) {
            let b_total = bc + bw;
            let r_total = rc + rw;
            let b_acc = if b_total > 0 {
                (bc as f64 / b_total as f64) * 100.0
            } else {
                0.0
            };
            let r_acc = if r_total > 0 {
                (rc as f64 / r_total as f64) * 100.0
            } else {
                0.0
            };
            let delta = b_acc - r_acc;
            let delta_str = if r_total == 0 {
                "n/a".to_string()
            } else {
                format!("{delta:+.1}pp")
            };
            println!(
                "    {:<12} {:>11.1}% {:>11.1}% {:>8}",
                risk.label(),
                b_acc,
                r_acc,
                delta_str,
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_approves_reads() {
        assert_eq!(
            rules_baseline_classify(Some("Read"), Some("file.rs")),
            "approve"
        );
        assert_eq!(
            rules_baseline_classify(Some("Glob"), Some("**/*.ts")),
            "approve"
        );
        assert_eq!(
            rules_baseline_classify(Some("Grep"), Some("TODO")),
            "approve"
        );
    }

    #[test]
    fn rules_denies_destructive() {
        assert_eq!(
            rules_baseline_classify(Some("Bash"), Some("rm -rf /tmp")),
            "deny"
        );
        assert_eq!(
            rules_baseline_classify(Some("Bash"), Some("git push --force")),
            "deny"
        );
    }

    #[test]
    fn rules_approves_safe_bash() {
        assert_eq!(
            rules_baseline_classify(Some("Bash"), Some("cargo test")),
            "approve"
        );
        assert_eq!(
            rules_baseline_classify(Some("Bash"), Some("git status")),
            "approve"
        );
    }

    #[test]
    fn rules_abstains_on_unknown() {
        assert_eq!(
            rules_baseline_classify(Some("Bash"), Some("python train.py")),
            "abstain"
        );
        assert_eq!(
            rules_baseline_classify(Some("Edit"), Some("src/main.rs")),
            "abstain"
        );
    }

    #[test]
    fn rules_approves_test_file_edits() {
        assert_eq!(
            rules_baseline_classify(Some("Write"), Some("tests/unit_test.rs")),
            "approve"
        );
    }
}

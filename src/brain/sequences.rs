#![allow(dead_code)]

//! Anti-pattern sequence detection (#201).
//!
//! Single-decision detectors miss multi-step failure shapes: `edit → edit → edit`
//! without a test; `deny → deny → deny` on the same tool; `bash(npm install)` on
//! repeat. This module extracts n-grams of `(tool, command_keyword, has_error)`
//! per session, scores each by P(bad terminal | sequence), and persists a
//! library that downstream callers (detectors, engine) consult.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use super::decisions::{DecisionRecord, decisions_dir};
use super::detectors::extract_command_keyword;
use super::insights::{Insight, InsightCategory, InsightSeverity, epoch_now};

// ────────────────────────────────────────────────────────────────────────────
// Tunables
// ────────────────────────────────────────────────────────────────────────────

/// Minimum total observations of a sequence before it can become an anti-pattern.
const MIN_OCCURRENCES: u32 = 3;

/// Sequence is flagged as an anti-pattern only if at least this fraction of
/// occurrences end in a bad terminal.
const MIN_BAD_RATE: f64 = 0.6;

/// N-gram range we mine over: 2-grams through 5-grams (issue #201).
const MIN_N: usize = 2;
const MAX_N: usize = 5;

/// Drop sequences with more steps than this (very rare, noisy).
const MAX_LIBRARY_N: usize = 8;

// ────────────────────────────────────────────────────────────────────────────
// Data shapes
// ────────────────────────────────────────────────────────────────────────────

/// One step in an anti-pattern sequence. `cmd` is the first-two-token keyword,
/// matching how `detectors.rs` already groups commands.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeqStep {
    pub tool: String,
    pub cmd: Option<String>,
    pub had_error: bool,
}

impl SeqStep {
    pub fn display(&self) -> String {
        let cmd_part = self
            .cmd
            .as_ref()
            .map(|c| format!(" \"{c}\""))
            .unwrap_or_default();
        let err = if self.had_error { "!" } else { "" };
        format!("[{}]{}{}", self.tool, cmd_part, err)
    }

    fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}",
            self.tool,
            self.cmd.as_deref().unwrap_or(""),
            self.had_error as u8
        )
    }
}

/// A discovered anti-pattern with its outcome stats.
#[derive(Debug, Clone)]
pub struct AntiPattern {
    pub steps: Vec<SeqStep>,
    /// Total times this sequence appeared in any session.
    pub total_occurrences: u32,
    /// Of those, how many ended in a bad terminal (error, rejection, blowout).
    pub bad_terminals: u32,
    /// Last epoch second the sequence was observed.
    pub last_seen: u64,
    /// Avg cost (USD) of the step immediately after the sequence ended in a
    /// bad terminal — proxy for downstream waste.
    pub avg_downstream_cost: f64,
}

impl AntiPattern {
    pub fn bad_rate(&self) -> f64 {
        if self.total_occurrences == 0 {
            return 0.0;
        }
        self.bad_terminals as f64 / self.total_occurrences as f64
    }

    pub fn fingerprint(&self) -> String {
        let body = self
            .steps
            .iter()
            .map(|s| s.fingerprint())
            .collect::<Vec<_>>()
            .join(">");
        format!("antipattern:{body}")
    }

    pub fn display(&self) -> String {
        self.steps
            .iter()
            .map(|s| s.display())
            .collect::<Vec<_>>()
            .join(" → ")
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Persistence
// ────────────────────────────────────────────────────────────────────────────

fn antipatterns_path() -> PathBuf {
    decisions_dir().join("decisions").join("antipatterns.json")
}

/// Persist the discovered library. Stable JSON layout for inspection and tests.
pub fn save_library(library: &[AntiPattern]) -> Result<(), String> {
    let path = antipatterns_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let json = serde_json::json!({
        "generated_at": epoch_now(),
        "antipatterns": library.iter().map(antipattern_to_json).collect::<Vec<_>>(),
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&json).map_err(|e| format!("json error: {e}"))?,
    )
    .map_err(|e| format!("write error: {e}"))
}

pub fn load_library() -> Vec<AntiPattern> {
    let path = antipatterns_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    json.get("antipatterns")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(antipattern_from_json).collect())
        .unwrap_or_default()
}

fn antipattern_to_json(ap: &AntiPattern) -> serde_json::Value {
    serde_json::json!({
        "steps": ap.steps.iter().map(|s| serde_json::json!({
            "tool": s.tool,
            "cmd": s.cmd,
            "had_error": s.had_error,
        })).collect::<Vec<_>>(),
        "total_occurrences": ap.total_occurrences,
        "bad_terminals": ap.bad_terminals,
        "last_seen": ap.last_seen,
        "avg_downstream_cost": ap.avg_downstream_cost,
    })
}

fn antipattern_from_json(v: &serde_json::Value) -> Option<AntiPattern> {
    let steps = v
        .get("steps")?
        .as_array()?
        .iter()
        .filter_map(|s| {
            Some(SeqStep {
                tool: s.get("tool")?.as_str()?.to_string(),
                cmd: s.get("cmd").and_then(|c| c.as_str()).map(|s| s.to_string()),
                had_error: s
                    .get("had_error")
                    .and_then(|c| c.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    if steps.is_empty() {
        return None;
    }
    Some(AntiPattern {
        steps,
        total_occurrences: v.get("total_occurrences")?.as_u64()? as u32,
        bad_terminals: v.get("bad_terminals")?.as_u64()? as u32,
        last_seen: v.get("last_seen").and_then(|c| c.as_u64()).unwrap_or(0),
        avg_downstream_cost: v
            .get("avg_downstream_cost")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0),
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Mining: turn raw decisions into an anti-pattern library
// ────────────────────────────────────────────────────────────────────────────

/// True if this decision should be treated as a bad terminal state when it
/// follows a sequence. We avoid double-counting "had_error" at the step level
/// itself — terminality is judged on a separate signal:
///
/// - User rejected the brain's approve
/// - Auto-approved decision was followed by an error-heavy context
/// - High context (>80%) right after the sequence (blowout)
fn is_bad_terminal(d: &DecisionRecord) -> bool {
    if d.is_negative() {
        return true;
    }
    let Some(ctx) = d.context.as_ref() else {
        return false;
    };
    if ctx.last_tool_error && d.is_positive() {
        // We approved into an error
        return true;
    }
    if ctx.context_pct >= 80 {
        return true;
    }
    false
}

fn step_from(d: &DecisionRecord) -> Option<SeqStep> {
    let tool = d.tool.clone()?;
    let cmd = extract_command_keyword(d.command.as_deref());
    let had_error = d
        .context
        .as_ref()
        .map(|c| c.last_tool_error)
        .unwrap_or(false);
    Some(SeqStep {
        tool,
        cmd,
        had_error,
    })
}

/// Mine the decision log for anti-patterns.
///
/// Algorithm:
/// 1. Group decisions by session pid, sort by index (jsonl order is temporal).
/// 2. For each session, walk every contiguous window of length n ∈ [MIN_N, MAX_N].
/// 3. For each window, record the step *immediately after* it: if that next
///    decision is a bad terminal, count it as a bad outcome.
/// 4. Aggregate across sessions; keep only sequences crossing both
///    MIN_OCCURRENCES and MIN_BAD_RATE.
pub fn mine_antipatterns(decisions: &[DecisionRecord]) -> Vec<AntiPattern> {
    if decisions.is_empty() {
        return Vec::new();
    }

    // Group by pid; preserve insertion order so we walk in temporal order.
    let mut by_session: HashMap<u32, Vec<&DecisionRecord>> = HashMap::new();
    for d in decisions {
        by_session.entry(d.pid).or_default().push(d);
    }

    #[derive(Default)]
    struct Stats {
        total: u32,
        bad: u32,
        last_seen: u64,
        cost_acc: f64,
        cost_n: u32,
    }
    let mut agg: HashMap<Vec<SeqStep>, Stats> = HashMap::new();

    for session in by_session.values() {
        let steps: Vec<Option<SeqStep>> = session.iter().map(|d| step_from(d)).collect();
        for n in MIN_N..=MAX_N {
            if session.len() <= n {
                continue;
            }
            for start in 0..session.len() - n {
                // Collect the window; skip windows containing a None step.
                let window: Option<Vec<SeqStep>> =
                    steps[start..start + n].iter().cloned().collect();
                let Some(window) = window else { continue };

                let next = session[start + n];
                let entry = agg.entry(window).or_default();
                entry.total += 1;
                if is_bad_terminal(next) {
                    entry.bad += 1;
                    if let Some(ctx) = next.context.as_ref() {
                        entry.cost_acc += ctx.cost_usd;
                        entry.cost_n += 1;
                    }
                }
                let ts = next.resolved_at.unwrap_or(0);
                if ts > entry.last_seen {
                    entry.last_seen = ts;
                }
            }
        }
    }

    let mut out: Vec<AntiPattern> = agg
        .into_iter()
        .filter_map(|(steps, s)| {
            if s.total < MIN_OCCURRENCES {
                return None;
            }
            let rate = s.bad as f64 / s.total as f64;
            if rate < MIN_BAD_RATE {
                return None;
            }
            let avg_cost = if s.cost_n > 0 {
                s.cost_acc / s.cost_n as f64
            } else {
                0.0
            };
            Some(AntiPattern {
                steps,
                total_occurrences: s.total,
                bad_terminals: s.bad,
                last_seen: s.last_seen,
                avg_downstream_cost: avg_cost,
            })
        })
        .filter(|ap| ap.steps.len() <= MAX_LIBRARY_N)
        .collect();

    // Sort by severity (bad_rate × occurrences) descending so the worst
    // patterns surface first.
    out.sort_by(|a, b| {
        let score_a = a.bad_rate() * a.total_occurrences as f64;
        let score_b = b.bad_rate() * b.total_occurrences as f64;
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

// ────────────────────────────────────────────────────────────────────────────
// Live prefix matching
// ────────────────────────────────────────────────────────────────────────────

/// Match the tail of an in-flight session against the anti-pattern library.
/// Returns the most-severe matched anti-pattern whose first (k) steps match
/// the last k steps of `recent`, where k = library entry length minus 1
/// (i.e. we match the prefix and the upcoming next decision is the predicted
/// "bad terminal").
///
/// This is the integration point the engine can call to lower confidence on
/// the next decision, surface a warning, or short-circuit auto-approve.
pub fn match_prefix<'a>(
    recent: &[DecisionRecord],
    library: &'a [AntiPattern],
) -> Option<&'a AntiPattern> {
    if recent.is_empty() {
        return None;
    }
    let recent_steps: Vec<SeqStep> = recent.iter().filter_map(step_from).collect();

    let mut best: Option<&AntiPattern> = None;
    for ap in library {
        if ap.steps.is_empty() || ap.steps.len() > recent_steps.len() {
            continue;
        }
        let tail = &recent_steps[recent_steps.len() - ap.steps.len()..];
        if tail == ap.steps.as_slice() {
            match best {
                None => best = Some(ap),
                Some(cur) => {
                    let cur_score = cur.bad_rate() * cur.total_occurrences as f64;
                    let new_score = ap.bad_rate() * ap.total_occurrences as f64;
                    if new_score > cur_score {
                        best = Some(ap);
                    }
                }
            }
        }
    }
    best
}

// ────────────────────────────────────────────────────────────────────────────
// Detector — convert anti-patterns into Insights
// ────────────────────────────────────────────────────────────────────────────

pub(crate) fn detect_antipattern_sequences(decisions: &[DecisionRecord]) -> Vec<Insight> {
    let library = mine_antipatterns(decisions);
    if library.is_empty() {
        return Vec::new();
    }
    let now = epoch_now();
    library
        .iter()
        .map(|ap| {
            let bad_rate = ap.bad_rate();
            let severity = if bad_rate >= 0.9 && ap.total_occurrences >= 5 {
                InsightSeverity::Warning
            } else {
                InsightSeverity::Suggestion
            };
            let cost_part = if ap.avg_downstream_cost > 0.0 {
                format!(", avg ${:.2} downstream", ap.avg_downstream_cost)
            } else {
                String::new()
            };
            Insight {
                fingerprint: ap.fingerprint(),
                generated_at: now,
                category: InsightCategory::AntiPattern,
                severity,
                summary: format!(
                    "{} → bad outcome {}/{} ({:.0}%{})",
                    ap.display(),
                    ap.bad_terminals,
                    ap.total_occurrences,
                    bad_rate * 100.0,
                    cost_part,
                ),
                suggestion: Some(format!(
                    "watch for this prefix in live sessions; n={}",
                    ap.steps.len()
                )),
                evidence_count: ap.total_occurrences,
            }
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::decisions::{DecisionContext, DecisionType};

    fn make_d(pid: u32, tool: &str, cmd: &str, user_action: &str, error: bool) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".into(),
            pid,
            project: "test".into(),
            tool: Some(tool.into()),
            command: Some(cmd.into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: String::new(),
            user_action: user_action.into(),
            context: Some(DecisionContext {
                cost_usd: 0.5,
                context_pct: if error { 50 } else { 40 },
                last_tool_error: error,
                error_message: None,
                model: "test".into(),
                elapsed_secs: 60,
                files_modified_count: 0,
                total_tool_calls: 1,
                has_file_conflict: false,
                status: "Processing".into(),
                burn_rate_per_hr: 1.0,
                recent_error_count: 0,
                subagent_count: 0,
                hour: None,
            }),
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: Some(1000 + pid as u64),
            override_reason: None,
            decision_id: None,
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
        }
    }

    /// Three sessions all show edit → edit followed by a rejection.
    /// We expect a 2-gram anti-pattern.
    #[test]
    fn mines_2gram_on_repeated_bad_terminal() {
        let mut decisions = Vec::new();
        for pid in 1..=4 {
            decisions.push(make_d(pid, "Edit", "src/main.rs", "accept", false));
            decisions.push(make_d(pid, "Edit", "src/main.rs", "accept", false));
            // bad terminal: user rejected
            decisions.push(make_d(pid, "Bash", "cargo build", "reject", false));
        }
        let lib = mine_antipatterns(&decisions);
        assert!(!lib.is_empty(), "library should contain at least one ap");
        let any_2 = lib.iter().any(|ap| ap.steps.len() == 2);
        assert!(any_2, "expected a 2-gram anti-pattern");
        let leading = &lib[0];
        assert!(
            leading.bad_rate() >= MIN_BAD_RATE,
            "top ap should clear the bad-rate threshold"
        );
    }

    #[test]
    fn skips_when_too_few_occurrences() {
        // Only two sessions — below MIN_OCCURRENCES of 3.
        let mut decisions = Vec::new();
        for pid in 1..=2 {
            decisions.push(make_d(pid, "Edit", "x", "accept", false));
            decisions.push(make_d(pid, "Edit", "x", "accept", false));
            decisions.push(make_d(pid, "Bash", "cmd", "reject", false));
        }
        let lib = mine_antipatterns(&decisions);
        assert!(
            lib.is_empty(),
            "no anti-pattern should emerge below threshold"
        );
    }

    #[test]
    fn matches_prefix_in_live_session() {
        let mut decisions = Vec::new();
        for pid in 1..=4 {
            decisions.push(make_d(pid, "Edit", "src/lib.rs", "accept", false));
            decisions.push(make_d(pid, "Edit", "src/lib.rs", "accept", false));
            decisions.push(make_d(pid, "Bash", "cargo run", "reject", false));
        }
        let lib = mine_antipatterns(&decisions);
        let live = vec![
            make_d(99, "Edit", "src/lib.rs", "accept", false),
            make_d(99, "Edit", "src/lib.rs", "accept", false),
        ];
        let matched = match_prefix(&live, &lib);
        assert!(matched.is_some(), "should match the 2-gram tail");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let lib = vec![AntiPattern {
            steps: vec![
                SeqStep {
                    tool: "Edit".into(),
                    cmd: Some("src/main.rs".into()),
                    had_error: false,
                },
                SeqStep {
                    tool: "Bash".into(),
                    cmd: Some("cargo build".into()),
                    had_error: true,
                },
            ],
            total_occurrences: 7,
            bad_terminals: 6,
            last_seen: 12345,
            avg_downstream_cost: 0.42,
        }];
        let tmp = tempfile::tempdir().unwrap();
        // Redirect HOME so antipatterns_path() points into the temp dir.
        let original_home = std::env::var("HOME").ok();
        // SAFETY: tests are single-threaded by Cargo default for cfg-controlled
        // env mutation here; we restore HOME below.
        unsafe { std::env::set_var("HOME", tmp.path()) };
        save_library(&lib).expect("save");
        let loaded = load_library();
        if let Some(h) = original_home {
            unsafe { std::env::set_var("HOME", h) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].steps.len(), 2);
        assert_eq!(loaded[0].total_occurrences, 7);
        assert_eq!(loaded[0].bad_terminals, 6);
    }
}

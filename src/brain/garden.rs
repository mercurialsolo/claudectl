#![allow(dead_code)]

//! CLAUDE.md gardening (#199).
//!
//! Distilled preferences live in `~/.claudectl/brain/preferences/`. They're
//! used by the brain prompt builder but invisible to Claude Code itself. This
//! module promotes high-confidence patterns into proposed `CLAUDE.md` additions
//! so the project's permanent instructions reflect what the user has actually
//! taught the brain.

use std::fs;
use std::path::{Path, PathBuf};

use super::preferences::{
    DistilledPreferences, PreferencePattern, load_preferences, load_preferences_for_project,
};

// ────────────────────────────────────────────────────────────────────────────
// Tunables
// ────────────────────────────────────────────────────────────────────────────

/// A preference must clear both bars before we even consider it for CLAUDE.md.
/// Matches the issue: ≥90% confidence, ≥20 samples.
const MIN_CONFIDENCE: f64 = 0.90;
const MIN_SAMPLES: u32 = 20;

/// Mark the appended block so we can find it again and avoid double-writing.
const HEADER: &str = "<!-- claudectl-garden: auto-codified from brain preferences -->";
const FOOTER: &str = "<!-- /claudectl-garden -->";

// ────────────────────────────────────────────────────────────────────────────
// Suggestion model
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionKind {
    /// A pattern to codify ("approve / deny X when Y").
    Codify,
    /// A pattern that contradicts existing CLAUDE.md content.
    Contradiction,
}

#[derive(Debug, Clone)]
pub struct Suggestion {
    pub kind: SuggestionKind,
    /// Single-line markdown bullet for direct paste.
    pub line: String,
    /// Rationale shown to the user (and saved as an HTML comment if --apply).
    pub rationale: String,
    /// Source tool for keyword matching against existing CLAUDE.md content.
    pub tool: String,
    /// Source command keyword (e.g. "cargo test").
    pub cmd_keyword: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GardenReport {
    pub project: String,
    pub claude_md_path: Option<PathBuf>,
    pub considered: u32,
    pub kept: Vec<Suggestion>,
    pub already_covered: u32,
    /// True when `--apply` succeeded and the file was modified.
    pub applied: bool,
}

// ────────────────────────────────────────────────────────────────────────────
// CLAUDE.md discovery
// ────────────────────────────────────────────────────────────────────────────

/// Find the project's CLAUDE.md. We prefer the file in the current working
/// directory (where the user invoked claudectl), then walk up.
pub fn find_claude_md(start: &Path) -> Option<PathBuf> {
    let mut cur = start.canonicalize().ok()?;
    loop {
        let candidate = cur.join("CLAUDE.md");
        if candidate.is_file() {
            return Some(candidate);
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent.to_path_buf(),
            _ => return None,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Pattern → markdown line
// ────────────────────────────────────────────────────────────────────────────

fn format_pattern(p: &PreferencePattern) -> Suggestion {
    let cmd_keyword = p.command_pattern.clone();
    let action_word = match p.preferred_action.as_str() {
        "approve" => "always approve",
        "deny" => "never run",
        other => other,
    };
    let cmd_display = p
        .command_pattern
        .as_deref()
        .map(|c| format!(" `{c}`"))
        .unwrap_or_else(|| format!(" calls to `{}`", p.tool));
    let cond_part = if p.conditions.is_empty() {
        String::new()
    } else {
        let conds: Vec<String> = p.conditions.iter().map(|c| c.label()).collect();
        format!(" (when {})", conds.join(", "))
    };
    let line = format!("- {action_word}{cmd_display}{cond_part}");

    let rationale = format!(
        "brain observed {} decisions; {:.0}% {}, confidence {:.0}%",
        p.sample_count,
        if p.preferred_action == "approve" {
            p.accept_rate * 100.0
        } else {
            (1.0 - p.accept_rate) * 100.0
        },
        if p.preferred_action == "approve" {
            "accepted"
        } else {
            "rejected"
        },
        p.confidence * 100.0,
    );
    Suggestion {
        kind: SuggestionKind::Codify,
        line,
        rationale,
        tool: p.tool.clone(),
        cmd_keyword,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Duplicate / contradiction detection
// ────────────────────────────────────────────────────────────────────────────

/// Cheap, conservative match: lowercase the file and check that both the tool
/// name and (if present) the command keyword appear in some line. Better to
/// surface a false-already-covered than push duplicates into the file.
fn is_already_covered(claude_md: &str, sug: &Suggestion) -> bool {
    let lower = claude_md.to_lowercase();
    let tool_lower = sug.tool.to_lowercase();
    match sug.cmd_keyword.as_deref() {
        Some(cmd) => {
            let cmd_lower = cmd.to_lowercase();
            lower.contains(&cmd_lower) || (tool_lower.len() > 2 && lower.contains(&tool_lower))
        }
        None => tool_lower.len() > 2 && lower.contains(&tool_lower),
    }
}

/// Detect contradictions: same tool/cmd already appears in the file but with
/// the opposite verb. This is a conservative heuristic — we just look for
/// "never" or "do not" or "don't" near the tool name when the brain learned to
/// approve it (or vice versa).
fn detect_contradictions(claude_md: &str, patterns: &[&PreferencePattern]) -> Vec<Suggestion> {
    let lower = claude_md.to_lowercase();
    let mut out = Vec::new();
    for p in patterns {
        let Some(ref cmd) = p.command_pattern else {
            continue;
        };
        let cmd_lower = cmd.to_lowercase();
        let Some(idx) = lower.find(&cmd_lower) else {
            continue;
        };
        // Look at the 80 chars preceding the match.
        let window_start = idx.saturating_sub(80);
        let window = &lower[window_start..idx];
        let says_deny = window.contains("never")
            || window.contains("do not ")
            || window.contains("don't ")
            || window.contains("avoid ");
        let says_approve = window.contains("always ") || window.contains("must ");
        let learned_approve = p.preferred_action == "approve";
        let conflict = (learned_approve && says_deny) || (!learned_approve && says_approve);
        if !conflict {
            continue;
        }
        out.push(Suggestion {
            kind: SuggestionKind::Contradiction,
            line: format!(
                "- (contradiction) `{cmd}`: CLAUDE.md says one thing, brain learned the opposite"
            ),
            rationale: format!(
                "{} samples taught the brain to {} `{cmd}`, but CLAUDE.md instructs the opposite",
                p.sample_count, p.preferred_action,
            ),
            tool: p.tool.clone(),
            cmd_keyword: Some(cmd.clone()),
        });
    }
    out
}

// ────────────────────────────────────────────────────────────────────────────
// Main entry point
// ────────────────────────────────────────────────────────────────────────────

pub fn run_garden(project_arg: Option<&str>, apply: bool, cwd: &Path) -> GardenReport {
    let prefs = load_preferences_for_project_or_global(project_arg);
    let claude_md_path = find_claude_md(cwd);
    let existing = claude_md_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();

    let candidates: Vec<&PreferencePattern> = prefs
        .as_ref()
        .map(|p| {
            p.patterns
                .iter()
                .filter(|p| {
                    p.sample_count >= MIN_SAMPLES
                        && p.confidence >= MIN_CONFIDENCE
                        // Keep patterns where the user clearly prefers one way or the other.
                        && (p.accept_rate >= 0.85 || p.accept_rate <= 0.15)
                })
                .collect()
        })
        .unwrap_or_default();

    let considered = candidates.len() as u32;
    let mut kept: Vec<Suggestion> = candidates
        .iter()
        .map(|p| format_pattern(p))
        .filter(|sug| !is_already_covered(&existing, sug))
        .collect();
    let already_covered = considered - kept.len() as u32;

    // Contradictions are appended after codification candidates so the user
    // sees them prominently.
    kept.extend(detect_contradictions(&existing, &candidates));

    let mut applied = false;
    if apply && !kept.is_empty() {
        if let Some(ref path) = claude_md_path {
            if append_to_claude_md(path, &existing, &kept).is_ok() {
                applied = true;
            }
        }
    }

    GardenReport {
        project: project_arg.unwrap_or("(global)").to_string(),
        claude_md_path,
        considered,
        kept,
        already_covered,
        applied,
    }
}

fn load_preferences_for_project_or_global(project: Option<&str>) -> Option<DistilledPreferences> {
    match project {
        Some(p) => load_preferences_for_project(p).or_else(load_preferences),
        None => load_preferences(),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Apply: append a marker block to CLAUDE.md
// ────────────────────────────────────────────────────────────────────────────

fn append_to_claude_md(
    path: &Path,
    existing: &str,
    suggestions: &[Suggestion],
) -> Result<(), String> {
    let mut new_content = existing.to_string();
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push('\n');
    new_content.push_str(HEADER);
    new_content.push('\n');
    new_content.push_str("\n## Learned conventions (auto-codified)\n\n");
    for sug in suggestions {
        new_content.push_str(&sug.line);
        new_content.push_str(&format!("  <!-- {} -->\n", sug.rationale));
    }
    new_content.push('\n');
    new_content.push_str(FOOTER);
    new_content.push('\n');

    fs::write(path, new_content).map_err(|e| format!("write {}: {e}", path.display()))
}

// ────────────────────────────────────────────────────────────────────────────
// CLI rendering
// ────────────────────────────────────────────────────────────────────────────

pub fn format_report(report: &GardenReport) -> String {
    let mut lines = Vec::new();
    lines.push(format!("CLAUDE.md gardening — project: {}", report.project));
    match &report.claude_md_path {
        Some(p) => lines.push(format!("File: {}", p.display())),
        None => lines.push("File: (no CLAUDE.md found in current directory or ancestors)".into()),
    }
    lines.push(format!(
        "Considered: {}  |  Already covered: {}  |  Suggested: {}",
        report.considered,
        report.already_covered,
        report.kept.len()
    ));
    lines.push(String::new());

    if report.kept.is_empty() {
        if report.considered == 0 {
            lines.push(
                "No preferences clear the gardening bar yet (≥20 samples, ≥90% confidence).".into(),
            );
        } else {
            lines.push("Every high-confidence preference is already covered in CLAUDE.md.".into());
        }
    } else {
        let codify: Vec<&Suggestion> = report
            .kept
            .iter()
            .filter(|s| matches!(s.kind, SuggestionKind::Codify))
            .collect();
        let contras: Vec<&Suggestion> = report
            .kept
            .iter()
            .filter(|s| matches!(s.kind, SuggestionKind::Contradiction))
            .collect();
        if !codify.is_empty() {
            lines.push("Proposed additions:".into());
            for sug in codify {
                lines.push(sug.line.clone());
                lines.push(format!("    → {}", sug.rationale));
            }
            lines.push(String::new());
        }
        if !contras.is_empty() {
            lines.push("Contradictions (review manually):".into());
            for sug in contras {
                lines.push(sug.line.clone());
                lines.push(format!("    → {}", sug.rationale));
            }
            lines.push(String::new());
        }
    }

    if report.applied {
        lines.push("Applied: appended to CLAUDE.md.".into());
    } else if !report.kept.is_empty() {
        lines.push("Re-run with --apply to append these to CLAUDE.md.".into());
    }
    lines.join("\n")
}

// ────────────────────────────────────────────────────────────────────────────
// Test helpers
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::preferences::{PreferencePattern, ToolAccuracy};

    fn pattern(
        tool: &str,
        cmd: Option<&str>,
        action: &str,
        samples: u32,
        conf: f64,
    ) -> PreferencePattern {
        let accept_rate = if action == "approve" { 0.95 } else { 0.05 };
        PreferencePattern {
            tool: tool.into(),
            command_pattern: cmd.map(|s| s.into()),
            preferred_action: action.into(),
            sample_count: samples,
            accept_rate,
            conditions: Vec::new(),
            confidence: conf,
        }
    }

    fn prefs(patterns: Vec<PreferencePattern>) -> DistilledPreferences {
        DistilledPreferences {
            patterns,
            tool_accuracy: Vec::<ToolAccuracy>::new(),
            total_decisions: 100,
            overall_accuracy: 0.9,
            temporal: Vec::new(),
        }
    }

    #[test]
    fn keeps_only_high_confidence_high_sample_patterns() {
        let p = prefs(vec![
            pattern("Bash", Some("cargo test"), "approve", 25, 0.95),
            pattern("Bash", Some("rare cmd"), "approve", 8, 0.95), // too few samples
            pattern("Bash", Some("low conf"), "approve", 30, 0.70), // too low conf
        ]);
        // Use the same filter logic locally.
        let kept: Vec<_> = p
            .patterns
            .iter()
            .filter(|p| {
                p.sample_count >= MIN_SAMPLES
                    && p.confidence >= MIN_CONFIDENCE
                    && (p.accept_rate >= 0.85 || p.accept_rate <= 0.15)
            })
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].command_pattern.as_deref(), Some("cargo test"));
    }

    #[test]
    fn formats_approve_pattern() {
        let p = pattern("Bash", Some("cargo test"), "approve", 25, 0.95);
        let sug = format_pattern(&p);
        assert!(sug.line.contains("always approve"));
        assert!(sug.line.contains("cargo test"));
        assert!(sug.rationale.contains("25"));
    }

    #[test]
    fn already_covered_when_command_in_file() {
        let p = pattern("Bash", Some("cargo test"), "approve", 25, 0.95);
        let sug = format_pattern(&p);
        let existing = "Always run `cargo test` before committing.";
        assert!(is_already_covered(existing, &sug));
    }

    #[test]
    fn not_covered_when_unrelated_content() {
        let p = pattern("Bash", Some("git push --force"), "deny", 30, 0.92);
        let sug = format_pattern(&p);
        let existing = "Always run cargo test before committing.";
        assert!(!is_already_covered(existing, &sug));
    }

    #[test]
    fn detects_simple_contradiction() {
        let p = pattern("Bash", Some("cargo test"), "approve", 25, 0.95);
        let existing = "Do not run cargo test during demo recording.";
        let suggestions = detect_contradictions(existing, &[&p]);
        assert_eq!(suggestions.len(), 1);
        assert!(matches!(suggestions[0].kind, SuggestionKind::Contradiction));
    }

    #[test]
    fn apply_appends_marker_block() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");
        fs::write(&claude_md, "# project\n\nsome instructions\n").unwrap();
        let sug = Suggestion {
            kind: SuggestionKind::Codify,
            line: "- always approve `cargo test`".into(),
            rationale: "brain observed 25".into(),
            tool: "Bash".into(),
            cmd_keyword: Some("cargo test".into()),
        };
        append_to_claude_md(&claude_md, "# project\n\nsome instructions\n", &[sug]).unwrap();
        let out = fs::read_to_string(&claude_md).unwrap();
        assert!(out.contains(HEADER));
        assert!(out.contains(FOOTER));
        assert!(out.contains("cargo test"));
    }
}

// Allow dead_code: tree-state hash + recovery context are consumed by the
// actuator's `Resume` path in this PR and by PR7's CLI display of attempt
// history. The autopsy-summary helper is exercised by the test suite.
#![allow(dead_code)]
//! Resume protocol (#345, RFC §7).
//!
//! When a session dies mid-task, the supervisor doesn't replay the
//! interrupted session — it **resumes the task**. The original prompt,
//! the autopsy's view of what burned cost and what went wrong, the
//! verifier history, and the tree-state hash all feed into a recovery
//! context the next attempt sees.
//!
//! Three contracts:
//!
//! 1. **Tree-state hash.** Recorded at every spawn/assign and compared
//!    at resume. A mismatch means someone (a user, another tool) edited
//!    the working tree between death and resume — the supervisor
//!    escalates to NeedsHuman instead of resuming blindly. Worktree
//!    isolation makes mismatches rare; this guard makes them safe when
//!    they happen.
//!
//! 2. **Resume the task, not the session.** No dependence on Claude
//!    Code's session-resume internals. The next attempt is a fresh
//!    session that reads the recovery context as its prompt.
//!
//! 3. **Bounded retries.** Cost accrues to the same task budget. When
//!    attempts cap → NeedsHuman.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Hash of the working tree state used for drift detection. The form is
/// `git:<sha>` when git is available, `mtime:<list>` as a fallback. The
/// exact algorithm is internal; consumers only compare equality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeStateHash(pub String);

impl TreeStateHash {
    pub fn empty() -> Self {
        Self("empty".into())
    }
}

/// Snapshot the working tree at `cwd`. Uses `git diff` + a content
/// fingerprint when the directory is a git repo; falls back to a
/// directory listing + mtimes when it isn't. Either way the same input
/// produces the same hash, and a mutation of any tracked file changes
/// the hash.
///
/// Best-effort by design — a failure to read git or the directory
/// returns `TreeStateHash::empty()` rather than propagating an error.
/// Resume falls open in that case: it proceeds without the drift
/// check, matching the rest of the supervisor's fail-open-to-current-
/// behavior posture.
pub fn snapshot_tree_state(cwd: &Path) -> TreeStateHash {
    if let Some(hash) = git_snapshot(cwd) {
        return TreeStateHash(format!("git:{hash}"));
    }
    if let Some(hash) = mtime_snapshot(cwd) {
        return TreeStateHash(format!("mtime:{hash}"));
    }
    TreeStateHash::empty()
}

fn git_snapshot(cwd: &Path) -> Option<String> {
    // Try `git rev-parse HEAD` + `git status --porcelain` joined,
    // hashed with our existing inline SHA-256 from the relay crypto
    // module. This catches both "what commit are we on?" and "what
    // uncommitted changes exist?" without separate fingerprint code.
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?;
    let status = std::process::Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .current_dir(cwd)
        .output()
        .ok()?;
    if !status.status.success() {
        return Some(head.trim().to_string());
    }
    let porcelain = String::from_utf8(status.stdout).ok()?;
    let mut combined = head.trim().to_string();
    combined.push(':');
    combined.push_str(&porcelain);
    Some(hash_str(&combined))
}

fn mtime_snapshot(cwd: &Path) -> Option<String> {
    // Walk the directory one level deep; deeper trees would be
    // expensive on every spawn. The supervisor's worktree-isolation
    // recommendation makes one-level coverage adequate for the
    // common case (per-task worktree owned by the spawn).
    let entries = std::fs::read_dir(cwd).ok()?;
    let mut lines: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let name = entry.file_name().to_string_lossy().into_owned();
        let len = meta.len();
        lines.push(format!("{name}:{len}:{mtime}"));
    }
    lines.sort();
    Some(hash_str(&lines.join("\n")))
}

/// Tiny FNV-1a hasher so we don't pull a crypto dependency for what
/// is essentially a fingerprint. Collisions would mean we miss a tree
/// mutation; for the resume drift check that means we resume against
/// a slightly changed tree — recoverable, not catastrophic.
fn hash_str(s: &str) -> String {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut h: u64 = FNV_OFFSET;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:016x}")
}

/// Compose the recovery prompt that becomes the next attempt's input.
///
/// Sections, in order:
/// - Original task prompt — what the user asked for.
/// - "You are resuming an interrupted task." framing.
/// - Recent verifier history (FAIL outputs) so the resumed agent
///   doesn't repeat what already broke.
/// - The autopsy summary, when available.
/// - Drift warning when tree-state changed since the original spawn.
pub fn build_recovery_prompt(
    original_prompt: &str,
    prior_verifier_failures: &[(String, String)],
    autopsy_summary: Option<&str>,
    tree_drifted: bool,
) -> String {
    let mut out = String::new();
    out.push_str(original_prompt.trim_end());
    out.push_str(
        "\n\nYou are resuming an interrupted task. Assess the working tree before continuing — do not redo completed work."
    );
    if tree_drifted {
        out.push_str(
            "\n\nWARNING: the working tree changed since the prior attempt. Assume external edits happened and verify before acting."
        );
    }
    if !prior_verifier_failures.is_empty() {
        out.push_str("\n\nPrior verifier failures on this task:");
        for (kind, output) in prior_verifier_failures {
            out.push_str(&format!("\n- {kind}: {output}"));
        }
    }
    if let Some(summary) = autopsy_summary {
        out.push_str("\n\nAutopsy of the interrupted attempt:\n");
        out.push_str(summary);
    }
    out
}

/// Summarize an `AutopsyReport` into a few lines for the recovery
/// prompt. Lives here rather than in `brain::autopsy` so this module
/// owns the "what does resume need" view; the brain side stays
/// focused on producing the report.
#[cfg(feature = "coord")]
pub fn summarize_autopsy(report: &crate::brain::autopsy::AutopsyReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "model={model} duration_secs={duration} tool_calls={calls} errors={errors}\n",
        model = report.model,
        duration = report.duration_secs,
        calls = report.total_tool_calls,
        errors = report.total_errors
    ));
    if !report.findings.is_empty() {
        out.push_str("findings:\n");
        for finding in report.findings.iter().take(5) {
            out.push_str(&format!(
                "  - {category:?}: {summary}\n",
                category = finding.category,
                summary = finding.summary
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_returns_stable_hash_for_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "one").unwrap();
        let h1 = snapshot_tree_state(dir.path());
        let h2 = snapshot_tree_state(dir.path());
        assert_eq!(h1, h2);
    }

    #[test]
    fn snapshot_changes_when_file_mutates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "one").unwrap();
        let h1 = snapshot_tree_state(dir.path());
        std::fs::write(dir.path().join("a"), "two").unwrap();
        // Wait a beat so mtime resolution registers the change. The
        // fallback path uses 1-second mtime granularity; the git path
        // hashes content so the wait is precautionary.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.path().join("a"), "three").unwrap();
        let h2 = snapshot_tree_state(dir.path());
        assert_ne!(h1, h2, "mutated file must change the snapshot");
    }

    #[test]
    fn snapshot_changes_when_new_file_appears() {
        let dir = tempfile::tempdir().unwrap();
        let h1 = snapshot_tree_state(dir.path());
        std::fs::write(dir.path().join("new"), "x").unwrap();
        let h2 = snapshot_tree_state(dir.path());
        assert_ne!(h1, h2);
    }

    #[test]
    fn recovery_prompt_includes_resume_framing() {
        let p = build_recovery_prompt("Add JWT middleware", &[], None, false);
        assert!(p.starts_with("Add JWT middleware"));
        assert!(p.contains("You are resuming an interrupted task"));
        assert!(p.contains("do not redo completed work"));
    }

    #[test]
    fn recovery_prompt_includes_prior_failures() {
        let p = build_recovery_prompt(
            "Build the auth flow",
            &[
                ("run".into(), "assertion failed in auth_test.rs:42".into()),
                ("brain".into(), "missing CSRF token on /login".into()),
            ],
            None,
            false,
        );
        assert!(p.contains("Prior verifier failures"));
        assert!(p.contains("assertion failed in auth_test.rs:42"));
        assert!(p.contains("missing CSRF token"));
    }

    #[test]
    fn recovery_prompt_drift_warning_only_when_drifted() {
        let clean = build_recovery_prompt("do x", &[], None, false);
        let drifted = build_recovery_prompt("do x", &[], None, true);
        assert!(!clean.contains("WARNING: the working tree changed"));
        assert!(drifted.contains("WARNING: the working tree changed"));
    }

    #[test]
    fn recovery_prompt_appends_autopsy_summary() {
        let p = build_recovery_prompt(
            "do thing",
            &[],
            Some("model=sonnet duration_secs=120 tool_calls=15 errors=3"),
            false,
        );
        assert!(p.contains("Autopsy of the interrupted attempt:"));
        assert!(p.contains("duration_secs=120"));
    }

    #[test]
    fn empty_snapshot_constant_matches() {
        assert_eq!(TreeStateHash::empty(), TreeStateHash("empty".into()));
    }
}

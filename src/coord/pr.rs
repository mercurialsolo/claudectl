//! PR-native integration (#369): link a supervisor task to its branch's PR and
//! post a summary comment.
//!
//! Best-effort by construction — every `git`/`gh` call returns `Option`/`Result`
//! and a missing tool, no remote, detached HEAD, or no open PR degrades to a
//! clear skip message rather than failing the task. This mirrors the plugin
//! hooks' "never block on integration" convention.
//!
//! Increment 1 (this file): `supervisor pr <task_id>` composes and posts a task
//! summary comment. Verifier-as-check (a PR status check from the Run/Brain/Agent
//! verdict) and auto-posting on task DONE are increment 2.

use std::path::Path;
use std::process::Command;

use super::tasks::{TaskRow, TaskState};

/// Current git branch at `cwd`, or `None` outside a repo or on detached HEAD.
pub fn current_branch(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// PR number for `branch` via `gh`, or `None` when gh is absent/unauthenticated
/// or the branch has no open PR.
pub fn pr_number_for_branch(cwd: &Path, branch: &str) -> Option<u64> {
    let out = Command::new("gh")
        .current_dir(cwd)
        .args(["pr", "view", branch, "--json", "number", "-q", ".number"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_pr_number(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the `gh pr view -q .number` output (a bare integer line).
pub fn parse_pr_number(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

/// Compose the PR comment body for a task. Pure so the formatting is testable
/// without git/gh.
pub fn build_pr_comment(task: &TaskRow, attempts: u32) -> String {
    let icon = match task.state {
        TaskState::Done => "✅",
        TaskState::NeedsHuman | TaskState::Cancelled => "⚠️",
        _ => "⏳",
    };
    let mut s = String::new();
    s.push_str(&format!(
        "### claudectl supervisor — {icon} {}\n\n",
        task.name
    ));
    s.push_str(&format!("- **State:** `{}`\n", task.state.as_str()));
    s.push_str(&format!(
        "- **Attempts:** {}/{}\n",
        attempts, task.max_retries
    ));
    if let Some(role) = &task.role {
        s.push_str(&format!("- **Role:** {role}\n"));
    }
    s.push_str(&format!("- **Task id:** `{}`\n", task.id));
    s.push_str("\n<sub>Posted by `claudectl supervisor pr`.</sub>\n");
    s
}

/// Post a comment to PR `pr` at `cwd` via `gh`. Best-effort: a missing `gh` or a
/// failed call returns `Err` with the reason for the caller to surface as a skip.
pub fn post_comment(cwd: &Path, pr: u64, body: &str) -> Result<(), String> {
    let out = Command::new("gh")
        .current_dir(cwd)
        .args(["pr", "comment", &pr.to_string(), "--body", body])
        .output()
        .map_err(|e| format!("gh not available: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh pr comment failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Orchestrate the task → branch → PR → comment flow. Returns a human status
/// string on success, or an `Err` describing why it was skipped (no PR, no gh,
/// not a repo). The CLI treats both as non-fatal.
pub fn post_task_summary(task_id: &str) -> Result<String, String> {
    let conn = super::store::open()?;
    let task = super::tasks::get_task(&conn, task_id)?
        .ok_or_else(|| format!("no such task: {task_id}"))?;
    let cwd = Path::new(&task.cwd);
    let branch = current_branch(cwd)
        .ok_or_else(|| format!("{}: not a git repo or detached HEAD", task.cwd))?;
    let pr = pr_number_for_branch(cwd, &branch)
        .ok_or_else(|| format!("no open PR for branch `{branch}` (or gh unavailable)"))?;
    let attempts = super::tasks::attempt_count(&conn, task_id).unwrap_or(0);
    let body = build_pr_comment(&task, attempts);
    post_comment(cwd, pr, &body)?;
    Ok(format!("posted summary to PR #{pr} (branch {branch})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(state: TaskState) -> TaskRow {
        TaskRow {
            id: "t-123".into(),
            name: "ship the thing".into(),
            state,
            role: Some("backend".into()),
            cwd: "/tmp/proj".into(),
            prompt: "do it".into(),
            model: None,
            budget_usd: None,
            max_retries: 3,
            timeout_min: 30,
            depends_on: vec![],
            policy: None,
            verifiers: vec![],
            created_at: "2026-06-28T10:00:00Z".into(),
            updated_at: "2026-06-28T10:05:00Z".into(),
        }
    }

    #[test]
    fn parse_pr_number_handles_bare_int_and_junk() {
        assert_eq!(parse_pr_number("123\n"), Some(123));
        assert_eq!(parse_pr_number("  42 "), Some(42));
        assert_eq!(parse_pr_number(""), None);
        assert_eq!(parse_pr_number("not-a-number"), None);
    }

    #[test]
    fn comment_includes_task_facts() {
        let body = build_pr_comment(&task(TaskState::Running), 1);
        assert!(body.contains("ship the thing"));
        assert!(body.contains("`RUNNING`"));
        assert!(body.contains("1/3"));
        assert!(body.contains("backend"));
        assert!(body.contains("t-123"));
    }

    #[test]
    fn comment_icon_reflects_outcome() {
        assert!(build_pr_comment(&task(TaskState::Done), 1).contains("✅"));
        assert!(build_pr_comment(&task(TaskState::NeedsHuman), 3).contains("⚠️"));
        assert!(build_pr_comment(&task(TaskState::Running), 0).contains("⏳"));
    }
}

// Allow dead_code: the supervisor calls `write`/`delete` from the actuator
// in PR4; the brain-gate hook calls `read` once the bash script is updated
// (separate follow-up). All paths are exercised by the test suite in this
// file.
#![allow(dead_code)]
//! Per-session policy file contract (#345, RFC v2 §8).
//!
//! When a task enters `ASSIGNED` / `RUNNING`, the supervisor evaluates
//! `force_manual_tasks = ["infra-*"]` against the task name **once** and
//! writes the effective approval mode to a per-session file at
//! `~/.claudectl/coord/session-policy/<session_id>.json`. The brain-gate
//! hook, on every tool call, does a single `fs::read_to_string` and
//! short-circuits to manual approval when the file says `force_manual`.
//!
//! Three contracts the rest of the system can rely on:
//!
//! 1. **Atomic write.** Files are produced via `tempfile in same dir +
//!    rename`. A crashed write never leaves a half-formed file the hook
//!    might mis-parse — the hook either sees the previous version or the
//!    new one, never a partial.
//! 2. **Tighten-only.** The file's only valid effect is *more* manual
//!    approval. A missing, unreadable, or malformed file degrades to
//!    `inherit` — meaning brain/rules behave exactly as they do today.
//!    Fail-open to `inherit` is what keeps the manual-upgrade gap from
//!    breaking already-running sessions.
//! 3. **Lifetime-bound.** Written on `ASSIGNED` / `RUNNING`; deleted on
//!    any terminal state (DONE / NEEDS_HUMAN / CANCELLED). A dangling
//!    file outliving its task is a doctor-row Advisory, not an error.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What the brain-gate hook reads. Stored as the only field of a struct
/// so future per-task overrides (timeout overrides, model overrides, etc.)
/// land additively without breaking the on-disk format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPolicy {
    pub task_id: String,
    pub approve_mode: ApproveMode,
    pub written_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApproveMode {
    /// Defer to whatever brain/rules say. Equivalent to the file being
    /// absent — exists so policies can be made explicit when desired.
    Inherit,
    /// Override brain/rules to force manual approval. The only valid
    /// tightening direction (RFC v2 §8 contract).
    ForceManual,
}

/// `~/.claudectl/coord/session-policy/`. Created on demand by `write()`.
pub fn dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("coord")
        .join("session-policy")
}

fn file_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.json"))
}

/// Write the per-session policy atomically. Uses a sibling tempfile +
/// rename so a partial write can never be read by the hook.
pub fn write(session_id: &str, policy: &SessionPolicy) -> io::Result<()> {
    write_at(&dir(), session_id, policy)
}

pub fn write_at(target_dir: &Path, session_id: &str, policy: &SessionPolicy) -> io::Result<()> {
    fs::create_dir_all(target_dir)?;
    let final_path = file_path(target_dir, session_id);
    let tmp_path = target_dir.join(format!(".{session_id}.json.tmp"));
    let body = serde_json::to_vec_pretty(policy).map_err(io::Error::other)?;
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&body)?;
        f.sync_data()?;
    }
    fs::rename(&tmp_path, &final_path)
}

/// Read the per-session policy. Returns `Ok(None)` for any failure mode
/// — missing file, unreadable, malformed JSON — so the brain-gate hook
/// falls open to `Inherit` instead of failing closed.
pub fn read(session_id: &str) -> Option<SessionPolicy> {
    read_at(&dir(), session_id)
}

pub fn read_at(target_dir: &Path, session_id: &str) -> Option<SessionPolicy> {
    let path = file_path(target_dir, session_id);
    let body = fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Delete the per-session policy. Missing-file is not an error — terminal
/// transitions sometimes happen before any policy was written.
pub fn delete(session_id: &str) -> io::Result<()> {
    delete_at(&dir(), session_id)
}

pub fn delete_at(target_dir: &Path, session_id: &str) -> io::Result<()> {
    match fs::remove_file(file_path(target_dir, session_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample(task_id: &str) -> SessionPolicy {
        SessionPolicy {
            task_id: task_id.into(),
            approve_mode: ApproveMode::ForceManual,
            written_at: "2026-06-09T12:00:00Z".into(),
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let p = sample("task_a");
        write_at(dir.path(), "sess_a", &p).unwrap();
        let got = read_at(dir.path(), "sess_a").expect("policy missing");
        assert_eq!(got, p);
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = tempdir().unwrap();
        assert!(read_at(dir.path(), "nope").is_none());
    }

    #[test]
    fn read_malformed_returns_none_fail_open() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(file_path(dir.path(), "sess_bad"), b"{not json").unwrap();
        // Fail-open to None — brain-gate hook should fall back to Inherit.
        assert!(read_at(dir.path(), "sess_bad").is_none());
    }

    #[test]
    fn write_is_atomic_via_tempfile_rename() {
        let dir = tempdir().unwrap();
        let p = sample("task_a");
        write_at(dir.path(), "sess_a", &p).unwrap();
        // No leftover tempfile after a successful write.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "only the final file should remain");
        let names: Vec<_> = entries
            .iter()
            .map(|n| n.to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["sess_a.json"]);
    }

    #[test]
    fn delete_missing_is_not_an_error() {
        let dir = tempdir().unwrap();
        delete_at(dir.path(), "never_existed").unwrap();
    }

    #[test]
    fn delete_removes_the_file() {
        let dir = tempdir().unwrap();
        write_at(dir.path(), "sess_a", &sample("task_a")).unwrap();
        delete_at(dir.path(), "sess_a").unwrap();
        assert!(read_at(dir.path(), "sess_a").is_none());
    }
}

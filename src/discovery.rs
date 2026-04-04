use std::fs;
use std::path::PathBuf;

use crate::session::{ClaudeSession, RawSession};

fn sessions_dir() -> PathBuf {
    dirs_home().join(".claude").join("sessions")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn projects_dir() -> PathBuf {
    dirs_home().join(".claude").join("projects")
}

pub fn scan_sessions() -> Vec<ClaudeSession> {
    let dir = sessions_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut sessions = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let raw: RawSession = match serde_json::from_str(&content) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let mut session = ClaudeSession::from_raw(raw);

        // Resolve JSONL path — find the most recently modified .jsonl in the project dir.
        // For --resume sessions, the JSONL is under the resumed session's UUID,
        // not the current session's UUID.
        let slug = cwd_to_slug(&session.cwd);
        let project_dir = projects_dir().join(&slug);
        session.jsonl_path = find_latest_jsonl(&project_dir);

        sessions.push(session);
    }

    sessions
}

/// Find the most recently modified .jsonl file in a project directory.
fn find_latest_jsonl(dir: &PathBuf) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry.metadata().ok()?.modified().ok()?;
        if best.as_ref().is_none_or(|(_, t)| modified > *t) {
            best = Some((path, modified));
        }
    }

    best.map(|(p, _)| p)
}

/// Convert a cwd like `/Users/barada/Sandbox/Mason/foo` to the slug
/// format Claude uses: `-Users-barada-Sandbox-Mason-foo`
fn cwd_to_slug(cwd: &str) -> String {
    cwd.replace('/', "-")
}

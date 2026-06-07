//! Role addressing & resolution. See spec §5.
//!
//! A role is a persistent address ("planner", "impl-frontend"). Sessions are
//! ephemeral; roles outlive process death and re-bind on restart.
//!
//! Resolution order, given a caller's current working directory:
//!
//! 1. **Explicit** — the caller set `CLAUDECTL_BUS_ROLE` at session launch.
//! 2. **cwd-inferred** — a single registered role's `cwd_selector` is a
//!    prefix of (or equal to) the caller's cwd.
//! 3. **Ambiguous** — multiple roles match; surface an `Ambiguous` resolution
//!    so the caller is asked to pass `--role` explicitly.
//! 4. **Unbound** — no match. Callers may auto-register from the cwd basename.

use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::store::{self, RoleRow};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub cwd_selector: String,
    pub last_session_id: Option<String>,
    pub last_seen: String,
    pub subscriptions: Vec<String>,
    /// PID this role is bound to, if any (#307).
    pub pid: Option<u32>,
}

impl From<RoleRow> for Role {
    fn from(r: RoleRow) -> Self {
        Self {
            name: r.role,
            cwd_selector: r.cwd_selector,
            last_session_id: r.last_session_id,
            last_seen: r.last_seen,
            subscriptions: r.subscriptions,
            pid: r.pid,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoleResolution {
    Resolved(Role),
    Ambiguous { candidates: Vec<String> },
    Unbound { cwd: String },
}

pub const ROLE_ENV: &str = "CLAUDECTL_BUS_ROLE";

/// Resolve the caller's role. Order:
/// 1. `explicit` (CLI `--role`)
/// 2. `CLAUDECTL_BUS_ROLE` env
/// 3. **PID-binding** (#307) — any role bound to a pid in the caller's
///    ancestor chain (starting at the caller's parent — for the bus stdio
///    server, that's the Claude Code process).
/// 4. cwd-inference (literal-prefix match against bound `cwd_selector`s)
pub fn resolve(
    conn: &Connection,
    explicit: Option<&str>,
    cwd: &Path,
) -> Result<RoleResolution, String> {
    if let Some(name) = explicit {
        return resolve_by_name(conn, name, cwd);
    }
    if let Ok(name) = std::env::var(ROLE_ENV) {
        if !name.trim().is_empty() {
            return resolve_by_name(conn, name.trim(), cwd);
        }
    }
    if let Some(role) = resolve_by_pid_chain(conn, &ancestor_pids())? {
        return Ok(RoleResolution::Resolved(role));
    }
    resolve_by_cwd(conn, cwd)
}

/// Pick the first role bound to any pid in `chain`. Split out from
/// `ancestor_pids()` so tests can drive it with a synthetic chain.
fn resolve_by_pid_chain(conn: &Connection, chain: &[u32]) -> Result<Option<Role>, String> {
    for pid in chain {
        if let Some(row) = store::get_role_by_pid(conn, *pid)? {
            return Ok(Some(row.into()));
        }
    }
    Ok(None)
}

/// Caller's parent pid chain, capped at depth 8 so we don't walk to init.
/// Depth 8 comfortably covers `claude → bash → mcp-stdio-server` plus
/// nested shells, tmux, etc.
fn ancestor_pids() -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = unsafe { libc::getppid() } as u32;
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        out.push(pid);
        pid = match parent_pid_of(pid) {
            Some(p) if p > 1 => p,
            _ => break,
        };
    }
    out
}

/// Walk the caller's ancestor pid chain and return the first one whose
/// `ps`-reported command line contains `claude` (case-insensitive). Used
/// by `bus role bind --self` to attach a role to the right session
/// without making the operator look up Claude's pid (#310).
pub fn find_claude_ancestor_pid() -> Option<u32> {
    ancestor_pids()
        .into_iter()
        .find(|pid| process_command_for(*pid).is_some_and(|c| c.to_lowercase().contains("claude")))
}

/// Fetch `ps -o command=` for a pid. Returns `None` when the pid is gone.
fn process_command_for(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

/// Look up `ppid` for `pid` via `ps`. Returns `None` when the pid is gone
/// or `ps` fails. Native `ps` keeps us off the sysinfo crate.
fn parent_pid_of(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn resolve_by_name(conn: &Connection, name: &str, _cwd: &Path) -> Result<RoleResolution, String> {
    match store::get_role(conn, name)? {
        Some(r) => Ok(RoleResolution::Resolved(r.into())),
        None => Ok(RoleResolution::Unbound {
            cwd: name.to_string(),
        }),
    }
}

fn resolve_by_cwd(conn: &Connection, cwd: &Path) -> Result<RoleResolution, String> {
    let all = store::list_roles(conn)?;
    let cwd_canon = canonicalize_for_match(cwd);
    let cwd_str = cwd_canon.to_string_lossy();
    let mut matches: Vec<Role> = all
        .into_iter()
        .filter(|r| {
            let sel_canon = canonicalize_for_match(Path::new(r.cwd_selector.trim_end_matches('*')));
            selector_matches(&sel_canon.to_string_lossy(), &cwd_str)
        })
        .map(Role::from)
        .collect();
    match matches.len() {
        0 => Ok(RoleResolution::Unbound {
            cwd: cwd_str.into_owned(),
        }),
        1 => Ok(RoleResolution::Resolved(matches.remove(0))),
        _ => Ok(RoleResolution::Ambiguous {
            candidates: matches.into_iter().map(|r| r.name).collect(),
        }),
    }
}

/// Phase-1 selectors are simple: literal path prefixes. A trailing wildcard
/// (`/work/proj-*`) is treated as a prefix match against the stem before `*`.
/// Glob support proper is a phase-3 setup-wizard concern.
fn selector_matches(selector: &str, cwd: &str) -> bool {
    let stem = selector.trim_end_matches('*').trim_end_matches('/');
    if stem.is_empty() {
        return false;
    }
    cwd == stem || cwd.starts_with(&format!("{stem}/"))
}

/// Resolve a path to its real on-disk form when possible. macOS in particular
/// canonicalizes symlinked roots like `/tmp` → `/private/tmp`, which would
/// otherwise make cwd inference miss roles bound with the un-canonicalized
/// form. Falls back to the input verbatim when the path doesn't exist yet.
fn canonicalize_for_match(p: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::store::{open_memory, upsert_role};
    use std::path::PathBuf;

    #[test]
    fn cwd_inference_picks_single_match() {
        let conn = open_memory();
        upsert_role(&conn, "planner", "/work/proj-plan", None, None).unwrap();
        upsert_role(&conn, "impl", "/work/proj-impl", None, None).unwrap();
        let r = resolve(&conn, None, &PathBuf::from("/work/proj-impl/src")).unwrap();
        match r {
            RoleResolution::Resolved(role) => assert_eq!(role.name, "impl"),
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn shared_cwd_is_ambiguous() {
        let conn = open_memory();
        upsert_role(&conn, "planner", "/shared/repo", None, None).unwrap();
        upsert_role(&conn, "impl", "/shared/repo", None, None).unwrap();
        let r = resolve(&conn, None, &PathBuf::from("/shared/repo")).unwrap();
        match r {
            RoleResolution::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn explicit_role_overrides_cwd() {
        let conn = open_memory();
        upsert_role(&conn, "planner", "/work/proj", None, None).unwrap();
        upsert_role(&conn, "impl", "/other/place", None, None).unwrap();
        let r = resolve(&conn, Some("impl"), &PathBuf::from("/work/proj")).unwrap();
        assert!(matches!(r, RoleResolution::Resolved(role) if role.name == "impl"));
    }

    #[test]
    fn unbound_cwd_reports_unbound() {
        let conn = open_memory();
        let r = resolve(&conn, None, &PathBuf::from("/nowhere")).unwrap();
        assert!(matches!(r, RoleResolution::Unbound { .. }));
    }

    #[test]
    fn pid_binding_takes_precedence_over_cwd_match() {
        // #307: a role bound to pid 12345 should win over a role whose
        // cwd_selector matches the caller's cwd, when 12345 is in the
        // caller's ancestor chain.
        let conn = open_memory();
        upsert_role(&conn, "cwd-role", "/work/proj", None, None).unwrap();
        upsert_role(&conn, "pid-role", "/work/proj", None, Some(12345)).unwrap();
        let role = resolve_by_pid_chain(&conn, &[99999, 12345, 1])
            .unwrap()
            .expect("expected pid match");
        assert_eq!(role.name, "pid-role");
        assert_eq!(role.pid, Some(12345));
    }

    #[test]
    fn pid_chain_with_no_match_falls_through() {
        // Resolver returns None so the caller falls back to cwd inference.
        let conn = open_memory();
        upsert_role(&conn, "cwd-role", "/work/proj", None, Some(11111)).unwrap();
        assert!(
            resolve_by_pid_chain(&conn, &[22222, 33333])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn upsert_keeps_existing_pid_when_rebinding_without_one() {
        // Re-binding a role for a new session_id shouldn't clobber the pid
        // — otherwise the TUI's resume flow would silently lose the binding.
        let conn = open_memory();
        upsert_role(&conn, "frontend", "/work/proj", None, Some(7777)).unwrap();
        upsert_role(&conn, "frontend", "/work/proj", Some("sess_42"), None).unwrap();
        let row = crate::bus::store::get_role(&conn, "frontend")
            .unwrap()
            .unwrap();
        assert_eq!(row.pid, Some(7777));
        assert_eq!(row.last_session_id.as_deref(), Some("sess_42"));
    }
}

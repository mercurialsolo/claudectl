//! Bind `BusView` to the binary's agent-bus store.
//!
//! Under `--features bus` the impl reads from the SQLite-backed bus DB
//! (`~/.claudectl/bus/bus.db`). Without it, every accessor returns an empty
//! list so the TUI can render the "no bus" state with zero conditional
//! compilation.

use claudectl_core::runtime::{AgentDirectoryEntry, BusView, RoleBinding};

pub struct LiveBusView;

impl BusView for LiveBusView {
    #[cfg(feature = "bus")]
    fn list_agents(&self) -> Vec<AgentDirectoryEntry> {
        use claudectl_core::discovery;

        let Ok(bus_conn) = crate::bus::store::open() else {
            return Vec::new();
        };
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);

        sessions
            .into_iter()
            .map(|s| {
                let role = resolve_role(&bus_conn, &s.cwd);
                AgentDirectoryEntry {
                    session_id: s.session_id,
                    pid: s.pid,
                    cwd: s.cwd,
                    project: s.project_name,
                    status: s.status.to_string(),
                    role,
                }
            })
            .collect()
    }
    #[cfg(not(feature = "bus"))]
    fn list_agents(&self) -> Vec<AgentDirectoryEntry> {
        Vec::new()
    }

    #[cfg(feature = "bus")]
    fn list_roles(&self) -> Vec<RoleBinding> {
        let Ok(conn) = crate::bus::store::open() else {
            return Vec::new();
        };
        crate::bus::store::list_roles(&conn)
            .unwrap_or_default()
            .into_iter()
            .map(|r| RoleBinding {
                name: r.role,
                cwd_selector: r.cwd_selector,
                last_session_id: r.last_session_id,
                last_seen: r.last_seen,
            })
            .collect()
    }
    #[cfg(not(feature = "bus"))]
    fn list_roles(&self) -> Vec<RoleBinding> {
        Vec::new()
    }
}

#[cfg(feature = "bus")]
fn resolve_role(conn: &rusqlite::Connection, cwd: &str) -> Option<String> {
    use crate::bus::roles;
    use std::path::PathBuf;
    match roles::resolve(conn, None, &PathBuf::from(cwd)) {
        Ok(roles::RoleResolution::Resolved(r)) => Some(r.name),
        _ => None,
    }
}

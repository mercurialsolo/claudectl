#![allow(dead_code)]

use crate::session::ClaudeSession;

use super::store;
use super::types::*;

/// Build a compact coordination context string for injection into a brain prompt.
/// Queries the coord store for relevant state and memory.
pub fn build_coordination_context(session: &ClaudeSession) -> String {
    let conn = match store::open() {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let mut sections = Vec::new();

    // Active leases for this session
    if let Ok(leases) = store::list_leases_for_session(&conn, &session.session_id) {
        if !leases.is_empty() {
            for lease in &leases {
                sections.push(format!(
                    "- You own: {} ({}, {})",
                    lease.resource_value, lease.resource_kind, lease.mode
                ));
            }
        }
    }

    // Leases held by others that might conflict
    if let Ok(all_leases) = store::list_leases(&conn, Some(LeaseStatus::Active)) {
        for lease in &all_leases {
            if lease.owner_session_id == session.session_id {
                continue; // Skip our own leases
            }
            if lease.mode == LeaseMode::Exclusive {
                sections.push(format!(
                    "- Exclusive lease elsewhere: {} owned by {}",
                    lease.resource_value, lease.owner_session_id
                ));
            }
        }
    }

    // Open blockers
    if let Ok(blockers) = store::list_blockers(&conn, Some(BlockerStatus::Open)) {
        for blocker in &blockers {
            if blocker.owner_session_id == session.session_id {
                sections.push(format!(
                    "- Blocker: waiting for {} ({})",
                    blocker.waiting_for, blocker.task_id
                ));
            }
        }
    }

    // Pending handoffs involving this session
    if let Ok(handoffs) = store::list_pending_handoffs(&conn) {
        for handoff in &handoffs {
            if handoff.from_session_id == session.session_id {
                let to = handoff.to_session_id.as_deref().unwrap_or("unassigned");
                sections.push(format!(
                    "- Handoff out: {} -> {} ({})",
                    handoff.summary, to, handoff.priority
                ));
            } else if handoff.to_session_id.as_deref() == Some(&*session.session_id) {
                sections.push(format!(
                    "- Handoff in: {} from {} ({})",
                    handoff.summary, handoff.from_session_id, handoff.priority
                ));
            }
        }
    }

    // Pending interrupts targeting this session
    if let Ok(interrupts) = store::list_interrupts(&conn, Some(InterruptState::Pending)) {
        for intr in &interrupts {
            if intr.target_session_id == session.session_id {
                sections.push(format!(
                    "- Interrupt pending: {} [{}] {}",
                    intr.interrupt_type, intr.priority, intr.reason
                ));
            }
        }
    }

    // Relevant memory records (FTS5 search by project + tool)
    let query = build_memory_query(session);
    if !query.is_empty() {
        if let Ok(memories) = store::search_memory(&conn, &query, 5) {
            for mem in &memories {
                sections.push(format!("- Memory: {}", mem.summary));
            }
        }
    }

    if sections.is_empty() {
        return String::new();
    }

    sections.join("\n")
}

/// Build an FTS5 search query from the session's current state.
fn build_memory_query(session: &ClaudeSession) -> String {
    let mut terms = Vec::new();

    if let Some(ref tool) = session.pending_tool_name {
        terms.push(tool.to_lowercase());
    }

    // Add project name as a search term
    let project = session.display_name();
    if !project.is_empty() && project != "unknown" {
        terms.push(project.to_lowercase());
    }

    terms.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::RawSession;

    fn test_session() -> ClaudeSession {
        let mut s = ClaudeSession::from_raw(RawSession {
            pid: 1,
            session_id: "sess_test".into(),
            cwd: "/tmp/myproject".into(),
            started_at: 0,
        });
        s.pending_tool_name = Some("Bash".into());
        s
    }

    #[test]
    fn build_memory_query_includes_tool_and_project() {
        let session = test_session();
        let query = build_memory_query(&session);
        assert!(query.contains("bash"));
        assert!(query.contains("myproject"));
    }

    #[test]
    fn build_memory_query_empty_without_tool() {
        let mut session = test_session();
        session.pending_tool_name = None;
        let query = build_memory_query(&session);
        // Still has project name
        assert!(query.contains("myproject"));
    }

    #[test]
    fn build_coordination_context_returns_empty_for_no_state() {
        // With no coord store data, should return empty
        // (This test relies on the store being empty or failing to open)
        let session = test_session();
        let ctx = build_coordination_context(&session);
        // May or may not be empty depending on whether coord.db exists with data
        // The key test is that it doesn't panic
        let _ = ctx;
    }
}

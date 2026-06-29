//! Bind `CoordView` to the binary's coordination store.
//!
//! Under `--features coord` the impl reads from the SQLite-backed `coord`
//! store. Without it, every accessor returns an empty list so the TUI can
//! render the "no coord" state without conditional compilation.

use claudectl_core::runtime::{
    CoordView, HandoffSummary, InterruptSummary, LeaseSummary, TaskSummary,
};

pub struct LiveCoordView;

impl CoordView for LiveCoordView {
    #[cfg(feature = "coord")]
    fn active_leases(&self) -> Vec<LeaseSummary> {
        use crate::coord::{store, types::LeaseStatus};
        let Ok(conn) = store::open() else {
            return Vec::new();
        };
        store::list_leases(&conn, Some(LeaseStatus::Active))
            .unwrap_or_default()
            .into_iter()
            .map(lease_summary_from)
            .collect()
    }
    #[cfg(not(feature = "coord"))]
    fn active_leases(&self) -> Vec<LeaseSummary> {
        Vec::new()
    }

    #[cfg(feature = "coord")]
    fn pending_handoffs(&self) -> Vec<HandoffSummary> {
        use crate::coord::store;
        let Ok(conn) = store::open() else {
            return Vec::new();
        };
        store::list_pending_handoffs(&conn)
            .unwrap_or_default()
            .into_iter()
            .map(handoff_summary_from)
            .collect()
    }
    #[cfg(not(feature = "coord"))]
    fn pending_handoffs(&self) -> Vec<HandoffSummary> {
        Vec::new()
    }

    #[cfg(feature = "coord")]
    fn pending_interrupts(&self) -> Vec<InterruptSummary> {
        use crate::coord::{store, types::InterruptState};
        let Ok(conn) = store::open() else {
            return Vec::new();
        };
        store::list_interrupts(&conn, Some(InterruptState::Pending))
            .unwrap_or_default()
            .into_iter()
            .map(interrupt_summary_from)
            .collect()
    }
    #[cfg(not(feature = "coord"))]
    fn pending_interrupts(&self) -> Vec<InterruptSummary> {
        Vec::new()
    }

    #[cfg(feature = "coord")]
    fn tasks(&self) -> Vec<TaskSummary> {
        use crate::coord::{store, tasks};
        let Ok(conn) = store::open() else {
            return Vec::new();
        };
        // `list_tasks` is ORDER BY created_at (oldest-first); reverse so the
        // panel shows newest tasks on top. Derive attempt count + latest
        // session per task — both cheap indexed lookups.
        tasks::list_tasks(&conn, None)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .map(|t| {
                let attempts = tasks::attempt_count(&conn, &t.id).unwrap_or(0);
                let last_session_id = tasks::latest_session_id(&conn, &t.id).ok().flatten();
                let last_verdict = tasks::latest_verification(&conn, &t.id)
                    .ok()
                    .flatten()
                    .map(|(_kind, verdict)| verdict);
                let cost_usd = tasks::task_cost_usd(&conn, &t.id).unwrap_or(0.0);
                TaskSummary {
                    id: t.id,
                    name: t.name,
                    state: t.state.as_str().to_string(),
                    role: t.role,
                    attempts,
                    max_retries: t.max_retries,
                    last_session_id,
                    last_verdict,
                    cost_usd,
                    created_at: t.created_at,
                    updated_at: t.updated_at,
                }
            })
            .collect()
    }
    #[cfg(not(feature = "coord"))]
    fn tasks(&self) -> Vec<TaskSummary> {
        Vec::new()
    }
}

#[cfg(feature = "coord")]
fn lease_summary_from(l: crate::coord::types::Lease) -> LeaseSummary {
    LeaseSummary {
        id: l.id,
        owner_session_id: l.owner_session_id,
        resource_kind: l.resource_kind,
        resource_value: l.resource_value,
        mode: l.mode.to_string(),
        acquired_at: l.acquired_at,
        expires_at: l.expires_at,
    }
}

#[cfg(feature = "coord")]
fn handoff_summary_from(h: crate::coord::types::Handoff) -> HandoffSummary {
    HandoffSummary {
        id: h.id,
        from_session_id: h.from_session_id,
        to_session_id: h.to_session_id,
        task_id: h.task_id,
        summary: h.summary,
        priority: h.priority,
        created_at: h.created_at,
    }
}

#[cfg(feature = "coord")]
fn interrupt_summary_from(i: crate::coord::types::Interrupt) -> InterruptSummary {
    InterruptSummary {
        id: i.id,
        interrupt_type: i.interrupt_type.to_string(),
        priority: i.priority,
        target_session_id: i.target_session_id,
        reason: i.reason,
        created_at: i.created_at,
    }
}

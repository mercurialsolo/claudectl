//! UI ↔ runtime contract.
//!
//! Tracking: issue #274 of the workspace-refactor epic (#279).
//!
//! The TUI today reaches deep into brain / coord / bus / rules internals. This
//! module defines the **read-only** boundary it should reach through instead,
//! so a future `claudectl-tui` crate (#275) can be extracted and iterated on
//! without recompiling brain or the bus.
//!
//! ## Why traits, why core-owned DTOs
//!
//! Each view is a trait, not a concrete struct, so:
//!
//! - The binary crate can hand the TUI a real implementation backed by SQLite
//!   / the engine / the bus DB. A future remote frontend can hand it an HTTP
//!   client. Tests hand it a fixture.
//! - Adding a method to a trait is a contract change reviewable in one PR;
//!   adding a method to a concrete struct ripples through every caller.
//!
//! Each DTO (`SessionSnapshot`, `LeaseSummary`, …) is owned by `core` so the
//! traits don't drag `brain::DecisionRecord` / `coord::Lease` upward into the
//! TUI's dependency surface. Conversion happens once, in the wrapper.
//!
//! ## What's in scope here
//!
//! Read-only views only. Side-effecting paths (`terminate_session`,
//! `inject_prompt`, `log_decision`) deserve a separate `Actions` trait once
//! the TUI's write surface is mapped. Adding it speculatively now would
//! violate the "only add methods existing call sites need" rule the epic
//! committed to.
//!
//! ## What's NOT in scope
//!
//! - The TUI doesn't yet call through these traits — that's #275.
//! - Brain's review/scorecard surface — it's heavier and worth its own
//!   trait once #275 stabilizes the basic shape.
//! - Pub/sub claim protocol — #283 builds on top of these traits, not the
//!   other way around.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ============================================================================
// Sessions
// ============================================================================

/// One running Claude Code session, as observed by the runtime. Minimal
/// projection of the binary-crate `ClaudeSession`; only fields the TUI
/// renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub pid: u32,
    pub cwd: String,
    pub project_name: String,
    pub status: String,
    pub cost_usd: f64,
    pub context_tokens: u64,
    pub context_max: u64,
    pub last_message_ts: u64,
}

/// Read access to the live session roster.
pub trait SessionSource: Send + Sync {
    /// Snapshot of every running Claude Code session. Order is the
    /// implementor's choice; the TUI sorts again client-side.
    fn list(&self) -> Vec<SessionSnapshot>;

    /// Fetch a specific session by its ID. `None` when the session has
    /// exited or never existed.
    fn detail(&self, session_id: &str) -> Option<SessionSnapshot> {
        self.list().into_iter().find(|s| s.session_id == session_id)
    }
}

// ============================================================================
// Brain
// ============================================================================

/// Mirrors the binary's `brain::GateMode` without depending on the brain
/// crate. Persisted as the lowercased label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainGateMode {
    On,
    Off,
    Auto,
}

/// A single past brain decision, projected for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionSummary {
    pub id: String,
    pub timestamp: String,
    pub action: String,
    pub confidence: Option<f64>,
    pub project: Option<String>,
    pub tool: Option<String>,
}

/// Read access to the brain's decision history and current mode.
pub trait BrainView: Send + Sync {
    fn gate_mode(&self) -> BrainGateMode;

    /// Most recent `n` decisions, newest first.
    fn recent_decisions(&self, n: usize) -> Vec<DecisionSummary>;

    /// Total count of brain decisions on disk. Drives the "decisions: N"
    /// status line.
    fn decision_count(&self) -> usize;
}

// ============================================================================
// Coordination layer
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseSummary {
    pub id: String,
    pub owner_session_id: String,
    pub resource_kind: String,
    pub resource_value: String,
    pub mode: String,
    pub acquired_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffSummary {
    pub id: String,
    pub from_session_id: String,
    pub to_session_id: Option<String>,
    pub task_id: String,
    pub summary: String,
    pub priority: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptSummary {
    pub id: String,
    pub interrupt_type: String,
    pub priority: String,
    pub target_session_id: String,
    pub reason: String,
    pub created_at: String,
}

/// Read access to coordination state (leases, handoffs, interrupts).
/// Backed today by the `coord` SQLite store; in tests, by a fixture.
pub trait CoordView: Send + Sync {
    fn active_leases(&self) -> Vec<LeaseSummary>;
    fn pending_handoffs(&self) -> Vec<HandoffSummary>;
    fn pending_interrupts(&self) -> Vec<InterruptSummary>;
}

// ============================================================================
// Agent bus
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDirectoryEntry {
    pub session_id: String,
    pub pid: u32,
    pub cwd: String,
    pub project: String,
    pub status: String,
    pub role: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleBinding {
    pub name: String,
    pub cwd_selector: String,
    pub last_session_id: Option<String>,
    pub last_seen: String,
}

/// Read access to the agent-bus roster + role table. Disabled implementations
/// (`bus` feature off in the binary) return empty vectors so the TUI can
/// render the panel as "no bus" without conditional compilation.
pub trait BusView: Send + Sync {
    fn list_agents(&self) -> Vec<AgentDirectoryEntry>;
    fn list_roles(&self) -> Vec<RoleBinding>;
}

// ============================================================================
// Runtime aggregate
// ============================================================================

/// Single struct the binary builds at startup and hands to the TUI.
///
/// All fields are `Arc<dyn ...>` so the TUI doesn't care whether an impl is
/// a thin SQLite wrapper, a remote HTTP client, or an in-memory mock — they
/// all share the same shape.
#[derive(Clone)]
pub struct Runtime {
    pub sessions: Arc<dyn SessionSource>,
    pub brain: Arc<dyn BrainView>,
    pub coord: Arc<dyn CoordView>,
    pub bus: Arc<dyn BusView>,
}

impl Runtime {
    pub fn new(
        sessions: Arc<dyn SessionSource>,
        brain: Arc<dyn BrainView>,
        coord: Arc<dyn CoordView>,
        bus: Arc<dyn BusView>,
    ) -> Self {
        Self {
            sessions,
            brain,
            coord,
            bus,
        }
    }
}

// ============================================================================
// MockRuntime — for tests in this crate and in claudectl-tui
// ============================================================================

/// In-memory runtime backed by `Vec`s. Used by tests in this crate to verify
/// the trait shapes compile and roundtrip cleanly, and by the future
/// `claudectl-tui` crate's tests to render the TUI against fixtures without
/// dragging in brain / coord / bus.
#[derive(Default, Clone)]
pub struct MockRuntime {
    pub sessions: Vec<SessionSnapshot>,
    pub gate_mode: Option<BrainGateMode>,
    pub decisions: Vec<DecisionSummary>,
    pub leases: Vec<LeaseSummary>,
    pub handoffs: Vec<HandoffSummary>,
    pub interrupts: Vec<InterruptSummary>,
    pub agents: Vec<AgentDirectoryEntry>,
    pub roles: Vec<RoleBinding>,
}

impl MockRuntime {
    pub fn into_runtime(self) -> Runtime {
        let arc = Arc::new(self);
        Runtime::new(arc.clone(), arc.clone(), arc.clone(), arc)
    }
}

impl SessionSource for MockRuntime {
    fn list(&self) -> Vec<SessionSnapshot> {
        self.sessions.clone()
    }
}

impl BrainView for MockRuntime {
    fn gate_mode(&self) -> BrainGateMode {
        self.gate_mode.unwrap_or(BrainGateMode::On)
    }
    fn recent_decisions(&self, n: usize) -> Vec<DecisionSummary> {
        self.decisions.iter().take(n).cloned().collect()
    }
    fn decision_count(&self) -> usize {
        self.decisions.len()
    }
}

impl CoordView for MockRuntime {
    fn active_leases(&self) -> Vec<LeaseSummary> {
        self.leases.clone()
    }
    fn pending_handoffs(&self) -> Vec<HandoffSummary> {
        self.handoffs.clone()
    }
    fn pending_interrupts(&self) -> Vec<InterruptSummary> {
        self.interrupts.clone()
    }
}

impl BusView for MockRuntime {
    fn list_agents(&self) -> Vec<AgentDirectoryEntry> {
        self.agents.clone()
    }
    fn list_roles(&self) -> Vec<RoleBinding> {
        self.roles.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session(id: &str) -> SessionSnapshot {
        SessionSnapshot {
            session_id: id.into(),
            pid: 12345,
            cwd: "/work/proj".into(),
            project_name: "proj".into(),
            status: "Processing".into(),
            cost_usd: 1.23,
            context_tokens: 4000,
            context_max: 200_000,
            last_message_ts: 1_780_000_000,
        }
    }

    #[test]
    fn mock_runtime_assembles_and_lists_sessions() {
        let mock = MockRuntime {
            sessions: vec![sample_session("sess_a"), sample_session("sess_b")],
            ..Default::default()
        };
        let rt = mock.into_runtime();
        let listed = rt.sessions.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].session_id, "sess_a");
    }

    #[test]
    fn session_detail_falls_back_to_list_scan() {
        let mock = MockRuntime {
            sessions: vec![sample_session("sess_a"), sample_session("sess_b")],
            ..Default::default()
        };
        let rt = mock.into_runtime();
        assert!(rt.sessions.detail("sess_a").is_some());
        assert!(rt.sessions.detail("sess_missing").is_none());
    }

    #[test]
    fn brain_view_returns_recent_decisions_with_cap() {
        let mock = MockRuntime {
            decisions: (0..5)
                .map(|i| DecisionSummary {
                    id: format!("dec_{i}"),
                    timestamp: format!("2026-06-06T00:00:0{i}Z"),
                    action: "approve".into(),
                    confidence: Some(0.9),
                    project: None,
                    tool: None,
                })
                .collect(),
            ..Default::default()
        };
        let rt = mock.into_runtime();
        assert_eq!(rt.brain.recent_decisions(3).len(), 3);
        assert_eq!(rt.brain.decision_count(), 5);
        assert_eq!(rt.brain.gate_mode(), BrainGateMode::On);
    }

    #[test]
    fn coord_view_reports_empty_state_cleanly() {
        let rt = MockRuntime::default().into_runtime();
        assert!(rt.coord.active_leases().is_empty());
        assert!(rt.coord.pending_handoffs().is_empty());
        assert!(rt.coord.pending_interrupts().is_empty());
    }

    #[test]
    fn bus_view_reports_empty_state_cleanly() {
        let rt = MockRuntime::default().into_runtime();
        assert!(rt.bus.list_agents().is_empty());
        assert!(rt.bus.list_roles().is_empty());
    }

    /// Smoke test that the trait shapes are usable behind `dyn`. If this
    /// compiles, the boxed-trait composition works.
    #[test]
    fn runtime_implements_clone_and_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Runtime>();

        let rt = MockRuntime::default().into_runtime();
        let _rt2 = rt.clone();
    }
}

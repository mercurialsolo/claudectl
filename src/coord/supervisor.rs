// Allow dead_code: `Action` variants, `Policy`, and `Sensors` are the type
// surface the actuator (PR4) and verifier runner (PR5) will consume. The
// tick() is exercised by tests in this file; the headless wire-in lands
// alongside the actuator.
#![allow(dead_code)]
//! Reconciler skeleton (#345, RFC v2 §3).
//!
//! The supervisor's tick loop is **pure**: given the desired state in
//! coord.db and the observed state from sensors, return a list of
//! `Action`s. A separate actuator carries them out. This split is what
//! makes the loop testable without spinning up real sessions, and what
//! makes "kill -9 mid-run → restart converges" a property we can write
//! down rather than wave at.
//!
//! Scope of this PR: the type surface, the bus-mailbox / spawn /
//! verify wiring as `Action` variants the actuator will consume in a
//! later PR (#346 / #347), and a minimal `tick()` that emits no-ops on
//! every state today. The headless daemon wires `tick()` in behind a
//! cfg-gate so the supervisor is reachable without affecting today's
//! behavior.

use std::path::PathBuf;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::tasks::{TaskState, list_tasks};

/// Actions the actuator carries out. Variants are the bridge between the
/// pure reconciler and the real subsystems (bus mailbox, launcher, brain).
/// Add new variants here when wiring new transition causes; don't make the
/// actuator inspect task rows directly — that would put control flow
/// outside the reconciler's pure scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Action {
    /// Write a `task` message to the role's mailbox. Becomes `ASSIGNED`
    /// once the bus insert lands.
    AssignViaMailbox { task_id: String, role: String },
    /// Spawn a Claude Code session in `cwd` with the task prompt — used
    /// when the role's mailbox went unclaimed past `claim_timeout`.
    Spawn { task_id: String, cwd: PathBuf },
    /// Verifier gate: run a shell/brain/agent verifier for the attempt.
    /// Verifier kind is opaque at this layer; the actuator dispatches
    /// based on the task's verifier list (PR5 / #347).
    RunVerifier { task_id: String, attempt_id: String },
    /// Write a per-session policy file (§8). Carries the effective
    /// `force_manual` / `inherit` choice the supervisor computed once at
    /// assignment time. The brain-gate hook reads this file on every
    /// tool call.
    WriteSessionPolicy {
        session_id: String,
        task_id: String,
        approve_mode: super::session_policy::ApproveMode,
    },
    /// Delete a per-session policy. Issued on any terminal transition
    /// so dangling files don't outlive their task.
    ClearSessionPolicy { session_id: String },
    /// Escalate to the `operator` role. Carries the cause so the
    /// downstream message body contains the same string the
    /// `task_transitions` row recorded.
    EscalateHuman { task_id: String, cause: String },
}

/// Knobs governing reconciler behavior. Loaded from
/// `~/.claudectl/coord/policy.toml` in a later PR; this PR ships the
/// type and the defaults so the reconciler can be wired in without
/// a config-loading hop.
#[derive(Debug, Clone)]
pub struct Policy {
    pub tick_ms: u64,
    pub max_concurrent: u32,
    pub claim_timeout_min: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            tick_ms: 2000,
            max_concurrent: 4,
            claim_timeout_min: 5,
        }
    }
}

/// Sensors abstract the inputs the reconciler needs to read. A `Sensors`
/// impl that returns hard-coded values is what makes the unit tests
/// possible. The real implementation will sit alongside `LiveBrainDriver`
/// in `src/runtime/` (next PR) and stitch JSONL tail + `ps` + hook events
/// together.
pub trait Sensors {
    /// Last `hook_events.id` the reconciler observed in the previous
    /// tick. The tick should query forward from this so it stays O(new
    /// events) instead of O(table size).
    fn last_hook_event_id(&self) -> i64;
    /// Sessions currently observable on the host. Reconciler refuses to
    /// actuate on `Unknown` per RFC v2 §6.
    fn observed_sessions(&self) -> Vec<ObservedSession>;
}

#[derive(Debug, Clone)]
pub struct ObservedSession {
    pub session_id: String,
    pub pid: u32,
    pub status: ObservedStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedStatus {
    Running,
    Idle,
    Dead,
    /// `Unknown` is load-bearing: when the sensor can't determine a
    /// session's state, the reconciler does nothing — RFC v2 §6 calls
    /// this the no-actuation backstop. Without it, the supervisor
    /// could re-spawn a session it thinks is dead but isn't.
    Unknown,
}

/// Reconciliation core. Reads desired state (`coord.tasks`), reads
/// observed state from `sensors`, returns the action list. Pure — no
/// I/O, no side effects, no writes to the DB.
pub struct Supervisor {
    pub policy: Policy,
}

impl Supervisor {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }

    pub fn with_defaults() -> Self {
        Self::new(Policy::default())
    }

    /// Read coord state and decide what to actuate. Pure: every input
    /// is in `conn` or `sensors`; every output is in the returned
    /// `Vec<Action>`. The actuator is responsible for both performing
    /// the action and writing the resulting transition.
    pub fn tick(&self, conn: &Connection, sensors: &dyn Sensors) -> Result<Vec<Action>, String> {
        let mut out = Vec::new();
        let pending = list_tasks(conn, Some(TaskState::Pending))?;
        let ready = list_tasks(conn, Some(TaskState::Ready))?;
        let assigned = list_tasks(conn, Some(TaskState::Assigned))?;
        let running = list_tasks(conn, Some(TaskState::Running))?;

        // 1) Pending tasks whose deps resolved → no-op for now; the deps
        //    resolver lands in M4. Walk the list so an empty pending set
        //    is a recognized branch instead of dead code.
        let _ = pending;
        let _ = ready;

        // 2) Assigned tasks → spawn fallback after claim_timeout. Not yet
        //    implemented — we don't have a way to read the message's
        //    insertion time without joining bus.db, which lives in M3
        //    (#346). Leaving the branch in place documents the intent.
        let _ = assigned;

        // 3) Running tasks → health-check triggers (#348 / M5). Skipped
        //    here; just make sure unknown observed sessions don't
        //    accidentally drive an actuation.
        for t in &running {
            let _ = t; // tracking only
        }
        let _ = sensors.observed_sessions();
        let _ = sensors.last_hook_event_id();

        // For now the reconciler is a no-op; the type surface is what
        // M3/M4/M5 will fill in. Returning an empty list is the
        // crash-safety baseline: restart from an arbitrary point and the
        // reconciler does no damage.
        Ok(out.split_off(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::store;
    use crate::coord::tasks::{NewTask, insert_task};

    struct StubSensors {
        last_id: i64,
        sessions: Vec<ObservedSession>,
    }

    impl Sensors for StubSensors {
        fn last_hook_event_id(&self) -> i64 {
            self.last_id
        }
        fn observed_sessions(&self) -> Vec<ObservedSession> {
            self.sessions.clone()
        }
    }

    #[test]
    fn tick_is_noop_when_no_tasks() {
        let conn = store::open_memory();
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_does_not_actuate_on_unknown_observed_status() {
        let conn = store::open_memory();
        let _ = insert_task(
            &conn,
            &NewTask {
                name: "t".into(),
                role: None,
                cwd: "/x".into(),
                prompt: "do".into(),
                model: None,
                budget_usd: None,
                max_retries: None,
                timeout_min: None,
                depends_on: vec![],
                policy: None,
            },
        )
        .unwrap();
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![ObservedSession {
                session_id: "ghost".into(),
                pid: 0,
                status: ObservedStatus::Unknown,
            }],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        // No actions for Unknown observed sessions, per RFC v2 §6.
        assert!(actions.is_empty());
    }
}

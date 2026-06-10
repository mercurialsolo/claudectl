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
    /// Resume an interrupted task (#347, RFC §7). The actuator marks
    /// `Resuming`, bumps the attempt counter, builds the recovery
    /// context (original prompt + verifier history + autopsy +
    /// tree-state drift warning), and re-enters Assigned. Carries the
    /// cause so the transition log shows whether the session died, was
    /// stalled, or hit a retry-loop trigger.
    Resume { task_id: String, cause: String },
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
    /// Health-check → action mapping (RFC §6). 10 shipped health
    /// checks are advisory-only by default; this map turns each into
    /// a supervisor transition trigger. Defaults match RFC §6 table.
    pub health_actions: HealthActionMap,
}

/// Reactions to the 10 shipped health checks. Each variant becomes
/// either a `Resume` (re-enter Assigned) or `Escalate` (NeedsHuman
/// via operator). Re-evaluate on cost-spike means the reconciler may
/// adjust budget but doesn't itself transition the task — that lands
/// when the budget policy plane grows up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthAction {
    /// Do nothing — leave the health check as a dashboard advisory
    /// (today's behavior). Used as the default when an installation
    /// wants to opt into the supervisor without the health-driven
    /// transitions yet.
    Ignore,
    /// Transition the task through Resume after a brief grace period.
    Resume,
    /// Escalate to NeedsHuman with the health check's name as the
    /// cause. Right for retry loops / repetition where more attempts
    /// won't help.
    Escalate,
}

/// Per-check mapping. Field names match the shipped
/// `HealthCheck::name` strings so adding a new health check requires
/// adding a new field here too — the compiler enforces the wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthActionMap {
    pub stalled: HealthAction,
    pub loop_detected: HealthAction,
    pub repetition: HealthAction,
    pub cost_spike: HealthAction,
    pub context_saturation: HealthAction,
    pub error_acceleration: HealthAction,
    pub cognitive_decay: HealthAction,
}

impl Default for HealthActionMap {
    fn default() -> Self {
        // RFC §6 table: stalled → resume, loop/repetition → escalate,
        // context_saturation → proactive compaction (handled by the
        // existing health pipeline, not a supervisor transition).
        // Anything not in the table is `Ignore` so the supervisor only
        // acts on signals it knows what to do with.
        Self {
            stalled: HealthAction::Resume,
            loop_detected: HealthAction::Escalate,
            repetition: HealthAction::Escalate,
            cost_spike: HealthAction::Ignore,
            context_saturation: HealthAction::Ignore,
            error_acceleration: HealthAction::Resume,
            cognitive_decay: HealthAction::Ignore,
        }
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            tick_ms: 2000,
            max_concurrent: 4,
            claim_timeout_min: 5,
            health_actions: HealthActionMap::default(),
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
    /// Active health-check names firing for this session, sorted from
    /// most-to-least severe (RFC §6). Reconciler maps each to the
    /// configured `HealthAction`. Empty when the session is healthy
    /// or sensors haven't reported on it yet.
    pub health_alerts: Vec<String>,
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
        let assigned = list_tasks(conn, Some(TaskState::Assigned))?;
        let running = list_tasks(conn, Some(TaskState::Running))?;

        // 1) Pending tasks → emit assignment actions. Tasks with a role
        //    go to that role's mailbox first; tasks without a role go
        //    straight to spawn. Dependency resolution is deferred (M4
        //    will gate this on `depends_on` rows reaching Done); for now
        //    every Pending task is treated as Ready.
        //
        //    Hard ceiling on concurrent in-flight (Assigned + Running)
        //    so a backlog can't push the fleet past `max_concurrent`.
        let in_flight = (assigned.len() + running.len()) as u32;
        if in_flight < self.policy.max_concurrent {
            let slots = self.policy.max_concurrent - in_flight;
            for task in pending.iter().take(slots as usize) {
                match &task.role {
                    Some(role) => out.push(Action::AssignViaMailbox {
                        task_id: task.id.clone(),
                        role: role.clone(),
                    }),
                    None => out.push(Action::Spawn {
                        task_id: task.id.clone(),
                        cwd: PathBuf::from(&task.cwd),
                    }),
                }
            }
        }

        // 2) Assigned tasks → spawn fallback after `claim_timeout`. The
        //    actuator records `task_attempts.started_at`; here we walk
        //    each assigned task, look at its most recent attempt's start
        //    time, and emit `Spawn` if the role's mailbox went unclaimed.
        //    No observable session for the role in `observed_sessions`
        //    is the trigger — RFC §4's "role mailbox unclaimed > T".
        let observed = sensors.observed_sessions();
        for task in &assigned {
            let elapsed_min = match super::tasks::latest_attempt_age_minutes(conn, &task.id)? {
                Some(m) => m,
                None => continue,
            };
            if elapsed_min < self.policy.claim_timeout_min as u64 {
                continue;
            }
            // Has anything observably claimed this role? If a session at
            // the task's `cwd` is Running, treat the mailbox as claimed
            // (the recipient picked it up, just hasn't transitioned the
            // task row yet). Unknown status is the no-actuation backstop.
            let claimed = observed.iter().any(|s| {
                matches!(s.status, ObservedStatus::Running | ObservedStatus::Idle)
                    && session_cwd_matches(&s.session_id, &task.cwd)
            });
            if !claimed {
                out.push(Action::Spawn {
                    task_id: task.id.clone(),
                    cwd: PathBuf::from(&task.cwd),
                });
            }
        }

        // 3) Running tasks → resume on session death + health-check
        //    transitions (RFC §6 / §7). For each Running task, look up
        //    its session in `observed` and emit Resume when the session
        //    is Dead; map each active health alert to its configured
        //    HealthAction.
        let observed_by_session: std::collections::HashMap<&str, &ObservedSession> = observed
            .iter()
            .map(|s| (s.session_id.as_str(), s))
            .collect();
        for task in &running {
            let session_id = super::tasks::latest_session_id(conn, &task.id)?;
            let Some(session_id) = session_id else {
                continue;
            };
            let Some(session) = observed_by_session.get(session_id.as_str()) else {
                continue;
            };
            // Dead session is the canonical resume trigger (RFC §7). It
            // wins over health alerts — a dead session has no health
            // signal worth honoring.
            if session.status == ObservedStatus::Dead {
                out.push(Action::Resume {
                    task_id: task.id.clone(),
                    cause: "session_died".to_string(),
                });
                continue;
            }
            // Unknown stays the no-actuation backstop. Don't act on
            // health alerts for a session whose state we can't pin
            // down — a slow ps poll shouldn't drive escalation.
            if session.status == ObservedStatus::Unknown {
                continue;
            }
            // Map active alerts. First one whose action isn't Ignore
            // wins so we don't emit conflicting Resume + Escalate for
            // the same task on the same tick.
            for alert in &session.health_alerts {
                let action = self.policy.health_actions.for_check(alert);
                match action {
                    HealthAction::Ignore => continue,
                    HealthAction::Resume => {
                        out.push(Action::Resume {
                            task_id: task.id.clone(),
                            cause: format!("health:{alert}"),
                        });
                    }
                    HealthAction::Escalate => {
                        out.push(Action::EscalateHuman {
                            task_id: task.id.clone(),
                            cause: format!("health:{alert}"),
                        });
                    }
                }
                break;
            }
        }
        let _ = sensors.last_hook_event_id();

        Ok(out)
    }
}

impl HealthActionMap {
    /// Map a `HealthCheck::name` string to its configured action. Names
    /// match the constants in `claudectl-core::health` — adding a new
    /// check there means adding a branch here too; the test suite
    /// asserts the table is exhaustive over the known set.
    pub fn for_check(&self, name: &str) -> HealthAction {
        match name {
            "Stalled" => self.stalled,
            "Loop detected" => self.loop_detected,
            "Repetition" => self.repetition,
            "Cost spike" => self.cost_spike,
            "Context saturation" => self.context_saturation,
            "Error acceleration" => self.error_acceleration,
            "Cognitive decay" => self.cognitive_decay,
            _ => HealthAction::Ignore,
        }
    }
}

/// Placeholder for the cwd-aware session matcher. The real implementation
/// will query `discovery::scan_sessions()` and compare per-session cwd.
/// For now we treat any session as a possible match — this preserves the
/// invariant "no spawn when something *might* be claiming the mailbox"
/// and errs on the side of doing nothing, which is the safe direction
/// while the sensor layer is still stubbed.
fn session_cwd_matches(_session_id: &str, _task_cwd: &str) -> bool {
    true
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
    fn tick_emits_assign_for_pending_with_role() {
        let conn = store::open_memory();
        let id = insert_task(
            &conn,
            &NewTask {
                name: "t".into(),
                role: Some("backend".into()),
                cwd: "/work/x".into(),
                prompt: "do".into(),
                model: None,
                budget_usd: None,
                max_retries: None,
                timeout_min: None,
                depends_on: vec![],
                policy: None,
                verifiers: vec![],
            },
        )
        .unwrap();
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::AssignViaMailbox { task_id, role } => {
                assert_eq!(task_id, &id);
                assert_eq!(role, "backend");
            }
            other => panic!("expected AssignViaMailbox, got {other:?}"),
        }
    }

    #[test]
    fn tick_emits_spawn_for_pending_without_role() {
        let conn = store::open_memory();
        let id = insert_task(
            &conn,
            &NewTask {
                name: "t".into(),
                role: None,
                cwd: "/work/x".into(),
                prompt: "do".into(),
                model: None,
                budget_usd: None,
                max_retries: None,
                timeout_min: None,
                depends_on: vec![],
                policy: None,
                verifiers: vec![],
            },
        )
        .unwrap();
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Spawn { task_id, cwd } => {
                assert_eq!(task_id, &id);
                assert_eq!(cwd.to_string_lossy(), "/work/x");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn tick_respects_max_concurrent_ceiling() {
        let conn = store::open_memory();
        // Five Pending tasks, max_concurrent = 4.
        for i in 0..5 {
            insert_task(
                &conn,
                &NewTask {
                    name: format!("t{i}"),
                    role: Some("backend".into()),
                    cwd: "/work/x".into(),
                    prompt: "do".into(),
                    model: None,
                    budget_usd: None,
                    max_retries: None,
                    timeout_min: None,
                    depends_on: vec![],
                    policy: None,
                    verifiers: vec![],
                },
            )
            .unwrap();
        }
        let sup = Supervisor::new(Policy {
            max_concurrent: 4,
            ..Policy::default()
        });
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        // Only 4 actions emitted — the fifth waits for an in-flight slot
        // to clear.
        assert_eq!(actions.len(), 4);
    }

    /// Helper for the resume / health tests: insert a task and drive it
    /// to Running with a known session_id so the reconciler can map the
    /// task back to an observed session.
    fn task_in_running(conn: &mut rusqlite::Connection, session_id: &str) -> String {
        use crate::coord::tasks::insert_attempt;
        let id = insert_task(
            conn,
            &NewTask {
                name: "t".into(),
                role: None,
                cwd: "/work/x".into(),
                prompt: "do".into(),
                model: None,
                budget_usd: None,
                max_retries: Some(2),
                timeout_min: None,
                depends_on: vec![],
                policy: None,
                verifiers: vec![],
            },
        )
        .unwrap();
        // Manually drive through Pending → Running so the resume
        // reconciler path has something observable to map.
        crate::coord::tasks::transition(
            conn,
            &id,
            TaskState::Pending,
            TaskState::Running,
            "test-running",
        )
        .unwrap();
        // Attempt row carries the session_id the reconciler joins on.
        insert_attempt(conn, &id, 1, Some(session_id), None, None).unwrap();
        id
    }

    #[test]
    fn tick_emits_resume_on_dead_session() {
        let mut conn = store::open_memory();
        let task_id = task_in_running(&mut conn, "sess_a");
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![ObservedSession {
                session_id: "sess_a".into(),
                pid: 0,
                status: ObservedStatus::Dead,
                health_alerts: vec![],
            }],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Resume {
                task_id: tid,
                cause,
            } => {
                assert_eq!(tid, &task_id);
                assert_eq!(cause, "session_died");
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn tick_does_not_act_on_unknown_observed_status() {
        let mut conn = store::open_memory();
        let _ = task_in_running(&mut conn, "sess_a");
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![ObservedSession {
                session_id: "sess_a".into(),
                pid: 0,
                status: ObservedStatus::Unknown,
                health_alerts: vec!["Stalled".into()], // would normally trigger
                                                       // Resume, but Unknown
                                                       // status blocks it
            }],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        // No actions: Unknown is the RFC §6 no-actuation backstop.
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_maps_stalled_alert_to_resume_action() {
        let mut conn = store::open_memory();
        let task_id = task_in_running(&mut conn, "sess_a");
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![ObservedSession {
                session_id: "sess_a".into(),
                pid: 0,
                status: ObservedStatus::Running,
                health_alerts: vec!["Stalled".into()],
            }],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Resume {
                task_id: tid,
                cause,
            } => {
                assert_eq!(tid, &task_id);
                assert_eq!(cause, "health:Stalled");
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn tick_maps_loop_alert_to_escalate_action() {
        let mut conn = store::open_memory();
        let task_id = task_in_running(&mut conn, "sess_a");
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![ObservedSession {
                session_id: "sess_a".into(),
                pid: 0,
                status: ObservedStatus::Running,
                health_alerts: vec!["Loop detected".into()],
            }],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::EscalateHuman {
                task_id: tid,
                cause,
            } => {
                assert_eq!(tid, &task_id);
                assert_eq!(cause, "health:Loop detected");
            }
            other => panic!("expected EscalateHuman, got {other:?}"),
        }
    }

    #[test]
    fn tick_ignores_alerts_mapped_to_ignore() {
        // Default policy: Cost spike is `Ignore`.
        let mut conn = store::open_memory();
        let _ = task_in_running(&mut conn, "sess_a");
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![ObservedSession {
                session_id: "sess_a".into(),
                pid: 0,
                status: ObservedStatus::Running,
                health_alerts: vec!["Cost spike".into()],
            }],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        assert!(actions.is_empty(), "Ignore-mapped alerts must not emit");
    }

    #[test]
    fn tick_first_actionable_alert_wins_over_later() {
        let mut conn = store::open_memory();
        let _ = task_in_running(&mut conn, "sess_a");
        let sup = Supervisor::with_defaults();
        let sensors = StubSensors {
            last_id: 0,
            sessions: vec![ObservedSession {
                session_id: "sess_a".into(),
                pid: 0,
                status: ObservedStatus::Running,
                // Order: ignored, then Stalled (Resume), then Loop (Escalate).
                // The reconciler must emit exactly one action (Resume) for
                // this task; emitting both would conflict.
                health_alerts: vec![
                    "Cost spike".into(),
                    "Stalled".into(),
                    "Loop detected".into(),
                ],
            }],
        };
        let actions = sup.tick(&conn, &sensors).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::Resume { .. }));
    }
}

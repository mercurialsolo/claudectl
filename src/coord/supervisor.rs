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
    /// Leave a ready task where it is this pass, and say why (#571): no
    /// machine has room, or the only machine that could take it is a peer.
    /// Changes nothing; `supervisor work` prints it when the reason changes.
    Hold { task_id: String, reason: String },
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
    /// Each machine a task could run on, measured (#571). Empty means none
    /// was measured: this machine is assumed, with `max_concurrent` as its
    /// only limit, which is what the reconciler did before placement.
    fn placement(&self) -> Vec<super::placement::PeerView> {
        Vec::new()
    }
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
    /// A spawned session ended and reported success (#570). Its task moves
    /// on to its verifiers.
    Completed,
    /// A spawned session ended and reported an error (#570): not logged in,
    /// out of budget, refused. Resumed like a death, up to the resume cap.
    Failed,
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
        let retrying = list_tasks(conn, Some(TaskState::Retrying))?;

        // 1) Retrying, then Pending tasks → emit start actions, under a hard
        //    ceiling on concurrent in-flight (Assigned + Running) so a
        //    backlog can't push the fleet past `max_concurrent`.
        //
        //    A Retrying task failed a verifier and goes back to work in the
        //    same workdir with the failure in its prompt (#574). It's further
        //    along than anything Pending, so it takes a slot first. It's
        //    always spawned: a role's mailbox would hand over the bare prompt.
        //
        //    A Pending task starts only when every task it depends on is
        //    DONE (#572). One that can never start, because a dependency
        //    ended without finishing, is escalated with the reason.
        //
        //    Where each one runs is placement's call (#571): this machine's
        //    measured headroom against its slot policy, the task's `requires`,
        //    its `peer`. Tasks can't run on a peer yet, so an unpinned task is
        //    placed among this machine only; one that only a peer could take
        //    is held with the reason, naming that peer.
        let in_flight = (assigned.len() + running.len()) as u32;
        let mut views = sensors.placement();
        if !views.iter().any(|v| v.peer.is_none()) {
            views.push(super::placement::PeerView::local_with_ceiling(
                self.policy.max_concurrent,
            ));
        }
        for v in views.iter_mut().filter(|v| v.peer.is_none()) {
            // What's in flight is known exactly here; the probe can lag.
            v.headroom.running_tasks = in_flight;
        }
        let mut place = |task: &super::tasks::TaskRow, out: &mut Vec<Action>| -> bool {
            use super::placement::{Decision, place};
            // Why this machine won't take it, unless it's pinned elsewhere.
            let mut here = None;
            if task.peer.is_none() {
                let local = views.iter().position(|v| v.peer.is_none()).expect("pushed");
                match place(&mut views[local..=local], &task.requires, None) {
                    Decision::Local => return true,
                    Decision::Wait(why) => here = Some(why),
                    Decision::Remote(_) => {}
                }
            }
            // Ask again over every machine, without counting it anywhere, to
            // say whether a peer could. This machine's own reason leads: a
            // peer with room is no help until tasks can run there.
            let reason = match (
                here,
                place(&mut views.clone(), &task.requires, task.peer.as_deref()),
            ) {
                (Some(why), Decision::Remote(p)) => {
                    format!("{why}; {p} has room, but running tasks on peers is not built yet")
                }
                (None, Decision::Remote(p)) => format!(
                    "pinned to {p}, which has room, but running tasks on peers is not built yet"
                ),
                (_, Decision::Wait(why)) => why,
                (here, Decision::Local) => here.unwrap_or_else(|| "this machine is full".into()),
            };
            out.push(Action::Hold {
                task_id: task.id.clone(),
                reason,
            });
            false
        };
        for task in &retrying {
            if place(task, &mut out) {
                out.push(Action::Spawn {
                    task_id: task.id.clone(),
                    cwd: PathBuf::from(&task.cwd),
                });
            }
        }
        let mut ready = Vec::new();
        for task in &pending {
            match dependency_gate(conn, task)? {
                Gate::Ready => ready.push(task),
                Gate::Waiting => {}
                Gate::Blocked(cause) => out.push(Action::EscalateHuman {
                    task_id: task.id.clone(),
                    cause,
                }),
            }
        }
        for task in ready {
            if !place(task, &mut out) {
                continue;
            }
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
            // A session that finished its work goes to its verifiers. This
            // is the only path from RUNNING to VERIFYING (#570); nothing
            // emitted RunVerifier before.
            if session.status == ObservedStatus::Completed {
                if let Some(attempt_id) = super::tasks::latest_attempt_id(conn, &task.id)? {
                    out.push(Action::RunVerifier {
                        task_id: task.id.clone(),
                        attempt_id,
                    });
                }
                continue;
            }
            if session.status == ObservedStatus::Failed {
                out.push(Action::Resume {
                    task_id: task.id.clone(),
                    cause: "session_failed".to_string(),
                });
                continue;
            }
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

/// Whether a Pending task may start, given its `depends_on` (#572).
#[derive(Debug, PartialEq)]
enum Gate {
    Ready,
    /// A dependency hasn't finished yet.
    Waiting,
    /// A dependency ended without finishing (NEEDS_HUMAN or CANCELLED), or
    /// doesn't exist, so this task can never start on its own.
    Blocked(String),
}

fn dependency_gate(conn: &Connection, task: &super::tasks::TaskRow) -> Result<Gate, String> {
    let mut waiting = false;
    for key in &task.depends_on {
        let Some(dep) = super::tasks::resolve_dependency(conn, key)? else {
            return Ok(Gate::Blocked(format!("dependency_missing:{key}")));
        };
        match dep.state {
            TaskState::Done => {}
            TaskState::NeedsHuman | TaskState::Cancelled => {
                return Ok(Gate::Blocked(format!(
                    "dependency_{}:{}",
                    dep.state.as_str().to_ascii_lowercase(),
                    dep.name
                )));
            }
            _ => waiting = true,
        }
    }
    Ok(if waiting { Gate::Waiting } else { Gate::Ready })
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
                git_ref: None,
                branch: None,
                permission_mode: None,
                requires: vec![],
                peer: None,
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

    fn task(name: &str, depends_on: &[&str]) -> NewTask {
        NewTask {
            name: name.into(),
            role: None,
            cwd: "/work/x".into(),
            prompt: "do".into(),
            model: None,
            budget_usd: None,
            max_retries: None,
            timeout_min: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            policy: None,
            verifiers: vec![],
            git_ref: None,
            branch: None,
            permission_mode: None,
            requires: vec![],
            peer: None,
        }
    }

    fn set_state(conn: &Connection, id: &str, state: TaskState) {
        conn.execute(
            "UPDATE tasks SET state = ?1 WHERE id = ?2",
            rusqlite::params![state.as_str(), id],
        )
        .unwrap();
    }

    fn no_sensors() -> StubSensors {
        StubSensors {
            last_id: 0,
            sessions: vec![],
        }
    }

    fn spawned(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Spawn { task_id, .. } => Some(task_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// #572: in A → B → C, only A starts; B starts once A is DONE.
    #[test]
    fn a_task_waits_until_every_dependency_is_done() {
        let conn = store::open_memory();
        let a = insert_task(&conn, &task("a", &[])).unwrap();
        let b = insert_task(&conn, &task("b", &[&a])).unwrap();
        let c = insert_task(&conn, &task("c", &[&b])).unwrap();
        let sup = Supervisor::with_defaults();
        assert_eq!(
            spawned(&sup.tick(&conn, &no_sensors()).unwrap()),
            vec![a.clone()]
        );

        set_state(&conn, &a, TaskState::Running);
        assert!(spawned(&sup.tick(&conn, &no_sensors()).unwrap()).is_empty());

        set_state(&conn, &a, TaskState::Done);
        assert_eq!(spawned(&sup.tick(&conn, &no_sensors()).unwrap()), vec![b]);
        let _ = c;
    }

    /// #572: a dependency that ends without finishing blocks its dependents
    /// with the reason, instead of leaving them Pending forever.
    #[test]
    fn a_cancelled_dependency_escalates_its_dependent_with_the_reason() {
        let conn = store::open_memory();
        let a = insert_task(&conn, &task("schema", &[])).unwrap();
        let b = insert_task(&conn, &task("api", &[&a])).unwrap();
        set_state(&conn, &a, TaskState::Cancelled);
        let actions = Supervisor::with_defaults()
            .tick(&conn, &no_sensors())
            .unwrap();
        assert!(
            matches!(&actions[..], [Action::EscalateHuman { task_id, cause }]
                if *task_id == b && cause == "dependency_cancelled:schema"),
            "{actions:?}"
        );
    }

    #[test]
    fn a_dependency_on_a_task_that_does_not_exist_is_blocked() {
        let conn = store::open_memory();
        let b = insert_task(&conn, &task("api", &["task_nope"])).unwrap();
        let actions = Supervisor::with_defaults()
            .tick(&conn, &no_sensors())
            .unwrap();
        assert!(
            matches!(&actions[..], [Action::EscalateHuman { task_id, cause }]
                if *task_id == b && cause == "dependency_missing:task_nope"),
            "{actions:?}"
        );
    }

    /// #574: a Retrying task is respawned, and takes a slot before anything
    /// Pending.
    struct PlacedSensors(Vec<crate::coord::placement::PeerView>);

    impl Sensors for PlacedSensors {
        fn last_hook_event_id(&self) -> i64 {
            0
        }
        fn observed_sessions(&self) -> Vec<ObservedSession> {
            vec![]
        }
        fn placement(&self) -> Vec<crate::coord::placement::PeerView> {
            self.0.clone()
        }
    }

    fn held(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Hold { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    /// #571: a task that needs something only a peer has waits, and says
    /// which peer would take it, instead of running here without it.
    #[test]
    fn a_task_only_a_peer_can_run_is_held_and_names_the_peer() {
        use crate::coord::placement::PeerView;
        let conn = store::open_memory();
        let id = insert_task(
            &conn,
            &NewTask {
                requires: vec!["docker".into()],
                ..task("build", &[])
            },
        )
        .unwrap();
        let mut box_ = PeerView::local_with_ceiling(2);
        box_.peer = Some("box".into());
        box_.tags = vec!["docker".into()];
        let sensors = PlacedSensors(vec![PeerView::local_with_ceiling(4), box_]);
        let actions = Supervisor::with_defaults().tick(&conn, &sensors).unwrap();
        assert!(spawned(&actions).is_empty(), "{actions:?}");
        assert_eq!(
            held(&actions),
            vec![
                "this machine lacks `docker`; box has room, but running tasks on peers is not built yet"
            ]
        );
        let _ = id;
    }

    /// #571: an unpinned task still runs here when this machine has room,
    /// however much more a peer has: it can't run on the peer yet.
    #[test]
    fn an_unpinned_task_runs_here_even_when_a_peer_has_more_room() {
        use crate::coord::placement::PeerView;
        let conn = store::open_memory();
        let id = insert_task(&conn, &task("t", &[])).unwrap();
        let mut big = PeerView::local_with_ceiling(10);
        big.peer = Some("big".into());
        big.headroom.mem_free_mb = Some(1_000_000);
        let sensors = PlacedSensors(vec![PeerView::local_with_ceiling(4), big]);
        let actions = Supervisor::with_defaults().tick(&conn, &sensors).unwrap();
        assert_eq!(spawned(&actions), vec![id]);
    }

    /// #571: a drained machine starts nothing, and the queue says why. The
    /// drain marker used to be read by `doctor` alone.
    #[test]
    fn a_drained_machine_holds_its_queue() {
        use crate::coord::placement::PeerView;
        let conn = store::open_memory();
        insert_task(&conn, &task("t", &[])).unwrap();
        let mut local = PeerView::local_with_ceiling(4);
        local.slots.drain = true;
        let actions = Supervisor::with_defaults()
            .tick(&conn, &PlacedSensors(vec![local]))
            .unwrap();
        assert_eq!(held(&actions), vec!["this machine is draining"]);
    }

    #[test]
    fn a_retrying_task_is_respawned_ahead_of_pending_work() {
        let conn = store::open_memory();
        let pending = insert_task(&conn, &task("new", &[])).unwrap();
        let retrying = insert_task(&conn, &task("again", &[])).unwrap();
        set_state(&conn, &retrying, TaskState::Retrying);
        let sup = Supervisor::new(Policy {
            max_concurrent: 1,
            ..Policy::default()
        });
        assert_eq!(
            spawned(&sup.tick(&conn, &no_sensors()).unwrap()),
            vec![retrying]
        );
        let _ = pending;
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
                git_ref: None,
                branch: None,
                permission_mode: None,
                requires: vec![],
                peer: None,
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
                    git_ref: None,
                    branch: None,
                    permission_mode: None,
                    requires: vec![],
                    peer: None,
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
        // Only 4 start — the fifth waits for an in-flight slot to clear,
        // and is held with the reason.
        let started = actions
            .iter()
            .filter(|a| !matches!(a, Action::Hold { .. }))
            .count();
        assert_eq!(started, 4);
        assert!(
            matches!(&actions[4], Action::Hold { reason, .. }
                if reason == "this machine is running 4 of 4 tasks"),
            "{actions:?}"
        );
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
                git_ref: None,
                branch: None,
                permission_mode: None,
                requires: vec![],
                peer: None,
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

    fn observed(status: ObservedStatus) -> StubSensors {
        StubSensors {
            last_id: 0,
            sessions: vec![ObservedSession {
                session_id: "sess_a".into(),
                pid: 0,
                status,
                health_alerts: vec![],
            }],
        }
    }

    /// The only way from RUNNING to VERIFYING (#570): nothing emitted
    /// RunVerifier before, so a task never reached its verifiers.
    #[test]
    fn a_completed_session_sends_its_task_to_the_verifiers() {
        let mut conn = store::open_memory();
        let task_id = task_in_running(&mut conn, "sess_a");
        let attempt = crate::coord::tasks::latest_attempt_id(&conn, &task_id)
            .unwrap()
            .unwrap();
        let actions = Supervisor::with_defaults()
            .tick(&conn, &observed(ObservedStatus::Completed))
            .unwrap();
        assert!(
            matches!(&actions[..], [Action::RunVerifier { task_id: t, attempt_id: a }] if *t == task_id && *a == attempt),
            "{actions:?}"
        );
    }

    #[test]
    fn a_failed_session_is_resumed_with_its_own_cause() {
        let mut conn = store::open_memory();
        let task_id = task_in_running(&mut conn, "sess_a");
        let actions = Supervisor::with_defaults()
            .tick(&conn, &observed(ObservedStatus::Failed))
            .unwrap();
        assert!(
            matches!(&actions[..], [Action::Resume { task_id: t, cause }] if *t == task_id && cause == "session_failed"),
            "{actions:?}"
        );
    }

    #[test]
    fn a_running_session_is_left_alone() {
        let mut conn = store::open_memory();
        let _ = task_in_running(&mut conn, "sess_a");
        let actions = Supervisor::with_defaults()
            .tick(&conn, &observed(ObservedStatus::Running))
            .unwrap();
        assert!(actions.is_empty(), "{actions:?}");
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

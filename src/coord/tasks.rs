// Allow dead_code at the module level: this is the v2-schema CRUD surface
// the reconciler will consume in PR4/PR5/PR6. Each function is exercised by
// the test suite in this file; the binary main loop wires them in next PR.
#![allow(dead_code)]
//! CRUD for the supervisor task tables (#345).
//!
//! The supervisor's correctness property is "restart re-converges from
//! coord.db" — so every state-machine move ends in one of these functions.
//! They write the new state *and* an append-only `task_transitions` row in
//! the same transaction; crash recovery just walks `tasks.state` + the
//! latest transition for each id.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::store::gen_id;

/// Lifecycle states from RFC §4. String-typed in SQLite for forward-compat
/// (a future state added by a newer binary still round-trips through an
/// older reader as an unrecognized variant; the schema-version gate is
/// what stops a writer from acting on it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Pending,
    Ready,
    Assigned,
    Running,
    Verifying,
    Done,
    Retrying,
    Resuming,
    NeedsHuman,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Ready => "READY",
            Self::Assigned => "ASSIGNED",
            Self::Running => "RUNNING",
            Self::Verifying => "VERIFYING",
            Self::Done => "DONE",
            Self::Retrying => "RETRYING",
            Self::Resuming => "RESUMING",
            Self::NeedsHuman => "NEEDS_HUMAN",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PENDING" => Some(Self::Pending),
            "READY" => Some(Self::Ready),
            "ASSIGNED" => Some(Self::Assigned),
            "RUNNING" => Some(Self::Running),
            "VERIFYING" => Some(Self::Verifying),
            "DONE" => Some(Self::Done),
            "RETRYING" => Some(Self::Retrying),
            "RESUMING" => Some(Self::Resuming),
            "NEEDS_HUMAN" => Some(Self::NeedsHuman),
            "CANCELLED" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Terminal states delete the per-session policy file and never get
    /// woken again by the reconciler.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::NeedsHuman | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub name: String,
    pub state: TaskState,
    pub role: Option<String>,
    pub cwd: String,
    pub prompt: String,
    pub model: Option<String>,
    pub budget_usd: Option<f64>,
    pub max_retries: u32,
    pub timeout_min: u32,
    pub depends_on: Vec<String>,
    /// Per-task policy JSON. Whatever the supervisor wrote at `ASSIGNED`
    /// time is what the brain-gate hook reads from
    /// `~/.claudectl/coord/session-policy/<session>.json`.
    pub policy: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

/// Args for creating a task. Splitting this from the full row keeps the
/// caller from having to invent state / id / timestamps.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub name: String,
    pub role: Option<String>,
    pub cwd: String,
    pub prompt: String,
    pub model: Option<String>,
    pub budget_usd: Option<f64>,
    pub max_retries: Option<u32>,
    pub timeout_min: Option<u32>,
    pub depends_on: Vec<String>,
    pub policy: Option<serde_json::Value>,
}

pub fn insert_task(conn: &Connection, t: &NewTask) -> Result<String, String> {
    let id = gen_id("task");
    let now = now();
    let depends_on = serde_json::to_string(&t.depends_on).map_err(|e| format!("json: {e}"))?;
    let policy = t
        .policy
        .as_ref()
        .map(|v| serde_json::to_string(v).map_err(|e| format!("policy json: {e}")))
        .transpose()?;
    conn.execute(
        "INSERT INTO tasks (id, name, state, role, cwd, prompt, model, budget_usd,
                            max_retries, timeout_min, depends_on, policy,
                            created_at, updated_at)
         VALUES (?1, ?2, 'PENDING', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![
            id,
            t.name,
            t.role,
            t.cwd,
            t.prompt,
            t.model,
            t.budget_usd,
            t.max_retries.unwrap_or(2),
            t.timeout_min.unwrap_or(45),
            depends_on,
            policy,
            now,
        ],
    )
    .map_err(|e| format!("insert task: {e}"))?;
    // Initial transition log entry — null `from_state` would break the NOT NULL
    // constraint, so we write the synthetic 'CREATED' bookmark.
    log_transition(
        conn,
        &id,
        "CREATED",
        TaskState::Pending.as_str(),
        "submitted",
    )?;
    Ok(id)
}

pub fn get_task(conn: &Connection, id: &str) -> Result<Option<TaskRow>, String> {
    conn.query_row(
        "SELECT id, name, state, role, cwd, prompt, model, budget_usd,
                max_retries, timeout_min, depends_on, policy,
                created_at, updated_at
         FROM tasks WHERE id = ?1",
        params![id],
        row_to_task,
    )
    .optional()
    .map_err(|e| format!("get task: {e}"))
}

pub fn list_tasks(conn: &Connection, state: Option<TaskState>) -> Result<Vec<TaskRow>, String> {
    let sql = "SELECT id, name, state, role, cwd, prompt, model, budget_usd,
                      max_retries, timeout_min, depends_on, policy,
                      created_at, updated_at
               FROM tasks
               WHERE (?1 IS NULL OR state = ?1)
               ORDER BY created_at";
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
    let state_str = state.map(|s| s.as_str());
    let rows = stmt
        .query_map(params![state_str], row_to_task)
        .map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

/// Move a task to `new_state` and log the transition. Same transaction so
/// a crash between the UPDATE and the INSERT cannot produce a task whose
/// state has no cause. The reconciler's whole correctness story rests on
/// this invariant.
pub fn transition(
    conn: &mut Connection,
    task_id: &str,
    from_state: TaskState,
    new_state: TaskState,
    cause: &str,
) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| format!("begin tx: {e}"))?;
    let now = now();
    let affected = tx
        .execute(
            "UPDATE tasks SET state = ?1, updated_at = ?2
             WHERE id = ?3 AND state = ?4",
            params![new_state.as_str(), now, task_id, from_state.as_str()],
        )
        .map_err(|e| format!("update task state: {e}"))?;
    if affected == 0 {
        return Err(format!(
            "transition rejected: task {task_id} not in state {from}",
            from = from_state.as_str()
        ));
    }
    tx.execute(
        "INSERT INTO task_transitions (task_id, from_state, to_state, cause, at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![task_id, from_state.as_str(), new_state.as_str(), cause, now],
    )
    .map_err(|e| format!("log transition: {e}"))?;
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(())
}

/// Append-only log used by `insert_task` (CREATED bookmark) and tests.
/// Production transitions go through `transition()` which writes both
/// halves in one transaction.
fn log_transition(
    conn: &Connection,
    task_id: &str,
    from_state: &str,
    to_state: &str,
    cause: &str,
) -> Result<(), String> {
    let now = now();
    conn.execute(
        "INSERT INTO task_transitions (task_id, from_state, to_state, cause, at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![task_id, from_state, to_state, cause, now],
    )
    .map_err(|e| format!("log transition: {e}"))?;
    Ok(())
}

pub fn list_transitions(
    conn: &Connection,
    task_id: &str,
) -> Result<Vec<(String, String, String, String)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT from_state, to_state, cause, at FROM task_transitions
             WHERE task_id = ?1 ORDER BY id",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map(params![task_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    let state_str: String = row.get(2)?;
    let depends_on_str: String = row.get(10)?;
    let policy_str: Option<String> = row.get(11)?;
    Ok(TaskRow {
        id: row.get(0)?,
        name: row.get(1)?,
        state: TaskState::parse(&state_str).unwrap_or(TaskState::Pending),
        role: row.get(3)?,
        cwd: row.get(4)?,
        prompt: row.get(5)?,
        model: row.get(6)?,
        budget_usd: row.get(7)?,
        max_retries: row.get::<_, i64>(8)? as u32,
        timeout_min: row.get::<_, i64>(9)? as u32,
        depends_on: serde_json::from_str(&depends_on_str).unwrap_or_default(),
        policy: policy_str.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn now() -> String {
    crate::logger::timestamp_now()
}

/// Test-only access to `store::open_memory` so suite tests can spin up an
/// isolated DB with the v2 schema applied. Production callers should hit
/// the real `store::open()` path which honors the schema-version gate.
#[cfg(test)]
pub(crate) fn open_memory_for_tests() -> Connection {
    super::store::open_memory()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NewTask {
        NewTask {
            name: "auth-middleware".into(),
            role: Some("backend".into()),
            cwd: "/work/services".into(),
            prompt: "Add JWT middleware".into(),
            model: Some("sonnet".into()),
            budget_usd: Some(3.0),
            max_retries: Some(2),
            timeout_min: Some(45),
            depends_on: vec![],
            policy: None,
        }
    }

    #[test]
    fn insert_and_get_round_trips_defaults() {
        let conn = open_memory_for_tests();
        let id = insert_task(&conn, &sample()).unwrap();
        let got = get_task(&conn, &id).unwrap().expect("task missing");
        assert_eq!(got.name, "auth-middleware");
        assert_eq!(got.state, TaskState::Pending);
        assert_eq!(got.role.as_deref(), Some("backend"));
        assert_eq!(got.max_retries, 2);
        assert_eq!(got.timeout_min, 45);
        // The CREATED bookmark transition is the only history at this point.
        let hist = list_transitions(&conn, &id).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].0, "CREATED");
        assert_eq!(hist[0].1, "PENDING");
    }

    #[test]
    fn transition_atomic_state_and_log() {
        let mut conn = open_memory_for_tests();
        let id = insert_task(&conn, &sample()).unwrap();
        transition(
            &mut conn,
            &id,
            TaskState::Pending,
            TaskState::Ready,
            "deps-resolved",
        )
        .unwrap();
        transition(
            &mut conn,
            &id,
            TaskState::Ready,
            TaskState::Assigned,
            "mailbox-write",
        )
        .unwrap();
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Assigned);
        let hist = list_transitions(&conn, &id).unwrap();
        let causes: Vec<_> = hist.iter().map(|(_, _, c, _)| c.as_str()).collect();
        assert_eq!(causes, vec!["submitted", "deps-resolved", "mailbox-write"]);
    }

    #[test]
    fn transition_rejects_wrong_from_state() {
        let mut conn = open_memory_for_tests();
        let id = insert_task(&conn, &sample()).unwrap();
        // Try to transition Pending → Running directly with wrong from_state.
        let err = transition(&mut conn, &id, TaskState::Ready, TaskState::Running, "bad");
        assert!(err.is_err(), "must reject from_state mismatch");
        // Task state unchanged, no extra transition row written.
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Pending);
        assert_eq!(list_transitions(&conn, &id).unwrap().len(), 1);
    }

    #[test]
    fn list_tasks_filters_by_state() {
        let conn = open_memory_for_tests();
        let _ = insert_task(&conn, &sample()).unwrap();
        let _ = insert_task(
            &conn,
            &NewTask {
                name: "other".into(),
                ..sample()
            },
        )
        .unwrap();
        assert_eq!(list_tasks(&conn, None).unwrap().len(), 2);
        assert_eq!(
            list_tasks(&conn, Some(TaskState::Pending)).unwrap().len(),
            2
        );
        assert!(list_tasks(&conn, Some(TaskState::Done)).unwrap().is_empty());
    }
}

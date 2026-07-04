//! SQLite-backed persistence for the agent bus.
//!
//! Lives in its own `bus.db` (parallel to `coord/coord.db`) so the bus's
//! schema can evolve independently of the coordination/event store. WAL mode
//! makes it safe for the TUI process and every `claudectl bus stdio`
//! subprocess to read/write concurrently.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("bus")
        .join("bus.db")
}

fn now_iso() -> String {
    crate::logger::timestamp_now()
}

pub fn gen_id(prefix: &str) -> String {
    // Nanosecond precision + a process-local counter so two calls inside the
    // same nanosecond (and one process invocation) still differ. Two separate
    // CLI processes started in the same second would otherwise collide on the
    // primary key.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{seq}")
}

pub fn open() -> Result<Connection, String> {
    open_at(&db_path())
}

pub fn open_at(path: &std::path::Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("open bus db: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("WAL mode: {e}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| format!("foreign_keys: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    Ok(conn)
}

#[allow(dead_code)]
pub fn open_memory() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory bus db");
    migrate(&conn).expect("migrate in-memory bus db");
    conn
}

fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS roles (
            role            TEXT PRIMARY KEY,
            cwd_selector    TEXT NOT NULL,
            last_session_id TEXT,
            last_seen       TEXT NOT NULL,
            subscriptions   TEXT NOT NULL DEFAULT '[]',
            created_at      TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_roles_cwd ON roles(cwd_selector);

        -- Migration #2: pid binding (issue #307). Stored as a nullable
        -- INTEGER so older bindings (cwd-only) keep working. The resolver
        -- prefers pid match when the caller's ancestor chain contains a
        -- bound pid. `ADD COLUMN` is idempotent if guarded by a check on
        -- table_info — SQLite has no `ADD COLUMN IF NOT EXISTS`.
        ",
    )?;
    add_column_if_missing(conn, "roles", "pid", "INTEGER")?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_roles_pid ON roles(pid) WHERE pid IS NOT NULL;

        CREATE TABLE IF NOT EXISTS messages (
            id              TEXT PRIMARY KEY,
            subject         TEXT NOT NULL,
            msg_type        TEXT NOT NULL,
            sender_role     TEXT,
            addressed_to    TEXT,
            thread_id       TEXT,
            body            TEXT NOT NULL,
            priority        TEXT NOT NULL DEFAULT 'normal',
            status          TEXT NOT NULL DEFAULT 'pending',
            claimed_by      TEXT,
            created_at      TEXT NOT NULL,
            delivered_at    TEXT,
            acked_at        TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_msg_addr   ON messages(addressed_to, status, created_at);
        CREATE INDEX IF NOT EXISTS idx_msg_subj   ON messages(subject, status, created_at);
        CREATE INDEX IF NOT EXISTS idx_msg_thread ON messages(thread_id);
        ",
    )?;
    // Migration #3: hop count column (#344). Carried per message so the
    // supervisor can refuse to forward beyond `policy::DEFAULT_MAX_HOPS`.
    // Existing rows default to 0; new rows inherit `parent_hop + 1` when
    // they're a forward.
    add_column_if_missing(conn, "messages", "hop_count", "INTEGER NOT NULL DEFAULT 0")
}

// ---------------- Roles ------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRow {
    pub role: String,
    pub cwd_selector: String,
    pub last_session_id: Option<String>,
    pub last_seen: String,
    pub subscriptions: Vec<String>,
    /// Process ID this role is bound to (#307). When the caller's parent
    /// chain contains this pid, the resolver picks this role over any
    /// cwd-inferred match. `None` for cwd-only bindings.
    pub pid: Option<u32>,
}

/// Insert or update a role binding. `pid` is optional: when supplied, the
/// resolver matches the caller's ancestor pids against it; without it the
/// role behaves exactly like the pre-#307 cwd-only binding. Passing `None`
/// on an update keeps the existing pid (so a re-bind that only refreshes
/// `session_id` doesn't clobber a pid set elsewhere).
///
/// Rejects any name listed in `policy::RESERVED_ROLES` (#344). Reserved
/// names are owned by the supervisor subsystem; allowing arbitrary sessions
/// to bind them would let a hostile cwd intercept escalations.
pub fn upsert_role(
    conn: &Connection,
    role: &str,
    cwd_selector: &str,
    session_id: Option<&str>,
    pid: Option<u32>,
) -> Result<(), String> {
    super::policy::validate_role_name(role).map_err(|e| e.to_string())?;
    let now = now_iso();
    conn.execute(
        "INSERT INTO roles(role, cwd_selector, last_session_id, last_seen, created_at, pid)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)
         ON CONFLICT(role) DO UPDATE SET
             cwd_selector = excluded.cwd_selector,
             last_session_id = COALESCE(excluded.last_session_id, roles.last_session_id),
             last_seen = excluded.last_seen,
             pid = COALESCE(excluded.pid, roles.pid)",
        params![role, cwd_selector, session_id, now, pid],
    )
    .map_err(|e| format!("upsert role: {e}"))?;
    Ok(())
}

pub fn list_roles(conn: &Connection) -> Result<Vec<RoleRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT role, cwd_selector, last_session_id, last_seen, subscriptions, pid
             FROM roles ORDER BY role",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map([], row_to_role)
        .map_err(|e| format!("query roles: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

pub fn get_role(conn: &Connection, role: &str) -> Result<Option<RoleRow>, String> {
    conn.query_row(
        "SELECT role, cwd_selector, last_session_id, last_seen, subscriptions, pid
         FROM roles WHERE role = ?1",
        params![role],
        row_to_role,
    )
    .optional()
    .map_err(|e| format!("get role: {e}"))
}

/// Lookup the role bound to `pid`, if any. Used by the resolver to pick a
/// pid-match over a cwd-match (#307).
pub fn get_role_by_pid(conn: &Connection, pid: u32) -> Result<Option<RoleRow>, String> {
    conn.query_row(
        "SELECT role, cwd_selector, last_session_id, last_seen, subscriptions, pid
         FROM roles WHERE pid = ?1 LIMIT 1",
        params![pid],
        row_to_role,
    )
    .optional()
    .map_err(|e| format!("get role by pid: {e}"))
}

fn row_to_role(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoleRow> {
    let subs: String = row.get(4)?;
    Ok(RoleRow {
        role: row.get(0)?,
        cwd_selector: row.get(1)?,
        last_session_id: row.get(2)?,
        last_seen: row.get(3)?,
        subscriptions: serde_json::from_str(&subs).unwrap_or_default(),
        pid: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
    })
}

/// Idempotent `ALTER TABLE ... ADD COLUMN` — SQLite has no native
/// `IF NOT EXISTS` form here, so we check `pragma_table_info` first.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    type_decl: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(());
        }
    }
    conn.execute_batch(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {type_decl}"
    ))?;
    Ok(())
}

// ---------------- Messages ---------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    pub id: String,
    pub subject: String,
    pub msg_type: String,
    pub sender_role: Option<String>,
    pub addressed_to: Option<String>,
    pub thread_id: Option<String>,
    pub body: String,
    pub priority: String,
    pub status: String,
    pub created_at: String,
    pub delivered_at: Option<String>,
    /// Hop count carried with the message (#344). Each forward bumps this by
    /// one; the publish handler refuses to insert rows above
    /// `policy::DEFAULT_MAX_HOPS`.
    pub hop_count: u32,
}

#[allow(clippy::too_many_arguments)]
pub fn insert_message(
    conn: &Connection,
    subject: &str,
    msg_type: &str,
    sender_role: Option<&str>,
    addressed_to: Option<&str>,
    thread_id: Option<&str>,
    body: &str,
    priority: &str,
    hop_count: u32,
) -> Result<String, String> {
    let id = gen_id("msg");
    let now = now_iso();
    conn.execute(
        "INSERT INTO messages(id, subject, msg_type, sender_role, addressed_to,
                               thread_id, body, priority, status, created_at, hop_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, ?10)",
        params![
            id,
            subject,
            msg_type,
            sender_role,
            addressed_to,
            thread_id,
            body,
            priority,
            now,
            hop_count,
        ],
    )
    .map_err(|e| format!("insert message: {e}"))?;
    Ok(id)
}

/// Shared SQL for both `peek_inbox` and `drain_inbox`. Keeping one query
/// guarantees the two paths never diverge on which rows they consider
/// pending — supervisor exactly-once delivery depends on `peek` returning
/// the same set the next `drain` will hand out.
const INBOX_QUERY: &str = "SELECT id, subject, msg_type, sender_role, addressed_to, thread_id,
                                  body, priority, status, created_at, delivered_at, hop_count
                           FROM messages
                           WHERE addressed_to = ?1
                             AND status = 'pending'
                             AND (?2 IS NULL OR created_at > ?2)
                           ORDER BY
                               CASE priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END,
                               created_at";

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: row.get(0)?,
        subject: row.get(1)?,
        msg_type: row.get(2)?,
        sender_role: row.get(3)?,
        addressed_to: row.get(4)?,
        thread_id: row.get(5)?,
        body: row.get(6)?,
        priority: row.get(7)?,
        status: row.get(8)?,
        created_at: row.get(9)?,
        delivered_at: row.get(10)?,
        hop_count: row.get::<_, i64>(11)? as u32,
    })
}

/// Non-destructive read (#344, RFC v2 §9). Returns the same rows
/// `drain_inbox` would, but leaves their `status = 'pending'` so a
/// subsequent drain still hands them out. The supervisor uses this to
/// decide whether to actuate without committing the messages as
/// delivered until the assignment lands.
pub fn peek_inbox(
    conn: &Connection,
    role: &str,
    since: Option<&str>,
) -> Result<Vec<MessageRow>, String> {
    let mut stmt = conn
        .prepare(INBOX_QUERY)
        .map_err(|e| format!("prepare peek: {e}"))?;
    let rows = stmt
        .query_map(params![role, since], row_to_message)
        .map_err(|e| format!("peek query: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

/// Pending directed messages for `role`, optionally filtered by an ISO
/// timestamp. Marks each returned row as `delivered`.
pub fn drain_inbox(
    conn: &mut Connection,
    role: &str,
    since: Option<&str>,
) -> Result<Vec<MessageRow>, String> {
    let tx = conn.transaction().map_err(|e| format!("begin tx: {e}"))?;
    let rows = {
        let mut stmt = tx
            .prepare(INBOX_QUERY)
            .map_err(|e| format!("prepare drain: {e}"))?;
        let rows = stmt
            .query_map(params![role, since], row_to_message)
            .map_err(|e| format!("drain query: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("row: {e}"))?);
        }
        out
    };
    let now = now_iso();
    for m in &rows {
        tx.execute(
            "UPDATE messages SET status = 'delivered', delivered_at = ?1 WHERE id = ?2",
            params![now, m.id],
        )
        .map_err(|e| format!("mark delivered: {e}"))?;
    }
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(rows)
}

// ---------------- Retention ------------------------------------------------

/// Default retention: 30 days of delivered messages. Matches the coord
/// store's `DEFAULT_RETENTION_DAYS` so operators only need to learn one
/// number.
pub const DEFAULT_RETENTION_DAYS: u64 = 30;

/// Delete delivered messages older than `retention_days`. Pending and
/// acked rows are untouched — we never lose in-flight work or the
/// audit trail of explicit acks. Returns the count deleted (#337).
///
/// Cutoff math mirrors `coord::store::prune` so the two prune paths
/// agree on what "30 days ago" means.
pub fn prune(conn: &Connection, retention_days: Option<u64>) -> Result<u64, String> {
    let days = retention_days.unwrap_or(DEFAULT_RETENTION_DAYS);
    let cutoff = prune_cutoff(days);
    let n = conn
        .execute(
            "DELETE FROM messages WHERE status = 'delivered' AND delivered_at IS NOT NULL AND delivered_at < ?1",
            params![cutoff],
        )
        .map_err(|e| format!("prune messages: {e}"))?;
    Ok(n as u64)
}

/// How many rows `prune` would delete without writing. Used by
/// `bus prune --dry-run` and by `claudectl doctor` when it wants to
/// surface "X stale messages waiting to be pruned" advisories.
pub fn prune_dry_run(conn: &Connection, retention_days: Option<u64>) -> Result<u64, String> {
    let days = retention_days.unwrap_or(DEFAULT_RETENTION_DAYS);
    let cutoff = prune_cutoff(days);
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE status = 'delivered' AND delivered_at IS NOT NULL AND delivered_at < ?1",
            params![cutoff],
            |row| row.get(0),
        )
        .map_err(|e| format!("prune dry-run count: {e}"))?;
    Ok(n as u64)
}

/// Total messages currently in the table. Used by `claudectl doctor`
/// to flag a growing mailbox before it becomes a problem.
pub fn message_count(conn: &Connection) -> Result<u64, String> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .map_err(|e| format!("count messages: {e}"))?;
    Ok(n as u64)
}

fn prune_cutoff(days: u64) -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days * 86400);
    let d = epoch / 86400;
    let (year, month, day) = crate::logger::days_to_date(d);
    format!("{year:04}-{month:02}-{day:02}T00:00:00Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upserts_and_lists_roles() {
        let conn = open_memory();
        upsert_role(&conn, "planner", "/work/proj-plan", Some("sess_a"), None).unwrap();
        upsert_role(&conn, "impl", "/work/proj-impl", Some("sess_b"), None).unwrap();
        let rs = list_roles(&conn).unwrap();
        assert_eq!(rs.len(), 2);
        let names: Vec<_> = rs.iter().map(|r| r.role.as_str()).collect();
        assert!(names.contains(&"planner"));
        assert!(names.contains(&"impl"));
    }

    #[test]
    fn reserved_role_names_are_rejected_at_binding() {
        let conn = open_memory();
        assert!(
            upsert_role(&conn, "supervisor", "/work/anywhere", None, None).is_err(),
            "binding the reserved 'supervisor' name must fail"
        );
        assert!(
            upsert_role(&conn, "operator", "/work/anywhere", None, None).is_err(),
            "binding the reserved 'operator' name must fail"
        );
        // Mixed-case variant — policy::validate_role_name compares
        // case-insensitively.
        assert!(upsert_role(&conn, "Supervisor", "/work/anywhere", None, None).is_err());
        // Empty roles table — no partial binding.
        assert!(list_roles(&conn).unwrap().is_empty());
    }

    #[test]
    fn drains_directed_messages_in_priority_order() {
        let mut conn = open_memory();
        insert_message(
            &conn,
            "task.created",
            "task",
            Some("planner"),
            Some("impl"),
            None,
            "low body",
            "normal",
            0,
        )
        .unwrap();
        insert_message(
            &conn,
            "task.created",
            "task",
            Some("planner"),
            Some("impl"),
            None,
            "urgent body",
            "high",
            0,
        )
        .unwrap();
        // Different recipient — should not be drained.
        insert_message(
            &conn,
            "task.created",
            "task",
            Some("planner"),
            Some("other"),
            None,
            "noise",
            "high",
            0,
        )
        .unwrap();

        let drained = drain_inbox(&mut conn, "impl", None).unwrap();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].body, "urgent body");
        assert_eq!(drained[1].body, "low body");

        // Second drain returns nothing — rows are now 'delivered'.
        let drained2 = drain_inbox(&mut conn, "impl", None).unwrap();
        assert!(drained2.is_empty());
    }

    /// ISO-8601 timestamp for `days` before *now*, derived the same way as
    /// `prune_cutoff`. Age-based prune tests must use this rather than
    /// hardcoded calendar dates — otherwise a fixed "5 days ago" row silently
    /// crosses the retention cutoff as real time passes and breaks the suite.
    fn days_ago(days: u64) -> String {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(days * 86400);
        let d = epoch / 86400;
        let (year, month, day) = crate::logger::days_to_date(d);
        format!("{year:04}-{month:02}-{day:02}T00:00:00Z")
    }

    /// Helper: insert a message with an explicit `delivered_at` so tests can
    /// fabricate "this was delivered N days ago" rows without sleeping.
    fn insert_delivered_at(conn: &Connection, id: &str, delivered_at: &str) {
        conn.execute(
            "INSERT INTO messages(id, subject, msg_type, sender_role, addressed_to,
                                  body, priority, status, created_at, delivered_at)
             VALUES (?1, 'task.created', 'task', 'spec', 'impl',
                     'body', 'normal', 'delivered', ?2, ?2)",
            params![id, delivered_at],
        )
        .unwrap();
    }

    #[test]
    fn prune_deletes_old_delivered_messages_only() {
        let conn = open_memory();
        // 60 days ago — should go.
        insert_delivered_at(&conn, "old", &days_ago(60));
        // 5 days ago — should stay.
        insert_delivered_at(&conn, "fresh", &days_ago(5));
        // Pending row from any era — should always stay.
        insert_message(
            &conn,
            "task.created",
            "task",
            Some("spec"),
            Some("impl"),
            None,
            "pending body",
            "normal",
            0,
        )
        .unwrap();
        assert_eq!(message_count(&conn).unwrap(), 3);
        let deleted = prune(&conn, Some(30)).unwrap();
        assert_eq!(deleted, 1, "only the 60-day-old delivered row should go");
        assert_eq!(message_count(&conn).unwrap(), 2);
    }

    #[test]
    fn prune_dry_run_counts_without_deleting() {
        let conn = open_memory();
        insert_delivered_at(&conn, "a", "2026-01-01T00:00:00Z");
        insert_delivered_at(&conn, "b", "2026-01-02T00:00:00Z");
        assert_eq!(prune_dry_run(&conn, Some(30)).unwrap(), 2);
        assert_eq!(
            message_count(&conn).unwrap(),
            2,
            "dry-run must not delete rows"
        );
    }

    #[test]
    fn prune_with_zero_days_drops_yesterday_and_older() {
        // 0-day retention means the cutoff is today at 00:00:00. Rows
        // with `delivered_at` strictly before that cutoff drop; rows
        // dated exactly today (or in the future) survive. This matches
        // the `<` in the DELETE WHERE clause.
        let conn = open_memory();
        insert_delivered_at(&conn, "yday", &days_ago(1)); // strictly before today → drops
        insert_delivered_at(&conn, "older", &days_ago(200)); // way before → drops
        let deleted = prune(&conn, Some(0)).unwrap();
        assert_eq!(deleted, 2);
    }

    #[test]
    fn prune_on_empty_table_is_a_noop() {
        let conn = open_memory();
        let deleted = prune(&conn, Some(30)).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(message_count(&conn).unwrap(), 0);
        assert_eq!(prune_dry_run(&conn, Some(30)).unwrap(), 0);
    }

    #[test]
    fn message_count_reflects_total_rows() {
        let conn = open_memory();
        assert_eq!(message_count(&conn).unwrap(), 0);
        insert_message(
            &conn,
            "task.created",
            "task",
            Some("a"),
            Some("b"),
            None,
            "body",
            "normal",
            0,
        )
        .unwrap();
        assert_eq!(message_count(&conn).unwrap(), 1);
    }

    #[test]
    fn peek_is_idempotent_and_drain_after_peek_still_works() {
        let mut conn = open_memory();
        insert_message(
            &conn,
            "task.created",
            "task",
            Some("spec"),
            Some("impl"),
            None,
            "do it",
            "normal",
            0,
        )
        .unwrap();
        let peeked1 = peek_inbox(&conn, "impl", None).unwrap();
        let peeked2 = peek_inbox(&conn, "impl", None).unwrap();
        assert_eq!(peeked1.len(), 1);
        assert_eq!(peeked2.len(), 1);
        assert_eq!(peeked1[0].id, peeked2[0].id);
        // Drain after peek still hands the message out — peek must not have
        // mutated status.
        let drained = drain_inbox(&mut conn, "impl", None).unwrap();
        assert_eq!(drained.len(), 1);
        // Second drain is empty — drain *did* mutate.
        let drained2 = drain_inbox(&mut conn, "impl", None).unwrap();
        assert!(drained2.is_empty());
        // Peek after drain is also empty.
        let peeked3 = peek_inbox(&conn, "impl", None).unwrap();
        assert!(peeked3.is_empty());
    }

    #[test]
    fn hop_count_round_trips_through_store() {
        let mut conn = open_memory();
        insert_message(
            &conn,
            "task.assigned",
            "task",
            Some("supervisor"),
            Some("impl"),
            None,
            "body",
            "normal",
            3,
        )
        .unwrap();
        let drained = drain_inbox(&mut conn, "impl", None).unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].hop_count, 3);
    }
}

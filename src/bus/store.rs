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
    )
}

// ---------------- Roles ------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRow {
    pub role: String,
    pub cwd_selector: String,
    pub last_session_id: Option<String>,
    pub last_seen: String,
    pub subscriptions: Vec<String>,
}

pub fn upsert_role(
    conn: &Connection,
    role: &str,
    cwd_selector: &str,
    session_id: Option<&str>,
) -> Result<(), String> {
    let now = now_iso();
    conn.execute(
        "INSERT INTO roles(role, cwd_selector, last_session_id, last_seen, created_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(role) DO UPDATE SET
             cwd_selector = excluded.cwd_selector,
             last_session_id = COALESCE(excluded.last_session_id, roles.last_session_id),
             last_seen = excluded.last_seen",
        params![role, cwd_selector, session_id, now],
    )
    .map_err(|e| format!("upsert role: {e}"))?;
    Ok(())
}

pub fn list_roles(conn: &Connection) -> Result<Vec<RoleRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT role, cwd_selector, last_session_id, last_seen, subscriptions
             FROM roles ORDER BY role",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let subs: String = row.get(4)?;
            Ok(RoleRow {
                role: row.get(0)?,
                cwd_selector: row.get(1)?,
                last_session_id: row.get(2)?,
                last_seen: row.get(3)?,
                subscriptions: serde_json::from_str(&subs).unwrap_or_default(),
            })
        })
        .map_err(|e| format!("query roles: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

pub fn get_role(conn: &Connection, role: &str) -> Result<Option<RoleRow>, String> {
    conn.query_row(
        "SELECT role, cwd_selector, last_session_id, last_seen, subscriptions
         FROM roles WHERE role = ?1",
        params![role],
        |row| {
            let subs: String = row.get(4)?;
            Ok(RoleRow {
                role: row.get(0)?,
                cwd_selector: row.get(1)?,
                last_session_id: row.get(2)?,
                last_seen: row.get(3)?,
                subscriptions: serde_json::from_str(&subs).unwrap_or_default(),
            })
        },
    )
    .optional()
    .map_err(|e| format!("get role: {e}"))
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
) -> Result<String, String> {
    let id = gen_id("msg");
    let now = now_iso();
    conn.execute(
        "INSERT INTO messages(id, subject, msg_type, sender_role, addressed_to,
                               thread_id, body, priority, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)",
        params![
            id,
            subject,
            msg_type,
            sender_role,
            addressed_to,
            thread_id,
            body,
            priority,
            now
        ],
    )
    .map_err(|e| format!("insert message: {e}"))?;
    Ok(id)
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
            .prepare(
                "SELECT id, subject, msg_type, sender_role, addressed_to, thread_id,
                        body, priority, status, created_at, delivered_at
                 FROM messages
                 WHERE addressed_to = ?1
                   AND status = 'pending'
                   AND (?2 IS NULL OR created_at > ?2)
                 ORDER BY
                     CASE priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END,
                     created_at",
            )
            .map_err(|e| format!("prepare drain: {e}"))?;
        let rows = stmt
            .query_map(params![role, since], |row| {
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
                })
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upserts_and_lists_roles() {
        let mut conn = open_memory();
        upsert_role(&mut conn, "planner", "/work/proj-plan", Some("sess_a")).unwrap();
        upsert_role(&mut conn, "impl", "/work/proj-impl", Some("sess_b")).unwrap();
        let rs = list_roles(&conn).unwrap();
        assert_eq!(rs.len(), 2);
        let names: Vec<_> = rs.iter().map(|r| r.role.as_str()).collect();
        assert!(names.contains(&"planner"));
        assert!(names.contains(&"impl"));
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
}

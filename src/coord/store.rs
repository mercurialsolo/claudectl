#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, params};

use super::types::*;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("coord")
        .join("coord.db")
}

fn now_iso() -> String {
    crate::logger::timestamp_now()
}

fn iso_after_secs(secs_from_now: u64) -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_add(secs_from_now);
    let d = epoch / 86400;
    let s = epoch % 86400;
    let (y, m, day) = crate::logger::days_to_date(d);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        (s % 3600) / 60,
        s % 60
    )
}

/// Generate a simple unique ID: `{prefix}_{epoch_secs}_{counter}`.
pub fn gen_id(prefix: &str) -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{epoch}_{seq}")
}

/// Schema version the running binary expects to see in
/// `PRAGMA user_version`. Bumped whenever `migrate()` adds a column or
/// table that older binaries don't know about.
///
/// `migrate()` always brings older DBs *forward* to this version. The
/// version gate (`check_schema_version`) only ever rejects the reverse
/// case: a DB at `user_version > EXPECTED` — meaning a newer binary
/// ran first and bumped the schema, and now an older binary is being
/// pointed at it (downgrade after `brew upgrade`, or two binaries on
/// `$PATH` at once). Without the gate, the older binary would happily
/// write rows against a schema it doesn't understand — the manual-upgrade
/// gap RFC v2 §12 calls out as worse than loudly refusing.
///
/// v1 = baseline (lease/blocker/handoff/interrupt/memory tables).
/// v2 = supervisor tables (#345): tasks, task_attempts,
///       task_verifications, task_transitions, hook_events.
pub const EXPECTED_COORD_SCHEMA_VERSION: u32 = 2;

/// Open (or create) the coordination database and run migrations.
pub fn open() -> Result<Connection, String> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let conn = Connection::open(&path).map_err(|e| format!("open db: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("WAL mode: {e}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| format!("foreign_keys: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    check_schema_version(&conn)?;
    Ok(conn)
}

/// Open an in-memory database (for testing and evals).
pub fn open_memory() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    check_schema_version(&conn).expect("in-memory schema version gate");
    conn
}

/// Refuse to proceed if the DB's `user_version` is *ahead* of the binary's
/// `EXPECTED_COORD_SCHEMA_VERSION`. Equal or behind is fine — `migrate()`
/// would have brought a behind-DB forward already.
pub fn check_schema_version(conn: &Connection) -> Result<(), String> {
    let actual: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .map_err(|e| format!("read user_version: {e}"))?;
    if actual > EXPECTED_COORD_SCHEMA_VERSION {
        return Err(format!(
            "coord DB schema at v{actual} but this binary expects v{expected}. \
             A newer claudectl initialized this DB. Upgrade to that version, or \
             run `claudectl init --upgrade` after upgrading the binary.",
            expected = EXPECTED_COORD_SCHEMA_VERSION
        ));
    }
    Ok(())
}

fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "
        -- Append-only event log
        CREATE TABLE IF NOT EXISTS events (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type  TEXT NOT NULL,
            timestamp   TEXT NOT NULL,
            session_id  TEXT,
            payload     TEXT NOT NULL,
            created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
        CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
        CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);

        -- Materialized lease state
        CREATE TABLE IF NOT EXISTS leases (
            id                TEXT PRIMARY KEY,
            owner_session_id  TEXT NOT NULL,
            owner_agent       TEXT NOT NULL DEFAULT 'claude-code',
            resource_kind     TEXT NOT NULL,
            resource_value    TEXT NOT NULL,
            mode              TEXT NOT NULL,
            reason            TEXT NOT NULL DEFAULT '',
            acquired_at       TEXT NOT NULL,
            expires_at        TEXT,
            status            TEXT NOT NULL DEFAULT 'active'
        );
        CREATE INDEX IF NOT EXISTS idx_leases_status ON leases(status);
        CREATE INDEX IF NOT EXISTS idx_leases_resource ON leases(resource_kind, resource_value);

        -- Materialized blocker state
        CREATE TABLE IF NOT EXISTS blockers (
            id                TEXT PRIMARY KEY,
            task_id           TEXT NOT NULL,
            depends_on        TEXT,
            waiting_for       TEXT NOT NULL,
            status            TEXT NOT NULL DEFAULT 'open',
            owner_session_id  TEXT NOT NULL,
            created_at        TEXT NOT NULL,
            resolved_at       TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_blockers_status ON blockers(status);

        -- Materialized handoff state
        CREATE TABLE IF NOT EXISTS handoffs (
            id                TEXT PRIMARY KEY,
            from_session_id   TEXT NOT NULL,
            to_session_id     TEXT,
            task_id           TEXT NOT NULL,
            summary           TEXT NOT NULL,
            state_json        TEXT NOT NULL,
            priority          TEXT NOT NULL DEFAULT 'medium',
            created_at        TEXT NOT NULL,
            acknowledged_at   TEXT
        );

        -- Materialized interrupt state
        CREATE TABLE IF NOT EXISTS interrupts (
            id                TEXT PRIMARY KEY,
            interrupt_type    TEXT NOT NULL,
            priority          TEXT NOT NULL,
            target_session_id TEXT NOT NULL,
            reason            TEXT NOT NULL,
            payload_json      TEXT,
            delivery_mode     TEXT NOT NULL DEFAULT 'safe_boundary',
            max_retries       INTEGER NOT NULL DEFAULT 3,
            retry_count       INTEGER NOT NULL DEFAULT 0,
            next_retry_at     TEXT,
            expires_at        TEXT,
            dedupe_key        TEXT,
            state             TEXT NOT NULL DEFAULT 'pending',
            created_at        TEXT NOT NULL,
            delivered_at      TEXT,
            acknowledged_at   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_interrupts_state ON interrupts(state);
        CREATE INDEX IF NOT EXISTS idx_interrupts_target ON interrupts(target_session_id);
        CREATE INDEX IF NOT EXISTS idx_interrupts_dedupe ON interrupts(dedupe_key);

        -- Memory records
        CREATE TABLE IF NOT EXISTS memory (
            id          TEXT PRIMARY KEY,
            mem_type    TEXT NOT NULL,
            scope_json  TEXT NOT NULL,
            subjects    TEXT NOT NULL,
            summary     TEXT NOT NULL,
            evidence    TEXT,
            source_json TEXT,
            confidence  REAL NOT NULL DEFAULT 0.5,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            expires_at  TEXT,
            tags        TEXT NOT NULL DEFAULT '[]'
        );
        ",
    )?;

    ensure_column(
        conn,
        "interrupts",
        "retry_count",
        "retry_count INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(conn, "interrupts", "next_retry_at", "next_retry_at TEXT")?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_interrupts_retry ON interrupts(state, next_retry_at)",
        [],
    )?;

    // FTS5 virtual table -- CREATE VIRTUAL TABLE IF NOT EXISTS is supported
    conn.execute_batch(
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
            summary,
            tags,
            content=memory,
            content_rowid=rowid
        );
        ",
    )?;

    // FTS sync triggers -- use INSERT OR IGNORE pattern via a version check
    // We create triggers only if they don't already exist by wrapping in a check
    let trigger_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='trigger' AND name='memory_ai'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !trigger_exists {
        conn.execute_batch(
            "
            CREATE TRIGGER memory_ai AFTER INSERT ON memory BEGIN
                INSERT INTO memory_fts(rowid, summary, tags)
                VALUES (new.rowid, new.summary, new.tags);
            END;

            CREATE TRIGGER memory_ad AFTER DELETE ON memory BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, summary, tags)
                VALUES ('delete', old.rowid, old.summary, old.tags);
            END;

            CREATE TRIGGER memory_au AFTER UPDATE ON memory BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, summary, tags)
                VALUES ('delete', old.rowid, old.summary, old.tags);
                INSERT INTO memory_fts(rowid, summary, tags)
                VALUES (new.rowid, new.summary, new.tags);
            END;
            ",
        )?;
    }

    // Migration to schema v2 — supervisor tables (#345). Idempotent: every
    // statement uses `IF NOT EXISTS` so re-running on a DB already at v2 is
    // a no-op. The pre-existing TUI / brain code paths do not touch these.
    conn.execute_batch(
        "
        -- Desired task state. One row per task; cattle-vs-pets: the task,
        -- not the session, carries identity, budget, attempt count.
        CREATE TABLE IF NOT EXISTS tasks (
            id            TEXT PRIMARY KEY,
            name          TEXT NOT NULL,
            state         TEXT NOT NULL,
            role          TEXT,
            cwd           TEXT NOT NULL,
            prompt        TEXT NOT NULL,
            model         TEXT,
            budget_usd    REAL,
            max_retries   INTEGER NOT NULL DEFAULT 2,
            timeout_min   INTEGER NOT NULL DEFAULT 45,
            depends_on    TEXT NOT NULL DEFAULT '[]',
            policy        TEXT,
            created_at    TEXT NOT NULL,
            updated_at    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state);
        CREATE INDEX IF NOT EXISTS idx_tasks_role  ON tasks(role) WHERE role IS NOT NULL;

        -- One row per attempt at a task.
        CREATE TABLE IF NOT EXISTS task_attempts (
            id              TEXT PRIMARY KEY,
            task_id         TEXT NOT NULL REFERENCES tasks(id),
            attempt_num     INTEGER NOT NULL,
            session_id      TEXT,
            bus_message_id  TEXT,
            cwd_hash        TEXT,
            started_at      TEXT NOT NULL,
            ended_at        TEXT,
            cost_usd        REAL NOT NULL DEFAULT 0,
            outcome         TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_attempts_task    ON task_attempts(task_id);
        CREATE INDEX IF NOT EXISTS idx_attempts_session ON task_attempts(session_id) WHERE session_id IS NOT NULL;

        -- Verifier verdicts. Retry-prompt builder reads these to compose
        -- 'previous attempt failed for these reasons' feedback.
        CREATE TABLE IF NOT EXISTS task_verifications (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            attempt_id  TEXT NOT NULL REFERENCES task_attempts(id),
            kind        TEXT NOT NULL,
            command     TEXT NOT NULL,
            verdict     TEXT NOT NULL,
            output      TEXT,
            cost_usd    REAL NOT NULL DEFAULT 0,
            ran_at      TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_verifications_attempt ON task_verifications(attempt_id);

        -- Append-only ledger of state-machine moves. Crash recovery is
        -- 'rebuild observed state by replaying transitions.'
        CREATE TABLE IF NOT EXISTS task_transitions (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id     TEXT NOT NULL REFERENCES tasks(id),
            from_state  TEXT NOT NULL,
            to_state    TEXT NOT NULL,
            cause       TEXT NOT NULL,
            at          TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_transitions_task ON task_transitions(task_id);

        -- Hook event ingestion (RFC v2 §6). Hooks push payloads here via
        -- `claudectl ingest` so the reconciler reacts in one tick instead
        -- of waiting on file-watch debounce. JSONL tail stays authoritative
        -- — this table is best-effort by construction (`|| true` in hook
        -- commands), which is why it cannot be the source of record.
        -- Name distinguishes it from the pre-existing coord `events` table.
        CREATE TABLE IF NOT EXISTS hook_events (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            hook         TEXT NOT NULL,
            session_id   TEXT,
            tool         TEXT,
            payload      TEXT NOT NULL,
            ingested_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_hook_events_session  ON hook_events(session_id, ingested_at);
        CREATE INDEX IF NOT EXISTS idx_hook_events_ingested ON hook_events(ingested_at);
        ",
    )?;

    // Bump `user_version` last — table creation runs first so a partially
    // migrated DB never gets the version stamp; the gate then refuses to
    // start if anything drifted.
    conn.pragma_update(None, "user_version", EXPECTED_COORD_SCHEMA_VERSION)?;

    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    column_def: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;

    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }

    conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column_def}"), [])?;
    Ok(())
}

// -- Events --------------------------------------------------------------------

pub fn append_event(conn: &Connection, event: &CoordEvent) -> Result<i64, String> {
    let payload_str = serde_json::to_string(&event.payload).map_err(|e| format!("json: {e}"))?;
    conn.execute(
        "INSERT INTO events (event_type, timestamp, session_id, payload)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            event.event_type.as_str(),
            event.timestamp,
            event.session_id,
            payload_str,
        ],
    )
    .map_err(|e| format!("insert event: {e}"))?;
    Ok(conn.last_insert_rowid())
}

pub fn query_events(
    conn: &Connection,
    limit: usize,
    event_type: Option<&str>,
) -> Result<Vec<CoordEvent>, String> {
    let sql = if event_type.is_some() {
        "SELECT id, event_type, timestamp, session_id, payload
         FROM events WHERE event_type = ?1
         ORDER BY id DESC LIMIT ?2"
    } else {
        "SELECT id, event_type, timestamp, session_id, payload
         FROM events ORDER BY id DESC LIMIT ?1"
    };

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;

    let rows = if let Some(et) = event_type {
        stmt.query_map(params![et, limit], row_to_event)
    } else {
        stmt.query_map(params![limit], row_to_event)
    }
    .map_err(|e| format!("query: {e}"))?;

    let mut events = Vec::new();
    for row in rows {
        events.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(events)
}

fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<CoordEvent> {
    let et_str: String = row.get(1)?;
    let payload_str: String = row.get(4)?;
    Ok(CoordEvent {
        id: Some(row.get(0)?),
        event_type: EventType::parse(&et_str).unwrap_or(EventType::SessionObserved),
        timestamp: row.get(2)?,
        session_id: row.get(3)?,
        payload: serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null),
    })
}

/// Count events by type, optionally filtered to events after `since` timestamp.
pub fn count_events_by_type(
    conn: &Connection,
    since: Option<&str>,
) -> Result<Vec<(String, u64)>, String> {
    let sql = if since.is_some() {
        "SELECT event_type, COUNT(*) FROM events WHERE timestamp >= ?1 GROUP BY event_type ORDER BY COUNT(*) DESC"
    } else {
        "SELECT event_type, COUNT(*) FROM events GROUP BY event_type ORDER BY COUNT(*) DESC"
    };

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
    let mut counts = Vec::new();

    let mapper =
        |row: &rusqlite::Row| -> rusqlite::Result<(String, i64)> { Ok((row.get(0)?, row.get(1)?)) };

    if let Some(ts) = since {
        let rows = stmt
            .query_map(params![ts], mapper)
            .map_err(|e| format!("query: {e}"))?;
        for row in rows {
            let (t, c) = row.map_err(|e| format!("row: {e}"))?;
            counts.push((t, c as u64));
        }
    } else {
        let rows = stmt
            .query_map([], mapper)
            .map_err(|e| format!("query: {e}"))?;
        for row in rows {
            let (t, c) = row.map_err(|e| format!("row: {e}"))?;
            counts.push((t, c as u64));
        }
    }

    Ok(counts)
}

/// Query events within a time window, optionally filtered by type.
pub fn query_events_since(
    conn: &Connection,
    since: &str,
    event_type: Option<&str>,
    limit: usize,
) -> Result<Vec<CoordEvent>, String> {
    let sql = if event_type.is_some() {
        "SELECT id, event_type, timestamp, session_id, payload
         FROM events WHERE timestamp >= ?1 AND event_type = ?2
         ORDER BY id DESC LIMIT ?3"
    } else {
        "SELECT id, event_type, timestamp, session_id, payload
         FROM events WHERE timestamp >= ?1
         ORDER BY id DESC LIMIT ?2"
    };

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
    let rows = if let Some(et) = event_type {
        stmt.query_map(params![since, et, limit], row_to_event)
    } else {
        stmt.query_map(params![since, limit], row_to_event)
    }
    .map_err(|e| format!("query: {e}"))?;

    let mut events = Vec::new();
    for row in rows {
        events.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(events)
}

// -- Leases --------------------------------------------------------------------

pub fn upsert_lease(conn: &Connection, lease: &Lease) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO leases
         (id, owner_session_id, owner_agent, resource_kind, resource_value,
          mode, reason, acquired_at, expires_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            lease.id,
            lease.owner_session_id,
            lease.owner_agent,
            lease.resource_kind,
            lease.resource_value,
            lease.mode.as_str(),
            lease.reason,
            lease.acquired_at,
            lease.expires_at,
            lease.status.as_str(),
        ],
    )
    .map_err(|e| format!("upsert lease: {e}"))?;
    Ok(())
}

pub fn list_leases(conn: &Connection, status: Option<LeaseStatus>) -> Result<Vec<Lease>, String> {
    let sql = if status.is_some() {
        "SELECT id, owner_session_id, owner_agent, resource_kind, resource_value,
                mode, reason, acquired_at, expires_at, status
         FROM leases WHERE status = ?1 ORDER BY acquired_at DESC"
    } else {
        "SELECT id, owner_session_id, owner_agent, resource_kind, resource_value,
                mode, reason, acquired_at, expires_at, status
         FROM leases ORDER BY acquired_at DESC"
    };

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;

    let rows = if let Some(st) = status {
        stmt.query_map(params![st.as_str()], row_to_lease)
    } else {
        stmt.query_map([], row_to_lease)
    }
    .map_err(|e| format!("query: {e}"))?;

    let mut leases = Vec::new();
    for row in rows {
        leases.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(leases)
}

fn row_to_lease(row: &rusqlite::Row) -> rusqlite::Result<Lease> {
    let mode_str: String = row.get(5)?;
    let status_str: String = row.get(9)?;
    Ok(Lease {
        id: row.get(0)?,
        owner_session_id: row.get(1)?,
        owner_agent: row.get(2)?,
        resource_kind: row.get(3)?,
        resource_value: row.get(4)?,
        mode: LeaseMode::parse(&mode_str).unwrap_or(LeaseMode::Advisory),
        reason: row.get(6)?,
        acquired_at: row.get(7)?,
        expires_at: row.get(8)?,
        status: LeaseStatus::parse(&status_str).unwrap_or(LeaseStatus::Active),
    })
}

pub fn expire_stale_leases(conn: &Connection) -> Result<u64, String> {
    let now = now_iso();
    let count = conn
        .execute(
            "UPDATE leases SET status = 'expired'
             WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at < ?1",
            params![now],
        )
        .map_err(|e| format!("expire leases: {e}"))?;
    Ok(count as u64)
}

pub fn get_lease(conn: &Connection, lease_id: &str) -> Result<Option<Lease>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, owner_session_id, owner_agent, resource_kind, resource_value,
                    mode, reason, acquired_at, expires_at, status
             FROM leases WHERE id = ?1",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let mut rows = stmt
        .query_map(params![lease_id], row_to_lease)
        .map_err(|e| format!("query: {e}"))?;

    match rows.next() {
        Some(Ok(lease)) => Ok(Some(lease)),
        Some(Err(e)) => Err(format!("row: {e}")),
        None => Ok(None),
    }
}

/// Find an active exclusive lease that conflicts with the given resource.
/// Handles path overlap: `src/**` conflicts with `src/app.rs`, and vice versa.
pub fn find_conflicting_lease(
    conn: &Connection,
    resource_kind: &str,
    resource_value: &str,
    exclude_session: &str,
) -> Result<Option<Lease>, String> {
    // Query all active exclusive leases by other sessions in the same resource kind
    let mut stmt = conn
        .prepare(
            "SELECT id, owner_session_id, owner_agent, resource_kind, resource_value,
                    mode, reason, acquired_at, expires_at, status
             FROM leases
             WHERE resource_kind = ?1
               AND status = 'active' AND mode = 'exclusive'
               AND owner_session_id != ?2",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let rows = stmt
        .query_map(params![resource_kind, exclude_session], row_to_lease)
        .map_err(|e| format!("query: {e}"))?;

    for row in rows {
        let lease = row.map_err(|e| format!("row: {e}"))?;
        if paths_overlap(&lease.resource_value, resource_value) {
            return Ok(Some(lease));
        }
    }
    Ok(None)
}

/// Check if two path patterns overlap.
/// A glob like `src/**` overlaps with `src/app.rs`.
/// A specific path `src/app.rs` overlaps with `src/**` or `src/app.rs`.
fn paths_overlap(existing: &str, requested: &str) -> bool {
    // Exact match
    if existing == requested {
        return true;
    }
    // Match-all glob overlaps with everything
    if existing == "**" || requested == "**" {
        return true;
    }
    // Glob: existing is a prefix pattern (ends with ** or /*)
    let existing_dir = existing
        .trim_end_matches("**")
        .trim_end_matches('*')
        .trim_end_matches('/');
    let requested_dir = requested
        .trim_end_matches("**")
        .trim_end_matches('*')
        .trim_end_matches('/');

    // One is a prefix of the other (directory containment)
    if !existing_dir.is_empty() && requested.starts_with(existing_dir) {
        return true;
    }
    if !requested_dir.is_empty() && existing.starts_with(requested_dir) {
        return true;
    }
    false
}

/// Atomically check for conflicts and claim a lease in a single transaction.
/// Returns Ok(None) on success, Ok(Some(conflict)) if a conflicting lease exists.
pub fn claim_lease_atomic(conn: &Connection, lease: &Lease) -> Result<Option<Lease>, String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin tx: {e}"))?;

    // Check for conflicts within the transaction
    if lease.mode == LeaseMode::Exclusive {
        let conflict = find_conflicting_lease(
            &tx,
            &lease.resource_kind,
            &lease.resource_value,
            &lease.owner_session_id,
        )?;
        if let Some(c) = conflict {
            tx.rollback().map_err(|e| format!("rollback: {e}"))?;
            return Ok(Some(c));
        }
    }

    // No conflict -- insert the lease
    tx.execute(
        "INSERT OR REPLACE INTO leases
         (id, owner_session_id, owner_agent, resource_kind, resource_value,
          mode, reason, acquired_at, expires_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            lease.id,
            lease.owner_session_id,
            lease.owner_agent,
            lease.resource_kind,
            lease.resource_value,
            lease.mode.as_str(),
            lease.reason,
            lease.acquired_at,
            lease.expires_at,
            lease.status.as_str(),
        ],
    )
    .map_err(|e| format!("insert lease: {e}"))?;

    tx.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(None)
}

pub fn release_lease(conn: &Connection, lease_id: &str) -> Result<bool, String> {
    conn.execute(
        "UPDATE leases SET status = 'released' WHERE id = ?1 AND status = 'active'",
        params![lease_id],
    )
    .map_err(|e| format!("release lease: {e}"))?;
    Ok(conn.changes() > 0)
}

pub fn list_leases_for_session(conn: &Connection, session_id: &str) -> Result<Vec<Lease>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, owner_session_id, owner_agent, resource_kind, resource_value,
                    mode, reason, acquired_at, expires_at, status
             FROM leases WHERE owner_session_id = ?1 AND status = 'active'
             ORDER BY acquired_at DESC",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let rows = stmt
        .query_map(params![session_id], row_to_lease)
        .map_err(|e| format!("query: {e}"))?;

    let mut leases = Vec::new();
    for row in rows {
        leases.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(leases)
}

pub fn list_pending_handoffs(conn: &Connection) -> Result<Vec<Handoff>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, from_session_id, to_session_id, task_id, summary,
                    state_json, priority, created_at, acknowledged_at
             FROM handoffs WHERE acknowledged_at IS NULL
             ORDER BY created_at DESC",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let rows = stmt
        .query_map([], row_to_handoff)
        .map_err(|e| format!("query: {e}"))?;

    let mut handoffs = Vec::new();
    for row in rows {
        handoffs.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(handoffs)
}

pub fn get_handoff(conn: &Connection, handoff_id: &str) -> Result<Option<Handoff>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, from_session_id, to_session_id, task_id, summary,
                    state_json, priority, created_at, acknowledged_at
             FROM handoffs WHERE id = ?1",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let mut rows = stmt
        .query_map(params![handoff_id], row_to_handoff)
        .map_err(|e| format!("query: {e}"))?;

    match rows.next() {
        Some(Ok(h)) => Ok(Some(h)),
        Some(Err(e)) => Err(format!("row: {e}")),
        None => Ok(None),
    }
}

pub fn accept_handoff(conn: &Connection, handoff_id: &str) -> Result<bool, String> {
    let now = now_iso();
    conn.execute(
        "UPDATE handoffs SET acknowledged_at = ?1 WHERE id = ?2 AND acknowledged_at IS NULL",
        params![now, handoff_id],
    )
    .map_err(|e| format!("accept handoff: {e}"))?;
    Ok(conn.changes() > 0)
}

// -- Blockers ------------------------------------------------------------------

pub fn insert_blocker(conn: &Connection, blocker: &Blocker) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO blockers
         (id, task_id, depends_on, waiting_for, status, owner_session_id, created_at, resolved_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            blocker.id,
            blocker.task_id,
            blocker.depends_on,
            blocker.waiting_for,
            blocker.status.as_str(),
            blocker.owner_session_id,
            blocker.created_at,
            blocker.resolved_at,
        ],
    )
    .map_err(|e| format!("insert blocker: {e}"))?;
    Ok(())
}

pub fn list_blockers(
    conn: &Connection,
    status: Option<BlockerStatus>,
) -> Result<Vec<Blocker>, String> {
    let sql = if status.is_some() {
        "SELECT id, task_id, depends_on, waiting_for, status, owner_session_id,
                created_at, resolved_at
         FROM blockers WHERE status = ?1 ORDER BY created_at DESC"
    } else {
        "SELECT id, task_id, depends_on, waiting_for, status, owner_session_id,
                created_at, resolved_at
         FROM blockers ORDER BY created_at DESC"
    };

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;

    let rows = if let Some(st) = status {
        stmt.query_map(params![st.as_str()], row_to_blocker)
    } else {
        stmt.query_map([], row_to_blocker)
    }
    .map_err(|e| format!("query: {e}"))?;

    let mut blockers = Vec::new();
    for row in rows {
        blockers.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(blockers)
}

fn row_to_blocker(row: &rusqlite::Row) -> rusqlite::Result<Blocker> {
    let status_str: String = row.get(4)?;
    Ok(Blocker {
        id: row.get(0)?,
        task_id: row.get(1)?,
        depends_on: row.get(2)?,
        waiting_for: row.get(3)?,
        status: BlockerStatus::parse(&status_str).unwrap_or(BlockerStatus::Open),
        owner_session_id: row.get(5)?,
        created_at: row.get(6)?,
        resolved_at: row.get(7)?,
    })
}

pub fn resolve_blocker(conn: &Connection, blocker_id: &str) -> Result<(), String> {
    let now = now_iso();
    conn.execute(
        "UPDATE blockers SET status = 'resolved', resolved_at = ?1 WHERE id = ?2",
        params![now, blocker_id],
    )
    .map_err(|e| format!("resolve blocker: {e}"))?;
    Ok(())
}

// -- Handoffs ------------------------------------------------------------------

pub fn insert_handoff(conn: &Connection, handoff: &Handoff) -> Result<(), String> {
    let state_json = serde_json::to_string(&handoff.state).map_err(|e| format!("json: {e}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO handoffs
         (id, from_session_id, to_session_id, task_id, summary, state_json,
          priority, created_at, acknowledged_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            handoff.id,
            handoff.from_session_id,
            handoff.to_session_id,
            handoff.task_id,
            handoff.summary,
            state_json,
            handoff.priority,
            handoff.created_at,
            handoff.acknowledged_at,
        ],
    )
    .map_err(|e| format!("insert handoff: {e}"))?;
    Ok(())
}

pub fn list_handoffs(conn: &Connection) -> Result<Vec<Handoff>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, from_session_id, to_session_id, task_id, summary,
                    state_json, priority, created_at, acknowledged_at
             FROM handoffs ORDER BY created_at DESC",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let rows = stmt
        .query_map([], row_to_handoff)
        .map_err(|e| format!("query: {e}"))?;

    let mut handoffs = Vec::new();
    for row in rows {
        handoffs.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(handoffs)
}

fn row_to_handoff(row: &rusqlite::Row) -> rusqlite::Result<Handoff> {
    let state_str: String = row.get(5)?;
    let state: HandoffState = serde_json::from_str(&state_str).unwrap_or(HandoffState {
        goal: String::new(),
        artifacts: Vec::new(),
        attempted: Vec::new(),
        next_steps: Vec::new(),
    });
    Ok(Handoff {
        id: row.get(0)?,
        from_session_id: row.get(1)?,
        to_session_id: row.get(2)?,
        task_id: row.get(3)?,
        summary: row.get(4)?,
        state,
        priority: row.get(6)?,
        created_at: row.get(7)?,
        acknowledged_at: row.get(8)?,
    })
}

// -- Interrupts ----------------------------------------------------------------

pub fn insert_interrupt(conn: &Connection, interrupt: &Interrupt) -> Result<(), String> {
    let payload_str = interrupt
        .payload
        .as_ref()
        .map(|p| serde_json::to_string(p).unwrap_or_default());
    conn.execute(
        "INSERT OR REPLACE INTO interrupts
         (id, interrupt_type, priority, target_session_id, reason, payload_json,
          delivery_mode, max_retries, retry_count, next_retry_at, expires_at,
          dedupe_key, state, created_at, delivered_at, acknowledged_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            interrupt.id,
            interrupt.interrupt_type.as_str(),
            interrupt.priority,
            interrupt.target_session_id,
            interrupt.reason,
            payload_str,
            interrupt.delivery_mode,
            interrupt.max_retries,
            interrupt.retry_count,
            interrupt.next_retry_at,
            interrupt.expires_at,
            interrupt.dedupe_key,
            interrupt.state.as_str(),
            interrupt.created_at,
            interrupt.delivered_at,
            interrupt.acknowledged_at,
        ],
    )
    .map_err(|e| format!("insert interrupt: {e}"))?;
    Ok(())
}

pub fn list_interrupts(
    conn: &Connection,
    state: Option<InterruptState>,
) -> Result<Vec<Interrupt>, String> {
    let sql = if state.is_some() {
        "SELECT id, interrupt_type, priority, target_session_id, reason, payload_json,
                delivery_mode, max_retries, expires_at, dedupe_key, state,
                created_at, delivered_at, acknowledged_at, retry_count, next_retry_at
         FROM interrupts WHERE state = ?1 ORDER BY created_at DESC"
    } else {
        "SELECT id, interrupt_type, priority, target_session_id, reason, payload_json,
                delivery_mode, max_retries, expires_at, dedupe_key, state,
                created_at, delivered_at, acknowledged_at, retry_count, next_retry_at
         FROM interrupts ORDER BY created_at DESC"
    };

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;

    let rows = if let Some(st) = state {
        stmt.query_map(params![st.as_str()], row_to_interrupt)
    } else {
        stmt.query_map([], row_to_interrupt)
    }
    .map_err(|e| format!("query: {e}"))?;

    let mut interrupts = Vec::new();
    for row in rows {
        interrupts.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(interrupts)
}

fn row_to_interrupt(row: &rusqlite::Row) -> rusqlite::Result<Interrupt> {
    let type_str: String = row.get(1)?;
    let payload_str: Option<String> = row.get(5)?;
    let state_str: String = row.get(10)?;
    Ok(Interrupt {
        id: row.get(0)?,
        interrupt_type: InterruptType::parse(&type_str).unwrap_or(InterruptType::Nudge),
        priority: row.get(2)?,
        target_session_id: row.get(3)?,
        reason: row.get(4)?,
        payload: payload_str.and_then(|s| serde_json::from_str(&s).ok()),
        delivery_mode: row.get(6)?,
        max_retries: row.get::<_, u32>(7)?,
        retry_count: row.get(14)?,
        next_retry_at: row.get(15)?,
        expires_at: row.get(8)?,
        dedupe_key: row.get(9)?,
        state: InterruptState::parse(&state_str).unwrap_or(InterruptState::Pending),
        created_at: row.get(11)?,
        delivered_at: row.get(12)?,
        acknowledged_at: row.get(13)?,
    })
}

pub fn get_interrupt(conn: &Connection, interrupt_id: &str) -> Result<Option<Interrupt>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, interrupt_type, priority, target_session_id, reason, payload_json,
                    delivery_mode, max_retries, expires_at, dedupe_key, state,
                    created_at, delivered_at, acknowledged_at, retry_count, next_retry_at
             FROM interrupts WHERE id = ?1",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let mut rows = stmt
        .query_map(params![interrupt_id], row_to_interrupt)
        .map_err(|e| format!("query: {e}"))?;

    match rows.next() {
        Some(Ok(i)) => Ok(Some(i)),
        Some(Err(e)) => Err(format!("row: {e}")),
        None => Ok(None),
    }
}

/// Pending interrupts eligible for delivery, ordered by priority (critical > high > medium > low) then age.
pub fn list_deliverable_interrupts(conn: &Connection) -> Result<Vec<Interrupt>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, interrupt_type, priority, target_session_id, reason, payload_json,
                    delivery_mode, max_retries, expires_at, dedupe_key, state,
                    created_at, delivered_at, acknowledged_at, retry_count, next_retry_at
             FROM interrupts
             WHERE state = 'pending'
               AND (expires_at IS NULL OR expires_at > ?1)
               AND (next_retry_at IS NULL OR next_retry_at <= ?1)
               AND retry_count < max_retries
             ORDER BY
               CASE priority
                 WHEN 'critical' THEN 0
                 WHEN 'high' THEN 1
                 WHEN 'medium' THEN 2
                 ELSE 3
               END,
               created_at ASC",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let now = now_iso();
    let rows = stmt
        .query_map(params![now], row_to_interrupt)
        .map_err(|e| format!("query: {e}"))?;

    let mut interrupts = Vec::new();
    for row in rows {
        interrupts.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(interrupts)
}

pub fn mark_interrupt_delivered(conn: &Connection, interrupt_id: &str) -> Result<bool, String> {
    let now = now_iso();
    conn.execute(
        "UPDATE interrupts
         SET state = 'delivered', delivered_at = ?1, next_retry_at = NULL
         WHERE id = ?2 AND state = 'pending'",
        params![now, interrupt_id],
    )
    .map_err(|e| format!("mark delivered: {e}"))?;
    Ok(conn.changes() > 0)
}

pub fn record_interrupt_delivery_failure(
    conn: &Connection,
    interrupt_id: &str,
) -> Result<Option<Interrupt>, String> {
    let Some(interrupt) = get_interrupt(conn, interrupt_id)? else {
        return Ok(None);
    };

    if interrupt.state != InterruptState::Pending {
        return Ok(Some(interrupt));
    }

    let retry_count = interrupt.retry_count.saturating_add(1);
    if retry_count >= interrupt.max_retries {
        conn.execute(
            "UPDATE interrupts
             SET retry_count = ?1, state = 'expired', next_retry_at = NULL
             WHERE id = ?2 AND state = 'pending'",
            params![retry_count, interrupt_id],
        )
        .map_err(|e| format!("record delivery failure: {e}"))?;
    } else {
        let next_retry_at = iso_after_secs(retry_backoff_secs(retry_count));
        conn.execute(
            "UPDATE interrupts
             SET retry_count = ?1, next_retry_at = ?2
             WHERE id = ?3 AND state = 'pending'",
            params![retry_count, next_retry_at, interrupt_id],
        )
        .map_err(|e| format!("record delivery failure: {e}"))?;
    }

    get_interrupt(conn, interrupt_id)
}

pub fn expire_exhausted_interrupts(conn: &Connection) -> Result<u64, String> {
    let count = conn
        .execute(
            "UPDATE interrupts
             SET state = 'expired', next_retry_at = NULL
             WHERE state = 'pending' AND retry_count >= max_retries",
            [],
        )
        .map_err(|e| format!("expire exhausted interrupts: {e}"))?;
    Ok(count as u64)
}

fn retry_backoff_secs(retry_count: u32) -> u64 {
    let shift = retry_count.saturating_sub(1).min(4);
    30 * (1u64 << shift)
}

pub fn mark_interrupt_acknowledged(conn: &Connection, interrupt_id: &str) -> Result<bool, String> {
    let now = now_iso();
    conn.execute(
        "UPDATE interrupts SET state = 'acknowledged', acknowledged_at = ?1 WHERE id = ?2 AND state = 'delivered'",
        params![now, interrupt_id],
    )
    .map_err(|e| format!("mark acknowledged: {e}"))?;
    Ok(conn.changes() > 0)
}

pub fn mark_interrupt_expired(conn: &Connection, interrupt_id: &str) -> Result<bool, String> {
    conn.execute(
        "UPDATE interrupts SET state = 'expired' WHERE id = ?1 AND state IN ('pending', 'delivered')",
        params![interrupt_id],
    )
    .map_err(|e| format!("mark expired: {e}"))?;
    Ok(conn.changes() > 0)
}

pub fn expire_stale_interrupts(conn: &Connection) -> Result<u64, String> {
    let now = now_iso();
    let count = conn
        .execute(
            "UPDATE interrupts SET state = 'expired'
             WHERE state IN ('pending', 'delivered')
               AND expires_at IS NOT NULL AND expires_at < ?1",
            params![now],
        )
        .map_err(|e| format!("expire interrupts: {e}"))?;
    Ok(count as u64)
}

pub fn find_duplicate_interrupt(
    conn: &Connection,
    dedupe_key: &str,
) -> Result<Option<Interrupt>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, interrupt_type, priority, target_session_id, reason, payload_json,
                    delivery_mode, max_retries, expires_at, dedupe_key, state,
                    created_at, delivered_at, acknowledged_at, retry_count, next_retry_at
             FROM interrupts
             WHERE dedupe_key = ?1 AND state IN ('pending', 'delivered')
             LIMIT 1",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let mut rows = stmt
        .query_map(params![dedupe_key], row_to_interrupt)
        .map_err(|e| format!("query: {e}"))?;

    match rows.next() {
        Some(Ok(i)) => Ok(Some(i)),
        Some(Err(e)) => Err(format!("row: {e}")),
        None => Ok(None),
    }
}

// -- Memory --------------------------------------------------------------------

pub fn insert_memory(conn: &Connection, record: &MemoryRecord) -> Result<(), String> {
    let scope_str = serde_json::to_string(&record.scope).map_err(|e| format!("json: {e}"))?;
    let subjects_str = serde_json::to_string(&record.subjects).map_err(|e| format!("json: {e}"))?;
    let evidence_str = serde_json::to_string(&record.evidence).map_err(|e| format!("json: {e}"))?;
    let source_str = record
        .source
        .as_ref()
        .map(|s| serde_json::to_string(s).unwrap_or_default());
    let tags_str = serde_json::to_string(&record.tags).map_err(|e| format!("json: {e}"))?;

    conn.execute(
        "INSERT OR REPLACE INTO memory
         (id, mem_type, scope_json, subjects, summary, evidence, source_json,
          confidence, created_at, updated_at, expires_at, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            record.id,
            record.mem_type,
            scope_str,
            subjects_str,
            record.summary,
            evidence_str,
            source_str,
            record.confidence,
            record.created_at,
            record.updated_at,
            record.expires_at,
            tags_str,
        ],
    )
    .map_err(|e| format!("insert memory: {e}"))?;
    Ok(())
}

pub fn search_memory(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<MemoryRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.mem_type, m.scope_json, m.subjects, m.summary,
                    m.evidence, m.source_json, m.confidence, m.created_at,
                    m.updated_at, m.expires_at, m.tags
             FROM memory m
             JOIN memory_fts f ON m.rowid = f.rowid
             WHERE memory_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let rows = stmt
        .query_map(params![query, limit], row_to_memory)
        .map_err(|e| format!("search: {e}"))?;

    let mut records = Vec::new();
    for row in rows {
        records.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(records)
}

pub fn list_memory(conn: &Connection, limit: usize) -> Result<Vec<MemoryRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, mem_type, scope_json, subjects, summary, evidence,
                    source_json, confidence, created_at, updated_at, expires_at, tags
             FROM memory ORDER BY updated_at DESC LIMIT ?1",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let rows = stmt
        .query_map(params![limit], row_to_memory)
        .map_err(|e| format!("query: {e}"))?;

    let mut records = Vec::new();
    for row in rows {
        records.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(records)
}

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<MemoryRecord> {
    let scope_str: String = row.get(2)?;
    let subjects_str: String = row.get(3)?;
    let evidence_str: Option<String> = row.get(5)?;
    let source_str: Option<String> = row.get(6)?;
    let tags_str: String = row.get(11)?;

    Ok(MemoryRecord {
        id: row.get(0)?,
        mem_type: row.get(1)?,
        scope: serde_json::from_str(&scope_str).unwrap_or(serde_json::Value::Null),
        subjects: serde_json::from_str(&subjects_str).unwrap_or_default(),
        summary: row.get(4)?,
        evidence: evidence_str
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        source: source_str.and_then(|s| serde_json::from_str(&s).ok()),
        confidence: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        expires_at: row.get(10)?,
        tags: serde_json::from_str(&tags_str).unwrap_or_default(),
    })
}

// -- Retention -----------------------------------------------------------------

/// Default retention: keep events for 30 days.
const DEFAULT_RETENTION_DAYS: u64 = 30;
/// Default max events to keep (safety cap).
const DEFAULT_MAX_EVENTS: u64 = 100_000;

/// Prune old events, resolved blockers, expired leases, and terminal interrupts.
/// Returns the total number of rows deleted.
pub fn prune(conn: &Connection, retention_days: Option<u64>) -> Result<u64, String> {
    let days = retention_days.unwrap_or(DEFAULT_RETENTION_DAYS);
    let cutoff = {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(days * 86400);
        let d = epoch / 86400;
        let (year, month, day) = crate::logger::days_to_date(d);
        format!("{year:04}-{month:02}-{day:02}T00:00:00Z")
    };

    let mut total = 0u64;

    // Prune old events
    let n = conn
        .execute("DELETE FROM events WHERE timestamp < ?1", params![cutoff])
        .map_err(|e| format!("prune events: {e}"))?;
    total += n as u64;

    // Prune resolved/expired blockers older than cutoff
    let n = conn
        .execute(
            "DELETE FROM blockers WHERE status IN ('resolved') AND created_at < ?1",
            params![cutoff],
        )
        .map_err(|e| format!("prune blockers: {e}"))?;
    total += n as u64;

    // Prune released/expired leases older than cutoff
    let n = conn
        .execute(
            "DELETE FROM leases WHERE status IN ('released', 'expired') AND acquired_at < ?1",
            params![cutoff],
        )
        .map_err(|e| format!("prune leases: {e}"))?;
    total += n as u64;

    // Prune terminal interrupts (acknowledged, expired, dismissed) older than cutoff
    let n = conn
        .execute(
            "DELETE FROM interrupts WHERE state IN ('acknowledged', 'expired', 'dismissed') AND created_at < ?1",
            params![cutoff],
        )
        .map_err(|e| format!("prune interrupts: {e}"))?;
    total += n as u64;

    // Prune acknowledged handoffs older than cutoff
    let n = conn
        .execute(
            "DELETE FROM handoffs WHERE acknowledged_at IS NOT NULL AND created_at < ?1",
            params![cutoff],
        )
        .map_err(|e| format!("prune handoffs: {e}"))?;
    total += n as u64;

    // Safety cap: if events table exceeds max, keep only the most recent
    let event_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap_or(0);
    if event_count > DEFAULT_MAX_EVENTS as i64 {
        let excess = event_count - DEFAULT_MAX_EVENTS as i64;
        let n = conn
            .execute(
                "DELETE FROM events WHERE id IN (SELECT id FROM events ORDER BY id ASC LIMIT ?1)",
                params![excess],
            )
            .map_err(|e| format!("cap events: {e}"))?;
        total += n as u64;
    }

    Ok(total)
}

// -- Tests ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_idempotent() {
        let conn = open_memory();
        // Second migration should succeed without error
        migrate(&conn).unwrap();
        // Triple-migrate too — `IF NOT EXISTS` should make this a no-op.
        migrate(&conn).unwrap();
    }

    #[test]
    fn schema_version_pragma_bumps_to_expected() {
        let conn = open_memory();
        let v: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, EXPECTED_COORD_SCHEMA_VERSION);
    }

    #[test]
    fn schema_version_gate_refuses_future_db() {
        // Simulate the downgrade scenario: a newer binary stamped the DB
        // at EXPECTED+1, then an older binary is asked to open it. The
        // gate must refuse loudly with the `init --upgrade` remediation.
        let conn = open_memory();
        let future = EXPECTED_COORD_SCHEMA_VERSION + 1;
        conn.pragma_update(None, "user_version", future).unwrap();
        let err = check_schema_version(&conn).expect_err("must refuse a future-version DB");
        assert!(
            err.contains(&format!("v{future}")),
            "error must name actual version: {err}"
        );
        assert!(
            err.contains("init --upgrade"),
            "error must point at the remediation: {err}"
        );
    }

    #[test]
    fn schema_version_gate_accepts_equal_or_lower() {
        let conn = open_memory();
        // Equal — happy path.
        check_schema_version(&conn).unwrap();
        // Lower — happens transiently if migrate() was rolled back mid-run.
        // The gate's job is not to flag underrun (migrate handles forward);
        // it only rejects overrun.
        conn.pragma_update(None, "user_version", 0u32).unwrap();
        check_schema_version(&conn).unwrap();
    }

    #[test]
    fn v2_tables_exist_after_migrate() {
        let conn = open_memory();
        for table in [
            "tasks",
            "task_attempts",
            "task_verifications",
            "task_transitions",
            "hook_events",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "v2 table missing: {table}");
        }
    }

    #[test]
    fn append_and_query_events() {
        let conn = open_memory();

        let e1 = CoordEvent {
            id: None,
            event_type: EventType::SessionObserved,
            timestamp: "2026-04-20T10:00:00Z".into(),
            session_id: Some("sess_1".into()),
            payload: serde_json::json!({"pid": 42}),
        };
        let e2 = CoordEvent {
            id: None,
            event_type: EventType::LeaseAcquired,
            timestamp: "2026-04-20T10:01:00Z".into(),
            session_id: Some("sess_1".into()),
            payload: serde_json::json!({"resource": "src/app.rs"}),
        };

        let id1 = append_event(&conn, &e1).unwrap();
        let id2 = append_event(&conn, &e2).unwrap();
        assert!(id2 > id1);

        // Query all
        let all = query_events(&conn, 10, None).unwrap();
        assert_eq!(all.len(), 2);
        // Most recent first
        assert_eq!(all[0].event_type, EventType::LeaseAcquired);

        // Query by type
        let leases = query_events(&conn, 10, Some("lease_acquired")).unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].event_type, EventType::LeaseAcquired);
    }

    #[test]
    fn upsert_and_list_leases() {
        let conn = open_memory();

        let lease = Lease {
            id: "lease_1".into(),
            owner_session_id: "sess_1".into(),
            owner_agent: "claude-code".into(),
            resource_kind: "path_glob".into(),
            resource_value: "src/brain/**".into(),
            mode: LeaseMode::Exclusive,
            reason: "Implementing threshold logic".into(),
            acquired_at: "2026-04-20T10:00:00Z".into(),
            expires_at: Some("2026-04-20T10:20:00Z".into()),
            status: LeaseStatus::Active,
        };

        upsert_lease(&conn, &lease).unwrap();

        let active = list_leases(&conn, Some(LeaseStatus::Active)).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].resource_value, "src/brain/**");
        assert_eq!(active[0].mode, LeaseMode::Exclusive);

        let released = list_leases(&conn, Some(LeaseStatus::Released)).unwrap();
        assert!(released.is_empty());
    }

    #[test]
    fn expire_stale_leases_works() {
        let conn = open_memory();

        // Insert a lease that already expired
        let lease = Lease {
            id: "lease_expired".into(),
            owner_session_id: "sess_1".into(),
            owner_agent: "claude-code".into(),
            resource_kind: "file".into(),
            resource_value: "src/app.rs".into(),
            mode: LeaseMode::Exclusive,
            reason: "test".into(),
            acquired_at: "2020-01-01T00:00:00Z".into(),
            expires_at: Some("2020-01-01T00:20:00Z".into()),
            status: LeaseStatus::Active,
        };
        upsert_lease(&conn, &lease).unwrap();

        let count = expire_stale_leases(&conn).unwrap();
        assert_eq!(count, 1);

        let active = list_leases(&conn, Some(LeaseStatus::Active)).unwrap();
        assert!(active.is_empty());

        let expired = list_leases(&conn, Some(LeaseStatus::Expired)).unwrap();
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn insert_and_list_blockers() {
        let conn = open_memory();

        let blocker = Blocker {
            id: "blocker_1".into(),
            task_id: "task_docs".into(),
            depends_on: Some("task_auth".into()),
            waiting_for: "API contract for JWT middleware".into(),
            status: BlockerStatus::Open,
            owner_session_id: "sess_docs".into(),
            created_at: "2026-04-20T10:05:00Z".into(),
            resolved_at: None,
        };
        insert_blocker(&conn, &blocker).unwrap();

        let open = list_blockers(&conn, Some(BlockerStatus::Open)).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].waiting_for, "API contract for JWT middleware");

        // Resolve it
        resolve_blocker(&conn, "blocker_1").unwrap();

        let open = list_blockers(&conn, Some(BlockerStatus::Open)).unwrap();
        assert!(open.is_empty());

        let resolved = list_blockers(&conn, Some(BlockerStatus::Resolved)).unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].resolved_at.is_some());
    }

    #[test]
    fn insert_and_list_handoffs() {
        let conn = open_memory();

        let handoff = Handoff {
            id: "handoff_1".into(),
            from_session_id: "sess_claude_1".into(),
            to_session_id: Some("sess_codex_2".into()),
            task_id: "task_windows".into(),
            summary: "Path normalization on Windows".into(),
            state: HandoffState {
                goal: "Fix Windows path tests".into(),
                artifacts: vec!["src/terminals/windows.rs".into()],
                attempted: vec!["Updated escaping".into()],
                next_steps: vec!["Normalize backslashes".into()],
            },
            priority: "high".into(),
            created_at: "2026-04-20T10:10:00Z".into(),
            acknowledged_at: None,
        };
        insert_handoff(&conn, &handoff).unwrap();

        let all = list_handoffs(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].state.goal, "Fix Windows path tests");
        assert_eq!(all[0].state.next_steps.len(), 1);
    }

    #[test]
    fn insert_and_list_interrupts() {
        let conn = open_memory();

        let interrupt = Interrupt {
            id: "intr_1".into(),
            interrupt_type: InterruptType::ReleaseOwnership,
            priority: "high".into(),
            target_session_id: "sess_codex_1".into(),
            reason: "Lease conflict on src/app.rs".into(),
            payload: Some(serde_json::json!({"resource": "src/app.rs"})),
            delivery_mode: "safe_boundary".into(),
            max_retries: 3,
            retry_count: 0,
            next_retry_at: None,
            expires_at: Some("2026-04-20T10:20:00Z".into()),
            dedupe_key: Some("lease:src/app.rs".into()),
            state: InterruptState::Pending,
            created_at: "2026-04-20T10:12:00Z".into(),
            delivered_at: None,
            acknowledged_at: None,
        };
        insert_interrupt(&conn, &interrupt).unwrap();

        let pending = list_interrupts(&conn, Some(InterruptState::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].interrupt_type, InterruptType::ReleaseOwnership);
        assert!(pending[0].payload.is_some());

        let acked = list_interrupts(&conn, Some(InterruptState::Acknowledged)).unwrap();
        assert!(acked.is_empty());
    }

    #[test]
    fn insert_and_search_memory_fts() {
        let conn = open_memory();

        let record = MemoryRecord {
            id: "mem_1".into(),
            mem_type: "workflow".into(),
            scope: serde_json::json!({"project": "claudectl"}),
            subjects: vec![Subject {
                kind: "path".into(),
                value: "src/health.rs".into(),
            }],
            summary: "When changing health thresholds, update both unit and integration tests."
                .into(),
            evidence: vec![Subject {
                kind: "path".into(),
                value: "tests/integration_tests.rs".into(),
            }],
            source: None,
            confidence: 0.92,
            created_at: "2026-04-20T10:00:00Z".into(),
            updated_at: "2026-04-20T10:00:00Z".into(),
            expires_at: None,
            tags: vec!["tests".into(), "health".into()],
        };
        insert_memory(&conn, &record).unwrap();

        // FTS search should find it
        let results = search_memory(&conn, "health thresholds", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "mem_1");
        assert_eq!(results[0].confidence, 0.92);

        // Search for non-matching term
        let empty = search_memory(&conn, "windows path normalization", 10).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn list_memory_returns_recent() {
        let conn = open_memory();

        for i in 0..3 {
            let record = MemoryRecord {
                id: format!("mem_{i}"),
                mem_type: "decision".into(),
                scope: serde_json::json!({}),
                subjects: vec![],
                summary: format!("Decision {i}"),
                evidence: vec![],
                source: None,
                confidence: 0.5,
                created_at: format!("2026-04-20T10:0{i}:00Z"),
                updated_at: format!("2026-04-20T10:0{i}:00Z"),
                expires_at: None,
                tags: vec![],
            };
            insert_memory(&conn, &record).unwrap();
        }

        let all = list_memory(&conn, 10).unwrap();
        assert_eq!(all.len(), 3);
        // Most recently updated first
        assert_eq!(all[0].id, "mem_2");
    }

    #[test]
    fn gen_id_is_unique() {
        let id1 = gen_id("test");
        let id2 = gen_id("test");
        assert_ne!(id1, id2);
        assert!(id1.starts_with("test_"));
    }

    #[test]
    fn paths_overlap_exact_match() {
        assert!(paths_overlap("src/app.rs", "src/app.rs"));
    }

    #[test]
    fn paths_overlap_glob_contains_file() {
        assert!(paths_overlap("src/**", "src/app.rs"));
        assert!(paths_overlap("src/*", "src/app.rs"));
        assert!(paths_overlap("src/brain/**", "src/brain/engine.rs"));
    }

    #[test]
    fn paths_overlap_file_under_glob() {
        assert!(paths_overlap("src/app.rs", "src/**"));
    }

    #[test]
    fn paths_overlap_disjoint() {
        assert!(!paths_overlap("tests/**", "src/app.rs"));
        assert!(!paths_overlap("src/app.rs", "tests/test.rs"));
    }

    #[test]
    fn paths_overlap_match_all_glob() {
        assert!(paths_overlap("**", "src/app.rs"));
        assert!(paths_overlap("src/app.rs", "**"));
        assert!(paths_overlap("**", "**"));
    }

    #[test]
    fn find_conflicting_lease_detects_glob_overlap() {
        let conn = open_memory();
        let lease = Lease {
            id: "l_glob".into(),
            owner_session_id: "sess_1".into(),
            owner_agent: "claude-code".into(),
            resource_kind: "path_glob".into(),
            resource_value: "src/**".into(),
            mode: LeaseMode::Exclusive,
            reason: "editing".into(),
            acquired_at: "2026-04-20T10:00:00Z".into(),
            expires_at: None,
            status: LeaseStatus::Active,
        };
        upsert_lease(&conn, &lease).unwrap();

        // A specific file under the glob should conflict
        let conflict = find_conflicting_lease(&conn, "path_glob", "src/app.rs", "sess_2").unwrap();
        assert!(conflict.is_some());
    }

    #[test]
    fn accept_handoff_sets_acknowledged() {
        let conn = open_memory();
        let h = Handoff {
            id: "h_acc".into(),
            from_session_id: "sess_1".into(),
            to_session_id: Some("sess_2".into()),
            task_id: "task_1".into(),
            summary: "Test".into(),
            state: HandoffState {
                goal: "Test".into(),
                artifacts: vec![],
                attempted: vec![],
                next_steps: vec![],
            },
            priority: "medium".into(),
            created_at: "2026-04-20T10:00:00Z".into(),
            acknowledged_at: None,
        };
        insert_handoff(&conn, &h).unwrap();

        let ok = accept_handoff(&conn, "h_acc").unwrap();
        assert!(ok);

        let after = get_handoff(&conn, "h_acc").unwrap().unwrap();
        assert!(after.acknowledged_at.is_some());

        // Accepting again should return false (already accepted)
        let ok2 = accept_handoff(&conn, "h_acc").unwrap();
        assert!(!ok2);
    }

    fn make_test_lease(id: &str, session: &str, resource: &str, mode: LeaseMode) -> Lease {
        Lease {
            id: id.into(),
            owner_session_id: session.into(),
            owner_agent: "claude-code".into(),
            resource_kind: "path_glob".into(),
            resource_value: resource.into(),
            mode,
            reason: "test".into(),
            acquired_at: "2026-04-20T10:00:00Z".into(),
            expires_at: None,
            status: LeaseStatus::Active,
        }
    }

    #[test]
    fn get_lease_returns_none_for_missing() {
        let conn = open_memory();
        let result = get_lease(&conn, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn get_lease_returns_existing() {
        let conn = open_memory();
        let lease = make_test_lease("lease_get", "sess_1", "src/app.rs", LeaseMode::Exclusive);
        upsert_lease(&conn, &lease).unwrap();

        let result = get_lease(&conn, "lease_get").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().owner_session_id, "sess_1");
    }

    #[test]
    fn find_conflicting_lease_finds_exclusive() {
        let conn = open_memory();
        let lease = make_test_lease("lease_exc", "sess_1", "src/app.rs", LeaseMode::Exclusive);
        upsert_lease(&conn, &lease).unwrap();

        // Another session trying to claim the same resource
        let conflict = find_conflicting_lease(&conn, "path_glob", "src/app.rs", "sess_2").unwrap();
        assert!(conflict.is_some());
        assert_eq!(conflict.unwrap().owner_session_id, "sess_1");
    }

    #[test]
    fn find_conflicting_lease_ignores_same_session() {
        let conn = open_memory();
        let lease = make_test_lease("lease_same", "sess_1", "src/app.rs", LeaseMode::Exclusive);
        upsert_lease(&conn, &lease).unwrap();

        // Same session should not conflict with itself
        let conflict = find_conflicting_lease(&conn, "path_glob", "src/app.rs", "sess_1").unwrap();
        assert!(conflict.is_none());
    }

    #[test]
    fn find_conflicting_lease_ignores_advisory() {
        let conn = open_memory();
        let lease = make_test_lease("lease_adv", "sess_1", "src/app.rs", LeaseMode::Advisory);
        upsert_lease(&conn, &lease).unwrap();

        // Advisory leases should not block exclusive claims
        let conflict = find_conflicting_lease(&conn, "path_glob", "src/app.rs", "sess_2").unwrap();
        assert!(conflict.is_none());
    }

    #[test]
    fn release_lease_sets_status() {
        let conn = open_memory();
        let lease = make_test_lease("lease_rel", "sess_1", "src/app.rs", LeaseMode::Exclusive);
        upsert_lease(&conn, &lease).unwrap();

        let released = release_lease(&conn, "lease_rel").unwrap();
        assert!(released);

        let after = get_lease(&conn, "lease_rel").unwrap().unwrap();
        assert_eq!(after.status, LeaseStatus::Released);
    }

    #[test]
    fn release_lease_returns_false_for_missing() {
        let conn = open_memory();
        let released = release_lease(&conn, "nonexistent").unwrap();
        assert!(!released);
    }

    #[test]
    fn list_leases_for_session_filters() {
        let conn = open_memory();
        let l1 = make_test_lease("lease_s1", "sess_1", "src/a.rs", LeaseMode::Exclusive);
        let l2 = make_test_lease("lease_s2", "sess_2", "src/b.rs", LeaseMode::Exclusive);
        let l3 = make_test_lease("lease_s3", "sess_1", "src/c.rs", LeaseMode::Advisory);
        upsert_lease(&conn, &l1).unwrap();
        upsert_lease(&conn, &l2).unwrap();
        upsert_lease(&conn, &l3).unwrap();

        let sess1_leases = list_leases_for_session(&conn, "sess_1").unwrap();
        assert_eq!(sess1_leases.len(), 2);

        let sess2_leases = list_leases_for_session(&conn, "sess_2").unwrap();
        assert_eq!(sess2_leases.len(), 1);
    }

    #[test]
    fn list_pending_handoffs_excludes_acknowledged() {
        let conn = open_memory();

        let h1 = Handoff {
            id: "h_pending".into(),
            from_session_id: "sess_1".into(),
            to_session_id: Some("sess_2".into()),
            task_id: "task_1".into(),
            summary: "Fix tests".into(),
            state: HandoffState {
                goal: "Fix tests".into(),
                artifacts: vec![],
                attempted: vec![],
                next_steps: vec![],
            },
            priority: "high".into(),
            created_at: "2026-04-20T10:00:00Z".into(),
            acknowledged_at: None,
        };
        let h2 = Handoff {
            id: "h_acked".into(),
            from_session_id: "sess_1".into(),
            to_session_id: Some("sess_3".into()),
            task_id: "task_2".into(),
            summary: "Done".into(),
            state: HandoffState {
                goal: "Done".into(),
                artifacts: vec![],
                attempted: vec![],
                next_steps: vec![],
            },
            priority: "medium".into(),
            created_at: "2026-04-20T10:01:00Z".into(),
            acknowledged_at: Some("2026-04-20T10:02:00Z".into()),
        };
        insert_handoff(&conn, &h1).unwrap();
        insert_handoff(&conn, &h2).unwrap();

        let pending = list_pending_handoffs(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "h_pending");
    }

    fn make_test_interrupt(id: &str, target: &str, itype: InterruptType) -> Interrupt {
        Interrupt {
            id: id.into(),
            interrupt_type: itype,
            priority: "medium".into(),
            target_session_id: target.into(),
            reason: "test reason".into(),
            payload: None,
            delivery_mode: "safe_boundary".into(),
            max_retries: 3,
            retry_count: 0,
            next_retry_at: None,
            expires_at: None,
            dedupe_key: None,
            state: InterruptState::Pending,
            created_at: "2026-04-20T10:00:00Z".into(),
            delivered_at: None,
            acknowledged_at: None,
        }
    }

    #[test]
    fn get_interrupt_returns_existing() {
        let conn = open_memory();
        let intr = make_test_interrupt("intr_get", "sess_1", InterruptType::Pause);
        insert_interrupt(&conn, &intr).unwrap();

        let result = get_interrupt(&conn, "intr_get").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().interrupt_type, InterruptType::Pause);
    }

    #[test]
    fn get_interrupt_returns_none_for_missing() {
        let conn = open_memory();
        assert!(get_interrupt(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn list_deliverable_interrupts_orders_by_priority() {
        let conn = open_memory();
        let mut low = make_test_interrupt("intr_low", "sess_1", InterruptType::Nudge);
        low.priority = "low".into();
        let mut high = make_test_interrupt("intr_high", "sess_1", InterruptType::Pause);
        high.priority = "high".into();
        // Insert low first, high second
        insert_interrupt(&conn, &low).unwrap();
        insert_interrupt(&conn, &high).unwrap();

        let deliverable = list_deliverable_interrupts(&conn).unwrap();
        assert_eq!(deliverable.len(), 2);
        // High priority should come first
        assert_eq!(deliverable[0].id, "intr_high");
        assert_eq!(deliverable[1].id, "intr_low");
    }

    #[test]
    fn list_deliverable_excludes_expired() {
        let conn = open_memory();
        let mut intr = make_test_interrupt("intr_exp", "sess_1", InterruptType::Compact);
        intr.expires_at = Some("2020-01-01T00:00:00Z".into()); // already expired
        insert_interrupt(&conn, &intr).unwrap();

        let deliverable = list_deliverable_interrupts(&conn).unwrap();
        assert!(deliverable.is_empty());
    }

    #[test]
    fn list_deliverable_excludes_interrupts_in_backoff() {
        let conn = open_memory();
        let mut intr = make_test_interrupt("intr_backoff", "sess_1", InterruptType::Compact);
        intr.retry_count = 1;
        intr.next_retry_at = Some("2099-01-01T00:00:00Z".into());
        insert_interrupt(&conn, &intr).unwrap();

        let deliverable = list_deliverable_interrupts(&conn).unwrap();
        assert!(deliverable.is_empty());
    }

    #[test]
    fn record_interrupt_delivery_failure_schedules_retry() {
        let conn = open_memory();
        let intr = make_test_interrupt("intr_retry", "sess_1", InterruptType::Pause);
        insert_interrupt(&conn, &intr).unwrap();

        let updated = record_interrupt_delivery_failure(&conn, "intr_retry")
            .unwrap()
            .unwrap();

        assert_eq!(updated.retry_count, 1);
        assert_eq!(updated.state, InterruptState::Pending);
        assert!(updated.next_retry_at.is_some());
        assert!(list_deliverable_interrupts(&conn).unwrap().is_empty());
    }

    #[test]
    fn record_interrupt_delivery_failure_expires_at_max_retries() {
        let conn = open_memory();
        let mut intr = make_test_interrupt("intr_retry_max", "sess_1", InterruptType::Pause);
        intr.max_retries = 2;
        insert_interrupt(&conn, &intr).unwrap();

        let first = record_interrupt_delivery_failure(&conn, "intr_retry_max")
            .unwrap()
            .unwrap();
        assert_eq!(first.retry_count, 1);
        assert_eq!(first.state, InterruptState::Pending);

        let second = record_interrupt_delivery_failure(&conn, "intr_retry_max")
            .unwrap()
            .unwrap();
        assert_eq!(second.retry_count, 2);
        assert_eq!(second.state, InterruptState::Expired);
        assert!(second.next_retry_at.is_none());
    }

    #[test]
    fn expire_exhausted_interrupts_marks_pending_as_expired() {
        let conn = open_memory();
        let mut intr = make_test_interrupt("intr_exhausted", "sess_1", InterruptType::Pause);
        intr.retry_count = 3;
        insert_interrupt(&conn, &intr).unwrap();

        let count = expire_exhausted_interrupts(&conn).unwrap();
        assert_eq!(count, 1);

        let after = get_interrupt(&conn, "intr_exhausted").unwrap().unwrap();
        assert_eq!(after.state, InterruptState::Expired);
    }

    #[test]
    fn mark_interrupt_delivered_transitions_state() {
        let conn = open_memory();
        let intr = make_test_interrupt("intr_del", "sess_1", InterruptType::Pause);
        insert_interrupt(&conn, &intr).unwrap();

        let ok = mark_interrupt_delivered(&conn, "intr_del").unwrap();
        assert!(ok);

        let after = get_interrupt(&conn, "intr_del").unwrap().unwrap();
        assert_eq!(after.state, InterruptState::Delivered);
        assert!(after.delivered_at.is_some());
    }

    #[test]
    fn mark_interrupt_acknowledged_requires_delivered() {
        let conn = open_memory();
        let intr = make_test_interrupt("intr_ack", "sess_1", InterruptType::Pause);
        insert_interrupt(&conn, &intr).unwrap();

        // Can't ack a pending interrupt
        let ok = mark_interrupt_acknowledged(&conn, "intr_ack").unwrap();
        assert!(!ok);

        // Deliver first, then ack
        mark_interrupt_delivered(&conn, "intr_ack").unwrap();
        let ok = mark_interrupt_acknowledged(&conn, "intr_ack").unwrap();
        assert!(ok);

        let after = get_interrupt(&conn, "intr_ack").unwrap().unwrap();
        assert_eq!(after.state, InterruptState::Acknowledged);
    }

    #[test]
    fn expire_stale_interrupts_works() {
        let conn = open_memory();
        let mut intr = make_test_interrupt("intr_stale", "sess_1", InterruptType::Compact);
        intr.expires_at = Some("2020-01-01T00:00:00Z".into());
        insert_interrupt(&conn, &intr).unwrap();

        let count = expire_stale_interrupts(&conn).unwrap();
        assert_eq!(count, 1);

        let after = get_interrupt(&conn, "intr_stale").unwrap().unwrap();
        assert_eq!(after.state, InterruptState::Expired);
    }

    #[test]
    fn migrate_adds_retry_columns_to_existing_interrupts_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE interrupts (
                id                TEXT PRIMARY KEY,
                interrupt_type    TEXT NOT NULL,
                priority          TEXT NOT NULL,
                target_session_id TEXT NOT NULL,
                reason            TEXT NOT NULL,
                payload_json      TEXT,
                delivery_mode     TEXT NOT NULL DEFAULT 'safe_boundary',
                max_retries       INTEGER NOT NULL DEFAULT 3,
                expires_at        TEXT,
                dedupe_key        TEXT,
                state             TEXT NOT NULL DEFAULT 'pending',
                created_at        TEXT NOT NULL,
                delivered_at      TEXT,
                acknowledged_at   TEXT
            );
            ",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(interrupts)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();

        assert!(columns.contains(&"retry_count".to_string()));
        assert!(columns.contains(&"next_retry_at".to_string()));
    }

    #[test]
    fn find_duplicate_interrupt_by_dedupe_key() {
        let conn = open_memory();
        let mut intr = make_test_interrupt("intr_dup", "sess_1", InterruptType::Compact);
        intr.dedupe_key = Some("compact:sess_1".into());
        insert_interrupt(&conn, &intr).unwrap();

        let dup = find_duplicate_interrupt(&conn, "compact:sess_1").unwrap();
        assert!(dup.is_some());
        assert_eq!(dup.unwrap().id, "intr_dup");

        // No match for different key
        let none = find_duplicate_interrupt(&conn, "compact:sess_2").unwrap();
        assert!(none.is_none());
    }
}

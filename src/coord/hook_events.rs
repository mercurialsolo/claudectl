// Allow dead_code: `since`/`count` are read by the supervisor's tick loop
// in PR4 once the reconciler tails this table. The ingest subcommand uses
// `append` today.
#![allow(dead_code)]
//! `hook_events` table — low-latency signal path (RFC v2 §6).
//!
//! Hooks push payloads here via the `claudectl ingest` subcommand
//! (`src/ingest.rs`). The reconciler tails this table on each tick so it
//! reacts to tool activity and turn boundaries in one tick instead of
//! waiting on a file-watch debounce of the JSONL transcript.
//!
//! **Hooks pull, the supervisor pushes via ingest.** The bash hooks call
//! `claudectl ingest --hook <name> 2>/dev/null || true`. The `|| true` is
//! deliberate — ingest is best-effort by construction, which is precisely
//! why it cannot be the source of record. JSONL tail + `ps` stay
//! authoritative; this table is a latency optimization on top.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    pub id: Option<i64>,
    pub hook: String,
    pub session_id: Option<String>,
    pub tool: Option<String>,
    /// Raw JSON payload as the hook received it on stdin. Stored as a
    /// string so the supervisor doesn't have to commit to a schema —
    /// hooks evolve under Claude Code's control, not ours.
    pub payload: String,
    pub ingested_at: String,
}

/// Append a hook event. The ingest subcommand is the only production
/// caller; tests use this directly.
pub fn append(conn: &Connection, ev: &HookEvent) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO hook_events (hook, session_id, tool, payload, ingested_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![ev.hook, ev.session_id, ev.tool, ev.payload, ev.ingested_at],
    )
    .map_err(|e| format!("insert hook event: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Read hook events newer than `since_id`. The reconciler tracks the
/// highest id it has seen per session and queries forward; this keeps the
/// tick loop O(new events) instead of O(table size).
pub fn since(conn: &Connection, since_id: i64, limit: usize) -> Result<Vec<HookEvent>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, hook, session_id, tool, payload, ingested_at
             FROM hook_events
             WHERE id > ?1
             ORDER BY id
             LIMIT ?2",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map(params![since_id, limit as i64], |row| {
            Ok(HookEvent {
                id: Some(row.get(0)?),
                hook: row.get(1)?,
                session_id: row.get(2)?,
                tool: row.get(3)?,
                payload: row.get(4)?,
                ingested_at: row.get(5)?,
            })
        })
        .map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

pub fn count(conn: &Connection) -> Result<u64, String> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM hook_events", [], |row| row.get(0))
        .map_err(|e| format!("count: {e}"))?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::store;

    fn ev(hook: &str, session: &str, tool: Option<&str>, body: &str) -> HookEvent {
        HookEvent {
            id: None,
            hook: hook.into(),
            session_id: Some(session.into()),
            tool: tool.map(String::from),
            payload: body.into(),
            ingested_at: crate::logger::timestamp_now(),
        }
    }

    #[test]
    fn round_trips_through_append_and_since() {
        let conn = store::open_memory();
        let id1 = append(&conn, &ev("PreToolUse", "sess_a", Some("Bash"), "{}")).unwrap();
        let id2 = append(&conn, &ev("PostToolUse", "sess_a", Some("Bash"), "{}")).unwrap();
        let _id3 = append(&conn, &ev("Stop", "sess_a", None, "{}")).unwrap();

        let all = since(&conn, 0, 10).unwrap();
        assert_eq!(all.len(), 3);
        // ID order matches insertion order (autoincrement).
        assert_eq!(all[0].id, Some(id1));
        assert_eq!(all[1].id, Some(id2));

        // since(>= id2) returns only events after id2.
        let after = since(&conn, id2, 10).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].hook, "Stop");
    }

    #[test]
    fn count_reflects_rows() {
        let conn = store::open_memory();
        assert_eq!(count(&conn).unwrap(), 0);
        let _ = append(&conn, &ev("Stop", "sess_x", None, "{}")).unwrap();
        assert_eq!(count(&conn).unwrap(), 1);
    }
}

//! Fleet snapshot — the cluster-wide session view, shared on disk.
//!
//! `claudectl relay serve` owns the write side: on every tick it records the
//! sessions each connected peer advertised in its heartbeat, plus the sessions
//! running on this machine. Readers (the TUI, `claudectl --json`) pick the file
//! up on their own refresh.
//!
//! A file rather than an HTTP call, because the relay and the TUI are separate
//! processes on the same machine and the peers panel already reads peer state
//! off disk the same way. That keeps the fleet view working with no port or
//! token to configure first; the coordinator's HTTP API stays available for
//! dashboards outside the machine.
//!
//! Writes go through a temp file + `rename` so a reader mid-tick never sees a
//! half-formed snapshot.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::session::ClaudeSession;

/// Workers whose last heartbeat is older than this are dropped on read. Three
/// times the default 30s heartbeat interval, matching the mesh's own expiry so
/// a peer doesn't linger in the fleet view after it stops reporting.
pub const WORKER_STALE_SECS: u64 = 90;

/// A snapshot older than this means `relay serve` is no longer running, so the
/// whole file is ignored rather than showing a frozen fleet.
pub const SNAPSHOT_STALE_SECS: u64 = 120;

/// One peer's contribution to the fleet view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetWorker {
    /// The peer's relay identity, used as the display prefix.
    pub worker_id: String,
    /// Sessions as serialized by [`ClaudeSession::to_json_value`] on that peer.
    pub sessions: Vec<serde_json::Value>,
    /// When that peer's heartbeat last arrived (epoch ms).
    pub updated_ms: u64,
}

/// What `relay serve` publishes for local readers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetSnapshot {
    /// When this snapshot was written (epoch ms).
    pub updated_ms: u64,
    /// This machine's relay identity.
    pub local_worker_id: String,
    /// Remote peers only. Local sessions are deliberately excluded: a reader on
    /// this machine already discovers those itself, and including them here
    /// would list every local session twice.
    pub workers: Vec<FleetWorker>,
}

impl FleetWorker {
    /// Whether this peer has reported recently enough to still be shown.
    pub fn is_live(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.updated_ms) <= WORKER_STALE_SECS * 1000
    }
}

impl FleetSnapshot {
    /// Peers still reporting. The one place staleness is decided, so a reader
    /// can't count one set of workers and display another.
    pub fn live_workers(&self) -> Vec<&FleetWorker> {
        let now = now_ms();
        self.workers.iter().filter(|w| w.is_live(now)).collect()
    }
}

fn relay_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claudectl").join("relay")
}

/// Where the fleet snapshot lives.
pub fn snapshot_path() -> PathBuf {
    relay_dir().join("fleet.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Write the snapshot atomically (temp file + rename).
pub fn write_snapshot(snapshot: &FleetSnapshot) -> io::Result<()> {
    write_snapshot_to(&relay_dir(), snapshot)
}

/// Write into an explicit directory. Exists so tests don't touch `$HOME`.
pub fn write_snapshot_to(dir: &std::path::Path, snapshot: &FleetSnapshot) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let body = serde_json::to_string(snapshot).map_err(io::Error::other)?;
    let tmp_path = dir.join(".fleet.json.tmp");
    fs::write(&tmp_path, body)?;
    fs::rename(&tmp_path, dir.join("fleet.json"))
}

/// Read the snapshot, or `None` when it's absent, unparseable, or stale enough
/// that `relay serve` has clearly stopped writing it.
pub fn read_snapshot() -> Option<FleetSnapshot> {
    read_snapshot_from(&snapshot_path())
}

/// Read from an explicit path. Exists so tests don't touch `$HOME`.
pub fn read_snapshot_from(path: &std::path::Path) -> Option<FleetSnapshot> {
    let body = fs::read_to_string(path).ok()?;
    let snapshot: FleetSnapshot = serde_json::from_str(&body).ok()?;
    let age_ms = now_ms().saturating_sub(snapshot.updated_ms);
    if age_ms > SNAPSHOT_STALE_SECS * 1000 {
        return None;
    }
    Some(snapshot)
}

/// The remote sessions a local reader should display, with stale workers
/// dropped and each entry tagged with its originating worker.
pub fn remote_sessions() -> Vec<ClaudeSession> {
    read_snapshot().map(sessions_from).unwrap_or_default()
}

/// Map a snapshot's live workers into displayable sessions.
pub fn sessions_from(snapshot: FleetSnapshot) -> Vec<ClaudeSession> {
    let mut out = Vec::new();
    for worker in snapshot.live_workers() {
        for value in &worker.sessions {
            if let Some(session) = ClaudeSession::from_remote_json(&worker.worker_id, value) {
                out.push(session);
            }
        }
    }
    out
}

/// Collects this machine's sessions for advertising to peers, keeping the
/// per-session parse state that makes repeated collection cheap.
///
/// Deliberately a narrower path than `App::refresh`: it discovers, enriches and
/// reads token usage, but fires no hooks, sends no notifications and records no
/// history. `relay serve` runs alongside the TUI, and those side effects would
/// otherwise happen twice.
///
/// It is stateful for the same reason `App` is: sessions carry their JSONL read
/// offset, accumulated tokens and CPU history. Rebuilding them from scratch each
/// time would re-parse every transcript from byte zero on every collection —
/// seconds of work on a long session, on the relay's own loop thread.
#[derive(Default)]
pub struct LocalSessionCollector {
    sessions: Vec<ClaudeSession>,
}

impl LocalSessionCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refresh in place and return the current sessions. Surviving PIDs keep
    /// their previous session object, so JSONL reads stay incremental.
    pub fn collect(&mut self) -> &[ClaudeSession] {
        let discovered = crate::discovery::scan_sessions();

        // Same merge as `App::refresh`: reuse the existing session for a PID we
        // already know, refreshing only the fields that move on their own.
        let mut existing: std::collections::HashMap<u32, ClaudeSession> =
            self.sessions.drain(..).map(|s| (s.pid, s)).collect();
        let mut sessions: Vec<ClaudeSession> = discovered
            .into_iter()
            .map(|new| match existing.remove(&new.pid) {
                Some(mut prev) => {
                    prev.elapsed = new.elapsed;
                    prev.started_at = new.started_at;
                    prev
                }
                None => new,
            })
            .collect();

        crate::process::fetch_and_enrich(&mut sessions);
        for session in &mut sessions {
            // Only sessions without a path need resolving; the rest kept theirs.
            if session.jsonl_path.is_none() {
                crate::discovery::resolve_jsonl_paths(std::slice::from_mut(session));
            }
        }
        for session in &mut sessions {
            crate::monitor::update_tokens(session);
        }

        self.sessions = sessions;
        &self.sessions
    }

    pub fn sessions(&self) -> &[ClaudeSession] {
        &self.sessions
    }

    /// The sessions in the JSON shape peers parse with
    /// [`ClaudeSession::from_remote_json`].
    pub fn as_json(&self) -> Vec<serde_json::Value> {
        self.sessions
            .iter()
            .map(ClaudeSession::to_json_value)
            .collect()
    }
}

/// One-shot collection, for callers that run once and exit (`relay fleet`).
/// Long-running callers should hold a [`LocalSessionCollector`] instead.
pub fn collect_local_sessions() -> Vec<serde_json::Value> {
    let mut collector = LocalSessionCollector::new();
    collector.collect();
    collector.as_json()
}

/// Build a snapshot from the local identity and the per-peer state the relay
/// has accumulated.
pub fn build_snapshot(local_worker_id: &str, workers: Vec<FleetWorker>) -> FleetSnapshot {
    FleetSnapshot {
        updated_ms: now_ms(),
        local_worker_id: local_worker_id.to_string(),
        workers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("claudectl-fleet-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample_session_json() -> serde_json::Value {
        serde_json::json!({
            "pid": 4242,
            "project": "claudectl",
            "status": "Processing",
            "cost_usd": 2.5,
            "elapsed_secs": 300,
            "tokens_in": 1000,
            "tokens_out": 200,
        })
    }

    #[test]
    fn snapshot_round_trips_through_disk() {
        let dir = temp_dir("roundtrip");
        let snapshot = build_snapshot(
            "laptop",
            vec![FleetWorker {
                worker_id: "mini".into(),
                sessions: vec![sample_session_json()],
                updated_ms: now_ms(),
            }],
        );
        write_snapshot_to(&dir, &snapshot).unwrap();

        let read = read_snapshot_from(&dir.join("fleet.json")).unwrap();
        assert_eq!(read.local_worker_id, "laptop");
        assert_eq!(read.workers.len(), 1);
        assert_eq!(read.workers[0].worker_id, "mini");

        let sessions = sessions_from(read);
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].is_remote());
        assert_eq!(sessions[0].project_name, "[mini] claudectl");
    }

    /// The shape guard: whatever `to_json_value` emits must survive a heartbeat
    /// and come back out of `from_remote_json`. A field renamed on one side
    /// without the other silently empties the fleet view, so assert the
    /// round trip on a real session rather than a hand-written literal.
    #[test]
    fn real_session_json_survives_the_round_trip() {
        let mut session = ClaudeSession::from_raw(crate::session::RawSession {
            pid: 777,
            session_id: "abc123".into(),
            cwd: "/Users/dev/myproject".into(),
            started_at: now_ms().saturating_sub(60_000),
        });
        session.status = crate::session::SessionStatus::NeedsInput;
        session.cost_usd = 1.5;
        session.total_input_tokens = 12_000;
        session.total_output_tokens = 3_000;
        session.usage_metrics_available = true;

        let value = session.to_json_value();
        let restored = ClaudeSession::from_remote_json("mini", &value)
            .expect("to_json_value output must parse back via from_remote_json");

        assert_eq!(restored.pid, 777);
        assert_eq!(restored.status, crate::session::SessionStatus::NeedsInput);
        assert_eq!(restored.total_input_tokens, 12_000);
        assert_eq!(restored.worker_origin.as_deref(), Some("mini"));
        assert!(restored.project_name.starts_with("[mini] "));
    }

    #[test]
    fn stale_snapshot_is_ignored() {
        let dir = temp_dir("stale");
        let mut snapshot = build_snapshot("laptop", vec![]);
        snapshot.updated_ms = now_ms().saturating_sub((SNAPSHOT_STALE_SECS + 10) * 1000);
        write_snapshot_to(&dir, &snapshot).unwrap();
        assert!(read_snapshot_from(&dir.join("fleet.json")).is_none());
    }

    #[test]
    fn stale_worker_is_dropped_from_a_live_snapshot() {
        let snapshot = build_snapshot(
            "laptop",
            vec![
                FleetWorker {
                    worker_id: "live".into(),
                    sessions: vec![sample_session_json()],
                    updated_ms: now_ms(),
                },
                FleetWorker {
                    worker_id: "gone".into(),
                    sessions: vec![sample_session_json()],
                    updated_ms: now_ms().saturating_sub((WORKER_STALE_SECS + 10) * 1000),
                },
            ],
        );
        let sessions = sessions_from(snapshot);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].worker_origin.as_deref(), Some("live"));
    }

    /// Counts and rows must come from the same set. A reader that counted
    /// `workers` but rendered `live_workers` (or the reverse) reported a
    /// different number of sessions than it printed.
    #[test]
    fn live_workers_and_sessions_from_agree() {
        let snapshot = build_snapshot(
            "laptop",
            vec![
                FleetWorker {
                    worker_id: "live".into(),
                    sessions: vec![sample_session_json(), sample_session_json()],
                    updated_ms: now_ms(),
                },
                FleetWorker {
                    worker_id: "gone".into(),
                    sessions: vec![sample_session_json()],
                    updated_ms: now_ms().saturating_sub((WORKER_STALE_SECS + 10) * 1000),
                },
            ],
        );
        let live = snapshot.live_workers();
        assert_eq!(live.len(), 1);
        let counted: usize = live.iter().map(|w| w.sessions.len()).sum();
        assert_eq!(counted, sessions_from(snapshot).len());
    }

    /// The collector must hand the same session object back for a surviving
    /// PID; a fresh one would restart its JSONL read at byte zero, which is
    /// what makes repeated collection expensive.
    #[test]
    fn collector_keeps_session_state_across_collections() {
        let mut collector = LocalSessionCollector::new();

        // Seed a session with parse state, as though it had already been read.
        collector.sessions = vec![{
            let mut s = ClaudeSession::from_raw(crate::session::RawSession {
                pid: std::process::id(),
                session_id: "seeded".into(),
                cwd: "/tmp/seeded".into(),
                started_at: now_ms(),
            });
            s.jsonl_offset = 4096;
            s.total_input_tokens = 9_000;
            s
        }];

        let pid = collector.sessions[0].pid;
        let mut existing: std::collections::HashMap<u32, ClaudeSession> =
            collector.sessions.drain(..).map(|s| (s.pid, s)).collect();

        // Exercise the merge directly: a rediscovered PID must reuse the object.
        let rediscovered = ClaudeSession::from_raw(crate::session::RawSession {
            pid,
            session_id: "seeded".into(),
            cwd: "/tmp/seeded".into(),
            started_at: now_ms(),
        });
        let merged = match existing.remove(&rediscovered.pid) {
            Some(mut prev) => {
                prev.elapsed = rediscovered.elapsed;
                prev
            }
            None => rediscovered,
        };

        assert_eq!(
            merged.jsonl_offset, 4096,
            "a surviving PID must keep its JSONL offset"
        );
        assert_eq!(merged.total_input_tokens, 9_000);
    }

    #[test]
    fn missing_snapshot_reads_as_empty() {
        let dir = temp_dir("missing");
        assert!(read_snapshot_from(&dir.join("fleet.json")).is_none());
    }
}

//! Advertising this machine's sessions to the mesh, and publishing what the
//! mesh reports back for local readers.
//!
//! Two halves of the same loop:
//!
//! - [`LocalSessionFeed`] collects the sessions running here so the heartbeat
//!   can carry them to every connected peer.
//! - [`publish_snapshot`] writes what peers have advertised to
//!   `~/.claudectl/relay/fleet.json`, where the TUI picks it up.
//!
//! Both halves run on the relay's 1s tick, but session collection shells out to
//! `ps` and re-reads JSONL tails, so it's throttled to [`COLLECT_INTERVAL`]
//! rather than running every tick.

use std::time::{Duration, Instant};

use claudectl_core::fleet;

use super::mesh::PeerRegistry;

/// How often to re-collect local sessions. Well under the 30s default heartbeat
/// interval, so a heartbeat never carries badly stale data, while keeping the
/// `ps` + JSONL cost off most of the 1s ticks.
pub const COLLECT_INTERVAL: Duration = Duration::from_secs(5);

/// Caches this machine's session list between collections.
///
/// Wraps a [`fleet::LocalSessionCollector`], which holds the per-session parse
/// state so each collection reads only the new bytes of each transcript.
pub struct LocalSessionFeed {
    collector: fleet::LocalSessionCollector,
    sessions: Vec<serde_json::Value>,
    last_collect: Option<Instant>,
    interval: Duration,
}

impl LocalSessionFeed {
    pub fn new() -> Self {
        Self::with_interval(COLLECT_INTERVAL)
    }

    pub fn with_interval(interval: Duration) -> Self {
        LocalSessionFeed {
            collector: fleet::LocalSessionCollector::new(),
            sessions: Vec::new(),
            last_collect: None,
            interval,
        }
    }

    /// Whether a collection is due on this tick.
    pub fn is_due(&self, now: Instant) -> bool {
        match self.last_collect {
            None => true,
            Some(last) => now.duration_since(last) >= self.interval,
        }
    }

    /// Collect when due. Returns whether a collection actually ran, so the
    /// caller can publish a fresh snapshot on the same beat.
    pub fn collect_if_due(&mut self, now: Instant) -> bool {
        if !self.is_due(now) {
            return false;
        }
        self.last_collect = Some(now);
        self.collector.collect();
        self.sessions = self.collector.as_json();
        true
    }

    /// Replace the cached sessions directly. Tests use this to avoid depending
    /// on whatever happens to be running on the machine.
    #[cfg(test)]
    pub fn set_sessions(&mut self, sessions: Vec<serde_json::Value>, now: Instant) {
        self.sessions = sessions;
        self.last_collect = Some(now);
    }

    /// What to attach to the next heartbeat.
    pub fn sessions(&self) -> &[serde_json::Value] {
        &self.sessions
    }
}

impl Default for LocalSessionFeed {
    fn default() -> Self {
        Self::new()
    }
}

/// Translate the registry's per-peer state into fleet workers.
pub fn workers_from(registry: &PeerRegistry) -> Vec<fleet::FleetWorker> {
    registry
        .all_worker_states()
        .values()
        .map(|ws| fleet::FleetWorker {
            worker_id: ws.worker_id.clone(),
            sessions: ws.sessions.clone(),
            updated_ms: ws.last_updated,
        })
        .collect()
}

/// Write the fleet snapshot for local readers (the TUI, `claudectl --json`).
///
/// Best-effort: a failed write must never take the relay down, so it logs and
/// moves on. The next collection republishes.
pub fn publish_snapshot(identity: &str, registry: &PeerRegistry) {
    let snapshot = fleet::build_snapshot(identity, workers_from(registry));
    if let Err(e) = fleet::write_snapshot(&snapshot) {
        eprintln!(
            "[{}] fleet snapshot write failed: {e}",
            crate::logger::timestamp_now()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_json(pid: u64) -> serde_json::Value {
        serde_json::json!({ "pid": pid, "project": "demo", "status": "Processing" })
    }

    #[test]
    fn first_collection_is_always_due() {
        let feed = LocalSessionFeed::new();
        assert!(feed.is_due(Instant::now()));
        assert!(feed.sessions().is_empty());
    }

    #[test]
    fn collection_is_throttled_to_the_interval() {
        let now = Instant::now();
        let mut feed = LocalSessionFeed::with_interval(Duration::from_secs(5));
        feed.set_sessions(vec![session_json(1)], now);

        // Same beat: not due again.
        assert!(!feed.is_due(now));
        assert!(!feed.collect_if_due(now));
        // Still serving the cached list, so heartbeats in between carry data.
        assert_eq!(feed.sessions().len(), 1);

        // Past the interval: due again.
        assert!(feed.is_due(now + Duration::from_secs(6)));
    }

    #[test]
    fn workers_from_an_empty_registry_is_empty() {
        let registry = PeerRegistry::new(30);
        assert!(workers_from(&registry).is_empty());
    }

    /// The end-to-end shape: a peer's heartbeat lands in the registry, becomes a
    /// fleet worker, and comes back out as a displayable remote session.
    #[test]
    fn heartbeat_sessions_reach_the_fleet_snapshot() {
        let mut registry = PeerRegistry::new(30);
        let peer = super::super::PeerId("mini".to_string());
        registry.handle_heartbeat(
            &peer,
            &serde_json::json!({
                "worker_id": "mini",
                "sessions": [session_json(4242)],
            }),
        );

        let workers = workers_from(&registry);
        assert_eq!(
            workers.len(),
            1,
            "heartbeat should produce one fleet worker"
        );

        let snapshot = fleet::build_snapshot("laptop", workers);
        let sessions = fleet::sessions_from(snapshot);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].worker_origin.as_deref(), Some("mini"));
        assert_eq!(sessions[0].pid, 4242);
    }
}

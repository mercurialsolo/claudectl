// Peer registry: tracks all connected peers, handles broadcast, dedup, heartbeats.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use super::peer::{PeerConnection, PeerState};
use super::{PeerId, RelayMessage};

/// Maximum number of message IDs to track for deduplication.
const DEDUP_CAPACITY: usize = 1000;

/// Session state snapshot received from a single worker peer.
#[derive(Debug, Clone)]
pub struct WorkerState {
    pub worker_id: String,
    pub sessions: Vec<serde_json::Value>,
    pub last_updated: u64, // epoch_ms
}

/// The peer registry: central state for all peer connections.
pub struct PeerRegistry {
    peers: HashMap<String, PeerConnection>, // peer_id string -> connection
    tx: Sender<(PeerId, RelayMessage)>,
    rx: Receiver<(PeerId, RelayMessage)>,
    seen_ids: VecDeque<String>,
    heartbeat_interval: Duration,
    last_heartbeat_tick: Instant,
    /// Session state received from each connected peer's heartbeat.
    worker_states: HashMap<String, WorkerState>,
}

impl PeerRegistry {
    pub fn new(heartbeat_interval_secs: u64) -> Self {
        let (tx, rx) = channel();
        PeerRegistry {
            peers: HashMap::new(),
            tx,
            rx,
            seen_ids: VecDeque::with_capacity(DEDUP_CAPACITY + 1),
            heartbeat_interval: Duration::from_secs(heartbeat_interval_secs),
            last_heartbeat_tick: Instant::now(),
            worker_states: HashMap::new(),
        }
    }

    /// Get a clone of the message sender (for passing to peer reader threads).
    pub fn message_tx(&self) -> Sender<(PeerId, RelayMessage)> {
        self.tx.clone()
    }

    /// Add a peer connection to the registry.
    pub fn add_peer(&mut self, conn: PeerConnection) {
        let id = conn.peer_id.0.clone();
        // If there's an existing connection to this peer, remove it
        self.peers.remove(&id);
        self.peers.insert(id, conn);
    }

    /// Remove a peer from the registry.
    pub fn remove_peer(&mut self, id: &str) {
        self.peers.remove(id);
    }

    /// Get a reference to a peer connection.
    pub fn get_peer(&self, id: &str) -> Option<&PeerConnection> {
        self.peers.get(id)
    }

    /// Get a mutable reference to a peer connection.
    pub fn get_peer_mut(&mut self, id: &str) -> Option<&mut PeerConnection> {
        self.peers.get_mut(id)
    }

    /// List all connected peer IDs.
    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.peers
            .values()
            .filter(|p| p.state == PeerState::Connected)
            .map(|p| p.peer_id.clone())
            .collect()
    }

    /// List all peer IDs regardless of state.
    pub fn all_peers(&self) -> Vec<(PeerId, PeerState)> {
        self.peers
            .values()
            .map(|p| (p.peer_id.clone(), p.state))
            .collect()
    }

    /// Broadcast a message to all connected peers.
    pub fn broadcast(&self, msg: &RelayMessage) {
        for peer in self.peers.values() {
            if peer.state == PeerState::Connected {
                let _ = peer.send(msg);
            }
        }
    }

    /// Send a message to a specific peer.
    pub fn send_to(&self, id: &str, msg: &RelayMessage) -> Result<(), String> {
        match self.peers.get(id) {
            Some(peer) if peer.state == PeerState::Connected => {
                peer.send(msg).map_err(|e| format!("send failed: {e}"))
            }
            Some(_) => Err("peer not connected".into()),
            None => Err("peer not found".into()),
        }
    }

    /// Drain all pending messages from peer reader threads.
    /// Returns messages that passed deduplication.
    pub fn drain_messages(&mut self) -> Vec<(PeerId, RelayMessage)> {
        let mut messages = Vec::new();
        while let Ok((peer_id, msg)) = self.rx.try_recv() {
            // Dedup check
            if self.seen_ids.contains(&msg.id) {
                continue;
            }
            self.seen_ids.push_back(msg.id.clone());
            if self.seen_ids.len() > DEDUP_CAPACITY {
                self.seen_ids.pop_front();
            }
            messages.push((peer_id, msg));
        }
        messages
    }

    /// Periodic tick: send heartbeats, check for dead peers, schedule reconnects.
    /// When `local_sessions` is provided, heartbeats include the session state.
    /// Returns a list of events (peer disconnected, peer needs reconnect, etc).
    pub fn tick(
        &mut self,
        identity: &str,
        local_sessions: Option<&[serde_json::Value]>,
    ) -> Vec<MeshEvent> {
        let mut events = Vec::new();
        let now = Instant::now();

        // Send heartbeats if interval elapsed
        let should_heartbeat =
            now.duration_since(self.last_heartbeat_tick) >= self.heartbeat_interval;
        if should_heartbeat {
            self.last_heartbeat_tick = now;
        }

        let peer_ids: Vec<String> = self.peers.keys().cloned().collect();
        for id in peer_ids {
            let peer = match self.peers.get_mut(&id) {
                Some(p) => p,
                None => continue,
            };

            match peer.state {
                PeerState::Connected => {
                    // Send heartbeat (with sessions if available)
                    if should_heartbeat {
                        let send_ok = match local_sessions {
                            Some(sessions) => peer
                                .send_heartbeat_with_sessions(identity, sessions)
                                .is_ok(),
                            None => peer.send_heartbeat(identity).is_ok(),
                        };
                        if !send_ok {
                            peer.mark_disconnected();
                            events.push(MeshEvent::PeerDisconnected(peer.peer_id.clone()));
                            continue;
                        }
                    }

                    // Check alive
                    if !peer.check_alive(self.heartbeat_interval) {
                        peer.mark_disconnected();
                        events.push(MeshEvent::PeerDisconnected(peer.peer_id.clone()));
                        if peer.is_initiator {
                            peer.schedule_reconnect();
                            events.push(MeshEvent::ReconnectScheduled(
                                peer.peer_id.clone(),
                                peer.reconnect_delay(),
                            ));
                        }
                    }
                }
                PeerState::Disconnected if peer.is_initiator => {
                    if peer.should_reconnect() {
                        events.push(MeshEvent::ReconnectNeeded(peer.peer_id.clone(), peer.addr));
                        peer.schedule_reconnect();
                    }
                }
                _ => {}
            }
        }

        // Expire stale worker states (3x heartbeat interval)
        let stale_ms = self.heartbeat_interval.as_millis() as u64 * 3;
        self.expire_stale_workers(stale_ms);

        events
    }

    /// Process an incoming heartbeat for a peer.
    /// If the payload contains session data, store the worker state.
    pub fn handle_heartbeat(&mut self, peer_id: &PeerId, payload: &serde_json::Value) {
        if let Some(peer) = self.peers.get_mut(&peer_id.0) {
            peer.record_heartbeat();
        }
        // Store worker state if sessions are present in the payload
        if let Some(sessions) = payload.get("sessions").and_then(|v| v.as_array()) {
            let worker_id = payload
                .get("worker_id")
                .and_then(|v| v.as_str())
                .unwrap_or(peer_id.as_str())
                .to_string();
            self.worker_states.insert(
                peer_id.0.clone(),
                WorkerState {
                    worker_id,
                    sessions: sessions.clone(),
                    last_updated: super::epoch_ms(),
                },
            );
        }
    }

    /// Get all worker states (session snapshots from connected peers).
    pub fn all_worker_states(&self) -> &HashMap<String, WorkerState> {
        &self.worker_states
    }

    /// Remove worker states that haven't been updated within `max_age_ms`.
    fn expire_stale_workers(&mut self, max_age_ms: u64) {
        let now = super::epoch_ms();
        self.worker_states
            .retain(|_, ws| now.saturating_sub(ws.last_updated) < max_age_ms);
    }

    /// Number of connected peers.
    pub fn connected_count(&self) -> usize {
        self.peers
            .values()
            .filter(|p| p.state == PeerState::Connected)
            .count()
    }

    /// Total number of tracked peers (any state).
    pub fn total_count(&self) -> usize {
        self.peers.len()
    }
}

/// Events generated by mesh tick.
#[derive(Debug)]
pub enum MeshEvent {
    PeerDisconnected(PeerId),
    ReconnectScheduled(PeerId, Duration),
    ReconnectNeeded(PeerId, Option<std::net::SocketAddr>),
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(id: &str) -> RelayMessage {
        RelayMessage {
            id: id.into(),
            msg_type: super::super::MessageType::Heartbeat,
            from_peer: "test".into(),
            timestamp: 0,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn dedup_filters_duplicate_ids() {
        let mut registry = PeerRegistry::new(30);

        // Manually push messages through the channel
        let tx = registry.message_tx();
        let peer = PeerId("peer1".into());
        tx.send((peer.clone(), make_msg("msg_1"))).unwrap();
        tx.send((peer.clone(), make_msg("msg_1"))).unwrap(); // duplicate
        tx.send((peer.clone(), make_msg("msg_2"))).unwrap();

        let messages = registry.drain_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].1.id, "msg_1");
        assert_eq!(messages[1].1.id, "msg_2");
    }

    #[test]
    fn dedup_evicts_oldest_beyond_capacity() {
        let mut registry = PeerRegistry::new(30);

        // Fill the dedup buffer
        for i in 0..DEDUP_CAPACITY + 5 {
            registry.seen_ids.push_back(format!("msg_{i}"));
            if registry.seen_ids.len() > DEDUP_CAPACITY {
                registry.seen_ids.pop_front();
            }
        }

        assert_eq!(registry.seen_ids.len(), DEDUP_CAPACITY);
        // msg_0 through msg_4 should have been evicted
        assert!(!registry.seen_ids.contains(&"msg_0".to_string()));
        assert!(!registry.seen_ids.contains(&"msg_4".to_string()));
        // msg_5 and later should still be present
        assert!(registry.seen_ids.contains(&"msg_5".to_string()));
    }

    #[test]
    fn connected_peers_filters_by_state() {
        let registry = PeerRegistry::new(30);
        // Empty registry
        assert_eq!(registry.connected_peers().len(), 0);
        assert_eq!(registry.connected_count(), 0);
        assert_eq!(registry.total_count(), 0);
    }

    #[test]
    fn broadcast_and_send_to_empty_registry() {
        let registry = PeerRegistry::new(30);
        let msg = make_msg("test");
        // Should not panic on empty registry
        registry.broadcast(&msg);
        assert!(registry.send_to("nonexistent", &msg).is_err());
    }

    #[test]
    fn handle_heartbeat_stores_worker_state() {
        let mut registry = PeerRegistry::new(30);
        let peer_id = PeerId("worker-01".into());
        let payload = serde_json::json!({
            "worker_id": "worker-01",
            "timestamp": 1234567890_u64,
            "sessions": [
                {"pid": 100, "project": "backend", "status": "Processing"},
                {"pid": 200, "project": "frontend", "status": "Idle"},
            ]
        });
        registry.handle_heartbeat(&peer_id, &payload);

        let states = registry.all_worker_states();
        assert_eq!(states.len(), 1);
        let ws = states.get("worker-01").unwrap();
        assert_eq!(ws.worker_id, "worker-01");
        assert_eq!(ws.sessions.len(), 2);
    }

    #[test]
    fn handle_heartbeat_empty_payload_is_liveness_only() {
        let mut registry = PeerRegistry::new(30);
        let peer_id = PeerId("worker-02".into());
        let payload = serde_json::json!({});
        registry.handle_heartbeat(&peer_id, &payload);

        assert!(registry.all_worker_states().is_empty());
    }

    #[test]
    fn expire_stale_workers_removes_old_entries() {
        let mut registry = PeerRegistry::new(30);
        let peer_id = PeerId("stale-worker".into());
        let payload = serde_json::json!({
            "worker_id": "stale-worker",
            "sessions": []
        });
        registry.handle_heartbeat(&peer_id, &payload);
        assert_eq!(registry.all_worker_states().len(), 1);

        // Manually backdate the entry
        if let Some(ws) = registry.worker_states.get_mut("stale-worker") {
            ws.last_updated = 1; // epoch_ms near zero = very stale
        }
        registry.expire_stale_workers(1000);
        assert!(registry.all_worker_states().is_empty());
    }

    #[test]
    fn handle_heartbeat_updates_existing_worker() {
        let mut registry = PeerRegistry::new(30);
        let peer_id = PeerId("worker-01".into());

        let payload1 = serde_json::json!({
            "worker_id": "worker-01",
            "sessions": [{"pid": 100}]
        });
        registry.handle_heartbeat(&peer_id, &payload1);
        assert_eq!(registry.all_worker_states()["worker-01"].sessions.len(), 1);

        let payload2 = serde_json::json!({
            "worker_id": "worker-01",
            "sessions": [{"pid": 100}, {"pid": 200}, {"pid": 300}]
        });
        registry.handle_heartbeat(&peer_id, &payload2);
        assert_eq!(registry.all_worker_states()["worker-01"].sessions.len(), 3);
    }
}

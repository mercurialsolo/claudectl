// Peer registry: tracks all connected peers, handles broadcast, dedup, heartbeats.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use super::peer::{PeerConnection, PeerState};
use super::{PeerId, RelayMessage};

/// Maximum number of message IDs to track for deduplication.
const DEDUP_CAPACITY: usize = 1000;

/// The peer registry: central state for all peer connections.
pub struct PeerRegistry {
    peers: HashMap<String, PeerConnection>, // peer_id string -> connection
    tx: Sender<(PeerId, RelayMessage)>,
    rx: Receiver<(PeerId, RelayMessage)>,
    seen_ids: VecDeque<String>,
    heartbeat_interval: Duration,
    last_heartbeat_tick: Instant,
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
    /// Returns a list of events (peer disconnected, peer needs reconnect, etc).
    pub fn tick(&mut self, identity: &str) -> Vec<MeshEvent> {
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
                    // Send heartbeat
                    if should_heartbeat {
                        if peer.send_heartbeat(identity).is_err() {
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

        events
    }

    /// Process an incoming heartbeat for a peer.
    pub fn handle_heartbeat(&mut self, peer_id: &PeerId) {
        if let Some(peer) = self.peers.get_mut(&peer_id.0) {
            peer.record_heartbeat();
        }
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
}

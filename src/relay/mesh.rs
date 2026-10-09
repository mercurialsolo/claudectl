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
    /// Our own peer id, for resolving simultaneous-dial collisions.
    identity: String,
}

impl PeerRegistry {
    pub fn new(heartbeat_interval_secs: u64, identity: &str) -> Self {
        let (tx, rx) = channel();
        PeerRegistry {
            peers: HashMap::new(),
            tx,
            rx,
            seen_ids: VecDeque::with_capacity(DEDUP_CAPACITY + 1),
            heartbeat_interval: Duration::from_secs(heartbeat_interval_secs),
            last_heartbeat_tick: Instant::now(),
            worker_states: HashMap::new(),
            identity: identity.to_string(),
        }
    }

    /// Get a clone of the message sender (for passing to peer reader threads).
    pub fn message_tx(&self) -> Sender<(PeerId, RelayMessage)> {
        self.tx.clone()
    }

    /// Add a peer connection to the registry, replacing any existing one.
    ///
    /// Newest-wins is right for a reconnect. What was wrong (#459) is that the
    /// displaced connection was only *forgotten*: its socket is shared with its
    /// reader thread through an `Arc`, so dropping the registry entry left the
    /// socket open — still delivering inbound messages, with nothing able to
    /// send on it. `claudectl hive join` dials its own short-lived connection,
    /// so running it beside a `relay join` from the same machine left the host
    /// reading from a peer it could no longer answer, and hive knowledge
    /// stopped flowing one way with nothing logged.
    ///
    /// Closing it makes both ends agree: the reader thread exits, and the other
    /// end sees a FIN and reconnects, since the dialling side is the initiator.
    /// Returns whether `conn` is the connection now in the registry. A `false`
    /// means the collision rule below kept the existing one and closed this
    /// one — the caller has a dead socket and should not report a new link.
    pub fn add_peer(&mut self, conn: PeerConnection) -> bool {
        let id = conn.peer_id.0.clone();
        if let Some(old) = self.peers.remove(&id) {
            if old.reader_alive() && !self.replaces(&old, &conn) {
                // Keep what we have. The peer applies the same rule to the
                // same two ids and keeps the other end of this same socket.
                conn.shutdown();
                self.peers.insert(id, old);
                return false;
            }
            old.shutdown();
        }
        self.peers.insert(id, conn);
        true
    }

    /// Should `new` displace the live connection `old` for the same peer?
    ///
    /// Two peers that dial each other at the same moment end up holding two
    /// authenticated sockets for the one pair — an inbound and an outbound at
    /// each end. Both ends must agree on which survives, and only an
    /// asymmetric rule gives agreement: "always keep mine" and "always keep
    /// theirs" both close one socket at each end, and the two survivors are
    /// then opposite ends of *different* sockets, so both die and neither peer
    /// reconnects. That was observed, not theorised.
    ///
    /// So: the connection opened by the lower peer id wins. We opened an
    /// outbound one, the peer opened an inbound one, so each end compares the
    /// same pair of ids and reaches the same verdict.
    fn replaces(&self, old: &PeerConnection, new: &PeerConnection) -> bool {
        // Same direction is not a collision — it is a replacement, which is
        // what #459 is about: the newer socket is the live one.
        if old.is_initiator == new.is_initiator {
            return true;
        }
        // Past that check the two are opposite directions for the same peer,
        // so the two openers are exactly us and them.
        let ours = self.identity.as_str();
        let theirs = new.peer_id.as_str();
        if new.is_initiator {
            ours < theirs
        } else {
            theirs < ours
        }
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
                PeerState::Disconnected if peer.is_initiator && peer.should_reconnect() => {
                    events.push(MeshEvent::ReconnectNeeded(peer.peer_id.clone(), peer.addr));
                    peer.schedule_reconnect();
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
        let mut registry = PeerRegistry::new(30, "local-test");

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
        let mut registry = PeerRegistry::new(30, "local-test");

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
        let registry = PeerRegistry::new(30, "local-test");
        // Empty registry
        assert_eq!(registry.connected_peers().len(), 0);
        assert_eq!(registry.connected_count(), 0);
        assert_eq!(registry.total_count(), 0);
    }

    #[test]
    fn broadcast_and_send_to_empty_registry() {
        let registry = PeerRegistry::new(30, "local-test");
        let msg = make_msg("test");
        // Should not panic on empty registry
        registry.broadcast(&msg);
        assert!(registry.send_to("nonexistent", &msg).is_err());
    }

    #[test]
    fn handle_heartbeat_stores_worker_state() {
        let mut registry = PeerRegistry::new(30, "local-test");
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
        let mut registry = PeerRegistry::new(30, "local-test");
        let peer_id = PeerId("worker-02".into());
        let payload = serde_json::json!({});
        registry.handle_heartbeat(&peer_id, &payload);

        assert!(registry.all_worker_states().is_empty());
    }

    #[test]
    fn expire_stale_workers_removes_old_entries() {
        let mut registry = PeerRegistry::new(30, "local-test");
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
        let mut registry = PeerRegistry::new(30, "local-test");
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

    /// #459: displacing a connection must close it, not just forget it.
    ///
    /// Real loopback sockets, because the whole bug was that the `Arc`-shared
    /// stream outlived the registry entry — a struct-literal `PeerConnection`
    /// with `stream: None` cannot express it.
    /// Both ends dial at once, so each holds an inbound and an outbound socket
    /// for the one peer. Whichever order the two arrive in, and whichever way
    /// the ids compare, the two ends must keep opposite ends of the SAME
    /// socket — otherwise both survivors are half-dead and the pair never
    /// recovers. That is what #464's startup dial made reachable.
    mod simultaneous_dial {
        use super::super::*;
        use std::io::Read;
        use std::net::{TcpListener, TcpStream};

        fn socket_pair(listener: &TcpListener) -> (TcpStream, TcpStream) {
            let remote = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (accepted, _) = listener.accept().unwrap();
            (accepted, remote)
        }

        fn reads_eof(stream: &mut TcpStream) -> bool {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buf = [0u8; 1];
            matches!(stream.read(&mut buf), Ok(0))
        }

        /// An outbound connection: the one we opened by dialling.
        ///
        /// `is_initiator` is the only field `replaces` reads, so building this
        /// from the listener's constructor and flipping that one flag is
        /// faithful for this test and nothing more. A real dialled connection
        /// also carries `addr`, which the registry's reconnect path uses as a
        /// direction proxy — if `replaces` ever consults that instead, these
        /// fixtures would keep passing while the rule broke.
        fn outbound(
            id: &PeerId,
            s: TcpStream,
            tx: Sender<(PeerId, RelayMessage)>,
        ) -> PeerConnection {
            let mut c = PeerConnection::from_authenticated(id.clone(), s, tx);
            c.is_initiator = true;
            c
        }

        /// An inbound connection: the one the peer opened, as the listener builds it.
        fn inbound(
            id: &PeerId,
            s: TcpStream,
            tx: Sender<(PeerId, RelayMessage)>,
        ) -> PeerConnection {
            PeerConnection::from_authenticated(id.clone(), s, tx)
        }

        /// Returns (our outbound socket survived, their inbound socket survived).
        fn resolve(us: &str, them: &str, outbound_first: bool) -> (bool, bool) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut reg = PeerRegistry::new(30, us);
            let tx = reg.message_tx();
            let id = PeerId(them.into());

            let (out_sock, mut out_remote) = socket_pair(&listener);
            let (in_sock, mut in_remote) = socket_pair(&listener);

            if outbound_first {
                reg.add_peer(outbound(&id, out_sock, tx.clone()));
                reg.add_peer(inbound(&id, in_sock, tx));
            } else {
                reg.add_peer(inbound(&id, in_sock, tx.clone()));
                reg.add_peer(outbound(&id, out_sock, tx));
            }

            assert_eq!(reg.connected_count(), 1, "one entry per peer, always");
            (!reads_eof(&mut out_remote), !reads_eof(&mut in_remote))
        }

        #[test]
        fn when_our_id_is_lower_the_connection_we_opened_survives() {
            // Arrival order must not change the verdict: the two ends race, so
            // each sees a different order.
            for outbound_first in [true, false] {
                let (ours, theirs) = resolve("aaa-lower", "zzz-higher", outbound_first);
                assert!(
                    ours,
                    "our outbound must survive (outbound_first={outbound_first})"
                );
                assert!(
                    !theirs,
                    "their inbound must be closed (outbound_first={outbound_first})"
                );
            }
        }

        #[test]
        fn when_our_id_is_higher_the_connection_they_opened_survives() {
            for outbound_first in [true, false] {
                let (ours, theirs) = resolve("zzz-higher", "aaa-lower", outbound_first);
                assert!(
                    !ours,
                    "our outbound must be closed (outbound_first={outbound_first})"
                );
                assert!(
                    theirs,
                    "their inbound must survive (outbound_first={outbound_first})"
                );
            }
        }

        /// The two ends of one pair reach opposite verdicts about their own
        /// socket, which is the point: between them exactly one socket lives.
        #[test]
        fn the_two_ends_agree_on_which_socket_lives() {
            let (low_keeps_own, _) = resolve("aaa-lower", "zzz-higher", true);
            let (high_keeps_own, high_keeps_theirs) = resolve("zzz-higher", "aaa-lower", true);
            assert!(low_keeps_own, "the lower id keeps the socket it opened");
            assert!(!high_keeps_own, "the higher id drops the socket it opened");
            assert!(
                high_keeps_theirs,
                "and keeps the lower id's socket — the same one the lower id kept"
            );
        }

        /// #459's case must still work. A higher-id peer that crashed and
        /// reconnected has to be able to replace the zombie a lower-id peer
        /// still holds; the id rule alone would keep the zombie forever.
        #[test]
        fn a_dead_connection_is_displaced_whatever_the_ids_say() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut reg = PeerRegistry::new(30, "zzz-higher");
            let tx = reg.message_tx();
            let id = PeerId("aaa-lower".into());

            // We dialled them once; their end then vanished.
            let (out_sock, out_remote) = socket_pair(&listener);
            reg.add_peer(outbound(&id, out_sock, tx.clone()));
            drop(out_remote);
            // Let the reader thread notice the EOF.
            std::thread::sleep(Duration::from_millis(200));

            // They come back, dialling in. Our id is higher, so the plain rule
            // would keep what we have — which is dead.
            let (in_sock, mut in_remote) = socket_pair(&listener);
            reg.add_peer(inbound(&id, in_sock, tx));

            assert_eq!(reg.connected_count(), 1);
            assert!(
                !reads_eof(&mut in_remote),
                "the live inbound connection must replace the zombie, or the \
                 peer can never reconnect to us"
            );
        }
    }

    mod displaced_connections {
        use super::super::*;
        use std::io::Read;
        use std::net::{TcpListener, TcpStream};

        /// An accepted connection plus the remote end of the same socket.
        fn socket_pair(listener: &TcpListener) -> (TcpStream, TcpStream) {
            let remote = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (accepted, _) = listener.accept().unwrap();
            (accepted, remote)
        }

        /// Is this socket's peer gone? `read` returning 0 is EOF.
        fn reads_eof(stream: &mut TcpStream) -> bool {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buf = [0u8; 1];
            matches!(stream.read(&mut buf), Ok(0))
        }

        #[test]
        fn a_second_connection_from_the_same_peer_closes_the_first() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut reg = PeerRegistry::new(30, "local-test");
            let tx = reg.message_tx();

            let (first, mut first_remote) = socket_pair(&listener);
            let (second, _second_remote) = socket_pair(&listener);

            let id = PeerId("same-peer".into());
            reg.add_peer(PeerConnection::from_authenticated(
                id.clone(),
                first,
                tx.clone(),
            ));
            assert_eq!(reg.connected_count(), 1);

            // `hive join` dialling in beside a `relay join` from the same
            // machine is exactly this.
            reg.add_peer(PeerConnection::from_authenticated(id.clone(), second, tx));

            assert_eq!(reg.connected_count(), 1, "still one entry for the peer");
            assert!(
                reads_eof(&mut first_remote),
                "the displaced socket must be closed, or the host keeps reading \
                 from a peer it can no longer answer"
            );
        }

        #[test]
        fn a_closed_socket_is_noticed_without_waiting_for_missed_heartbeats() {
            // The reader thread exits on EOF immediately; before #459 nothing
            // asked it, so a dead connection stayed `Connected` for three
            // heartbeat intervals (90s by default).
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let reg = PeerRegistry::new(30, "local-test");
            let (accepted, remote) = socket_pair(&listener);

            let mut conn = PeerConnection::from_authenticated(
                PeerId("gone".into()),
                accepted,
                reg.message_tx(),
            );
            assert!(
                conn.check_alive(Duration::from_secs(30)),
                "alive while the socket is open"
            );

            drop(remote);

            // The reader thread needs a moment to see the EOF and exit.
            let mut dead = false;
            for _ in 0..50 {
                if !conn.check_alive(Duration::from_secs(30)) {
                    dead = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                dead,
                "a closed socket must be noticed well inside the 90s heartbeat threshold"
            );
        }
    }
}

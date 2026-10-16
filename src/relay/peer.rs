// Single peer connection: connect, send, read, heartbeat, reconnect.

use std::io::{self, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::features::Features;
use super::protocol;
use super::{PeerId, RelayMessage};

/// Connection state for a single peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Disconnected,
    /// Never constructed: `connect` blocks for up to ten seconds and returns
    /// an already-`Connected` peer, and `from_authenticated` starts connected
    /// too, so nothing observes a peer mid-dial. Surfaced by the #465 audit
    /// when `PeerState::label` — its only other reader — was removed; kept
    /// because the dial path is where this should be set, not deleted to
    /// silence the warning.
    #[allow(dead_code, reason = "no dial path sets it yet; see the note above")]
    Connecting,
    Connected,
}

/// A connection to a single remote peer.
pub struct PeerConnection {
    pub peer_id: PeerId,
    pub state: PeerState,
    pub addr: Option<SocketAddr>,
    stream: Option<Arc<Mutex<TcpStream>>>,
    reader_handle: Option<JoinHandle<()>>,
    pub last_heartbeat_sent: Instant,
    pub last_heartbeat_recv: Instant,
    pub missed_heartbeats: u32,
    /// Whether this side initiated the connection (for reconnect responsibility).
    pub is_initiator: bool,
    /// Reconnect backoff state.
    pub reconnect_attempts: u32,
    pub next_reconnect_at: Option<Instant>,
    /// What this link agreed to in the handshake (#559). Empty for a peer
    /// from before #559, and for a placeholder. Anything new on the wire is
    /// sent only when this says the peer has it.
    pub features: Features,
}

impl PeerConnection {
    /// Create a new PeerConnection from an already-authenticated stream, with
    /// no negotiated features: NDJSON, as every peer from before #559 speaks.
    /// Tests only; production goes through `from_authenticated_with`.
    #[cfg(test)]
    pub fn from_authenticated(
        peer_id: PeerId,
        stream: TcpStream,
        tx: Sender<(PeerId, RelayMessage)>,
    ) -> Self {
        Self::from_authenticated_with(peer_id, stream, tx, Features::default())
    }

    /// Create a PeerConnection with what its handshake agreed. The listener's
    /// path. The features go in before the reader thread starts, because they
    /// decide the framing it reads with (#562): a reader started on NDJSON for
    /// a link that agreed binary frames would misread every byte.
    pub fn from_authenticated_with(
        peer_id: PeerId,
        stream: TcpStream,
        tx: Sender<(PeerId, RelayMessage)>,
        features: Features,
    ) -> Self {
        let now = Instant::now();
        let stream_arc = Arc::new(Mutex::new(stream));

        let mut conn = PeerConnection {
            peer_id: peer_id.clone(),
            state: PeerState::Connected,
            addr: None,
            stream: Some(Arc::clone(&stream_arc)),
            reader_handle: None,
            last_heartbeat_sent: now,
            last_heartbeat_recv: now,
            missed_heartbeats: 0,
            is_initiator: false,
            reconnect_attempts: 0,
            next_reconnect_at: None,
            features,
        };

        conn.spawn_reader(stream_arc, tx);
        conn
    }

    /// How this link frames its bytes, from what its handshake agreed.
    pub fn framing(&self) -> protocol::Framing {
        protocol::Framing::for_link(&self.features)
    }

    /// A peer we know about and have not reached (#543).
    ///
    /// `spawn_startup_dials` used to drop a peer whose every address failed,
    /// and `tick`'s reconnect arm only acts on peers that are *in* the registry
    /// and disconnected — so nothing retried it, and an address learned later
    /// (#506, #541) had nothing to act on it. A placeholder gives the existing
    /// backoff something to drive.
    ///
    /// It holds no socket, so it can only ever be a reconnect target: `tick`
    /// heartbeats `Connected` peers, `send_to` refuses anything else, and
    /// `add_peer` always lets a real connection displace it.
    pub fn pending(peer_id: PeerId, addr: Option<SocketAddr>) -> Self {
        let now = Instant::now();
        PeerConnection {
            peer_id,
            state: PeerState::Disconnected,
            addr,
            stream: None,
            reader_handle: None,
            last_heartbeat_sent: now,
            last_heartbeat_recv: now,
            missed_heartbeats: 0,
            // The whole point: `should_reconnect` gates on this.
            is_initiator: true,
            reconnect_attempts: 0,
            next_reconnect_at: None,
            features: Features::default(),
        }
    }

    /// Whether this entry is a placeholder rather than a live connection.
    ///
    /// A placeholder must never win `add_peer`'s collision rule: it has no
    /// socket, so keeping it over a real inbound connection would discard the
    /// only working link. `reader_alive` cannot answer this — it reports `true`
    /// when there is no reader at all, which is exactly a placeholder.
    pub fn is_placeholder(&self) -> bool {
        self.stream.is_none() && self.reader_handle.is_none()
    }

    /// Connect to a remote peer (client-side).
    /// Dial a peer. `intent` tells the acceptor what kind of connection this
    /// is — a lasting link or a one-shot delivery — and what address it could
    /// dial us back at. See `protocol::DialIntent`.
    pub fn connect(
        addr: SocketAddr,
        psk: &[u8; 32],
        identity: &PeerId,
        tx: Sender<(PeerId, RelayMessage)>,
        intent: &protocol::DialIntent,
    ) -> Result<Self, String> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
            .map_err(|e| format!("connect to {addr}: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| format!("set read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("set write timeout: {e}"))?;

        // Capacity 1: a buffered reader would read past the ack, and this one
        // is dropped when the handshake ends. Bytes the acceptor sends right
        // after its ack would vanish with it, and under `frame-bin` a lost
        // partial frame desyncs every frame after it (#562).
        let mut reader = BufReader::with_capacity(
            1,
            stream
                .try_clone()
                .map_err(|e| format!("clone stream: {e}"))?,
        );

        // Read challenge
        let challenge = protocol::read_message(&mut reader)
            .map_err(|e| format!("read challenge: {e}"))?
            .ok_or("connection closed before challenge")?;

        let nonce = challenge
            .payload
            .get("nonce")
            .and_then(|v| v.as_str())
            .ok_or("challenge missing nonce")?;

        // Send handshake proof
        let mut write_stream = stream
            .try_clone()
            .map_err(|e| format!("clone for write: {e}"))?;
        protocol::send_handshake(&mut write_stream, identity.as_str(), nonce, psk, intent)
            .map_err(|e| format!("send handshake: {e}"))?;

        // Await ack
        let (remote_peer_id, features) =
            protocol::await_handshake_ack(&mut reader).map_err(|e| format!("auth failed: {e}"))?;

        let now = Instant::now();
        let stream_arc = Arc::new(Mutex::new(stream));

        let mut conn = PeerConnection {
            peer_id: PeerId(remote_peer_id),
            state: PeerState::Connected,
            addr: Some(addr),
            stream: Some(Arc::clone(&stream_arc)),
            reader_handle: None,
            last_heartbeat_sent: now,
            last_heartbeat_recv: now,
            missed_heartbeats: 0,
            is_initiator: true,
            reconnect_attempts: 0,
            next_reconnect_at: None,
            features,
        };

        conn.spawn_reader(stream_arc, tx);
        Ok(conn)
    }

    /// Whether this connection's reader thread is still running.
    ///
    /// The reader exits on EOF, so a finished reader means the socket is gone
    /// even if `state` still says `Connected` and no heartbeats have been
    /// missed yet.
    pub fn reader_alive(&self) -> bool {
        !self.reader_handle.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Close this connection's socket.
    ///
    /// `stream` is an `Arc` shared with the reader thread, which holds its own
    /// `try_clone` of the same socket, so dropping a `PeerConnection` closes
    /// nothing — the reader keeps reading and the socket stays open with no
    /// way to write to it. `shutdown(Both)` acts on the socket rather than on
    /// one descriptor, so it ends the reader's blocking read and sends a FIN to
    /// the other end.
    pub fn shutdown(&self) {
        if let Some(stream) = self.stream.as_ref() {
            if let Ok(guard) = stream.lock() {
                let _ = guard.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// Half-close the sending side and wait for the peer to finish consuming
    /// what was written.
    ///
    /// `write_all` returning means the kernel took the bytes, not that the
    /// peer read them. That difference is exactly how #487 stayed invisible:
    /// the write succeeded, the peer hung up, and the caller was told the
    /// message had been delivered. A fixed sleep cannot stand in for this
    /// once the payload is megabytes rather than one frame.
    ///
    /// Shutting down only the write half leaves the peer's reader able to
    /// drain every frame still in flight; it sees EOF after the last one,
    /// exits, and drops its socket handle. That close is what ends this
    /// side's reader thread, so this thread finishing is the signal that the
    /// peer consumed the whole stream.
    pub fn finish_sending(&self, timeout: std::time::Duration) -> Result<(), String> {
        if let Some(stream) = self.stream.as_ref() {
            if let Ok(guard) = stream.lock() {
                guard
                    .shutdown(std::net::Shutdown::Write)
                    .map_err(|e| format!("half-close: {e}"))?;
            }
        }
        let Some(handle) = self.reader_handle.as_ref() else {
            // No reader to wait on; nothing can be confirmed either way.
            return Ok(());
        };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if handle.is_finished() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        Err(format!("peer did not finish reading within {:?}", timeout))
    }

    /// Send a message to this peer.
    pub fn send(&self, msg: &RelayMessage) -> io::Result<()> {
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| io::Error::other("not connected"))?;
        let framing = self.framing();
        let mut guard = stream
            .lock()
            .map_err(|_| io::Error::other("stream lock poisoned"))?;
        protocol::write_frame(&mut *guard, msg, framing)
    }

    /// Send a heartbeat (liveness only).
    pub fn send_heartbeat(&mut self, identity: &str) -> io::Result<()> {
        let msg = protocol::heartbeat_message(identity);
        self.send(&msg)?;
        self.last_heartbeat_sent = Instant::now();
        Ok(())
    }

    /// Send a heartbeat carrying session state.
    /// Heartbeat carrying sessions and this machine's capability manifest.
    pub fn send_heartbeat_with(
        &mut self,
        identity: &str,
        sessions: &[serde_json::Value],
        capabilities: Option<&serde_json::Value>,
        listen_addrs: &[String],
        headroom: Option<&serde_json::Value>,
    ) -> io::Result<()> {
        // Only a link that agreed to it gets the headroom key.
        let headroom = headroom.filter(|_| self.features.has(super::features::HEADROOM));
        let msg =
            protocol::heartbeat_full(identity, sessions, capabilities, listen_addrs, headroom);
        self.send(&msg)?;
        self.last_heartbeat_sent = Instant::now();
        Ok(())
    }

    /// Record that a heartbeat was received from this peer.
    pub fn record_heartbeat(&mut self) {
        self.last_heartbeat_recv = Instant::now();
        self.missed_heartbeats = 0;
    }

    /// Check if the peer is alive based on heartbeat timing.
    /// Returns false if 3 heartbeats have been missed.
    pub fn check_alive(&mut self, heartbeat_interval: Duration) -> bool {
        if self.state != PeerState::Connected {
            return false;
        }
        // The reader thread breaks out of its loop on EOF or a socket error,
        // so a finished handle *is* the connection being gone — known the
        // moment it happens, where the heartbeat threshold below takes three
        // intervals (90s by default) to reach the same conclusion.
        if self.reader_handle.as_ref().is_some_and(|h| h.is_finished()) {
            self.missed_heartbeats = 3;
            return false;
        }
        let elapsed = self.last_heartbeat_recv.elapsed();
        let threshold = heartbeat_interval * 3;
        if elapsed > threshold {
            self.missed_heartbeats = 3;
            false
        } else {
            true
        }
    }

    /// Mark this peer as disconnected.
    pub fn mark_disconnected(&mut self) {
        self.state = PeerState::Disconnected;
        self.stream = None;
        // Don't join the reader — it will exit on its own when the stream closes
    }

    /// Calculate next reconnect delay with exponential backoff.
    pub fn reconnect_delay(&self) -> Duration {
        let base = 5u64;
        let max = 60u64;
        let delay = base.saturating_mul(1u64 << self.reconnect_attempts.min(4));
        Duration::from_secs(delay.min(max))
    }

    /// Whether this peer should attempt to reconnect now.
    pub fn should_reconnect(&self) -> bool {
        if !self.is_initiator || self.state != PeerState::Disconnected {
            return false;
        }
        match self.next_reconnect_at {
            Some(at) => Instant::now() >= at,
            None => true,
        }
    }

    /// Schedule the next reconnect attempt.
    pub fn schedule_reconnect(&mut self) {
        self.reconnect_attempts += 1;
        self.next_reconnect_at = Some(Instant::now() + self.reconnect_delay());
    }

    /// Spawn a reader thread that reads messages and sends them to the channel.
    fn spawn_reader(&mut self, stream: Arc<Mutex<TcpStream>>, tx: Sender<(PeerId, RelayMessage)>) {
        let peer_id = self.peer_id.clone();
        // Fixed for the life of the connection: a link's features don't
        // change after its handshake.
        let framing = self.framing();

        let handle = std::thread::spawn(move || {
            // Clone the TcpStream for the reader
            let raw_stream = {
                let guard = match stream.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                match guard.try_clone() {
                    Ok(s) => s,
                    Err(_) => return,
                }
            };

            let mut reader = BufReader::new(raw_stream);
            loop {
                match protocol::read_frame(&mut reader, framing) {
                    Ok(Some(msg)) => {
                        if tx.send((peer_id.clone(), msg)).is_err() {
                            break; // Receiver dropped
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(e) => {
                        let kind = e.kind();
                        if kind == io::ErrorKind::TimedOut || kind == io::ErrorKind::WouldBlock {
                            continue; // Read timeout, try again
                        }
                        break; // Connection error
                    }
                }
            }
        });

        self.reader_handle = Some(handle);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_delay_exponential_backoff() {
        let mut conn = PeerConnection {
            peer_id: PeerId("test".into()),
            state: PeerState::Disconnected,
            addr: None,
            stream: None,
            reader_handle: None,
            last_heartbeat_sent: Instant::now(),
            last_heartbeat_recv: Instant::now(),
            missed_heartbeats: 0,
            is_initiator: true,
            reconnect_attempts: 0,
            next_reconnect_at: None,
            features: Features::default(),
        };

        assert_eq!(conn.reconnect_delay(), Duration::from_secs(5));
        conn.reconnect_attempts = 1;
        assert_eq!(conn.reconnect_delay(), Duration::from_secs(10));
        conn.reconnect_attempts = 2;
        assert_eq!(conn.reconnect_delay(), Duration::from_secs(20));
        conn.reconnect_attempts = 3;
        assert_eq!(conn.reconnect_delay(), Duration::from_secs(40));
        conn.reconnect_attempts = 4;
        assert_eq!(conn.reconnect_delay(), Duration::from_secs(60)); // capped
        conn.reconnect_attempts = 10;
        assert_eq!(conn.reconnect_delay(), Duration::from_secs(60)); // still capped
    }

    #[test]
    fn should_reconnect_only_if_initiator() {
        let conn = PeerConnection {
            peer_id: PeerId("test".into()),
            state: PeerState::Disconnected,
            addr: None,
            stream: None,
            reader_handle: None,
            last_heartbeat_sent: Instant::now(),
            last_heartbeat_recv: Instant::now(),
            missed_heartbeats: 0,
            is_initiator: false, // Not initiator
            reconnect_attempts: 0,
            next_reconnect_at: None,
            features: Features::default(),
        };
        assert!(!conn.should_reconnect());

        let conn2 = PeerConnection {
            is_initiator: true,
            ..PeerConnection {
                peer_id: PeerId("test2".into()),
                state: PeerState::Disconnected,
                addr: None,
                stream: None,
                reader_handle: None,
                last_heartbeat_sent: Instant::now(),
                last_heartbeat_recv: Instant::now(),
                missed_heartbeats: 0,
                is_initiator: true,
                reconnect_attempts: 0,
                next_reconnect_at: None,
                features: Features::default(),
            }
        };
        assert!(conn2.should_reconnect());
    }

    #[test]
    fn schedule_reconnect_keeps_peer_retriable() {
        let mut conn = PeerConnection {
            peer_id: PeerId("test".into()),
            state: PeerState::Disconnected,
            addr: None,
            stream: None,
            reader_handle: None,
            last_heartbeat_sent: Instant::now(),
            last_heartbeat_recv: Instant::now(),
            missed_heartbeats: 0,
            is_initiator: true,
            reconnect_attempts: 0,
            next_reconnect_at: None,
            features: Features::default(),
        };

        conn.schedule_reconnect();
        assert_eq!(conn.state, PeerState::Disconnected);
        assert!(conn.next_reconnect_at.is_some());
    }

    // ── End to end over loopback (#562) ─────────────────────────────────
    //
    // The dialler is the real `PeerConnection::connect`. The acceptor runs
    // the listener's own steps with the same protocol functions, so no test
    // needs a HOME to look a PSK up in.

    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::mpsc::{Receiver, channel};

    const WAIT: Duration = Duration::from_secs(5);

    fn heartbeat(from: &str, payload: serde_json::Value) -> RelayMessage {
        RelayMessage {
            id: super::super::gen_msg_id(),
            msg_type: super::super::MessageType::Heartbeat,
            from_peer: from.into(),
            timestamp: 0,
            payload,
        }
    }

    fn big() -> serde_json::Value {
        serde_json::json!({"sessions": "project staffai status Idle cost 1.23 ".repeat(300)})
    }

    /// Accept one connection the way `listener::handle_incoming` does. If
    /// `then_send` is given, it's written the instant the ack is out, before
    /// the connection object even exists.
    fn accept_like_the_listener(
        listener: &TcpListener,
        psk: [u8; 32],
        then_send: Option<RelayMessage>,
    ) -> (PeerConnection, Receiver<(PeerId, RelayMessage)>) {
        let (stream, _) = listener.accept().unwrap();
        let mut write = stream.try_clone().unwrap();
        let mut reader = BufReader::with_capacity(1, stream);
        let nonce = protocol::send_challenge(&mut write).unwrap();
        let hs = protocol::read_message(&mut reader).unwrap().unwrap();
        let peer = protocol::verify_handshake(&hs, &nonce, &psk).unwrap();
        // The ack and any first frame go out in a single write, so they
        // reach the dialler in one segment, which is the case a buffered
        // handshake reader gets wrong.
        let mut out = Vec::new();
        let features = protocol::send_handshake_ack_negotiated(&mut out, "acceptor", &hs).unwrap();
        if let Some(msg) = then_send {
            protocol::write_frame(&mut out, &msg, protocol::Framing::for_link(&features)).unwrap();
        }
        write.write_all(&out).unwrap();
        let (tx, rx) = channel();
        let conn = PeerConnection::from_authenticated_with(
            PeerId(peer),
            reader.into_inner(),
            tx,
            features,
        );
        (conn, rx)
    }

    fn dial(
        addr: SocketAddr,
        psk: &[u8; 32],
    ) -> (PeerConnection, Receiver<(PeerId, RelayMessage)>) {
        let (tx, rx) = channel();
        let conn = PeerConnection::connect(
            addr,
            psk,
            &PeerId("dialler".into()),
            tx,
            &protocol::DialIntent::peer_link(None),
        )
        .unwrap();
        (conn, rx)
    }

    #[test]
    fn two_new_peers_agree_binary_and_exchange_compressed_frames_both_ways() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let psk = super::super::crypto::generate_psk();
        let acceptor = std::thread::spawn(move || accept_like_the_listener(&listener, psk, None));
        let (client, client_rx) = dial(addr, &psk);
        let (server, server_rx) = acceptor.join().unwrap();

        assert_eq!(client.framing(), protocol::Framing::Binary);
        assert_eq!(server.framing(), protocol::Framing::Binary);

        // Dialler → acceptor, large then small, order kept.
        client.send(&heartbeat("dialler", big())).unwrap();
        client
            .send(&heartbeat("dialler", serde_json::json!({"n": 2})))
            .unwrap();
        let (_, a) = server_rx.recv_timeout(WAIT).unwrap();
        let (_, b) = server_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(a.payload, big());
        assert_eq!(b.payload, serde_json::json!({"n": 2}));

        // Acceptor → dialler.
        server.send(&heartbeat("acceptor", big())).unwrap();
        let (_, c) = client_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(c.payload, big());

        // Many frames back to back, mixed sizes.
        for i in 0..200 {
            let p = if i % 3 == 0 {
                big()
            } else {
                serde_json::json!({"i": i})
            };
            client.send(&heartbeat("dialler", p)).unwrap();
        }
        for i in 0..200 {
            let (_, m) = server_rx.recv_timeout(WAIT).unwrap();
            let want = if i % 3 == 0 {
                big()
            } else {
                serde_json::json!({"i": i})
            };
            assert_eq!(m.payload, want, "frame {i}");
        }
        client.shutdown();
        server.shutdown();
    }

    /// The acceptor's first frame can arrive in the same TCP segment as its
    /// ack. A buffered handshake reader would swallow it and the link would
    /// start mid-frame.
    #[test]
    fn a_frame_sent_right_after_the_ack_is_not_lost() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let psk = super::super::crypto::generate_psk();
        let first = heartbeat("acceptor", big());
        let want = first.payload.clone();
        let acceptor =
            std::thread::spawn(move || accept_like_the_listener(&listener, psk, Some(first)));
        let (client, client_rx) = dial(addr, &psk);
        let (server, _) = acceptor.join().unwrap();
        let (_, got) = client_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(got.payload, want);
        client.shutdown();
        server.shutdown();
    }

    /// A 0.80 dialler: NDJSON handshake with no `proto`, NDJSON frames after.
    #[test]
    fn an_old_dialler_stays_on_ndjson_with_a_new_acceptor() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let psk = super::super::crypto::generate_psk();
        let acceptor = std::thread::spawn(move || accept_like_the_listener(&listener, psk, None));

        let old = TcpStream::connect(addr).unwrap();
        let mut old_w = old.try_clone().unwrap();
        let mut old_r = BufReader::new(old);
        let challenge = protocol::read_message(&mut old_r).unwrap().unwrap();
        let nonce = challenge.payload["nonce"].as_str().unwrap().to_string();
        let hs = RelayMessage {
            payload: serde_json::json!({
                "proof": protocol::compute_proof(&nonce, &psk),
                "version": "0.80.0",
            }),
            msg_type: super::super::MessageType::Handshake,
            ..heartbeat("old-peer", serde_json::json!({}))
        };
        protocol::write_message(&mut old_w, &hs).unwrap();
        let ack = protocol::read_message(&mut old_r).unwrap().unwrap();
        assert_eq!(ack.payload, serde_json::json!({"status": "ok"}));

        let (server, server_rx) = acceptor.join().unwrap();
        assert_eq!(server.framing(), protocol::Framing::Lines);

        protocol::write_message(&mut old_w, &heartbeat("old-peer", big())).unwrap();
        let (_, got) = server_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(got.payload, big());

        server.send(&heartbeat("acceptor", big())).unwrap();
        let back = protocol::read_message(&mut old_r).unwrap().unwrap();
        assert_eq!(back.payload, big());
        server.shutdown();
    }

    /// A 0.80 acceptor answers with the old ack; the new dialler must stay on
    /// NDJSON, or it would send it binary frames it can't read.
    #[test]
    fn a_new_dialler_stays_on_ndjson_with_an_old_acceptor() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let psk = super::super::crypto::generate_psk();
        let old_acceptor = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut w = stream.try_clone().unwrap();
            let mut r = BufReader::new(stream);
            let nonce = protocol::send_challenge(&mut w).unwrap();
            let hs = protocol::read_message(&mut r).unwrap().unwrap();
            protocol::verify_handshake(&hs, &nonce, &psk).unwrap();
            protocol::send_handshake_ack(&mut w, "old-acceptor", "ok").unwrap();
            // What the old relay reads next must be a line.
            let got = protocol::read_message(&mut r).unwrap().unwrap();
            protocol::write_message(&mut w, &heartbeat("old-acceptor", big())).unwrap();
            got
        });
        let (client, client_rx) = dial(addr, &psk);
        assert_eq!(client.framing(), protocol::Framing::Lines);
        client.send(&heartbeat("dialler", big())).unwrap();
        let got = old_acceptor.join().unwrap();
        assert_eq!(got.payload, big());
        let (_, back) = client_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(back.payload, big());
        client.shutdown();
    }
}

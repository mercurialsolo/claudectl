// Single peer connection: connect, send, read, heartbeat, reconnect.

use std::io::{self, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::protocol;
use super::{PeerId, RelayMessage};

/// Connection state for a single peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Disconnected,
    Connecting,
    Connected,
}

impl PeerState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
        }
    }
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
}

impl PeerConnection {
    /// Create a new PeerConnection from an already-authenticated stream.
    /// Used by the listener after successful auth.
    pub fn from_authenticated(
        peer_id: PeerId,
        stream: TcpStream,
        tx: Sender<(PeerId, RelayMessage)>,
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
        };

        conn.spawn_reader(stream_arc, tx);
        conn
    }

    /// Connect to a remote peer (client-side).
    pub fn connect(
        addr: SocketAddr,
        psk: &[u8; 32],
        identity: &PeerId,
        tx: Sender<(PeerId, RelayMessage)>,
    ) -> Result<Self, String> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
            .map_err(|e| format!("connect to {addr}: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| format!("set read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("set write timeout: {e}"))?;

        let mut reader = BufReader::new(
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
        protocol::send_handshake(&mut write_stream, identity.as_str(), nonce, psk)
            .map_err(|e| format!("send handshake: {e}"))?;

        // Await ack
        let remote_peer_id =
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
        };

        conn.spawn_reader(stream_arc, tx);
        Ok(conn)
    }

    /// Send a message to this peer.
    pub fn send(&self, msg: &RelayMessage) -> io::Result<()> {
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| io::Error::other("not connected"))?;
        let mut guard = stream
            .lock()
            .map_err(|_| io::Error::other("stream lock poisoned"))?;
        protocol::write_message(&mut guard, msg)
    }

    /// Send a heartbeat.
    pub fn send_heartbeat(&mut self, identity: &str) -> io::Result<()> {
        let msg = protocol::heartbeat_message(identity);
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

    /// Reset reconnect state after a successful connection.
    pub fn reset_reconnect(&mut self) {
        self.reconnect_attempts = 0;
        self.next_reconnect_at = None;
    }

    /// Spawn a reader thread that reads messages and sends them to the channel.
    fn spawn_reader(&mut self, stream: Arc<Mutex<TcpStream>>, tx: Sender<(PeerId, RelayMessage)>) {
        let peer_id = self.peer_id.clone();

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
                match protocol::read_message(&mut reader) {
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
    fn peer_state_labels() {
        assert_eq!(PeerState::Disconnected.label(), "disconnected");
        assert_eq!(PeerState::Connecting.label(), "connecting");
        assert_eq!(PeerState::Connected.label(), "connected");
    }

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
        };

        conn.schedule_reconnect();
        assert_eq!(conn.state, PeerState::Disconnected);
        assert!(conn.next_reconnect_at.is_some());
    }
}

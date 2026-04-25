// TcpListener accept loop: authenticates incoming peers and adds them to the mesh.

use std::collections::HashMap;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::mesh::PeerRegistry;
use super::peer::PeerConnection;
use super::protocol;
use super::{
    PENDING_PEER_ID, PeerId, clear_pending_psk, is_valid_peer_id, load_peer_psk, load_pending_psk,
    save_peer_psk,
};

/// Maximum concurrent auth threads (prevents connection flood).
const MAX_AUTH_THREADS: usize = 16;

/// Cooldown after N failed auth attempts from the same IP.
const AUTH_FAIL_LIMIT: u32 = 5;
/// How long to block an IP after hitting the fail limit.
const AUTH_COOLDOWN: Duration = Duration::from_secs(60);

/// A running relay listener.
pub struct RelayListener {
    pub addr: SocketAddr,
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl RelayListener {
    /// Start listening for incoming peer connections.
    pub fn start(
        addr: SocketAddr,
        registry: Arc<Mutex<PeerRegistry>>,
        identity: PeerId,
        max_peers: u8,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let local_addr = listener.local_addr()?;

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let auth_threads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fail_tracker: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let handle = std::thread::spawn(move || {
            let _ = listener.set_nonblocking(true);

            loop {
                if shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                match listener.accept() {
                    Ok((stream, peer_addr)) => {
                        // Check auth thread limit
                        let current = auth_threads.load(std::sync::atomic::Ordering::Relaxed);
                        if current >= MAX_AUTH_THREADS {
                            crate::logger::log(
                                "RELAY",
                                &format!(
                                    "rejecting connection from {peer_addr}: too many auth threads"
                                ),
                            );
                            drop(stream);
                            continue;
                        }

                        // Check max peers
                        if let Ok(reg) = registry.lock() {
                            if reg.connected_count() >= max_peers as usize {
                                crate::logger::log(
                                    "RELAY",
                                    &format!(
                                        "rejecting connection from {peer_addr}: max peers reached"
                                    ),
                                );
                                drop(stream);
                                continue;
                            }
                        }

                        // Check rate limiting
                        let ip = peer_addr.ip();
                        if let Ok(tracker) = fail_tracker.lock() {
                            if let Some((count, since)) = tracker.get(&ip) {
                                if *count >= AUTH_FAIL_LIMIT && since.elapsed() < AUTH_COOLDOWN {
                                    crate::logger::log(
                                        "RELAY",
                                        &format!(
                                            "rejecting connection from {peer_addr}: rate limited"
                                        ),
                                    );
                                    drop(stream);
                                    continue;
                                }
                            }
                        }

                        let registry = Arc::clone(&registry);
                        let identity = identity.clone();
                        let thread_count = Arc::clone(&auth_threads);
                        let fail_track = Arc::clone(&fail_tracker);

                        thread_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                        std::thread::spawn(move || {
                            let success = handle_incoming(stream, peer_addr, &registry, &identity);

                            if !success {
                                // Track failed auth attempt
                                if let Ok(mut tracker) = fail_track.lock() {
                                    let entry = tracker
                                        .entry(peer_addr.ip())
                                        .or_insert((0, Instant::now()));
                                    // Reset counter if cooldown has passed
                                    if entry.1.elapsed() >= AUTH_COOLDOWN {
                                        *entry = (1, Instant::now());
                                    } else {
                                        entry.0 += 1;
                                    }
                                }
                            }

                            thread_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        });

        Ok(RelayListener {
            addr: local_addr,
            handle: Some(handle),
            shutdown,
        })
    }

    /// Signal the listener to stop.
    pub fn stop(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for RelayListener {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Handle an incoming TCP connection: authenticate, then add to registry.
/// Returns true on successful auth, false on failure.
fn handle_incoming(
    stream: TcpStream,
    peer_addr: SocketAddr,
    registry: &Arc<Mutex<PeerRegistry>>,
    identity: &PeerId,
) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let mut write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut reader = BufReader::new(stream);

    // Step 1: Send challenge
    let nonce = match protocol::send_challenge(&mut write_stream) {
        Ok(n) => n,
        Err(e) => {
            crate::logger::log(
                "RELAY",
                &format!("challenge send failed from {peer_addr}: {e}"),
            );
            return false;
        }
    };

    // Step 2: Read handshake response
    let handshake_msg = match protocol::read_message(&mut reader) {
        Ok(Some(msg)) => msg,
        Ok(None) => return false,
        Err(e) => {
            crate::logger::log(
                "RELAY",
                &format!("handshake read failed from {peer_addr}: {e}"),
            );
            return false;
        }
    };

    // Step 3: Authenticate using only the key stored for the claimed peer ID.
    // A peer that knows one valid PSK must not be able to authenticate as a
    // different peer by changing the from_peer field.
    let claimed_peer = handshake_msg.from_peer.clone();
    if !is_valid_peer_id(&claimed_peer) || claimed_peer == PENDING_PEER_ID {
        let _ = protocol::send_handshake_ack(&mut write_stream, identity.as_str(), "denied");
        crate::logger::log(
            "RELAY",
            &format!("auth failed from {peer_addr}: invalid peer id '{claimed_peer}'"),
        );
        return false;
    }

    let remote_peer_id = if let Some(psk) = load_peer_psk(&claimed_peer) {
        match protocol::verify_handshake(&handshake_msg, &nonce, &psk) {
            Ok(id) if id == claimed_peer => id,
            Ok(id) => {
                let _ =
                    protocol::send_handshake_ack(&mut write_stream, identity.as_str(), "denied");
                crate::logger::log(
                    "RELAY",
                    &format!(
                        "auth failed from {peer_addr}: peer id mismatch '{id}' != '{claimed_peer}'"
                    ),
                );
                return false;
            }
            Err(_) => {
                let _ =
                    protocol::send_handshake_ack(&mut write_stream, identity.as_str(), "denied");
                crate::logger::log(
                    "RELAY",
                    &format!("auth failed from {peer_addr}: no matching PSK"),
                );
                return false;
            }
        }
    } else if let Some(psk) = load_pending_psk() {
        match protocol::verify_handshake(&handshake_msg, &nonce, &psk) {
            Ok(id) if id == claimed_peer => {
                if let Err(e) = save_peer_psk(&claimed_peer, &psk) {
                    let _ = protocol::send_handshake_ack(
                        &mut write_stream,
                        identity.as_str(),
                        "denied",
                    );
                    crate::logger::log(
                        "RELAY",
                        &format!("auth failed from {peer_addr}: could not save peer key: {e}"),
                    );
                    return false;
                }
                clear_pending_psk();
                id
            }
            _ => {
                let _ =
                    protocol::send_handshake_ack(&mut write_stream, identity.as_str(), "denied");
                crate::logger::log(
                    "RELAY",
                    &format!("auth failed from {peer_addr}: unknown peer"),
                );
                return false;
            }
        }
    } else {
        let _ = protocol::send_handshake_ack(&mut write_stream, identity.as_str(), "denied");
        crate::logger::log(
            "RELAY",
            &format!("auth failed from {peer_addr}: unknown peer"),
        );
        return false;
    };

    // Step 4: Send ack
    if let Err(e) = protocol::send_handshake_ack(&mut write_stream, identity.as_str(), "ok") {
        crate::logger::log("RELAY", &format!("ack send failed to {peer_addr}: {e}"));
        return false;
    }

    // Step 5: Reconstruct the underlying TcpStream from the reader
    let stream = reader.into_inner();

    // Step 6: Add to registry
    let peer_id = PeerId(remote_peer_id);
    let tx = {
        let reg = match registry.lock() {
            Ok(r) => r,
            Err(_) => return false,
        };
        reg.message_tx()
    };

    let conn = PeerConnection::from_authenticated(peer_id.clone(), stream, tx);

    if let Ok(mut reg) = registry.lock() {
        reg.add_peer(conn);
        crate::logger::log(
            "RELAY",
            &format!("peer {} connected from {}", peer_id, peer_addr),
        );
    }

    true
}

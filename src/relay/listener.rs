// TcpListener accept loop: authenticates incoming peers and adds them to the mesh.

use std::io::BufReader;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::mesh::PeerRegistry;
use super::peer::PeerConnection;
use super::protocol;
use super::{PeerId, list_known_peers, load_peer_psk};

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
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let local_addr = listener.local_addr()?;
        listener.set_nonblocking(false)?;

        // Use SO_REUSEADDR
        // Already handled by TcpListener::bind on most platforms

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            // Set a timeout on accept so we can check shutdown periodically
            let _ = listener.set_nonblocking(true);

            loop {
                if shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                match listener.accept() {
                    Ok((stream, peer_addr)) => {
                        let registry = Arc::clone(&registry);
                        let identity = identity.clone();

                        std::thread::spawn(move || {
                            handle_incoming(stream, peer_addr, &registry, &identity);
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
fn handle_incoming(
    stream: TcpStream,
    peer_addr: SocketAddr,
    registry: &Arc<Mutex<PeerRegistry>>,
    identity: &PeerId,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let mut write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
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
            return;
        }
    };

    // Step 2: Read handshake response
    let handshake_msg = match protocol::read_message(&mut reader) {
        Ok(Some(msg)) => msg,
        Ok(None) => return,
        Err(e) => {
            crate::logger::log(
                "RELAY",
                &format!("handshake read failed from {peer_addr}: {e}"),
            );
            return;
        }
    };

    // Step 3: Try each known PSK until one works
    let known_peers = list_known_peers();
    let mut authed_peer_id = None;

    for known_id in &known_peers {
        if let Some(psk) = load_peer_psk(known_id) {
            if let Ok(remote_id) = protocol::verify_handshake(&handshake_msg, &nonce, &psk) {
                authed_peer_id = Some(remote_id);
                break;
            }
        }
    }

    let remote_peer_id = match authed_peer_id {
        Some(id) => id,
        None => {
            // Try using the from_peer field as the peer ID to look up the PSK
            if let Some(psk) = load_peer_psk(&handshake_msg.from_peer) {
                match protocol::verify_handshake(&handshake_msg, &nonce, &psk) {
                    Ok(id) => id,
                    Err(_) => {
                        let _ = protocol::send_handshake_ack(
                            &mut write_stream,
                            identity.as_str(),
                            "denied",
                        );
                        crate::logger::log(
                            "RELAY",
                            &format!("auth failed from {peer_addr}: no matching PSK"),
                        );
                        return;
                    }
                }
            } else {
                let _ =
                    protocol::send_handshake_ack(&mut write_stream, identity.as_str(), "denied");
                crate::logger::log(
                    "RELAY",
                    &format!("auth failed from {peer_addr}: unknown peer"),
                );
                return;
            }
        }
    };

    // Step 4: Send ack
    if let Err(e) = protocol::send_handshake_ack(&mut write_stream, identity.as_str(), "ok") {
        crate::logger::log("RELAY", &format!("ack send failed to {peer_addr}: {e}"));
        return;
    }

    // Step 5: Reconstruct the underlying TcpStream from the reader
    let stream = reader.into_inner();

    // Step 6: Add to registry
    let peer_id = PeerId(remote_peer_id);
    let tx = {
        let reg = match registry.lock() {
            Ok(r) => r,
            Err(_) => return,
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
}

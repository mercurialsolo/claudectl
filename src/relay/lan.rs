// LAN broadcast discovery: find nearby claudectl instances via UDP.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use super::PeerId;

const LAN_PORT: u16 = 9848;
const ANNOUNCE_MAGIC: &[u8; 4] = b"CCTL";
const STALE_AFTER: Duration = Duration::from_secs(30);

// ────────────────────────────────────────────────────────────────────────────
// Discovered peer info
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub identity: String,
    pub addr: SocketAddr,
    pub relay_port: u16,
    pub version: String,
    pub last_seen: Instant,
}

impl DiscoveredPeer {
    pub fn is_stale(&self) -> bool {
        self.last_seen.elapsed() > STALE_AFTER
    }

    /// The relay address to connect to (peer IP + relay port).
    pub fn relay_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr.ip(), self.relay_port)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Announcer: broadcast our presence on the LAN
// ────────────────────────────────────────────────────────────────────────────

/// Build an announcement payload.
fn build_announcement(identity: &str, relay_port: u16, version: &str) -> Vec<u8> {
    let json = serde_json::json!({
        "identity": identity,
        "port": relay_port,
        "version": version,
    });
    let json_bytes = serde_json::to_vec(&json).unwrap_or_default();

    let mut payload = Vec::with_capacity(4 + json_bytes.len());
    payload.extend_from_slice(ANNOUNCE_MAGIC);
    payload.extend_from_slice(&json_bytes);
    payload
}

/// Parse an announcement payload.
fn parse_announcement(data: &[u8]) -> Option<(String, u16, String)> {
    if data.len() < 5 || &data[..4] != ANNOUNCE_MAGIC {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&data[4..]).ok()?;
    let identity = json.get("identity")?.as_str()?.to_string();
    let port = json.get("port")?.as_u64()? as u16;
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Some((identity, port, version))
}

/// Send a single UDP broadcast announcement.
pub fn send_announcement(identity: &str, relay_port: u16) -> Result<(), String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    socket
        .set_broadcast(true)
        .map_err(|e| format!("set_broadcast: {e}"))?;

    let payload = build_announcement(identity, relay_port, env!("CARGO_PKG_VERSION"));
    let broadcast_addr = SocketAddr::new(Ipv4Addr::BROADCAST.into(), LAN_PORT);

    socket
        .send_to(&payload, broadcast_addr)
        .map_err(|e| format!("send: {e}"))?;

    Ok(())
}

/// Start a background announcer thread that broadcasts periodically.
pub fn start_announcer(
    identity: PeerId,
    relay_port: u16,
    interval_secs: u64,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = send_announcement(identity.as_str(), relay_port);
            std::thread::sleep(Duration::from_secs(interval_secs));
        }
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Scanner: listen for nearby announcements
// ────────────────────────────────────────────────────────────────────────────

/// Scan the LAN for claudectl instances. Listens for `duration` seconds.
pub fn scan_lan(duration: Duration, own_identity: &str) -> Vec<DiscoveredPeer> {
    let socket = match UdpSocket::bind(format!("0.0.0.0:{LAN_PORT}")) {
        Ok(s) => s,
        Err(e) => {
            crate::logger::log("LAN", &format!("bind failed on port {LAN_PORT}: {e}"));
            return Vec::new();
        }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));

    let mut peers: HashMap<String, DiscoveredPeer> = HashMap::new();
    let start = Instant::now();
    let mut buf = [0u8; 1024];

    while start.elapsed() < duration {
        match socket.recv_from(&mut buf) {
            Ok((n, from_addr)) => {
                if let Some((identity, relay_port, version)) = parse_announcement(&buf[..n]) {
                    // Don't discover ourselves
                    if identity == own_identity {
                        continue;
                    }
                    peers.insert(
                        identity.clone(),
                        DiscoveredPeer {
                            identity,
                            addr: from_addr,
                            relay_port,
                            version,
                            last_seen: Instant::now(),
                        },
                    );
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }

    peers.into_values().collect()
}

/// Start a background listener that accumulates discovered peers.
/// Returns a shared peer map that the main loop can read.
pub fn start_listener(
    own_identity: String,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::sync::Arc<std::sync::Mutex<HashMap<String, DiscoveredPeer>>> {
    let peers = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    let peers_clone = std::sync::Arc::clone(&peers);

    std::thread::spawn(move || {
        let socket = match UdpSocket::bind(format!("0.0.0.0:{LAN_PORT}")) {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
        let mut buf = [0u8; 1024];

        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            match socket.recv_from(&mut buf) {
                Ok((n, from_addr)) => {
                    if let Some((identity, relay_port, version)) = parse_announcement(&buf[..n]) {
                        if identity == own_identity {
                            continue;
                        }
                        if let Ok(mut map) = peers_clone.lock() {
                            map.insert(
                                identity.clone(),
                                DiscoveredPeer {
                                    identity,
                                    addr: from_addr,
                                    relay_port,
                                    version,
                                    last_seen: Instant::now(),
                                },
                            );
                            // Prune stale entries
                            map.retain(|_, p| !p.is_stale());
                        }
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    continue;
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    });

    peers
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcement_roundtrip() {
        let payload = build_announcement("laptop-a3f2", 9847, "0.36.0");
        let (identity, port, version) = parse_announcement(&payload).unwrap();
        assert_eq!(identity, "laptop-a3f2");
        assert_eq!(port, 9847);
        assert_eq!(version, "0.36.0");
    }

    #[test]
    fn announcement_magic_check() {
        assert!(parse_announcement(b"BADDATA").is_none());
        assert!(parse_announcement(b"").is_none());
        assert!(parse_announcement(b"CCT").is_none());
    }

    #[test]
    fn announcement_bad_json() {
        let mut payload = Vec::new();
        payload.extend_from_slice(ANNOUNCE_MAGIC);
        payload.extend_from_slice(b"not json");
        assert!(parse_announcement(&payload).is_none());
    }

    #[test]
    fn discovered_peer_staleness() {
        let peer = DiscoveredPeer {
            identity: "test".into(),
            addr: "127.0.0.1:9848".parse().unwrap(),
            relay_port: 9847,
            version: "0.36.0".into(),
            last_seen: Instant::now(),
        };
        assert!(!peer.is_stale());

        let old_peer = DiscoveredPeer {
            last_seen: Instant::now() - Duration::from_secs(60),
            ..peer
        };
        assert!(old_peer.is_stale());
    }

    #[test]
    fn relay_addr_uses_relay_port() {
        let peer = DiscoveredPeer {
            identity: "test".into(),
            addr: "192.168.1.50:9848".parse().unwrap(), // discovery port
            relay_port: 9847,                           // relay port
            version: "0.36.0".into(),
            last_seen: Instant::now(),
        };
        let relay = peer.relay_addr();
        assert_eq!(relay.port(), 9847);
        assert_eq!(relay.ip().to_string(), "192.168.1.50");
    }
}

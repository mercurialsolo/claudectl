// LAN broadcast discovery: find nearby claudectl instances via UDP.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use super::PeerId;

pub const LAN_PORT: u16 = 9848;
const ANNOUNCE_MAGIC: &[u8; 4] = b"CCTL";
/// How long a discovered peer stays in `start_listener`'s map.
///
/// Only `start_listener` accumulates peers over time — `scan_lan`, which
/// `relay discover` uses, returns what it heard in one window and keeps
/// nothing. `start_listener` keeps a map instead, and expires from it with
/// this (#506).
const STALE_AFTER: Duration = Duration::from_secs(30);

/// How often `relay serve` broadcasts its presence.
///
/// Discovery is passive: a scanner only hears a peer that happens to announce
/// while it is listening. So this and [`SCAN_DURATION`] are a pair and live
/// together — a scan shorter than the announce interval misses peers at
/// roughly `1 - scan/interval`, which is what a 3-second scan against a
/// 5-second announcer would have done.
pub const ANNOUNCE_INTERVAL_SECS: u64 = 5;

/// How long `relay discover` listens.
///
/// One second longer than the announce interval, so every announcing peer is
/// heard at least once rather than most of the time.
pub const SCAN_DURATION: Duration = Duration::from_secs(ANNOUNCE_INTERVAL_SECS + 1);

// ────────────────────────────────────────────────────────────────────────────
// Discovered peer info
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub identity: String,
    pub addr: SocketAddr,
    pub relay_port: u16,
    pub version: String,
    /// When this peer was last heard from. Read by `is_stale` to expire
    /// entries from the long-lived map `start_listener` keeps (#506).
    pub last_seen: Instant,
    /// The hive this machine advertises, if it named one.
    pub hive: Option<HiveAd>,
}

impl DiscoveredPeer {
    /// Whether this entry is old enough to drop from a long-lived map.
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

/// The hive block a named machine adds to its announcement (#433, RFC §7.3).
///
/// Additive fields on the existing datagram, so a peer on an older build parses
/// the three keys it knows and ignores this entirely — asserted by a test that
/// runs the pre-#433 extraction against a payload carrying one.
///
/// `description` is deliberately **not** advertised. It is up to 200 bytes, the
/// receive buffer is 1 KB, and a discovery listing shows a name and counts
/// rather than prose.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HiveAd {
    pub id: String,
    pub name: String,
    /// `invite`, `ask` or `open`.
    ///
    /// This is the **effective** policy, never the stored one: an `open` that
    /// was never confirmed advertises as `invite`. See
    /// `hive::identity::HiveIdentity::effective_join_policy` — the gate #432 put
    /// on the data rather than on its CLI exists exactly so this code path
    /// cannot leak an unconsented `open` onto the network.
    pub join_policy: String,
    /// Connected peers, and knowledge units held. Advisory — they are a snapshot
    /// from whenever the announcer last ticked.
    #[serde(default)]
    pub peers: u32,
    #[serde(default)]
    pub units: u32,
}

/// One decoded announcement.
///
/// A struct rather than the tuple this used to return: #433 adds a fourth field
/// and a growing tuple at three call sites is how a field gets dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub identity: String,
    pub relay_port: u16,
    pub version: String,
    /// `None` when the sender's hive is unnamed, which is every machine that has
    /// not run `hive identity set`. Absence is the default and means "no hive to
    /// advertise", never an error.
    pub hive: Option<HiveAd>,
}

/// Build an announcement payload.
fn build_announcement(
    identity: &str,
    relay_port: u16,
    version: &str,
    hive: Option<&HiveAd>,
) -> Vec<u8> {
    let mut json = serde_json::json!({
        "identity": identity,
        "port": relay_port,
        "version": version,
    });
    // Added only when there is a named hive, so an unnamed machine's datagram is
    // byte-identical to what it sent before #433.
    if let Some(h) = hive {
        if let Some(obj) = json.as_object_mut() {
            obj.insert(
                "hive".to_string(),
                serde_json::to_value(h).unwrap_or(serde_json::Value::Null),
            );
        }
    }
    let json_bytes = serde_json::to_vec(&json).unwrap_or_default();

    let mut payload = Vec::with_capacity(4 + json_bytes.len());
    payload.extend_from_slice(ANNOUNCE_MAGIC);
    payload.extend_from_slice(&json_bytes);
    payload
}

/// Parse an announcement payload.
fn parse_announcement(data: &[u8]) -> Option<Announcement> {
    if data.len() < 5 || &data[..4] != ANNOUNCE_MAGIC {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&data[4..]).ok()?;
    let identity = json.get("identity")?.as_str()?.to_string();
    let relay_port = json.get("port")?.as_u64()? as u16;
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    // A malformed hive block costs the hive block, not the whole announcement:
    // the machine is still worth discovering, and a future field this build does
    // not understand must not make a peer vanish.
    let hive = json
        .get("hive")
        .and_then(|h| serde_json::from_value::<HiveAd>(h.clone()).ok())
        .filter(|h| !h.name.is_empty() && !h.id.is_empty());
    Some(Announcement {
        identity,
        relay_port,
        version,
        hive,
    })
}

/// Send a single UDP broadcast announcement.
pub fn send_announcement(
    identity: &str,
    relay_port: u16,
    hive: Option<&HiveAd>,
) -> Result<(), String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    socket
        .set_broadcast(true)
        .map_err(|e| format!("set_broadcast: {e}"))?;

    let payload = build_announcement(identity, relay_port, env!("CARGO_PKG_VERSION"), hive);
    let broadcast_addr = SocketAddr::new(Ipv4Addr::BROADCAST.into(), LAN_PORT);

    socket
        .send_to(&payload, broadcast_addr)
        .map_err(|e| format!("send: {e}"))?;

    Ok(())
}

/// The hive identity and live counts an announcer should advertise (#433).
///
/// The name and policy are fixed at `relay serve` startup — the identity file is
/// read once, like `query serve` reads its config once, so renaming a hive takes
/// a restart. The counts move, so they are atomics the serve loop stores into
/// and the announcer reads: no lock is shared with a thread that is about to do
/// a blocking `send_to`.
#[derive(Clone)]
pub struct HiveAdvert {
    pub id: String,
    pub name: String,
    pub join_policy: String,
    pub peers: std::sync::Arc<std::sync::atomic::AtomicU32>,
    pub units: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl HiveAdvert {
    /// Snapshot the current counts into a wire record.
    pub fn snapshot(&self) -> HiveAd {
        use std::sync::atomic::Ordering;
        HiveAd {
            id: self.id.clone(),
            name: self.name.clone(),
            join_policy: self.join_policy.clone(),
            peers: self.peers.load(Ordering::Relaxed),
            units: self.units.load(Ordering::Relaxed),
        }
    }
}

/// Start a background announcer thread that broadcasts periodically.
pub fn start_announcer(
    identity: PeerId,
    relay_port: u16,
    interval_secs: u64,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    hive: Option<HiveAdvert>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // A send failure used to be discarded with `let _ =`. Nothing called
        // this function at all, so it never mattered — but the moment it is
        // wired up, a silently failing broadcast is indistinguishable from a
        // working one, which is exactly the shape of the bug #433 had to find by
        // packet-sniffing. Log the first failure and then stay quiet, so a
        // machine with no broadcast route says so once instead of every tick.
        let mut warned = false;
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            // Snapshotted each tick, so the counts a peer sees are at most one
            // interval stale rather than frozen at startup.
            let ad = hive.as_ref().map(|h| h.snapshot());
            match send_announcement(identity.as_str(), relay_port, ad.as_ref()) {
                Ok(()) => warned = false,
                Err(e) if !warned => {
                    crate::logger::log("LAN", &format!("announcement failed: {e}"));
                    eprintln!(
                        "warning: LAN announcement failed ({e}); peers will not discover this machine"
                    );
                    warned = true;
                }
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_secs(interval_secs));
        }
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Scanner: listen for nearby announcements
// ────────────────────────────────────────────────────────────────────────────

/// Scan the LAN for claudectl instances. Listens for `duration` seconds.
/// Bind the discovery port so several readers on one machine can share it.
///
/// Needed by #506. `relay serve` holds this socket for its whole life, and
/// `relay discover` binds the same port from a separate process. `UdpSocket::bind`
/// gives neither `SO_REUSEADDR` nor `SO_REUSEPORT`, so the second bind fails —
/// which would make `discover` on any machine running a relay report
/// "No instances found". That is the #433 symptom, reintroduced by fixing #506.
///
/// Both options are set, not just `SO_REUSEPORT`: on Linux `SO_REUSEPORT`
/// alone is enough for multiple binds, on macOS/BSD the pair is the portable
/// incantation. `libc` is already a dependency and already used this way
/// (`bus/roles.rs`, `relay/agent.rs`, `core/process.rs`), so this adds no crate.
fn bind_shared(port: u16) -> std::io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;

    // SAFETY: the fd is wrapped in a `UdpSocket` immediately after creation, so
    // every later error path closes it by drop rather than leaking. The
    // `sockaddr_in` is zeroed first, which sets `sin_len` correctly on the BSD
    // layouts that have the field and is ignored on the ones that do not.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let socket = UdpSocket::from_raw_fd(fd);

        let on: libc::c_int = 1;
        for opt in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
            let rc = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                std::ptr::addr_of!(on).cast(),
                std::mem::size_of_val(&on) as libc::socklen_t,
            );
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_port = port.to_be();
        addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();
        let rc = libc::bind(
            fd,
            std::ptr::addr_of!(addr).cast(),
            std::mem::size_of_val(&addr) as libc::socklen_t,
        );
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(socket)
    }
}

pub fn scan_lan(duration: Duration, own_identity: &str) -> Vec<DiscoveredPeer> {
    let socket = match bind_shared(LAN_PORT) {
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
                if let Some(ann) = parse_announcement(&buf[..n]) {
                    // Don't discover ourselves
                    if ann.identity == own_identity {
                        continue;
                    }
                    peers.insert(
                        ann.identity.clone(),
                        DiscoveredPeer {
                            identity: ann.identity,
                            addr: from_addr,
                            relay_port: ann.relay_port,
                            version: ann.version,
                            last_seen: Instant::now(),
                            hive: ann.hive,
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
/// Returns a shared peer map that the caller reads on its own schedule.
///
/// This is continuous discovery: unlike `scan_lan`, which listens for one
/// window, this keeps a map up to date for as long as it runs. Two callers:
/// `relay serve` (#506, which is what lets a paired peer that changed network
/// be found again without a re-invite) and the TUI's Hive tab through
/// `LiveDiscovery` in `src/runtime/hive.rs`.
///
/// The socket is bound here, on the caller's thread, so a port that cannot
/// be bound is an error the caller sees rather than a thread that exits in
/// silence and a map that stays empty forever. Whoever holds this port
/// should also call [`publish_seen`], so a second process reads instead of
/// binding beside it (see the note above `SEEN_FRESH_SECS`).
pub fn start_listener(
    own_identity: String,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<std::sync::Arc<std::sync::Mutex<HashMap<String, DiscoveredPeer>>>> {
    let socket = bind_shared(LAN_PORT)?;
    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));

    let peers = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    let peers_clone = std::sync::Arc::clone(&peers);

    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];

        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            match socket.recv_from(&mut buf) {
                Ok((n, from_addr)) => {
                    if let Some(ann) = parse_announcement(&buf[..n]) {
                        if ann.identity == own_identity {
                            continue;
                        }
                        if let Ok(mut map) = peers_clone.lock() {
                            map.insert(
                                ann.identity.clone(),
                                DiscoveredPeer {
                                    identity: ann.identity,
                                    addr: from_addr,
                                    relay_port: ann.relay_port,
                                    version: ann.version,
                                    last_seen: Instant::now(),
                                    hive: ann.hive,
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

    Ok(peers)
}

// ────────────────────────────────────────────────────────────────────────────
// Publishing what a running relay has heard
// ────────────────────────────────────────────────────────────────────────────

// Why this exists, since it looks like duplication of `scan_lan`:
//
// Once `relay serve` holds the discovery port continuously (#506), a second
// process binding the same port does *not* get a copy of each announcement.
// `SO_REUSEPORT` load-balances datagrams across the sockets bound to a port
// rather than duplicating them, so the long-lived listener wins most of them
// and `relay discover` — a six-second window — can easily see none at all.
// Measured, not assumed: with a relay serving, `discover` reported "No
// claudectl instances found" while the serving process was logging the very
// announcements it needed; stopping the relay made `discover` work instantly.
//
// So the serving process publishes what it hears, and `discover` reads that
// when a relay is running. This mirrors `fleet.json` (`core/fleet.rs`): one
// writer, atomic rename, a staleness gate on the reader. It is also strictly
// better than the old behaviour — the answer is continuous rather than
// whatever happened to arrive inside one window, and it returns immediately.

/// How long a published file is trusted before the reader falls back to its own
/// scan. Three announce intervals: long enough to survive a missed broadcast,
/// short enough that a stopped relay is noticed quickly.
pub const SEEN_FRESH_SECS: u64 = ANNOUNCE_INTERVAL_SECS * 3;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SeenPeer {
    pub identity: String,
    pub addr: String,
    pub relay_port: u16,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hive: Option<HiveAd>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SeenSnapshot {
    pub written_ms: u64,
    pub peers: Vec<SeenPeer>,
    /// The process that wrote this. A holder of the port that also publishes
    /// (the TUI's Hive tab) reads the file back on its next tick, and must
    /// not mistake its own writing for another process having taken over.
    /// Absent in files written before this field existed, which is `0`.
    #[serde(default)]
    pub writer_pid: u32,
}

fn seen_path() -> std::path::PathBuf {
    super::relay_dir().join("discovered.json")
}

impl DiscoveredPeer {
    /// Rebuild from a published entry (#506), so `relay discover` can render
    /// the running relay's view through exactly the same path as a live scan.
    ///
    /// `last_seen` becomes "now": the publisher already dropped stale entries,
    /// and the reader's staleness gate is the file's own age.
    pub fn from_seen(seen: &SeenPeer) -> Self {
        let addr = seen
            .addr
            .parse::<SocketAddr>()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], seen.relay_port)));
        DiscoveredPeer {
            identity: seen.identity.clone(),
            addr,
            relay_port: seen.relay_port,
            version: seen.version.clone(),
            last_seen: Instant::now(),
            hive: seen.hive.clone(),
        }
    }
}

/// Publish what this process has heard. Best-effort: discovery is a
/// convenience, and a failed write must never disturb the serve loop.
pub fn publish_seen(map: &HashMap<String, DiscoveredPeer>) {
    let peers: Vec<SeenPeer> = map
        .values()
        .filter(|p| !p.is_stale())
        .map(|p| SeenPeer {
            identity: p.identity.clone(),
            addr: p.relay_addr().to_string(),
            relay_port: p.relay_port,
            version: p.version.clone(),
            hive: p.hive.clone(),
        })
        .collect();
    let snapshot = SeenSnapshot {
        written_ms: super::epoch_ms(),
        peers,
        writer_pid: std::process::id(),
    };
    let dir = super::relay_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(body) = serde_json::to_string(&snapshot) else {
        return;
    };
    let tmp = dir.join(".discovered.json.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, seen_path());
    }
}

/// What a running relay has heard, or `None` when there is no relay, the file
/// is unreadable, or it is old enough that whoever wrote it has clearly stopped.
pub fn read_seen() -> Option<SeenSnapshot> {
    let body = std::fs::read_to_string(seen_path()).ok()?;
    let snap: SeenSnapshot = serde_json::from_str(&body).ok()?;
    let age_ms = super::epoch_ms().saturating_sub(snap.written_ms);
    if age_ms > SEEN_FRESH_SECS * 1000 {
        return None;
    }
    Some(snap)
}

/// The peers in a published snapshot, minus this machine, in the form a live
/// scan would have produced. `relay discover`, `hive discover` and the TUI
/// all read one snapshot with [`read_seen`], decide what to tell the operator,
/// and then render through here — one read per command, so what the header
/// says and what the listing came from cannot disagree across the freshness
/// edge.
pub fn peers_from_seen(snapshot: &SeenSnapshot, own_identity: &str) -> Vec<DiscoveredPeer> {
    snapshot
        .peers
        .iter()
        .filter(|p| p.identity != own_identity)
        .map(DiscoveredPeer::from_seen)
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason `bind_shared` exists: `relay serve` holds the discovery
    /// port for its lifetime once #506 wires the listener, and `relay discover`
    /// binds the same port from another process. A plain `UdpSocket::bind`
    /// fails the second time, which is how `discover` would start reporting
    /// "No instances found" on any machine running a relay.
    #[test]
    fn two_readers_can_hold_the_discovery_port_at_once() {
        // A high port, so a developer machine already running a relay on the
        // real LAN_PORT does not make this pass or fail for the wrong reason.
        let port = 47_653;
        let first = match bind_shared(port) {
            Ok(s) => s,
            // A sandbox with no AF_INET at all; nothing to assert.
            Err(_) => return,
        };
        let second = bind_shared(port).expect("a second reader must be able to bind");
        assert_eq!(first.local_addr().unwrap().port(), port);
        assert_eq!(second.local_addr().unwrap().port(), port);
    }

    #[test]
    fn a_plain_bind_on_the_same_port_is_what_fails() {
        let port = 47_654;
        let Ok(_shared) = bind_shared(port) else {
            return;
        };
        // Documents the behaviour being worked around: without the options,
        // the second bind is refused. If a future std gains reuse by default
        // this assertion flips and `bind_shared` can go.
        assert!(
            UdpSocket::bind(format!("0.0.0.0:{port}")).is_err(),
            "plain bind unexpectedly succeeded; bind_shared may be unnecessary now"
        );
    }

    #[test]
    fn announcement_roundtrip() {
        let payload = build_announcement("laptop-a3f2", 9847, "0.36.0", None);
        let ann = parse_announcement(&payload).unwrap();
        assert_eq!(ann.identity, "laptop-a3f2");
        assert_eq!(ann.relay_port, 9847);
        assert_eq!(ann.version, "0.36.0");
        assert_eq!(ann.hive, None, "an unnamed hive must advertise nothing");
    }

    fn sample_ad() -> HiveAd {
        HiveAd {
            id: "hv_3a9f21".into(),
            name: "barrys-hive".into(),
            join_policy: "invite".into(),
            peers: 3,
            units: 412,
        }
    }

    /// Extract the three fields exactly as the pre-#433 parser did.
    ///
    /// This *is* the old implementation, kept as a test fixture rather than a
    /// comment, so "a peer on an older build is unaffected" is a thing the suite
    /// checks instead of a thing the PR asserts.
    fn parse_v0(data: &[u8]) -> Option<(String, u16, String)> {
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

    #[test]
    fn a_pre_433_payload_still_parses_with_no_hive() {
        // A literal fixture of the shape that has been on the wire all along —
        // not one this build produced, so a change to `build_announcement`
        // cannot make this test vacuous.
        let mut payload = Vec::new();
        payload.extend_from_slice(ANNOUNCE_MAGIC);
        payload.extend_from_slice(br#"{"identity":"laptop-a3f2","port":9847,"version":"0.64.0"}"#);
        let ann = parse_announcement(&payload).expect("the old shape must still parse");
        assert_eq!(ann.identity, "laptop-a3f2");
        assert_eq!(ann.relay_port, 9847);
        assert_eq!(ann.version, "0.64.0");
        assert_eq!(ann.hive, None);
    }

    #[test]
    fn an_older_build_is_unaffected_by_the_hive_block() {
        // The acceptance criterion, run rather than argued: the pre-#433
        // extraction against a *new* payload carrying a hive block.
        let payload = build_announcement("laptop-a3f2", 9847, "0.66.0", Some(&sample_ad()));
        let (identity, port, version) =
            parse_v0(&payload).expect("an old build must still read a new datagram");
        assert_eq!(identity, "laptop-a3f2");
        assert_eq!(port, 9847);
        assert_eq!(version, "0.66.0");
    }

    #[test]
    fn a_hive_block_round_trips() {
        let payload = build_announcement("laptop-a3f2", 9847, "0.66.0", Some(&sample_ad()));
        let ann = parse_announcement(&payload).unwrap();
        assert_eq!(ann.hive, Some(sample_ad()));
    }

    #[test]
    fn an_unnamed_machine_adds_no_hive_key_at_all() {
        // Not merely `hive: None` on parse — the key must be absent from the
        // bytes, so an unnamed machine's datagram is what it always was.
        let payload = build_announcement("laptop-a3f2", 9847, "0.66.0", None);
        let text = String::from_utf8_lossy(&payload[4..]);
        assert!(!text.contains("hive"), "{text}");
    }

    #[test]
    fn a_malformed_hive_block_costs_the_block_not_the_peer() {
        // A machine running a future build that adds a field, or a corrupted
        // datagram, must still be discoverable.
        let mut payload = Vec::new();
        payload.extend_from_slice(ANNOUNCE_MAGIC);
        payload.extend_from_slice(
            br#"{"identity":"laptop-a3f2","port":9847,"version":"0.66.0","hive":"not-an-object"}"#,
        );
        let ann = parse_announcement(&payload).expect("the peer must survive a bad hive block");
        assert_eq!(ann.identity, "laptop-a3f2");
        assert_eq!(ann.hive, None);

        // An object missing the fields that identify a hive is also no hive.
        let mut partial = Vec::new();
        partial.extend_from_slice(ANNOUNCE_MAGIC);
        partial.extend_from_slice(
            br#"{"identity":"x","port":1,"version":"v","hive":{"id":"","name":"","join_policy":"open"}}"#,
        );
        assert_eq!(parse_announcement(&partial).unwrap().hive, None);
    }

    /// Must equal `hive::identity::MAX_NAME_LEN`, which a relay-only build
    /// cannot see. Pinned by the test below.
    const MAX_HIVE_NAME_LEN: usize = 64;

    #[cfg(feature = "hive")]
    #[test]
    fn the_name_cap_matches_the_hive_one() {
        assert_eq!(
            MAX_HIVE_NAME_LEN,
            crate::hive::identity::MAX_NAME_LEN,
            "the local copy has drifted, so the worst-case payload test below \
             is no longer testing the worst case"
        );
    }

    #[test]
    fn the_worst_case_payload_fits_the_receive_buffer() {
        // The scanner reads into a 1 KB buffer, so a datagram larger than that is
        // silently truncated and then fails to parse. Check the largest thing
        // this code can emit, rather than assuming a name and two integers are
        // small.
        // `hive::identity::MAX_NAME_LEN` is not reachable in a relay-only
        // build, but a relay-only peer still receives ads from hive-enabled
        // ones, so the worst case matters here too and the test must not be
        // gated away. `the_name_cap_matches_the_hive_one` keeps the two in step
        // wherever both are compiled.
        let name = "n".repeat(MAX_HIVE_NAME_LEN);
        let ad = HiveAd {
            id: "hv_ffffff".into(),
            name,
            join_policy: "invite".into(),
            peers: u32::MAX,
            units: u32::MAX,
        };
        // Longest plausible identity: `is_valid_peer_id` caps it, and the real
        // ones are `<host>-<8 hex>`.
        let identity = "x".repeat(64);
        let payload = build_announcement(&identity, u16::MAX, "999.999.999", Some(&ad));
        assert!(
            payload.len() < 1024,
            "worst-case announcement is {} bytes, which the 1 KB scan buffer truncates",
            payload.len()
        );
        // And it still parses at that size.
        assert_eq!(parse_announcement(&payload).unwrap().hive, Some(ad));
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
            hive: None,
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
            hive: None,
        };
        let relay = peer.relay_addr();
        assert_eq!(relay.port(), 9847);
        assert_eq!(relay.ip().to_string(), "192.168.1.50");
    }
}

pub mod advertise;
pub mod agent;
pub mod cli;
pub mod crypto;
pub mod delegation;
/// The hive join handshake — needs the hive module, which `relay` does not imply.
#[cfg(feature = "hive")]
pub mod hivejoin;
/// The gossip side of both relay loops. Same gating as `hivejoin`.
#[cfg(feature = "hive")]
pub mod hivesync;
pub mod http;
pub mod invite;
pub mod lan;
pub mod listener;
pub mod mesh;
pub mod outcome;
pub mod peer;
pub mod protocol;
pub mod tasks;
/// Moving a conversation to another host (#478 item 3).
pub mod transfer;
pub mod transient;
pub mod worker;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Unique identity for a claudectl instance in the relay network.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub String);

/// Temporary peer ID used while a pairing code has not yet been claimed.
pub const PENDING_PEER_ID: &str = "_pending";

impl PeerId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every message over the relay wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayMessage {
    pub id: String,
    pub msg_type: MessageType,
    pub from_peer: String,
    pub timestamp: u64,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    // Layer 1: transport
    Challenge,
    Handshake,
    HandshakeAck,
    Heartbeat,
    Ack,

    // Layer 2: coordination (Phase 2)
    DelegateTask,
    TaskStatus,
    TaskHandoff,
    TaskInterrupt,

    // Layer 3: hive (Phase 4)
    KnowledgeSync,
    KnowledgeRequest,
    KnowledgeSnapshot,

    // Layer 3: hive membership (#434)
    HiveJoinRequest,
    HiveJoinResult,
    /// Knowledge was refused rather than merged (#435) — a reader tried to
    /// contribute, or a non-member did. Sent so the sender finds out, instead
    /// of its units vanishing into a silence it cannot tell from a network
    /// fault.
    KnowledgeRejected,

    // Layer 2: conversation transfer (#478 item 3)
    /// One chunk of a session transcript. A transcript is median 5.5 MB and
    /// `protocol::MAX_LINE_SIZE` is 1 MiB, so it never fits in one message.
    SessionTransfer,
    /// The receiver placed a transferred transcript and says where, plus the
    /// command to resume it. Sent because placement is load-bearing and only
    /// the receiver knows the path it chose.
    SessionReceived,
}

static MSG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique message ID.
pub fn gen_msg_id() -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seq = MSG_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("msg_{epoch}_{seq}")
}

/// Current epoch milliseconds.
pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ────────────────────────────────────────────────────────────────────────────
// Identity and peer persistence
// ────────────────────────────────────────────────────────────────────────────

fn relay_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claudectl").join("relay")
}

fn identity_path() -> PathBuf {
    relay_dir().join("identity")
}

pub fn peers_dir() -> PathBuf {
    relay_dir().join("peers")
}

/// Load the local PeerId from disk, or create one on first run.
pub fn load_or_create_identity() -> PeerId {
    let path = identity_path();
    if let Ok(content) = fs::read_to_string(&path) {
        let id = content.trim().to_string();
        if !id.is_empty() {
            return PeerId(id);
        }
    }

    // Generate: hostname + 4 random hex chars
    let hostname = hostname_short();
    let suffix = crypto::random_hex(4);
    let id = format!("{hostname}-{suffix}");

    let dir = relay_dir();
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(&path, &id);

    PeerId(id)
}

/// Short hostname (first component, lowercased).
fn hostname_short() -> String {
    let full = std::env::var("HOSTNAME")
        .or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|_| "unknown".into());
    let short = full.split('.').next().unwrap_or("unknown").to_lowercase();
    let sanitized: String = short
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "unknown".into()
    } else {
        sanitized.to_string()
    }
}

/// Validate a peer ID before using it in filesystem paths or trust decisions.
pub fn is_valid_peer_id(peer_id: &str) -> bool {
    !peer_id.is_empty()
        && peer_id.len() <= 128
        && peer_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn peer_key_path(peer_id: &str) -> Option<PathBuf> {
    if is_valid_peer_id(peer_id) {
        Some(peers_dir().join(format!("{peer_id}.key")))
    } else {
        None
    }
}

fn peer_meta_path(peer_id: &str) -> Option<PathBuf> {
    if is_valid_peer_id(peer_id) {
        Some(peers_dir().join(format!("{peer_id}.meta")))
    } else {
        None
    }
}

/// Load a stored PSK for a peer, or None if not paired.
pub fn load_peer_psk(peer_id: &str) -> Option<[u8; 32]> {
    let path = peer_key_path(peer_id)?;
    let content = fs::read_to_string(&path).ok()?;
    crypto::hex_decode(content.trim()).ok().and_then(|bytes| {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Some(arr)
        } else {
            None
        }
    })
}

/// Store a PSK for a peer (chmod 600).
pub fn save_peer_psk(peer_id: &str, psk: &[u8; 32]) -> Result<(), String> {
    let path = peer_key_path(peer_id).ok_or_else(|| format!("invalid peer id: {peer_id}"))?;
    let dir = peers_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create peers dir: {e}"))?;
    let hex = crypto::hex_encode(psk);
    fs::write(&path, &hex).map_err(|e| format!("write PSK: {e}"))?;

    // chmod 600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = fs::set_permissions(&path, perms);
    }

    Ok(())
}

/// How many addresses to remember per peer. A machine realistically appears on
/// a handful at once — a LAN address, a VPN address, maybe a second interface —
/// and an unbounded list would mean dialling through every address the peer has
/// ever had before reaching the one it is on now.
const MAX_PEER_ADDRS: usize = 8;

/// Record that we have seen this peer at `addr`, keeping the addresses we saw
/// it at before (#484).
///
/// This used to rewrite the whole record, so the newest address replaced every
/// earlier one and a laptop that moved networks had nothing left to try. The
/// addresses are now a list, most-recent first, so a peer reachable on a LAN
/// and over a VPN accumulates both and either can be dialled.
///
/// `addr` keeps its old meaning — the newest address — so every existing
/// reader is unaffected; `addrs` carries the history. A record written before
/// this change has no `addrs` and reads as a single-address list.
pub fn save_peer_meta(peer_id: &str, addr: &str) -> Result<(), String> {
    let path = peer_meta_path(peer_id).ok_or_else(|| format!("invalid peer id: {peer_id}"))?;
    let dir = peers_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create peers dir: {e}"))?;

    let now = epoch_ms();
    let mut addrs = vec![serde_json::json!({ "addr": addr, "last_seen": now })];
    for known in peer_addrs(load_peer_meta(peer_id).as_ref()) {
        if known.addr == addr {
            continue; // already at the front, with a fresher timestamp
        }
        addrs.push(serde_json::json!({
            "addr": known.addr,
            "last_seen": known.last_seen,
        }));
    }
    addrs.truncate(MAX_PEER_ADDRS);

    let meta = serde_json::json!({
        "addr": addr,
        "last_seen": now,
        "addrs": addrs,
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| format!("write meta: {e}"))
}

/// One address we have seen a peer at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddr {
    pub addr: String,
    pub last_seen: u64,
}

/// Every address recorded for a peer, most-recently-seen first.
///
/// Reads both shapes: the `addrs` list this version writes, and the lone
/// `addr` string written before #484. Takes the loaded record rather than a
/// peer id so it can be tested without a HOME to read from.
pub fn peer_addrs(meta: Option<&serde_json::Value>) -> Vec<PeerAddr> {
    let Some(meta) = meta else {
        return Vec::new();
    };
    let last_seen_of = |v: &serde_json::Value| v.get("last_seen").and_then(|t| t.as_u64());

    if let Some(list) = meta.get("addrs").and_then(|v| v.as_array()) {
        let mut out: Vec<PeerAddr> = Vec::new();
        for entry in list {
            let Some(addr) = entry.get("addr").and_then(|v| v.as_str()) else {
                continue;
            };
            if out.iter().any(|a| a.addr == addr) {
                continue;
            }
            out.push(PeerAddr {
                addr: addr.to_string(),
                last_seen: last_seen_of(entry).unwrap_or(0),
            });
        }
        if !out.is_empty() {
            return out;
        }
        // An `addrs` key that holds nothing usable: fall through to `addr`
        // rather than reporting a peer as having no addresses at all.
    }

    match meta.get("addr").and_then(|v| v.as_str()) {
        Some(addr) => vec![PeerAddr {
            addr: addr.to_string(),
            last_seen: last_seen_of(meta).unwrap_or(0),
        }],
        None => Vec::new(),
    }
}

/// Load peer metadata.
pub fn load_peer_meta(peer_id: &str) -> Option<serde_json::Value> {
    let path = peer_meta_path(peer_id)?;
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// List all known peer IDs (those with .key files).
pub fn list_known_peers() -> Vec<String> {
    let dir = peers_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".key")
                .filter(|id| *id != PENDING_PEER_ID && is_valid_peer_id(id))
                .map(|s| s.to_string())
        })
        .collect()
}

/// Load the pending pairing PSK, if a local `pair` command is waiting to be claimed.
pub fn load_pending_psk() -> Option<[u8; 32]> {
    load_peer_psk(PENDING_PEER_ID)
}

/// Store the pending pairing PSK.
pub fn save_pending_psk(psk: &[u8; 32]) -> Result<(), String> {
    save_peer_psk(PENDING_PEER_ID, psk)
}

/// Clear any pending pairing PSK.
pub fn clear_pending_psk() {
    forget_peer(PENDING_PEER_ID);
}

/// Remove all data for a peer.
pub fn forget_peer(peer_id: &str) {
    if let Some(path) = peer_key_path(peer_id) {
        let _ = fs::remove_file(path);
    }
    if let Some(path) = peer_meta_path(peer_id) {
        let _ = fs::remove_file(path);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_type_serde_roundtrip() {
        let val = MessageType::Heartbeat;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, "\"heartbeat\"");
        let back: MessageType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, val);
    }

    #[test]
    fn message_type_all_variants() {
        let variants = [
            MessageType::Challenge,
            MessageType::Handshake,
            MessageType::HandshakeAck,
            MessageType::Heartbeat,
            MessageType::Ack,
            MessageType::DelegateTask,
            MessageType::TaskStatus,
            MessageType::TaskHandoff,
            MessageType::TaskInterrupt,
            MessageType::KnowledgeSync,
            MessageType::KnowledgeRequest,
            MessageType::KnowledgeSnapshot,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: MessageType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn relay_message_roundtrip() {
        let msg = RelayMessage {
            id: "msg_1".into(),
            msg_type: MessageType::Heartbeat,
            from_peer: "test-host".into(),
            timestamp: 1234567890,
            payload: serde_json::json!({}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: RelayMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "msg_1");
        assert_eq!(back.msg_type, MessageType::Heartbeat);
    }

    #[test]
    fn gen_msg_id_unique() {
        let a = gen_msg_id();
        let b = gen_msg_id();
        assert_ne!(a, b);
    }

    #[test]
    fn peer_id_display() {
        let p = PeerId("test-abc1".into());
        assert_eq!(p.to_string(), "test-abc1");
        assert_eq!(p.as_str(), "test-abc1");
    }

    #[test]
    fn peer_id_validation_rejects_paths() {
        assert!(is_valid_peer_id("peer-abc_123"));
        assert!(is_valid_peer_id(PENDING_PEER_ID));
        assert!(!is_valid_peer_id("../peer"));
        assert!(!is_valid_peer_id("peer/key"));
        assert!(!is_valid_peer_id(""));
    }
}

#[cfg(test)]
mod peer_address_records {
    use super::{PeerAddr, peer_addrs};
    use serde_json::json;

    fn addrs(meta: serde_json::Value) -> Vec<String> {
        peer_addrs(Some(&meta))
            .into_iter()
            .map(|a| a.addr)
            .collect()
    }

    // Records written before #484 carry one `addr` and no list. They have to
    // keep working, or this change strands every pairing made so far.
    #[test]
    fn a_record_from_before_the_list_reads_as_one_address() {
        assert_eq!(
            peer_addrs(Some(&json!({"addr": "192.168.4.39:9847", "last_seen": 42}))),
            vec![PeerAddr {
                addr: "192.168.4.39:9847".into(),
                last_seen: 42
            }]
        );
    }

    #[test]
    fn the_list_is_returned_in_the_order_it_is_stored() {
        assert_eq!(
            addrs(json!({
                "addr": "100.85.22.1:9847",
                "addrs": [
                    {"addr": "100.85.22.1:9847", "last_seen": 20},
                    {"addr": "192.168.4.24:9847", "last_seen": 10},
                ]
            })),
            vec!["100.85.22.1:9847", "192.168.4.24:9847"]
        );
    }

    #[test]
    fn a_repeated_address_appears_once() {
        assert_eq!(
            addrs(json!({"addrs": [
                {"addr": "a:1", "last_seen": 2},
                {"addr": "a:1", "last_seen": 1},
                {"addr": "b:2", "last_seen": 1},
            ]})),
            vec!["a:1", "b:2"]
        );
    }

    // A list that parses but holds nothing usable must not mask the `addr`
    // alongside it — otherwise a malformed write loses a working address.
    #[test]
    fn an_unusable_list_falls_back_to_the_single_address() {
        assert_eq!(
            addrs(json!({"addr": "192.168.4.39:9847", "addrs": [{"last_seen": 1}]})),
            vec!["192.168.4.39:9847"]
        );
        assert_eq!(
            addrs(json!({"addr": "192.168.4.39:9847", "addrs": []})),
            vec!["192.168.4.39:9847"]
        );
    }

    #[test]
    fn nothing_recorded_is_no_addresses_rather_than_a_guess() {
        assert!(peer_addrs(None).is_empty());
        assert!(peer_addrs(Some(&json!({"last_seen": 1}))).is_empty());
        assert!(peer_addrs(Some(&json!({"addr": 9847}))).is_empty());
    }

    #[test]
    fn a_missing_timestamp_reads_as_zero_rather_than_dropping_the_address() {
        let got = peer_addrs(Some(&json!({"addrs": [{"addr": "a:1"}]})));
        assert_eq!(got.len(), 1, "the address still counts");
        assert_eq!(got[0].last_seen, 0);
    }
}

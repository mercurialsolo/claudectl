#![allow(dead_code)]

pub mod cli;
pub mod crypto;
pub mod delegation;
pub mod listener;
pub mod mesh;
pub mod peer;
pub mod protocol;
pub mod worker;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Unique identity for a claudectl instance in the relay network.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub String);

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
    full.split('.').next().unwrap_or("unknown").to_lowercase()
}

/// Load a stored PSK for a peer, or None if not paired.
pub fn load_peer_psk(peer_id: &str) -> Option<[u8; 32]> {
    let path = peers_dir().join(format!("{peer_id}.key"));
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
    let dir = peers_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create peers dir: {e}"))?;
    let path = dir.join(format!("{peer_id}.key"));
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

/// Save peer metadata (addr, last_seen, etc).
pub fn save_peer_meta(peer_id: &str, addr: &str) -> Result<(), String> {
    let dir = peers_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create peers dir: {e}"))?;
    let path = dir.join(format!("{peer_id}.meta"));
    let meta = serde_json::json!({
        "addr": addr,
        "last_seen": epoch_ms(),
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| format!("write meta: {e}"))
}

/// Load peer metadata.
pub fn load_peer_meta(peer_id: &str) -> Option<serde_json::Value> {
    let path = peers_dir().join(format!("{peer_id}.meta"));
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
            name.strip_suffix(".key").map(|s| s.to_string())
        })
        .collect()
}

/// Remove all data for a peer.
pub fn forget_peer(peer_id: &str) {
    let dir = peers_dir();
    let _ = fs::remove_file(dir.join(format!("{peer_id}.key")));
    let _ = fs::remove_file(dir.join(format!("{peer_id}.meta")));
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
}

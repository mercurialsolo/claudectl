// Per-peer trust tracking with auto-drift based on concordance.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// Trust tiers
// ────────────────────────────────────────────────────────────────────────────

/// Trust tier determines how peer knowledge appears in the brain prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustTier {
    /// >= 0.8 — knowledge appears as confirmed `[hive]`
    Confirmed,
    /// >= 0.5 — knowledge appears as `[hive, suggested]`
    Suggested,
    /// >= 0.2 — knowledge appears as `[hive, unverified]`
    Unverified,
    /// < 0.2 — knowledge is stored but not injected into prompts
    Ignored,
}

impl TrustTier {
    pub fn from_level(level: f64) -> Self {
        if level >= 0.8 {
            Self::Confirmed
        } else if level >= 0.5 {
            Self::Suggested
        } else if level >= 0.2 {
            Self::Unverified
        } else {
            Self::Ignored
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Confirmed => "hive",
            Self::Suggested => "hive, suggested",
            Self::Unverified => "hive, unverified",
            Self::Ignored => "hive, ignored",
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// PeerTrust
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerTrust {
    pub peer_id: String,
    /// Trust level: 0.0 (ignore everything) to 1.0 (fully trusted). Default: 0.5.
    pub trust_level: f64,
    /// How many knowledge units accepted from this peer.
    pub knowledge_accepted: u32,
    /// How many units conflicted with local knowledge.
    pub knowledge_conflicted: u32,
    /// When this peer was first seen (epoch secs).
    pub first_seen: u64,
    /// When this peer last synced knowledge (epoch secs).
    pub last_sync: u64,
}

impl PeerTrust {
    pub fn new(peer_id: &str, default_trust: f64) -> Self {
        let now = super::epoch_secs();
        PeerTrust {
            peer_id: peer_id.to_string(),
            trust_level: default_trust,
            first_seen: now,
            last_sync: now,
            knowledge_accepted: 0,
            knowledge_conflicted: 0,
        }
    }

    pub fn tier(&self) -> TrustTier {
        TrustTier::from_level(self.trust_level)
    }

    /// Drift trust up by 0.01 (concordant decision).
    pub fn drift_up(&mut self) {
        self.trust_level = (self.trust_level + 0.01).min(1.0);
    }

    /// Drift trust down by 0.01 (discordant decision).
    pub fn drift_down(&mut self) {
        self.trust_level = (self.trust_level - 0.01).max(0.0);
    }

    /// Record an accepted knowledge unit.
    pub fn record_accept(&mut self) {
        self.knowledge_accepted += 1;
        self.last_sync = super::epoch_secs();
    }

    /// Record a conflicted knowledge unit.
    pub fn record_conflict(&mut self) {
        self.knowledge_conflicted += 1;
    }
}

// ────────────────────────────────────────────────────────────────────────────
// TrustStore
// ────────────────────────────────────────────────────────────────────────────

/// Persistent store for peer trust levels.
pub struct TrustStore {
    peers: HashMap<String, PeerTrust>,
    default_trust: f64,
}

fn trust_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("trust.json")
}

impl TrustStore {
    /// Load from disk, or create empty.
    pub fn load() -> Self {
        Self::load_with_default(0.5)
    }

    pub fn load_with_default(default_trust: f64) -> Self {
        let path = trust_path();
        let peers = match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        TrustStore {
            peers,
            default_trust,
        }
    }

    /// Save to disk.
    pub fn save(&self) -> Result<(), String> {
        let path = trust_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
        }
        let json =
            serde_json::to_string_pretty(&self.peers).map_err(|e| format!("serialize: {e}"))?;
        fs::write(&path, json).map_err(|e| format!("write: {e}"))
    }

    /// Get trust for a peer, or None if unknown.
    pub fn get(&self, peer_id: &str) -> Option<&PeerTrust> {
        self.peers.get(peer_id)
    }

    /// Get or create trust for a peer.
    pub fn get_or_create(&mut self, peer_id: &str) -> &mut PeerTrust {
        self.peers
            .entry(peer_id.to_string())
            .or_insert_with(|| PeerTrust::new(peer_id, self.default_trust))
    }

    /// Set trust level explicitly.
    pub fn set_trust(&mut self, peer_id: &str, level: f64) {
        let clamped = level.clamp(0.0, 1.0);
        let trust = self.get_or_create(peer_id);
        trust.trust_level = clamped;
    }

    /// All tracked peers.
    pub fn all(&self) -> Vec<&PeerTrust> {
        self.peers.values().collect()
    }

    /// Record a concordant decision (local agrees with hive knowledge from peer).
    pub fn record_concordant(&mut self, peer_id: &str) {
        let trust = self.get_or_create(peer_id);
        trust.drift_up();
        trust.record_accept();
    }

    /// Record a discordant decision (local disagrees with hive knowledge from peer).
    pub fn record_discordant(&mut self, peer_id: &str) {
        let trust = self.get_or_create(peer_id);
        trust.drift_down();
        trust.record_conflict();
    }

    /// Number of tracked peers.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_tier_classification() {
        assert_eq!(TrustTier::from_level(0.9), TrustTier::Confirmed);
        assert_eq!(TrustTier::from_level(0.8), TrustTier::Confirmed);
        assert_eq!(TrustTier::from_level(0.79), TrustTier::Suggested);
        assert_eq!(TrustTier::from_level(0.5), TrustTier::Suggested);
        assert_eq!(TrustTier::from_level(0.49), TrustTier::Unverified);
        assert_eq!(TrustTier::from_level(0.2), TrustTier::Unverified);
        assert_eq!(TrustTier::from_level(0.19), TrustTier::Ignored);
        assert_eq!(TrustTier::from_level(0.0), TrustTier::Ignored);
    }

    #[test]
    fn drift_up_clamped() {
        let mut trust = PeerTrust::new("peer-a", 0.99);
        trust.drift_up();
        assert_eq!(trust.trust_level, 1.0);
        trust.drift_up();
        assert_eq!(trust.trust_level, 1.0); // still clamped
    }

    #[test]
    fn drift_down_clamped() {
        let mut trust = PeerTrust::new("peer-a", 0.01);
        trust.drift_down();
        assert_eq!(trust.trust_level, 0.0);
        trust.drift_down();
        assert_eq!(trust.trust_level, 0.0); // still clamped
    }

    #[test]
    fn concordant_discordant_tracking() {
        let mut store = TrustStore {
            peers: HashMap::new(),
            default_trust: 0.5,
        };

        store.record_concordant("peer-a");
        store.record_concordant("peer-a");
        store.record_discordant("peer-a");

        let trust = store.get("peer-a").unwrap();
        assert_eq!(trust.knowledge_accepted, 2);
        assert_eq!(trust.knowledge_conflicted, 1);
        // 0.5 + 0.01 + 0.01 - 0.01 = 0.51
        assert!((trust.trust_level - 0.51).abs() < 0.001);
    }

    #[test]
    fn set_trust_clamped() {
        let mut store = TrustStore {
            peers: HashMap::new(),
            default_trust: 0.5,
        };

        store.set_trust("peer-a", 1.5);
        assert_eq!(store.get("peer-a").unwrap().trust_level, 1.0);

        store.set_trust("peer-b", -0.5);
        assert_eq!(store.get("peer-b").unwrap().trust_level, 0.0);
    }

    #[test]
    fn get_or_create_default() {
        let mut store = TrustStore {
            peers: HashMap::new(),
            default_trust: 0.7,
        };

        let trust = store.get_or_create("new-peer");
        assert_eq!(trust.trust_level, 0.7);
        assert_eq!(trust.peer_id, "new-peer");
    }

    #[test]
    fn tier_labels() {
        assert_eq!(TrustTier::Confirmed.label(), "hive");
        assert_eq!(TrustTier::Suggested.label(), "hive, suggested");
        assert_eq!(TrustTier::Unverified.label(), "hive, unverified");
        assert_eq!(TrustTier::Ignored.label(), "hive, ignored");
    }

    #[test]
    fn peer_trust_serde_roundtrip() {
        let trust = PeerTrust::new("peer-a", 0.8);
        let json = serde_json::to_string(&trust).unwrap();
        let back: PeerTrust = serde_json::from_str(&json).unwrap();
        assert_eq!(back.peer_id, "peer-a");
        assert_eq!(back.trust_level, 0.8);
    }
}

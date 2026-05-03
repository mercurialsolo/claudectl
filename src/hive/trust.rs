// Per-peer trust tracking with auto-drift based on concordance.
//
// #226 sybil quarantine + collision freeze:
//   - New peers can't be promoted above the default trust for 7 days (their
//     knowledge is gossiped but not injected into brain prompts during this
//     period).
//   - A daily rate cap on incoming units flags anomalies and freezes peers
//     pending review.
//   - When an incoming unit's `semantic_key` matches a stored unit but the
//     content/confidence diverges sharply, both peers are frozen.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// Sybil thresholds (#226)
// ────────────────────────────────────────────────────────────────────────────

/// How long a newly-seen peer stays in quarantine. During this window the peer
/// cannot drift above the default trust level, and its knowledge is gossiped
/// but never injected into brain prompts.
pub const QUARANTINE_DAYS: u64 = 7;

/// Hard cap on units a single peer may push us in a 24h window before we
/// freeze the peer. Catches the simplest flooding attack — a malicious peer
/// trying to drown out concordant signal with volume.
pub const DAILY_RATE_CAP: u32 = 1_000;

/// Confidence delta above which two units sharing a `semantic_key` count as a
/// collision (suspicious disagreement). Trusted peers don't usually swing this
/// far on the same fact.
pub const COLLISION_CONFIDENCE_DELTA: f64 = 0.3;

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
    /// Quarantine deadline (#226). Until this epoch, the peer cannot drift
    /// above its starting trust level and is excluded from prompt injection.
    /// 0 means "no quarantine" (used for old PeerTrust records on disk).
    #[serde(default)]
    pub quarantined_until: u64,
    /// Manually frozen pending review (#226). Set by collision detection and
    /// rate-anomaly flags; cleared by `claudectl hive review --unfreeze`.
    /// While frozen the peer cannot drift up and is excluded from injection.
    #[serde(default)]
    pub frozen: bool,
    /// Why we froze this peer (last reason — overwritten on each freeze).
    #[serde(default)]
    pub freeze_reason: Option<String>,
    /// Per-day counts of incoming knowledge units, keyed by day-bucket
    /// (epoch_secs / 86_400). Capped to the last 30 days to keep the file small.
    #[serde(default)]
    pub daily_received: HashMap<u64, u32>,
    /// Last time the daily rate cap was exceeded (epoch secs).
    #[serde(default)]
    pub last_anomaly_at: Option<u64>,
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
            quarantined_until: now + QUARANTINE_DAYS * 86_400,
            frozen: false,
            freeze_reason: None,
            daily_received: HashMap::new(),
            last_anomaly_at: None,
        }
    }

    pub fn tier(&self) -> TrustTier {
        TrustTier::from_level(self.trust_level)
    }

    /// Whether this peer is still in its quarantine window.
    pub fn quarantine_active(&self, now: u64) -> bool {
        self.quarantined_until > now
    }

    /// Whether the peer is currently blocked from prompt injection
    /// (quarantine still active or manually frozen). Their knowledge is still
    /// stored and gossiped — we just don't let it shape brain decisions.
    pub fn is_blocked_from_injection(&self, now: u64) -> bool {
        self.quarantine_active(now) || self.frozen
    }

    /// Drift trust up by 0.01 (concordant decision). No-ops while quarantined
    /// (above the entry trust) or frozen — promotion has to be earned over
    /// time, not in a burst.
    pub fn drift_up(&mut self) {
        if self.frozen {
            return;
        }
        let now = super::epoch_secs();
        // During quarantine, allow recovery up to the starting trust but no
        // higher. We don't know the original default trust here, so use 0.5
        // as the floor — matches the default_trust in `TrustStore::load()`.
        let cap = if self.quarantine_active(now) {
            0.5
        } else {
            1.0
        };
        if self.trust_level >= cap {
            return;
        }
        self.trust_level = (self.trust_level + 0.01).min(cap);
    }

    /// Drift trust down by 0.01 (discordant decision). Always permitted —
    /// quarantine and freezing are about preventing premature *promotion*,
    /// not recovery from a fall.
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

    /// Freeze the peer pending manual review.
    pub fn freeze(&mut self, reason: &str) {
        self.frozen = true;
        self.freeze_reason = Some(reason.to_string());
    }

    /// Clear a manual freeze (operator decision).
    pub fn unfreeze(&mut self) {
        self.frozen = false;
        self.freeze_reason = None;
    }

    /// Tick daily-received counter. Returns true when this push crossed
    /// `DAILY_RATE_CAP` for the current day — caller should freeze the peer.
    pub fn record_received(&mut self, now: u64, count: u32) -> bool {
        let bucket = now / 86_400;
        let entry = self.daily_received.entry(bucket).or_insert(0);
        let was_under = *entry < DAILY_RATE_CAP;
        *entry = entry.saturating_add(count);
        let crossed = was_under && *entry >= DAILY_RATE_CAP;

        // Trim to the last 30 days so the on-disk file doesn't grow without bound.
        if self.daily_received.len() > 30 {
            let cutoff = bucket.saturating_sub(30);
            self.daily_received.retain(|day, _| *day >= cutoff);
        }

        if crossed {
            self.last_anomaly_at = Some(now);
        }
        crossed
    }

    /// Total received in the current 24h bucket.
    pub fn received_today(&self, now: u64) -> u32 {
        let bucket = now / 86_400;
        self.daily_received.get(&bucket).copied().unwrap_or(0)
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

    /// In-memory store (no disk read). Used by tests that need a deterministic
    /// trust map regardless of what the shared `trust.json` happens to contain.
    pub fn empty(default_trust: f64) -> Self {
        TrustStore {
            peers: HashMap::new(),
            default_trust,
        }
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

    /// Tick the daily-received counter for a peer. Returns true when this
    /// caused the rate cap to be crossed; in that case the caller should
    /// freeze the peer.
    pub fn record_received(&mut self, peer_id: &str, count: u32) -> bool {
        let now = super::epoch_secs();
        let trust = self.get_or_create(peer_id);
        trust.record_received(now, count)
    }

    /// Manually freeze a peer.
    pub fn freeze(&mut self, peer_id: &str, reason: &str) {
        let trust = self.get_or_create(peer_id);
        trust.freeze(reason);
    }

    /// Clear a manual freeze.
    pub fn unfreeze(&mut self, peer_id: &str) {
        if let Some(trust) = self.peers.get_mut(peer_id) {
            trust.unfreeze();
        }
    }

    /// All currently-frozen peers (for `hive review`).
    pub fn frozen_peers(&self) -> Vec<&PeerTrust> {
        self.peers.values().filter(|p| p.frozen).collect()
    }

    /// Whether a peer is blocked from prompt injection (quarantined or frozen).
    /// Peers we've never seen are not blocked — they'll get a fresh PeerTrust
    /// on first contact.
    pub fn is_blocked_from_injection(&self, peer_id: &str) -> bool {
        let now = super::epoch_secs();
        self.peers
            .get(peer_id)
            .is_some_and(|p| p.is_blocked_from_injection(now))
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
// Collision detection (#226)
// ────────────────────────────────────────────────────────────────────────────

/// A suspicious disagreement between an incoming unit and a stored unit
/// sharing the same `semantic_key`.
#[derive(Debug, Clone)]
pub struct Collision {
    pub semantic_key: String,
    pub incoming_unit_id: String,
    pub incoming_peer: String,
    pub existing_unit_id: String,
    pub existing_peer: String,
    pub confidence_delta: f64,
}

/// Detect collisions: incoming units that share a semantic_key with a stored
/// unit but disagree by more than `COLLISION_CONFIDENCE_DELTA` in confidence,
/// or whose `KnowledgeContent` discriminant differs.
pub fn detect_collisions(
    store: &super::store::HiveStore,
    incoming: &[super::KnowledgeUnit],
) -> Vec<Collision> {
    let mut hits = Vec::new();
    for unit in incoming {
        let sk = super::semantic_key(unit);
        let Some(existing) = store.find_by_semantic_key(&sk) else {
            continue;
        };
        // Same peer updating their own unit isn't a collision — that's a
        // version bump, handled by the merger.
        if existing.source_peer == unit.source_peer {
            continue;
        }
        let same_kind =
            std::mem::discriminant(&existing.content) == std::mem::discriminant(&unit.content);
        let conf_delta = (existing.confidence - unit.confidence).abs();
        if !same_kind || conf_delta > COLLISION_CONFIDENCE_DELTA {
            hits.push(Collision {
                semantic_key: sk,
                incoming_unit_id: unit.id.clone(),
                incoming_peer: unit.source_peer.clone(),
                existing_unit_id: existing.id.clone(),
                existing_peer: existing.source_peer.clone(),
                confidence_delta: conf_delta,
            });
        }
    }
    hits
}

/// Apply detected collisions: freeze both peers and log the event.
/// Returns the set of peer IDs that were freshly frozen by this call.
pub fn apply_collisions(
    trust: &mut TrustStore,
    local_peer_id: &str,
    collisions: &[Collision],
) -> Vec<String> {
    let mut newly_frozen = Vec::new();
    for c in collisions {
        let reason = format!(
            "collision on {} (Δconf {:.2}) with {}",
            c.semantic_key, c.confidence_delta, c.existing_peer
        );
        for peer in [&c.incoming_peer, &c.existing_peer] {
            if peer == local_peer_id {
                continue; // never freeze ourselves
            }
            let was_frozen = trust.peers.get(peer).is_some_and(|p| p.frozen);
            trust.freeze(peer, &reason);
            if !was_frozen {
                newly_frozen.push(peer.clone());
            }
        }
        log_collision(c);
    }
    newly_frozen
}

fn collisions_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("collisions.jsonl")
}

fn log_collision(c: &Collision) {
    let path = collisions_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let record = serde_json::json!({
        "ts": super::epoch_secs(),
        "semantic_key": c.semantic_key,
        "incoming_unit_id": c.incoming_unit_id,
        "incoming_peer": c.incoming_peer,
        "existing_unit_id": c.existing_unit_id,
        "existing_peer": c.existing_peer,
        "confidence_delta": c.confidence_delta,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(
            file,
            "{}",
            serde_json::to_string(&record).unwrap_or_default()
        );
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
        trust.quarantined_until = 0; // bypass #226 quarantine for this test
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

        // Touch the peer so it exists, then bypass #226 quarantine before
        // drift events. (Quarantine is exercised by its own dedicated tests.)
        store.get_or_create("peer-a").quarantined_until = 0;

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

    // ── #226 sybil quarantine ──────────────────────────────────────────

    #[test]
    fn new_peer_is_quarantined_for_seven_days() {
        let trust = PeerTrust::new("peer-a", 0.5);
        let now = super::super::epoch_secs();
        assert!(trust.quarantine_active(now));
        // Quarantine expires after 7 days
        let after_quarantine = now + (QUARANTINE_DAYS + 1) * 86_400;
        assert!(!trust.quarantine_active(after_quarantine));
    }

    #[test]
    fn quarantined_peer_cannot_drift_up_above_default() {
        let mut trust = PeerTrust::new("peer-a", 0.5);
        // 100 concordant signals during quarantine — should not promote.
        for _ in 0..100 {
            trust.drift_up();
        }
        assert!((trust.trust_level - 0.5).abs() < 1e-9);
    }

    #[test]
    fn quarantined_peer_can_still_drift_down() {
        let mut trust = PeerTrust::new("peer-a", 0.5);
        trust.drift_down();
        trust.drift_down();
        assert!((trust.trust_level - 0.48).abs() < 1e-9);
    }

    #[test]
    fn drift_up_works_after_quarantine_expires() {
        let mut trust = PeerTrust::new("peer-a", 0.5);
        trust.quarantined_until = 0; // simulate expired quarantine
        trust.drift_up();
        trust.drift_up();
        assert!((trust.trust_level - 0.52).abs() < 1e-9);
    }

    #[test]
    fn quarantined_peer_blocks_injection() {
        let trust = PeerTrust::new("peer-a", 0.5);
        let now = super::super::epoch_secs();
        assert!(trust.is_blocked_from_injection(now));
    }

    #[test]
    fn frozen_peer_cannot_drift_up_or_inject() {
        let mut trust = PeerTrust::new("peer-a", 0.5);
        trust.quarantined_until = 0; // bypass quarantine
        trust.freeze("test reason");
        let before = trust.trust_level;
        trust.drift_up();
        assert_eq!(trust.trust_level, before);
        assert!(trust.is_blocked_from_injection(super::super::epoch_secs()));
    }

    #[test]
    fn unfreeze_clears_state() {
        let mut trust = PeerTrust::new("peer-a", 0.5);
        trust.freeze("collision");
        assert!(trust.frozen);
        assert!(trust.freeze_reason.is_some());
        trust.unfreeze();
        assert!(!trust.frozen);
        assert!(trust.freeze_reason.is_none());
    }

    #[test]
    fn rate_cap_triggers_at_threshold() {
        let mut trust = PeerTrust::new("peer-a", 0.5);
        let now = super::super::epoch_secs();
        // First push under cap — no anomaly
        assert!(!trust.record_received(now, DAILY_RATE_CAP - 1));
        // Push that crosses cap returns true
        assert!(trust.record_received(now, 2));
        assert!(trust.last_anomaly_at.is_some());
        // Subsequent pushes (already over cap) don't re-trigger
        assert!(!trust.record_received(now, 100));
    }

    #[test]
    fn rate_cap_bucket_per_day() {
        let mut trust = PeerTrust::new("peer-a", 0.5);
        let day1 = 86_400;
        let day2 = 86_400 * 2;
        trust.record_received(day1, 500);
        trust.record_received(day2, 500);
        assert_eq!(trust.received_today(day1), 500);
        assert_eq!(trust.received_today(day2), 500);
    }

    #[test]
    fn old_peer_trust_deserializes_with_defaults() {
        // Pre-#226 PeerTrust on disk doesn't carry the new fields.
        let old_json = r#"{
            "peer_id":"legacy",
            "trust_level":0.7,
            "knowledge_accepted":5,
            "knowledge_conflicted":1,
            "first_seen":1000,
            "last_sync":2000
        }"#;
        let trust: PeerTrust = serde_json::from_str(old_json).unwrap();
        assert_eq!(trust.peer_id, "legacy");
        assert_eq!(trust.quarantined_until, 0); // defaulted; treated as expired
        assert!(!trust.frozen);
        assert!(trust.daily_received.is_empty());
        // Old peers shouldn't be retroactively quarantined.
        let now = super::super::epoch_secs();
        assert!(!trust.quarantine_active(now));
    }

    // ── Collision detection (#226) ─────────────────────────────────────

    fn pattern_unit(
        id: &str,
        peer: &str,
        cmd: &str,
        confidence: f64,
    ) -> super::super::KnowledgeUnit {
        super::super::KnowledgeUnit {
            id: id.into(),
            scope: super::super::KnowledgeScope::Universal,
            category: super::super::KnowledgeCategory::BestPractice,
            content: super::super::KnowledgeContent::Pattern {
                tool: "Bash".into(),
                command_pattern: Some(cmd.into()),
                preferred_action: "approve".into(),
                accept_rate: confidence,
                sample_count: 10,
                conditions: vec![],
            },
            evidence_count: 10,
            confidence,
            source_peer: peer.into(),
            originated_at: 0,
            last_validated_at: 0,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: super::super::InjectionState::Live,
            injection_stats: super::super::InjectionStats::default(),
            sharing_consent: None,
        }
    }

    fn empty_store() -> super::super::store::HiveStore {
        super::super::store::HiveStore::load_from(std::path::Path::new("/nonexistent"))
    }

    #[test]
    fn detects_confidence_collision() {
        let mut store = empty_store();
        store.insert(pattern_unit("ku_a", "peer-a", "git push", 0.9));
        let incoming = vec![pattern_unit("ku_b", "peer-b", "git push", 0.5)];
        let collisions = detect_collisions(&store, &incoming);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].incoming_peer, "peer-b");
        assert_eq!(collisions[0].existing_peer, "peer-a");
        assert!((collisions[0].confidence_delta - 0.4).abs() < 1e-9);
    }

    #[test]
    fn ignores_within_threshold_disagreement() {
        let mut store = empty_store();
        store.insert(pattern_unit("ku_a", "peer-a", "git push", 0.9));
        // Δ = 0.2, below COLLISION_CONFIDENCE_DELTA (0.3)
        let incoming = vec![pattern_unit("ku_b", "peer-b", "git push", 0.7)];
        let collisions = detect_collisions(&store, &incoming);
        assert!(collisions.is_empty());
    }

    #[test]
    fn ignores_same_peer_version_bump() {
        let mut store = empty_store();
        store.insert(pattern_unit("ku_a", "peer-a", "git push", 0.9));
        let incoming = vec![pattern_unit("ku_a", "peer-a", "git push", 0.4)];
        let collisions = detect_collisions(&store, &incoming);
        assert!(collisions.is_empty());
    }

    #[test]
    fn apply_collisions_freezes_both_peers() {
        let mut store = empty_store();
        store.insert(pattern_unit("ku_a", "peer-a", "git push", 0.9));
        let incoming = vec![pattern_unit("ku_b", "peer-b", "git push", 0.5)];
        let collisions = detect_collisions(&store, &incoming);

        let mut trust = TrustStore {
            peers: HashMap::new(),
            default_trust: 0.5,
        };
        let frozen = apply_collisions(&mut trust, "local", &collisions);
        assert_eq!(frozen.len(), 2);
        assert!(trust.get("peer-a").unwrap().frozen);
        assert!(trust.get("peer-b").unwrap().frozen);
    }

    #[test]
    fn apply_collisions_skips_local_peer() {
        let mut store = empty_store();
        store.insert(pattern_unit("ku_a", "local", "git push", 0.9));
        let incoming = vec![pattern_unit("ku_b", "peer-b", "git push", 0.5)];
        let collisions = detect_collisions(&store, &incoming);

        let mut trust = TrustStore {
            peers: HashMap::new(),
            default_trust: 0.5,
        };
        let frozen = apply_collisions(&mut trust, "local", &collisions);
        assert_eq!(frozen, vec!["peer-b"]);
        assert!(trust.get("local").is_none(), "must never freeze local peer");
    }
}

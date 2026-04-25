// Gossip protocol: sync knowledge units between connected peers.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use super::merger::{self, MergeStats};
use super::store::HiveStore;
use super::{KnowledgeUnit, epoch_secs};
use crate::relay::{MessageType, PeerId, RelayMessage, epoch_ms, gen_msg_id};

/// Maximum payload size for a KnowledgeSnapshot message (500 KB).
const MAX_SNAPSHOT_SIZE: usize = 500 * 1024;

// ────────────────────────────────────────────────────────────────────────────
// Per-peer sync state
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerSyncState {
    pub peer_id: String,
    pub last_sync_epoch: u64,
    /// IDs of units already sent to this peer.
    pub units_sent: HashSet<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Gossip engine
// ────────────────────────────────────────────────────────────────────────────

/// Manages gossip protocol state for knowledge sharing.
pub struct GossipEngine {
    sync_states: HashMap<String, PeerSyncState>,
    max_propagation: u32,
    knowledge_ttl_days: u32,
    local_peer_id: String,
    sharing_filter: super::SharingFilter,
}

impl GossipEngine {
    pub fn new(local_peer_id: &str, max_propagation: u32, knowledge_ttl_days: u32) -> Self {
        let sync_states = load_sync_states();
        GossipEngine {
            sync_states,
            max_propagation,
            knowledge_ttl_days,
            local_peer_id: local_peer_id.to_string(),
            sharing_filter: super::SharingFilter::default(),
        }
    }

    /// Set the user's sharing filter (from HiveConfig).
    pub fn set_sharing_filter(&mut self, filter: super::SharingFilter) {
        self.sharing_filter = filter;
    }

    /// Create a fresh engine with no persisted sync state (for testing).
    #[cfg(test)]
    pub fn new_empty(local_peer_id: &str, max_propagation: u32, knowledge_ttl_days: u32) -> Self {
        GossipEngine {
            sync_states: HashMap::new(),
            max_propagation,
            knowledge_ttl_days,
            local_peer_id: local_peer_id.to_string(),
            sharing_filter: super::SharingFilter::default(),
        }
    }

    /// Generate KnowledgeSync messages for each connected peer.
    /// Only includes units not already sent to that peer.
    pub fn generate_sync_messages(
        &mut self,
        store: &HiveStore,
        connected_peers: &[PeerId],
    ) -> Vec<(PeerId, RelayMessage)> {
        let mut messages = Vec::new();
        let now = epoch_secs();

        // Pre-compute propagation parameters to avoid borrow conflicts
        let max_prop = self.max_propagation;
        let ttl_secs = self.knowledge_ttl_days as u64 * 86400;
        let identity = self.local_peer_id.clone();
        let filter = self.sharing_filter.clone();

        for peer in connected_peers {
            let peer_id = peer.0.clone();
            let sync_state =
                self.sync_states
                    .entry(peer_id.clone())
                    .or_insert_with(|| PeerSyncState {
                        peer_id: peer_id.clone(),
                        last_sync_epoch: 0,
                        units_sent: HashSet::new(),
                    });

            // Find units not yet sent to this peer
            let unsent: Vec<&KnowledgeUnit> = store
                .all_units()
                .into_iter()
                .filter(|u| {
                    !sync_state.units_sent.contains(&u.id)
                        && u.source_peer != peer_id // don't echo back
                        && is_propagatable_static(u, max_prop, ttl_secs, &filter)
                })
                .collect();

            if unsent.is_empty() {
                continue;
            }

            // Build sync message
            let units: Vec<KnowledgeUnit> = unsent.into_iter().cloned().collect();
            let msg = build_sync_message(&units, &identity, now);

            // Track what we sent
            for unit in &units {
                sync_state.units_sent.insert(unit.id.clone());
            }
            sync_state.last_sync_epoch = now;

            messages.push((peer.clone(), msg));
        }

        let _ = save_sync_states(&self.sync_states);
        messages
    }

    /// Handle an incoming KnowledgeSync message.
    /// Returns merge stats and any units to re-propagate.
    pub fn handle_sync(
        &mut self,
        store: &mut HiveStore,
        msg: &RelayMessage,
    ) -> (MergeStats, Vec<KnowledgeUnit>) {
        let units = parse_units_from_payload(msg);
        let stats = merger::merge_batch(store, &units, &self.local_peer_id);

        // Collect accepted units for propagation
        let accepted: Vec<KnowledgeUnit> = units
            .into_iter()
            .filter(|u| store.get(&u.id).is_some() && self.is_propagatable(u))
            .collect();

        let _ = store.save();
        (stats, accepted)
    }

    /// Handle an incoming KnowledgeRequest (new peer wants a snapshot).
    /// Returns one or more KnowledgeSnapshot messages (paginated if needed).
    pub fn handle_request(&self, store: &HiveStore, msg: &RelayMessage) -> Vec<RelayMessage> {
        let since_epoch = msg
            .payload
            .get("since_epoch")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let units: Vec<&KnowledgeUnit> = if since_epoch == 0 {
            store.all_units()
        } else {
            store.units_since(since_epoch)
        };

        // Paginate into chunks that fit within MAX_SNAPSHOT_SIZE
        let mut pages = Vec::new();
        let mut current_page: Vec<KnowledgeUnit> = Vec::new();
        let mut current_size: usize = 0;

        for unit in units {
            let unit_json = serde_json::to_string(unit).unwrap_or_default();
            let unit_size = unit_json.len();

            if current_size + unit_size > MAX_SNAPSHOT_SIZE && !current_page.is_empty() {
                pages.push(std::mem::take(&mut current_page));
                current_size = 0;
            }

            current_page.push(unit.clone());
            current_size += unit_size;
        }
        if !current_page.is_empty() {
            pages.push(current_page);
        }

        let total_pages = pages.len();
        pages
            .into_iter()
            .enumerate()
            .map(|(i, units)| {
                build_snapshot_message(&units, &self.local_peer_id, i + 1, total_pages)
            })
            .collect()
    }

    /// Handle an incoming KnowledgeSnapshot.
    pub fn handle_snapshot(&mut self, store: &mut HiveStore, msg: &RelayMessage) -> MergeStats {
        let units = parse_units_from_payload(msg);
        let stats = merger::merge_batch(store, &units, &self.local_peer_id);
        let _ = store.save();
        stats
    }

    /// Build a KnowledgeRequest message for requesting a snapshot from a peer.
    pub fn build_request_message(&self, since_epoch: u64) -> RelayMessage {
        RelayMessage {
            id: gen_msg_id(),
            msg_type: MessageType::KnowledgeRequest,
            from_peer: self.local_peer_id.clone(),
            timestamp: epoch_ms(),
            payload: serde_json::json!({
                "since_epoch": since_epoch,
            }),
        }
    }

    /// Generate propagation messages for accepted units to other peers.
    /// Excludes the source peer and peers that already have the unit.
    pub fn propagate(
        &mut self,
        accepted_units: &[KnowledgeUnit],
        source_peer: &PeerId,
        connected_peers: &[PeerId],
    ) -> Vec<(PeerId, RelayMessage)> {
        let now = epoch_secs();
        let mut messages = Vec::new();

        // Filter peers: exclude source
        let target_peers: Vec<&PeerId> = connected_peers
            .iter()
            .filter(|p| p.0 != source_peer.0)
            .collect();

        if target_peers.is_empty() {
            return messages;
        }

        // Filter units: only propagatable ones
        let propagatable: Vec<&KnowledgeUnit> = accepted_units
            .iter()
            .filter(|u| self.is_propagatable(u))
            .collect();

        if propagatable.is_empty() {
            return messages;
        }

        for peer in target_peers {
            let sync_state =
                self.sync_states
                    .entry(peer.0.clone())
                    .or_insert_with(|| PeerSyncState {
                        peer_id: peer.0.clone(),
                        last_sync_epoch: 0,
                        units_sent: HashSet::new(),
                    });

            let unsent: Vec<KnowledgeUnit> = propagatable
                .iter()
                .filter(|u| !sync_state.units_sent.contains(&u.id))
                .map(|u| (*u).clone())
                .collect();

            if unsent.is_empty() {
                continue;
            }

            let msg = build_sync_message(&unsent, &self.local_peer_id, now);
            for unit in &unsent {
                sync_state.units_sent.insert(unit.id.clone());
            }
            messages.push((peer.clone(), msg));
        }

        let _ = save_sync_states(&self.sync_states);
        messages
    }

    /// Check if a unit is eligible for propagation.
    fn is_propagatable(&self, unit: &KnowledgeUnit) -> bool {
        let ttl_secs = self.knowledge_ttl_days as u64 * 86400;
        is_propagatable_static(unit, self.max_propagation, ttl_secs, &self.sharing_filter)
    }

    /// Get the sync state for a specific peer.
    pub fn get_sync_state(&self, peer_id: &str) -> Option<&PeerSyncState> {
        self.sync_states.get(peer_id)
    }

    /// Get all sync states.
    pub fn all_sync_states(&self) -> &HashMap<String, PeerSyncState> {
        &self.sync_states
    }
}

/// Check propagation eligibility without borrowing self.
fn is_propagatable_static(
    unit: &KnowledgeUnit,
    max_propagation: u32,
    ttl_secs: u64,
    filter: &super::SharingFilter,
) -> bool {
    // Personal knowledge never propagates
    if !unit.category.is_shareable() {
        return false;
    }
    // User-configured exclusions
    if !filter.allows(unit) {
        return false;
    }
    if unit.propagation_count >= max_propagation {
        return false;
    }
    let age = epoch_secs().saturating_sub(unit.last_validated_at);
    if age > ttl_secs {
        return false;
    }
    true
}

// ────────────────────────────────────────────────────────────────────────────
// Message builders
// ────────────────────────────────────────────────────────────────────────────

fn build_sync_message(units: &[KnowledgeUnit], identity: &str, sync_epoch: u64) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::KnowledgeSync,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "units": units,
            "sync_epoch": sync_epoch,
        }),
    }
}

fn build_snapshot_message(
    units: &[KnowledgeUnit],
    identity: &str,
    page: usize,
    total_pages: usize,
) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::KnowledgeSnapshot,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "units": units,
            "page": page,
            "total_pages": total_pages,
        }),
    }
}

fn parse_units_from_payload(msg: &RelayMessage) -> Vec<KnowledgeUnit> {
    msg.payload
        .get("units")
        .and_then(|v| serde_json::from_value::<Vec<KnowledgeUnit>>(v.clone()).ok())
        .unwrap_or_default()
}

// ────────────────────────────────────────────────────────────────────────────
// Sync state persistence
// ────────────────────────────────────────────────────────────────────────────

fn sync_state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("sync_state.json")
}

fn load_sync_states() -> HashMap<String, PeerSyncState> {
    let path = sync_state_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_sync_states(states: &HashMap<String, PeerSyncState>) -> Result<(), String> {
    let path = sync_state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(states).map_err(|e| format!("serialize sync state: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("write sync state: {e}"))
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::{KnowledgeContent, KnowledgeScope};

    fn make_unit(id: &str, tool: &str, peer: &str) -> KnowledgeUnit {
        KnowledgeUnit {
            id: id.into(),
            scope: KnowledgeScope::Universal,
            category: crate::hive::KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Pattern {
                tool: tool.into(),
                command_pattern: Some("test".into()),
                preferred_action: "approve".into(),
                accept_rate: 0.9,
                sample_count: 10,
                conditions: vec![],
            },
            evidence_count: 10,
            confidence: 0.9,
            source_peer: peer.into(),
            originated_at: epoch_secs(),
            last_validated_at: epoch_secs(),
            propagation_count: 0,
            version: 1,
        }
    }

    fn empty_store() -> HiveStore {
        HiveStore::load_from(std::path::Path::new("/nonexistent"))
    }

    #[test]
    fn generate_sync_only_unsent() {
        let mut store = empty_store();
        store.insert(make_unit("ku_1", "Bash", "local"));
        store.insert(make_unit("ku_2", "Read", "local"));

        let mut engine = GossipEngine::new_empty("local", 5, 30);
        let peers = vec![PeerId("peer-a".into())];

        // First sync: both units should be sent
        let msgs = engine.generate_sync_messages(&store, &peers);
        assert_eq!(msgs.len(), 1);
        let units = parse_units_from_payload(&msgs[0].1);
        assert_eq!(units.len(), 2);

        // Second sync: nothing new
        let msgs = engine.generate_sync_messages(&store, &peers);
        assert_eq!(msgs.len(), 0);

        // Add a new unit → should be sent
        store.insert(make_unit("ku_3", "Write", "local"));
        let msgs = engine.generate_sync_messages(&store, &peers);
        assert_eq!(msgs.len(), 1);
        let units = parse_units_from_payload(&msgs[0].1);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].id, "ku_3");
    }

    #[test]
    fn dont_echo_back_to_source() {
        let mut store = empty_store();
        // Unit originated from peer-a
        store.insert(make_unit("ku_1", "Bash", "peer-a"));

        let mut engine = GossipEngine::new_empty("local", 5, 30);
        let peers = vec![PeerId("peer-a".into())];

        // Should NOT send peer-a's own unit back to peer-a
        let msgs = engine.generate_sync_messages(&store, &peers);
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn handle_sync_merges_units() {
        let mut store = empty_store();
        let mut engine = GossipEngine::new_empty("local", 5, 30);

        let incoming_units = vec![
            make_unit("ku_r1", "Bash", "peer-a"),
            make_unit("ku_r2", "Read", "peer-a"),
        ];
        let msg = build_sync_message(&incoming_units, "peer-a", epoch_secs());

        let (stats, accepted) = engine.handle_sync(&mut store, &msg);
        assert_eq!(stats.accepted, 2);
        assert_eq!(accepted.len(), 2);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn handle_request_returns_snapshot() {
        let mut store = empty_store();
        store.insert(make_unit("ku_1", "Bash", "local"));
        store.insert(make_unit("ku_2", "Read", "local"));

        let engine = GossipEngine::new_empty("local", 5, 30);

        let request = RelayMessage {
            id: "req_1".into(),
            msg_type: MessageType::KnowledgeRequest,
            from_peer: "peer-a".into(),
            timestamp: 0,
            payload: serde_json::json!({ "since_epoch": 0 }),
        };

        let snapshots = engine.handle_request(&store, &request);
        assert!(!snapshots.is_empty());

        let total_units: usize = snapshots
            .iter()
            .map(|s| parse_units_from_payload(s).len())
            .sum();
        assert_eq!(total_units, 2);
    }

    #[test]
    fn handle_snapshot_merges() {
        let mut store = empty_store();
        let mut engine = GossipEngine::new_empty("local", 5, 30);

        let units = vec![make_unit("ku_1", "Bash", "peer-a")];
        let msg = build_snapshot_message(&units, "peer-a", 1, 1);

        let stats = engine.handle_snapshot(&mut store, &msg);
        assert_eq!(stats.accepted, 1);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn propagation_excludes_source() {
        let mut engine = GossipEngine::new_empty("local", 5, 30);
        let units = vec![make_unit("ku_1", "Bash", "peer-a")];
        let source = PeerId("peer-a".into());
        let connected = vec![PeerId("peer-a".into()), PeerId("peer-b".into())];

        let msgs = engine.propagate(&units, &source, &connected);
        // Should only send to peer-b, not peer-a (the source)
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0.0, "peer-b");
    }

    #[test]
    fn propagation_respects_max_hops() {
        let mut engine = GossipEngine::new_empty("local", 3, 30);
        let mut unit = make_unit("ku_1", "Bash", "peer-a");
        unit.propagation_count = 3; // at max

        let source = PeerId("peer-a".into());
        let connected = vec![PeerId("peer-b".into())];

        let msgs = engine.propagate(&[unit], &source, &connected);
        assert_eq!(msgs.len(), 0); // should not propagate
    }

    #[test]
    fn expired_knowledge_not_propagated() {
        let mut engine = GossipEngine::new_empty("local", 5, 30);
        let mut unit = make_unit("ku_1", "Bash", "peer-a");
        // Set last_validated_at to 60 days ago
        unit.last_validated_at = epoch_secs().saturating_sub(60 * 86400);

        let source = PeerId("peer-a".into());
        let connected = vec![PeerId("peer-b".into())];

        let msgs = engine.propagate(&[unit], &source, &connected);
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn build_request_message_fields() {
        let engine = GossipEngine::new_empty("local", 5, 30);
        let msg = engine.build_request_message(1000);
        assert_eq!(msg.msg_type, MessageType::KnowledgeRequest);
        assert_eq!(
            msg.payload.get("since_epoch").and_then(|v| v.as_u64()),
            Some(1000)
        );
    }

    #[test]
    fn snapshot_pagination() {
        let mut store = empty_store();
        // Insert enough units to trigger pagination (each ~300 bytes, need >500KB total)
        // 2000 units × ~300 bytes ≈ 600KB > 500KB limit
        for i in 0..2000 {
            let unit = make_unit(&format!("ku_pag_{i}"), &format!("Tool_{i}_abcdef"), "local");
            store.insert(unit);
        }

        let engine = GossipEngine::new_empty("local", 5, 30);
        let request = RelayMessage {
            id: "req_1".into(),
            msg_type: MessageType::KnowledgeRequest,
            from_peer: "peer-a".into(),
            timestamp: 0,
            payload: serde_json::json!({ "since_epoch": 0 }),
        };

        let snapshots = engine.handle_request(&store, &request);
        // Should be paginated into multiple messages
        assert!(snapshots.len() > 1);

        // Each page should have page/total_pages metadata
        for (i, snap) in snapshots.iter().enumerate() {
            let page = snap.payload.get("page").and_then(|v| v.as_u64()).unwrap();
            let total = snap
                .payload
                .get("total_pages")
                .and_then(|v| v.as_u64())
                .unwrap();
            assert_eq!(page, (i + 1) as u64);
            assert_eq!(total, snapshots.len() as u64);
        }

        // All units should be covered
        let total_units: usize = snapshots
            .iter()
            .map(|s| parse_units_from_payload(s).len())
            .sum();
        assert_eq!(total_units, 2000);
    }
}

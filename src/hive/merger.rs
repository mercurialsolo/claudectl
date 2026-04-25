// Conflict resolution for merging incoming knowledge units.

use super::store::HiveStore;
use super::{KnowledgeUnit, semantic_key};

// ────────────────────────────────────────────────────────────────────────────
// Merge result
// ────────────────────────────────────────────────────────────────────────────

/// Result of attempting to merge a remote knowledge unit into the local store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeResult {
    /// New unit, no conflict — accepted.
    Accepted,
    /// Updated an existing peer unit with higher confidence/version.
    Updated,
    /// Conflict with local knowledge — rejected.
    RejectedLocal,
    /// Conflict with a peer unit that has higher confidence — rejected.
    RejectedPeer,
    /// Already have this exact version — duplicate.
    Duplicate,
}

/// Aggregate stats for a batch merge.
#[derive(Debug, Default)]
pub struct MergeStats {
    pub accepted: u32,
    pub updated: u32,
    pub rejected: u32,
    pub duplicates: u32,
}

// ────────────────────────────────────────────────────────────────────────────
// Merge logic
// ────────────────────────────────────────────────────────────────────────────

/// Merge a single incoming knowledge unit into the local store.
/// `local_peer_id` is this instance's peer identity — used to detect local-originated units.
pub fn merge_unit(
    store: &mut HiveStore,
    incoming: &KnowledgeUnit,
    local_peer_id: &str,
) -> MergeResult {
    let sk = semantic_key(incoming);

    match store.find_by_semantic_key(&sk) {
        None => {
            // No existing unit with this semantic key — accept
            store.insert(incoming.clone());
            MergeResult::Accepted
        }
        Some(existing) => {
            // Check if it's the same unit (same ID, same version)
            if existing.id == incoming.id && existing.version >= incoming.version {
                return MergeResult::Duplicate;
            }

            // Local-originated knowledge always wins
            if existing.source_peer == local_peer_id {
                super::store::log_conflict(existing, incoming);
                return MergeResult::RejectedLocal;
            }

            // Both are from peers — compare quality
            let existing_score = existing.confidence * existing.evidence_count as f64;
            let incoming_score = incoming.confidence * incoming.evidence_count as f64;
            let scores_equal = (incoming_score - existing_score).abs() < 1e-9;

            if incoming_score > existing_score
                || (scores_equal && incoming.version > existing.version)
            {
                // Incoming is better — update
                store.insert(incoming.clone());
                MergeResult::Updated
            } else {
                // Existing is better — reject
                super::store::log_conflict(existing, incoming);
                MergeResult::RejectedPeer
            }
        }
    }
}

/// Merge a batch of incoming units. Returns aggregate stats.
pub fn merge_batch(
    store: &mut HiveStore,
    units: &[KnowledgeUnit],
    local_peer_id: &str,
) -> MergeStats {
    let mut stats = MergeStats::default();

    for unit in units {
        match merge_unit(store, unit, local_peer_id) {
            MergeResult::Accepted => stats.accepted += 1,
            MergeResult::Updated => stats.updated += 1,
            MergeResult::RejectedLocal | MergeResult::RejectedPeer => stats.rejected += 1,
            MergeResult::Duplicate => stats.duplicates += 1,
        }
    }

    stats
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::{KnowledgeContent, KnowledgeScope};

    fn make_unit(
        id: &str,
        tool: &str,
        peer: &str,
        confidence: f64,
        evidence: u32,
    ) -> KnowledgeUnit {
        KnowledgeUnit {
            id: id.into(),
            scope: KnowledgeScope::Universal,
            category: crate::hive::KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Pattern {
                tool: tool.into(),
                command_pattern: Some("test".into()),
                preferred_action: "approve".into(),
                accept_rate: confidence,
                sample_count: evidence,
                conditions: vec![],
            },
            evidence_count: evidence,
            confidence,
            source_peer: peer.into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
        }
    }

    fn empty_store() -> HiveStore {
        HiveStore::load_from(std::path::Path::new("/nonexistent"))
    }

    #[test]
    fn accept_new_unit() {
        let mut store = empty_store();
        let unit = make_unit("ku_1", "Bash", "peer-a", 0.9, 10);
        assert_eq!(
            merge_unit(&mut store, &unit, "local"),
            MergeResult::Accepted
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn reject_when_local_exists() {
        let mut store = empty_store();
        // Insert a local-originated unit
        let local_unit = make_unit("ku_local", "Bash", "local", 0.7, 5);
        store.insert(local_unit);

        // Incoming peer unit with same semantic key
        let incoming = make_unit("ku_remote", "Bash", "peer-a", 0.99, 100);
        assert_eq!(
            merge_unit(&mut store, &incoming, "local"),
            MergeResult::RejectedLocal
        );
        // Store should still have the local unit
        assert_eq!(store.len(), 1);
        assert!(store.get("ku_local").is_some());
    }

    #[test]
    fn update_peer_with_higher_confidence() {
        let mut store = empty_store();
        // Insert a peer unit with low confidence
        let old = make_unit("ku_old", "Bash", "peer-a", 0.5, 5);
        store.insert(old);

        // Incoming peer unit with higher confidence*evidence
        let new = make_unit("ku_new", "Bash", "peer-b", 0.9, 20);
        assert_eq!(merge_unit(&mut store, &new, "local"), MergeResult::Updated);
        assert_eq!(store.len(), 1);
        assert!(store.get("ku_new").is_some());
        assert!(store.get("ku_old").is_none());
    }

    #[test]
    fn reject_peer_with_lower_confidence() {
        let mut store = empty_store();
        // Insert a peer unit with high confidence
        let strong = make_unit("ku_strong", "Bash", "peer-a", 0.95, 50);
        store.insert(strong);

        // Incoming peer unit with lower confidence*evidence
        let weak = make_unit("ku_weak", "Bash", "peer-b", 0.6, 3);
        assert_eq!(
            merge_unit(&mut store, &weak, "local"),
            MergeResult::RejectedPeer
        );
        assert_eq!(store.len(), 1);
        assert!(store.get("ku_strong").is_some());
    }

    #[test]
    fn detect_duplicate() {
        let mut store = empty_store();
        let unit = make_unit("ku_1", "Bash", "peer-a", 0.9, 10);
        store.insert(unit.clone());

        assert_eq!(
            merge_unit(&mut store, &unit, "local"),
            MergeResult::Duplicate
        );
    }

    #[test]
    fn update_same_id_newer_version() {
        let mut store = empty_store();
        let mut v1 = make_unit("ku_1", "Bash", "peer-a", 0.9, 10);
        v1.version = 1;
        store.insert(v1);

        let mut v2 = make_unit("ku_1", "Bash", "peer-a", 0.95, 15);
        v2.version = 2;
        assert_eq!(merge_unit(&mut store, &v2, "local"), MergeResult::Updated);
        assert_eq!(store.get("ku_1").unwrap().version, 2);
    }

    #[test]
    fn batch_merge_stats() {
        let mut store = empty_store();
        // Pre-populate with a local unit
        let local = make_unit("ku_local", "Bash", "local", 0.8, 10);
        store.insert(local);

        let units = vec![
            make_unit("ku_new1", "Read", "peer-a", 0.9, 10), // accepted (different tool)
            make_unit("ku_new2", "Bash", "peer-a", 0.99, 100), // rejected (local wins)
            make_unit("ku_new3", "Write", "peer-b", 0.7, 5), // accepted
        ];

        let stats = merge_batch(&mut store, &units, "local");
        assert_eq!(stats.accepted, 2);
        assert_eq!(stats.rejected, 1);
        assert_eq!(stats.duplicates, 0);
    }
}

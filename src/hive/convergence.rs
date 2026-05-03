// Convergence metrics (#229) — how widely has each unit propagated, and
// how close is each peer to having seen everything we have?
//
// Unlike the other discovery/effectiveness queries, convergence depends on
// the gossip-layer's per-peer sync state. That state lives in
// `~/.claudectl/hive/sync_state.json` (managed by `hive::gossip`), so this
// module reads it directly when `relay` is enabled. When `relay` is off
// the file simply doesn't exist and `peer_convergence` returns empty.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::store::HiveStore;

// ────────────────────────────────────────────────────────────────────────────
// Per-peer convergence row
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PeerConvergence {
    pub peer_id: String,
    /// Total units in the local store at query time.
    pub local_total: u64,
    /// Units we've already shipped to this peer (subset of local_total).
    pub units_sent: u64,
    /// `units_sent / local_total` (0.0 when local_total == 0).
    pub ratio: f64,
    /// Last time we successfully synced to this peer (epoch secs, 0 = never).
    pub last_sync_epoch: u64,
}

impl PeerConvergence {
    /// True when this peer has seen ≥90% of what we have. Matches the
    /// "convergence milestone" threshold suggested in #229.
    pub fn is_converged(&self) -> bool {
        self.ratio >= 0.9
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Sync state — local read-only mirror of the gossip layer's serialized state
// ────────────────────────────────────────────────────────────────────────────

/// Mirrors `hive::gossip::PeerSyncState`'s on-disk format. Kept here as a
/// separate type so this module can be compiled without the `relay` feature
/// (the gossip module is cfg-gated, but the file format isn't).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncStateRecord {
    pub peer_id: String,
    pub last_sync_epoch: u64,
    pub units_sent: HashSet<String>,
}

fn sync_state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("sync_state.json")
}

fn load_sync_states() -> HashMap<String, SyncStateRecord> {
    let path = sync_state_path();
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

// ────────────────────────────────────────────────────────────────────────────
// Queries
// ────────────────────────────────────────────────────────────────────────────

/// Build per-peer convergence rows. Sorted by ratio asc — peers who lag
/// behind appear first so operators can quickly spot stalled syncs.
pub fn peer_convergence(store: &HiveStore) -> Vec<PeerConvergence> {
    let states = load_sync_states();
    let local_total = store.len() as u64;

    let mut rows: Vec<PeerConvergence> = states
        .into_values()
        .map(|s| {
            // Only count units this peer was sent that we still have. A unit
            // that's been evicted shouldn't make us look more converged than
            // we are.
            let kept: u64 = s
                .units_sent
                .iter()
                .filter(|id| store.get(id).is_some())
                .count() as u64;
            let ratio = if local_total == 0 {
                0.0
            } else {
                kept as f64 / local_total as f64
            };
            PeerConvergence {
                peer_id: s.peer_id,
                local_total,
                units_sent: kept,
                ratio,
                last_sync_epoch: s.last_sync_epoch,
            }
        })
        .collect();

    rows.sort_by(|a, b| {
        a.ratio
            .partial_cmp(&b.ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.peer_id.cmp(&b.peer_id))
    });
    rows
}

/// Median peer convergence ratio. Useful as a single-number network health
/// signal: "median peer has seen X% of what we know".
pub fn median_convergence(rows: &[PeerConvergence]) -> Option<f64> {
    if rows.is_empty() {
        return None;
    }
    let mut ratios: Vec<f64> = rows.iter().map(|r| r.ratio).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = ratios.len() / 2;
    Some(if ratios.len() % 2 == 0 {
        (ratios[mid - 1] + ratios[mid]) / 2.0
    } else {
        ratios[mid]
    })
}

/// Count of peers fully converged (≥90%).
pub fn converged_peer_count(rows: &[PeerConvergence]) -> usize {
    rows.iter().filter(|r| r.is_converged()).count()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn row(peer: &str, ratio: f64) -> PeerConvergence {
        PeerConvergence {
            peer_id: peer.into(),
            local_total: 100,
            units_sent: (ratio * 100.0) as u64,
            ratio,
            last_sync_epoch: 0,
        }
    }

    #[test]
    fn convergence_threshold() {
        assert!(row("a", 0.9).is_converged());
        assert!(row("b", 0.95).is_converged());
        assert!(!row("c", 0.89).is_converged());
        assert!(!row("d", 0.5).is_converged());
    }

    #[test]
    fn median_empty() {
        assert!(median_convergence(&[]).is_none());
    }

    #[test]
    fn median_odd() {
        let rows = [row("a", 0.2), row("b", 0.5), row("c", 0.9)];
        assert_eq!(median_convergence(&rows), Some(0.5));
    }

    #[test]
    fn median_even() {
        let rows = [row("a", 0.2), row("b", 0.4), row("c", 0.6), row("d", 0.8)];
        assert_eq!(median_convergence(&rows), Some(0.5));
    }

    #[test]
    fn count_converged() {
        let rows = [
            row("a", 0.95),
            row("b", 0.92),
            row("c", 0.5),
            row("d", 0.99),
        ];
        assert_eq!(converged_peer_count(&rows), 3);
    }
}

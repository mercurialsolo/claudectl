// Local hive knowledge store. Append-only JSONL with in-memory semantic index.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use super::{KnowledgeUnit, semantic_key};

// ────────────────────────────────────────────────────────────────────────────
// Paths
// ────────────────────────────────────────────────────────────────────────────

fn hive_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claudectl").join("hive")
}

fn knowledge_path() -> PathBuf {
    hive_dir().join("knowledge.jsonl")
}

fn conflicts_path() -> PathBuf {
    hive_dir().join("conflicts.jsonl")
}

// ────────────────────────────────────────────────────────────────────────────
// HiveStore
// ────────────────────────────────────────────────────────────────────────────

/// In-memory knowledge store backed by a JSONL file.
pub struct HiveStore {
    /// All knowledge units, keyed by ID.
    units: HashMap<String, KnowledgeUnit>,
    /// Semantic key → unit ID, for dedup/merge lookups.
    semantic_index: HashMap<String, String>,
}

impl HiveStore {
    /// Load the store from disk, or create an empty one.
    pub fn load() -> Self {
        let path = knowledge_path();
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };

        if let Ok(content) = fs::read_to_string(&path) {
            for line in content.lines() {
                if let Ok(unit) = serde_json::from_str::<KnowledgeUnit>(line) {
                    let sk = semantic_key(&unit);
                    store.semantic_index.insert(sk, unit.id.clone());
                    store.units.insert(unit.id.clone(), unit);
                }
            }
        }

        store
    }

    /// Load from a specific path (for testing).
    pub fn load_from(path: &std::path::Path) -> Self {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };

        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                if let Ok(unit) = serde_json::from_str::<KnowledgeUnit>(line) {
                    let sk = semantic_key(&unit);
                    store.semantic_index.insert(sk, unit.id.clone());
                    store.units.insert(unit.id.clone(), unit);
                }
            }
        }

        store
    }

    /// Insert or update a knowledge unit. Returns true if this is a new unit.
    /// If a different unit with the same semantic key exists, it is replaced.
    pub fn insert(&mut self, unit: KnowledgeUnit) -> bool {
        let sk = semantic_key(&unit);
        let is_new = !self.units.contains_key(&unit.id);

        // If a different unit held this semantic key, remove it
        if let Some(old_id) = self.semantic_index.get(&sk) {
            if *old_id != unit.id {
                self.units.remove(&old_id.clone());
            }
        }

        self.semantic_index.insert(sk, unit.id.clone());
        self.units.insert(unit.id.clone(), unit);
        is_new
    }

    /// Get a unit by ID.
    pub fn get(&self, id: &str) -> Option<&KnowledgeUnit> {
        self.units.get(id)
    }

    /// Find a unit by its semantic key.
    pub fn find_by_semantic_key(&self, key: &str) -> Option<&KnowledgeUnit> {
        self.semantic_index
            .get(key)
            .and_then(|id| self.units.get(id))
    }

    /// Get the semantic key for a unit if it exists.
    pub fn semantic_key_for(&self, unit: &KnowledgeUnit) -> String {
        semantic_key(unit)
    }

    /// All units.
    pub fn all_units(&self) -> Vec<&KnowledgeUnit> {
        self.units.values().collect()
    }

    /// Units created or validated after a given epoch.
    pub fn units_since(&self, epoch: u64) -> Vec<&KnowledgeUnit> {
        self.units
            .values()
            .filter(|u| u.last_validated_at >= epoch)
            .collect()
    }

    /// Units matching a specific scope.
    pub fn by_scope(&self, scope: &super::KnowledgeScope) -> Vec<&KnowledgeUnit> {
        self.units.values().filter(|u| &u.scope == scope).collect()
    }

    /// Units from a specific source peer.
    pub fn by_source(&self, peer: &str) -> Vec<&KnowledgeUnit> {
        self.units
            .values()
            .filter(|u| u.source_peer == peer)
            .collect()
    }

    /// Remove a unit by ID. Returns true if it existed.
    pub fn remove(&mut self, id: &str) -> bool {
        if let Some(unit) = self.units.remove(id) {
            let sk = semantic_key(&unit);
            self.semantic_index.remove(&sk);
            true
        } else {
            false
        }
    }

    /// Number of units in the store.
    pub fn len(&self) -> usize {
        self.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// Compact the store: remove expired units, prune stale peers, enforce max_units cap.
    /// Returns evicted units (can be archived to cold storage).
    pub fn compact(
        &mut self,
        ttl_days: u32,
        max_units: usize,
        stale_peer_days: u32,
        trust_store: Option<&super::trust::TrustStore>,
    ) -> Vec<super::KnowledgeUnit> {
        let now = super::epoch_secs();
        let ttl_secs = ttl_days as u64 * 86400;
        let stale_secs = stale_peer_days as u64 * 86400;
        let mut evicted = Vec::new();

        // 1. Remove expired units (past TTL without revalidation)
        let expired_ids: Vec<String> = self
            .units
            .values()
            .filter(|u| now.saturating_sub(u.last_validated_at) > ttl_secs)
            .map(|u| u.id.clone())
            .collect();
        for id in &expired_ids {
            if let Some(unit) = self.units.get(id).cloned() {
                evicted.push(unit);
            }
            self.remove(id);
        }

        // 2. Prune knowledge from stale peers
        if let Some(ts) = trust_store {
            let stale_peers: Vec<String> = self
                .units
                .values()
                .map(|u| u.source_peer.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .filter(|peer| {
                    ts.get(peer)
                        .is_some_and(|t| now.saturating_sub(t.last_sync) > stale_secs)
                })
                .collect();
            for peer in &stale_peers {
                let ids: Vec<String> = self
                    .units
                    .values()
                    .filter(|u| &u.source_peer == peer)
                    .map(|u| u.id.clone())
                    .collect();
                for id in &ids {
                    if let Some(unit) = self.units.get(id).cloned() {
                        evicted.push(unit);
                    }
                    self.remove(id);
                }
            }
        }

        // 3. Enforce max_units cap — evict lowest confidence * evidence score
        if max_units > 0 && self.units.len() > max_units {
            let mut scored: Vec<(String, f64)> = self
                .units
                .values()
                .map(|u| (u.id.clone(), u.confidence * u.evidence_count as f64))
                .collect();
            scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            let to_evict = self.units.len() - max_units;
            for (id, _) in scored.into_iter().take(to_evict) {
                if let Some(unit) = self.units.get(&id).cloned() {
                    evicted.push(unit);
                }
                self.remove(&id);
            }
        }

        evicted
    }

    /// Save the entire store to disk (atomic rewrite via temp file + rename).
    pub fn save(&self) -> std::io::Result<()> {
        let dir = hive_dir();
        fs::create_dir_all(&dir)?;
        let path = knowledge_path();
        let tmp_path = path.with_extension("jsonl.tmp");

        let lines: Vec<String> = self
            .units
            .values()
            .filter_map(|u| serde_json::to_string(u).ok())
            .collect();

        fs::write(&tmp_path, lines.join("\n") + "\n")?;
        fs::rename(&tmp_path, &path)
    }

    /// Save to a specific path (for testing).
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let lines: Vec<String> = self
            .units
            .values()
            .filter_map(|u| serde_json::to_string(u).ok())
            .collect();

        fs::write(path, lines.join("\n") + "\n")
    }

    /// Append a single unit to the JSONL file (incremental write).
    pub fn append(&self, unit: &KnowledgeUnit) -> std::io::Result<()> {
        let dir = hive_dir();
        fs::create_dir_all(&dir)?;
        let path = knowledge_path();

        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let json = serde_json::to_string(unit)
            .map_err(|e| std::io::Error::other(format!("serialize: {e}")))?;
        writeln!(file, "{json}")
    }

    /// Export all units as a JSON array string.
    pub fn export_json(&self) -> String {
        let units: Vec<&KnowledgeUnit> = self.units.values().collect();
        serde_json::to_string_pretty(&units).unwrap_or_else(|_| "[]".into())
    }

    /// Import units from a JSON array string. Returns count imported.
    pub fn import_json(&mut self, json: &str) -> Result<u32, String> {
        let units: Vec<KnowledgeUnit> =
            serde_json::from_str(json).map_err(|e| format!("parse error: {e}"))?;

        let mut count = 0;
        for unit in units {
            if self.insert(unit) {
                count += 1;
            }
        }
        Ok(count)
    }
}

/// Log a merge conflict for diagnostics.
pub fn log_conflict(local: &KnowledgeUnit, incoming: &KnowledgeUnit) {
    let dir = hive_dir();
    let _ = fs::create_dir_all(&dir);
    let path = conflicts_path();

    let record = serde_json::json!({
        "ts": super::epoch_secs(),
        "local_id": local.id,
        "local_peer": local.source_peer,
        "local_confidence": local.confidence,
        "incoming_id": incoming.id,
        "incoming_peer": incoming.source_peer,
        "incoming_confidence": incoming.confidence,
        "semantic_key": semantic_key(local),
    });

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
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
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
        }
    }

    #[test]
    fn insert_and_get() {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        let unit = make_unit("ku_1", "Bash", "peer-a");
        assert!(store.insert(unit));
        assert_eq!(store.len(), 1);
        assert!(store.get("ku_1").is_some());
    }

    #[test]
    fn find_by_semantic_key() {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        let unit = make_unit("ku_1", "Bash", "peer-a");
        store.insert(unit);

        let found = store.find_by_semantic_key("universal/pattern:Bash:test");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "ku_1");

        assert!(
            store
                .find_by_semantic_key("universal/pattern:Read:test")
                .is_none()
        );
    }

    #[test]
    fn remove_unit() {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        store.insert(make_unit("ku_1", "Bash", "peer-a"));
        assert_eq!(store.len(), 1);

        assert!(store.remove("ku_1"));
        assert_eq!(store.len(), 0);
        assert!(store.get("ku_1").is_none());
        assert!(
            store
                .find_by_semantic_key("universal/pattern:Bash:test")
                .is_none()
        );

        assert!(!store.remove("nonexistent"));
    }

    #[test]
    fn duplicate_insert_not_new() {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        let unit = make_unit("ku_1", "Bash", "peer-a");
        assert!(store.insert(unit.clone()));
        assert!(!store.insert(unit)); // same ID = not new
    }

    #[test]
    fn by_source_filters() {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        store.insert(make_unit("ku_1", "Bash", "peer-a"));
        store.insert(make_unit("ku_2", "Read", "peer-b"));
        store.insert(make_unit("ku_3", "Write", "peer-a"));

        assert_eq!(store.by_source("peer-a").len(), 2);
        assert_eq!(store.by_source("peer-b").len(), 1);
        assert_eq!(store.by_source("peer-c").len(), 0);
    }

    #[test]
    fn export_import_roundtrip() {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        store.insert(make_unit("ku_1", "Bash", "peer-a"));
        store.insert(make_unit("ku_2", "Read", "peer-b"));

        let json = store.export_json();

        let mut store2 = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        let imported = store2.import_json(&json).unwrap();
        assert_eq!(imported, 2);
        assert_eq!(store2.len(), 2);
        assert!(store2.get("ku_1").is_some());
        assert!(store2.get("ku_2").is_some());
    }

    #[test]
    fn import_deduplicates() {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        store.insert(make_unit("ku_1", "Bash", "peer-a"));

        let json = store.export_json();
        let imported = store.import_json(&json).unwrap();
        assert_eq!(imported, 0); // already exists
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        store.insert(make_unit("ku_1", "Bash", "peer-a"));
        store.insert(make_unit("ku_2", "Read", "peer-b"));
        store.save_to(&path).unwrap();

        let loaded = HiveStore::load_from(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.get("ku_1").is_some());
        assert!(loaded.get("ku_2").is_some());
    }

    #[test]
    fn units_since_filters_by_time() {
        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        let mut old = make_unit("ku_1", "Bash", "peer-a");
        old.last_validated_at = 500;
        let mut new = make_unit("ku_2", "Read", "peer-b");
        new.last_validated_at = 2000;

        store.insert(old);
        store.insert(new);

        assert_eq!(store.units_since(1000).len(), 1);
        assert_eq!(store.units_since(1000)[0].id, "ku_2");
    }

    #[test]
    fn is_empty_and_len() {
        let store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }
}

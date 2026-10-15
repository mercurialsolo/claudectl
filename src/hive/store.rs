// Local hive knowledge store. Append-only JSONL with in-memory semantic index.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use super::{KnowledgeUnit, effective_confidence, epoch_secs, semantic_key};

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

        // 3. Enforce max_units cap — evict lowest effective_confidence * evidence
        // score. Effective confidence (#224) applies time decay so stale units
        // are preferentially evicted before fresh ones.
        if max_units > 0 && self.units.len() > max_units {
            let now = epoch_secs();
            let mut scored: Vec<(String, f64)> = self
                .units
                .values()
                .map(|u| {
                    (
                        u.id.clone(),
                        effective_confidence(u, now) * u.evidence_count as f64,
                    )
                })
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
        // See `hive::skip_ambient_write`: this would overwrite the developer's
        // real knowledge store with whatever a test's store holds.
        if super::skip_ambient_write() {
            return Ok(());
        }
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
///
/// Skipped in unit tests: `merge_unit` calls this transitively, `hive_dir`
/// resolves `HOME` at call time, and the merger's tests do not override it — so
/// before #552 running them appended fixture records to the developer's own
/// `~/.claudectl/hive/conflicts.jsonl`. The file on this machine had 1,771 of
/// them and not one real conflict. The writer itself is covered by testing
/// [`log_conflict_in`] directly, which is where the interesting behaviour is.
pub fn log_conflict(local: &KnowledgeUnit, incoming: &KnowledgeUnit) {
    if super::skip_ambient_write() {
        return;
    }
    log_conflict_in(&hive_dir(), local, incoming);
}

/// [`log_conflict`] into an explicit directory.
///
/// # Why one `write_all` and not `writeln!`
///
/// `File` is unbuffered, so `writeln!` reaches `write_fmt` and emits the JSON
/// and the newline as **two** writes. `O_APPEND` makes each individual write
/// atomic, not the pair — so a concurrent logger lands its record in between
/// and two records end up sharing a line. Gossip merges batches from several
/// peers, which is exactly where that happens: 131 of 1,618 lines on this
/// machine held more than one record (#552).
///
/// Building the line and its terminator into one buffer makes a record either
/// wholly present or wholly absent, which is what a diagnostic log has to be
/// before anything can read it.
pub fn log_conflict_in(dir: &std::path::Path, local: &KnowledgeUnit, incoming: &KnowledgeUnit) {
    let _ = fs::create_dir_all(dir);
    let path = dir.join("conflicts.jsonl");

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

    let Ok(mut line) = serde_json::to_string(&record) else {
        return;
    };
    line.push('\n');

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// How many conflict records the log holds.
///
/// Counts **records**, not lines. A log written before #552 can hold several
/// concatenated records on one line, and `serde_json`'s streaming reader takes
/// those in its stride — so an existing file reports honestly without needing a
/// migration, and `hive status` stops undercounting itself (1,617 shown against
/// 1,771 actual, on this machine).
pub fn conflict_count_in(dir: &std::path::Path) -> usize {
    let Ok(body) = fs::read_to_string(dir.join("conflicts.jsonl")) else {
        return 0;
    };
    serde_json::Deserializer::from_str(&body)
        .into_iter::<serde_json::Value>()
        .take_while(|r| r.is_ok())
        .count()
}

/// [`conflict_count_in`] for the real hive directory.
pub fn conflict_count() -> usize {
    conflict_count_in(&hive_dir())
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::{KnowledgeContent, KnowledgeScope};

    /// Every record must be on its own line and parse on its own. This is the
    /// #552 regression: `writeln!` on an unbuffered `File` wrote the JSON and
    /// the newline separately, so a concurrent logger split a record in two.
    #[test]
    fn concurrent_conflict_logging_never_splits_a_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        const THREADS: usize = 8;
        const EACH: usize = 60;

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for i in 0..EACH {
                        let local = make_unit(&format!("l{t}_{i}"), "Bash", "local");
                        let incoming = make_unit(&format!("r{t}_{i}"), "Bash", "peer-a");
                        log_conflict_in(&dir, &local, &incoming);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let body = fs::read_to_string(dir.join("conflicts.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            THREADS * EACH,
            "expected one line per record, got {} lines for {} records",
            lines.len(),
            THREADS * EACH
        );
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("line is not one whole record ({e}): {line}"));
        }
        assert_eq!(conflict_count_in(&dir), THREADS * EACH);
    }

    /// A log written before the fix has concatenated records on one line. The
    /// count has to be right on those too, or the number only becomes honest
    /// after someone deletes their file.
    #[test]
    fn the_count_reads_a_legacy_run_together_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path()).unwrap();
        // Three records, no separators — exactly the shape found on disk.
        let one = r#"{"ts":1,"local_id":"ku_local","semantic_key":"k"}"#;
        fs::write(
            tmp.path().join("conflicts.jsonl"),
            format!("{one}{one}{one}\n"),
        )
        .unwrap();

        // What the old line counter would have said.
        let body = fs::read_to_string(tmp.path().join("conflicts.jsonl")).unwrap();
        assert_eq!(body.lines().filter(|l| !l.is_empty()).count(), 1);
        // What it actually holds.
        assert_eq!(conflict_count_in(tmp.path()), 3);
    }

    #[test]
    fn the_count_is_zero_with_no_log_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(conflict_count_in(tmp.path()), 0);
    }

    /// Truncated trailing bytes must not panic or inflate the count — the log
    /// is appended to by a process that can be killed mid-write.
    #[test]
    fn a_truncated_tail_counts_the_whole_records_before_it() {
        let tmp = tempfile::tempdir().unwrap();
        let one = r#"{"ts":1,"local_id":"ku_local","semantic_key":"k"}"#;
        fs::write(
            tmp.path().join("conflicts.jsonl"),
            format!("{one}\n{one}\n{{\"ts\":3,\"local_i"),
        )
        .unwrap();
        assert_eq!(conflict_count_in(tmp.path()), 2);
    }

    /// Every byte under the real `~/.claudectl/hive` must survive a test run
    /// (#552). Before the fix, `cargo test` replaced the developer's
    /// `knowledge.jsonl` with fixture units — `save` persists whatever the
    /// store holds — and appended to three logs besides.
    ///
    /// Exercises the ambient entry points, the ones that resolve `$HOME`
    /// themselves, and asserts the directory is unchanged afterwards.
    #[test]
    fn the_real_hive_directory_survives_a_test_run() {
        fn snapshot() -> Vec<(PathBuf, usize)> {
            let Ok(entries) = fs::read_dir(hive_dir()) else {
                return Vec::new();
            };
            let mut out: Vec<(PathBuf, usize)> = entries
                .flatten()
                .filter(|e| e.path().is_file())
                .filter_map(|e| fs::read(e.path()).ok().map(|b| (e.path(), b.len())))
                .collect();
            out.sort();
            out
        }

        let before = snapshot();

        let mut store = HiveStore {
            units: HashMap::new(),
            semantic_index: HashMap::new(),
        };
        store.insert(make_unit("ku_local", "Bash", "local"));

        // knowledge.jsonl — the data-loss one.
        store.save().unwrap();
        // conflicts.jsonl, and resolutions.jsonl through the merger.
        let incoming = make_unit("ku_remote", "Bash", "peer-a");
        crate::hive::merger::merge_unit(&mut store, &incoming, "local");
        log_conflict(&make_unit("a", "Bash", "local"), &incoming);

        assert_eq!(
            before,
            snapshot(),
            "a test run changed {} — see hive::skip_ambient_write",
            hive_dir().display()
        );
    }

    /// The choke point the test above depends on.
    #[test]
    fn ambient_writes_are_suppressed_in_a_test_build() {
        assert!(crate::hive::skip_ambient_write());
    }

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
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
            sharing_consent: None,
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

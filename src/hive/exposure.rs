// Per-unit exposure: which knowledge units the user has chosen to broadcast.
//
// Stored separately from the unit JSONL so the wire format and store schema
// stay unchanged. Two-mode model:
//   - `share_mode = "auto"`   → missing entry means exposed (current behavior).
//   - `share_mode = "manual"` → missing entry means hidden; user opts in per unit.
// Explicit `expose` / `hide` entries always win regardless of mode.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExposureState {
    Expose,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareMode {
    Auto,
    Manual,
}

impl ShareMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "auto" => Some(ShareMode::Auto),
            "manual" => Some(ShareMode::Manual),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    pub fn default_exposed(&self) -> bool {
        matches!(self, Self::Auto)
    }
}

fn exposure_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("exposure.json")
}

#[derive(Default)]
pub struct ExposureStore {
    entries: HashMap<String, ExposureState>,
}

impl ExposureStore {
    pub fn load() -> Self {
        Self::load_from(&exposure_path())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        let entries = fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, ExposureState>>(&s).ok())
            .unwrap_or_default();
        ExposureStore { entries }
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_to(&exposure_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.entries)
            .map_err(|e| io::Error::other(format!("serialize exposure: {e}")))?;
        fs::write(path, json)
    }

    pub fn get(&self, id: &str) -> Option<ExposureState> {
        self.entries.get(id).copied()
    }

    pub fn set(&mut self, id: &str, state: ExposureState) {
        self.entries.insert(id.to_string(), state);
    }

    pub fn clear(&mut self, id: &str) {
        self.entries.remove(id);
    }

    pub fn is_exposed(&self, id: &str, mode: ShareMode) -> bool {
        match self.entries.get(id) {
            Some(ExposureState::Expose) => true,
            Some(ExposureState::Hide) => false,
            None => mode.default_exposed(),
        }
    }

    pub fn entries(&self) -> &HashMap<String, ExposureState> {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn share_mode_parse() {
        assert_eq!(ShareMode::parse("auto"), Some(ShareMode::Auto));
        assert_eq!(ShareMode::parse("Manual"), Some(ShareMode::Manual));
        assert_eq!(ShareMode::parse(" auto "), Some(ShareMode::Auto));
        assert_eq!(ShareMode::parse("nope"), None);
    }

    #[test]
    fn auto_mode_defaults_to_exposed() {
        let store = ExposureStore::default();
        assert!(store.is_exposed("ku_unknown", ShareMode::Auto));
    }

    #[test]
    fn manual_mode_defaults_to_hidden() {
        let store = ExposureStore::default();
        assert!(!store.is_exposed("ku_unknown", ShareMode::Manual));
    }

    #[test]
    fn explicit_state_overrides_mode() {
        let mut store = ExposureStore::default();
        store.set("ku_a", ExposureState::Hide);
        store.set("ku_b", ExposureState::Expose);

        assert!(!store.is_exposed("ku_a", ShareMode::Auto));
        assert!(store.is_exposed("ku_b", ShareMode::Manual));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("exposure.json");

        let mut store = ExposureStore::default();
        store.set("ku_1", ExposureState::Expose);
        store.set("ku_2", ExposureState::Hide);
        store.save_to(&path).unwrap();

        let loaded = ExposureStore::load_from(&path);
        assert_eq!(loaded.get("ku_1"), Some(ExposureState::Expose));
        assert_eq!(loaded.get("ku_2"), Some(ExposureState::Hide));
        assert_eq!(loaded.get("ku_missing"), None);
    }

    #[test]
    fn clear_removes_entry() {
        let mut store = ExposureStore::default();
        store.set("ku_a", ExposureState::Hide);
        store.clear("ku_a");
        assert_eq!(store.get("ku_a"), None);
    }
}

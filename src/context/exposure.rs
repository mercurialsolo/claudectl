//! Per-category index exposure (#428, RFC §4.2).
//!
//! Mirrors `hive::exposure::ExposureStore`'s semantics exactly — explicit
//! entries always win, and a missing entry means exposed in `auto` mode and
//! hidden in `manual` mode — but keys on *index source category* rather than on
//! individual knowledge-unit id.
//!
//! The types are defined locally rather than imported from `hive::exposure`
//! because this module is not feature-gated and hive is. The wire values match
//! (`"expose"` / `"hide"`), so the two files are interchangeable by eye, and a
//! `From` conversion exists when hive is compiled in.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which index sources exist. One exposure decision per variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    ClaudeMd,
    Readme,
    Docs,
    ModuleMap,
    Skills,
    HiveUnits,
}

impl Category {
    pub const ALL: &'static [Category] = &[
        Category::ClaudeMd,
        Category::Readme,
        Category::Docs,
        Category::ModuleMap,
        Category::Skills,
        Category::HiveUnits,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Category::ClaudeMd => "claude_md",
            Category::Readme => "readme",
            Category::Docs => "docs",
            Category::ModuleMap => "module_map",
            Category::Skills => "skills",
            Category::HiveUnits => "hive_units",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Category::ALL
            .iter()
            .copied()
            .find(|c| c.label() == s.trim().to_lowercase())
    }
}

/// An explicit per-category decision. Same wire values as
/// `hive::exposure::ExposureState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExposureState {
    Expose,
    Hide,
}

/// What a missing entry means. Same semantics as `hive::exposure::ShareMode`.
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
            ShareMode::Auto => "auto",
            ShareMode::Manual => "manual",
        }
    }

    /// Auto exposes by default; manual hides by default.
    pub fn default_exposed(&self) -> bool {
        matches!(self, ShareMode::Auto)
    }
}

#[cfg(feature = "hive")]
impl From<crate::hive::exposure::ShareMode> for ShareMode {
    fn from(m: crate::hive::exposure::ShareMode) -> Self {
        match m {
            crate::hive::exposure::ShareMode::Auto => ShareMode::Auto,
            crate::hive::exposure::ShareMode::Manual => ShareMode::Manual,
        }
    }
}

/// Resolve the share mode the same way every hive consumer does: parse
/// `Config.hive.share_mode`, defaulting to `auto` on anything unparseable.
///
/// Reusing that one knob rather than adding an index-specific config field —
/// "how freely does this machine share" is one question, not two.
pub fn mode_from_config() -> ShareMode {
    #[cfg(feature = "hive")]
    {
        let cfg = crate::config::Config::load();
        let share = cfg.hive.map(|h| h.share_mode).unwrap_or_default();
        ShareMode::parse(&share).unwrap_or(ShareMode::Auto)
    }
    #[cfg(not(feature = "hive"))]
    {
        ShareMode::Auto
    }
}

/// Per-category exposure decisions, persisted as a flat map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexExposure {
    entries: HashMap<String, ExposureState>,
}

impl IndexExposure {
    pub fn load() -> Self {
        Self::load_from(&exposure_path())
    }

    /// Load from an explicit path.
    ///
    /// A *missing* file means "no explicit decisions", so every category falls
    /// to its mode default. A file that exists but will not parse is a
    /// different thing and fails **closed**: `auto` is the default mode, so
    /// discarding the map would silently republish everything the operator had
    /// hidden. With no CLI yet, hand-editing is the only way this file gets
    /// written, which makes a typo the likely case rather than a rare one.
    ///
    /// A single unparseable *value* likewise reads as `Hide` rather than being
    /// dropped, so one bad entry cannot expose its own category.
    pub fn load_from(path: &Path) -> Self {
        let Ok(body) = std::fs::read_to_string(path) else {
            return Self::default();
        };

        let Ok(raw) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&body) else {
            return Self::all_hidden();
        };

        let entries = raw
            .into_iter()
            .map(|(key, value)| {
                let state =
                    serde_json::from_value::<ExposureState>(value).unwrap_or(ExposureState::Hide);
                (key, state)
            })
            .collect();
        Self { entries }
    }

    /// Every known category explicitly hidden — the fail-closed position.
    pub fn all_hidden() -> Self {
        let entries = Category::ALL
            .iter()
            .map(|c| (c.label().to_string(), ExposureState::Hide))
            .collect();
        Self { entries }
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_to(&exposure_path())
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(&self.entries).map_err(io::Error::other)?;
        std::fs::write(path, body)
    }

    pub fn get(&self, category: Category) -> Option<ExposureState> {
        self.entries.get(category.label()).copied()
    }

    pub fn set(&mut self, category: Category, state: ExposureState) {
        self.entries.insert(category.label().to_string(), state);
    }

    pub fn clear(&mut self, category: Category) {
        self.entries.remove(category.label());
    }

    /// Explicit entries win; otherwise the mode decides.
    pub fn is_exposed(&self, category: Category, mode: ShareMode) -> bool {
        match self.get(category) {
            Some(ExposureState::Expose) => true,
            Some(ExposureState::Hide) => false,
            None => mode.default_exposed(),
        }
    }
}

fn exposure_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("access")
        .join("index-exposure.json")
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_exposes_by_default_and_manual_hides() {
        let store = IndexExposure::default();
        for c in Category::ALL {
            assert!(
                store.is_exposed(*c, ShareMode::Auto),
                "{} should default on in auto",
                c.label()
            );
            assert!(
                !store.is_exposed(*c, ShareMode::Manual),
                "{} should default off in manual",
                c.label()
            );
        }
    }

    #[test]
    fn an_explicit_entry_wins_in_both_modes() {
        let mut store = IndexExposure::default();
        store.set(Category::ModuleMap, ExposureState::Hide);
        store.set(Category::Skills, ExposureState::Expose);

        // Hidden even though auto would expose.
        assert!(!store.is_exposed(Category::ModuleMap, ShareMode::Auto));
        // Exposed even though manual would hide.
        assert!(store.is_exposed(Category::Skills, ShareMode::Manual));
    }

    #[test]
    fn clear_returns_a_category_to_its_mode_default() {
        let mut store = IndexExposure::default();
        store.set(Category::Docs, ExposureState::Hide);
        assert!(!store.is_exposed(Category::Docs, ShareMode::Auto));
        store.clear(Category::Docs);
        assert!(store.is_exposed(Category::Docs, ShareMode::Auto));
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("index-exposure.json");

        let mut store = IndexExposure::default();
        store.set(Category::HiveUnits, ExposureState::Hide);
        store.save_to(&path).unwrap();

        let back = IndexExposure::load_from(&path);
        assert_eq!(back, store);
        assert_eq!(back.get(Category::HiveUnits), Some(ExposureState::Hide));
    }

    #[test]
    fn wire_values_match_the_hive_store() {
        // The two files should be readable side by side.
        let mut store = IndexExposure::default();
        store.set(Category::Readme, ExposureState::Expose);
        store.set(Category::Docs, ExposureState::Hide);
        let json = serde_json::to_string(&store.entries).unwrap();
        assert!(json.contains("\"readme\":\"expose\""), "{json}");
        assert!(json.contains("\"docs\":\"hide\""), "{json}");
    }

    #[test]
    fn a_missing_file_leaves_every_category_on_its_mode_default() {
        // Absent is not the same as broken: nothing has been decided, so the
        // mode decides.
        let dir = tempfile::tempdir().unwrap();
        let store = IndexExposure::load_from(&dir.path().join("nope.json"));
        assert_eq!(store, IndexExposure::default());
        assert!(store.is_exposed(Category::Docs, ShareMode::Auto));
        assert!(!store.is_exposed(Category::Docs, ShareMode::Manual));
    }

    #[test]
    fn a_corrupt_file_fails_closed_rather_than_republishing_everything() {
        // `auto` is the default mode, so discarding the map on a parse error
        // would expose every category the operator had hidden — the worst
        // direction for a typo in a hand-edited file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.json");
        std::fs::write(&path, "{ not json").unwrap();

        let store = IndexExposure::load_from(&path);
        for c in Category::ALL {
            assert!(
                !store.is_exposed(*c, ShareMode::Auto),
                "{} should be hidden when the file will not parse",
                c.label()
            );
        }
    }

    #[test]
    fn one_bad_value_hides_only_its_own_category() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.json");
        std::fs::write(&path, r#"{"docs":"hide","module_map":"bogus"}"#).unwrap();

        let store = IndexExposure::load_from(&path);
        assert!(!store.is_exposed(Category::Docs, ShareMode::Auto));
        // The unparseable value reads as hide, not as absent-so-exposed.
        assert!(!store.is_exposed(Category::ModuleMap, ShareMode::Auto));
        // A category the file never mentioned keeps its mode default.
        assert!(store.is_exposed(Category::Readme, ShareMode::Auto));
    }

    #[test]
    fn category_labels_round_trip() {
        for c in Category::ALL {
            assert_eq!(Category::parse(c.label()), Some(*c));
        }
        assert_eq!(Category::parse("MODULE_MAP"), Some(Category::ModuleMap));
        assert_eq!(Category::parse("nonsense"), None);
    }

    #[test]
    fn share_mode_parses_like_the_hive_one() {
        assert_eq!(ShareMode::parse("auto"), Some(ShareMode::Auto));
        assert_eq!(ShareMode::parse(" Manual "), Some(ShareMode::Manual));
        assert_eq!(ShareMode::parse("sometimes"), None);
        assert!(ShareMode::Auto.default_exposed());
        assert!(!ShareMode::Manual.default_exposed());
    }
}

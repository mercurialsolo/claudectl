// Inbound accept controls — install received skills/commands automatically or
// hold them in a pending queue for explicit approval.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptMode {
    /// Hold all incoming artifacts; user must `claudectl hive accept <id>`.
    Manual,
    /// Auto-install only when source peer is in the Confirmed trust tier.
    Trusted,
    /// Auto-install every received skill/command (hooks always require manual).
    All,
}

impl AcceptMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "trusted" | "auto-trusted" => Some(Self::Trusted),
            "all" | "auto" | "auto-all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Trusted => "trusted",
            Self::All => "all",
        }
    }
}

fn accept_mode_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("accept-mode")
}

/// Read the accept mode override file. Falls back to `default_mode` on absence
/// or invalid content.
pub fn read_mode(default_mode: AcceptMode) -> AcceptMode {
    fs::read_to_string(accept_mode_path())
        .ok()
        .and_then(|s| AcceptMode::parse(&s))
        .unwrap_or(default_mode)
}

pub fn write_mode(mode: AcceptMode) -> io::Result<()> {
    let path = accept_mode_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, mode.label())
}

// ────────────────────────────────────────────────────────────────────────────
// Installed tracker
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallRecord {
    pub installed_at: u64,
    pub mode: String,
    pub source_peer: String,
}

fn installed_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("installed.json")
}

#[derive(Default)]
pub struct InstalledTracker {
    entries: HashMap<String, InstallRecord>,
}

impl InstalledTracker {
    pub fn load() -> Self {
        Self::load_from(&installed_path())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        let entries = fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, InstallRecord>>(&s).ok())
            .unwrap_or_default();
        InstalledTracker { entries }
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_to(&installed_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.entries)
            .map_err(|e| io::Error::other(format!("serialize installed: {e}")))?;
        fs::write(path, json)
    }

    pub fn is_installed(&self, unit_id: &str) -> bool {
        self.entries.contains_key(unit_id)
    }

    pub fn record(&mut self, unit_id: &str, source_peer: &str, mode: AcceptMode) {
        self.entries.insert(
            unit_id.to_string(),
            InstallRecord {
                installed_at: super::epoch_secs(),
                mode: mode.label().to_string(),
                source_peer: source_peer.to_string(),
            },
        );
    }

    pub fn entries(&self) -> &HashMap<String, InstallRecord> {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_modes() {
        assert_eq!(AcceptMode::parse("manual"), Some(AcceptMode::Manual));
        assert_eq!(AcceptMode::parse("Trusted"), Some(AcceptMode::Trusted));
        assert_eq!(AcceptMode::parse("auto"), Some(AcceptMode::All));
        assert_eq!(AcceptMode::parse("auto-all"), Some(AcceptMode::All));
        assert_eq!(AcceptMode::parse("nope"), None);
    }

    #[test]
    fn tracker_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("installed.json");

        let mut tracker = InstalledTracker::default();
        tracker.record("ku_a", "peer-x", AcceptMode::Trusted);
        tracker.save_to(&path).unwrap();

        let loaded = InstalledTracker::load_from(&path);
        assert!(loaded.is_installed("ku_a"));
        assert!(!loaded.is_installed("ku_missing"));
        let entry = loaded.entries().get("ku_a").unwrap();
        assert_eq!(entry.source_peer, "peer-x");
        assert_eq!(entry.mode, "trusted");
    }
}

//! `~/.claudectl/onboarding.json` — the durable record of which init phases
//! ran, when, and against which claudectl version.
//!
//! The marker exists so:
//!
//! * `claudectl init` (no args) on an already-onboarded environment can skip
//!   the wizard and report status instead of re-prompting.
//! * `claudectl init --check` has a baseline to diff against environment
//!   detection (drift = recorded as installed but no longer detected).
//! * `claudectl init --remove` knows exactly which artifacts to clean up.
//!
//! Lives outside the SQLite stores (coord, bus, history) so it's
//! human-readable and trivially deletable when someone wants to factory-reset
//! their claudectl install.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Snapshot of a single phase's recorded outcome.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhaseRecord {
    /// Last status string we wrote — see `PhaseStatus::label` in `state.rs`.
    pub status: String,
    /// Free-form one-liner the phase wants to remember (a URL, a budget, a
    /// settings path, the role bindings count). Used to render `--check`.
    #[serde(default)]
    pub details: Option<String>,
    /// ISO timestamp when this phase was last applied.
    #[serde(default)]
    pub applied_at: Option<String>,
}

/// Full marker contents. New fields should be added with `#[serde(default)]`
/// so older marker files still parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OnboardingMarker {
    /// claudectl version that last completed onboarding.
    pub version: String,
    /// ISO timestamp when onboarding last completed.
    pub completed_at: String,
    /// Per-phase records keyed by phase id (`budget`, `brain`, …).
    #[serde(default)]
    pub phases: std::collections::BTreeMap<String, PhaseRecord>,
}

/// Default location: `$HOME/.claudectl/onboarding.json`. Used in production;
/// tests inject their own path.
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".claudectl").join("onboarding.json")
}

/// Load the marker, returning `None` when the file doesn't exist (i.e. the
/// user has never run `init`). Invalid JSON returns `Ok(None)` rather than
/// erroring so a corrupted marker never blocks a fresh init pass.
pub fn load(path: &Path) -> io::Result<Option<OnboardingMarker>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw).ok())
}

/// Save the marker atomically: write to a sibling temp file then rename, so a
/// crash mid-write never leaves a half-written marker.
pub fn save(path: &Path, marker: &OnboardingMarker) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(marker)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(&tmp, format!("{json}\n"))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Delete the marker. Idempotent — missing file is success.
pub fn clear(path: &Path) -> io::Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("onboarding.json");
        assert!(load(&p).unwrap().is_none());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nested").join("onboarding.json");
        let mut m = OnboardingMarker {
            version: "0.99.0".into(),
            completed_at: "2026-06-06T00:00:00Z".into(),
            ..Default::default()
        };
        m.phases.insert(
            "budget".into(),
            PhaseRecord {
                status: "installed".into(),
                details: Some("$50/wk".into()),
                applied_at: Some("2026-06-06T00:00:00Z".into()),
            },
        );
        save(&p, &m).unwrap();
        let loaded = load(&p).unwrap().expect("present");
        assert_eq!(loaded.version, "0.99.0");
        assert_eq!(loaded.phases["budget"].details.as_deref(), Some("$50/wk"));
    }

    #[test]
    fn load_returns_none_on_invalid_json() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("onboarding.json");
        fs::write(&p, "{not json").unwrap();
        // Corrupted marker should NOT error — we treat it as missing so
        // `init` can recover by overwriting it.
        assert!(load(&p).unwrap().is_none());
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("onboarding.json");
        clear(&p).unwrap(); // missing — OK
        fs::write(&p, "{}").unwrap();
        clear(&p).unwrap(); // present — removed
        assert!(!p.exists());
        clear(&p).unwrap(); // missing again — OK
    }
}

pub mod agents;
pub mod audit;
pub mod autopsy;
pub mod baseline;
pub mod briefing;
pub mod client;
pub mod context;
pub mod decisions;
pub mod detectors;
pub mod diff_digest;
pub mod engine;
pub mod evals;
pub mod garden;
pub mod health;
pub mod heuristic;
pub mod insights;
pub mod mailbox;
pub mod metrics;
pub mod outcomes;
pub mod pref_store;
pub mod preferences;
pub mod prompts;
pub mod retrieval;
pub mod review;
pub mod risk;
pub mod sequences;

use std::path::{Path, PathBuf};

/// `~/.claudectl/brain`, where the gate-mode file lives.
fn gate_mode_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claudectl").join("brain")
}

/// Path to the brain gate mode file (`~/.claudectl/brain/gate-mode`).
pub fn gate_mode_path() -> PathBuf {
    gate_mode_path_in(&gate_mode_dir())
}

/// The gate-mode file inside an explicitly given brain directory.
/// See `decisions::decisions_path_in` for why the `_in` form holds the body.
pub fn gate_mode_path_in(root: &Path) -> PathBuf {
    root.join("gate-mode")
}

/// Read the current brain gate mode from disk. Returns `"on"` if no file exists.
pub fn read_gate_mode() -> String {
    read_gate_mode_in(&gate_mode_dir())
}

/// `read_gate_mode` against an explicit brain directory.
pub fn read_gate_mode_in(root: &Path) -> String {
    std::fs::read_to_string(gate_mode_path_in(root))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "on".into())
}

/// Write the brain gate mode, creating the brain directory if needed.
pub fn write_gate_mode(label: &str) -> Result<(), String> {
    write_gate_mode_in(&gate_mode_dir(), label)
}

/// `write_gate_mode` against an explicit brain directory.
///
/// The writer lives here next to the reader so the two cannot disagree about
/// where the file goes; `LiveActions::set_gate_mode` used to build the path
/// itself.
pub fn write_gate_mode_in(root: &Path, label: &str) -> Result<(), String> {
    let path = gate_mode_path_in(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create gate-mode dir: {e}"))?;
    }
    std::fs::write(&path, label).map_err(|e| format!("write gate-mode: {e}"))
}

/// Path to the brain-lite mode file (`~/.claudectl/brain/heuristic-mode`).
pub fn heuristic_mode_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("brain")
        .join("heuristic-mode")
}

/// Read the brain-lite (heuristic) mode from disk. Absent file or unknown value
/// ⇒ the default (`Balanced`), so a missing/corrupt file never disables the
/// safety-relevant Critical-deny behavior.
pub fn read_heuristic_mode() -> heuristic::HeuristicMode {
    std::fs::read_to_string(heuristic_mode_path())
        .ok()
        .and_then(|s| heuristic::HeuristicMode::parse(&s))
        .unwrap_or_default()
}

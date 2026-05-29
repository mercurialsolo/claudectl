pub mod agents;
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

use std::path::PathBuf;

/// Path to the brain gate mode file (`~/.claudectl/brain/gate-mode`).
pub fn gate_mode_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("brain")
        .join("gate-mode")
}

/// Read the current brain gate mode from disk. Returns `"on"` if no file exists.
pub fn read_gate_mode() -> String {
    let path = gate_mode_path();
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "on".into())
}

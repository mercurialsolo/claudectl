//! `claudectl init` — opinionated onboarding wizard.
//!
//! Tracking issue: <https://github.com/mercurialsolo/claudectl/issues/257>.
//!
//! This module owns the single canonical first-run flow for getting a
//! claudectl install ready: weekly budget cap, local-LLM brain detection,
//! Claude Code hook install, agent-bus role binding, and curated skill
//! suggestions. The deferred `claudectl setup` wizard (AGENT_BUS.md §8) folds
//! in here as the "bus" phase rather than existing as a parallel command.
//!
//! Public surface:
//!
//! * [`run_wizard`] — interactive flow. The default `claudectl init`.
//! * [`run_non_interactive`] — same flow with pre-filled answers. For CI and
//!   dotfile automation.
//! * [`run_check`] — drift report comparing the recorded marker against
//!   current environment detection.
//! * [`run_remove`] — uninstall every claudectl-managed artifact.
//! * [`run_reset`] — clear the marker so the next `init` run prompts again.
//!
//! Module layout:
//!
//! * `hooks.rs` — the legacy `--init` / `--uninstall` hook writer (moved here
//!   unchanged; phases delegate to it for the plugin step).
//! * `marker.rs` — `~/.claudectl/onboarding.json` read/write.
//! * `prompt.rs` — minimal stdin/stdout prompt helpers.
//! * `state.rs` — environment detection (probes ollama, settings.json, bus
//!   roles, etc.).
//! * `phases.rs` — `Phase` trait + Budget/Brain/Plugin/Bus/Skills impls.

pub mod hooks;
pub mod marker;
pub mod phases;
pub mod prompt;
pub mod state;

use std::io;

use marker::{OnboardingMarker, PhaseRecord};
use phases::{Answers, Phase};
use state::PhaseStatus;

// Re-export the legacy entry points so existing `--init` / `--uninstall`
// flag dispatch in main.rs still compiles. The new subcommand path delegates
// through `run_wizard` and friends instead.
pub use hooks::{run_init, run_uninit};

/// Interactive wizard — walks every phase in `registry()` order, prompts,
/// applies, and updates the onboarding marker.
pub fn run_wizard() -> io::Result<()> {
    let registry = phases::registry();
    print_banner(&registry);

    let report = state::EnvironmentReport::detect();
    println!("Current state:");
    print!("{}", report.render_human());

    let total = registry.len();
    let mut new_records = std::collections::BTreeMap::new();
    let stamp = timestamp_now();

    for (idx, phase) in registry.iter().enumerate() {
        prompt::section_header(idx + 1, total, phase.label());
        let status = phase.run_interactive()?;
        print_outcome(phase.label(), &status);
        new_records.insert(
            phase.id().to_string(),
            phases::record_from_status(&status, &stamp),
        );
    }

    persist_marker(new_records, &stamp)?;
    println!();
    println!(
        "Onboarding complete. Re-run with `claudectl init --check` any time to inspect drift."
    );
    Ok(())
}

/// Non-interactive wizard. Same phases, no prompts. Skipped phases produce
/// a `PhaseStatus::Skipped` record so `--check` knows the difference between
/// "not configured because you don't want it" and "should be configured but
/// isn't yet."
pub fn run_non_interactive(answers: &Answers) -> io::Result<()> {
    let registry = phases::registry();
    let mut new_records = std::collections::BTreeMap::new();
    let stamp = timestamp_now();
    for phase in &registry {
        let status = phase.run_non_interactive(answers)?;
        println!(
            "{label}: {status_label}{detail}",
            label = phase.label(),
            status_label = status.label(),
            detail = status
                .details()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default(),
        );
        new_records.insert(
            phase.id().to_string(),
            phases::record_from_status(&status, &stamp),
        );
    }
    persist_marker(new_records, &stamp)?;
    Ok(())
}

/// Drift report: detect each phase's current state and diff against the
/// marker. Exits with code 1 (via returned `io::Result`) when drift is
/// detected so CI can gate on `init --check`.
pub fn run_check() -> io::Result<()> {
    let registry = phases::registry();
    let recorded = marker::load(&marker::default_path())?;

    if recorded.is_none() {
        println!("claudectl has not been onboarded — run `claudectl init` to begin.");
        return Err(io::Error::other("not onboarded"));
    }
    let recorded = recorded.unwrap();

    println!("claudectl init --check");
    println!(
        "  recorded version : {}",
        if recorded.version.is_empty() {
            "(unknown)"
        } else {
            &recorded.version
        }
    );
    println!("  last completed   : {}", recorded.completed_at);
    println!();
    println!("Phase status (current → recorded):");

    let mut drift_count = 0;
    for phase in &registry {
        let current = phase.detect();
        let recorded_status = recorded
            .phases
            .get(phase.id())
            .map(|r| r.status.as_str())
            .unwrap_or("(no record)");

        let drifted = is_drift(&current, recorded_status);
        let marker_char = if drifted { "⚠" } else { "✓" };
        let current_detail = current.details().unwrap_or("");
        println!(
            "  {marker} {label:<10} {cur:<14} ← {rec}{detail}",
            marker = marker_char,
            label = phase.id(),
            cur = current.label(),
            rec = recorded_status,
            detail = if current_detail.is_empty() {
                String::new()
            } else {
                format!("   [{current_detail}]")
            }
        );
        if drifted {
            drift_count += 1;
        }
    }

    if drift_count > 0 {
        println!();
        println!("⚠  {drift_count} phase(s) have drifted from the recorded onboarding.");
        println!("   Run `claudectl init` to re-apply, or `claudectl init --reset` to start over.");
        return Err(io::Error::other(format!("{drift_count} phase(s) drifted")));
    }
    println!();
    println!("✓ all phases match the recorded state.");
    Ok(())
}

/// Remove every claudectl-managed artifact. Phases that own user state (the
/// bus DB, the config file's `budget` line) decline to delete it — we don't
/// erase a user's setup, only artifacts claudectl actively manages.
pub fn run_remove() -> io::Result<()> {
    let registry = phases::registry();
    let mut errors = Vec::new();
    for phase in &registry {
        if let Err(e) = phase.remove() {
            errors.push(format!("{}: {e}", phase.id()));
        } else {
            println!("  removed: {}", phase.label());
        }
    }
    marker::clear(&marker::default_path())?;
    println!("Cleared onboarding marker.");
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "remove errors: {}",
            errors.join("; ")
        )))
    }
}

/// Clear the marker so the next `init` run prompts again. Does NOT touch any
/// installed artifacts.
pub fn run_reset() -> io::Result<()> {
    marker::clear(&marker::default_path())?;
    println!("Cleared onboarding marker — `claudectl init` will start from scratch next run.");
    Ok(())
}

// ---------------- internal helpers ------------------------------------------

fn print_banner(registry: &[Box<dyn Phase>]) {
    println!();
    println!(
        "claudectl init — opinionated onboarding ({} phases)",
        registry.len()
    );
    println!("══════════════════════════════════════════════════════════════");
    println!();
}

fn print_outcome(label: &str, status: &PhaseStatus) {
    match status {
        PhaseStatus::Installed { details } => prompt::phase_outcome(label, details),
        PhaseStatus::Skipped => prompt::phase_skipped(label, "user declined"),
        PhaseStatus::NotInstalled => prompt::phase_skipped(label, "not configured"),
        PhaseStatus::Drift { reason } => prompt::phase_skipped(label, reason),
    }
}

fn persist_marker(
    phase_records: std::collections::BTreeMap<String, PhaseRecord>,
    stamp: &str,
) -> io::Result<()> {
    let marker_value = OnboardingMarker {
        version: env!("CARGO_PKG_VERSION").to_string(),
        completed_at: stamp.to_string(),
        phases: phase_records,
    };
    marker::save(&marker::default_path(), &marker_value)
}

fn timestamp_now() -> String {
    crate::logger::timestamp_now()
}

/// Decide whether the current state diverges from what the marker recorded.
///
/// We treat "not_installed" and "skipped" as equivalent for drift purposes —
/// both mean "phase is not configured." Drift triggers only when:
///
/// * recorded "installed" but current state is missing it (artifact removed
///   out-of-band), or
/// * recorded "skipped"/"not_installed" but the current state now detects an
///   install (an artifact appeared since onboarding — could be intentional or
///   could be a stale install the user wants to clean up).
fn is_drift(current: &PhaseStatus, recorded_label: &str) -> bool {
    let current_label = current.label();
    let cur_configured = matches!(current_label, "installed");
    let rec_configured = matches!(recorded_label, "installed");
    cur_configured != rec_configured
}

#[cfg(test)]
mod drift_tests {
    use super::*;

    fn installed() -> PhaseStatus {
        PhaseStatus::Installed {
            details: "x".into(),
        }
    }

    #[test]
    fn not_installed_and_skipped_treated_as_equivalent() {
        assert!(!is_drift(&PhaseStatus::NotInstalled, "skipped"));
        assert!(!is_drift(&PhaseStatus::Skipped, "not_installed"));
        assert!(!is_drift(&PhaseStatus::NotInstalled, "not_installed"));
        assert!(!is_drift(&PhaseStatus::Skipped, "skipped"));
    }

    #[test]
    fn matched_installed_is_not_drift() {
        assert!(!is_drift(&installed(), "installed"));
    }

    #[test]
    fn installed_then_missing_is_drift() {
        assert!(is_drift(&PhaseStatus::NotInstalled, "installed"));
        assert!(is_drift(&PhaseStatus::Skipped, "installed"));
    }

    #[test]
    fn unexpected_install_is_drift() {
        assert!(is_drift(&installed(), "skipped"));
        assert!(is_drift(&installed(), "not_installed"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_interactive_records_marker_for_every_phase() {
        // Drive the wizard in non-interactive mode with all-skip answers and
        // assert the marker captures one record per phase.
        let registry = phases::registry();
        let answers = Answers {
            skip_budget: true,
            skip_brain: true,
            install_plugin: Some(false),
            skip_bus: true,
            skip_skills: true,
            ..Answers::default()
        };

        let mut records = std::collections::BTreeMap::new();
        let stamp = "2026-06-06T00:00:00Z";
        for phase in &registry {
            let status = phase.run_non_interactive(&answers).unwrap();
            records.insert(
                phase.id().to_string(),
                phases::record_from_status(&status, stamp),
            );
        }
        // Five entries, one per phase, all skipped.
        assert_eq!(records.len(), 5);
        for record in records.values() {
            assert_eq!(record.status, "skipped");
        }
    }
}

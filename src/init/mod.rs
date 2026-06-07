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

/// Hard uninstall: `--remove` plus delete `~/.claudectl/` and
/// `~/.config/claudectl/config.toml`. Used to start from a truly clean
/// slate (e.g. for reinstall testing or recovering from corrupted state).
///
/// User confirms before any deletion unless `assume_yes` is set. Each
/// path that's missing is silently skipped so re-running `--purge` after
/// a successful one is a no-op rather than an error.
pub fn run_purge(assume_yes: bool) -> io::Result<()> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let claudectl_dir = home.as_ref().map(|h| h.join(".claudectl"));
    let config_path = crate::config::Config::global_path();

    println!("This will delete:");
    println!("  • Claude Code hooks claudectl installed (`~/.claude/settings.json` entries)");
    if let Some(dir) = claudectl_dir.as_ref() {
        println!(
            "  • {} (bus DB, brain decisions, hive, relay, coord)",
            dir.display()
        );
    }
    if let Some(cfg) = config_path.as_ref() {
        println!("  • {} (claudectl config file)", cfg.display());
    }
    println!();
    println!("User-edited files outside these paths are preserved. To remove only");
    println!("the hooks and onboarding marker (keep user data), use `init --remove`.");
    println!();

    if !assume_yes && !prompt::yes_no("Proceed with purge?", false)? {
        println!("Aborted.");
        return Ok(());
    }

    // First, the soft uninstall — strips hook entries and clears the marker.
    // We run this with `?` only after the destructive deletions so a failure
    // here (e.g. settings.json edit conflict) doesn't abort the directory
    // wipes that follow.
    let remove_errors = match run_remove_silent() {
        Ok(()) => Vec::new(),
        Err(e) => vec![format!("hook/marker removal: {e}")],
    };

    let mut errors = remove_errors;
    if let Some(dir) = claudectl_dir.as_ref() {
        if let Err(e) = remove_dir_if_present(dir) {
            errors.push(format!("{}: {e}", dir.display()));
        } else {
            println!("  removed: {}", dir.display());
        }
    }
    if let Some(cfg) = config_path.as_ref() {
        if let Err(e) = remove_file_if_present(cfg) {
            errors.push(format!("{}: {e}", cfg.display()));
        } else {
            println!("  removed: {}", cfg.display());
        }
        // Also try the parent ~/.config/claudectl/ dir — only succeeds if
        // it's now empty (we don't recursively delete the parent because
        // it could contain user-authored files we don't know about).
        if let Some(parent) = cfg.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    if errors.is_empty() {
        println!();
        println!("Purge complete. `claudectl init` will start fresh.");
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "purge errors: {}",
            errors.join("; ")
        )))
    }
}

/// `run_remove` without printing the per-phase progress lines — used by
/// `run_purge` so its UI doesn't have duplicated "removed: X" rows.
fn run_remove_silent() -> io::Result<()> {
    let registry = phases::registry();
    let mut errors = Vec::new();
    for phase in &registry {
        if let Err(e) = phase.remove() {
            errors.push(format!("{}: {e}", phase.id()));
        }
    }
    marker::clear(&marker::default_path())?;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "remove errors: {}",
            errors.join("; ")
        )))
    }
}

/// `remove_dir_all` that treats a missing directory as success — same
/// semantics as `rm -rf` without erroring on non-existence.
fn remove_dir_if_present(path: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_file_if_present(path: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
    fn remove_helpers_treat_missing_paths_as_success() {
        // `remove_dir_if_present` / `remove_file_if_present` are the
        // building blocks of --purge. Idempotency matters: re-running
        // --purge after a successful one must not error.
        let tmp = tempfile::tempdir().unwrap();
        let missing_dir = tmp.path().join("nope");
        let missing_file = tmp.path().join("nope.txt");
        assert!(remove_dir_if_present(&missing_dir).is_ok());
        assert!(remove_file_if_present(&missing_file).is_ok());
    }

    #[test]
    fn remove_dir_if_present_wipes_existing_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("claudectl");
        std::fs::create_dir_all(target.join("brain")).unwrap();
        std::fs::create_dir_all(target.join("bus")).unwrap();
        std::fs::write(target.join("brain").join("d.jsonl"), "{}").unwrap();
        std::fs::write(target.join("bus").join("bus.db"), "x").unwrap();

        assert!(target.exists());
        remove_dir_if_present(&target).unwrap();
        assert!(
            !target.exists(),
            "expected tree to be gone, but {} still exists",
            target.display()
        );
    }

    #[test]
    fn remove_file_if_present_removes_just_that_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        let sibling = tmp.path().join("other.toml");
        std::fs::write(&cfg, "budget = 25").unwrap();
        std::fs::write(&sibling, "keep me").unwrap();

        remove_file_if_present(&cfg).unwrap();
        assert!(!cfg.exists(), "config.toml should be gone");
        assert!(sibling.exists(), "sibling untouched");
    }

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

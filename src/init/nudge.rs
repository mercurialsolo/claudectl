//! First-run activation nudge (#322).
//!
//! After `brew install`/`cargo install`, running `claudectl` was silent about
//! what to do next — the marquee coordination + brain features stay dormant
//! until `init` wires up Claude Code hooks and the plugin. This module turns
//! that silence into a one-line, actionable pointer.
//!
//! The decision (`activation_nudge`) is pure so it's unit-tested without
//! touching `$HOME`; `gather` does the filesystem reads and `nudge` combines
//! them for callers.

use std::path::{Path, PathBuf};

/// Observable activation signals, decoupled from IO so the message logic is
/// testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivationState {
    /// The user has completed `claudectl init` at least once (marker present).
    pub onboarded: bool,
    /// Claude Code's `settings.json` references `claudectl` (hooks wired up).
    pub hooks_installed: bool,
}

/// The one-line nudge for the current activation state, or `None` when the
/// environment is fully wired up and nothing needs saying.
///
/// Ordered by severity: a never-onboarded user needs the full `init`; an
/// onboarded user whose hooks went missing (reinstall, edited settings) needs
/// the narrower re-run.
pub fn activation_nudge(state: &ActivationState) -> Option<String> {
    if !state.onboarded {
        return Some(
            "Not set up yet — run `claudectl init` to wire up hooks, the brain, and \
             coordination (~1 min)."
                .to_string(),
        );
    }
    if !state.hooks_installed {
        return Some(
            "Claude Code hooks aren't installed — run `claudectl init`, or `claudectl doctor` \
             to see what's missing."
                .to_string(),
        );
    }
    None
}

/// Whether `~/.claude/settings.json` wires up claudectl hooks. Mirrors the
/// `doctor` hooks check; a missing/unreadable file counts as not installed.
fn hooks_installed_at(settings_path: &Path) -> bool {
    std::fs::read_to_string(settings_path)
        .map(|s| s.contains("claudectl"))
        .unwrap_or(false)
}

fn claude_settings_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join(".claude").join("settings.json")
}

/// Read the on-disk activation signals.
pub fn gather() -> ActivationState {
    let onboarded = super::marker::load(&super::marker::default_path())
        .ok()
        .flatten()
        .is_some();
    ActivationState {
        onboarded,
        hooks_installed: hooks_installed_at(&claude_settings_path()),
    }
}

/// The activation nudge for the real environment, or `None` when fully set up.
pub fn nudge() -> Option<String> {
    activation_nudge(&gather())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_onboarded_points_to_init() {
        let msg = activation_nudge(&ActivationState {
            onboarded: false,
            hooks_installed: false,
        })
        .expect("expected a nudge");
        assert!(msg.contains("claudectl init"));
    }

    #[test]
    fn onboarded_but_hooks_missing_points_to_reinstall() {
        let msg = activation_nudge(&ActivationState {
            onboarded: true,
            hooks_installed: false,
        })
        .expect("expected a nudge");
        assert!(msg.contains("hooks aren't installed"));
        assert!(msg.contains("doctor"));
    }

    #[test]
    fn fully_wired_up_says_nothing() {
        assert_eq!(
            activation_nudge(&ActivationState {
                onboarded: true,
                hooks_installed: true,
            }),
            None
        );
    }

    #[test]
    fn onboarding_dominates_hooks_message() {
        // A never-onboarded user gets the full init line even though hooks are
        // also missing — the broader action supersedes the narrower one.
        let msg = activation_nudge(&ActivationState {
            onboarded: false,
            hooks_installed: false,
        })
        .unwrap();
        assert!(msg.contains("Not set up yet"));
    }

    #[test]
    fn hooks_detected_from_settings_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(!hooks_installed_at(&path)); // missing file
        std::fs::write(
            &path,
            r#"{"hooks":{"PreToolUse":[{"command":"claudectl ingest"}]}}"#,
        )
        .unwrap();
        assert!(hooks_installed_at(&path));
        std::fs::write(&path, r#"{"hooks":{}}"#).unwrap();
        assert!(!hooks_installed_at(&path));
    }
}

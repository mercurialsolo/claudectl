//! Environment detection for the `init` wizard.
//!
//! Each phase has a corresponding probe here that answers "what's the current
//! state?" with no side effects. The wizard uses these to decide what to ask
//! and what to skip; `init --check` uses them to diff against the recorded
//! marker; the install/remove paths use them to be idempotent.
//!
//! All probes are tiny and synchronous — file checks, `curl --max-time 1`,
//! reading a TOML — so the whole detection pass takes well under a second
//! even on a cold machine.

use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;

use super::hooks;

/// The shape every phase's probe returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseStatus {
    /// Phase has not been configured. The wizard will offer to set it up.
    NotInstalled,
    /// Phase is currently configured. `details` is one human line ("ollama at
    /// http://localhost:11434", "$50/wk", "2 roles bound") rendered in
    /// `--check`.
    Installed { details: String },
    /// Phase was recorded as installed in the marker but no longer detected
    /// in the environment. The wizard treats this as a re-prompt case.
    ///
    /// Currently no probe synthesizes this directly; `init --check` derives
    /// it by comparing detection against the recorded marker. Kept on the
    /// enum so phases can return it once we add drift-aware detection.
    #[allow(dead_code)]
    Drift { reason: String },
    /// User opted out of this phase last time and we should respect that
    /// until `--reset` is run.
    Skipped,
}

impl PhaseStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::Installed { .. } => "installed",
            Self::Drift { .. } => "drift",
            Self::Skipped => "skipped",
        }
    }

    pub fn details(&self) -> Option<&str> {
        match self {
            Self::Installed { details } => Some(details.as_str()),
            Self::Drift { reason } => Some(reason.as_str()),
            _ => None,
        }
    }
}

// ---------------- Budget ----------------------------------------------------

/// Budget is "installed" when `config.budget` is set in the layered config.
pub fn detect_budget() -> PhaseStatus {
    let cfg = Config::load();
    match cfg.budget {
        Some(b) if b > 0.0 => PhaseStatus::Installed {
            details: format!("${b:.0}/week cap"),
        },
        _ => PhaseStatus::NotInstalled,
    }
}

// ---------------- Brain (local LLM) -----------------------------------------

/// Known local-LLM endpoints worth probing. First hit wins.
const BRAIN_PROBES: &[(&str, &str, &str)] = &[
    ("ollama", "http://localhost:11434", "/api/tags"),
    ("llama.cpp", "http://localhost:8080", "/v1/models"),
    ("lm-studio", "http://localhost:1234", "/v1/models"),
    ("vllm", "http://localhost:8000", "/v1/models"),
];

/// Probe each candidate endpoint with a 1-second `curl`. We do not require an
/// LLM model to be loaded — only that the endpoint answers — because the user
/// might be about to pull one.
pub fn detect_brain() -> PhaseStatus {
    for (name, base, path) in BRAIN_PROBES {
        if probe_http(&format!("{base}{path}")) {
            return PhaseStatus::Installed {
                details: format!("{name} at {base}"),
            };
        }
    }
    PhaseStatus::NotInstalled
}

fn probe_http(url: &str) -> bool {
    Command::new("curl")
        .args(["-s", "-o", "/dev/null", "--max-time", "1", url])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------- Plugin (Claude Code hooks) --------------------------------

/// Plugin is "installed" when our hooks are present in
/// `~/.claude/settings.json` (global scope). Reuses the existing detection
/// logic from `hooks.rs` so it stays consistent with what `--init` writes.
pub fn detect_plugin() -> PhaseStatus {
    let path = hooks::user_settings_path();
    match hooks::settings_contain_claudectl_hooks(&path) {
        Some(true) => PhaseStatus::Installed {
            details: format!("hooks in {}", path.display()),
        },
        Some(false) => PhaseStatus::NotInstalled,
        None => PhaseStatus::NotInstalled,
    }
}

// ---------------- Bus (agent bus role bindings) -----------------------------

/// Detect a usable bus install. Returns NotInstalled when the `bus` feature
/// is not compiled in (or no roles bound). On a `--features bus` build,
/// reports the number of bound roles.
pub fn detect_bus() -> PhaseStatus {
    #[cfg(feature = "bus")]
    {
        match crate::bus::store::open().and_then(|c| crate::bus::store::list_roles(&c)) {
            Ok(roles) if !roles.is_empty() => PhaseStatus::Installed {
                details: format!("{} role(s) bound", roles.len()),
            },
            _ => PhaseStatus::NotInstalled,
        }
    }
    #[cfg(not(feature = "bus"))]
    {
        PhaseStatus::Skipped
    }
}

// ---------------- Skills (curated list) -------------------------------------

/// Skills installation is owned by Claude Code itself (`/plugin install`),
/// not by claudectl. We treat the phase as "installed" only when the user
/// recorded acknowledging the suggestions, via the marker — so detection
/// here always returns NotInstalled and the wizard relies on the marker for
/// idempotency.
pub fn detect_skills() -> PhaseStatus {
    PhaseStatus::NotInstalled
}

// ---------------- Aggregate report -----------------------------------------

/// Full snapshot used by `init --check` and the wizard's opening summary.
#[derive(Debug, Clone)]
pub struct EnvironmentReport {
    pub budget: PhaseStatus,
    pub brain: PhaseStatus,
    pub plugin: PhaseStatus,
    pub bus: PhaseStatus,
    pub skills: PhaseStatus,
}

impl EnvironmentReport {
    pub fn detect() -> Self {
        Self {
            budget: detect_budget(),
            brain: detect_brain(),
            plugin: detect_plugin(),
            bus: detect_bus(),
            skills: detect_skills(),
        }
    }

    pub fn render_human(&self) -> String {
        let mut out = String::new();
        for (label, status) in self.entries() {
            let detail = status.details().unwrap_or("");
            let marker = match status {
                PhaseStatus::Installed { .. } => "✓",
                PhaseStatus::NotInstalled => "·",
                PhaseStatus::Drift { .. } => "⚠",
                PhaseStatus::Skipped => "—",
            };
            out.push_str(&format!("  {marker} {label:<10} {detail}\n"));
        }
        out
    }

    pub fn entries(&self) -> [(&'static str, &PhaseStatus); 5] {
        [
            ("budget", &self.budget),
            ("brain", &self.brain),
            ("plugin", &self.plugin),
            ("bus", &self.bus),
            ("skills", &self.skills),
        ]
    }
}

#[allow(dead_code)]
pub(crate) fn _claude_config_dir_for_tests() -> PathBuf {
    hooks::user_settings_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_status_labels_are_stable() {
        assert_eq!(PhaseStatus::NotInstalled.label(), "not_installed");
        assert_eq!(
            PhaseStatus::Installed {
                details: "x".into()
            }
            .label(),
            "installed"
        );
        assert_eq!(PhaseStatus::Drift { reason: "y".into() }.label(), "drift");
        assert_eq!(PhaseStatus::Skipped.label(), "skipped");
    }

    #[test]
    fn environment_report_renders_five_lines() {
        let r = EnvironmentReport::detect();
        let rendered = r.render_human();
        assert_eq!(rendered.lines().count(), 5);
    }
}

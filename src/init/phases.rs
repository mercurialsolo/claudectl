//! Onboarding phases. Each phase is a self-contained step the wizard walks:
//! detect current state → ask the user → apply if accepted → record outcome.
//!
//! Phases share one `Phase` trait so the wizard, `init --check`, and
//! `init --remove` all walk the same registry without per-phase branching.

use std::io;
use std::path::{Path, PathBuf};

use super::hooks;
use super::marker::PhaseRecord;
use super::prompt;
use super::state::{self, PhaseStatus};

/// Pre-filled answers for the non-interactive path. The wizard either reads
/// these or asks the user; both forms produce the same outcome.
///
/// Fields cross feature gates (`bus_role` / `bus_cwd` are only read on
/// `--features bus` builds), so the struct allows dead-code in non-bus
/// configurations rather than per-field cfg-gating.
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct Answers {
    pub budget_weekly_usd: Option<f64>,
    pub skip_budget: bool,

    pub brain_url: Option<String>,
    pub skip_brain: bool,

    pub install_plugin: Option<bool>,

    pub bus_role: Option<String>,
    pub bus_cwd: Option<PathBuf>,
    pub skip_bus: bool,

    pub skip_skills: bool,
}

/// Single uniform shape across all phases.
pub trait Phase {
    /// Stable identifier — keys `onboarding.json`'s `phases` map. Never
    /// rename without a migration.
    fn id(&self) -> &'static str;

    /// One-line label used in section headers and `--check` output.
    fn label(&self) -> &'static str;

    /// What's there now?
    fn detect(&self) -> PhaseStatus;

    /// Interactive run. Calls into `prompt::*`. Implementations should:
    /// 1. ask any phase-specific questions (with sensible defaults),
    /// 2. perform the install/configure work,
    /// 3. return the resulting `PhaseStatus`.
    fn run_interactive(&self) -> io::Result<PhaseStatus>;

    /// Non-interactive equivalent: take pre-filled answers, do the same work,
    /// return the same status. No prompting.
    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus>;

    /// Tear down whatever this phase installed. Idempotent.
    fn remove(&self) -> io::Result<()>;
}

/// Convert a status into a marker record. Callers stamp `applied_at`
/// themselves so the timestamp ties to the wizard's clock.
pub fn record_from_status(status: &PhaseStatus, applied_at: &str) -> PhaseRecord {
    PhaseRecord {
        status: status.label().to_string(),
        details: status.details().map(String::from),
        applied_at: Some(applied_at.to_string()),
    }
}

/// The full ordered registry the wizard walks. The order matters: budget
/// first because it's the most important guardrail, plugin before bus because
/// the bus's MCP server needs the plugin path to be writeable, skills last
/// because they're optional.
pub fn registry() -> Vec<Box<dyn Phase>> {
    vec![
        Box::new(BudgetPhase),
        Box::new(BrainPhase),
        Box::new(PluginPhase),
        Box::new(BusPhase),
        Box::new(SkillsPhase),
    ]
}

// ===================== Budget ===========================================

pub struct BudgetPhase;

impl Phase for BudgetPhase {
    fn id(&self) -> &'static str {
        "budget"
    }
    fn label(&self) -> &'static str {
        "Weekly budget cap"
    }

    fn detect(&self) -> PhaseStatus {
        state::detect_budget()
    }

    fn run_interactive(&self) -> io::Result<PhaseStatus> {
        println!("Set a weekly per-session budget so a runaway agent can't burn unlimited cost.");
        println!("Claude Code alerts at 80% and (optionally) kills the session at 100%.");
        if !prompt::yes_no("Set a weekly budget cap?", true)? {
            return Ok(PhaseStatus::Skipped);
        }
        let amount = prompt::number_or_default("  Weekly budget (USD)", 50.0)?;
        write_budget_to_config(amount)?;
        Ok(PhaseStatus::Installed {
            details: format!("${amount:.0}/week cap"),
        })
    }

    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus> {
        if answers.skip_budget {
            return Ok(PhaseStatus::Skipped);
        }
        let Some(amount) = answers.budget_weekly_usd else {
            return Ok(PhaseStatus::NotInstalled);
        };
        write_budget_to_config(amount)?;
        Ok(PhaseStatus::Installed {
            details: format!("${amount:.0}/week cap"),
        })
    }

    fn remove(&self) -> io::Result<()> {
        // We don't strip the budget from a user-edited config — the value
        // is theirs, not ours. Drop the marker record instead (handled by
        // the orchestrator). `--reset` re-prompts.
        Ok(())
    }
}

fn write_budget_to_config(weekly_usd: f64) -> io::Result<()> {
    // Write to ~/.config/claudectl/config.toml as a top-level `budget` field.
    // We merge, don't overwrite, so other config keys are preserved.
    let Some(cfg_path) = crate::config::Config::global_path() else {
        // No HOME — non-interactive CI env. Skip silently rather than fail.
        return Ok(());
    };
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let updated = upsert_toml_top_level_number(&raw, "budget", weekly_usd);
    std::fs::write(&cfg_path, updated)?;
    Ok(())
}

/// Tiny TOML edit: replace or append a `key = N` line at the top level.
/// Pragmatic — we own the file format. A full TOML round-trip would need a
/// dep; this preserves comments adequately for the common case.
fn upsert_toml_top_level_number(raw: &str, key: &str, value: f64) -> String {
    let prefix = format!("{key} ");
    let prefix_eq = format!("{key}=");
    let mut found = false;
    let mut out = String::with_capacity(raw.len() + 32);
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if !found
            && (trimmed.starts_with(&prefix) || trimmed.starts_with(&prefix_eq))
            && !trimmed.starts_with('#')
        {
            out.push_str(&format!("{key} = {value}\n"));
            found = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{key} = {value}\n"));
    }
    out
}

// ===================== Brain (local LLM) ================================

pub struct BrainPhase;

impl Phase for BrainPhase {
    fn id(&self) -> &'static str {
        "brain"
    }
    fn label(&self) -> &'static str {
        "Local-LLM brain auto-pilot"
    }

    fn detect(&self) -> PhaseStatus {
        state::detect_brain()
    }

    fn run_interactive(&self) -> io::Result<PhaseStatus> {
        println!(
            "The brain learns your preferences and can approve/deny tool calls automatically."
        );
        println!("Requires a local LLM endpoint (ollama / llama.cpp / LM Studio / vLLM).");

        let detected = state::detect_brain();
        if let PhaseStatus::Installed { details } = &detected {
            println!("  Detected: {details}");
            if prompt::yes_no("Use this endpoint?", true)? {
                return Ok(detected);
            }
        } else {
            // #324 — print a concrete install hint when no endpoint is
            // reachable, instead of silently moving on. Most users hitting
            // this won't know ollama exists.
            print_ollama_install_hint();
        }

        if !prompt::yes_no("Configure a custom endpoint?", false)? {
            return Ok(PhaseStatus::Skipped);
        }
        let url = prompt::line_or_default("  Endpoint URL", Some("http://localhost:11434"))?
            .unwrap_or_default();
        Ok(PhaseStatus::Installed {
            details: format!("endpoint at {url}"),
        })
    }

    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus> {
        if answers.skip_brain {
            return Ok(PhaseStatus::Skipped);
        }
        if let Some(url) = &answers.brain_url {
            return Ok(PhaseStatus::Installed {
                details: format!("endpoint at {url}"),
            });
        }
        let status = state::detect_brain();
        // #324 — even non-interactive mode should surface the install hint
        // (printed once, doesn't change the recorded status). CI / dotfile
        // users skim the output; they shouldn't have to guess why brain
        // recorded `not_installed`.
        if !matches!(status, PhaseStatus::Installed { .. }) {
            print_ollama_install_hint();
        }
        Ok(status)
    }

    fn remove(&self) -> io::Result<()> {
        // We don't shut down the user's ollama install. Marker record drop is
        // handled by the orchestrator.
        Ok(())
    }
}

/// Three-line install hint shown when the Brain phase can't reach any
/// local-LLM endpoint. Mirrors `docs/quickstart.md` "Optional: add the
/// local LLM brain" so the wizard and the docs say the same thing.
fn print_ollama_install_hint() {
    println!("  No local-LLM endpoint detected on the common ports.");
    println!("  To enable the brain, install ollama and a small model:");
    println!("    brew install ollama && ollama serve &");
    println!("    ollama pull gemma4:e4b");
    println!("  Then re-run `claudectl init` to wire it up.");
}

// ===================== Plugin (Claude Code hooks) =======================

pub struct PluginPhase;

impl Phase for PluginPhase {
    fn id(&self) -> &'static str {
        "plugin"
    }
    fn label(&self) -> &'static str {
        "Claude Code hooks (supervisor plugin)"
    }

    fn detect(&self) -> PhaseStatus {
        state::detect_plugin()
    }

    fn run_interactive(&self) -> io::Result<PhaseStatus> {
        println!("Install claudectl's hooks into ~/.claude/settings.json so Claude Code");
        println!("notifies claudectl on tool use and session end. Existing hooks are preserved.");
        if !prompt::yes_no("Install hooks?", true)? {
            return Ok(PhaseStatus::Skipped);
        }
        install_plugin_hooks()?;
        Ok(self.detect())
    }

    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus> {
        match answers.install_plugin {
            Some(true) => {
                install_plugin_hooks()?;
                Ok(self.detect())
            }
            Some(false) => Ok(PhaseStatus::Skipped),
            None => {
                // Unspecified non-interactive = install (the wizard's default).
                install_plugin_hooks()?;
                Ok(self.detect())
            }
        }
    }

    fn remove(&self) -> io::Result<()> {
        // Run the existing uninit against the global settings.
        hooks::run_uninit(false)
    }
}

fn install_plugin_hooks() -> io::Result<()> {
    hooks::run_init(false, false)
}

// ===================== Bus (agent-bus role binding) =====================

pub struct BusPhase;

impl Phase for BusPhase {
    fn id(&self) -> &'static str {
        "bus"
    }
    fn label(&self) -> &'static str {
        "Agent bus role binding"
    }

    fn detect(&self) -> PhaseStatus {
        state::detect_bus()
    }

    fn run_interactive(&self) -> io::Result<PhaseStatus> {
        #[cfg(not(feature = "bus"))]
        {
            println!("(bus feature not compiled in — rebuild with `--features bus` to enable)");
            Ok(PhaseStatus::Skipped)
        }
        #[cfg(feature = "bus")]
        {
            println!("Bind a role to this cwd so other Claude Code sessions can address you.");
            if !prompt::yes_no("Bind a role for the current directory?", true)? {
                return Ok(PhaseStatus::Skipped);
            }
            let default_name = derive_role_from_cwd();
            let role = prompt::line_or_default("  Role name", Some(&default_name))?
                .unwrap_or(default_name);
            let cwd = std::env::current_dir()?;
            bind_bus_role(&role, &cwd)?;
            Ok(PhaseStatus::Installed {
                details: format!("role `{role}` -> {}", cwd.display()),
            })
        }
    }

    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus> {
        if answers.skip_bus {
            return Ok(PhaseStatus::Skipped);
        }
        #[cfg(not(feature = "bus"))]
        {
            let _ = answers;
            Ok(PhaseStatus::Skipped)
        }
        #[cfg(feature = "bus")]
        {
            let cwd = answers
                .bus_cwd
                .clone()
                .map(Ok)
                .unwrap_or_else(std::env::current_dir)?;
            let role = answers
                .bus_role
                .clone()
                .unwrap_or_else(|| derive_role_from_cwd_at(&cwd));
            bind_bus_role(&role, &cwd)?;
            Ok(PhaseStatus::Installed {
                details: format!("role `{role}` -> {}", cwd.display()),
            })
        }
    }

    fn remove(&self) -> io::Result<()> {
        // Roles are persistent identity, not artifacts — leaving them in the
        // bus DB is intentional so re-running `init` reconnects rather than
        // orphans state. Marker drop is handled by the orchestrator.
        Ok(())
    }
}

#[cfg(feature = "bus")]
fn bind_bus_role(role: &str, cwd: &Path) -> io::Result<()> {
    let conn = crate::bus::store::open().map_err(io::Error::other)?;
    crate::bus::store::upsert_role(&conn, role, &cwd.to_string_lossy(), None, None)
        .map_err(io::Error::other)?;
    Ok(())
}

#[cfg(not(feature = "bus"))]
#[allow(dead_code)]
fn bind_bus_role(_role: &str, _cwd: &Path) -> io::Result<()> {
    Ok(())
}

#[allow(dead_code)]
fn derive_role_from_cwd() -> String {
    std::env::current_dir()
        .ok()
        .as_deref()
        .map(derive_role_from_cwd_at)
        .unwrap_or_else(|| "default".to_string())
}

#[allow(dead_code)]
fn derive_role_from_cwd_at(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .map(|s| s.trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

// ===================== Skills ============================================

/// Suggestions only — we don't shell into Claude Code's plugin installer.
const SUGGESTED_SKILLS: &[(&str, &str)] = &[
    ("humanizer", "rewrite AI-shaped prose into natural language"),
    ("update-config", "edit settings.json safely"),
    ("verify", "drive the app to confirm a change actually works"),
];

pub struct SkillsPhase;

impl Phase for SkillsPhase {
    fn id(&self) -> &'static str {
        "skills"
    }
    fn label(&self) -> &'static str {
        "Curated skill suggestions"
    }

    fn detect(&self) -> PhaseStatus {
        state::detect_skills()
    }

    fn run_interactive(&self) -> io::Result<PhaseStatus> {
        if !prompt::yes_no("Print suggested Claude Code skills?", false)? {
            return Ok(PhaseStatus::Skipped);
        }
        println!();
        for (name, blurb) in SUGGESTED_SKILLS {
            println!("  /plugin install {name:<14}  — {blurb}");
        }
        println!();
        println!(
            "  (Run these inside any Claude Code session. claudectl does not install \
             skills automatically.)"
        );
        Ok(PhaseStatus::Installed {
            details: format!("{} suggestion(s) shown", SUGGESTED_SKILLS.len()),
        })
    }

    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus> {
        if answers.skip_skills {
            return Ok(PhaseStatus::Skipped);
        }
        Ok(PhaseStatus::Skipped)
    }

    fn remove(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_five_phases_in_canonical_order() {
        let r = registry();
        let ids: Vec<_> = r.iter().map(|p| p.id()).collect();
        assert_eq!(ids, vec!["budget", "brain", "plugin", "bus", "skills"]);
    }

    #[test]
    fn record_from_status_preserves_label_and_details() {
        let r = record_from_status(
            &PhaseStatus::Installed {
                details: "x".into(),
            },
            "2026-06-06T00:00:00Z",
        );
        assert_eq!(r.status, "installed");
        assert_eq!(r.details.as_deref(), Some("x"));
    }

    #[test]
    fn upsert_toml_appends_when_absent() {
        let updated = upsert_toml_top_level_number("interval = 2000\n", "budget", 50.0);
        assert!(updated.contains("interval = 2000"));
        assert!(updated.contains("budget = 50"));
    }

    #[test]
    fn upsert_toml_replaces_existing() {
        let updated =
            upsert_toml_top_level_number("budget = 25\ninterval = 2000\n", "budget", 50.0);
        assert!(!updated.contains("budget = 25"));
        assert!(updated.contains("budget = 50"));
        assert!(updated.contains("interval = 2000"));
    }

    #[test]
    fn derive_role_strips_leading_dot_and_lowercases() {
        assert_eq!(derive_role_from_cwd_at(Path::new("/work/MyProj")), "myproj");
        assert_eq!(
            derive_role_from_cwd_at(Path::new("/work/.hidden")),
            "hidden"
        );
        assert_eq!(derive_role_from_cwd_at(Path::new("/")), "default");
    }
}

//! `claudectl doctor` — install + runtime health check (#326).
//!
//! Top-down checklist that answers "is everything wired up?" in one
//! command. Replaces what was scattered across:
//!
//! * `claudectl --doctor` (terminal compat only)
//! * `claudectl init --check` (onboarding-marker drift only)
//! * scattered "is X reachable?" probes the user had to chain manually
//!
//! Each check returns a `Check` with status + a fix hint. The renderer
//! shows ✓ / ⚠ / ✗ icons and a one-line message; advisories are
//! non-fatal so a Homebrew-build user with no bus feature compiled in
//! still exits 0.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// Wired up and working.
    Pass,
    /// Wired up partially; works but suboptimal.
    Advisory,
    /// Broken in a way that affects functionality.
    Fail,
    /// Not applicable to this install path / feature set.
    Skipped,
}

impl CheckStatus {
    fn icon(self) -> &'static str {
        match self {
            CheckStatus::Pass => "\u{2713}",     // ✓
            CheckStatus::Advisory => "\u{26a0}", // ⚠
            CheckStatus::Fail => "\u{2717}",     // ✗
            CheckStatus::Skipped => "\u{2014}",  // —
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    /// Short name, fits on one line.
    pub name: String,
    pub status: CheckStatus,
    /// One-line summary of the result.
    pub message: String,
    /// Hint for fixing a Fail or following an Advisory. None when status
    /// is Pass.
    pub fix_hint: Option<String>,
}

/// Run every health check, in display order. Order is meaningful: PATH
/// first because everything else depends on the binary being callable;
/// session discovery last because it's the integration that ties it all
/// together.
pub fn run_all_checks() -> Vec<Check> {
    vec![
        check_binary_on_path(),
        check_claude_code_hooks(),
        check_plugin_installed(),
        check_brain_endpoint(),
        check_bus_feature(),
        check_bus_db(),
        check_session_discovery(),
        check_terminal_integration(),
    ]
}

/// Human-readable renderer. Lays out one row per check, two-space
/// indent, fixed-width name column so messages align.
pub fn render_checks(checks: &[Check]) -> String {
    let mut out = String::new();
    out.push_str("claudectl doctor\n");
    out.push_str("=================\n\n");
    let max_name = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in checks {
        out.push_str(&format!(
            "  {} {:<width$}  {}\n",
            c.status.icon(),
            c.name,
            c.message,
            width = max_name
        ));
        if let Some(hint) = &c.fix_hint {
            out.push_str(&format!("      \u{2192} {hint}\n"));
        }
    }
    out.push('\n');
    let (pass, advisory, fail) = counts(checks);
    out.push_str(&format!(
        "{pass} passed, {advisory} advisory, {fail} failed.\n"
    ));
    out
}

pub fn render_checks_json(checks: &[Check]) -> io::Result<String> {
    serde_json::to_string_pretty(checks).map_err(io::Error::other)
}

/// Exit code: 0 when all Pass + Advisory + Skipped, non-zero when any
/// Fail. Matches the "exit non-zero on any actual problem" rule the
/// epic spec called for.
pub fn exit_code(checks: &[Check]) -> i32 {
    if checks.iter().any(|c| c.status == CheckStatus::Fail) {
        1
    } else {
        0
    }
}

fn counts(checks: &[Check]) -> (usize, usize, usize) {
    let mut pass = 0;
    let mut advisory = 0;
    let mut fail = 0;
    for c in checks {
        match c.status {
            CheckStatus::Pass => pass += 1,
            CheckStatus::Advisory => advisory += 1,
            CheckStatus::Fail => fail += 1,
            CheckStatus::Skipped => {}
        }
    }
    (pass, advisory, fail)
}

// ─── individual checks ──────────────────────────────────────────────────────

fn check_binary_on_path() -> Check {
    // Compare the running binary against what `which claudectl` resolves
    // to. Mismatches mean the user is running one binary while their
    // hooks resolve a different one (typical after `cargo install` on top
    // of a previous `brew install`).
    let running = std::env::current_exe().ok();
    let on_path = std::process::Command::new("which")
        .arg("claudectl")
        .output()
        .ok()
        .and_then(|o| {
            if !o.status.success() {
                return None;
            }
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| PathBuf::from(s.trim()))
        });
    match (running, on_path) {
        (Some(r), Some(p)) if r.canonicalize().ok() == p.canonicalize().ok() => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Pass,
            message: p.display().to_string(),
            fix_hint: None,
        },
        (Some(r), Some(p)) => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Advisory,
            message: format!("running {}, PATH resolves {}", r.display(), p.display()),
            fix_hint: Some(
                "Two installs detected. Hooks call `claudectl` by name — \
                 verify they use the version you expect."
                    .into(),
            ),
        },
        (Some(r), None) => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Fail,
            message: format!("{} not on PATH", r.display()),
            fix_hint: Some(
                "Add the install dir to PATH so hooks can find `claudectl` by name.".into(),
            ),
        },
        _ => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Advisory,
            message: "could not resolve running binary".into(),
            fix_hint: None,
        },
    }
}

fn check_claude_code_hooks() -> Check {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Check {
            name: "Claude Code hooks".into(),
            status: CheckStatus::Fail,
            message: "HOME not set".into(),
            fix_hint: None,
        };
    };
    let settings = home.join(".claude").join("settings.json");
    let contents = match std::fs::read_to_string(&settings) {
        Ok(s) => s,
        Err(_) => {
            return Check {
                name: "Claude Code hooks".into(),
                status: CheckStatus::Fail,
                message: format!("{} not found", settings.display()),
                fix_hint: Some("Run `claudectl init` to install hooks.".into()),
            };
        }
    };
    if !contents.contains("claudectl") {
        return Check {
            name: "Claude Code hooks".into(),
            status: CheckStatus::Fail,
            message: format!("{} has no claudectl entries", settings.display()),
            fix_hint: Some("Run `claudectl init` (or `claudectl init --plugin-only`).".into()),
        };
    }
    let entries = contents.matches("claudectl").count();
    Check {
        name: "Claude Code hooks".into(),
        status: CheckStatus::Pass,
        message: format!("{entries} claudectl entries in {}", settings.display()),
        fix_hint: None,
    }
}

fn check_plugin_installed() -> Check {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Check {
            name: "plugin files".into(),
            status: CheckStatus::Fail,
            message: "HOME not set".into(),
            fix_hint: None,
        };
    };
    let plugin_dir = home.join(".claude").join("plugins").join("claudectl");
    let manifest = plugin_dir.join(".claude-plugin").join("plugin.json");
    if !manifest.exists() {
        return Check {
            name: "plugin files".into(),
            status: CheckStatus::Fail,
            message: format!("{} missing", plugin_dir.display()),
            fix_hint: Some(
                "Run `claudectl init --plugin-only` to write the embedded plugin files.".into(),
            ),
        };
    }
    let file_count = walk_count(&plugin_dir);
    Check {
        name: "plugin files".into(),
        status: CheckStatus::Pass,
        message: format!("{file_count} files at {}", plugin_dir.display()),
        fix_hint: None,
    }
}

fn walk_count(root: &std::path::Path) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                count += 1;
            }
        }
    }
    count
}

fn check_brain_endpoint() -> Check {
    // Match the existing brain probe (curl + 1s timeout, common ollama
    // port). When unreachable, advise — most users running the TUI
    // without the brain enabled don't care.
    let url = "http://localhost:11434/api/tags";
    let curl = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "1", url])
        .output();
    match curl {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => Check {
            name: "brain endpoint".into(),
            status: CheckStatus::Pass,
            message: format!("ollama reachable at {url}"),
            fix_hint: None,
        },
        _ => Check {
            name: "brain endpoint".into(),
            status: CheckStatus::Advisory,
            message: "no local-LLM endpoint reachable on localhost:11434".into(),
            fix_hint: Some(
                "Brain is optional. To enable: `brew install ollama && ollama serve &` + `ollama pull gemma4:e4b`."
                    .into(),
            ),
        },
    }
}

fn check_bus_feature() -> Check {
    #[cfg(feature = "bus")]
    {
        Check {
            name: "bus feature".into(),
            status: CheckStatus::Pass,
            message: "compiled in".into(),
            fix_hint: None,
        }
    }
    #[cfg(not(feature = "bus"))]
    {
        Check {
            name: "bus feature".into(),
            status: CheckStatus::Advisory,
            message: "not compiled — multi-session coordination unavailable".into(),
            fix_hint: Some(
                "Reinstall with `cargo install claudectl --features bus,coord,relay,hive` or `brew install mercurialsolo/tap/claudectl` (Homebrew bottle includes bus since 0.57.0)."
                    .into(),
            ),
        }
    }
}

fn check_bus_db() -> Check {
    #[cfg(not(feature = "bus"))]
    {
        return Check {
            name: "bus DB".into(),
            status: CheckStatus::Skipped,
            message: "bus feature not compiled in".into(),
            fix_hint: None,
        };
    }
    #[cfg(feature = "bus")]
    {
        // Lazy creation — opening the store creates the dir + DB if
        // they don't exist, so this also acts as a writability probe.
        match crate::bus::store::open() {
            Ok(_conn) => {
                let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
                    return Check {
                        name: "bus DB".into(),
                        status: CheckStatus::Pass,
                        message: "opened (no HOME path to display)".into(),
                        fix_hint: None,
                    };
                };
                let db = home.join(".claudectl").join("bus").join("bus.db");
                let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
                Check {
                    name: "bus DB".into(),
                    status: CheckStatus::Pass,
                    message: format!("{} ({} bytes)", db.display(), size),
                    fix_hint: None,
                }
            }
            Err(e) => Check {
                name: "bus DB".into(),
                status: CheckStatus::Fail,
                message: format!("cannot open ~/.claudectl/bus/bus.db: {e}"),
                fix_hint: Some(
                    "Check that ~/.claudectl/bus/ is writable. `claudectl init --purge --yes` resets everything.".into(),
                ),
            },
        }
    }
}

fn check_session_discovery() -> Check {
    // Discovery never errors per se — it returns 0 sessions when nothing
    // matches. The signal we want is "the scanner runs and finds at
    // least one session." Zero sessions is normal if no Claude is
    // running; advise instead of fail.
    let sessions = claudectl_core::discovery::scan_sessions();
    if sessions.is_empty() {
        Check {
            name: "session discovery".into(),
            status: CheckStatus::Advisory,
            message: "0 sessions discovered (no Claude Code running?)".into(),
            fix_hint: Some(
                "Start a Claude session in another terminal (`claude`) and re-run `claudectl doctor`."
                    .into(),
            ),
        }
    } else {
        Check {
            name: "session discovery".into(),
            status: CheckStatus::Pass,
            message: format!("{} session(s) discovered", sessions.len()),
            fix_hint: None,
        }
    }
}

fn check_terminal_integration() -> Check {
    // Re-use the existing terminal doctor report. We collapse it to a
    // one-line summary (the detailed view is still available via the
    // legacy `--doctor` flag).
    let report = claudectl_core::terminals::doctor_report();
    if report.terminal == "Unknown" {
        return Check {
            name: "terminal integration".into(),
            status: CheckStatus::Advisory,
            message: "terminal not recognized".into(),
            fix_hint: Some(
                "Tab switching + input automation work in: Ghostty, Kitty, tmux, WezTerm, Warp, iTerm2, Terminal.app, Gnome Terminal, Windows Terminal."
                    .into(),
            ),
        };
    }
    let action_count = report.actions.len();
    Check {
        name: "terminal integration".into(),
        status: CheckStatus::Pass,
        message: format!(
            "{} on {} ({} actions supported)",
            report.terminal, report.platform, action_count
        ),
        fix_hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_handles_empty_check_list() {
        let out = render_checks(&[]);
        assert!(out.contains("claudectl doctor"));
        assert!(out.contains("0 passed"));
    }

    #[test]
    fn exit_code_zero_when_all_pass() {
        let checks = vec![Check {
            name: "x".into(),
            status: CheckStatus::Pass,
            message: "ok".into(),
            fix_hint: None,
        }];
        assert_eq!(exit_code(&checks), 0);
    }

    #[test]
    fn exit_code_zero_when_only_advisories() {
        let checks = vec![Check {
            name: "x".into(),
            status: CheckStatus::Advisory,
            message: "not configured".into(),
            fix_hint: None,
        }];
        assert_eq!(exit_code(&checks), 0);
    }

    #[test]
    fn exit_code_nonzero_on_any_fail() {
        let checks = vec![
            Check {
                name: "a".into(),
                status: CheckStatus::Pass,
                message: "ok".into(),
                fix_hint: None,
            },
            Check {
                name: "b".into(),
                status: CheckStatus::Fail,
                message: "broken".into(),
                fix_hint: Some("fix it".into()),
            },
        ];
        assert_eq!(exit_code(&checks), 1);
    }

    #[test]
    fn counts_split_correctly() {
        let checks = vec![
            Check {
                name: "a".into(),
                status: CheckStatus::Pass,
                message: "".into(),
                fix_hint: None,
            },
            Check {
                name: "b".into(),
                status: CheckStatus::Advisory,
                message: "".into(),
                fix_hint: None,
            },
            Check {
                name: "c".into(),
                status: CheckStatus::Advisory,
                message: "".into(),
                fix_hint: None,
            },
            Check {
                name: "d".into(),
                status: CheckStatus::Fail,
                message: "".into(),
                fix_hint: None,
            },
            Check {
                name: "e".into(),
                status: CheckStatus::Skipped,
                message: "".into(),
                fix_hint: None,
            },
        ];
        assert_eq!(counts(&checks), (1, 2, 1));
    }

    #[test]
    fn render_includes_fix_hint_when_present() {
        let checks = vec![Check {
            name: "test".into(),
            status: CheckStatus::Fail,
            message: "broken".into(),
            fix_hint: Some("run this".into()),
        }];
        let out = render_checks(&checks);
        assert!(out.contains("run this"));
    }

    #[test]
    fn render_omits_hint_when_none() {
        let checks = vec![Check {
            name: "test".into(),
            status: CheckStatus::Pass,
            message: "ok".into(),
            fix_hint: None,
        }];
        let out = render_checks(&checks);
        // No arrow line.
        assert!(!out.contains("\u{2192}"));
    }

    #[test]
    fn json_round_trips() {
        let checks = vec![Check {
            name: "x".into(),
            status: CheckStatus::Pass,
            message: "ok".into(),
            fix_hint: None,
        }];
        let json = render_checks_json(&checks).unwrap();
        let parsed: Vec<Check> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].status, CheckStatus::Pass);
    }
}

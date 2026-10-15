//! What a peer can actually run — probed, not declared (#536).
//!
//! `relay delegate` used to place work on faith. The worker spawns `claude`
//! (`worker.rs`) and a missing binary, a missing working directory and a
//! logged-out machine all surfaced the same way: late, after the message was
//! authenticated, and for a logged-out `claude`, after the caller had been
//! billed for a session that returned "Not logged in" as its result.
//!
//! So a peer advertises what it has on the heartbeat, and the delegator checks
//! before anything goes on the wire.
//!
//! **Probed, not declared**, because this module's whole family of bugs is
//! features that read correctly and ship broken (#433, #434, #455, #464, #487).
//! A manifest a peer asserts without running anything would be the same mistake
//! in a new place.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The tools worth knowing about before placing a task. `claude` is the one
/// that matters — it is what the worker actually spawns — and the rest are the
/// toolchains a delegated task most often needs under it.
pub const PROBED_TOOLS: &[&str] = &["claude", "git", "cargo", "python3", "node"];

/// How long a probe is trusted. A heartbeat every 30s must not shell out to
/// five binaries each time; tool installs are rare and a stale answer is
/// corrected on the next refresh.
const PROBE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ToolState {
    Present {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// Whether a credential for this tool exists locally.
        ///
        /// `Some(false)` is the signal worth having: a `claude` that is
        /// installed but never logged in. **This is presence, not validity** —
        /// an expired credential still reads as `Some(true)`, because the only
        /// way to know better is to spend money asking the API, which a
        /// heartbeat may never do. Treated accordingly at the call site.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<bool>,
    },
    Missing,
}

/// A peer's advertised capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub claudectl_version: String,
    pub os: String,
    pub arch: String,
    pub tools: BTreeMap<String, ToolState>,
}

/// One thing a task needs: `cargo`, `cargo>=1.88`, or `claude:auth`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub tool: String,
    pub min_version: Option<String>,
    pub needs_credential: bool,
}

impl Requirement {
    /// Parse one requirement. `tool`, `tool>=X.Y`, `tool:auth`, or both.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty requirement".into());
        }
        let (head, needs_credential) = match s.strip_suffix(":auth") {
            Some(rest) => (rest, true),
            None => (s, false),
        };
        let (tool, min_version) = match head.split_once(">=") {
            Some((t, v)) => {
                let v = v.trim();
                if v.is_empty() {
                    return Err(format!("{s}: no version after >="));
                }
                (t.trim(), Some(v.to_string()))
            }
            None => (head.trim(), None),
        };
        if tool.is_empty() {
            return Err(format!("{s}: no tool name"));
        }
        Ok(Requirement {
            tool: tool.to_string(),
            min_version,
            needs_credential,
        })
    }

    /// Parse a comma-separated list.
    pub fn parse_list(s: &str) -> Result<Vec<Self>, String> {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(Requirement::parse)
            .collect()
    }
}

impl Capabilities {
    /// Which requirements this peer does not meet, as sentences a caller can
    /// print. Empty means every requirement is satisfied.
    ///
    /// A tool the peer never probed is *not* reported as unmet — an older peer
    /// advertises a manifest without it, and refusing work on that basis would
    /// break setups that work today. Unknown is not the same as missing.
    pub fn unmet(&self, reqs: &[Requirement]) -> Vec<String> {
        let mut out = Vec::new();
        for req in reqs {
            match self.tools.get(&req.tool) {
                None => continue, // never probed; see above
                Some(ToolState::Missing) => {
                    out.push(format!("{} is not installed", req.tool));
                }
                Some(ToolState::Present {
                    version,
                    credential,
                }) => {
                    if let (Some(min), Some(have)) = (&req.min_version, version) {
                        if version_lt(have, min) {
                            out.push(format!("{} is {have}, need >= {min}", req.tool));
                        }
                    }
                    if req.needs_credential && *credential == Some(false) {
                        out.push(format!("{} has no credential on that machine", req.tool));
                    }
                }
            }
        }
        out
    }

    /// One line for `relay peers`.
    pub fn summary(&self) -> String {
        let present: Vec<String> = self
            .tools
            .iter()
            .filter_map(|(name, state)| match state {
                ToolState::Present { version, .. } => Some(match version {
                    Some(v) => format!("{name} {v}"),
                    None => name.clone(),
                }),
                ToolState::Missing => None,
            })
            .collect();
        if present.is_empty() {
            format!("claudectl {}", self.claudectl_version)
        } else {
            format!(
                "claudectl {} · {}",
                self.claudectl_version,
                present.join(", ")
            )
        }
    }
}

/// Compare dotted numeric versions. `true` when `have` is older than `want`.
///
/// Hand-rolled rather than a semver crate: the sync core keeps its dependency
/// count deliberately low, and this only ever compares `--version` output.
/// Non-numeric components compare as 0, so `1.88.0-nightly` reads as `1.88.0`
/// — which is the lenient direction, and lenient is right here: a wrong
/// refusal blocks work that would have succeeded.
pub fn version_lt(have: &str, want: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (h, w) = (parts(have), parts(want));
    for i in 0..h.len().max(w.len()) {
        let a = h.get(i).copied().unwrap_or(0);
        let b = w.get(i).copied().unwrap_or(0);
        if a != b {
            return a < b;
        }
    }
    false
}

/// Pull the first dotted-numeric token out of `--version` output.
/// `cargo 1.88.0 (abc 2025-01-01)` → `1.88.0`.
pub fn extract_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|tok| {
            let core = tok.trim_start_matches('v');
            core.contains('.') && core.starts_with(|c: char| c.is_ascii_digit())
        })
        .map(|tok| tok.trim_start_matches('v').to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// Probing
// ────────────────────────────────────────────────────────────────────────────

fn cache() -> &'static Mutex<Option<(Instant, Capabilities)>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<(Instant, Capabilities)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// This machine's capabilities, probed at most once per [`PROBE_TTL`].
pub fn local() -> Capabilities {
    if let Ok(guard) = cache().lock() {
        if let Some((at, caps)) = guard.as_ref() {
            if at.elapsed() < PROBE_TTL {
                return caps.clone();
            }
        }
    }
    let caps = probe();
    if let Ok(mut guard) = cache().lock() {
        *guard = Some((Instant::now(), caps.clone()));
    }
    caps
}

fn probe() -> Capabilities {
    let mut tools = BTreeMap::new();
    for name in PROBED_TOOLS {
        tools.insert((*name).to_string(), probe_tool(name));
    }
    Capabilities {
        claudectl_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        tools,
    }
}

fn probe_tool(name: &str) -> ToolState {
    let out = std::process::Command::new(name).arg("--version").output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            ToolState::Present {
                version: extract_version(&text),
                credential: credential_for(name),
            }
        }
        // A non-zero `--version` still means the binary is there; a spawn
        // error is what means it is not.
        Ok(_) => ToolState::Present {
            version: None,
            credential: credential_for(name),
        },
        Err(_) => ToolState::Missing,
    }
}

/// Whether a credential exists locally for `name`, without spending anything.
///
/// Only `claude` is checked, and only for *presence*, never validity.
///
/// The three-valued answer is load-bearing and the platforms differ:
///
/// - **macOS** — Claude Code keeps credentials in the login Keychain.
///   `security find-generic-password` without `-w` reports whether the item
///   exists and never prints the secret. Absent there *and* on disk is a real
///   `Some(false)`: that machine has not been logged in.
/// - **Linux and everything else** — a credentials file present is
///   `Some(true)`. Its absence returns **`None`, not `Some(false)`**, because
///   the storage location has not been verified on a real Linux host and a
///   wrong "logged out" would refuse work that would have succeeded. Unknown
///   refuses nothing (see [`Capabilities::unmet`]).
///
/// `CLAUDE_CONFIG_DIR` is honoured first on every platform, since a host that
/// relocates its config is exactly the one a hardcoded path gets wrong.
fn credential_for(name: &str) -> Option<bool> {
    if name != "claude" {
        return None;
    }
    if credentials_file_exists() {
        return Some(true);
    }
    #[cfg(target_os = "macos")]
    {
        let in_keychain = std::process::Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        // On macOS the storage is known, so "nowhere to be found" is an answer.
        Some(in_keychain)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Where Claude Code keeps its config: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
/// `None` when neither is known, which is not the same as "not there".
fn config_base() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(std::path::PathBuf::from(dir));
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => Some(std::path::PathBuf::from(home).join(".claude")),
        _ => None,
    }
}

/// Whether a credentials file sits under `base`.
///
/// Split from [`config_base`] so it is testable on every platform without
/// mutating `HOME` or `CLAUDE_CONFIG_DIR` — env mutation races with parallel
/// tests, and #468 moved this repo away from repointing `HOME` in tests for
/// exactly that reason.
fn credentials_file_in(base: &std::path::Path) -> bool {
    base.join(".credentials.json").exists()
}

fn credentials_file_exists() -> bool {
    config_base().is_some_and(|base| credentials_file_in(&base))
}

// ────────────────────────────────────────────────────────────────────────────
// Persistence
// ────────────────────────────────────────────────────────────────────────────

// A peer's manifest lives in its own `peers/<id>.caps`, not inside `.meta`.
// `save_peer_meta` rewrites the whole meta blob from addr/last_seen/addrs, so
// anything merged in there is erased the next time an address is learned —
// which happens on every reconnect. A separate file cannot be clobbered by it.
//
// It is on disk at all because `relay peers` and `relay delegate` are separate
// processes from `relay serve`: they cannot reach the live registry, and the
// refusal has to work from the CLI.

fn caps_path(peer_id: &str) -> Option<std::path::PathBuf> {
    if super::is_valid_peer_id(peer_id) {
        Some(super::peers_dir().join(format!("{peer_id}.caps")))
    } else {
        None
    }
}

/// Record what a peer advertised. Returns whether anything was written —
/// an unchanged manifest is not rewritten, so a 30-second heartbeat does not
/// churn the disk for every peer forever.
pub fn save_peer_capabilities(peer_id: &str, caps: &Capabilities) -> Result<bool, String> {
    let path = caps_path(peer_id).ok_or_else(|| format!("invalid peer id: {peer_id}"))?;
    if load_peer_capabilities(peer_id).as_ref() == Some(caps) {
        return Ok(false);
    }
    std::fs::create_dir_all(super::peers_dir()).map_err(|e| format!("create peers dir: {e}"))?;
    let json = serde_json::to_string_pretty(caps).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("write caps: {e}"))?;
    Ok(true)
}

/// What a peer last advertised, if anything. `None` covers both "never heard
/// from" and "running a build older than #536" — neither is a reason to refuse
/// work, so the two do not need telling apart.
pub fn load_peer_capabilities(peer_id: &str) -> Option<Capabilities> {
    let path = caps_path(peer_id)?;
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(tools: Vec<(&str, ToolState)>) -> Capabilities {
        Capabilities {
            claudectl_version: "0.79.0".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            tools: tools.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    fn present(v: Option<&str>, cred: Option<bool>) -> ToolState {
        ToolState::Present {
            version: v.map(str::to_string),
            credential: cred,
        }
    }

    #[test]
    fn requirement_parses_each_form() {
        assert_eq!(
            Requirement::parse("cargo").unwrap(),
            Requirement {
                tool: "cargo".into(),
                min_version: None,
                needs_credential: false
            }
        );
        assert_eq!(
            Requirement::parse("cargo>=1.88").unwrap().min_version,
            Some("1.88".into())
        );
        assert!(Requirement::parse("claude:auth").unwrap().needs_credential);
        let both = Requirement::parse("claude>=2.0:auth").unwrap();
        assert_eq!(both.min_version, Some("2.0".into()));
        assert!(both.needs_credential);
    }

    #[test]
    fn requirement_rejects_nonsense() {
        assert!(Requirement::parse("").is_err());
        assert!(Requirement::parse(">=1.0").is_err());
        assert!(Requirement::parse("cargo>=").is_err());
    }

    #[test]
    fn missing_tool_is_unmet() {
        let c = caps(vec![("cargo", ToolState::Missing)]);
        let reqs = Requirement::parse_list("cargo").unwrap();
        assert_eq!(c.unmet(&reqs), vec!["cargo is not installed"]);
    }

    #[test]
    fn old_version_is_unmet_and_names_both_numbers() {
        let c = caps(vec![("cargo", present(Some("1.80.0"), None))]);
        let reqs = Requirement::parse_list("cargo>=1.88").unwrap();
        assert_eq!(c.unmet(&reqs), vec!["cargo is 1.80.0, need >= 1.88"]);
    }

    #[test]
    fn new_enough_version_is_met() {
        let c = caps(vec![("cargo", present(Some("1.90.1"), None))]);
        let reqs = Requirement::parse_list("cargo>=1.88").unwrap();
        assert!(c.unmet(&reqs).is_empty());
    }

    #[test]
    fn a_logged_out_claude_is_unmet() {
        let c = caps(vec![("claude", present(Some("2.1.0"), Some(false)))]);
        let reqs = Requirement::parse_list("claude:auth").unwrap();
        assert_eq!(
            c.unmet(&reqs),
            vec!["claude has no credential on that machine"]
        );
    }

    #[test]
    fn a_logged_in_claude_is_met() {
        let c = caps(vec![("claude", present(Some("2.1.0"), Some(true)))]);
        let reqs = Requirement::parse_list("claude:auth").unwrap();
        assert!(c.unmet(&reqs).is_empty());
    }

    /// An older peer advertises a manifest that predates a tool being probed.
    /// Refusing on that basis would break setups that work today, so unknown
    /// has to read as "no opinion", never as "missing".
    #[test]
    fn an_unprobed_tool_is_not_treated_as_missing() {
        let c = caps(vec![("git", present(Some("2.39.0"), None))]);
        let reqs = Requirement::parse_list("deno>=1.0,git").unwrap();
        assert!(c.unmet(&reqs).is_empty());
    }

    #[test]
    fn unmet_reports_every_failure_not_just_the_first() {
        let c = caps(vec![
            ("cargo", ToolState::Missing),
            ("claude", present(Some("1.0.0"), Some(false))),
        ]);
        let reqs = Requirement::parse_list("cargo,claude>=2.0:auth").unwrap();
        assert_eq!(c.unmet(&reqs).len(), 3);
    }

    #[test]
    fn version_compare_handles_uneven_lengths_and_suffixes() {
        assert!(version_lt("1.88", "1.88.1"));
        assert!(!version_lt("1.88.0", "1.88"));
        assert!(!version_lt("1.90", "1.88.9"));
        // A pre-release reads as its release number — the lenient direction.
        assert!(!version_lt("1.88.0-nightly", "1.88.0"));
    }

    #[test]
    fn version_extraction_from_real_output() {
        assert_eq!(
            extract_version("cargo 1.88.0 (abc 2025-01-01)").as_deref(),
            Some("1.88.0")
        );
        assert_eq!(
            extract_version("2.1.290 (Claude Code)").as_deref(),
            Some("2.1.290")
        );
        assert_eq!(
            extract_version("git version 2.39.5").as_deref(),
            Some("2.39.5")
        );
        assert_eq!(extract_version("Python 3.11.9").as_deref(), Some("3.11.9"));
        assert_eq!(extract_version("v22.14.0").as_deref(), Some("22.14.0"));
        assert_eq!(extract_version("no numbers here"), None);
    }

    #[test]
    fn capabilities_survive_a_json_round_trip() {
        let c = caps(vec![
            ("claude", present(Some("2.1.290"), Some(true))),
            ("cargo", ToolState::Missing),
        ]);
        let json = serde_json::to_value(&c).unwrap();
        let back: Capabilities = serde_json::from_value(json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn a_credentials_file_is_found_under_the_config_base() {
        let dir = std::env::temp_dir().join(format!("cctl-caps-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert!(
            !credentials_file_in(&dir),
            "an empty config dir has no credential"
        );
        std::fs::write(dir.join(".credentials.json"), "{}").unwrap();
        assert!(credentials_file_in(&dir), "a present file is found");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The platforms answer differently on purpose, and the Linux answer is
    /// the one that matters: an absent file there is **unknown**, not
    /// "logged out". Reporting `Some(false)` would refuse work on every Linux
    /// worker whose credentials this code has not learned to find.
    #[test]
    fn an_absent_credential_is_unknown_off_macos() {
        let answer = credential_for("claude");
        #[cfg(target_os = "macos")]
        assert!(
            answer.is_some(),
            "on macOS the Keychain is authoritative, so there is always an answer"
        );
        #[cfg(not(target_os = "macos"))]
        if !credentials_file_exists() {
            assert_eq!(
                answer, None,
                "no file and no known store must read as unknown, never as logged out"
            );
        }
    }

    #[test]
    fn only_claude_has_a_credential_notion() {
        assert_eq!(credential_for("cargo"), None);
        assert_eq!(credential_for("git"), None);
    }

    #[test]
    fn summary_lists_present_tools_only() {
        let c = caps(vec![
            ("claude", present(Some("2.1.290"), Some(true))),
            ("cargo", ToolState::Missing),
        ]);
        let s = c.summary();
        assert!(s.contains("claudectl 0.79.0"));
        assert!(s.contains("claude 2.1.290"));
        assert!(!s.contains("cargo"));
    }
}

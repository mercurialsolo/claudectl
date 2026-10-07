//! Keep `relay serve` alive across logout and reboot (#438, RFC §8.3).
//!
//! `relay serve` is a foreground "Press Ctrl+C to stop" process. Nothing
//! restarts it, so the cluster view goes stale the moment a terminal closes —
//! and `relay fleet` with no fresh snapshot falls back to local sessions. Every
//! user who wanted a durable relay had to write their own `launchd` plist or
//! park it in `tmux`.
//!
//! This is #425's phase 3, but it is **not** app-specific: the agent is managed
//! from the CLI, so solving it for the Mac app solves it for terminal users in
//! the same change. That is why it lives in `src/relay/` rather than inside an
//! app bundle.
//!
//! # Why `relay serve` is safe to run headless
//!
//! It never reads stdin. The loop polls an `AtomicBool` that a `ctrlc` handler
//! clears, so with no TTY it simply parks — which is what a daemon should do. A
//! process that blocked on `read_line` would crashloop under `KeepAlive`.
//!
//! # Why there is no graceful-shutdown handshake
//!
//! `launchctl bootout` sends `SIGTERM`, and `ctrlc` is built here without its
//! `termination` feature, so the default action applies and the process dies at
//! once. That is deliberate rather than overlooked: every file the relay owns is
//! written atomically (temp + rename — `fleet.json` in
//! `claudectl_core::fleet`, the knowledge store in `hive::store`), so an abrupt
//! death cannot leave a torn file. The worst case is losing the current
//! one-second tick, and trading that for a signal-handling path that has to be
//! correct under `launchd` is not a good trade.
//!
//! # macOS only, and it says so
//!
//! The commands exist on every platform and explain themselves on the ones
//! without `launchd`, rather than being compiled away — a missing subcommand
//! reads as a broken build, while a clear "use systemd --user, here is the
//! recipe" is an answer.

use std::path::{Path, PathBuf};

/// Reverse-DNS label. Also the plist's basename and the `launchctl` service
/// name, so it is defined once.
pub const AGENT_LABEL: &str = "io.claudectl.relay";

/// Arguments the agent will run `claudectl` with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub port: u16,
    pub http_port: Option<u16>,
    pub http_addr: Option<String>,
    pub auth_token: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            port: 9847,
            http_port: None,
            http_addr: None,
            auth_token: None,
        }
    }
}

impl AgentConfig {
    /// The `ProgramArguments` vector, binary first.
    ///
    /// Built as a `Vec` rather than a command string because a plist takes an
    /// argv array — which also means a path containing a space needs no quoting
    /// and cannot be re-split.
    pub fn argv(&self, binary: &Path) -> Vec<String> {
        let mut args = vec![
            binary.display().to_string(),
            "relay".to_string(),
            "serve".to_string(),
            "--port".to_string(),
            self.port.to_string(),
        ];
        if let Some(p) = self.http_port {
            args.push("--http-port".to_string());
            args.push(p.to_string());
        }
        if let Some(a) = &self.http_addr {
            args.push("--http-addr".to_string());
            args.push(a.clone());
        }
        if let Some(t) = &self.auth_token {
            args.push("--auth-token".to_string());
            args.push(t.clone());
        }
        args
    }
}

/// `~/Library/LaunchAgents/io.claudectl.relay.plist`.
pub fn plist_path() -> PathBuf {
    claudectl_core::helpers::dirs_home()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{AGENT_LABEL}.plist"))
}

/// Where the agent's stdout and stderr go.
///
/// Under `~/.claudectl/relay/` beside `fleet.json`, not `~/Library/Logs`, so
/// everything about one relay is in one place.
pub fn log_paths() -> (PathBuf, PathBuf) {
    let dir = claudectl_core::helpers::dirs_home()
        .join(".claudectl")
        .join("relay");
    (dir.join("agent.out.log"), dir.join("agent.err.log"))
}

/// Escape text for an XML character-data position.
///
/// A home directory can contain `&` or `<`, and an unescaped one produces a
/// plist `launchd` silently refuses to load — the failure mode being an agent
/// that is "installed" and never runs.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render the launchd property list.
///
/// Pure: path in, XML out, no filesystem. That is what makes the interesting
/// part testable without touching a real `launchd`.
pub fn render_plist(binary: &Path, cfg: &AgentConfig) -> String {
    let (out_log, err_log) = log_paths();
    let args: String = cfg
        .argv(binary)
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
  <key>WorkingDirectory</key>
  <string>{home}</string>
</dict>
</plist>
"#,
        label = AGENT_LABEL,
        args = args,
        out = xml_escape(&out_log.display().to_string()),
        err = xml_escape(&err_log.display().to_string()),
        home = xml_escape(&claudectl_core::helpers::dirs_home().display().to_string()),
    )
}

/// The `claudectl` binary the agent should run.
///
/// `current_exe()` and deliberately **not** canonicalized. On a Homebrew
/// install, `/opt/homebrew/bin/claudectl` is a symlink into a versioned Cellar
/// directory; resolving it would bake `.../claudectl/0.65.0/bin/claudectl` into
/// the plist, and the next `brew upgrade` would leave `launchd` restarting a
/// path that no longer exists. The stable symlink is the right thing to record.
pub fn resolve_binary() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot find this binary's own path: {e}"))
}

/// `gui/<uid>`, the modern `launchctl` domain for a per-user agent.
#[cfg(target_os = "macos")]
fn gui_domain() -> String {
    let uid = unsafe { libc::getuid() };
    format!("gui/{uid}")
}

/// Whether `launchd` currently has the agent loaded.
#[cfg(target_os = "macos")]
pub fn is_loaded() -> bool {
    std::process::Command::new("launchctl")
        .args(["print", &format!("{}/{AGENT_LABEL}", gui_domain())])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn is_loaded() -> bool {
    false
}

/// What `relay agent-status` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub plist_exists: bool,
    pub loaded: bool,
    pub plist: PathBuf,
}

pub fn status() -> AgentStatus {
    let plist = plist_path();
    AgentStatus {
        plist_exists: plist.exists(),
        loaded: is_loaded(),
        plist,
    }
}

/// Install (or replace) the agent and load it.
///
/// Idempotent: an existing plist is overwritten and the service reloaded, so
/// re-running after a `brew upgrade` or a port change is the supported way to
/// update it.
#[cfg(target_os = "macos")]
pub fn install(cfg: &AgentConfig) -> Result<PathBuf, String> {
    let binary = resolve_binary()?;
    let path = plist_path();
    let dir = path
        .parent()
        .ok_or_else(|| "LaunchAgents path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // The log directory has to exist before launchd opens the paths, or the
    // agent fails to spawn with a bare errno and no obvious cause.
    let (out_log, _) = log_paths();
    if let Some(log_dir) = out_log.parent() {
        std::fs::create_dir_all(log_dir)
            .map_err(|e| format!("create {}: {e}", log_dir.display()))?;
    }

    // Unload an existing instance first. A plist rewritten underneath a loaded
    // service leaves launchd running the old argv until something reloads it,
    // which looks exactly like the install not having worked.
    if is_loaded() {
        let _ = bootout();
    }

    std::fs::write(&path, render_plist(&binary, cfg))
        .map_err(|e| format!("write {}: {e}", path.display()))?;

    bootstrap(&path)?;
    Ok(path)
}

/// `launchctl bootstrap`, falling back to the older `load`.
#[cfg(target_os = "macos")]
fn bootstrap(plist: &Path) -> Result<(), String> {
    let out = std::process::Command::new("launchctl")
        .args(["bootstrap", &gui_domain(), &plist.display().to_string()])
        .output()
        .map_err(|e| format!("run launchctl bootstrap: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // `bootstrap` arrived in 10.11 and is the documented path, but it is also
    // stricter — fall back rather than fail on a machine where `load` is what
    // works.
    let legacy = std::process::Command::new("launchctl")
        .args(["load", "-w", &plist.display().to_string()])
        .output()
        .map_err(|e| format!("run launchctl load: {e}"))?;
    if legacy.status.success() {
        return Ok(());
    }
    Err(format!(
        "launchctl could not load the agent.\n  bootstrap: {}\n  load: {}",
        String::from_utf8_lossy(&out.stderr).trim(),
        String::from_utf8_lossy(&legacy.stderr).trim()
    ))
}

#[cfg(target_os = "macos")]
fn bootout() -> Result<(), String> {
    let out = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{}/{AGENT_LABEL}", gui_domain())])
        .output()
        .map_err(|e| format!("run launchctl bootout: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// Unload the agent and remove its plist.
///
/// **The plist is removed even when `bootout` fails**, which is the whole point:
/// the usual failure mode for this kind of feature is an orphaned plist that
/// keeps resurrecting a service the user thought they removed. A `bootout`
/// failure usually means the agent was not loaded anyway, so it is reported as
/// a note rather than aborting the removal.
///
/// Returns whether anything was actually there, so an uninstall with nothing
/// installed can say so instead of claiming success.
#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<(bool, Option<String>), String> {
    let path = plist_path();
    let existed = path.exists();
    let loaded = is_loaded();

    let note = if loaded { bootout().err() } else { None };

    if existed {
        std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    }
    Ok((existed || loaded, note))
}

#[cfg(not(target_os = "macos"))]
pub fn install(_cfg: &AgentConfig) -> Result<PathBuf, String> {
    Err(unsupported())
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall() -> Result<(bool, Option<String>), String> {
    Err(unsupported())
}

/// What to say on a platform without `launchd`.
///
/// A recipe rather than a refusal: the equivalent is short, and a user on Linux
/// asking for a durable relay deserves the answer rather than a closed door.
#[cfg(not(target_os = "macos"))]
pub fn unsupported() -> String {
    "`relay install-agent` manages a macOS launchd agent, and this is not macOS.\n\n\
     On Linux, systemd --user does the same job. Write\n\
     ~/.config/systemd/user/claudectl-relay.service:\n\n\
     \x20 [Unit]\n\
     \x20 Description=claudectl relay\n\n\
     \x20 [Service]\n\
     \x20 ExecStart=%h/.cargo/bin/claudectl relay serve --port 9847\n\
     \x20 Restart=always\n\n\
     \x20 [Install]\n\
     \x20 WantedBy=default.target\n\n\
     then: systemctl --user enable --now claudectl-relay\n\
     and:  loginctl enable-linger $USER   (so it survives logout)"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_starts_with_the_binary_and_the_subcommand() {
        let cfg = AgentConfig::default();
        let argv = cfg.argv(Path::new("/opt/homebrew/bin/claudectl"));
        assert_eq!(
            argv,
            vec![
                "/opt/homebrew/bin/claudectl",
                "relay",
                "serve",
                "--port",
                "9847"
            ]
        );
    }

    #[test]
    fn optional_flags_appear_only_when_set() {
        let cfg = AgentConfig {
            port: 9000,
            http_port: Some(9876),
            http_addr: Some("127.0.0.1".into()),
            auth_token: Some("secret".into()),
        };
        let argv = cfg.argv(Path::new("/usr/local/bin/claudectl"));
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/claudectl",
                "relay",
                "serve",
                "--port",
                "9000",
                "--http-port",
                "9876",
                "--http-addr",
                "127.0.0.1",
                "--auth-token",
                "secret",
            ]
        );
        // And the default carries none of them.
        let bare = AgentConfig::default().argv(Path::new("/x/claudectl"));
        assert!(!bare.iter().any(|a| a.starts_with("--http")));
        assert!(!bare.iter().any(|a| a == "--auth-token"));
    }

    #[test]
    fn the_plist_carries_the_label_argv_and_keepalive() {
        let xml = render_plist(
            Path::new("/opt/homebrew/bin/claudectl"),
            &AgentConfig::default(),
        );
        assert!(xml.contains("<string>io.claudectl.relay</string>"), "{xml}");
        assert!(xml.contains("<key>KeepAlive</key>\n  <true/>"), "{xml}");
        assert!(xml.contains("<key>RunAtLoad</key>\n  <true/>"), "{xml}");
        // Every argv element is its own <string>, so a path with a space cannot
        // be re-split by launchd.
        assert!(
            xml.contains("<string>/opt/homebrew/bin/claudectl</string>"),
            "{xml}"
        );
        assert!(xml.contains("<string>relay</string>"), "{xml}");
        assert!(xml.contains("<string>serve</string>"), "{xml}");
        assert!(xml.contains("<string>9847</string>"), "{xml}");
        // A plist launchd will actually parse.
        assert!(xml.starts_with("<?xml version=\"1.0\""), "{xml}");
        assert!(xml.trim_end().ends_with("</plist>"), "{xml}");
    }

    #[test]
    fn a_path_with_a_space_survives_as_one_argument() {
        // The naive "join argv with spaces into a command string" approach
        // breaks here, which is why `argv` is a vector.
        let argv = AgentConfig::default().argv(Path::new("/Users/a b/bin/claudectl"));
        assert_eq!(argv[0], "/Users/a b/bin/claudectl");
        let xml = render_plist(
            Path::new("/Users/a b/bin/claudectl"),
            &AgentConfig::default(),
        );
        assert!(
            xml.contains("<string>/Users/a b/bin/claudectl</string>"),
            "{xml}"
        );
    }

    #[test]
    fn xml_special_characters_are_escaped() {
        // An unescaped `&` in a home directory yields a plist launchd silently
        // refuses — an agent that is "installed" and never runs.
        let xml = render_plist(
            Path::new("/Users/a&b/bin/claudectl"),
            &AgentConfig::default(),
        );
        assert!(xml.contains("/Users/a&amp;b/bin/claudectl"), "{xml}");
        assert!(
            !xml.contains("/Users/a&b/"),
            "a raw ampersand reached the plist: {xml}"
        );
        assert_eq!(
            xml_escape("a<b>c&d\"e'f"),
            "a&lt;b&gt;c&amp;d&quot;e&apos;f"
        );
    }

    #[test]
    fn the_token_is_not_logged_into_the_argv_test_by_accident() {
        // `--auth-token` lands in the plist, which is 0644 by default under
        // ~/Library/LaunchAgents. Document that by asserting it: a caller who
        // passes a token should know it is readable there.
        let cfg = AgentConfig {
            auth_token: Some("sekrit".into()),
            ..Default::default()
        };
        let xml = render_plist(Path::new("/x/claudectl"), &cfg);
        assert!(
            xml.contains("<string>sekrit</string>"),
            "the token has to be in the plist for the agent to use it"
        );
    }

    #[test]
    fn the_label_is_used_for_both_the_plist_name_and_the_service() {
        assert!(
            plist_path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(AGENT_LABEL),
            "the plist basename should be derived from the label"
        );
        assert_eq!(AGENT_LABEL, "io.claudectl.relay");
    }

    #[test]
    fn logs_land_beside_the_fleet_snapshot() {
        let (out, err) = log_paths();
        assert!(out.ends_with("agent.out.log"), "{out:?}");
        assert!(err.ends_with("agent.err.log"), "{err:?}");
        assert_eq!(out.parent(), err.parent());
        assert!(
            out.parent().unwrap().ends_with(".claudectl/relay"),
            "{out:?}"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_non_macos_platform_gets_a_recipe_not_a_refusal() {
        let msg = unsupported();
        assert!(msg.contains("systemd --user"), "{msg}");
        assert!(msg.contains("enable-linger"), "{msg}");
        assert!(install(&AgentConfig::default()).is_err());
    }
}

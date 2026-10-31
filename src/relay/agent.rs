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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfig {
    pub port: Option<u16>,
    pub http_port: Option<u16>,
    pub http_addr: Option<String>,
    pub auth_token: Option<String>,
}

impl AgentConfig {
    /// The `ProgramArguments` vector, binary first.
    ///
    /// Built as a `Vec` rather than a command string because a plist takes an
    /// argv array — which also means a path containing a space needs no quoting
    /// and cannot be re-split.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(dead_code, reason = "only the launchd agent path renders a plist")
    )]
    pub fn argv(&self, binary: &Path) -> Vec<String> {
        let mut args = vec![
            binary.display().to_string(),
            "relay".to_string(),
            "serve".to_string(),
        ];
        if let Some(port) = self.port {
            args.push("--port".to_string());
            args.push(port.to_string());
        }
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

/// `io.claudectl.headless`, the brain and supervisor service.
pub const HEADLESS_LABEL: &str = "io.claudectl.headless";

/// Which background service (#546, gap 2).
///
/// `relay serve` was the only thing with an installer, so the orchestrating
/// half of "always running and coordinating in the background" was started by
/// hand and stopped with the terminal.
///
/// #546 describes that half as "the brain and supervisor — `--headless --brain
/// --auto-run`, `supervisor run`". There is no `supervisor run` daemon: that
/// subcommand submits a `tasks.toml` and exits. The reconciler and the actuator
/// tick inside `commands::run_headless` next to the brain, so the two are
/// **one** service, not two — which is why this enum has two variants, not three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// `relay serve` — transport, heartbeats, and the fleet snapshot.
    Relay,
    /// `--headless` — the brain and the supervisor reconciler, one process.
    Headless,
}

impl Service {
    /// Every service, for the commands that act on the whole background set.
    pub const ALL: [Service; 2] = [Service::Relay, Service::Headless];

    /// Reverse-DNS label: the plist basename and the `launchctl` service name.
    pub fn label(self) -> &'static str {
        match self {
            Service::Relay => AGENT_LABEL,
            Service::Headless => HEADLESS_LABEL,
        }
    }

    /// The unit file name `systemctl --user` addresses it by.
    ///
    /// Only the systemd path uses this for real work, but it is compiled on
    /// both platforms because the collision test asserts unit names everywhere
    /// — a name clash would mean installing one service overwrote the other,
    /// and that is worth catching on whichever machine runs the tests.
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "used by tests and by the systemd install path")
    )]
    pub fn unit_name(self) -> &'static str {
        match self {
            Service::Relay => "claudectl-relay.service",
            Service::Headless => "claudectl-headless.service",
        }
    }

    /// Its directory under `~/.claudectl/`. One service, one place to look —
    /// the relay's logs already sit beside `fleet.json` for that reason, and
    /// the headless heartbeat sits beside its own logs for the same one.
    pub fn dir_name(self) -> &'static str {
        match self {
            Service::Relay => "relay",
            Service::Headless => "headless",
        }
    }

    /// `Description=` in the systemd unit, and what the CLI calls it.
    pub fn description(self) -> &'static str {
        match self {
            Service::Relay => "claudectl relay",
            Service::Headless => "claudectl headless (brain + supervisor)",
        }
    }
}

/// Arguments the headless service will run `claudectl` with.
///
/// `--brain` and `--auto-run` are both off by default, and that is a decision
/// rather than an oversight: a service installed once at login that executes
/// brain decisions without being asked is not something to default on.
/// `--headless` alone still runs the supervisor reconciler and the coordination
/// loop, which is the half with no other way to stay up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadlessConfig {
    /// Enable the local-LLM brain. With no reachable endpoint the process warns
    /// and runs on without it (`commands::run_headless`), so this is safe to set
    /// on a machine whose endpoint comes and goes — it will not crashloop.
    pub brain: bool,
    /// Execute brain suggestions without confirmation. Implies `brain`.
    pub auto_run: bool,
}

impl HeadlessConfig {
    /// The argv, binary first — same contract as [`AgentConfig::argv`].
    pub fn argv(&self, binary: &Path) -> Vec<String> {
        let mut args = vec![binary.display().to_string(), "--headless".to_string()];
        // `--auto-run` without `--brain` is accepted by clap but does nothing,
        // since the brain is what produces the suggestions to run. Treat the
        // narrower flag as implying the wider one rather than installing a
        // service that silently ignores what it was asked for.
        if self.brain || self.auto_run {
            args.push("--brain".to_string());
        }
        if self.auto_run {
            args.push("--auto-run".to_string());
        }
        args
    }
}

/// What to install, for one of the two services.
///
/// The variants carry different settings — ports and a token for the relay,
/// brain flags for the headless process — so this is an enum rather than one
/// struct with half its fields ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceConfig {
    Relay(AgentConfig),
    Headless(HeadlessConfig),
}

impl ServiceConfig {
    pub fn service(&self) -> Service {
        match self {
            ServiceConfig::Relay(_) => Service::Relay,
            ServiceConfig::Headless(_) => Service::Headless,
        }
    }

    /// The argv for whichever service this is.
    pub fn argv(&self, binary: &Path) -> Vec<String> {
        match self {
            ServiceConfig::Relay(c) => c.argv(binary),
            ServiceConfig::Headless(c) => c.argv(binary),
        }
    }
}

/// `~/Library/LaunchAgents/<label>.plist`.
///
/// macOS only: `service_path` routes to the systemd unit elsewhere (#546), and
/// the tests all take an explicit home through [`plist_path_in`], so this has
/// no caller on any other platform — in a test build either.
#[cfg(target_os = "macos")]
pub fn plist_path(service: Service) -> PathBuf {
    plist_path_in(&claudectl_core::helpers::dirs_home(), service)
}

/// [`plist_path`] under an explicit home.
///
/// Split out so the tests never read the ambient `HOME`. Another test in this
/// crate mutates `HOME` process-wide (`brain::decisions`), and cargo runs tests
/// on parallel threads, so anything reading the env here is a race.
#[cfg(any(target_os = "macos", test))]
pub fn plist_path_in(home: &Path, service: Service) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", service.label()))
}

/// Where the agent's stdout and stderr go.
///
/// Under `~/.claudectl/relay/` beside `fleet.json`, not `~/Library/Logs`, so
/// everything about one relay is in one place.
pub fn log_paths(service: Service) -> (PathBuf, PathBuf) {
    log_paths_in(&claudectl_core::helpers::dirs_home(), service)
}

/// [`log_paths`] under an explicit home. See [`plist_path_in`] for why.
pub fn log_paths_in(home: &Path, service: Service) -> (PathBuf, PathBuf) {
    let dir = home.join(".claudectl").join(service.dir_name());
    (dir.join("agent.out.log"), dir.join("agent.err.log"))
}

/// Escape text for an XML character-data position.
///
/// A home directory can contain `&` or `<`, and an unescaped one produces a
/// plist `launchd` silently refuses to load — the failure mode being an agent
/// that is "installed" and never runs.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only the launchd agent path renders a plist")
)]
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
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only the launchd agent path renders a plist")
)]
pub fn render_plist(binary: &Path, cfg: &ServiceConfig) -> String {
    render_plist_in(binary, cfg, &claudectl_core::helpers::dirs_home())
}

/// [`render_plist`] under an explicit home. See [`plist_path_in`] for why.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only the launchd agent path renders a plist")
)]
pub fn render_plist_in(binary: &Path, cfg: &ServiceConfig, home: &Path) -> String {
    let (out_log, err_log) = log_paths_in(home, cfg.service());
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
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home}</string>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
</dict>
</plist>
"#,
        label = cfg.service().label(),
        args = args,
        out = xml_escape(&out_log.display().to_string()),
        err = xml_escape(&err_log.display().to_string()),
        home = xml_escape(&home.display().to_string()),
        path = xml_escape(&agent_path(home)),
    )
}

/// The PATH the agent runs with: where `claude`, `node`, `cargo` and Homebrew
/// tools are normally installed, then the system directories.
///
/// launchd's own default is `/usr/bin:/bin:/usr/sbin:/sbin`, under which a
/// relay can't find `claude` to run delegated work, and its capability probe
/// reports it missing so peers refuse to delegate at all. Built from `home`
/// rather than copied from the installing shell, so `install-agent` over ssh
/// (a non-login shell with a bare PATH) produces the same agent as from a
/// terminal.
pub fn agent_path(home: &Path) -> String {
    let h = home.display();
    format!(
        "{h}/.local/bin:{h}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:{h}/.claude/local:/usr/bin:/bin:/usr/sbin:/sbin"
    )
}

/// The `claudectl` binary the agent should run.
///
/// `current_exe()` and deliberately **not** canonicalized. On a Homebrew
/// install, `/opt/homebrew/bin/claudectl` is a symlink into a versioned Cellar
/// directory; resolving it would bake `.../claudectl/0.65.0/bin/claudectl` into
/// the plist, and the next `brew upgrade` would leave `launchd` restarting a
/// path that no longer exists. The stable symlink is the right thing to record.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only the launchd agent path renders a plist")
)]
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
pub fn is_loaded(service: Service) -> bool {
    std::process::Command::new("launchctl")
        .args(["print", &format!("{}/{}", gui_domain(), service.label())])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// What `relay agent-status` reports.
///
/// Named for the service rather than the plist since #546: the same struct now
/// describes a launchd agent on macOS and a systemd `--user` unit on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub service_exists: bool,
    pub loaded: bool,
    pub service: PathBuf,
}

/// The service definition this platform's manager reads.
#[cfg(target_os = "macos")]
pub fn service_path(service: Service) -> PathBuf {
    plist_path(service)
}

#[cfg(not(target_os = "macos"))]
pub fn service_path(service: Service) -> PathBuf {
    unit_path(service)
}

pub fn status(service: Service) -> AgentStatus {
    let path = service_path(service);
    AgentStatus {
        service_exists: path.exists(),
        loaded: is_loaded(service),
        service: path,
    }
}

/// Install (or replace) the agent and load it.
///
/// Idempotent: an existing plist is overwritten and the service reloaded, so
/// re-running after a `brew upgrade` or a port change is the supported way to
/// update it.
#[cfg(target_os = "macos")]
pub fn install(cfg: &ServiceConfig) -> Result<PathBuf, String> {
    let binary = resolve_binary()?;
    let service = cfg.service();
    let path = plist_path(service);
    let dir = path
        .parent()
        .ok_or_else(|| "LaunchAgents path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // The log directory has to exist before launchd opens the paths, or the
    // agent fails to spawn with a bare errno and no obvious cause.
    let (out_log, _) = log_paths(service);
    if let Some(log_dir) = out_log.parent() {
        std::fs::create_dir_all(log_dir)
            .map_err(|e| format!("create {}: {e}", log_dir.display()))?;
    }

    let rendered = render_plist(&binary, cfg);

    // Nothing to do if this exact service is already installed and running.
    // Without this, adding the headless service with `--headless` would also
    // tear down and restart a relay that nothing had changed about — a side
    // effect the caller never asked for, on the one process that is supposed
    // to stay up.
    if is_loaded(service) && std::fs::read_to_string(&path).is_ok_and(|on_disk| on_disk == rendered)
    {
        return Ok(path);
    }

    // Unload an existing instance first. A plist rewritten underneath a loaded
    // service leaves launchd running the old argv until something reloads it,
    // which looks exactly like the install not having worked.
    if is_loaded(service) {
        let _ = bootout(service);
    }

    std::fs::write(&path, rendered).map_err(|e| format!("write {}: {e}", path.display()))?;

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
fn bootout(service: Service) -> Result<(), String> {
    let out = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{}/{}", gui_domain(), service.label())])
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
pub fn uninstall(service: Service) -> Result<(bool, Option<String>), String> {
    let path = plist_path(service);
    let existed = path.exists();
    let loaded = is_loaded(service);

    let note = if loaded { bootout(service).err() } else { None };

    if existed {
        std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    }
    Ok((existed || loaded, note))
}

// ────────────────────────────────────────────────────────────────────────────
// Linux: systemd --user
// ────────────────────────────────────────────────────────────────────────────
//
// Mirrors the launchd side deliberately: a pure renderer with tests, then the
// IO on top. #546 — this used to print a recipe and install nothing, which is
// documentation rather than a daemon, on exactly the machine most likely to be
// left running.

/// Where the unit file lives.
#[cfg(not(target_os = "macos"))]
pub fn unit_path(service: Service) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    unit_path_in(Path::new(&home), service)
}

/// Explicit home, so tests never read the ambient one.
#[cfg(not(target_os = "macos"))]
pub fn unit_path_in(home: &Path, service: Service) -> PathBuf {
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(service.unit_name())
}

/// Render the unit.
///
/// `Restart=always` with a delay is the `KeepAlive` equivalent. `RestartSec`
/// matters more than it looks: a relay that cannot bind its port would
/// otherwise respawn in a tight loop and systemd would give up and mark the
/// unit failed, which reads as "the agent is broken" rather than "the port is
/// taken".
///
/// Logs go to the journal rather than to files. The launchd side writes files
/// because launchd has no journal; duplicating that here would mean two places
/// to look on one platform.
#[cfg(not(target_os = "macos"))]
pub fn render_unit(binary: &Path, cfg: &ServiceConfig) -> String {
    let argv = cfg.argv(binary);
    // `argv` is already shell-free — systemd splits ExecStart on whitespace, so
    // a path containing a space has to be quoted here even though the plist
    // needed no such thing.
    let exec: Vec<String> = argv
        .iter()
        .map(|a| {
            if a.contains(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect();
    format!(
        "[Unit]\n\
         Description={}\n\
         Documentation=https://github.com/mercurialsolo/claudectl\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        cfg.service().description(),
        exec.join(" ")
    )
}

#[cfg(not(target_os = "macos"))]
fn systemctl(args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl --user {}: {e}", args.join(" ")))
}

#[cfg(not(target_os = "macos"))]
pub fn is_loaded(service: Service) -> bool {
    systemctl(&["is-enabled", service.unit_name()])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn install(cfg: &ServiceConfig) -> Result<PathBuf, String> {
    let binary = resolve_binary()?;
    let service = cfg.service();
    let path = unit_path(service);
    let dir = path
        .parent()
        .ok_or_else(|| "unit path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    let rendered = render_unit(&binary, cfg);
    // See the launchd side: an unchanged unit that is already enabled is left
    // strictly alone, so installing one service never restarts the other.
    if is_loaded(service) && std::fs::read_to_string(&path).is_ok_and(|on_disk| on_disk == rendered)
    {
        return Ok(path);
    }

    std::fs::write(&path, rendered).map_err(|e| format!("write {}: {e}", path.display()))?;

    // A unit rewritten under a running service keeps the old ExecStart until
    // something reloads it — the same failure the launchd side unloads to avoid.
    let reload = systemctl(&["daemon-reload"])?;
    if !reload.status.success() {
        return Err(format!(
            "systemctl --user daemon-reload failed: {}",
            String::from_utf8_lossy(&reload.stderr).trim()
        ));
    }
    let enable = systemctl(&["enable", "--now", service.unit_name()])?;
    if !enable.status.success() {
        return Err(format!(
            "systemctl --user enable --now {} failed: {}",
            service.unit_name(),
            String::from_utf8_lossy(&enable.stderr).trim()
        ));
    }

    // Without lingering, the user manager stops at logout and takes the
    // service with it — which is the whole point of installing one. It needs
    // policy permission, so a refusal is reported rather than failing an
    // install that otherwise worked.
    if let Err(hint) = enable_linger() {
        eprintln!("warning: {hint}");
    }
    Ok(path)
}

/// `loginctl enable-linger`, the systemd equivalent of a launchd agent
/// outliving the login session.
#[cfg(not(target_os = "macos"))]
fn enable_linger() -> Result<(), String> {
    let user = std::env::var("USER").unwrap_or_default();
    let out = std::process::Command::new("loginctl")
        .args(["enable-linger", &user])
        .output()
        .map_err(|e| format!("loginctl enable-linger: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "could not enable lingering, so the service will stop at logout. \
         Run: loginctl enable-linger {user}   ({})",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall(service: Service) -> Result<(bool, Option<String>), String> {
    let path = unit_path(service);
    let existed = path.exists();
    let mut note = None;
    if existed {
        let disable = systemctl(&["disable", "--now", service.unit_name()])?;
        if !disable.status.success() {
            note = Some(format!(
                "systemctl --user disable failed: {}",
                String::from_utf8_lossy(&disable.stderr).trim()
            ));
        }
        std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
        let _ = systemctl(&["daemon-reload"]);
    }
    Ok((existed, note))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed home, so no test here reads the ambient `HOME` — see
    /// [`plist_path_in`].
    fn home() -> PathBuf {
        PathBuf::from("/Users/testuser")
    }

    #[test]
    fn argv_starts_with_the_binary_and_the_subcommand() {
        let cfg = AgentConfig::default();
        let argv = cfg.argv(Path::new("/opt/homebrew/bin/claudectl"));
        assert_eq!(argv, vec!["/opt/homebrew/bin/claudectl", "relay", "serve"]);
    }

    #[test]
    fn an_explicit_default_port_is_kept_in_the_agent_command() {
        let cfg = AgentConfig {
            port: Some(9847),
            ..AgentConfig::default()
        };
        assert_eq!(
            cfg.argv(Path::new("/opt/homebrew/bin/claudectl")),
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
            port: Some(9000),
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

    /// Without this a relay under launchd can't find `claude` to run
    /// delegated work, and reports it missing so peers refuse to delegate.
    #[test]
    fn the_plist_gives_the_agent_a_path_with_the_usual_tool_dirs() {
        let xml = render_plist_in(
            Path::new("/opt/homebrew/bin/claudectl"),
            &ServiceConfig::Relay(AgentConfig::default()),
            &home(),
        );
        assert!(xml.contains("<key>EnvironmentVariables</key>"), "{xml}");
        let path = agent_path(&home());
        assert!(xml.contains(&format!("<string>{path}</string>")), "{xml}");
        for dir in [
            "/.local/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
        ] {
            assert!(path.contains(dir), "{path} lacks {dir}");
        }
        // The plist is still valid XML plist structure for launchd.
        assert!(xml.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn the_plist_carries_the_label_argv_and_keepalive() {
        let xml = render_plist_in(
            Path::new("/opt/homebrew/bin/claudectl"),
            &ServiceConfig::Relay(AgentConfig::default()),
            &home(),
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
        assert!(!xml.contains("<string>--port</string>"), "{xml}");
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
        let xml = render_plist_in(
            Path::new("/Users/a b/bin/claudectl"),
            &ServiceConfig::Relay(AgentConfig::default()),
            &home(),
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
        let xml = render_plist_in(
            Path::new("/Users/a&b/bin/claudectl"),
            &ServiceConfig::Relay(AgentConfig::default()),
            &home(),
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
        let xml = render_plist_in(
            Path::new("/x/claudectl"),
            &ServiceConfig::Relay(cfg),
            &home(),
        );
        assert!(
            xml.contains("<string>sekrit</string>"),
            "the token has to be in the plist for the agent to use it"
        );
    }

    #[test]
    fn the_label_is_used_for_both_the_plist_name_and_the_service() {
        assert!(
            plist_path_in(&home(), Service::Relay)
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
        let (out, err) = log_paths_in(&home(), Service::Relay);
        assert!(out.ends_with("agent.out.log"), "{out:?}");
        assert!(err.ends_with("agent.err.log"), "{err:?}");
        assert_eq!(out.parent(), err.parent());
        assert!(
            out.parent().unwrap().ends_with(".claudectl/relay"),
            "{out:?}"
        );
    }

    /// #546 gap 2: the headless process carries the brain *and* the supervisor
    /// reconciler, so this argv is the whole orchestrating half of the stack.
    #[test]
    fn the_headless_argv_runs_the_brain_and_supervisor_process() {
        let argv = HeadlessConfig::default().argv(Path::new("/opt/homebrew/bin/claudectl"));
        assert_eq!(argv, vec!["/opt/homebrew/bin/claudectl", "--headless"]);
    }

    /// Neither flag is on by default: a login-installed service that executes
    /// brain decisions unasked is opt-in.
    #[test]
    fn brain_and_auto_run_are_opt_in() {
        let bare = HeadlessConfig::default().argv(Path::new("/x/claudectl"));
        assert!(!bare.iter().any(|a| a == "--brain"), "{bare:?}");
        assert!(!bare.iter().any(|a| a == "--auto-run"), "{bare:?}");

        let brain = HeadlessConfig {
            brain: true,
            auto_run: false,
        }
        .argv(Path::new("/x/claudectl"));
        assert_eq!(brain, vec!["/x/claudectl", "--headless", "--brain"]);
    }

    /// `--auto-run` alone is accepted by clap but does nothing without the
    /// brain that produces the suggestions, so it implies `--brain` rather than
    /// installing a service that ignores what it was asked for.
    #[test]
    fn auto_run_implies_brain() {
        let argv = HeadlessConfig {
            brain: false,
            auto_run: true,
        }
        .argv(Path::new("/x/claudectl"));
        assert_eq!(
            argv,
            vec!["/x/claudectl", "--headless", "--brain", "--auto-run"]
        );
    }

    #[test]
    fn the_headless_plist_carries_its_own_label_and_argv() {
        let xml = render_plist_in(
            Path::new("/opt/homebrew/bin/claudectl"),
            &ServiceConfig::Headless(HeadlessConfig::default()),
            &home(),
        );
        assert!(
            xml.contains("<string>io.claudectl.headless</string>"),
            "{xml}"
        );
        assert!(xml.contains("<string>--headless</string>"), "{xml}");
        // And none of the relay's argv leaked into it.
        assert!(!xml.contains("<string>serve</string>"), "{xml}");
    }

    /// The two services must never share a plist, a unit name, a label or a
    /// log directory. Any collision means installing one silently uninstalls
    /// or overwrites the other.
    #[test]
    fn the_two_services_share_no_paths() {
        let mut labels = vec![];
        let mut dirs = vec![];
        let mut units = vec![];
        for svc in Service::ALL {
            labels.push(svc.label());
            units.push(svc.unit_name());
            let (out, err) = log_paths_in(&home(), svc);
            assert_eq!(out.parent(), err.parent());
            dirs.push(out.parent().unwrap().to_path_buf());
        }
        labels.sort_unstable();
        labels.dedup();
        units.sort_unstable();
        units.dedup();
        dirs.sort();
        dirs.dedup();
        assert_eq!(labels.len(), Service::ALL.len(), "labels collide");
        assert_eq!(units.len(), Service::ALL.len(), "unit names collide");
        assert_eq!(dirs.len(), Service::ALL.len(), "log directories collide");
    }

    /// The headless heartbeat has to land in the same directory as that
    /// service's logs — one service, one place to look.
    #[test]
    fn the_heartbeat_sits_with_the_headless_logs() {
        let (out, _) = log_paths_in(&home(), Service::Headless);
        let beat = claudectl_core::heartbeat::path_in(&home());
        assert_eq!(
            beat.parent(),
            out.parent(),
            "heartbeat {beat:?} should sit beside {out:?}"
        );
    }

    // The systemd half, rendered and checked without installing anything —
    // neither CI nor a container can meaningfully run `systemd --user`, so the
    // unit text is what gets asserted (#546). Same approach as the plist above.
    #[cfg(not(target_os = "macos"))]
    mod systemd {
        use super::*;

        fn linux_home() -> PathBuf {
            PathBuf::from("/home/testuser")
        }

        #[test]
        fn the_unit_lives_where_systemd_user_looks_for_it() {
            let path = unit_path_in(&linux_home(), Service::Relay);
            assert_eq!(
                path,
                PathBuf::from("/home/testuser/.config/systemd/user/claudectl-relay.service")
            );
            let headless = unit_path_in(&linux_home(), Service::Headless);
            assert_eq!(
                headless,
                PathBuf::from("/home/testuser/.config/systemd/user/claudectl-headless.service")
            );
        }

        #[test]
        fn the_unit_restarts_and_installs_into_the_default_target() {
            let unit = render_unit(
                Path::new("/usr/local/bin/claudectl"),
                &ServiceConfig::Relay(AgentConfig::default()),
            );
            assert!(unit.contains("Restart=always"), "{unit}");
            // Without a delay a relay that cannot bind its port respawns in a
            // tight loop until systemd gives up and marks the unit failed,
            // which reads as a broken agent rather than a taken port.
            assert!(unit.contains("RestartSec="), "{unit}");
            assert!(unit.contains("WantedBy=default.target"), "{unit}");
            assert!(
                unit.contains("ExecStart=/usr/local/bin/claudectl relay serve"),
                "{unit}"
            );
            assert!(!unit.contains("--port 9847"), "{unit}");
        }

        /// systemd splits `ExecStart` on whitespace, where a launchd plist takes
        /// an argv array and needs no quoting at all. A path with a space is
        /// the case that difference breaks.
        #[test]
        fn a_binary_path_with_a_space_is_quoted() {
            let unit = render_unit(
                Path::new("/home/testuser/My Tools/claudectl"),
                &ServiceConfig::Relay(AgentConfig::default()),
            );
            assert!(
                unit.contains("ExecStart=\"/home/testuser/My Tools/claudectl\" relay serve"),
                "{unit}"
            );
        }

        #[test]
        fn optional_config_reaches_the_unit() {
            let cfg = AgentConfig {
                port: Some(9900),
                http_port: Some(9876),
                ..AgentConfig::default()
            };
            let unit = render_unit(Path::new("/usr/bin/claudectl"), &ServiceConfig::Relay(cfg));
            assert!(unit.contains("--port 9900"), "{unit}");
            assert!(unit.contains("--http-port 9876"), "{unit}");
        }

        #[test]
        fn the_unit_waits_for_the_network() {
            // A relay started before the network is up dials every peer once,
            // fails, and — before #543 — forgot them all.
            let unit = render_unit(
                Path::new("/usr/bin/claudectl"),
                &ServiceConfig::Relay(AgentConfig::default()),
            );
            assert!(unit.contains("After=network-online.target"), "{unit}");
        }

        /// The headless unit is a separate unit with its own Description and
        /// ExecStart — not the relay's with a different name.
        #[test]
        fn the_headless_unit_runs_the_headless_process() {
            let unit = render_unit(
                Path::new("/usr/bin/claudectl"),
                &ServiceConfig::Headless(HeadlessConfig {
                    brain: true,
                    auto_run: false,
                }),
            );
            assert!(
                unit.contains("ExecStart=/usr/bin/claudectl --headless --brain"),
                "{unit}"
            );
            assert!(
                unit.contains("Description=claudectl headless (brain + supervisor)"),
                "{unit}"
            );
            assert!(!unit.contains("relay serve"), "{unit}");
            assert!(unit.contains("Restart=always"), "{unit}");
            assert!(unit.contains("WantedBy=default.target"), "{unit}");
        }
    }
}

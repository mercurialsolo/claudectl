//! One-shot orphan reaper for in-sandbox `claude` processes.
//!
//! When a user closes an iTerm2 tab whose `claude` runs inside the
//! `linera-agent` agent-sandbox, Docker exec doesn't propagate SIGHUP to the
//! container-side exec target (moby/moby#9098). The in-VM `claude` survives,
//! its sidecar (`{pid}.terminal.json`) keeps pointing at a host TTY that is
//! no longer attached, and the row sits Idle forever.
//!
//! The reaper detects this by diffing two sets:
//! - Open set: host-side TTYs of currently-running `sbx exec ... <sandbox>`
//!   processes, extracted from `SANDBOX_HOST_TTY=/dev/ttysNNN` in argv.
//! - Sandbox set: per-PID sidecars under the sandbox sessions dir, each
//!   carrying its `host_tty` and a kill(0) liveness check.
//!
//! Any sandbox PID whose sidecar `host_tty` is not in the open set AND whose
//! process is alive is sent SIGHUP. Sidecars whose PID is dead are swept off
//! disk along with their `{pid}.json` companion.
//!
//! Wired as `claudectl --reap-orphans` in `main.rs`. Add `--dry-run` to
//! preview without killing or removing. Also exposes `--install-reaper` /
//! `--uninstall-reaper` to wire a launchd job on macOS.
//!
//! ## Environment overrides
//!
//! - `CLAUDECTL_SANDBOX_NAME` — sbx sandbox to scan. If unset, the reaper
//!   runs `sbx ls` once and uses the single running sandbox if there is
//!   exactly one; otherwise falls back to `linera-agent`.
//! - `CLAUDECTL_SANDBOX_SESSIONS_DIR` — in-sandbox path holding the per-PID
//!   `{pid}.terminal.json` sidecars. Default `/var/lib/sandbox-sessions`.
//!
//! Both env vars are read on every invocation; an empty value falls back to
//! the default (treat empty as unset).

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// Process-wide cache for the auto-detected sandbox name. `None` means
/// `sbx ls` could not pick exactly one running sandbox; the resolver then
/// falls back to `DEFAULT_SANDBOX_NAME`. Populated lazily on first call.
static AUTO_SANDBOX_NAME: OnceLock<Option<String>> = OnceLock::new();

const DEFAULT_SANDBOX_NAME: &str = "linera-agent";

fn sandbox_name() -> String {
    let env = std::env::var("CLAUDECTL_SANDBOX_NAME").ok();
    let auto = AUTO_SANDBOX_NAME.get_or_init(detect_running_sandbox_name);
    resolve_sandbox_name(env.as_deref(), auto.as_deref(), DEFAULT_SANDBOX_NAME)
}

/// Pure resolver. Picks the first non-empty source: explicit env override
/// → auto-detected name → default. Tests target this directly so they
/// don't have to mutate process-global env state (which races other tests
/// under parallel cargo test).
pub(crate) fn resolve_sandbox_name(
    env_override: Option<&str>,
    auto_detected: Option<&str>,
    default: &str,
) -> String {
    if let Some(v) = env_override {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(v) = auto_detected {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    default.to_string()
}

/// Shell out to `sbx ls` once and try to identify a unique running sandbox.
/// Returns `None` for any failure (binary missing, non-zero exit, parse miss),
/// in which case the caller falls back to `DEFAULT_SANDBOX_NAME`.
fn detect_running_sandbox_name() -> Option<String> {
    let output = Command::new("sbx").arg("ls").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_sbx_ls_for_single_running_sandbox(&text)
}

/// Pure parser: given `sbx ls` stdout, return the name of the unique running
/// sandbox if and only if there is exactly one. Anything else (zero, multiple,
/// stopped-only, malformed) returns `None` so the caller can fall back to the
/// default. We require the running condition because a stopped sandbox is no
/// help to the reaper — it has no in-VM processes to scan.
pub(crate) fn parse_sbx_ls_for_single_running_sandbox(stdout: &str) -> Option<String> {
    // Header is the first non-empty line whose first whitespace-delimited
    // token equals "SANDBOX". Non-headers are body rows: `name agent status
    // ports workspace`. We only count rows whose status column is "running".
    let mut running: Vec<String> = Vec::new();
    let mut saw_header = false;
    for raw in stdout.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut cols = line.split_whitespace();
        let Some(first) = cols.next() else {
            continue;
        };
        if !saw_header {
            if first.eq_ignore_ascii_case("SANDBOX") {
                saw_header = true;
            }
            // Skip everything until we see the header. Anything before it
            // (banners, warnings) isn't a sandbox row.
            continue;
        }
        // Body row. Columns are: SANDBOX AGENT STATUS [PORTS] WORKSPACE.
        let _agent = cols.next();
        let Some(status) = cols.next() else {
            continue;
        };
        if status == "running" {
            running.push(first.to_string());
        }
    }
    if running.len() == 1 {
        return running.pop();
    }
    None
}

fn sandbox_sessions_dir() -> String {
    resolve_or_default(
        std::env::var("CLAUDECTL_SANDBOX_SESSIONS_DIR")
            .ok()
            .as_deref(),
        "/var/lib/sandbox-sessions",
    )
}

/// Treat both "unset" and "set-but-empty" as fallback to default. Empty env
/// values almost always come from a typo or an unquoted shell expansion;
/// silently using `""` would make `sbx exec ""` fail with a confusing error.
fn resolve_or_default(value: Option<&str>, default: &str) -> String {
    match value {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => default.to_string(),
    }
}

/// Sidecar entry parsed from inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSidecar {
    pub pid: u32,
    pub host_tty: String,
    /// True if `kill -0 <pid>` succeeded inside the sandbox at scan time.
    pub alive: bool,
    /// Optional human label from `{pid}.json` (e.g. session name). May be
    /// empty when the companion `{pid}.json` is missing.
    pub name: String,
}

/// Result of orphan-set computation. Two disjoint groups: live processes to
/// SIGHUP, and dead sidecars whose disk artefacts should be swept.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OrphanPlan {
    /// Sidecars whose `host_tty` is not in the open set AND PID is alive.
    /// These get SIGHUP.
    pub kill: Vec<SandboxSidecar>,
    /// Sidecars whose PID is dead. Just clean their `{pid}.terminal.json`
    /// (and any matching `{pid}.json`).
    pub sweep: Vec<SandboxSidecar>,
}

/// Cap on the number of alive orphans the auto-reaper will kill in a single
/// pass. A spike past this is more likely a parser regression or env
/// corruption than a genuine surge in orphans.
pub const MAX_KILLS_PER_PASS: usize = 10;

/// Decision returned by `decide_action`. Pure; the I/O wrapper in `run()`
/// translates this into stderr/stdout output and `sbx exec` calls.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Refuse to act. The string is the human-readable reason logged to
    /// stderr.
    Skip(String),
    /// Proceed with this plan. May be empty (no orphans).
    Execute(OrphanPlan),
}

/// Pure decision function. Given the host's open TTY set and the sandbox's
/// sidecar list, decide whether to act and how. Centralises the safety
/// guards so they can be unit-tested without mocking I/O.
pub fn decide_action(open_ttys: &HashSet<String>, sidecars: &[SandboxSidecar]) -> Action {
    let plan = compute_orphans(open_ttys, sidecars);
    let alive_count = sidecars.iter().filter(|s| s.alive).count();

    // Guard 1: 0 open host TTYs but alive sandbox claudes exist => host
    // scan probably failed (transient ps glitch, sbx daemon restart).
    // Refusing is conservative; the next pass will pick up real orphans
    // once the host scan succeeds.
    if open_ttys.is_empty() && alive_count > 0 {
        return Action::Skip(format!(
            "0 open host TTYs but {alive_count} alive sandbox claudes — refusing to act \
             (probable host scan failure). Re-run --dry-run to investigate."
        ));
    }

    // Guard 2: kill set exceeds the safety cap. More likely a bug than a
    // real surge.
    if plan.kill.len() > MAX_KILLS_PER_PASS {
        return Action::Skip(format!(
            "{} kill candidates exceeds safety cap ({}); refusing to act. \
             Run `claudectl --reap-orphans --dry-run` to inspect.",
            plan.kill.len(),
            MAX_KILLS_PER_PASS
        ));
    }

    Action::Execute(plan)
}

/// Pure orphan-detection. No I/O.
///
/// Rules:
/// - sidecar PID is dead → sweep (disk-only orphan).
/// - sidecar PID is alive AND host_tty is in `open_ttys` → current, keep.
/// - sidecar PID is alive AND host_tty is NOT in `open_ttys` → kill.
///
/// In the TTY-reuse case (two sidecars on the same host_tty), both go through
/// these rules independently; the dead one ends up in `sweep`, the alive one
/// in either `kill` or "current" depending on whether the TTY is still open.
pub fn compute_orphans(open_ttys: &HashSet<String>, sidecars: &[SandboxSidecar]) -> OrphanPlan {
    let mut plan = OrphanPlan::default();
    for sc in sidecars {
        if !sc.alive {
            plan.sweep.push(sc.clone());
            continue;
        }
        if !open_ttys.contains(&sc.host_tty) {
            plan.kill.push(sc.clone());
        }
    }
    plan
}

// ── Tick-skip cache ───────────────────────────────────────────────────────
//
// Most ticks have no host-side TTY changes since the previous tick → no new
// orphans can have appeared → previous tick's full pass already handled
// everything. Skipping the in-sandbox scan saves the sbx exec round-trip
// (~2.6s startup overhead per call), which dominates steady-state cost when
// the timer fires every minute.
//
// Cache shape: `<XDG_CACHE_HOME or ~/.cache>/claudectl/reaper-last-state`.
// File body  : sorted, newline-joined open SANDBOX_HOST_TTY values.
// Freshness  : the file's own mtime, ceilinged at MAX_CACHE_AGE.
//
// Cache is written ONLY after a successful scan + decide_action accepted
// the plan. Skipped on dry-run, on safety-guard skips, and on error paths,
// so a transient failure or preview pass doesn't suppress the next real
// tick.

/// Hard ceiling on cache freshness. Past this, the next tick must do a
/// full pass even if the host TTY set hasn't changed — catches in-sandbox
/// `claude` deaths (panic, oom-kill) that wouldn't show up as a host TTY
/// transition.
const MAX_CACHE_AGE: Duration = Duration::from_secs(30 * 60);

/// What the cache decides we should do this tick.
#[derive(Debug, PartialEq, Eq)]
pub enum CacheAction {
    /// State matches the cached snapshot AND the cache is within the
    /// freshness ceiling — skip the in-sandbox scan entirely.
    Skip,
    /// State differs, cache is stale, or there is no cache yet — run the
    /// full pass and (on success) refresh the cache.
    FullPass,
}

/// Stable string representation of the open-TTY set. Sorted so the same
/// set always serialises to the same body (HashSet iteration order isn't
/// stable across runs, and we need byte-exact equality for the cache).
pub fn state_string(open_ttys: &HashSet<String>) -> String {
    let mut sorted: Vec<&str> = open_ttys.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join("\n")
}

/// Pure decision: does the current tick's open-TTY state plus the cached
/// state warrant skipping the in-sandbox scan? Skips only when the state
/// matches AND the cache age is within `max_age`.
pub fn decide_cache_action(
    prev_state: Option<&str>,
    new_state: &str,
    cache_age: Option<Duration>,
    max_age: Duration,
) -> CacheAction {
    match (prev_state, cache_age) {
        (Some(prev), Some(age)) if prev == new_state && age <= max_age => CacheAction::Skip,
        _ => CacheAction::FullPass,
    }
}

fn cache_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().ok().map(|h| h.join(".cache")))?;
    Some(dir.join("claudectl").join("reaper-last-state"))
}

fn read_cache_state(path: &Path) -> Option<(String, Duration)> {
    let body = std::fs::read_to_string(path).ok()?;
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = std::time::SystemTime::now().duration_since(mtime).ok()?;
    Some((body, age))
}

fn write_cache_state(path: &Path, state: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, state)
}

/// Entry point for `claudectl --reap-orphans`. Returns `Ok(())` even when no
/// sandboxes/sbx are present — the reaper is a no-op fallback.
pub fn run(dry_run: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if !sbx_available() {
        writeln!(
            io::stderr(),
            "reaper: `sbx` not in PATH; nothing to do (no sandboxes on this host)",
        )?;
        return Ok(());
    }

    let open_ttys = match scan_host_open_ttys() {
        Ok(set) => set,
        Err(e) => {
            writeln!(io::stderr(), "reaper: host ps scan failed: {e}")?;
            return Ok(());
        }
    };

    let new_state = state_string(&open_ttys);
    let cache = cache_path();
    if !dry_run {
        if let Some(path) = &cache {
            let prev = read_cache_state(path);
            let action = decide_cache_action(
                prev.as_ref().map(|(s, _)| s.as_str()),
                &new_state,
                prev.as_ref().map(|(_, age)| *age),
                MAX_CACHE_AGE,
            );
            if matches!(action, CacheAction::Skip) {
                return Ok(());
            }
        }
    }

    let sidecars = match scan_sandbox_sidecars() {
        Ok(list) => list,
        Err(e) => {
            writeln!(io::stderr(), "reaper: sandbox sidecar scan failed: {e}")?;
            return Ok(());
        }
    };

    let plan = match decide_action(&open_ttys, &sidecars) {
        Action::Skip(reason) => {
            writeln!(io::stderr(), "reaper: {reason}")?;
            return Ok(());
        }
        Action::Execute(plan) => plan,
    };

    // Refresh the cache as soon as both observations succeeded and the
    // safety guard accepted the plan. Even if the kill/sweep below errors
    // out, the cache reflects "we've seen this state and decided to act";
    // the MAX_CACHE_AGE ceiling bounds how long a survived orphan can
    // linger before the next tick re-checks anyway.
    if !dry_run {
        if let Some(path) = &cache {
            if let Err(e) = write_cache_state(path, &new_state) {
                writeln!(io::stderr(), "reaper: cache write failed: {e}")?;
            }
        }
    }

    if plan.kill.is_empty() && plan.sweep.is_empty() {
        writeln!(out, "no orphans")?;
        return Ok(());
    }

    for orphan in &plan.kill {
        writeln!(
            out,
            "{}reaped: pid={} tty={} name={}",
            if dry_run { "[dry-run] " } else { "" },
            orphan.pid,
            orphan.host_tty,
            if orphan.name.is_empty() {
                "?"
            } else {
                orphan.name.as_str()
            },
        )?;
    }
    for orphan in &plan.sweep {
        writeln!(
            out,
            "{}swept (dead): pid={} tty={} name={}",
            if dry_run { "[dry-run] " } else { "" },
            orphan.pid,
            orphan.host_tty,
            if orphan.name.is_empty() {
                "?"
            } else {
                orphan.name.as_str()
            },
        )?;
    }

    if dry_run {
        return Ok(());
    }

    // Apply the plan in ONE sbx exec instead of two. The dead pids get
    // their `{pid}.json` and `{pid}.terminal.json` removed; the alive pids
    // get SIGHUP. Alive pids' sidecars are NOT swept this pass — if the
    // HUP fails or the process ignores it, the sidecar must remain so the
    // next reaper tick re-detects it.
    let dead_pids: Vec<u32> = plan.sweep.iter().map(|o| o.pid).collect();
    let alive_pids: Vec<u32> = plan.kill.iter().map(|o| o.pid).collect();
    apply_plan(&dead_pids, &alive_pids)?;

    Ok(())
}

fn sbx_available() -> bool {
    // `sbx --help` exits 0 when the binary is in PATH and runnable. We use
    // it instead of `--version` (which sbx doesn't accept) because we just
    // want a "is this binary present and executable" probe.
    Command::new("sbx")
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Parse host `ps -ax -o pid,command` for `sbx exec ... linera-agent` lines
/// and pull `SANDBOX_HOST_TTY=/dev/ttysNNN` out of each.
fn scan_host_open_ttys() -> io::Result<HashSet<String>> {
    let output = Command::new("ps")
        .args(["-ax", "-o", "pid,command"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "ps exited non-zero: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(extract_open_ttys(&text, &sandbox_name()))
}

/// Pure parser: takes `ps -ax -o pid,command` output, returns the set of
/// `SANDBOX_HOST_TTY` values from `sbx exec ... <sandbox>` lines.
fn extract_open_ttys(ps_output: &str, sandbox: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for line in ps_output.lines() {
        if !line.contains("sbx exec") {
            continue;
        }
        if !line.contains(sandbox) {
            continue;
        }
        if let Some(tty) = line
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("SANDBOX_HOST_TTY="))
        {
            set.insert(tty.to_string());
        }
    }
    set
}

/// Run a single `sbx exec linera-agent bash -c '...'` that walks the sandbox's
/// sessions dir and prints one tab-separated line per `{pid}.terminal.json`:
/// `pid<TAB>host_tty<TAB>alive<TAB>name`.
fn scan_sandbox_sidecars() -> io::Result<Vec<SandboxSidecar>> {
    // The script enumerates every {pid}.terminal.json, extracts host_tty,
    // checks kill -0 on the pid (read from the matching {pid}.json's "pid"
    // field, falling back to the sidecar filename's pid stem), and pulls a
    // best-effort name from {pid}.json's "name" field if present.
    //
    // No jq dependency in the sandbox — use bash + grep + sed. Each output
    // line is `<pid>\t<host_tty>\t<alive>\t<name>` where alive is 0 or 1.
    let dir = sandbox_sessions_dir();
    let script = format!(
        r#"
set -u
DIR={dir}
shopt -s nullglob
for sc in "$DIR"/*.terminal.json; do
  fname="${{sc##*/}}"
  pid="${{fname%.terminal.json}}"
  host_tty=$(grep -o '"host_tty"[[:space:]]*:[[:space:]]*"[^"]*"' "$sc" 2>/dev/null \
    | sed 's/.*"host_tty"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' | head -n1)
  [ -z "$host_tty" ] && continue
  if kill -0 "$pid" 2>/dev/null; then alive=1; else alive=0; fi
  name=""
  if [ -f "$DIR/$pid.json" ]; then
    name=$(grep -o '"name"[[:space:]]*:[[:space:]]*"[^"]*"' "$DIR/$pid.json" 2>/dev/null \
      | sed 's/.*"name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' | head -n1)
  fi
  printf '%s\t%s\t%s\t%s\n' "$pid" "$host_tty" "$alive" "$name"
done
"#
    );

    let output = Command::new("sbx")
        .args(["exec", &sandbox_name(), "bash", "-c", &script])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "sbx exec sidecar-scan failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_sandbox_sidecars(&text))
}

/// Pure parser for the tab-separated sandbox-side scan output.
fn parse_sandbox_sidecars(text: &str) -> Vec<SandboxSidecar> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let Some(pid_s) = parts.next() else { continue };
        let Some(host_tty) = parts.next() else {
            continue;
        };
        let Some(alive_s) = parts.next() else {
            continue;
        };
        let name = parts.next().unwrap_or("").to_string();
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        let alive = alive_s == "1";
        out.push(SandboxSidecar {
            pid,
            host_tty: host_tty.to_string(),
            alive,
            name,
        });
    }
    out
}

/// Apply the reaper plan in ONE `sbx exec`: sweep dead-PID sidecar files
/// then SIGHUP alive-PID orphans. No-op when both lists are empty.
///
/// The script gets the dead-pid count as its first positional, lets the
/// shell shift it off, and uses the rest of the count as sweep targets;
/// anything still on the argv after that is the kill set. This avoids a
/// second `sbx exec` round-trip (each is on the order of seconds of
/// startup overhead from inside the sbx wrapper).
///
/// Errors:
/// - `Command::output` failure (sbx binary missing/unspawnable) is
///   propagated — same as the prior split implementation.
/// - Non-zero bash exit (rm trips, kill returns non-zero) is logged to
///   stderr but does NOT fail the run. `rm -f` swallows its own errors
///   already; a non-zero exit here usually means kill couldn't deliver
///   the signal, which is a soft warning at worst — the next reaper pass
///   will re-detect the still-alive orphan and retry.
fn apply_plan(dead_pids: &[u32], alive_pids: &[u32]) -> io::Result<()> {
    if dead_pids.is_empty() && alive_pids.is_empty() {
        return Ok(());
    }

    let dir = sandbox_sessions_dir();
    let script = format!(
        r#"
set -u
DIR={dir}
N_DEAD=$1; shift
i=0
while [ "$i" -lt "$N_DEAD" ]; do
  pid=$1; shift
  rm -f "$DIR/$pid.json" "$DIR/$pid.terminal.json" 2>/dev/null || true
  i=$((i+1))
done
if [ "$#" -gt 0 ]; then
  kill -HUP "$@" 2>&1
fi
"#
    );

    let mut all_args: Vec<String> = Vec::with_capacity(1 + dead_pids.len() + alive_pids.len());
    all_args.push(dead_pids.len().to_string());
    all_args.extend(dead_pids.iter().map(u32::to_string));
    all_args.extend(alive_pids.iter().map(u32::to_string));

    let name = sandbox_name();
    let mut cmd = Command::new("sbx");
    cmd.args(["exec", &name, "bash", "-c", &script, "--"]);
    cmd.args(&all_args);
    match cmd.output() {
        Ok(o) if !o.status.success() => {
            writeln!(
                io::stderr(),
                "reaper: apply_plan returned non-zero: {}",
                String::from_utf8_lossy(&o.stderr)
            )?;
            Ok(())
        }
        Err(e) => Err(e),
        Ok(_) => Ok(()),
    }
}

// ── Install/uninstall (macOS launchd + Linux systemd-user) ────────────────

// Used by macOS install/uninstall and by the plist-renderer tests; absent
// in non-test Linux builds so the binary compiles dead-code-clean there.
#[cfg(any(target_os = "macos", test))]
const LAUNCH_AGENT_LABEL: &str = "linera.claudectl-reaper";

// Used by Linux install/uninstall and by the systemd unit-renderer tests.
#[cfg(any(target_os = "linux", test))]
const SYSTEMD_UNIT_BASENAME: &str = "claudectl-reaper";

/// Hard floor: anything below this hammers `sbx exec` faster than a real
/// reaper pass completes (the in-sandbox bash + grep + kill pipeline takes
/// a second or two). Hard ceiling: anything above an hour means the user
/// is closing tabs faster than the reaper can find them.
pub const MIN_INTERVAL_SECONDS: u64 = 10;
pub const MAX_INTERVAL_SECONDS: u64 = 3600;

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set; cannot locate user home"))
}

#[cfg(target_os = "macos")]
fn plist_path() -> io::Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn err_log_path() -> io::Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("Logs")
        .join("claudectl-reaper.err.log"))
}

#[cfg(target_os = "linux")]
fn systemd_user_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".config").join("systemd").join("user"))
}

#[cfg(target_os = "linux")]
fn systemd_service_path() -> io::Result<PathBuf> {
    Ok(systemd_user_dir()?.join(format!("{SYSTEMD_UNIT_BASENAME}.service")))
}

#[cfg(target_os = "linux")]
fn systemd_timer_path() -> io::Result<PathBuf> {
    Ok(systemd_user_dir()?.join(format!("{SYSTEMD_UNIT_BASENAME}.timer")))
}

#[cfg(target_os = "linux")]
fn linux_err_log_path() -> io::Result<PathBuf> {
    // Per XDG Base Directory spec: state goes under $XDG_STATE_HOME, default
    // ~/.local/state. systemd resolves `%h` to the user's HOME at runtime.
    Ok(home_dir()?
        .join(".local")
        .join("state")
        .join("claudectl-reaper.err.log"))
}

/// Pure plist renderer. The XML body is byte-for-byte equivalent to the
/// hand-written plist that's been driving the auto-reaper on Andre's box —
/// changing whitespace here will break the byte-equivalence verification.
#[cfg(any(target_os = "macos", test))]
pub fn build_plist(exe_path: &Path, interval_seconds: u64, err_log: &Path, home: &Path) -> String {
    let exe = exe_path.display();
    let err = err_log.display();
    let home_disp = home.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCH_AGENT_LABEL}</string>

    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--reap-orphans</string>
    </array>

    <key>StartInterval</key>
    <integer>{interval_seconds}</integer>

    <key>RunAtLoad</key>
    <false/>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
        <key>HOME</key>
        <string>{home_disp}</string>
    </dict>

    <key>StandardOutPath</key>
    <string>/dev/null</string>

    <key>StandardErrorPath</key>
    <string>{err}</string>

    <key>ProcessType</key>
    <string>Background</string>

    <key>Nice</key>
    <integer>5</integer>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
fn current_uid() -> u32 {
    // libc::getuid is FFI but always-succeeds (returns the real UID of the
    // calling process). No errno path.
    // SAFETY: getuid() takes no arguments and has no failure modes per
    // POSIX; it only reads kernel state.
    unsafe { libc::getuid() }
}

/// Best-effort `launchctl bootout`. Failure is expected when nothing is
/// loaded yet; we ignore the error and let the caller continue.
#[cfg(target_os = "macos")]
fn launchctl_bootout(uid: u32) -> Result<bool, io::Error> {
    let target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
    let output = Command::new("launchctl")
        .args(["bootout", &target])
        .output();
    match output {
        Ok(o) => Ok(o.status.success()),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "macos")]
fn launchctl_bootstrap(uid: u32, plist: &Path) -> io::Result<()> {
    let target = format!("gui/{uid}");
    let output = Command::new("launchctl")
        .args(["bootstrap", &target])
        .arg(plist)
        .output()
        .map_err(|e| io::Error::other(format!("launchctl bootstrap exec failed: {e}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "launchctl bootstrap failed (exit {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Atomic plist write: `<plist>.tmp` then rename. Avoids the half-written
/// file racing against an in-flight launchd reload.
#[cfg(target_os = "macos")]
fn write_plist_atomic(path: &Path, body: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.to_path_buf();
    let mut name = tmp
        .file_name()
        .ok_or_else(|| io::Error::other("plist path has no filename"))?
        .to_owned();
    name.push(".tmp");
    tmp.set_file_name(name);
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn validate_interval(interval_seconds: u64) -> io::Result<()> {
    if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&interval_seconds) {
        writeln!(
            io::stderr(),
            "claudectl --install-reaper: --reaper-interval {interval_seconds} \
             out of range [{MIN_INTERVAL_SECONDS}..={MAX_INTERVAL_SECONDS}] seconds"
        )?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interval out of range",
        ));
    }
    Ok(())
}

/// Wire `claudectl --reap-orphans` to the host's user-scoped scheduler at
/// the given interval. macOS uses launchd, Linux uses a systemd user timer;
/// other platforms print a hint and exit 0. Idempotent on both supported
/// platforms.
#[cfg(target_os = "macos")]
pub fn install_launch_agent(interval_seconds: u64) -> io::Result<()> {
    validate_interval(interval_seconds)?;

    let exe = std::env::current_exe()
        .map_err(|e| io::Error::other(format!("cannot resolve current binary path: {e}")))?;
    let home = home_dir()?;
    let plist = plist_path()?;
    let err_log = err_log_path()?;

    if let Some(parent) = err_log.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let body = build_plist(&exe, interval_seconds, &err_log, &home);
    write_plist_atomic(&plist, &body)?;

    let uid = current_uid();
    // bootout-then-bootstrap is the launchctl-blessed reload pattern. We
    // don't propagate bootout failures because the most common cause is
    // "agent isn't loaded", which is fine.
    let _ = launchctl_bootout(uid);
    launchctl_bootstrap(uid, &plist)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "installed: {} (interval={}s)",
        plist.display(),
        interval_seconds
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn install_launch_agent(interval_seconds: u64) -> io::Result<()> {
    validate_interval(interval_seconds)?;

    let exe = std::env::current_exe()
        .map_err(|e| io::Error::other(format!("cannot resolve current binary path: {e}")))?;
    let unit_dir = systemd_user_dir()?;
    std::fs::create_dir_all(&unit_dir)?;

    let err_log = linux_err_log_path()?;
    if let Some(parent) = err_log.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let service_path = systemd_service_path()?;
    let timer_path = systemd_timer_path()?;
    let service_body = build_systemd_service(&exe);
    let timer_body = build_systemd_timer(interval_seconds);
    std::fs::write(&service_path, service_body)?;
    std::fs::write(&timer_path, timer_body)?;

    // systemctl is the only way to make systemd notice the new units. If
    // it's missing, leave the unit files on disk so the user can wire them
    // manually (e.g. via a non-systemd init or a remote reload).
    if !systemctl_available() {
        writeln!(
            io::stderr(),
            "reaper: `systemctl` not in PATH. Wrote {} and {}, but did not enable the timer. \
             Reload manually once systemctl is available.",
            service_path.display(),
            timer_path.display()
        )?;
        return Err(io::Error::other("systemctl not found"));
    }

    systemctl_user(&["daemon-reload"])?;
    let timer_unit = format!("{SYSTEMD_UNIT_BASENAME}.timer");
    systemctl_user(&["enable", "--now", &timer_unit])?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "installed: {} (interval={}s)",
        timer_path.display(),
        interval_seconds
    )?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn install_launch_agent(interval_seconds: u64) -> io::Result<()> {
    let _ = interval_seconds;
    writeln!(
        io::stderr(),
        "reaper auto-install: not supported on {}; run `claudectl --reap-orphans` \
         from cron or a custom scheduler.",
        std::env::consts::OS
    )?;
    Ok(())
}

/// Reverse of `install_launch_agent`. Tolerates "nothing was installed".
#[cfg(target_os = "macos")]
pub fn uninstall_launch_agent() -> io::Result<()> {
    let plist = plist_path()?;
    let uid = current_uid();
    match launchctl_bootout(uid) {
        Ok(true) => {}
        Ok(false) => {
            writeln!(
                io::stderr(),
                "reaper: launchctl bootout returned non-zero (likely already unloaded)"
            )?;
        }
        Err(e) => {
            writeln!(io::stderr(), "reaper: launchctl bootout exec failed: {e}")?;
        }
    }
    let _ = std::fs::remove_file(&plist);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "uninstalled: {}", plist.display())?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall_launch_agent() -> io::Result<()> {
    let timer_path = systemd_timer_path()?;
    let service_path = systemd_service_path()?;
    let timer_unit = format!("{SYSTEMD_UNIT_BASENAME}.timer");

    if systemctl_available() {
        // Best-effort disable; ignore exit code because "not loaded" is fine.
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", &timer_unit])
            .output();
    }

    let _ = std::fs::remove_file(&timer_path);
    let _ = std::fs::remove_file(&service_path);

    if systemctl_available() {
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "uninstalled: {}", timer_path.display())?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn uninstall_launch_agent() -> io::Result<()> {
    writeln!(
        io::stderr(),
        "reaper auto-uninstall: not supported on {}; nothing to do.",
        std::env::consts::OS
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemctl_available() -> bool {
    Command::new("systemctl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn systemctl_user(args: &[&str]) -> io::Result<()> {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--user");
    cmd.args(args);
    let output = cmd
        .output()
        .map_err(|e| io::Error::other(format!("systemctl --user {args:?} exec failed: {e}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "systemctl --user {args:?} failed (exit {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Pure renderer for the systemd `.service` unit. Snapshot-tested. The unit
/// is `Type=oneshot`: each timer firing executes `claudectl --reap-orphans`
/// to completion and exits, mirroring the launchd `StartInterval` model.
/// `StandardError=append:%h/.local/state/...` puts the error log at a
/// predictable path inside the user's HOME (resolved by systemd via `%h`).
#[cfg(any(target_os = "linux", test))]
pub fn build_systemd_service(exe_path: &Path) -> String {
    let exe = exe_path.display();
    format!(
        "[Unit]\n\
Description=claudectl orphan reaper for in-sandbox claude processes\n\
\n\
[Service]\n\
Type=oneshot\n\
ExecStart={exe} --reap-orphans\n\
Nice=5\n\
StandardOutput=null\n\
StandardError=append:%h/.local/state/claudectl-reaper.err.log\n"
    )
}

/// Pure renderer for the systemd `.timer` unit. `OnUnitActiveSec` schedules
/// the next run that many seconds after the previous run completed, which
/// matches launchd's `StartInterval` semantics. `Persistent=true` makes the
/// timer catch up on missed runs after suspend/reboot.
#[cfg(any(target_os = "linux", test))]
pub fn build_systemd_timer(interval_seconds: u64) -> String {
    format!(
        "[Unit]\n\
Description=Periodic claudectl orphan reaper\n\
\n\
[Timer]\n\
Unit={SYSTEMD_UNIT_BASENAME}.service\n\
OnUnitActiveSec={interval_seconds}s\n\
OnBootSec={interval_seconds}s\n\
Persistent=true\n\
\n\
[Install]\n\
WantedBy=timers.target\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(pid: u32, host_tty: &str, alive: bool) -> SandboxSidecar {
        SandboxSidecar {
            pid,
            host_tty: host_tty.into(),
            alive,
            name: String::new(),
        }
    }

    fn ttys(values: &[&str]) -> HashSet<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    // ---- Tick-skip cache ----------------------------------------------

    #[test]
    fn state_string_is_deterministic_across_iteration_orders() {
        let a = ttys(&["/dev/ttys003", "/dev/ttys001", "/dev/ttys002"]);
        let b = ttys(&["/dev/ttys001", "/dev/ttys002", "/dev/ttys003"]);
        assert_eq!(state_string(&a), state_string(&b));
        assert_eq!(state_string(&a), "/dev/ttys001\n/dev/ttys002\n/dev/ttys003");
    }

    #[test]
    fn state_string_empty_set_is_empty_body() {
        assert_eq!(state_string(&HashSet::new()), "");
    }

    #[test]
    fn cache_skips_when_state_matches_and_within_max_age() {
        let action = decide_cache_action(
            Some("/dev/ttys001\n/dev/ttys002"),
            "/dev/ttys001\n/dev/ttys002",
            Some(Duration::from_secs(60)),
            MAX_CACHE_AGE,
        );
        assert_eq!(action, CacheAction::Skip);
    }

    #[test]
    fn cache_full_pass_when_state_changed() {
        let action = decide_cache_action(
            Some("/dev/ttys001"),
            "/dev/ttys001\n/dev/ttys002",
            Some(Duration::from_secs(60)),
            MAX_CACHE_AGE,
        );
        assert_eq!(action, CacheAction::FullPass);
    }

    #[test]
    fn cache_full_pass_when_age_exceeds_max() {
        let action = decide_cache_action(
            Some("/dev/ttys001"),
            "/dev/ttys001",
            Some(MAX_CACHE_AGE + Duration::from_secs(1)),
            MAX_CACHE_AGE,
        );
        assert_eq!(action, CacheAction::FullPass);
    }

    #[test]
    fn cache_full_pass_when_no_prev_state() {
        let action = decide_cache_action(None, "/dev/ttys001", None, MAX_CACHE_AGE);
        assert_eq!(action, CacheAction::FullPass);
    }

    #[test]
    fn cache_full_pass_when_age_unknown_even_if_state_matches() {
        // mtime-read failure (clock skew, FS oddity) → must NOT skip,
        // because we can't bound how stale the cache is.
        let action = decide_cache_action(Some("/dev/ttys001"), "/dev/ttys001", None, MAX_CACHE_AGE);
        assert_eq!(action, CacheAction::FullPass);
    }

    #[test]
    fn cache_skip_at_exact_max_age_boundary() {
        // age == max → still in window. The check is `<= max_age`.
        let action = decide_cache_action(
            Some("/dev/ttys001"),
            "/dev/ttys001",
            Some(MAX_CACHE_AGE),
            MAX_CACHE_AGE,
        );
        assert_eq!(action, CacheAction::Skip);
    }

    #[test]
    fn cache_full_pass_when_both_states_empty_but_no_prev() {
        // First-ever invocation with no host TTYs open: still need a
        // full pass to populate the cache.
        let action = decide_cache_action(None, "", None, MAX_CACHE_AGE);
        assert_eq!(action, CacheAction::FullPass);
    }

    #[test]
    fn cache_skips_when_both_states_empty_and_fresh() {
        let action =
            decide_cache_action(Some(""), "", Some(Duration::from_secs(10)), MAX_CACHE_AGE);
        assert_eq!(action, CacheAction::Skip);
    }

    #[test]
    fn alive_with_open_tty_is_current() {
        let plan = compute_orphans(&ttys(&["/dev/ttys001"]), &[sc(100, "/dev/ttys001", true)]);
        assert!(plan.kill.is_empty());
        assert!(plan.sweep.is_empty());
    }

    #[test]
    fn alive_with_closed_tty_is_kill() {
        let plan = compute_orphans(&ttys(&["/dev/ttys001"]), &[sc(200, "/dev/ttys999", true)]);
        assert_eq!(plan.kill, vec![sc(200, "/dev/ttys999", true)]);
        assert!(plan.sweep.is_empty());
    }

    #[test]
    fn dead_pid_goes_to_sweep_regardless_of_tty() {
        // dead AND tty closed
        let plan = compute_orphans(&ttys(&["/dev/ttys001"]), &[sc(300, "/dev/ttys999", false)]);
        assert!(plan.kill.is_empty());
        assert_eq!(plan.sweep, vec![sc(300, "/dev/ttys999", false)]);
        // dead AND tty still open (e.g. zombie sidecar after PID exit)
        let plan = compute_orphans(&ttys(&["/dev/ttys001"]), &[sc(301, "/dev/ttys001", false)]);
        assert!(plan.kill.is_empty());
        assert_eq!(plan.sweep, vec![sc(301, "/dev/ttys001", false)]);
    }

    #[test]
    fn tty_reuse_keeps_alive_in_open_set_kills_others() {
        // Two sidecars on /dev/ttys055: one alive AND in open_set (current),
        // one alive but on a different (closed) tty (orphan).
        let open = ttys(&["/dev/ttys055"]);
        let plan = compute_orphans(
            &open,
            &[
                sc(400, "/dev/ttys055", true),  // current
                sc(401, "/dev/ttys999", true),  // orphan (different tty, closed)
                sc(402, "/dev/ttys055", false), // disk-orphan (dead, same tty)
            ],
        );
        assert_eq!(plan.kill, vec![sc(401, "/dev/ttys999", true)]);
        assert_eq!(plan.sweep, vec![sc(402, "/dev/ttys055", false)]);
    }

    #[test]
    fn no_sidecars_means_no_orphans() {
        let plan = compute_orphans(&ttys(&["/dev/ttys001"]), &[]);
        assert!(plan.kill.is_empty());
        assert!(plan.sweep.is_empty());
    }

    #[test]
    fn no_open_ttys_means_every_alive_is_orphan() {
        let plan = compute_orphans(
            &ttys(&[]),
            &[sc(500, "/dev/ttys001", true), sc(501, "/dev/ttys002", true)],
        );
        assert_eq!(plan.kill.len(), 2);
    }

    // ---- Safety guard tests (decide_action) ----------------------------

    #[test]
    fn decide_skips_when_open_ttys_empty_and_alive_sidecars_exist() {
        // The footgun case: ps glitch returns no open TTYs but there are
        // alive sandbox claudes. compute_orphans would mark them all kill;
        // decide_action must refuse instead.
        let action = decide_action(
            &ttys(&[]),
            &[sc(500, "/dev/ttys001", true), sc(501, "/dev/ttys002", true)],
        );
        match action {
            Action::Skip(reason) => assert!(
                reason.contains("0 open host TTYs"),
                "unexpected skip reason: {reason}"
            ),
            Action::Execute(_) => panic!("must Skip when open_ttys empty and alive sidecars > 0"),
        }
    }

    #[test]
    fn decide_executes_when_open_ttys_empty_and_no_alive_sidecars() {
        // Pure dead-sweep case must still proceed even if open_ttys is
        // empty — there are no live sessions at risk.
        let action = decide_action(&ttys(&[]), &[sc(700, "/dev/ttys001", false)]);
        match action {
            Action::Execute(plan) => {
                assert_eq!(plan.kill.len(), 0);
                assert_eq!(plan.sweep.len(), 1);
            }
            Action::Skip(r) => panic!("must Execute when no alive sidecars; got Skip({r})"),
        }
    }

    #[test]
    fn decide_skips_when_kill_count_exceeds_cap() {
        // 11 alive orphans → over the cap of 10 → refuse. The user
        // intervenes manually with --dry-run.
        let mut sidecars = Vec::new();
        for i in 0..(MAX_KILLS_PER_PASS + 1) {
            sidecars.push(sc(1000 + i as u32, &format!("/dev/closed{i}"), true));
        }
        let action = decide_action(&ttys(&["/dev/ttys001"]), &sidecars);
        match action {
            Action::Skip(reason) => assert!(
                reason.contains("exceeds safety cap"),
                "unexpected skip reason: {reason}"
            ),
            Action::Execute(_) => panic!("must Skip when kill count exceeds cap"),
        }
    }

    #[test]
    fn decide_executes_at_exactly_the_cap() {
        // Boundary: exactly MAX_KILLS_PER_PASS is allowed.
        let mut sidecars = Vec::new();
        for i in 0..MAX_KILLS_PER_PASS {
            sidecars.push(sc(2000 + i as u32, &format!("/dev/closed{i}"), true));
        }
        let action = decide_action(&ttys(&["/dev/ttys001"]), &sidecars);
        match action {
            Action::Execute(plan) => assert_eq!(plan.kill.len(), MAX_KILLS_PER_PASS),
            Action::Skip(r) => panic!("must Execute at exactly cap; got Skip({r})"),
        }
    }

    // ---- Parser tests --------------------------------------------------

    #[test]
    fn extract_open_ttys_picks_sandbox_host_tty() {
        let ps = "\
  100 ?? Ss   0:00.01 /usr/sbin/sshd
  200 ?? S    0:00.05 sbx exec --env SANDBOX_HOST_TTY=/dev/ttys001 linera-agent bash
  201 ?? S    0:00.05 sbx exec --env SANDBOX_HOST_TTY=/dev/ttys055 linera-agent bash
  202 ?? S    0:00.05 sbx exec --env SANDBOX_HOST_TTY=/dev/ttys077 other-sandbox bash
  300 ?? S    0:00.05 ps -ax -o pid,command
";
        let set = extract_open_ttys(ps, "linera-agent");
        assert!(set.contains("/dev/ttys001"));
        assert!(set.contains("/dev/ttys055"));
        assert!(!set.contains("/dev/ttys077")); // wrong sandbox name
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn extract_open_ttys_honors_alternate_sandbox_name() {
        let ps = "\
  200 ?? S    0:00.05 sbx exec --env SANDBOX_HOST_TTY=/dev/ttys001 linera-agent bash
  201 ?? S    0:00.05 sbx exec --env SANDBOX_HOST_TTY=/dev/ttys055 my-team-sandbox bash
";
        let set = extract_open_ttys(ps, "my-team-sandbox");
        assert!(set.contains("/dev/ttys055"));
        assert!(!set.contains("/dev/ttys001"));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn sandbox_name_default_when_env_unset() {
        // Use a unique key so we don't disturb a real env value if a parallel
        // test happened to set CLAUDECTL_SANDBOX_NAME. Test the resolver
        // logic directly via a private helper that accepts the value.
        assert_eq!(resolve_or_default(None, "linera-agent"), "linera-agent");
        assert_eq!(resolve_or_default(Some(""), "linera-agent"), "linera-agent");
        assert_eq!(
            resolve_or_default(Some("my-team-sandbox"), "linera-agent"),
            "my-team-sandbox"
        );
    }

    #[test]
    fn resolve_sandbox_name_env_override_wins_over_auto_and_default() {
        assert_eq!(
            resolve_sandbox_name(Some("from-env"), Some("from-auto"), "default"),
            "from-env"
        );
    }

    #[test]
    fn resolve_sandbox_name_falls_back_to_auto_when_env_empty() {
        assert_eq!(
            resolve_sandbox_name(Some(""), Some("from-auto"), "default"),
            "from-auto"
        );
    }

    #[test]
    fn resolve_sandbox_name_falls_back_to_auto_when_env_none() {
        assert_eq!(
            resolve_sandbox_name(None, Some("from-auto"), "default"),
            "from-auto"
        );
    }

    #[test]
    fn resolve_sandbox_name_falls_back_to_default_when_no_signal() {
        assert_eq!(resolve_sandbox_name(None, None, "default"), "default");
    }

    #[test]
    fn resolve_sandbox_name_treats_empty_auto_as_no_signal() {
        // An empty auto-detect (parser couldn't pick a unique sandbox)
        // must fall through to the default, not to "".
        assert_eq!(resolve_sandbox_name(None, Some(""), "default"), "default");
    }

    // ---- sbx ls parser tests ------------------------------------------

    #[test]
    fn parse_sbx_ls_returns_single_running_sandbox() {
        // Mirrors `sbx ls` on Andre's box on 2026-04-27.
        let stdout = "\
SANDBOX        AGENT    STATUS    PORTS   WORKSPACE
linera-agent   claude   running           /Users/ndr/repos
";
        assert_eq!(
            parse_sbx_ls_for_single_running_sandbox(stdout),
            Some("linera-agent".to_string())
        );
    }

    #[test]
    fn parse_sbx_ls_returns_none_when_no_sandboxes() {
        let stdout = "SANDBOX  AGENT  STATUS  PORTS  WORKSPACE\n";
        assert_eq!(parse_sbx_ls_for_single_running_sandbox(stdout), None);
    }

    #[test]
    fn parse_sbx_ls_returns_none_when_multiple_running() {
        let stdout = "\
SANDBOX        AGENT    STATUS    PORTS   WORKSPACE
sbx-a          claude   running           /a
sbx-b          claude   running           /b
";
        assert_eq!(parse_sbx_ls_for_single_running_sandbox(stdout), None);
    }

    #[test]
    fn parse_sbx_ls_returns_none_when_only_stopped() {
        let stdout = "\
SANDBOX        AGENT    STATUS    PORTS   WORKSPACE
linera-agent   claude   stopped           /Users/ndr/repos
";
        assert_eq!(parse_sbx_ls_for_single_running_sandbox(stdout), None);
    }

    #[test]
    fn parse_sbx_ls_returns_running_when_one_running_one_stopped() {
        // A stopped sandbox is not a candidate; the lone running one is.
        let stdout = "\
SANDBOX        AGENT    STATUS    PORTS   WORKSPACE
linera-agent   claude   running           /Users/ndr/repos
old-sbx        claude   stopped           /elsewhere
";
        assert_eq!(
            parse_sbx_ls_for_single_running_sandbox(stdout),
            Some("linera-agent".to_string())
        );
    }

    #[test]
    fn parse_sbx_ls_returns_none_for_empty_input() {
        assert_eq!(parse_sbx_ls_for_single_running_sandbox(""), None);
    }

    #[test]
    fn parse_sbx_ls_returns_none_for_garbage_input() {
        // No SANDBOX header line → can't parse → None.
        let stdout = "totally bogus\noutput here\n";
        assert_eq!(parse_sbx_ls_for_single_running_sandbox(stdout), None);
    }

    #[test]
    fn parse_sandbox_sidecars_basic() {
        let text = "\
123\t/dev/ttys001\t1\tfix-validator-oom
456\t/dev/ttys999\t0\t
789\tnot-a-tty\t1\t
";
        let parsed = parse_sandbox_sidecars(text);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].pid, 123);
        assert_eq!(parsed[0].host_tty, "/dev/ttys001");
        assert!(parsed[0].alive);
        assert_eq!(parsed[0].name, "fix-validator-oom");
        assert!(!parsed[1].alive);
        assert_eq!(parsed[1].name, "");
        assert!(parsed[2].alive);
    }

    #[test]
    fn parse_sandbox_sidecars_skips_malformed() {
        let text = "\
notapid\t/dev/ttys001\t1\t
123\tonly-two-fields
\t\t\t
";
        let parsed = parse_sandbox_sidecars(text);
        // first line: pid not numeric → skipped
        // second line: only 2 columns → skipped (alive_s missing)
        // third line: empty pid → not parseable → skipped
        assert_eq!(parsed.len(), 0);
    }

    // ---- Plist generator -----------------------------------------------

    /// Snapshot test pinned to the exact body of the hand-written plist
    /// driving Andre's auto-reaper since 2026-04-26. Drift here means a new
    /// install would not be byte-equivalent to the existing one — caller
    /// must update both intentionally.
    #[test]
    fn build_plist_matches_known_good_snapshot() {
        let exe = std::path::PathBuf::from("/Users/ndr/.cargo/bin/claudectl");
        let err = std::path::PathBuf::from("/Users/ndr/Library/Logs/claudectl-reaper.err.log");
        let home = std::path::PathBuf::from("/Users/ndr");
        let body = build_plist(&exe, 60, &err, &home);
        let expected = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>linera.claudectl-reaper</string>

    <key>ProgramArguments</key>
    <array>
        <string>/Users/ndr/.cargo/bin/claudectl</string>
        <string>--reap-orphans</string>
    </array>

    <key>StartInterval</key>
    <integer>60</integer>

    <key>RunAtLoad</key>
    <false/>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
        <key>HOME</key>
        <string>/Users/ndr</string>
    </dict>

    <key>StandardOutPath</key>
    <string>/dev/null</string>

    <key>StandardErrorPath</key>
    <string>/Users/ndr/Library/Logs/claudectl-reaper.err.log</string>

    <key>ProcessType</key>
    <string>Background</string>

    <key>Nice</key>
    <integer>5</integer>
</dict>
</plist>
"#;
        assert_eq!(body, expected);
    }

    #[test]
    fn build_plist_substitutes_interval() {
        let exe = std::path::PathBuf::from("/x/y");
        let err = std::path::PathBuf::from("/e");
        let home = std::path::PathBuf::from("/h");
        let body = build_plist(&exe, 120, &err, &home);
        assert!(body.contains("<integer>120</integer>"));
        assert!(body.contains("<string>/x/y</string>"));
        assert!(body.contains("<string>/e</string>"));
        assert!(body.contains("<string>/h</string>"));
    }

    // ---- systemd unit generators --------------------------------------

    #[test]
    fn build_systemd_service_matches_known_good_snapshot() {
        let exe = std::path::PathBuf::from("/home/dev/.cargo/bin/claudectl");
        let body = build_systemd_service(&exe);
        let expected = "[Unit]\n\
Description=claudectl orphan reaper for in-sandbox claude processes\n\
\n\
[Service]\n\
Type=oneshot\n\
ExecStart=/home/dev/.cargo/bin/claudectl --reap-orphans\n\
Nice=5\n\
StandardOutput=null\n\
StandardError=append:%h/.local/state/claudectl-reaper.err.log\n";
        assert_eq!(body, expected);
    }

    #[test]
    fn build_systemd_timer_matches_known_good_snapshot() {
        let body = build_systemd_timer(60);
        let expected = "[Unit]\n\
Description=Periodic claudectl orphan reaper\n\
\n\
[Timer]\n\
Unit=claudectl-reaper.service\n\
OnUnitActiveSec=60s\n\
OnBootSec=60s\n\
Persistent=true\n\
\n\
[Install]\n\
WantedBy=timers.target\n";
        assert_eq!(body, expected);
    }

    #[test]
    fn build_systemd_timer_substitutes_interval() {
        let body = build_systemd_timer(300);
        assert!(body.contains("OnUnitActiveSec=300s"));
        assert!(body.contains("OnBootSec=300s"));
        assert!(body.contains("Persistent=true"));
        assert!(body.contains("WantedBy=timers.target"));
    }
}

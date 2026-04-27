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
//! - `CLAUDECTL_SANDBOX_NAME` — sbx sandbox to scan. Default `linera-agent`.
//! - `CLAUDECTL_SANDBOX_SESSIONS_DIR` — in-sandbox path holding the per-PID
//!   `{pid}.terminal.json` sidecars. Default `/var/lib/sandbox-sessions`.
//!
//! Both env vars are read on every invocation; an empty value falls back to
//! the default (treat empty as unset).

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

fn sandbox_name() -> String {
    resolve_or_default(
        std::env::var("CLAUDECTL_SANDBOX_NAME").ok().as_deref(),
        "linera-agent",
    )
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

    // Sweep already-dead sidecars unconditionally — they have no live
    // process, removing their disk artefacts is always safe.
    let dead_pids: Vec<u32> = plan.sweep.iter().map(|o| o.pid).collect();
    if !dead_pids.is_empty() {
        sweep_sandbox_files(&dead_pids)?;
    }

    // Send SIGHUP to alive orphans. Do NOT pre-emptively sweep their disk
    // files: if the kill fails (sbx down, signal lost, claude ignores HUP)
    // we want their sidecars to remain so the next reaper pass can retry.
    // Successful kills will be followed by their PIDs going dead, and the
    // next pass will sweep them via the dead-sidecar path above.
    if !plan.kill.is_empty() {
        let pids: Vec<String> = plan.kill.iter().map(|o| o.pid.to_string()).collect();
        let name = sandbox_name();
        match Command::new("sbx")
            .args(["exec", &name, "kill", "-HUP"])
            .args(&pids)
            .output()
        {
            Ok(o) if !o.status.success() => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                writeln!(io::stderr(), "reaper: kill -HUP failed: {stderr}")?;
            }
            Err(e) => {
                writeln!(io::stderr(), "reaper: kill -HUP exec failed: {e}")?;
            }
            Ok(_) => {}
        }
    }

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

/// Sweep `{pid}.json` and `{pid}.terminal.json` for every reaped/dead pid.
/// Single sbx exec, comma/space-joined.
fn sweep_sandbox_files(pids: &[u32]) -> io::Result<()> {
    let pids_arg: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
    let dir = sandbox_sessions_dir();
    let script = format!(
        r#"
set -u
DIR={dir}
for pid in "$@"; do
  rm -f "$DIR/$pid.json" "$DIR/$pid.terminal.json" 2>/dev/null || true
done
"#
    );
    let name = sandbox_name();
    let mut cmd = Command::new("sbx");
    cmd.args(["exec", &name, "bash", "-c", &script, "--"]);
    cmd.args(&pids_arg);
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "sbx exec sweep failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

// ── Launchd install/uninstall ─────────────────────────────────────────────

const LAUNCH_AGENT_LABEL: &str = "linera.claudectl-reaper";

/// Hard floor: anything below this hammers `sbx exec` faster than a real
/// reaper pass completes (the in-sandbox bash + grep + kill pipeline takes
/// a second or two). Hard ceiling: anything above an hour means the user
/// is closing tabs faster than the reaper can find them.
pub const MIN_INTERVAL_SECONDS: u64 = 10;
pub const MAX_INTERVAL_SECONDS: u64 = 3600;

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set; cannot locate ~/Library"))
}

fn plist_path() -> io::Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist")))
}

fn err_log_path() -> io::Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("Logs")
        .join("claudectl-reaper.err.log"))
}

/// Pure plist renderer. The XML body is byte-for-byte equivalent to the
/// hand-written plist that's been driving the auto-reaper on Andre's box —
/// changing whitespace here will break the byte-equivalence verification.
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

fn current_uid() -> u32 {
    // libc::getuid is FFI but always-succeeds (returns the real UID of the
    // calling process). No errno path.
    // SAFETY: getuid() takes no arguments and has no failure modes per
    // POSIX; it only reads kernel state.
    unsafe { libc::getuid() }
}

/// Best-effort `launchctl bootout`. Failure is expected when nothing is
/// loaded yet; we ignore the error and let the caller continue.
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

/// Wire `claudectl --reap-orphans` to launchd at the given interval.
/// Idempotent: bootouts any existing job first, then bootstraps the new one.
pub fn install_launch_agent(interval_seconds: u64) -> io::Result<()> {
    if !cfg!(target_os = "macos") {
        writeln!(
            io::stderr(),
            "reaper auto-install: only macOS launchd is implemented. \
             On Linux, run `claudectl --reap-orphans` from a systemd user \
             timer or a cron entry. See docs/known-bugs/hazmat-orphan-disconnect.md."
        )?;
        return Ok(());
    }
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

/// Reverse of `install_launch_agent`. Tolerates "nothing was installed".
pub fn uninstall_launch_agent() -> io::Result<()> {
    if !cfg!(target_os = "macos") {
        writeln!(
            io::stderr(),
            "reaper auto-uninstall: only macOS launchd is implemented; nothing to do on this platform."
        )?;
        return Ok(());
    }

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
    fn sandbox_name_picks_up_env_override() {
        // Real env-var roundtrip. SAFETY: cargo test runs tests in parallel
        // by default and `set_var`/`remove_var` mutate process-global state,
        // so any other test reading CLAUDECTL_SANDBOX_NAME at the same time
        // would race. We're the only test that touches this var.
        // SAFETY: see comment above on global env mutation.
        unsafe {
            std::env::set_var("CLAUDECTL_SANDBOX_NAME", "ndr-private-test-sbx");
        }
        assert_eq!(sandbox_name(), "ndr-private-test-sbx");
        // SAFETY: see comment above on global env mutation.
        unsafe {
            std::env::remove_var("CLAUDECTL_SANDBOX_NAME");
        }
        assert_eq!(sandbox_name(), "linera-agent");
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
}

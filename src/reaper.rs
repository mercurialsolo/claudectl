//! One-shot orphan reaper for in-sandbox `claude` processes.
//!
//! When a user closes an iTerm2 tab whose `claude` runs inside the
//! `linera-agent` agent-sandbox, Docker exec doesn't propagate SIGHUP to the
//! container-side exec target (moby/moby#9098). The in-VM `claude` survives,
//! its sidecar (`{pid}.terminal.json`) keeps pointing at a host TTY that is
//! no longer attached, and the row sits Idle forever.
//!
//! The reaper detects this by diffing two sets:
//! - Open set: host-side TTYs of currently-running `sbx exec ... linera-agent`
//!   processes, extracted from `SANDBOX_HOST_TTY=/dev/ttysNNN` in argv.
//! - Sandbox set: per-PID sidecars under `/var/lib/sandbox-sessions`, each
//!   carrying its `host_tty` and a kill(0) liveness check.
//!
//! Any sandbox PID whose sidecar `host_tty` is not in the open set AND whose
//! process is alive is sent SIGHUP. Sidecars whose PID is dead are swept off
//! disk along with their `{pid}.json` companion.
//!
//! Wired as `claudectl --reap-orphans` in `main.rs`. Add `--dry-run` to
//! preview without killing or removing.

use std::collections::HashSet;
use std::io::{self, Write};
use std::process::Command;

const SANDBOX_NAME: &str = "linera-agent";
const SANDBOX_SESSIONS_DIR: &str = "/var/lib/sandbox-sessions";

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
        match Command::new("sbx")
            .args(["exec", SANDBOX_NAME, "kill", "-HUP"])
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
    Ok(extract_open_ttys(&text))
}

/// Pure parser: takes `ps -ax -o pid,command` output, returns the set of
/// `SANDBOX_HOST_TTY` values from `sbx exec ... linera-agent` lines.
fn extract_open_ttys(ps_output: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for line in ps_output.lines() {
        if !line.contains("sbx exec") {
            continue;
        }
        if !line.contains(SANDBOX_NAME) {
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
    let script = format!(
        r#"
set -u
DIR={SANDBOX_SESSIONS_DIR}
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
        .args(["exec", SANDBOX_NAME, "bash", "-c", &script])
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
    let script = format!(
        r#"
set -u
DIR={SANDBOX_SESSIONS_DIR}
for pid in "$@"; do
  rm -f "$DIR/$pid.json" "$DIR/$pid.terminal.json" 2>/dev/null || true
done
"#
    );
    let mut cmd = Command::new("sbx");
    cmd.args(["exec", SANDBOX_NAME, "bash", "-c", &script, "--"]);
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
        let set = extract_open_ttys(ps);
        assert!(set.contains("/dev/ttys001"));
        assert!(set.contains("/dev/ttys055"));
        assert!(!set.contains("/dev/ttys077")); // wrong sandbox name
        assert_eq!(set.len(), 2);
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
}

//! Liveness for the headless process (#546, gap 3 for the headless service).
//!
//! `launchd` and `systemd` restart a process that *exited*. Neither notices one
//! that is alive and doing nothing, so "the service is loaded" and "the service
//! is working" are different questions — the distinction #546 was filed over.
//!
//! `relay serve` already answers the second question by accident: it rewrites
//! `fleet.json` every tick, and [`crate::fleet::read_snapshot`] returns `None`
//! once that file goes stale. The headless process had no equivalent. Its only
//! periodic output is `emit_headless_event`, which goes to stdout — a log file
//! under `launchd`, the journal under `systemd`. Reading log mtime on one
//! platform and `journalctl` on the other produces a check that behaves
//! differently per platform, which is the kind nobody trusts.
//!
//! So the headless loop writes this file instead: same atomic temp + rename as
//! `fleet.rs`, same staleness constant, one code path on both platforms.
//!
//! It deliberately carries no state beyond a timestamp and a pid. Anything
//! else would make it a status file that something starts depending on; the
//! only question it exists to answer is "did the loop go round recently".

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How long before a heartbeat means the loop has stopped going round.
///
/// The same value as [`crate::fleet::SNAPSHOT_STALE_SECS`], and deliberately
/// not a separate number: both answer "has this process stopped publishing",
/// and two constants would drift into two different definitions of wedged.
pub const STALE_SECS: u64 = crate::fleet::SNAPSHOT_STALE_SECS;

/// One tick of the headless loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Milliseconds since the epoch, when the tick was written.
    pub updated_ms: u64,
    /// The writing process, so a stale file can be matched against what is
    /// actually running when someone investigates.
    pub pid: u32,
}

fn headless_dir_in(home: &Path) -> PathBuf {
    home.join(".claudectl").join("headless")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

/// `~/.claudectl/headless/heartbeat.json`.
///
/// Beside the headless service's own logs rather than under the relay's
/// directory — one service, one place to look, which is the rule
/// `relay/agent.rs` states for the relay's logs.
pub fn path() -> PathBuf {
    path_in(&home())
}

/// [`path`] under an explicit home. Exists so tests never read `$HOME`.
pub fn path_in(home: &Path) -> PathBuf {
    headless_dir_in(home).join("heartbeat.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Record a tick. Atomic, so a reader mid-write never sees a torn file.
pub fn write() -> io::Result<()> {
    write_in(&home())
}

/// [`write`] under an explicit home. Exists so tests never touch `$HOME`.
pub fn write_in(home: &Path) -> io::Result<()> {
    let dir = headless_dir_in(home);
    fs::create_dir_all(&dir)?;
    let beat = Heartbeat {
        updated_ms: now_ms(),
        pid: std::process::id(),
    };
    let body = serde_json::to_string(&beat).map_err(io::Error::other)?;
    let tmp = dir.join(".heartbeat.json.tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, dir.join("heartbeat.json"))
}

/// The last tick, or `None` when the file is absent, unreadable, or old enough
/// that the loop has clearly stopped.
///
/// Absent and stale collapse to the same answer on purpose: the caller's
/// question is "is it working", and both mean no. Whether a service is
/// *installed* is a separate question, which `doctor` asks first.
pub fn read() -> Option<Heartbeat> {
    read_from(&path())
}

/// [`read`] from an explicit path. Exists so tests never read `$HOME`.
pub fn read_from(path: &Path) -> Option<Heartbeat> {
    let body = fs::read_to_string(path).ok()?;
    let beat: Heartbeat = serde_json::from_str(&body).ok()?;
    if now_ms().saturating_sub(beat.updated_ms) > STALE_SECS * 1000 {
        return None;
    }
    Some(beat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_heartbeat_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        write_in(tmp.path()).unwrap();
        let beat = read_from(&path_in(tmp.path())).expect("fresh heartbeat should read back");
        assert_eq!(beat.pid, std::process::id());
    }

    #[test]
    fn a_missing_heartbeat_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_from(&path_in(tmp.path())).is_none());
    }

    #[test]
    fn a_stale_heartbeat_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = headless_dir_in(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        let old = Heartbeat {
            updated_ms: now_ms().saturating_sub((STALE_SECS + 10) * 1000),
            pid: 1234,
        };
        fs::write(path_in(tmp.path()), serde_json::to_string(&old).unwrap()).unwrap();
        assert!(
            read_from(&path_in(tmp.path())).is_none(),
            "a heartbeat older than STALE_SECS means the loop stopped"
        );
    }

    /// A file that is present but not a heartbeat must read as "not working"
    /// rather than panicking a `doctor` run.
    #[test]
    fn unparseable_is_none_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = headless_dir_in(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(path_in(tmp.path()), "{ truncated").unwrap();
        assert!(read_from(&path_in(tmp.path())).is_none());
    }

    /// The temp file must not be left behind, or `~/.claudectl/headless` fills
    /// with them on a crash-restart loop.
    #[test]
    fn writing_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_in(tmp.path()).unwrap();
        let leftover = headless_dir_in(tmp.path()).join(".heartbeat.json.tmp");
        assert!(
            !leftover.exists(),
            "temp file should have been renamed away"
        );
    }

    /// Guards the reason `STALE_SECS` is an alias rather than its own number.
    #[test]
    fn staleness_matches_the_fleet_snapshot() {
        assert_eq!(STALE_SECS, crate::fleet::SNAPSHOT_STALE_SECS);
    }
}

//! How much room this machine has for another task (#571): free memory,
//! load per core, free disk. What a relay peer publishes on its heartbeat and
//! what supervisor placement compares against a slot policy.
//!
//! Probed with the platform's own tools (`sysctl`, `vm_stat`, `df` on macOS;
//! `/proc` and `df` on Linux) rather than a system-info crate, for the same
//! reason `process.rs` uses `ps`. Each parser is a pure function of the
//! command's output. A probe that fails leaves its field `None`: placement
//! treats an unknown as not meeting a floor, never as plenty.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// One machine's headroom at a moment. `running_tasks` is supplied by the
/// caller, which knows what it's running; the rest is probed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Headroom {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_free_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_per_core: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_mb: Option<u64>,
    #[serde(default)]
    pub running_tasks: u32,
    /// The machine's own task ceiling, when it has one (#571). Advertised so
    /// a supervisor elsewhere doesn't send work this machine would refuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tasks: Option<u32>,
}

/// Probe this machine. `disk_path` is where task worktrees go; its
/// filesystem's free space is what a new task would use.
pub fn probe(disk_path: &Path, running_tasks: u32) -> Headroom {
    Headroom {
        mem_free_mb: mem_free_mb(),
        load_per_core: load_per_core(),
        disk_free_mb: run("df", &["-Pk", &disk_path.to_string_lossy()])
            .and_then(|o| parse_df_free_mb(&o)),
        running_tasks,
        max_tasks: None,
    }
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn cores() -> Option<f64> {
    std::thread::available_parallelism()
        .ok()
        .map(|n| n.get() as f64)
}

#[cfg(target_os = "macos")]
fn mem_free_mb() -> Option<u64> {
    run("vm_stat", &[]).and_then(|o| parse_vm_stat_free_mb(&o))
}

#[cfg(target_os = "linux")]
fn mem_free_mb() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_meminfo_available_mb(&s))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn mem_free_mb() -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn load_per_core() -> Option<f64> {
    let one = run("sysctl", &["-n", "vm.loadavg"]).and_then(|o| parse_loadavg_1m(&o))?;
    Some(one / cores()?)
}

#[cfg(target_os = "linux")]
fn load_per_core() -> Option<f64> {
    let one = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| parse_loadavg_1m(&s))?;
    Some(one / cores()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn load_per_core() -> Option<f64> {
    None
}

/// Memory a new process could get without paging: free, inactive,
/// speculative and purgeable pages. `wired` and `active` are in use.
pub fn parse_vm_stat_free_mb(out: &str) -> Option<u64> {
    let page: u64 = out
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let pages = |label: &str| -> u64 {
        out.lines()
            .find(|l| l.starts_with(label))
            .and_then(|l| l.rsplit(':').next())
            .and_then(|v| v.trim().trim_end_matches('.').parse().ok())
            .unwrap_or(0)
    };
    let free = pages("Pages free")
        + pages("Pages inactive")
        + pages("Pages speculative")
        + pages("Pages purgeable");
    (free > 0).then_some(free * page / (1024 * 1024))
}

/// `MemAvailable` from /proc/meminfo, the kernel's own estimate.
pub fn parse_meminfo_available_mb(s: &str) -> Option<u64> {
    let kb: u64 = s
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kb / 1024)
}

/// The 1-minute load from `sysctl -n vm.loadavg` (`{ 2.95 3.45 3.47 }`) or
/// /proc/loadavg (`2.95 3.45 3.47 2/1000 12345`).
pub fn parse_loadavg_1m(s: &str) -> Option<f64> {
    s.split_whitespace().find(|w| *w != "{")?.parse().ok()
}

/// Available space from `df -Pk <path>`: the data line's fourth column, in KiB.
pub fn parse_df_free_mb(out: &str) -> Option<u64> {
    let kb: u64 = out
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(kb / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_stat_free_counts_reclaimable_pages() {
        let out = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                   Pages free:                                    20232.\n\
                   Pages active:                                 193921.\n\
                   Pages inactive:                               188863.\n\
                   Pages speculative:                              4256.\n\
                   Pages wired down:                             193979.\n\
                   Pages purgeable:                                   6.\n";
        // (20232 + 188863 + 4256 + 6) * 16384 / 1 MiB
        assert_eq!(parse_vm_stat_free_mb(out), Some(3333));
        assert_eq!(parse_vm_stat_free_mb("garbage"), None);
    }

    #[test]
    fn meminfo_uses_mem_available() {
        let s = "MemTotal:       16000000 kB\nMemFree:  100 kB\nMemAvailable:    8388608 kB\n";
        assert_eq!(parse_meminfo_available_mb(s), Some(8192));
        assert_eq!(parse_meminfo_available_mb("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn loadavg_reads_both_formats() {
        assert_eq!(parse_loadavg_1m("{ 2.95 3.45 3.47 }\n"), Some(2.95));
        assert_eq!(parse_loadavg_1m("0.50 0.40 0.30 2/1000 12345\n"), Some(0.5));
        assert_eq!(parse_loadavg_1m(""), None);
    }

    #[test]
    fn df_reads_available_kib() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   /dev/disk3s5   482797652 440552948  11948356    98%    /System/Volumes/Data\n";
        assert_eq!(parse_df_free_mb(out), Some(11668));
        assert_eq!(parse_df_free_mb("header only\n"), None);
    }

    #[test]
    fn an_unprobed_field_is_left_out_of_the_wire_form() {
        let h = Headroom {
            mem_free_mb: Some(1),
            running_tasks: 2,
            ..Headroom::default()
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v, serde_json::json!({"mem_free_mb": 1, "running_tasks": 2}));
        let back: Headroom = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(back, Headroom::default());
    }

    #[test]
    fn probing_this_machine_finds_something() {
        let h = probe(&std::env::temp_dir(), 0);
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(h.mem_free_mb.is_some(), "{h:?}");
            assert!(h.load_per_core.is_some(), "{h:?}");
            assert!(h.disk_free_mb.is_some(), "{h:?}");
        }
    }
}

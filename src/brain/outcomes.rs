#![allow(dead_code)]

//! Outcome capture for brain decisions (#220 baselining v1).
//!
//! A `PostToolUse` hook in Claude Code writes a "pending outcome" file each
//! time a tool finishes. The reaper periodically attributes each pending
//! outcome to the most recent matching decision in `decisions.jsonl` and
//! writes the resolved outcome to `outcomes/<decision_id>.json`. Distillation
//! reads decisions and outcomes together to build per-approach success
//! statistics that feed into the hive as `ApproachOutcome` knowledge units.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::decisions::{DecisionRecord, decisions_dir, read_all_decisions};

// ────────────────────────────────────────────────────────────────────────────
// Constants
// ────────────────────────────────────────────────────────────────────────────

/// How recent a decision must be (seconds) to be a candidate for outcome
/// attribution. Keeps fuzzy matching from binding outcomes to ancient
/// decisions when many sessions reuse the same command.
const ATTRIBUTION_WINDOW_SECS: u64 = 600;

/// How long an unattributed pending outcome lives before being marked orphaned.
const ORPHAN_AFTER_SECS: u64 = 86_400;

/// Cap on stderr_tail bytes stored — protects against runaway log capture.
pub const MAX_STDERR_TAIL_BYTES: usize = 2_048;

/// Lookback window (seconds) for attributing a test-runner failure to recent
/// brain-approved edits (#238). Shorter than `ATTRIBUTION_WINDOW_SECS` because
/// the signal degrades quickly — edits 10 minutes before a failing test run
/// are less likely to be the cause.
const TEST_FAILURE_FANOUT_WINDOW_SECS: u64 = 300;

/// Cap on how many recent edits a single test failure attributes to. Prevents
/// long stretches of refactor edits from all sharing one failure.
const TEST_FAILURE_FANOUT_MAX_EDITS: usize = 5;

/// Edit-like tools whose decisions get tagged when a subsequent test run fails.
const EDIT_LIKE_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

// ────────────────────────────────────────────────────────────────────────────
// Types
// ────────────────────────────────────────────────────────────────────────────

/// What the PostToolUse hook saw, written before any decision attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingOutcome {
    /// Tool name (e.g., "Bash", "Edit").
    pub tool: String,
    /// Command or input summary captured by the hook.
    #[serde(default)]
    pub command: Option<String>,
    /// Project slug (basename of cwd at hook time).
    pub project: String,
    /// Claude Code session id, if the hook payload carried one.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Claude Code tool_use_id, if available — used for stricter joining later.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// Tool exit code (0 = success). None when the hook can't infer one.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Wall-clock duration of the tool call in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Last MAX_STDERR_TAIL_BYTES of stderr or tool error output.
    #[serde(default)]
    pub stderr_tail: Option<String>,
    /// Epoch seconds when the outcome was captured.
    pub ts: u64,
}

/// Resolved outcome: a `PendingOutcome` attributed to a specific decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedOutcome {
    pub decision_id: String,
    pub tool: String,
    #[serde(default)]
    pub command: Option<String>,
    pub project: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub stderr_tail: Option<String>,
    pub ts: u64,
}

/// Stats returned by `reap()`.
#[derive(Debug, Default, Clone)]
pub struct ReapStats {
    pub scanned: u32,
    pub attributed: u32,
    pub orphaned: u32,
    pub still_pending: u32,
    pub errors: u32,
    /// Edit decisions newly tagged as `TestFailed` from a fan-out attribution.
    pub test_failures_attributed: u32,
}

/// Marker file written into `test-failures/<decision_id>.json` when a
/// test-runner command failed shortly after a brain-approved edit (#238).
/// Backfilled into `DecisionOutcome::TestFailed` during distillation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestFailureMarker {
    pub decision_id: String,
    /// The exact failing command (e.g. "cargo test", "npm test --watch=false").
    pub failed_test_command: String,
    /// Epoch seconds when the failure was observed.
    pub outcome_ts: u64,
}

// ────────────────────────────────────────────────────────────────────────────
// Path helpers
// ────────────────────────────────────────────────────────────────────────────

/// Directory where pending PostToolUse outcomes accumulate.
pub fn pending_dir() -> PathBuf {
    decisions_dir().join("pending-outcomes")
}

/// Directory where attributed outcomes live, keyed by `<decision_id>.json`.
pub fn outcomes_dir() -> PathBuf {
    decisions_dir().join("outcomes")
}

/// Directory where pending files that failed attribution after `ORPHAN_AFTER_SECS`
/// are quarantined for inspection.
pub fn orphaned_dir() -> PathBuf {
    decisions_dir().join("outcomes-orphaned")
}

/// Directory where test-failure attribution markers live, keyed by `<decision_id>.json`.
pub fn test_failures_dir() -> PathBuf {
    decisions_dir().join("test-failures")
}

fn ensure_dir(path: &PathBuf) -> std::io::Result<()> {
    fs::create_dir_all(path)
}

// ────────────────────────────────────────────────────────────────────────────
// ID generation
// ────────────────────────────────────────────────────────────────────────────

static OUTCOME_COUNTER: AtomicU64 = AtomicU64::new(0);

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Generate a unique pending outcome filename stem (no extension).
fn gen_pending_id() -> String {
    let epoch = epoch_secs();
    let seq = OUTCOME_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("po_{epoch}_{pid}_{seq}")
}

// ────────────────────────────────────────────────────────────────────────────
// Write / read
// ────────────────────────────────────────────────────────────────────────────

/// Truncate stderr to MAX_STDERR_TAIL_BYTES from the tail.
pub fn truncate_stderr(s: &str) -> String {
    if s.len() <= MAX_STDERR_TAIL_BYTES {
        return s.to_string();
    }
    // Take the trailing slice on a char boundary.
    let start = s.len() - MAX_STDERR_TAIL_BYTES;
    let safe_start = (start..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    s[safe_start..].to_string()
}

/// Persist a pending outcome to `pending-outcomes/<id>.json`.
pub fn write_pending(out: &PendingOutcome) -> std::io::Result<PathBuf> {
    let dir = pending_dir();
    ensure_dir(&dir)?;
    let path = dir.join(format!("{}.json", gen_pending_id()));
    let json = serde_json::to_string(out).map_err(std::io::Error::other)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    file.write_all(json.as_bytes())?;
    Ok(path)
}

/// Read all pending outcomes (path + parsed body).
pub fn list_pending() -> Vec<(PathBuf, PendingOutcome)> {
    let dir = pending_dir();
    let mut out = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(p) = serde_json::from_str::<PendingOutcome>(&content) {
            out.push((path, p));
        }
    }
    out
}

/// Load all attributed outcomes keyed by `decision_id`.
pub fn load_resolved_map() -> std::collections::HashMap<String, ResolvedOutcome> {
    let mut map = std::collections::HashMap::new();
    let dir = outcomes_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return map,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(r) = serde_json::from_str::<ResolvedOutcome>(&content) {
            map.insert(r.decision_id.clone(), r);
        }
    }
    map
}

// ────────────────────────────────────────────────────────────────────────────
// Test failure attribution (#238)
// ────────────────────────────────────────────────────────────────────────────

/// Check whether `cmd` is invocation of one of the configured test runners.
/// Match is a normalized, case-insensitive prefix comparison on whitespace-
/// collapsed forms — so `"  CARGO   test --release "` matches `"cargo test"`.
pub fn is_test_runner_cmd(cmd: &str, runners: &[String]) -> bool {
    let cmd_norm = normalize_command(cmd).to_lowercase();
    if cmd_norm.is_empty() {
        return false;
    }
    runners.iter().any(|r| {
        let r_norm = normalize_command(r).to_lowercase();
        if r_norm.is_empty() {
            return false;
        }
        // Prefix match on token boundary (either equal or followed by space/end).
        if cmd_norm == r_norm {
            return true;
        }
        if let Some(rest) = cmd_norm.strip_prefix(&r_norm) {
            return rest.starts_with(' ');
        }
        false
    })
}

/// Load existing test-failure markers, keyed by decision_id.
pub fn load_test_failures() -> std::collections::HashMap<String, TestFailureMarker> {
    let mut map = std::collections::HashMap::new();
    let dir = test_failures_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return map,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(m) = serde_json::from_str::<TestFailureMarker>(&content) {
            map.insert(m.decision_id.clone(), m);
        }
    }
    map
}

/// Fan a failed test-runner pending outcome out to the most recent brain-
/// approved edits in the same project, writing one marker per edit decision.
///
/// Returns the number of new markers written. Idempotent: existing markers
/// for a decision_id are never overwritten.
fn fanout_test_failures(
    pending: &[(PathBuf, PendingOutcome)],
    decisions: &[DecisionRecord],
    runners: &[String],
) -> u32 {
    if runners.is_empty() {
        return 0;
    }
    let existing = load_test_failures();
    let dir = test_failures_dir();
    let _ = ensure_dir(&dir);
    let mut written = 0u32;

    for (_, p) in pending {
        if p.exit_code.unwrap_or(0) == 0 {
            continue;
        }
        let Some(cmd) = &p.command else {
            continue;
        };
        if !is_test_runner_cmd(cmd, runners) {
            continue;
        }

        // Collect candidate edit decisions: same project, positive outcome,
        // edit-like tool, timestamp inside the fan-out window before this run.
        let mut candidates: Vec<&DecisionRecord> = decisions
            .iter()
            .filter(|d| {
                let Some(_did) = d.decision_id.as_deref() else {
                    return false;
                };
                if !d.project.eq_ignore_ascii_case(&p.project) {
                    return false;
                }
                let tool = d.tool.as_deref().unwrap_or("");
                if !EDIT_LIKE_TOOLS.contains(&tool) {
                    return false;
                }
                if !d.is_positive() {
                    return false;
                }
                let Some(d_ts) = parse_ts(&d.timestamp) else {
                    return false;
                };
                d_ts <= p.ts && p.ts.saturating_sub(d_ts) <= TEST_FAILURE_FANOUT_WINDOW_SECS
            })
            .collect();

        // Most recent first so we attribute the last N edits.
        candidates.sort_by(|a, b| {
            parse_ts(&b.timestamp)
                .unwrap_or(0)
                .cmp(&parse_ts(&a.timestamp).unwrap_or(0))
        });

        for d in candidates.iter().take(TEST_FAILURE_FANOUT_MAX_EDITS) {
            let did = match d.decision_id.as_deref() {
                Some(s) => s.to_string(),
                None => continue,
            };
            if existing.contains_key(&did) {
                continue;
            }
            let marker = TestFailureMarker {
                decision_id: did.clone(),
                failed_test_command: cmd.clone(),
                outcome_ts: p.ts,
            };
            let dest = dir.join(format!("{did}.json"));
            let Ok(json) = serde_json::to_string(&marker) else {
                continue;
            };
            // create_new makes this idempotent: if a marker already exists on
            // disk (from a previous reap pass), we skip silently.
            match OpenOptions::new().create_new(true).write(true).open(&dest) {
                Ok(mut file) => {
                    if file.write_all(json.as_bytes()).is_ok() {
                        written += 1;
                    }
                }
                Err(_) => continue,
            }
        }
    }
    written
}

// ────────────────────────────────────────────────────────────────────────────
// Reaper
// ────────────────────────────────────────────────────────────────────────────

/// Normalise a command string for fuzzy matching against decision records.
/// Strips leading/trailing whitespace and collapses internal runs.
fn normalize_command(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a decision timestamp (currently stored as `"<epoch_secs>"`).
fn parse_ts(s: &str) -> Option<u64> {
    s.trim_matches('"').parse::<u64>().ok()
}

/// Walk pending outcomes, attribute each to a matching decision, and write
/// resolved outcomes. Pending files older than `ORPHAN_AFTER_SECS` are moved
/// to `orphaned_dir()` for inspection.
///
/// Attribution rule: the most recent decision in `decisions.jsonl` such that
///   - same tool
///   - normalized command equals the outcome's normalized command (when both present)
///   - same project (case-insensitive)
///   - decision timestamp <= outcome timestamp, within ATTRIBUTION_WINDOW_SECS
///   - decision has a `decision_id`
///   - no resolved outcome exists yet for that `decision_id`
pub fn reap() -> ReapStats {
    let runners = crate::config::Config::load()
        .brain
        .map(|b| b.test_runners)
        .unwrap_or_else(crate::config::default_test_runners);
    reap_with_runners(&runners)
}

/// `reap()` with explicit test-runner patterns. Exposed for tests so they
/// don't depend on a Config layered TOML load.
pub fn reap_with_runners(test_runners: &[String]) -> ReapStats {
    let mut stats = ReapStats::default();
    let pending = list_pending();
    if pending.is_empty() {
        return stats;
    }

    let _ = ensure_dir(&outcomes_dir());
    let _ = ensure_dir(&orphaned_dir());

    let decisions = read_all_decisions();
    let resolved = load_resolved_map();
    let now = epoch_secs();

    // #238: fan failing test runs out to recent brain-approved edits before
    // we mutate the pending list. Idempotent — re-running is safe.
    stats.test_failures_attributed = fanout_test_failures(&pending, &decisions, test_runners);
    // Track decisions claimed within this reap pass so a single decision
    // doesn't get attributed to two pending outcomes when we run before
    // the resolved map is reloaded.
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (path, p) in pending {
        stats.scanned += 1;

        // Try attribution
        let p_cmd_norm = p.command.as_deref().map(normalize_command);
        let mut best: Option<(usize, u64)> = None; // (index, ts) — pick most recent

        for (i, d) in decisions.iter().enumerate() {
            let Some(decision_id) = d.decision_id.as_deref() else {
                continue;
            };
            if resolved.contains_key(decision_id) || claimed.contains(decision_id) {
                continue;
            }
            let Some(d_tool) = d.tool.as_deref() else {
                continue;
            };
            if d_tool != p.tool {
                continue;
            }
            if !d.project.eq_ignore_ascii_case(&p.project) {
                continue;
            }
            // Command match (only enforced if both sides present)
            if let (Some(pc), Some(dc)) = (&p_cmd_norm, &d.command) {
                if normalize_command(dc) != *pc {
                    continue;
                }
            }
            let Some(d_ts) = parse_ts(&d.timestamp) else {
                continue;
            };
            if d_ts > p.ts {
                continue; // decision must precede outcome
            }
            if p.ts.saturating_sub(d_ts) > ATTRIBUTION_WINDOW_SECS {
                continue;
            }
            match best {
                None => best = Some((i, d_ts)),
                Some((_, prev_ts)) if d_ts > prev_ts => best = Some((i, d_ts)),
                _ => {}
            }
        }

        if let Some((idx, _)) = best {
            let d = &decisions[idx];
            let decision_id = d.decision_id.clone().unwrap();
            let resolved = ResolvedOutcome {
                decision_id: decision_id.clone(),
                tool: p.tool.clone(),
                command: p.command.clone(),
                project: p.project.clone(),
                exit_code: p.exit_code,
                duration_ms: p.duration_ms,
                stderr_tail: p.stderr_tail.clone(),
                ts: p.ts,
            };
            let dest = outcomes_dir().join(format!("{decision_id}.json"));
            match fs::write(&dest, serde_json::to_string(&resolved).unwrap_or_default()) {
                Ok(_) => {
                    claimed.insert(decision_id.clone());
                    let _ = fs::remove_file(&path);
                    stats.attributed += 1;
                }
                Err(_) => stats.errors += 1,
            }
        } else if now.saturating_sub(p.ts) > ORPHAN_AFTER_SECS {
            // Move to orphaned for inspection.
            let dest = orphaned_dir().join(
                path.file_name()
                    .map(|n| n.to_owned())
                    .unwrap_or_else(|| std::ffi::OsString::from("orphan.json")),
            );
            if fs::rename(&path, &dest).is_ok() {
                stats.orphaned += 1;
            } else {
                stats.errors += 1;
            }
        } else {
            stats.still_pending += 1;
        }
    }

    stats
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_stderr_short() {
        assert_eq!(truncate_stderr("hello"), "hello");
    }

    #[test]
    fn truncate_stderr_long_keeps_tail() {
        let s = "a".repeat(MAX_STDERR_TAIL_BYTES * 2);
        let t = truncate_stderr(&s);
        assert_eq!(t.len(), MAX_STDERR_TAIL_BYTES);
        assert!(t.chars().all(|c| c == 'a'));
    }

    #[test]
    fn truncate_stderr_respects_char_boundary() {
        // "é" is two bytes in UTF-8. Construct a string whose tail boundary
        // would split a multibyte char if we naively sliced.
        let mut s = String::new();
        for _ in 0..MAX_STDERR_TAIL_BYTES {
            s.push('é');
        }
        let t = truncate_stderr(&s);
        // Must be valid UTF-8 (the assertion is implicit in String — we just
        // verify it didn't panic and produced something <= cap bytes).
        assert!(t.len() <= MAX_STDERR_TAIL_BYTES);
    }

    #[test]
    fn normalize_command_collapses_whitespace() {
        assert_eq!(normalize_command("  cargo   test  "), "cargo test");
        assert_eq!(normalize_command("cargo\ttest"), "cargo test");
    }

    #[test]
    fn parse_ts_handles_quoted_and_plain() {
        assert_eq!(parse_ts("123"), Some(123));
        assert_eq!(parse_ts("\"123\""), Some(123));
        assert_eq!(parse_ts("not a number"), None);
    }

    #[test]
    fn pending_outcome_round_trip_json() {
        let p = PendingOutcome {
            tool: "Bash".into(),
            command: Some("cargo test".into()),
            project: "claudectl".into(),
            session_id: Some("sess-1".into()),
            tool_use_id: Some("tu-1".into()),
            exit_code: Some(0),
            duration_ms: Some(1234),
            stderr_tail: None,
            ts: 100,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: PendingOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tool, "Bash");
        assert_eq!(back.command.as_deref(), Some("cargo test"));
        assert_eq!(back.exit_code, Some(0));
    }

    #[test]
    fn pending_outcome_parses_minimal_json() {
        // Hook scripts may omit optional fields.
        let s = r#"{"tool":"Bash","project":"p","ts":1}"#;
        let p: PendingOutcome = serde_json::from_str(s).unwrap();
        assert_eq!(p.tool, "Bash");
        assert!(p.command.is_none());
        assert!(p.exit_code.is_none());
    }

    #[test]
    fn gen_pending_id_unique_within_process() {
        let a = gen_pending_id();
        let b = gen_pending_id();
        assert_ne!(a, b);
    }

    // ── Test-runner detection (#238) ──────────────────────────────────

    fn runners() -> Vec<String> {
        ["cargo test", "npm test", "pytest", "go test", "bun test"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn is_test_runner_cmd_matches_exact_prefix() {
        assert!(is_test_runner_cmd("cargo test", &runners()));
        assert!(is_test_runner_cmd("pytest", &runners()));
        assert!(is_test_runner_cmd("go test", &runners()));
    }

    #[test]
    fn is_test_runner_cmd_matches_with_args() {
        assert!(is_test_runner_cmd("cargo test --release", &runners()));
        assert!(is_test_runner_cmd("pytest tests/foo.py", &runners()));
        assert!(is_test_runner_cmd("npm test -- --watch=false", &runners()));
    }

    #[test]
    fn is_test_runner_cmd_case_insensitive_and_whitespace() {
        assert!(is_test_runner_cmd("  CARGO   TEST --release  ", &runners()));
        assert!(is_test_runner_cmd("Cargo\tTest", &runners()));
    }

    #[test]
    fn is_test_runner_cmd_rejects_unrelated() {
        assert!(!is_test_runner_cmd("ls", &runners()));
        assert!(!is_test_runner_cmd("cargo build", &runners()));
        // Substring without token boundary must not match.
        assert!(!is_test_runner_cmd("cargotest", &runners()));
        // Empty command is not a test run.
        assert!(!is_test_runner_cmd("", &runners()));
    }

    #[test]
    fn is_test_runner_cmd_empty_runners_never_matches() {
        assert!(!is_test_runner_cmd("cargo test", &[]));
    }

    #[test]
    fn test_failure_marker_round_trip_json() {
        let m = TestFailureMarker {
            decision_id: "dec_1_2_3".into(),
            failed_test_command: "cargo test".into(),
            outcome_ts: 100,
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: TestFailureMarker = serde_json::from_str(&s).unwrap();
        assert_eq!(back.decision_id, "dec_1_2_3");
        assert_eq!(back.failed_test_command, "cargo test");
        assert_eq!(back.outcome_ts, 100);
    }
}

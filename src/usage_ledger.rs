//! Persistent usage ledger — append-only per-message token totals, sourced
//! from every Claude Code JSONL transcript on disk (including subagents).
//!
//! This exists because `history::record_session` only fires when claudectl
//! observes a session transition into `Finished` — a race window that misses
//! any session closed via a terminal-close/SIGHUP (Claude Code deletes its
//! own pointer file on exit, so the next tick drops the session before
//! claudectl can write a history row). The ledger side-steps that race by
//! reading directly from `~/.claude/projects/**/*.jsonl`, which Claude Code
//! retains effectively forever.
//!
//! Cost is computed at read time (not stored in the CSV) so a fix to
//! `models.rs` pricing retroactively corrects every historical summary, and
//! so the raw token counts remain usable for future "what-if" queries.
//!
//! Format:
//!   CSV: ~/.local/share/claudectl/usage_log.csv
//!     timestamp_ms,session_id,model,fresh_input,cache_read,cache_write,output
//!   Offsets: ~/.local/share/claudectl/usage_offsets.json
//!     { "<jsonl-path>": { "last_byte": u64, "mtime_ms": u64 } }

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::models;
use crate::transcript::{TranscriptEvent, TranscriptRole, parse_line};

const LEDGER_BASENAME: &str = "usage_log.csv";
const OFFSETS_BASENAME: &str = "usage_offsets.json";
const HEADER: &str = "timestamp_ms,session_id,model,fresh_input,cache_read,cache_write,output";

/// Aggregated usage over a time window. Cost is computed from `model` at
/// read time using current `models.rs` pricing; historical pricing changes
/// therefore retroactively flow through.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSummary {
    pub fresh_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost_usd: f64,
    pub msg_count: u64,
}

impl UsageSummary {
    pub fn total_tokens(&self) -> u64 {
        self.fresh_input + self.cache_read + self.cache_write + self.output
    }
}

/// Result of a single `scan_and_append` invocation. Surfaced to the TUI so
/// the user can see "first scan indexed N messages" on startup.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanReport {
    pub files_scanned: usize,
    pub files_updated: usize,
    pub rows_appended: u64,
}

fn ledger_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local")
        .join("share")
        .join("claudectl")
}

fn projects_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".claude")
        .join("projects")
}

fn ledger_path() -> PathBuf {
    ledger_dir().join(LEDGER_BASENAME)
}

fn offsets_path() -> PathBuf {
    ledger_dir().join(OFFSETS_BASENAME)
}

#[derive(Debug, Clone, Default)]
struct FileOffset {
    last_byte: u64,
    mtime_ms: u64,
}

type OffsetMap = HashMap<String, FileOffset>;

fn load_offsets_at(path: &Path) -> OffsetMap {
    let Ok(raw) = fs::read_to_string(path) else {
        return OffsetMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return OffsetMap::new();
    };
    let Some(obj) = value.as_object() else {
        return OffsetMap::new();
    };
    let mut out = OffsetMap::new();
    for (k, v) in obj {
        let last_byte = v.get("last_byte").and_then(|n| n.as_u64()).unwrap_or(0);
        let mtime_ms = v.get("mtime_ms").and_then(|n| n.as_u64()).unwrap_or(0);
        out.insert(
            k.clone(),
            FileOffset {
                last_byte,
                mtime_ms,
            },
        );
    }
    out
}

fn save_offsets_at(path: &Path, offsets: &OffsetMap) {
    let mut obj = serde_json::Map::new();
    for (k, v) in offsets {
        let mut entry = serde_json::Map::new();
        entry.insert("last_byte".into(), Value::from(v.last_byte));
        entry.insert("mtime_ms".into(), Value::from(v.mtime_ms));
        obj.insert(k.clone(), Value::Object(entry));
    }
    let Ok(rendered) = serde_json::to_string(&Value::Object(obj)) else {
        return;
    };
    let _ = fs::write(path, rendered);
}

/// Recursively enumerate every `*.jsonl` under `~/.claude/projects`. Order
/// is filesystem-dependent; scan_and_append treats files independently so
/// order doesn't matter.
fn find_jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out
}

fn mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Session id carved from the JSONL filename stem. Works for both the
/// top-level `<uuid>.jsonl` and subagent `agent-*.jsonl` layouts — in the
/// latter case the string returned is the agent id, which is what we want
/// for attribution.
fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// CSV-safe rendering of a model string. Model names are lowercase
/// alphanumerics + `-` in practice, but we still defensively strip commas
/// and newlines so a pathological entry can't corrupt the ledger.
fn csv_escape(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != ',' && *c != '\n' && *c != '\r')
        .collect()
}

/// Scan every JSONL and append any new assistant `usage` blocks to the
/// ledger. Offsets persist across runs so subsequent scans are O(new bytes).
pub fn scan_and_append() -> ScanReport {
    scan_and_append_at(&projects_dir(), &ledger_path(), &offsets_path())
}

/// Testable variant: explicit paths. Production wrapper computes paths from
/// `$HOME` and delegates here.
pub fn scan_and_append_at(projects_root: &Path, ledger: &Path, offsets_file: &Path) -> ScanReport {
    if let Some(parent) = ledger.parent() {
        if fs::create_dir_all(parent).is_err() {
            return ScanReport::default();
        }
    }

    let needs_header = !ledger.exists();

    let Ok(ledger_file) = OpenOptions::new().create(true).append(true).open(ledger) else {
        return ScanReport::default();
    };
    let mut ledger_out = BufWriter::new(ledger_file);

    if needs_header {
        let _ = writeln!(ledger_out, "{HEADER}");
    }

    let mut offsets = load_offsets_at(offsets_file);
    let files = find_jsonl_files(projects_root);

    let mut report = ScanReport {
        files_scanned: files.len(),
        ..Default::default()
    };

    for jsonl in &files {
        let key = jsonl.display().to_string();
        let current_mtime = mtime_ms(jsonl);
        let current_size = fs::metadata(jsonl).map(|m| m.len()).unwrap_or(0);

        let prev = offsets.get(&key).cloned().unwrap_or_default();

        // Truncation / rewrite: fall back to full re-scan by resetting offset.
        let mut start = prev.last_byte;
        if current_size < prev.last_byte {
            start = 0;
        }
        if start == current_size {
            continue;
        }

        let Ok(mut file) = File::open(jsonl) else {
            continue;
        };
        if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
            continue;
        }

        let reader = BufReader::new(&file);
        let sid = session_id_from_path(jsonl);
        let mut appended = 0u64;

        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Some(TranscriptEvent::Message(msg)) = parse_line(&line) else {
                continue;
            };
            if msg.role != TranscriptRole::Assistant {
                continue;
            }
            let Some(usage) = msg.usage else { continue };
            if usage.input_tokens == 0
                && usage.cache_read_input_tokens == 0
                && usage.cache_creation_input_tokens == 0
                && usage.output_tokens == 0
            {
                continue;
            }
            let ts = msg.timestamp_ms.unwrap_or(current_mtime);
            let model = msg.model.as_deref().unwrap_or("");
            let row = format!(
                "{},{},{},{},{},{},{}",
                ts,
                csv_escape(&sid),
                csv_escape(model),
                usage.input_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
                usage.output_tokens,
            );
            if writeln!(ledger_out, "{row}").is_ok() {
                appended += 1;
            }
        }

        if appended > 0 {
            report.files_updated += 1;
            report.rows_appended += appended;
        }

        offsets.insert(
            key,
            FileOffset {
                last_byte: current_size,
                mtime_ms: current_mtime,
            },
        );
    }

    let _ = ledger_out.flush();
    save_offsets_at(offsets_file, &offsets);
    report
}

/// Aggregate ledger rows whose timestamp falls in `[since_ms, now)`. Pass
/// `since_ms == 0` for the full-history total. Cost is computed per row
/// using current `models::resolve` prices.
/// Single ledger row in cached form. Cost is pre-computed at parse time
/// so the hot summary path doesn't redo the model-pricing lookup. 48 bytes
/// per row; 31d of activity at the user's observed density (~50k rows/day)
/// fits in ~75 MB worst-case, ~5 MB typical.
#[derive(Debug, Clone, Copy)]
struct LedgerRow {
    ts_ms: u64,
    fresh_input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    cost_usd: f64,
}

/// Process-local cache of parsed ledger rows. Avoids re-parsing the entire
/// 100MB+ usage_log.csv every time `load_summary` is called (3× per
/// 6-second tick, ~750ms/pass in release-mode Rust at 1.4M rows). With the
/// cache, the only per-tick I/O is reading bytes appended since the last
/// scan — typically zero or a few KB — plus a linear scan of in-memory
/// rows whose ts_ms >= cutoff.
///
/// Rows older than `MAX_RETENTION_MS` are evicted on every refresh so
/// memory doesn't grow unbounded; the existing CSV file is never trimmed.
const MAX_RETENTION_MS: u64 = 31 * 86_400_000;

#[derive(Default)]
struct LedgerCache {
    last_scan_size: u64,
    rows: Vec<LedgerRow>,
}

fn cache() -> &'static Mutex<LedgerCache> {
    static CACHE: OnceLock<Mutex<LedgerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LedgerCache::default()))
}

/// Read newly-appended bytes since the last refresh and merge them into the
/// in-memory cache. On file truncation (size shrunk) the cache is reset and
/// re-scanned from the start. Errors are swallowed — a missing or
/// unreadable ledger leaves the cache empty, same as the prior file-only
/// implementation.
fn refresh_cache_from(ledger: &Path) {
    let Ok(meta) = fs::metadata(ledger) else {
        return;
    };
    let current_size = meta.len();
    let mut c = cache().lock().expect("ledger cache mutex poisoned");

    let start = if current_size < c.last_scan_size {
        // Truncation / rotation: fall back to a full re-scan.
        c.rows.clear();
        0
    } else if current_size == c.last_scan_size {
        return;
    } else {
        c.last_scan_size
    };

    let Ok(mut file) = File::open(ledger) else {
        return;
    };
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return;
    }

    let reader = BufReader::new(&file);
    for (idx, line) in reader.lines().enumerate() {
        let Ok(line) = line else { break };
        // The header line is only present at the start of the file; on
        // tail reads `start > 0` we never encounter it. On full reads
        // `start == 0` it's the first line (idx 0) and the prefix check
        // catches it.
        if start == 0 && idx == 0 && line.starts_with("timestamp_ms") {
            continue;
        }
        let Some(row) = parse_csv_row(&line) else {
            continue;
        };
        c.rows.push(row);
    }
    c.last_scan_size = current_size;

    // Evict rows older than MAX_RETENTION_MS. CSV is append-only with
    // monotonic timestamps, so the cache is sorted ascending and the
    // eviction is a prefix drop. Note: scan_and_append walks JSONL files
    // in directory-walk order (not strictly time-sorted), so there can
    // be small local out-of-order ranges. They're bounded to a single
    // refresh batch and don't break the bulk-prefix eviction below — at
    // worst a row up to MAX_RETENTION_MS ago survives one extra cycle.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cutoff = now.saturating_sub(MAX_RETENTION_MS);
    let evict_to = c.rows.partition_point(|r| r.ts_ms < cutoff);
    if evict_to > 0 {
        c.rows.drain(..evict_to);
    }
}

fn parse_csv_row(line: &str) -> Option<LedgerRow> {
    let fields: Vec<&str> = line.splitn(7, ',').collect();
    if fields.len() != 7 {
        return None;
    }
    let ts_ms: u64 = fields[0].parse().ok()?;
    // fields[1] = session_id (unused for summary)
    let model = fields[2];
    let fresh: u64 = fields[3].parse().unwrap_or(0);
    let cache_read: u64 = fields[4].parse().unwrap_or(0);
    let cache_write: u64 = fields[5].parse().unwrap_or(0);
    let output: u64 = fields[6].parse().unwrap_or(0);

    let p = models::resolve(model).profile;
    let cost = (fresh as f64 * p.input_per_m
        + cache_read as f64 * p.cache_read_per_m
        + cache_write as f64 * p.cache_write_per_m
        + output as f64 * p.output_per_m)
        / 1_000_000.0;
    Some(LedgerRow {
        ts_ms,
        fresh_input: fresh,
        cache_read,
        cache_write,
        output,
        cost_usd: cost,
    })
}

pub fn load_summary(since_ms: u64) -> UsageSummary {
    refresh_cache_from(&ledger_path());
    summarize_cached(since_ms)
}

fn summarize_cached(since_ms: u64) -> UsageSummary {
    let c = cache().lock().expect("ledger cache mutex poisoned");
    let mut summary = UsageSummary::default();
    // Rows are sorted by ts_ms ascending (CSV is append-only); the cutoff
    // walk skips the older prefix in O(log N) and accumulates O(K) where
    // K = rows since the cutoff.
    let start = c.rows.partition_point(|r| r.ts_ms < since_ms);
    for row in &c.rows[start..] {
        summary.fresh_input += row.fresh_input;
        summary.cache_read += row.cache_read;
        summary.cache_write += row.cache_write;
        summary.output += row.output;
        summary.cost_usd += row.cost_usd;
        summary.msg_count += 1;
    }
    summary
}

#[cfg(test)]
fn reset_cache_for_tests() {
    let mut c = cache().lock().expect("ledger cache mutex poisoned");
    c.last_scan_size = 0;
    c.rows.clear();
}

/// Testable variant: explicit ledger path, NO cache. Used by tests so each
/// test sees only its own fixture; the production path uses the
/// process-wide cache via `load_summary`.
#[cfg(test)]
pub fn load_summary_at(ledger: &Path, since_ms: u64) -> UsageSummary {
    let Ok(file) = File::open(ledger) else {
        return UsageSummary::default();
    };
    let reader = BufReader::new(file);
    let mut summary = UsageSummary::default();
    for (idx, line) in reader.lines().enumerate() {
        let Ok(line) = line else { break };
        if idx == 0 && line.starts_with("timestamp_ms") {
            continue;
        }
        let Some(row) = parse_csv_row(&line) else {
            continue;
        };
        if row.ts_ms < since_ms {
            continue;
        }
        summary.fresh_input += row.fresh_input;
        summary.cache_read += row.cache_read;
        summary.cache_write += row.cache_write;
        summary.output += row.output;
        summary.cost_usd += row.cost_usd;
        summary.msg_count += 1;
    }
    summary
}

/// Convenience: current unix time in ms.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Each test gets its own tmp subdirectory; counter ensures uniqueness
    /// even when tests run in parallel.
    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TestPaths {
        _root: PathBuf, // kept alive to own the tmp tree
        projects: PathBuf,
        ledger: PathBuf,
        offsets: PathBuf,
    }

    impl TestPaths {
        fn new(label: &str) -> Self {
            let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "claudectl-ledger-{}-{}-{n}",
                std::process::id(),
                label
            ));
            let _ = fs::remove_dir_all(&root);
            let projects = root.join("projects");
            let share = root.join("share");
            fs::create_dir_all(&projects).unwrap();
            fs::create_dir_all(&share).unwrap();
            Self {
                ledger: share.join("usage_log.csv"),
                offsets: share.join("usage_offsets.json"),
                projects,
                _root: root,
            }
        }

        fn scan(&self) -> ScanReport {
            scan_and_append_at(&self.projects, &self.ledger, &self.offsets)
        }

        fn summary(&self, since_ms: u64) -> UsageSummary {
            load_summary_at(&self.ledger, since_ms)
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self._root);
        }
    }

    fn write_tmp(path: &Path, contents: &str) {
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        let mut f = File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    fn fixture_assistant_line(
        ts: &str,
        model: &str,
        inp: u64,
        cr: u64,
        cw: u64,
        out: u64,
    ) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","model":"{model}","usage":{{"input_tokens":{inp},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":{cw},"output_tokens":{out}}},"content":[]}}}}"#
        )
    }

    #[test]
    fn scan_appends_assistant_usage_rows() {
        let p = TestPaths::new("scan-basic");
        let project = p.projects.join("-test/sess-abc.jsonl");
        let content = [
            fixture_assistant_line(
                "2026-04-22T10:00:00.000Z",
                "claude-opus-4-7",
                100,
                50,
                10,
                200,
            ),
            fixture_assistant_line(
                "2026-04-22T10:01:00.000Z",
                "claude-sonnet-4-6",
                80,
                20,
                5,
                120,
            ),
        ]
        .join("\n");
        write_tmp(&project, &content);

        let report = p.scan();
        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.files_updated, 1);
        assert_eq!(report.rows_appended, 2);

        let summary = p.summary(0);
        assert_eq!(summary.msg_count, 2);
        assert_eq!(summary.fresh_input, 180);
        assert_eq!(summary.cache_read, 70);
        assert_eq!(summary.cache_write, 15);
        assert_eq!(summary.output, 320);
        assert!(summary.cost_usd > 0.0);
    }

    #[test]
    fn scan_is_incremental_across_runs() {
        let p = TestPaths::new("incremental");
        let project = p.projects.join("-test/sess-x.jsonl");
        write_tmp(
            &project,
            &fixture_assistant_line("2026-04-22T10:00:00.000Z", "claude-opus-4-7", 10, 0, 0, 5),
        );
        let r1 = p.scan();
        assert_eq!(r1.rows_appended, 1);

        // Append another message to the same JSONL.
        let mut f = OpenOptions::new().append(true).open(&project).unwrap();
        writeln!(
            f,
            "\n{}",
            fixture_assistant_line("2026-04-22T10:05:00.000Z", "claude-opus-4-7", 30, 0, 0, 7)
        )
        .unwrap();
        drop(f);

        let r2 = p.scan();
        assert_eq!(r2.rows_appended, 1, "only new bytes should be re-parsed");

        let summary = p.summary(0);
        assert_eq!(summary.msg_count, 2);
        assert_eq!(summary.fresh_input, 40);
        assert_eq!(summary.output, 12);
    }

    #[test]
    fn user_messages_and_zero_usage_are_ignored() {
        let p = TestPaths::new("filter");
        let project = p.projects.join("-test/sess-y.jsonl");
        let content = [
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#.to_string(),
            fixture_assistant_line("2026-04-22T10:00:00.000Z", "claude-opus-4-7", 0, 0, 0, 0),
            fixture_assistant_line("2026-04-22T10:01:00.000Z", "claude-opus-4-7", 1, 0, 0, 2),
        ]
        .join("\n");
        write_tmp(&project, &content);

        let report = p.scan();
        assert_eq!(report.rows_appended, 1);
        let summary = p.summary(0);
        assert_eq!(summary.msg_count, 1);
        assert_eq!(summary.fresh_input, 1);
        assert_eq!(summary.output, 2);
    }

    #[test]
    fn since_filter_windows_ledger_by_timestamp() {
        let p = TestPaths::new("since");
        let project = p.projects.join("-test/sess-z.jsonl");
        let content = [
            fixture_assistant_line("2026-04-20T10:00:00.000Z", "claude-opus-4-7", 100, 0, 0, 50),
            fixture_assistant_line("2026-04-22T10:00:00.000Z", "claude-opus-4-7", 10, 0, 0, 5),
        ]
        .join("\n");
        write_tmp(&project, &content);
        p.scan();

        // 2026-04-21T00:00:00 UTC ≈ 1776844800000 ms
        let cutoff = 1776844800000u64;
        let recent = p.summary(cutoff);
        assert_eq!(recent.msg_count, 1);
        assert_eq!(recent.fresh_input, 10);

        let all = p.summary(0);
        assert_eq!(all.msg_count, 2);
        assert_eq!(all.fresh_input, 110);
    }

    #[test]
    fn subagent_files_are_scanned_too() {
        let p = TestPaths::new("subagents");
        let sub = p
            .projects
            .join("-test/parent-session/subagents/agent-abc.jsonl");
        write_tmp(
            &sub,
            &fixture_assistant_line("2026-04-22T10:00:00.000Z", "claude-haiku", 100, 0, 0, 50),
        );
        let r = p.scan();
        assert_eq!(r.files_scanned, 1);
        assert_eq!(r.rows_appended, 1);
        let s = p.summary(0);
        assert_eq!(s.fresh_input, 100);
        assert_eq!(s.output, 50);
    }

    // ---- Cache tests --------------------------------------------------
    //
    // The cache is a process-global singleton, so these tests must
    // serialise via the same mutex `cache()` returns. Calling
    // `reset_cache_for_tests()` between scenarios + naming each ledger
    // file uniquely (TestPaths counter) keeps them order-independent.

    fn write_csv_rows(path: &Path, rows: &[(u64, &str, u64, u64, u64, u64)]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut body = String::from(HEADER);
        body.push('\n');
        for (ts, model, fresh, cr, cw, out) in rows {
            body.push_str(&format!("{ts},sess,{model},{fresh},{cr},{cw},{out}\n"));
        }
        std::fs::write(path, body).unwrap();
    }

    fn append_csv_rows(path: &Path, rows: &[(u64, &str, u64, u64, u64, u64)]) {
        let mut body = String::new();
        for (ts, model, fresh, cr, cw, out) in rows {
            body.push_str(&format!("{ts},sess,{model},{fresh},{cr},{cw},{out}\n"));
        }
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    /// All cache tests share the global cache, so each test owns its file
    /// and resets state via `reset_cache_for_tests()`. Keep them in one
    /// `#[test]` so they run sequentially under any test-thread setting.
    #[test]
    fn cache_refresh_is_incremental_truncation_safe_and_filters_by_cutoff() {
        let p = TestPaths::new("cache");
        let ledger = p.ledger.clone();
        // Use real-clock-relative timestamps so MAX_RETENTION_MS doesn't
        // evict the test rows: a row from 2 hours ago is well within the
        // 31-day window. Spacing 60s apart keeps the cutoff filter test
        // unambiguous.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let t1 = now - 7_200_000; // 2h ago
        let t2 = t1 + 60_000;
        let t3 = t2 + 60_000;
        let cutoff_between_t2_t3 = t2 + 30_000;

        // Initial population: cache is empty, refresh reads everything.
        reset_cache_for_tests();
        write_csv_rows(
            &ledger,
            &[
                (t1, "claude-opus-4-7", 100, 0, 0, 50),
                (t2, "claude-opus-4-7", 200, 0, 0, 100),
            ],
        );
        refresh_cache_from(&ledger);
        let s = summarize_cached(0);
        assert_eq!(s.msg_count, 2);
        assert_eq!(s.fresh_input, 300);

        // Append: only the new row should be parsed; cache must show
        // 3 rows total without re-reading the first 2.
        append_csv_rows(&ledger, &[(t3, "claude-opus-4-7", 5, 0, 0, 1)]);
        refresh_cache_from(&ledger);
        let s = summarize_cached(0);
        assert_eq!(s.msg_count, 3);
        assert_eq!(s.fresh_input, 305);

        // Cutoff filter: only rows at-or-after `since_ms` count.
        let s = summarize_cached(cutoff_between_t2_t3);
        assert_eq!(s.msg_count, 1);
        assert_eq!(s.fresh_input, 5);

        // File truncation/rotation: write a smaller file in place. Cache
        // must reset and re-scan from byte 0.
        let t4 = now - 60_000;
        write_csv_rows(&ledger, &[(t4, "claude-opus-4-7", 7, 0, 0, 0)]);
        refresh_cache_from(&ledger);
        let s = summarize_cached(0);
        assert_eq!(s.msg_count, 1);
        assert_eq!(s.fresh_input, 7);

        // Cleanup so a later test starting with the same global cache
        // has no leftover rows from this one.
        reset_cache_for_tests();
    }

    #[test]
    fn cache_evicts_rows_older_than_max_retention() {
        let p = TestPaths::new("cache_evict");
        let ledger = p.ledger.clone();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // One row inside the retention window, one well outside it.
        let recent = now - 60_000;
        let ancient = now.saturating_sub(MAX_RETENTION_MS + 86_400_000);

        reset_cache_for_tests();
        write_csv_rows(
            &ledger,
            &[
                (ancient, "claude-opus-4-7", 999, 0, 0, 0),
                (recent, "claude-opus-4-7", 1, 0, 0, 0),
            ],
        );
        refresh_cache_from(&ledger);
        // Asking for the full window must yield only the recent row;
        // the ancient one was evicted on refresh.
        let s = summarize_cached(0);
        assert_eq!(s.msg_count, 1);
        assert_eq!(s.fresh_input, 1);

        reset_cache_for_tests();
    }
}

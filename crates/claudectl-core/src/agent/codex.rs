//! Codex session discovery, from the rollout transcripts Codex writes.
//!
//! ## Why there is no process here
//!
//! Claude Code writes `~/.claude/sessions/<pid>.json` per session, so
//! liveness is exact: the pid either exists or it does not. Codex has no
//! such file, and — measured on a real machine — no usable process mapping
//! either. The ChatGPT desktop app runs Codex as **one** long-lived
//! `codex … app-server` process serving every session, alongside 27 Electron
//! helpers. A terminal `codex` would be its own process, but nothing
//! distinguishes which session a given process owns.
//!
//! So a Codex session is discovered from its transcript, and the
//! consequences are honest rather than papered over:
//!
//! * There is **no pid**, so `ClaudeSession::pid` is 0 and the session is not
//!   actionable — see `ClaudeSession::is_actionable`. Sending input to it or
//!   killing it is not merely unimplemented; with the app there is no process
//!   that would mean the right thing.
//! * There is **no cost**. Codex is subscription-billed and rollouts record
//!   tokens only, so `cost_usd` stays 0 with `usage_metrics_available`
//!   false — absent, rather than a `$0.00` that would understate a fleet
//!   total.
//! * There is **no session-end event**. `task_complete` ends a *turn*, not a
//!   session, and a session the user closed is byte-identical to one sitting
//!   idle. Liveness is therefore a recency window, and a crashed session mid
//!   turn reads as working until that window expires. `CODEX_SESSION_TTL_SECS`
//!   is the whole of the liveness model, and that is a property of what Codex
//!   records, not a shortcut.
//!
//! ## Layout
//!
//! ```text
//! ~/.codex/sessions/YYYY/MM/DD/rollout-<ISO-ts>-<uuid>.jsonl
//! ```
//!
//! One JSON object per line: `{timestamp, ordinal, type, payload}`. The types
//! this module reads are `session_meta` (identity and cwd),
//! `event_msg/token_count` (usage and the context window),
//! `event_msg/{task_started,task_complete,turn_aborted}` (status), and
//! `response_item/{function_call,custom_tool_call}` (the pending tool).

use std::fs;
use std::path::{Path, PathBuf};

use crate::helpers::dirs_home;

/// How long after its last write a rollout still counts as a live session.
///
/// Chosen from the 671 rollouts on a development machine: 0 files touched in
/// the last hour, 1 in four hours, 25 in a week. The distribution is bimodal
/// — a session is either being written to right now or was abandoned hours
/// ago — so anything from a few minutes to a few hours lists the same
/// sessions. 30 minutes is the span in which "the turn finished and I am
/// still at my desk" is plausible; past it, the file is a transcript rather
/// than a session.
///
/// Raising this costs precision in exactly one way: a Codex session that
/// crashed mid-turn keeps reading as working until the window expires,
/// because Codex records no session-end event to distinguish it.
pub const CODEX_SESSION_TTL_SECS: u64 = 30 * 60;

/// The lifecycle state the transcript last recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lifecycle {
    /// A turn is in flight: `task_started` with no matching terminal event.
    Working,
    /// The last turn finished (`task_complete`) or was abandoned
    /// (`turn_aborted`). The *session* may still be open — Codex records no
    /// difference.
    Idle,
    /// No lifecycle event at all. A session that opened and was closed before
    /// the first turn.
    #[default]
    Unknown,
}

/// What one rollout transcript says about its session.
#[derive(Debug, Clone, Default)]
pub struct RolloutSummary {
    /// Codex's own session id, from `session_meta`.
    pub session_id: String,
    /// Working directory the session was started in.
    pub cwd: String,
    /// Epoch seconds of the first event.
    pub started_at_secs: u64,
    /// Epoch seconds of the last event carrying a timestamp.
    pub last_event_secs: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// `total_tokens` as Codex reports it — what the context window is
    /// measured against.
    pub total_tokens: u64,
    /// The model's context window, when the transcript says.
    pub context_window: Option<u64>,
    pub model: Option<String>,
    pub lifecycle: Lifecycle,
    /// Tool calls seen, for the activity signal.
    pub tool_calls: u32,
    /// The most recent tool call's name, already canonicalized into the
    /// cross-agent vocabulary (#522) so it compares with a Claude session's.
    pub last_tool: Option<String>,
    /// The transcript this came from.
    pub path: PathBuf,
}

/// `~/.codex/sessions`.
pub fn sessions_root() -> PathBuf {
    super::home_for(super::vocab::CODEX)
        .unwrap_or_else(|| dirs_home().join(".codex"))
        .join("sessions")
}

/// Every rollout whose last write is within `ttl_secs` of `now_secs`, parsed.
///
/// Walks only `YYYY/MM/DD` depth, so an unrelated file dropped in
/// `~/.codex/sessions` costs one `read_dir` rather than a recursive sweep.
/// Returns newest-first.
pub fn scan(now_secs: u64, ttl_secs: u64) -> Vec<RolloutSummary> {
    let mut out = Vec::new();
    for path in recent_rollout_paths(&sessions_root(), now_secs, ttl_secs) {
        match fs::read_to_string(&path) {
            Ok(content) => {
                if let Some(mut summary) = parse_rollout(&content) {
                    summary.path = path;
                    out.push(summary);
                }
            }
            Err(e) => crate::logger::log(
                "DEBUG",
                &format!("codex rollout read failed: {}: {e}", path.display()),
            ),
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.last_event_secs));
    out
}

/// Rollout paths under `root` modified within the window, newest first.
fn recent_rollout_paths(root: &Path, now_secs: u64, ttl_secs: u64) -> Vec<PathBuf> {
    let mut found: Vec<(PathBuf, u64)> = Vec::new();
    // sessions/<year>/<month>/<day>/*.jsonl — three levels, no deeper.
    for year in subdirs(root) {
        for month in subdirs(&year) {
            for day in subdirs(&month) {
                let Ok(entries) = fs::read_dir(&day) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Some(mtime) = mtime_secs(&entry) else {
                        continue;
                    };
                    if now_secs.saturating_sub(mtime) <= ttl_secs {
                        found.push((path, mtime));
                    }
                }
            }
        }
    }
    found.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    found.into_iter().map(|(p, _)| p).collect()
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn mtime_secs(entry: &fs::DirEntry) -> Option<u64> {
    entry
        .metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Parse a rollout transcript. Pure, so it is testable without a filesystem.
///
/// `None` when no `session_meta` was found — a file that is not a rollout, or
/// one truncated before its first line landed.
///
/// A line that fails to parse is skipped rather than failing the file: a
/// transcript being appended to right now routinely ends mid-line.
pub fn parse_rollout(content: &str) -> Option<RolloutSummary> {
    let mut s = RolloutSummary::default();
    let mut saw_meta = false;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if let Some(secs) = event
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(crate::history::parse_timestamp_epoch)
        {
            if s.started_at_secs == 0 {
                s.started_at_secs = secs;
            }
            s.last_event_secs = s.last_event_secs.max(secs);
        }

        let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let payload = event.get("payload");
        let payload_kind = payload
            .and_then(|p| p.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match (kind, payload_kind) {
            ("session_meta", _) => {
                saw_meta = true;
                if let Some(p) = payload {
                    // `session_id` is the thread; `id` is this rollout. Prefer
                    // the thread so a resumed session keeps one identity.
                    if let Some(id) = p
                        .get("session_id")
                        .or_else(|| p.get("id"))
                        .and_then(|v| v.as_str())
                    {
                        s.session_id = id.to_string();
                    }
                    if let Some(cwd) = p.get("cwd").and_then(|v| v.as_str()) {
                        s.cwd = cwd.to_string();
                    }
                }
            }
            ("event_msg", "token_count") => {
                if let Some(info) = payload.and_then(|p| p.get("info")) {
                    if let Some(total) = info.get("total_token_usage") {
                        s.input_tokens = u64_at(total, "input_tokens");
                        s.cached_input_tokens = u64_at(total, "cached_input_tokens");
                        s.output_tokens = u64_at(total, "output_tokens");
                        s.total_tokens = u64_at(total, "total_tokens");
                    }
                    if let Some(window) = info.get("model_context_window").and_then(|v| v.as_u64())
                    {
                        s.context_window = Some(window);
                    }
                }
            }
            ("event_msg", "task_started") => s.lifecycle = Lifecycle::Working,
            ("event_msg", "task_complete" | "turn_aborted") => s.lifecycle = Lifecycle::Idle,
            ("turn_context", _) => {
                if let Some(model) = payload
                    .and_then(|p| p.get("model"))
                    .and_then(|v| v.as_str())
                {
                    s.model = Some(model.to_string());
                }
            }
            ("response_item", "function_call" | "custom_tool_call") => {
                s.tool_calls = s.tool_calls.saturating_add(1);
                if let Some(p) = payload {
                    if let Some(name) = p.get("name").and_then(|v| v.as_str()) {
                        let namespace = p.get("namespace").and_then(|v| v.as_str());
                        let arg = p
                            .get("input")
                            .or_else(|| p.get("arguments"))
                            .and_then(|v| v.as_str());
                        s.last_tool = Some(super::vocab::canonical_tool(
                            super::vocab::CODEX,
                            namespace,
                            name,
                            arg,
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    saw_meta.then_some(s)
}

fn u64_at(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped from a real transcript (§9.1 of docs/cross-agent-hive.md).
    fn rollout_fixture() -> String {
        [
            r#"{"timestamp":"2026-10-11T19:08:37.776Z","ordinal":0,"type":"session_meta","payload":{"session_id":"01a12c5d-36b7-7ec3-bead-f78afff85501","id":"01a12c5d-f5ba-7783-96fa-69029d38aec6","cwd":"/Users/dev/project","runtime_workspace_roots":["/Users/dev"]}}"#,
            r#"{"timestamp":"2026-10-11T19:08:38.000Z","ordinal":1,"type":"turn_context","payload":{"model":"gpt-6-astra"}}"#,
            r#"{"timestamp":"2026-10-11T19:08:39.000Z","ordinal":2,"type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"timestamp":"2026-10-11T19:08:40.000Z","ordinal":3,"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"c1"}}"#,
            r#"{"timestamp":"2026-10-11T19:08:41.000Z","ordinal":4,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":33434,"cached_input_tokens":22912,"output_tokens":215,"total_tokens":33649},"model_context_window":272000}}}"#,
            r#"{"timestamp":"2026-10-11T19:08:42.000Z","ordinal":5,"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn parses_identity_and_cwd() {
        let s = parse_rollout(&rollout_fixture()).expect("fixture should parse");
        assert_eq!(s.session_id, "01a12c5d-36b7-7ec3-bead-f78afff85501");
        assert_eq!(s.cwd, "/Users/dev/project");
    }

    /// `session_id` is the thread and `id` is this rollout. Preferring the
    /// thread keeps one identity across a resume; preferring `id` would make
    /// a resumed session look like a new one.
    #[test]
    fn prefers_the_thread_id_over_the_rollout_id() {
        let line = r#"{"type":"session_meta","payload":{"session_id":"thread","id":"rollout","cwd":"/x"}}"#;
        let s = parse_rollout(line).unwrap();
        assert_eq!(s.session_id, "thread");
    }

    /// An older rollout may carry only `id`.
    #[test]
    fn falls_back_to_the_rollout_id() {
        let line = r#"{"type":"session_meta","payload":{"id":"rollout-only","cwd":"/x"}}"#;
        let s = parse_rollout(line).unwrap();
        assert_eq!(s.session_id, "rollout-only");
    }

    #[test]
    fn parses_usage_and_the_context_window() {
        let s = parse_rollout(&rollout_fixture()).unwrap();
        assert_eq!(s.input_tokens, 33434);
        assert_eq!(s.cached_input_tokens, 22912);
        assert_eq!(s.output_tokens, 215);
        assert_eq!(s.total_tokens, 33649);
        assert_eq!(
            s.context_window,
            Some(272_000),
            "model_context_window is present, so context pressure needs no new signal"
        );
    }

    #[test]
    fn parses_model_from_turn_context() {
        let s = parse_rollout(&rollout_fixture()).unwrap();
        assert_eq!(s.model.as_deref(), Some("gpt-6-astra"));
    }

    /// The last lifecycle event wins: this fixture ends on `task_complete`.
    #[test]
    fn a_finished_turn_reads_as_idle() {
        let s = parse_rollout(&rollout_fixture()).unwrap();
        assert_eq!(s.lifecycle, Lifecycle::Idle);
    }

    /// `task_started` with no terminal event is a turn in flight.
    #[test]
    fn an_unfinished_turn_reads_as_working() {
        let lines = [
            r#"{"type":"session_meta","payload":{"session_id":"s","cwd":"/x"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]
        .join("\n");
        assert_eq!(parse_rollout(&lines).unwrap().lifecycle, Lifecycle::Working);
    }

    /// `turn_aborted` is a finished turn too — the user interrupted it.
    #[test]
    fn an_aborted_turn_reads_as_idle() {
        let lines = [
            r#"{"type":"session_meta","payload":{"session_id":"s","cwd":"/x"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"type":"event_msg","payload":{"type":"turn_aborted"}}"#,
        ]
        .join("\n");
        assert_eq!(parse_rollout(&lines).unwrap().lifecycle, Lifecycle::Idle);
    }

    #[test]
    fn a_session_with_no_turn_is_unknown() {
        let line = r#"{"type":"session_meta","payload":{"session_id":"s","cwd":"/x"}}"#;
        assert_eq!(parse_rollout(line).unwrap().lifecycle, Lifecycle::Unknown);
    }

    /// The pending tool is recorded in the canonical vocabulary (#522), so it
    /// compares with a Claude session's `pending_tool_name`.
    #[test]
    fn the_last_tool_is_canonicalized() {
        let s = parse_rollout(&rollout_fixture()).unwrap();
        assert_eq!(
            s.last_tool.as_deref(),
            Some("Bash"),
            "exec_command must surface as Bash, not as its Codex spelling"
        );
        assert_eq!(s.tool_calls, 1);
    }

    /// An `apply_patch` needs its envelope to tell Write from Edit, and the
    /// envelope is in `input` for a custom_tool_call.
    #[test]
    fn apply_patch_uses_its_envelope() {
        let lines = [
            r#"{"type":"session_meta","payload":{"session_id":"s","cwd":"/x"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Add File: new.rs\n+x"}}"#,
        ]
        .join("\n");
        assert_eq!(
            parse_rollout(&lines).unwrap().last_tool.as_deref(),
            Some("Write")
        );
    }

    #[test]
    fn timestamps_bracket_the_session() {
        let s = parse_rollout(&rollout_fixture()).unwrap();
        assert!(s.started_at_secs > 0);
        assert!(
            s.last_event_secs >= s.started_at_secs,
            "last event must not precede the first"
        );
    }

    /// A transcript being appended to right now routinely ends mid-line, so a
    /// trailing partial line must not lose the whole file.
    #[test]
    fn a_truncated_trailing_line_is_skipped() {
        let content = format!("{}\n{{\"timestamp\":\"2026-10", rollout_fixture());
        let s = parse_rollout(&content).expect("a partial last line must not fail the file");
        assert_eq!(s.session_id, "01a12c5d-36b7-7ec3-bead-f78afff85501");
        assert_eq!(s.total_tokens, 33649);
    }

    /// Without `session_meta` this is not a rollout.
    #[test]
    fn a_file_without_session_meta_is_not_a_session() {
        assert!(
            parse_rollout(r#"{"type":"event_msg","payload":{"type":"task_started"}}"#).is_none()
        );
        assert!(parse_rollout("").is_none());
        assert!(parse_rollout("not json at all").is_none());
    }

    #[test]
    fn sessions_root_is_under_the_codex_home() {
        let root = sessions_root();
        assert!(root.ends_with("sessions"));
        assert!(root.to_string_lossy().contains(".codex"));
    }

    // ── Scanning ──────────────────────────────────────────────────────────

    #[test]
    fn recent_paths_respect_the_ttl_window() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("10").join("11");
        fs::create_dir_all(&day).unwrap();

        let rollout = day.join("rollout-a.jsonl");
        let mut f = fs::File::create(&rollout).unwrap();
        writeln!(f, "{}", rollout_fixture()).unwrap();
        // Not a rollout; must be ignored on extension alone.
        fs::write(day.join("notes.txt"), "x").unwrap();

        let mtime = mtime_of(&rollout);
        let found = recent_rollout_paths(dir.path(), mtime, 600);
        assert_eq!(found.len(), 1, "only the .jsonl should be picked up");
        assert_eq!(found[0], rollout);

        // Same file, asked about long after its write.
        let stale = recent_rollout_paths(dir.path(), mtime + 10_000, 600);
        assert!(
            stale.is_empty(),
            "past the window a rollout is a transcript, not a session"
        );
    }

    #[test]
    fn scanning_a_missing_root_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(recent_rollout_paths(&dir.path().join("nope"), 0, 600).is_empty());
    }

    fn mtime_of(path: &Path) -> u64 {
        fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}

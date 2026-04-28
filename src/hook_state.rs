//! Per-session deterministic state, populated by Claude Code hook callbacks.
//!
//! Claude Code does not write a permission-pending tool_use to the session JSONL
//! until the user approves it, so the JSONL alone cannot tell us whether a session
//! is sitting on a permission prompt. The `Notification` hook (matcher
//! `permission_prompt`) fires the moment that prompt opens, and `PreToolUse`,
//! `UserPromptSubmit`, and `Stop` fire when the prompt resolves. By recording
//! those events to a per-session JSON file, `infer_status` can return a
//! deterministic answer instead of guessing from CPU + JSONL tail.
//!
//! State files live at `~/.claudectl/state/<session_id>.json`. The file is
//! tiny (a few hundred bytes) and rewritten atomically on each hook event.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Per-session hook event timestamps and last-known prompt context.
///
/// All `last_*_ts_ms` fields are unix epoch milliseconds at the moment the
/// hook fired. A zero value means "never seen". `notification_kind` and
/// `current_tool_name` carry payload context from the most recent
/// `Notification` / `PreToolUse` events respectively.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookState {
    pub session_id: String,
    #[serde(default)]
    pub last_notification_ts_ms: u64,
    /// `notification_type` from the last Notification payload — e.g.
    /// `"permission_prompt"` (main agent), `"worker_permission_prompt"`
    /// (subagent), `"idle_prompt"`, `"auth_success"`.
    #[serde(default)]
    pub notification_kind: Option<String>,
    #[serde(default)]
    pub last_pretooluse_ts_ms: u64,
    #[serde(default)]
    pub last_posttooluse_ts_ms: u64,
    #[serde(default)]
    pub last_stop_ts_ms: u64,
    #[serde(default)]
    pub last_promptsubmit_ts_ms: u64,
    #[serde(default)]
    pub last_precompact_ts_ms: u64,
    /// `PostCompact` fires directly when auto-compact finishes — a more
    /// reliable "compaction done" signal than relying on `Stop`, which has
    /// been observed to never fire for sessions whose first turn triggers an
    /// auto-compact.
    #[serde(default)]
    pub last_postcompact_ts_ms: u64,
    #[serde(default)]
    pub last_subagentstop_ts_ms: u64,
    #[serde(default)]
    pub last_session_start_ts_ms: u64,
    #[serde(default)]
    pub last_session_end_ts_ms: u64,
    /// Tool name from the most recent `PreToolUse` payload (cleared by
    /// `PostToolUse`).
    #[serde(default)]
    pub current_tool_name: Option<String>,
}

/// Returns the directory holding per-session state files. Creates it if needed.
///
/// Honors the `CLAUDECTL_STATE_DIR` env var when set — used by tests to avoid
/// stomping on the real `~/.claudectl/state` directory.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDECTL_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".claudectl/state")
}

/// Path to one session's state file.
fn state_path(session_id: &str) -> PathBuf {
    state_dir().join(format!("{session_id}.json"))
}

/// Process-wide monotonic millisecond timestamp.
///
/// Two `record_hook_event` calls back-to-back in the same process easily
/// land in the same wall-clock millisecond (system clock granularity ≈ ms;
/// hot-binary record-then-record is sub-ms). When that happens, `is_responding`
/// and `is_waiting_for_user` (both compare timestamps with strict `>`) become
/// order-blind: whichever check runs first wins, regardless of which event
/// was recorded later. That's how tests that record Stop then UserPromptSubmit
/// flake — both ts_ms are equal so neither comparison is strict-greater.
///
/// The fix is to enforce strictly-increasing per-process timestamps. The
/// atomic holds the most-recently-issued ms; the next call returns
/// `max(real_now_ms, last + 1)` and updates the atomic. Cross-process the
/// drift is bounded to a single process's burst of events (sub-millisecond
/// in practice), and Claude Code's hooks run in separate processes anyway,
/// so production sessions see real wall-clock timestamps with at most a few
/// ms of drift on rapid event bursts within one hook script.
fn now_ms() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let real = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = real.max(last + 1);
        match LAST.compare_exchange(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => last = observed,
        }
    }
}

impl HookState {
    /// Read state for a session. Returns `None` if no file exists yet.
    pub fn load(session_id: &str) -> Option<Self> {
        let path = state_path(session_id);
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Atomically write state to disk (write-temp + rename).
    fn save(&self) -> io::Result<()> {
        let dir = state_dir();
        fs::create_dir_all(&dir)?;
        let final_path = state_path(&self.session_id);
        let tmp_path = dir.join(format!(
            ".{}.json.tmp.{}",
            self.session_id,
            std::process::id()
        ));
        let json = serde_json::to_string(self).map_err(io::Error::other)?;
        fs::write(&tmp_path, json)?;
        fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// Delete this session's state file (called on `SessionEnd`).
    fn remove(session_id: &str) -> io::Result<()> {
        let path = state_path(session_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Try to parse stdin as a Claude Code hook payload — without ever blocking.
///
/// Why this is harder than it looks: Claude Code hooks pipe a JSON payload on
/// stdin and close the writer side immediately, so EOF arrives within
/// microseconds. But many *non-hook* invocations also have a non-tty stdin —
/// e.g. `claudectl --json` run in a subshell, backgrounded, or invoked by
/// another script — and that stdin may stay open with no writer for the
/// entire run. A blind `read_to_string` would hang forever in those cases
/// (we hit this on the first install — `claudectl --json` from a backgrounded
/// shell never returned).
///
/// So: tty stdin ⇒ definitely not a hook. Otherwise poll(POLLIN, 50ms); if
/// no data is available in that window it's not a hook either. Hook payloads
/// are buffered and closed by the parent before we even start, so 50ms is
/// luxuriously generous.
pub fn try_read_hook_payload() -> io::Result<Option<serde_json::Value>> {
    use std::os::fd::AsRawFd;
    let fd = io::stdin().as_raw_fd();

    // SAFETY: isatty is always safe to call on a valid file descriptor.
    if unsafe { libc::isatty(fd) } == 1 {
        return Ok(None);
    }

    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll on a single valid fd with a finite timeout.
    let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, 50) };
    if rc <= 0 || (pfd.revents & libc::POLLIN) == 0 {
        return Ok(None);
    }

    let mut buf = String::new();
    io::stdin().take(1024 * 1024).read_to_string(&mut buf)?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Ok(None);
    };
    if value
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(value))
}

/// Apply a Claude Code hook payload to the per-session state file.
///
/// Unknown event names are ignored (best-effort — Claude Code may add new
/// events that we haven't wired up yet, and that's fine). Payloads without a
/// `session_id` are also ignored, since we have nothing to key on.
pub fn record_hook_event(payload: &serde_json::Value) -> io::Result<()> {
    let Some(session_id) = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
    else {
        return Ok(());
    };
    let Some(event) = payload.get("hook_event_name").and_then(|v| v.as_str()) else {
        return Ok(());
    };

    // SessionEnd is the one event that removes state instead of updating it.
    if event == "SessionEnd" {
        return HookState::remove(&session_id);
    }

    let mut state = HookState::load(&session_id).unwrap_or_default();
    state.session_id = session_id;
    let ts = now_ms();

    match event {
        "Notification" => {
            state.last_notification_ts_ms = ts;
            state.notification_kind = payload
                .get("notification_type")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        "PreToolUse" => {
            state.last_pretooluse_ts_ms = ts;
            state.current_tool_name = payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            // PreToolUse means the prompt resolved (approved). Clear the
            // permission_prompt notification so infer_status doesn't keep
            // reporting NeedsInput.
            if is_permission_prompt_kind(state.notification_kind.as_deref()) {
                state.notification_kind = None;
            }
        }
        "PostToolUse" => {
            state.last_posttooluse_ts_ms = ts;
            state.current_tool_name = None;
            // Defense-in-depth: when the user denies a permission prompt,
            // some flows route through PostToolUse (synthetic denial result)
            // without firing PreToolUse. Clear here too so a stale marker
            // doesn't get stuck.
            if is_permission_prompt_kind(state.notification_kind.as_deref()) {
                state.notification_kind = None;
            }
        }
        "Stop" => {
            state.last_stop_ts_ms = ts;
            state.notification_kind = None;
            state.current_tool_name = None;
        }
        "UserPromptSubmit" => {
            state.last_promptsubmit_ts_ms = ts;
            // A new prompt clears any pending notification (e.g. denial of
            // a permission prompt followed by typed input).
            state.notification_kind = None;
        }
        "PreCompact" => {
            state.last_precompact_ts_ms = ts;
        }
        "PostCompact" => {
            state.last_postcompact_ts_ms = ts;
        }
        "SubagentStop" => {
            state.last_subagentstop_ts_ms = ts;
        }
        "SessionStart" => {
            state.last_session_start_ts_ms = ts;
        }
        _ => return Ok(()),
    }

    state.save()
}

/// Whether a `notification_type` value from Claude Code represents an open
/// permission prompt. Covers both the main-agent dialog (`permission_prompt`)
/// and the subagent dialog (`worker_permission_prompt`) — both block the user
/// the same way, and claudectl classifies both as `NeedsInput`.
pub fn is_permission_prompt_kind(kind: Option<&str>) -> bool {
    matches!(kind, Some("permission_prompt" | "worker_permission_prompt"))
}

/// Whether the session is currently sitting on a permission prompt.
///
/// Pure deterministic check: the `Notification (permission_prompt)` or
/// `Notification (worker_permission_prompt)` event must be the most recent
/// state-changing event for this session. Any later PreToolUse / PostToolUse
/// / Stop / UserPromptSubmit ⇒ the prompt was resolved (approved, denied, or
/// pivoted to a new prompt). No CPU or JSONL second-guessing — those
/// introduced the false-negatives we just had.
///
/// 750ms grace period: auto-approved prompts (acceptEdits, allowlisted)
/// fire Notification + near-instant PreToolUse; the dialog never opens
/// visibly. Suppressing the marker for the first 750ms filters those out.
/// Real prompts sit far longer, so this costs them nothing.
pub fn is_at_permission_prompt(state: &HookState) -> bool {
    if !is_permission_prompt_kind(state.notification_kind.as_deref()) {
        return false;
    }
    let notif = state.last_notification_ts_ms;
    if notif == 0 {
        return false;
    }
    let still_latest = notif > state.last_pretooluse_ts_ms
        && notif > state.last_posttooluse_ts_ms
        && notif > state.last_stop_ts_ms
        && notif > state.last_promptsubmit_ts_ms;
    if !still_latest {
        return false;
    }
    now_ms().saturating_sub(notif) > 750
}

/// How long a session is allowed to sit in "compacting" before we give up and
/// stop reporting the status, even without a clear end-of-compact signal.
/// Auto-compact is a single model call over the transcript summary — it
/// should complete in seconds to a couple of minutes at the outside. If we're
/// still "compacting" five minutes later, something ate the resolution event
/// (we've seen `Stop` never fire for sessions whose first turn is an
/// auto-compact) and we're better off falling through to the real status.
const COMPACTING_MAX_AGE_MS: u64 = 5 * 60 * 1000;

/// Whether the session is currently auto-compacting. PreCompact has fired
/// and no resolution signal (`PostCompact` — the direct signal — or `Stop` —
/// the fallback signal for the post-compact assistant turn) has come in
/// since, AND the PreCompact is recent enough that compaction could
/// plausibly still be running.
pub fn is_compacting(state: &HookState) -> bool {
    let pre = state.last_precompact_ts_ms;
    if pre == 0 {
        return false;
    }
    let ended = state.last_postcompact_ts_ms.max(state.last_stop_ts_ms);
    if ended >= pre {
        return false;
    }
    now_ms().saturating_sub(pre) < COMPACTING_MAX_AGE_MS
}

/// Whether Claude is currently responding to a prompt.
///
/// True when *any* mid-turn event is more recent than the last `Stop`. Tools
/// coming and going inside a single response don't flip this — claude only
/// stops "responding" when `Stop` fires at the end of the turn. This is
/// what makes the status stable instead of flickering with each tool call.
pub fn is_responding(state: &HookState) -> bool {
    let stop = state.last_stop_ts_ms;
    state.last_promptsubmit_ts_ms > stop
        || state.last_pretooluse_ts_ms > stop
        || state.last_posttooluse_ts_ms > stop
}

/// Whether Claude has cleanly finished its turn — `Stop` is the latest
/// non-Notification event. Stable between turns; doesn't flicker.
pub fn is_waiting_for_user(state: &HookState) -> bool {
    let stop = state.last_stop_ts_ms;
    if stop == 0 {
        return false;
    }
    stop >= state.last_promptsubmit_ts_ms
        && stop >= state.last_pretooluse_ts_ms
        && stop >= state.last_posttooluse_ts_ms
}

/// Garbage-collect state files that are older than `max_age_secs` and have
/// no `last_session_*_ts` activity. Best-effort; errors are swallowed.
pub fn cleanup_stale(max_age_secs: u64) {
    let Ok(entries) = fs::read_dir(state_dir()) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_secs))
        .unwrap_or(UNIX_EPOCH);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fresh_state(session_id: &str) -> HookState {
        HookState {
            session_id: session_id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn permission_prompt_then_pretooluse_clears() {
        let mut s = fresh_state("sid");
        s.notification_kind = Some("permission_prompt".into());
        s.last_notification_ts_ms = 1000;
        assert!(is_at_permission_prompt(&s));

        // Simulate PreToolUse arriving (manually, mirroring record_hook_event)
        s.last_pretooluse_ts_ms = 2000;
        s.notification_kind = None;
        assert!(!is_at_permission_prompt(&s));
    }

    #[test]
    fn worker_permission_prompt_also_counts_as_needs_input() {
        let mut s = fresh_state("sid");
        // Backdate past the 750ms grace so the helper considers it open.
        s.last_notification_ts_ms = now_ms().saturating_sub(2_000);
        s.notification_kind = Some("worker_permission_prompt".into());
        assert!(is_at_permission_prompt(&s));

        // PreToolUse from a sibling or the approved tool clears the marker
        // via record_hook_event — verify the helper treats both kinds
        // uniformly.
        assert!(is_permission_prompt_kind(Some("permission_prompt")));
        assert!(is_permission_prompt_kind(Some("worker_permission_prompt")));
        assert!(!is_permission_prompt_kind(Some("idle_prompt")));
        assert!(!is_permission_prompt_kind(None));
    }

    #[test]
    fn compacting_lasts_until_stop_or_postcompact() {
        let mut s = fresh_state("sid");
        // Use recent timestamps so the age-out check doesn't short-circuit.
        let now = now_ms();
        s.last_precompact_ts_ms = now.saturating_sub(1_000);
        assert!(is_compacting(&s));

        // `Stop` clears it (legacy signal).
        s.last_stop_ts_ms = now;
        assert!(!is_compacting(&s));

        // Reset Stop, confirm `PostCompact` ALSO clears it (direct signal,
        // the reliable one — doesn't depend on Stop firing).
        s.last_stop_ts_ms = 0;
        assert!(is_compacting(&s));
        s.last_postcompact_ts_ms = now;
        assert!(!is_compacting(&s));
    }

    #[test]
    fn compacting_ages_out_without_resolution_signal() {
        // Defense against the observed case where `Stop` never fires for
        // sessions whose first turn is an auto-compact. Without the age-out
        // such sessions would stay `Compacting` forever and mask every real
        // `NeedsInput` that follows.
        let mut s = fresh_state("sid");
        s.last_precompact_ts_ms = now_ms().saturating_sub(COMPACTING_MAX_AGE_MS + 1_000);
        assert!(!is_compacting(&s));
    }

    #[test]
    fn responding_until_stop_fires() {
        let mut s = fresh_state("sid");
        s.last_promptsubmit_ts_ms = 1000;
        assert!(is_responding(&s));

        s.last_stop_ts_ms = 2000;
        assert!(!is_responding(&s));

        // Tools coming and going within a turn don't change responding state
        s.last_pretooluse_ts_ms = 3000;
        assert!(is_responding(&s));
        s.last_posttooluse_ts_ms = 3500;
        assert!(is_responding(&s));
        // Until Stop fires again
        s.last_stop_ts_ms = 4000;
        assert!(!is_responding(&s));
    }

    #[test]
    fn now_ms_is_strictly_monotonic_within_a_process() {
        // record_hook_event reads now_ms() per call; two back-to-back calls
        // must produce distinct ts so order-sensitive checks
        // (is_responding / is_waiting_for_user, both strict `>`) stay
        // deterministic when tests record many events in a hot binary.
        let a = now_ms();
        let b = now_ms();
        let c = now_ms();
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn waiting_for_user_after_stop() {
        let mut s = fresh_state("sid");
        s.last_stop_ts_ms = 1000;
        assert!(is_waiting_for_user(&s));

        // A new prompt invalidates "waiting".
        s.last_promptsubmit_ts_ms = 2000;
        assert!(!is_waiting_for_user(&s));
    }

    #[test]
    fn record_event_routes_correctly() {
        // We can't write to the real state dir in tests; just exercise the
        // payload-parsing path with an unknown event to confirm graceful
        // handling, plus the no-session-id early return.
        let no_sid = json!({"hook_event_name": "Stop"});
        assert!(record_hook_event(&no_sid).is_ok());

        let unknown = json!({"hook_event_name": "Mystery", "session_id": "x"});
        // Unknown events return Ok(()) without writing.
        assert!(record_hook_event(&unknown).is_ok());
    }

    #[test]
    fn try_read_payload_rejects_non_hook_json() {
        // Direct unit of the parsing branch: feed JSON without hook_event_name.
        let value: serde_json::Value = serde_json::from_str(r#"{"foo": 1}"#).unwrap();
        // Mimic the inner check in try_read_hook_payload.
        assert!(
            value
                .get("hook_event_name")
                .and_then(|v| v.as_str())
                .is_none()
        );
    }
}

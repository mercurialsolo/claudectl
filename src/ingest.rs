//! `claudectl ingest --hook <name>` (#345, RFC v2 §6).
//!
//! Reads a hook payload from stdin and appends it to the coord
//! `hook_events` table. Bash hooks call this as
//! `claudectl ingest --hook PostToolUse 2>/dev/null || true`. The
//! `|| true` is deliberate — ingest is best-effort, which is exactly
//! why JSONL tail + `ps` stay authoritative. The latency win is what
//! makes the supervisor's reconciler react in one tick instead of
//! waiting on file-watch debounce.
//!
//! Startup budget: under 50 ms so per-tool-call overhead is invisible.
//! The fast path opens the coord DB, runs migrations (idempotent), reads
//! stdin to memory, parses just enough to pull `session_id` / `tool`,
//! and inserts. Nothing else.

use std::io::{self, Read, Write};

use serde::Deserialize;

use crate::coord::hook_events::{HookEvent, append};
use crate::coord::store;

/// Hook names we accept. Anything else is rejected to keep typos out of
/// the table — the supervisor would otherwise see garbage `hook` values
/// and have to defensively filter.
const KNOWN_HOOKS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SessionStart",
    "Notification",
    "UserPromptSubmit",
];

/// Run the ingest path. Returns 0 on success and 1 on any failure mode;
/// stdout / stderr are written through directly so the caller can pipe
/// them somewhere. The bash hooks discard both streams with
/// `2>/dev/null` — they want a clean exit code.
pub fn run(hook: &str) -> io::Result<i32> {
    if !KNOWN_HOOKS.contains(&hook) {
        eprintln!("claudectl ingest: unknown hook '{hook}' (expected one of {KNOWN_HOOKS:?})");
        return Ok(1);
    }
    let mut payload = String::new();
    io::stdin().read_to_string(&mut payload)?;
    // Empty stdin is a no-op — some Claude Code hook invocations send
    // nothing and we shouldn't fail on that.
    if payload.trim().is_empty() {
        return Ok(0);
    }

    // Parse just enough to lift session_id / tool out for indexed
    // querying. The full payload still goes in `payload`.
    let parsed: HookPayload = serde_json::from_str(&payload).unwrap_or_default();

    // `coord` feature gates compilation; `bus` does too. Both are in
    // default features since PR1 (#350), but the cfg keeps the minimal
    // sync-only build green.
    #[cfg(feature = "coord")]
    {
        let conn = match store::open() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("claudectl ingest: coord open failed: {e}");
                return Ok(1);
            }
        };
        let ev = HookEvent {
            id: None,
            hook: hook.into(),
            session_id: parsed.session_id,
            tool: parsed.tool_name,
            payload,
            ingested_at: crate::logger::timestamp_now(),
        };
        if let Err(e) = append(&conn, &ev) {
            eprintln!("claudectl ingest: append failed: {e}");
            return Ok(1);
        }
        // Flush stderr — some test harnesses buffer and we want errors
        // visible to operators who *did* leave stderr connected.
        let _ = io::stderr().flush();
        Ok(0)
    }

    #[cfg(not(feature = "coord"))]
    {
        let _ = parsed; // suppress dead-code warning
        let _ = append; // suppress import warning
        eprintln!("claudectl ingest: coord feature not compiled in");
        Ok(1)
    }
}

/// What we lift out of the hook payload for indexed querying. Hook
/// schemas evolve under Claude Code's control, so we keep this minimal
/// and tolerant: missing fields default to `None`, and the raw JSON is
/// what the reconciler reads when it needs full context.
#[derive(Debug, Default, Deserialize)]
struct HookPayload {
    #[serde(rename = "session_id", alias = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "tool_name", alias = "toolName", alias = "tool")]
    tool_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_id_and_tool() {
        let raw = r#"{"session_id":"sess_xyz","tool_name":"Bash"}"#;
        let p: HookPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.session_id.as_deref(), Some("sess_xyz"));
        assert_eq!(p.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn accepts_camel_case_aliases() {
        let raw = r#"{"sessionId":"sess_a","toolName":"Edit"}"#;
        let p: HookPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.session_id.as_deref(), Some("sess_a"));
        assert_eq!(p.tool_name.as_deref(), Some("Edit"));
    }

    #[test]
    fn missing_fields_default_to_none() {
        let raw = r#"{}"#;
        let p: HookPayload = serde_json::from_str(raw).unwrap();
        assert!(p.session_id.is_none());
        assert!(p.tool_name.is_none());
    }
}

use std::io::Write;
use std::time::Duration;

use claudectl::discovery;
use claudectl::models;
use claudectl::monitor;
use claudectl::session::{ClaudeSession, RawSession, SessionStatus, TelemetryStatus};

/// Helper: create a minimal session for testing status inference.
fn make_session(cpu: f32, last_message_age_secs: u64) -> ClaudeSession {
    let raw = RawSession {
        pid: 1,
        session_id: "test-session".into(),
        cwd: "/tmp/test-project".into(),
        started_at: 0,
    };
    let mut s = ClaudeSession::from_raw(raw);
    s.cpu_percent = cpu;
    s.telemetry_status = TelemetryStatus::Available;
    s.usage_metrics_available = true;

    // Set last_message_ts relative to now
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    s.last_message_ts = now_ms.saturating_sub(last_message_age_secs * 1000);
    s
}

// ────────────────────────────────────────────────────────────────────────────
// Status Inference Tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn status_high_cpu_always_processing() {
    let mut s = make_session(50.0, 0);
    monitor::infer_status(&mut s, "", "", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_high_cpu_overrides_waiting_for_task() {
    let mut s = make_session(10.0, 0);
    monitor::infer_status(&mut s, "assistant", "end_turn", true);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_high_cpu_overrides_end_turn() {
    let mut s = make_session(20.0, 60);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_waiting_for_task_needs_input() {
    let mut s = make_session(0.5, 10);
    monitor::infer_status(&mut s, "", "", true);
    assert_eq!(s.status, SessionStatus::NeedsInput);
}

#[test]
fn status_end_turn_recent_waiting_input() {
    // Assistant said end_turn, 2 minutes ago, low CPU
    let mut s = make_session(0.5, 120);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::WaitingInput);
}

#[test]
fn status_end_turn_old_idle() {
    // Assistant said end_turn, 15 minutes ago → Idle
    let mut s = make_session(0.5, 15 * 60);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::Idle);
}

#[test]
fn status_end_turn_exactly_10min_still_waiting() {
    // 10 minutes = boundary, should still be WaitingInput (>10 is Idle)
    let mut s = make_session(0.5, 10 * 60);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::WaitingInput);
}

#[test]
fn status_end_turn_11min_idle() {
    let mut s = make_session(0.5, 11 * 60);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::Idle);
}

#[test]
fn status_tool_use_low_cpu_old_needs_input() {
    // tool_use + low CPU + >5s ago = permission prompt
    let mut s = make_session(0.5, 30);
    monitor::infer_status(&mut s, "assistant", "tool_use", false);
    assert_eq!(s.status, SessionStatus::NeedsInput);
}

#[test]
fn status_tool_use_low_cpu_recent_processing() {
    // tool_use + low CPU + <5s ago = still processing (tool just fired)
    let mut s = make_session(0.5, 2);
    monitor::infer_status(&mut s, "assistant", "tool_use", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_tool_use_high_cpu_processing() {
    // tool_use + high CPU = still crunching
    let mut s = make_session(15.0, 30);
    monitor::infer_status(&mut s, "assistant", "tool_use", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_user_message_pending_processing() {
    let mut s = make_session(3.0, 5);
    monitor::infer_status(&mut s, "user", "", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_user_message_low_cpu_still_processing() {
    // User sent message, CPU low — could be waiting for API
    let mut s = make_session(0.5, 5);
    monitor::infer_status(&mut s, "user", "", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_no_signals_idle() {
    // No JSONL signals at all → Idle
    let mut s = make_session(0.0, 0);
    monitor::infer_status(&mut s, "", "", false);
    assert_eq!(s.status, SessionStatus::Idle);
}

#[test]
fn status_no_telemetry_unknown() {
    let raw = RawSession {
        pid: 1,
        session_id: "test-session".into(),
        cwd: "/tmp/test-project".into(),
        started_at: 0,
    };
    let mut s = ClaudeSession::from_raw(raw);
    monitor::infer_status(&mut s, "", "", false);
    assert_eq!(s.status, SessionStatus::Unknown);
}

#[test]
fn status_cpu_threshold_boundary() {
    // CPU exactly 5.0 — should NOT trigger Processing (threshold is >5.0)
    let mut s = make_session(5.0, 0);
    monitor::infer_status(&mut s, "", "", false);
    assert_eq!(s.status, SessionStatus::Idle);

    // CPU 5.1 — should trigger Processing
    let mut s2 = make_session(5.1, 0);
    monitor::infer_status(&mut s2, "", "", false);
    assert_eq!(s2.status, SessionStatus::Processing);
}

#[test]
fn status_persisted_tool_use_survives_empty_tick() {
    // Reproduces the bug: session blocked on permission prompt ("Do you want to
    // proceed?"), first tick correctly detects NeedsInput via tool_use + low CPU,
    // but second tick has no new JSONL data (empty signals) and must NOT fall
    // through to Idle.
    let mut s = make_session(0.5, 30);

    // Tick 1: new JSONL data — tool_use detected
    monitor::infer_status(&mut s, "assistant", "tool_use", false);
    assert_eq!(s.status, SessionStatus::NeedsInput);

    // Simulate what update_tokens() now does: persist the signals
    s.last_msg_type = "assistant".into();
    s.last_stop_reason = "tool_use".into();
    s.is_waiting_for_task = false;

    // Tick 2: no new JSONL data — signals come from persisted fields
    let msg_type = s.last_msg_type.clone();
    let stop_reason = s.last_stop_reason.clone();
    let waiting = s.is_waiting_for_task;
    monitor::infer_status(&mut s, &msg_type, &stop_reason, waiting);
    assert_eq!(s.status, SessionStatus::NeedsInput);
}

#[test]
fn status_null_stop_reason_with_tool_use_infers_needs_input() {
    // Claude Code writes stop_reason: null for tool calls awaiting approval.
    // The content still has a tool_use block — infer tool_use from content so
    // that the session shows NeedsInput instead of Idle.
    let jsonl = r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-6","stop_reason":null,"content":[{"type":"tool_use","id":"toolu_01X","name":"Bash","input":{"command":"echo hi"}}],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    s.cpu_percent = 0.5;
    monitor::update_tokens(&mut s);

    // stop_reason was null in JSONL but must be inferred from tool_use content
    assert_eq!(s.last_stop_reason, "tool_use");
    // pending_tool_name is set (ToolUse parsed, no ToolResult yet) so low CPU
    // immediately infers NeedsInput — no need to wait for the 5s age threshold.
    assert_eq!(s.pending_tool_name, Some("Bash".into()));
    assert_eq!(s.status, SessionStatus::NeedsInput);
}

// ────────────────────────────────────────────────────────────────────────────
// Cost Estimation Tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn cost_opus_tokens() {
    let mut s = make_session(0.0, 0);
    s.model = "opus-4.6".into();
    s.total_input_tokens = 1_000_000;
    s.total_output_tokens = 100_000;
    s.cache_read_tokens = 500_000;
    s.cache_write_tokens = 200_000;

    let cost = monitor::estimate_cost(&s);
    // plain_input = 1M - 500k - 200k = 300k
    // cost = 300k/1M * 15 + 100k/1M * 75 + 500k/1M * 1.875 + 200k/1M * 18.75
    //      = 0.3 * 15 + 0.1 * 75 + 0.5 * 1.875 + 0.2 * 18.75
    //      = 4.5 + 7.5 + 0.9375 + 3.75 = 16.6875
    let expected = 16.6875;
    assert!(
        (cost - expected).abs() < 0.001,
        "opus cost={cost}, expected={expected}"
    );
}

#[test]
fn cost_sonnet_tokens() {
    let mut s = make_session(0.0, 0);
    s.model = "sonnet-4.6".into();
    s.total_input_tokens = 100_000;
    s.total_output_tokens = 50_000;
    s.cache_read_tokens = 0;
    s.cache_write_tokens = 0;

    let cost = monitor::estimate_cost(&s);
    // plain_input = 100k
    // cost = 100k/1M * 3 + 50k/1M * 15 = 0.3 + 0.75 = 1.05
    let expected = 1.05;
    assert!(
        (cost - expected).abs() < 0.001,
        "sonnet cost={cost}, expected={expected}"
    );
}

#[test]
fn cost_haiku_tokens() {
    let mut s = make_session(0.0, 0);
    s.model = "haiku".into();
    s.total_input_tokens = 100_000;
    s.total_output_tokens = 50_000;
    s.cache_read_tokens = 0;
    s.cache_write_tokens = 0;

    let cost = monitor::estimate_cost(&s);
    // plain_input = 100k
    // cost = 100k/1M * 0.80 + 50k/1M * 4.0 = 0.08 + 0.2 = 0.28
    let expected = 0.28;
    assert!(
        (cost - expected).abs() < 0.001,
        "haiku cost={cost}, expected={expected}"
    );
}

#[test]
fn cost_unknown_model_defaults_to_opus() {
    let mut s = make_session(0.0, 0);
    s.model = "some-future-model".into();
    s.total_input_tokens = 1_000_000;
    s.total_output_tokens = 0;
    s.cache_read_tokens = 0;
    s.cache_write_tokens = 0;

    let cost = monitor::estimate_cost(&s);
    // Should use opus pricing: 1M/1M * 15 = 15.0
    let expected = 15.0;
    assert!(
        (cost - expected).abs() < 0.001,
        "unknown model cost={cost}, expected={expected}"
    );
}

#[test]
fn cost_zero_tokens() {
    let s = make_session(0.0, 0);
    let cost = monitor::estimate_cost(&s);
    assert_eq!(cost, 0.0);
}

// ────────────────────────────────────────────────────────────────────────────
// Model Context Max Tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn context_max_opus() {
    assert_eq!(monitor::model_context_max("opus-4.6"), 1_000_000);
    assert_eq!(monitor::model_context_max("opus"), 1_000_000);
}

#[test]
fn context_max_sonnet() {
    assert_eq!(monitor::model_context_max("sonnet-4.6"), 200_000);
    assert_eq!(monitor::model_context_max("sonnet"), 200_000);
}

#[test]
fn context_max_haiku() {
    assert_eq!(monitor::model_context_max("haiku"), 200_000);
}

#[test]
fn context_max_unknown() {
    assert_eq!(monitor::model_context_max("unknown-model"), 200_000);
}

// ────────────────────────────────────────────────────────────────────────────
// Model Shortening Tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn shorten_model_opus_46() {
    assert_eq!(
        monitor::shorten_model("claude-opus-4-6-20260401"),
        "opus-4.6"
    );
}

#[test]
fn shorten_model_opus_generic() {
    assert_eq!(monitor::shorten_model("claude-opus-20260101"), "opus");
}

#[test]
fn shorten_model_sonnet_46() {
    assert_eq!(
        monitor::shorten_model("claude-sonnet-4-6-20260401"),
        "sonnet-4.6"
    );
}

#[test]
fn shorten_model_sonnet_generic() {
    assert_eq!(monitor::shorten_model("claude-sonnet-20260101"), "sonnet");
}

#[test]
fn shorten_model_haiku() {
    assert_eq!(monitor::shorten_model("claude-haiku-4-5-20251001"), "haiku");
}

#[test]
fn shorten_model_unknown() {
    assert_eq!(monitor::shorten_model("gpt-4o"), "gpt-4o");
}

// ────────────────────────────────────────────────────────────────────────────
// JSONL Parsing Integration Tests (using temp files)
// ────────────────────────────────────────────────────────────────────────────

fn make_session_with_jsonl(content: &str) -> (ClaudeSession, tempfile::NamedTempFile) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(content.as_bytes()).unwrap();
    file.flush().unwrap();

    let raw = RawSession {
        pid: 1,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut s = ClaudeSession::from_raw(raw);
    s.jsonl_path = Some(file.path().to_path_buf());
    (s, file)
}

fn make_session_with_paths(
    cwd: String,
    session_id: String,
    jsonl_path: std::path::PathBuf,
) -> ClaudeSession {
    let raw = RawSession {
        pid: 1,
        session_id,
        cwd,
        started_at: 0,
    };
    let mut s = ClaudeSession::from_raw(raw);
    s.jsonl_path = Some(jsonl_path);
    s
}

fn write_jsonl(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn expected_cost(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    let profile = models::resolve(model).profile;
    (input_tokens as f64 / 1_000_000.0) * profile.input_per_m
        + (output_tokens as f64 / 1_000_000.0) * profile.output_per_m
}

#[test]
fn jsonl_parse_token_usage() {
    let jsonl = r#"{"type":"assistant","message":{"model":"claude-opus-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":50000,"output_tokens":10000,"cache_read_input_tokens":20000,"cache_creation_input_tokens":5000}}}"#;

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    monitor::update_tokens(&mut s);

    assert_eq!(s.total_input_tokens, 75000); // 50000 + 20000 + 5000
    assert_eq!(s.total_output_tokens, 10000);
    assert_eq!(s.cache_read_tokens, 20000);
    assert_eq!(s.cache_write_tokens, 5000);
    assert_eq!(s.model, "opus-4.6");
    assert_eq!(s.context_max, 1_000_000);
}

#[test]
fn jsonl_parse_multiple_entries() {
    let jsonl = concat!(
        r#"{"type":"user","message":{"type":"user"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6-20260401","stop_reason":"tool_use","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":2000,"output_tokens":1000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    monitor::update_tokens(&mut s);

    assert_eq!(s.total_input_tokens, 3000); // 1000 + 2000
    assert_eq!(s.total_output_tokens, 1500); // 500 + 1000
    assert_eq!(s.model, "sonnet-4.6");
}

#[test]
fn jsonl_incremental_reads() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    let line1 = r#"{"type":"assistant","message":{"model":"claude-opus-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
    writeln!(file, "{line1}").unwrap();
    file.flush().unwrap();

    let raw = RawSession {
        pid: 1,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut s = ClaudeSession::from_raw(raw);
    s.jsonl_path = Some(file.path().to_path_buf());

    // First read
    monitor::update_tokens(&mut s);
    assert_eq!(s.total_input_tokens, 1000);
    assert_eq!(s.total_output_tokens, 500);

    // Second read with no new data — should not double-count
    monitor::update_tokens(&mut s);
    assert_eq!(s.total_input_tokens, 1000);
    assert_eq!(s.total_output_tokens, 500);

    // Append more data
    let line2 = r#"{"type":"assistant","message":{"model":"claude-opus-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":2000,"output_tokens":800,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
    writeln!(file, "{line2}").unwrap();
    file.flush().unwrap();

    // Third read — should pick up new data only
    monitor::update_tokens(&mut s);
    assert_eq!(s.total_input_tokens, 3000);
    assert_eq!(s.total_output_tokens, 1300);
}

#[test]
fn jsonl_empty_file() {
    let (mut s, _file) = make_session_with_jsonl("");
    monitor::update_tokens(&mut s);
    assert_eq!(s.total_input_tokens, 0);
    assert_eq!(s.total_output_tokens, 0);
}

#[test]
fn jsonl_corrupted_lines_skipped() {
    let jsonl = concat!(
        "not valid json at all\n",
        "{\"type\":\"something but no usage\"}\n",
        r#"{"type":"assistant","message":{"model":"claude-opus-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":5000,"output_tokens":1000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    monitor::update_tokens(&mut s);

    // Should still parse the valid line
    assert_eq!(s.total_input_tokens, 5000);
    assert_eq!(s.total_output_tokens, 1000);
}

#[test]
fn jsonl_waiting_for_task_detection() {
    let jsonl = concat!(
        r#"{"type":"assistant","message":{"model":"claude-opus-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"progress","data":"waiting_for_task"}"#,
    );

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    s.cpu_percent = 0.5; // Low CPU
    monitor::update_tokens(&mut s);

    // Status should be NeedsInput (waiting_for_task + low CPU)
    assert_eq!(s.status, SessionStatus::NeedsInput);
}

#[test]
fn jsonl_missing_file() {
    let raw = RawSession {
        pid: 1,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut s = ClaudeSession::from_raw(raw);
    s.jsonl_path = Some(std::path::PathBuf::from("/nonexistent/path.jsonl"));

    // Should not panic
    monitor::update_tokens(&mut s);
    assert_eq!(s.total_input_tokens, 0);
}

#[test]
fn jsonl_no_path() {
    let raw = RawSession {
        pid: 1,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut s = ClaudeSession::from_raw(raw);
    // jsonl_path is None

    monitor::update_tokens(&mut s);
    assert_eq!(s.total_input_tokens, 0);
}

#[test]
fn jsonl_rolls_up_subagent_tokens_and_cost() {
    let temp = tempfile::tempdir().unwrap();
    let parent_jsonl = temp.path().join("parent.jsonl");
    write_jsonl(
        &parent_jsonl,
        r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":100000,"output_tokens":50000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );

    let session_id = format!("subagent-rollup-{}", std::process::id());
    let cwd = format!("/tmp/claudectl-rollup-{}", std::process::id());
    let slug = cwd.replace('/', "-");
    let uid = unsafe { libc::getuid() };
    let tasks_dir = std::path::PathBuf::from(format!("/tmp/claude-{uid}"))
        .join(&slug)
        .join(&session_id)
        .join("tasks");
    write_jsonl(
        &tasks_dir.join("agent-1.jsonl"),
        r#"{"type":"assistant","message":{"model":"claude-opus-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":200000,"output_tokens":50000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );
    write_jsonl(
        &tasks_dir.join("nested/agent-2.jsonl"),
        r#"{"type":"assistant","message":{"model":"claude-haiku-4-5-20260101","stop_reason":"end_turn","usage":{"input_tokens":50000,"output_tokens":10000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );

    let mut s = make_session_with_paths(cwd, session_id, parent_jsonl);
    discovery::scan_subagents(std::slice::from_mut(&mut s));
    monitor::update_tokens(&mut s);

    assert_eq!(s.active_subagent_count, 2);
    assert_eq!(s.subagent_count, 2);
    assert_eq!(s.total_input_tokens, 350_000);
    assert_eq!(s.total_output_tokens, 110_000);

    let expected = expected_cost("sonnet-4.6", 100_000, 50_000)
        + expected_cost("opus-4.6", 200_000, 50_000)
        + expected_cost("haiku", 50_000, 10_000);
    assert!((s.cost_usd - expected).abs() < 0.0001);
    assert!(!s.cost_estimate_unverified);

    let _ = std::fs::remove_dir_all(
        std::path::PathBuf::from(format!("/tmp/claude-{uid}"))
            .join(&slug)
            .join(&s.session_id),
    );
}

#[test]
fn subagent_rollup_persists_after_task_file_disappears() {
    let temp = tempfile::tempdir().unwrap();
    let parent_jsonl = temp.path().join("parent.jsonl");
    write_jsonl(
        &parent_jsonl,
        r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":100000,"output_tokens":10000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );

    let session_id = format!("subagent-persist-{}", std::process::id());
    let cwd = format!("/tmp/claudectl-persist-{}", std::process::id());
    let slug = cwd.replace('/', "-");
    let uid = unsafe { libc::getuid() };
    let subagent_root = std::path::PathBuf::from(format!("/tmp/claude-{uid}"))
        .join(&slug)
        .join(&session_id);
    let tasks_dir = subagent_root.join("tasks");
    write_jsonl(
        &tasks_dir.join("agent-1.jsonl"),
        r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6-20260401","stop_reason":"end_turn","usage":{"input_tokens":200000,"output_tokens":20000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );

    let mut s = make_session_with_paths(cwd, session_id, parent_jsonl);
    discovery::scan_subagents(std::slice::from_mut(&mut s));
    monitor::update_tokens(&mut s);

    assert_eq!(s.active_subagent_count, 1);
    assert_eq!(s.subagent_count, 1);
    assert_eq!(s.total_input_tokens, 300_000);
    assert_eq!(s.total_output_tokens, 30_000);

    std::fs::remove_dir_all(&subagent_root).unwrap();

    discovery::scan_subagents(std::slice::from_mut(&mut s));
    monitor::update_tokens(&mut s);

    assert_eq!(s.active_subagent_count, 0);
    assert_eq!(s.subagent_count, 1);
    assert_eq!(s.total_input_tokens, 300_000);
    assert_eq!(s.total_output_tokens, 30_000);
}

// ────────────────────────────────────────────────────────────────────────────
// Session Formatting Edge Cases
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn context_percent_zero_max() {
    let mut s = make_session(0.0, 0);
    s.context_max = 0;
    s.context_tokens = 1000;
    assert_eq!(s.context_percent(), 0.0);
}

#[test]
fn context_percent_zero_tokens() {
    let mut s = make_session(0.0, 0);
    s.context_max = 200_000;
    s.context_tokens = 0;
    assert_eq!(s.context_percent(), 0.0);
}

#[test]
fn context_percent_calculation() {
    let mut s = make_session(0.0, 0);
    s.context_max = 200_000;
    s.context_tokens = 100_000;
    assert!((s.context_percent() - 50.0).abs() < 0.01);
}

#[test]
fn sparkline_empty() {
    let s = make_session(0.0, 0);
    assert_eq!(s.format_sparkline(), "-");
}

#[test]
fn sparkline_records_and_renders() {
    let mut s = make_session(0.0, 0);
    s.status = SessionStatus::Processing;
    s.record_activity();
    s.status = SessionStatus::Idle;
    s.record_activity();

    let sparkline = s.format_sparkline();
    assert_eq!(sparkline.chars().count(), 2);
}

#[test]
fn sparkline_ring_buffer_limit() {
    let mut s = make_session(0.0, 0);
    for _ in 0..20 {
        s.status = SessionStatus::Processing;
        s.record_activity();
    }
    // Should be capped at 15
    assert_eq!(s.activity_history.len(), 15);
}

#[test]
fn json_export_format() {
    let mut s = make_session(0.0, 0);
    s.model = "opus-4.6".into();
    s.cost_usd = 1.234;
    s.total_input_tokens = 50000;
    s.total_output_tokens = 10000;
    s.elapsed = Duration::from_secs(300);

    let json = s.to_json_value();
    assert_eq!(json["pid"], 1);
    assert_eq!(json["status"], "Idle");
    assert_eq!(json["elapsed_secs"], 300);
    assert_eq!(json["tokens_in"], 50000);
    assert_eq!(json["tokens_out"], 10000);
    assert!(json["subagent_breakdown"].as_array().unwrap().is_empty());
}

#[test]
fn json_export_includes_subagent_breakdown() {
    let mut s = make_session(0.0, 0);
    s.active_subagent_jsonl_paths = vec![std::path::PathBuf::from(
        "/tmp/claude-1/-tmp-project/session-1/tasks/agent-2.jsonl",
    )];
    s.subagent_rollups.insert(
        std::path::PathBuf::from("/tmp/claude-1/-tmp-project/session-1/tasks/agent-1.jsonl"),
        claudectl::session::SubagentRollup {
            input_tokens: 20_000,
            output_tokens: 2_000,
            cost_usd: 0.4,
            usage_metrics_available: true,
            ..claudectl::session::SubagentRollup::default()
        },
    );
    s.subagent_rollups.insert(
        std::path::PathBuf::from("/tmp/claude-1/-tmp-project/session-1/tasks/agent-2.jsonl"),
        claudectl::session::SubagentRollup {
            input_tokens: 10_000,
            output_tokens: 1_000,
            cost_usd: 0.2,
            usage_metrics_available: true,
            ..claudectl::session::SubagentRollup::default()
        },
    );
    s.subagent_count = 2;
    s.active_subagent_count = 1;

    let json = s.to_json_value();
    let breakdown = json["subagent_breakdown"].as_array().unwrap();
    assert_eq!(breakdown.len(), 2);
    assert_eq!(breakdown[0]["label"], "completed");
    assert_eq!(breakdown[0]["state"], "Completed");
    assert_eq!(breakdown[0]["tokens_in"], 20000);
    assert_eq!(breakdown[1]["label"], "agent-2");
    assert_eq!(breakdown[1]["state"], "Active");
}

#[test]
fn burn_rate_formatting() {
    let mut s = make_session(0.0, 0);
    assert_eq!(s.format_burn_rate(), "-");

    s.burn_rate_per_hr = 0.50;
    assert_eq!(s.format_burn_rate(), "$0.50/h");

    s.burn_rate_per_hr = 3.5;
    assert_eq!(s.format_burn_rate(), "$3.5/h");
}

#[test]
fn mem_formatting() {
    let mut s = make_session(0.0, 0);
    assert_eq!(s.format_mem(), "-");

    s.mem_mb = 256.7;
    assert_eq!(s.format_mem(), "257M");
}

#[test]
fn context_bar_formatting() {
    let mut s = make_session(0.0, 0);
    assert_eq!(s.format_context_bar(10), "-");

    s.context_max = 200_000;
    s.context_tokens = 100_000; // 50%
    let bar = s.format_context_bar(10);
    assert!(bar.contains("50%"));
    assert!(bar.contains("█████"));
    assert!(bar.contains("░░░░░"));
}

// ────────────────────────────────────────────────────────────────────────────
// Session Recorder Tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn session_recorder_produces_highlight_reel() {
    use claudectl::session_recorder::SessionRecorder;

    // Create empty JSONL first, then create recorder (which seeks to end),
    // then write events to simulate live session activity
    let mut jsonl_file = tempfile::NamedTempFile::new().unwrap();
    jsonl_file.flush().unwrap();

    let output_file = tempfile::NamedTempFile::new().unwrap();
    let output_path = output_file.path().to_str().unwrap().to_string() + ".cast";

    let mut rec = SessionRecorder::new(jsonl_file.path(), &output_path, "test-project", 120, 40)
        .expect("Failed to create session recorder");

    // Now write events AFTER recorder was created (simulates live recording)
    writeln!(jsonl_file, r#"{{"message":{{"role":"assistant","type":"message","content":[{{"type":"text","text":"I'll fix the authentication bug by updating the middleware."}}],"stop_reason":"tool_use"}}}}"#).unwrap();
    writeln!(jsonl_file, r#"{{"message":{{"role":"assistant","type":"message","content":[{{"type":"tool_use","name":"Edit","input":{{"file_path":"/src/auth.rs","old_string":"fn check()","new_string":"fn check_auth(token: &str)"}}}}],"stop_reason":"tool_use"}}}}"#).unwrap();
    writeln!(jsonl_file, r#"{{"message":{{"role":"assistant","type":"message","content":[{{"type":"tool_use","name":"Bash","input":{{"command":"cargo test"}}}}],"stop_reason":"tool_use"}}}}"#).unwrap();
    writeln!(jsonl_file, r#"{{"message":{{"role":"user","type":"message","content":[{{"type":"tool_result","content":"test result: ok. 12 passed","is_error":false}}]}}}}"#).unwrap();
    writeln!(jsonl_file, r#"{{"message":{{"role":"assistant","type":"message","content":[{{"type":"tool_use","name":"Read","input":{{"file_path":"/src/main.rs"}}}}],"stop_reason":"tool_use"}}}}"#).unwrap();
    jsonl_file.flush().unwrap();

    let had_events = rec.poll().expect("Failed to poll");
    assert!(had_events, "Should have found events in the JSONL");

    rec.finish().expect("Failed to finish recording");

    let content = std::fs::read_to_string(&output_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();

    // First line is the asciicast header
    assert!(
        lines[0].contains("\"version\":2"),
        "Should have asciicast v2 header"
    );
    assert!(
        lines[0].contains("test-project"),
        "Header should contain session name"
    );

    // Should have multiple frames (header + title card + events + finish)
    assert!(
        lines.len() >= 4,
        "Should have at least 4 lines (header + title + events + finish), got {}",
        lines.len()
    );

    // Should contain the Edit tool rendered as Claude Code style "Update(file)"
    let full = content.to_string();
    assert!(
        full.contains("Update"),
        "Should contain Update event for Edit tool"
    );
    assert!(full.contains("auth.rs"), "Should contain edited file name");

    // Should contain the Bash command rendered Claude Code style
    assert!(
        full.contains("bash command"),
        "Should contain bash command indicator"
    );
    assert!(full.contains("cargo test"), "Should contain bash command");

    // Read events should appear as brief gray context lines (not full highlight frames)
    assert!(
        full.contains("Read"),
        "Read tool should appear as context line"
    );

    // Should contain final summary
    assert!(
        full.contains("complete"),
        "Should contain completion message"
    );

    // Clean up
    let _ = std::fs::remove_file(&output_path);
}

#[test]
fn session_recorder_empty_jsonl() {
    use claudectl::session_recorder::SessionRecorder;

    let jsonl_file = tempfile::NamedTempFile::new().unwrap();
    let output_file = tempfile::NamedTempFile::new().unwrap();
    let output_path = output_file.path().to_str().unwrap().to_string() + ".cast";

    let mut rec = SessionRecorder::new(jsonl_file.path(), &output_path, "empty-session", 80, 24)
        .expect("Failed to create recorder");

    let had_events = rec.poll().expect("Failed to poll");
    assert!(!had_events, "Empty JSONL should produce no events");

    rec.finish().expect("Failed to finish");

    let content = std::fs::read_to_string(&output_path).unwrap();
    assert!(
        content.contains("\"version\":2"),
        "Should still have header"
    );

    let _ = std::fs::remove_file(&output_path);
}

#[test]
fn recorder_cast_file_creation() {
    use claudectl::recorder::Recorder;

    let output_file = tempfile::NamedTempFile::new().unwrap();
    let output_path = output_file.path().to_str().unwrap().to_string() + ".cast";

    let mut rec = Recorder::new(&output_path, 120, 40).expect("Failed to create recorder");
    rec.capture(b"hello world");
    rec.flush_frame().expect("Failed to flush");
    rec.capture(b"second frame");
    rec.flush_frame().expect("Failed to flush");
    rec.finish().expect("Failed to finish");

    let content = std::fs::read_to_string(&output_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();

    assert!(lines[0].contains("\"version\":2"));
    assert!(lines[0].contains("\"width\":120"));
    assert!(lines[0].contains("\"height\":40"));
    assert!(
        lines.len() == 3,
        "Should have header + 2 frames, got {}",
        lines.len()
    );
    assert!(lines[1].contains("hello world"));
    assert!(lines[2].contains("second frame"));

    let _ = std::fs::remove_file(&output_path);
}

// ────────────────────────────────────────────────────────────────────────────
// Transcript Discovery Tests (Issue #161)
//
// These tests mutate the HOME env var so projects_dir() resolves to a temp dir.
// A mutex serializes them to prevent concurrent HOME changes across threads.
// ────────────────────────────────────────────────────────────────────────────

use std::sync::Mutex;
static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Helper: build a fake ~/.claude layout in a temp dir and run resolve_jsonl_paths.
/// Holds HOME_LOCK for the duration.
fn resolve_with_layout(
    cwd: &str,
    session_id: &str,
    slug_on_disk: &str,
) -> (ClaudeSession, tempfile::TempDir) {
    let _guard = HOME_LOCK.lock().unwrap();

    let home = tempfile::tempdir().unwrap();
    let original_home = std::env::var("HOME").ok();
    unsafe { std::env::set_var("HOME", home.path()) };

    let project_dir = home.path().join(".claude/projects").join(slug_on_disk);
    std::fs::create_dir_all(&project_dir).unwrap();
    let jsonl_content = r#"{"type":"assistant","message":{"model":"claude-opus-4-6","stop_reason":"end_turn","usage":{"input_tokens":1,"cache_creation_input_tokens":523,"cache_read_input_tokens":79425,"output_tokens":937}}}"#;
    std::fs::write(
        project_dir.join(format!("{session_id}.jsonl")),
        jsonl_content,
    )
    .unwrap();

    let raw = RawSession {
        pid: 86131,
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        started_at: 1776421121745,
    };
    let mut session = ClaudeSession::from_raw(raw);
    discovery::resolve_jsonl_paths(std::slice::from_mut(&mut session));

    // Restore HOME
    if let Some(h) = original_home {
        unsafe { std::env::set_var("HOME", h) };
    }

    (session, home)
}

#[test]
fn resolve_jsonl_standard_cwd() {
    let (s, _home) = resolve_with_layout(
        "/Users/testuser/Repos/data-platform-answers",
        "db55eb53-8ff0-45b7-9f8f-0d5dfa51e701",
        "-Users-testuser-Repos-data-platform-answers",
    );
    assert!(
        s.jsonl_path.is_some(),
        "should find JSONL for standard cwd (no trailing slash)"
    );
}

#[test]
fn resolve_jsonl_trailing_slash_cwd() {
    let (s, _home) = resolve_with_layout(
        "/Users/testuser/Repos/data-platform-answers/",
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "-Users-testuser-Repos-data-platform-answers",
    );
    assert!(
        s.jsonl_path.is_some(),
        "should find JSONL even when cwd has trailing slash"
    );
}

#[test]
fn resolve_jsonl_cwd_with_hyphens() {
    let (s, _home) = resolve_with_layout(
        "/Users/dev/my-cool-project",
        "11111111-2222-3333-4444-555555555555",
        "-Users-dev-my-cool-project",
    );
    assert!(
        s.jsonl_path.is_some(),
        "should find JSONL when cwd contains hyphens"
    );
}

#[test]
fn resolve_jsonl_encoding_mismatch_fallback() {
    let _guard = HOME_LOCK.lock().unwrap();

    let home = tempfile::tempdir().unwrap();
    let original_home = std::env::var("HOME").ok();
    unsafe { std::env::set_var("HOME", home.path()) };

    let session_id = "deadbeef-1234-5678-9abc-def012345678";
    let cwd = "/Users/testuser/projects/webapp";

    // JSONL under a slug that does NOT match cwd_to_slug(cwd)
    let wrong_slug = "-some-other-encoding-of-the-cwd";
    let project_dir = home.path().join(".claude/projects").join(wrong_slug);
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join(format!("{session_id}.jsonl")),
        r#"{"type":"assistant","message":{"model":"claude-opus-4-6","stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    ).unwrap();

    let raw = RawSession {
        pid: 99999,
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        started_at: 0,
    };
    let mut session = ClaudeSession::from_raw(raw);
    discovery::resolve_jsonl_paths(std::slice::from_mut(&mut session));

    if let Some(h) = original_home {
        unsafe { std::env::set_var("HOME", h) };
    }

    assert!(
        session.jsonl_path.is_some(),
        "should find JSONL via fallback scan when slug encoding differs"
    );
}

#[test]
fn resolve_jsonl_telemetry_available_after_resolution() {
    let (mut s, _home) = resolve_with_layout(
        "/Users/testuser/myproject",
        "face0000-face-face-face-faceface0000",
        "-Users-testuser-myproject",
    );
    assert!(s.jsonl_path.is_some(), "precondition: jsonl_path found");

    monitor::update_tokens(&mut s);
    assert_eq!(
        s.telemetry_status,
        TelemetryStatus::Available,
        "telemetry should be Available after parsing JSONL, not {:?}",
        s.telemetry_status
    );
    assert!(s.usage_metrics_available);
    assert!(s.own_output_tokens > 0, "should have parsed output tokens");
}

// ────────────────────────────────────────────────────────────────────────────
// #220 baselining — reaper attribution tests
// ────────────────────────────────────────────────────────────────────────────
//
// Each test isolates a tempdir HOME (under HOME_LOCK), seeds a decisions.jsonl
// and a pending-outcome file, runs the reaper, then asserts on the resulting
// outcomes/ and pending-outcomes/ directories. HOME is restored on teardown.

use claudectl::brain::outcomes;

struct HomeGuard {
    original: Option<String>,
    _tempdir: tempfile::TempDir,
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

fn isolated_home() -> HomeGuard {
    let original = std::env::var("HOME").ok();
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", dir.path()) };
    HomeGuard {
        original,
        _tempdir: dir,
    }
}

fn write_decision_jsonl(line: &str) {
    let path = std::env::var("HOME").unwrap();
    let dir = std::path::PathBuf::from(path).join(".claudectl/brain");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("decisions.jsonl"))
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

fn write_pending_file(p: &outcomes::PendingOutcome) {
    let path = outcomes::write_pending(p).unwrap();
    assert!(path.exists());
}

#[test]
fn reaper_attributes_outcome_to_recent_decision() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let dec_ts = now - 30;
    let decision_id = format!("dec_{dec_ts}_42_0");

    write_decision_jsonl(&format!(
        r#"{{"ts":"{dec_ts}","pid":42,"project":"alpha","tool":"Bash","command":"cargo test","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept","decision_type":"session","decision_id":"{decision_id}"}}"#
    ));

    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("cargo test".into()),
        project: "alpha".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(0),
        duration_ms: Some(2_400),
        stderr_tail: None,
        ts: now,
    });

    let stats = outcomes::reap();
    assert_eq!(stats.attributed, 1, "exactly one outcome should attribute");
    assert_eq!(stats.still_pending, 0);
    assert_eq!(stats.orphaned, 0);

    let resolved = outcomes::load_resolved_map();
    assert_eq!(resolved.len(), 1);
    let r = resolved.get(&decision_id).expect("resolved by decision_id");
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.duration_ms, Some(2_400));
}

#[test]
fn reaper_leaves_orphaned_outcome_pending_within_window() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    // No decisions written at all → outcome can't attribute, but isn't old
    // enough to be orphaned either.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("ls".into()),
        project: "p".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(0),
        duration_ms: None,
        stderr_tail: None,
        ts: now,
    });

    let stats = outcomes::reap();
    assert_eq!(stats.attributed, 0);
    assert_eq!(stats.still_pending, 1);
    assert_eq!(stats.orphaned, 0);
    assert!(outcomes::load_resolved_map().is_empty());
}

#[test]
fn reaper_orphans_old_unattributed_outcome() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    // Outcome older than ORPHAN_AFTER_SECS (24h) with no matching decision.
    let stale_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 90_000; // 25h ago

    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("rm -rf /tmp/stale".into()),
        project: "p".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(0),
        duration_ms: None,
        stderr_tail: None,
        ts: stale_ts,
    });

    let stats = outcomes::reap();
    assert_eq!(stats.attributed, 0);
    assert_eq!(stats.orphaned, 1);

    // The pending dir is empty; the orphaned dir has the file.
    let pending: Vec<_> = std::fs::read_dir(outcomes::pending_dir())
        .unwrap()
        .collect();
    assert!(pending.is_empty(), "pending dir should be drained");
    let orphaned: Vec<_> = std::fs::read_dir(outcomes::orphaned_dir())
        .unwrap()
        .collect();
    assert_eq!(orphaned.len(), 1);
}

#[test]
fn reaper_will_not_double_attribute_one_decision() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let dec_ts = now - 5;
    let decision_id = format!("dec_{dec_ts}_7_0");
    write_decision_jsonl(&format!(
        r#"{{"ts":"{dec_ts}","pid":7,"project":"p","tool":"Bash","command":"echo hi","brain_action":"approve","brain_confidence":1.0,"brain_reasoning":"","user_action":"accept","decision_type":"session","decision_id":"{decision_id}"}}"#
    ));

    // Two pending outcomes for the same approach.
    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("echo hi".into()),
        project: "p".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(0),
        duration_ms: Some(1),
        stderr_tail: None,
        ts: now,
    });
    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("echo hi".into()),
        project: "p".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(0),
        duration_ms: Some(2),
        stderr_tail: None,
        ts: now + 1,
    });

    let stats = outcomes::reap();
    // Only one attributes; the other stays pending until a future decision
    // is logged.
    assert_eq!(stats.attributed, 1);
    assert_eq!(stats.still_pending, 1);
    assert_eq!(outcomes::load_resolved_map().len(), 1);
}

#[test]
fn reaper_skips_decisions_lacking_decision_id() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Old-style record (no decision_id field) — must be skipped.
    write_decision_jsonl(&format!(
        r#"{{"ts":"{}","pid":1,"project":"p","tool":"Bash","command":"ls","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"","user_action":"accept","decision_type":"session"}}"#,
        now - 5
    ));
    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("ls".into()),
        project: "p".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(0),
        duration_ms: None,
        stderr_tail: None,
        ts: now,
    });

    let stats = outcomes::reap();
    assert_eq!(stats.attributed, 0);
    assert_eq!(stats.still_pending, 1);
}

// ── #238: test-failure fan-out attribution ────────────────────────────

fn default_runners() -> Vec<String> {
    ["cargo test", "npm test", "pytest", "go test", "bun test"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn reaper_attributes_test_failure_to_recent_edit() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let edit_ts = now - 30;
    let edit_id = format!("dec_{edit_ts}_42_0");

    // A brain-approved Edit decision, then a Bash cargo test that fails.
    write_decision_jsonl(&format!(
        r#"{{"ts":"{edit_ts}","pid":42,"project":"alpha","tool":"Edit","command":"src/lib.rs","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept","decision_type":"session","decision_id":"{edit_id}"}}"#
    ));

    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("cargo test --release".into()),
        project: "alpha".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(101),
        duration_ms: Some(3_200),
        stderr_tail: Some("FAILED my_test::failing_case".into()),
        ts: now,
    });

    let stats = outcomes::reap_with_runners(&default_runners());
    assert_eq!(
        stats.test_failures_attributed, 1,
        "edit decision should receive one test-failure marker"
    );

    let markers = outcomes::load_test_failures();
    let m = markers
        .get(&edit_id)
        .expect("marker keyed by edit decision_id");
    assert_eq!(m.failed_test_command, "cargo test --release");
}

#[test]
fn reaper_test_failure_skips_passing_run() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let edit_ts = now - 30;
    let edit_id = format!("dec_{edit_ts}_42_0");

    write_decision_jsonl(&format!(
        r#"{{"ts":"{edit_ts}","pid":42,"project":"alpha","tool":"Edit","command":"src/lib.rs","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept","decision_type":"session","decision_id":"{edit_id}"}}"#
    ));

    // Successful test run → no fan-out marker.
    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("cargo test".into()),
        project: "alpha".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(0),
        duration_ms: Some(2_400),
        stderr_tail: None,
        ts: now,
    });

    let stats = outcomes::reap_with_runners(&default_runners());
    assert_eq!(stats.test_failures_attributed, 0);
    assert!(outcomes::load_test_failures().is_empty());
}

#[test]
fn reaper_test_failure_only_taps_edit_like_tools() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let read_ts = now - 30;
    let read_id = format!("dec_{read_ts}_42_0");

    // A Read decision — should NOT be tagged even though it precedes a failed test.
    write_decision_jsonl(&format!(
        r#"{{"ts":"{read_ts}","pid":42,"project":"alpha","tool":"Read","command":"src/lib.rs","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept","decision_type":"session","decision_id":"{read_id}"}}"#
    ));

    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("pytest".into()),
        project: "alpha".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(1),
        duration_ms: Some(500),
        stderr_tail: None,
        ts: now,
    });

    let stats = outcomes::reap_with_runners(&default_runners());
    assert_eq!(stats.test_failures_attributed, 0);
    assert!(outcomes::load_test_failures().is_empty());
}

#[test]
fn reaper_test_failure_is_idempotent() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let edit_ts = now - 30;
    let edit_id = format!("dec_{edit_ts}_99_0");

    write_decision_jsonl(&format!(
        r#"{{"ts":"{edit_ts}","pid":99,"project":"alpha","tool":"Write","command":"new.rs","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept","decision_type":"session","decision_id":"{edit_id}"}}"#
    ));

    // First failing test run.
    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("cargo test".into()),
        project: "alpha".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(101),
        duration_ms: Some(1_000),
        stderr_tail: None,
        ts: now,
    });
    let first = outcomes::reap_with_runners(&default_runners());
    assert_eq!(first.test_failures_attributed, 1);

    // A second failing run after attribution must not double-tag the same
    // edit — markers are written with create_new.
    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("cargo test".into()),
        project: "alpha".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(101),
        duration_ms: Some(1_000),
        stderr_tail: None,
        ts: now + 5,
    });
    let second = outcomes::reap_with_runners(&default_runners());
    assert_eq!(
        second.test_failures_attributed, 0,
        "second pass should not re-tag the same edit"
    );
    assert_eq!(outcomes::load_test_failures().len(), 1);
}

#[test]
fn reaper_test_failure_respects_fanout_window() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = isolated_home();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Edit is 10 minutes older than the test run — outside the 5-minute window.
    let edit_ts = now - 600;
    let edit_id = format!("dec_{edit_ts}_42_0");
    write_decision_jsonl(&format!(
        r#"{{"ts":"{edit_ts}","pid":42,"project":"alpha","tool":"Edit","command":"src/lib.rs","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept","decision_type":"session","decision_id":"{edit_id}"}}"#
    ));

    write_pending_file(&outcomes::PendingOutcome {
        tool: "Bash".into(),
        command: Some("cargo test".into()),
        project: "alpha".into(),
        session_id: None,
        tool_use_id: None,
        exit_code: Some(1),
        duration_ms: None,
        stderr_tail: None,
        ts: now,
    });

    let stats = outcomes::reap_with_runners(&default_runners());
    assert_eq!(
        stats.test_failures_attributed, 0,
        "edits outside the fan-out window should not be tagged"
    );
}

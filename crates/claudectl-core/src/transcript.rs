use serde_json::Value;

/// The tools that change a file, in Claude Code's vocabulary. One list for
/// the monitor's `files_modified`, the session focus (#596), the brain's
/// edit-outcome attribution and role suggestion, so a renamed tool is found
/// in one place.
pub const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Whether `name` is one of [`EDIT_TOOLS`].
pub fn is_edit_tool(name: &str) -> bool {
    EDIT_TOOLS.contains(&name)
}

/// The file an edit tool's `input` targets: `file_path`, or `notebook_path`
/// for `NotebookEdit`.
pub fn edited_path(input: &Value) -> Option<&str> {
    input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(|v| v.as_str())
}

/// The last `cap` bytes of a transcript, lossily decoded. Recent lines carry
/// the strongest "what is this session doing now" signal, and a bounded
/// read keeps a multi-hundred-MB transcript off the hot path. The first
/// line of the window is usually cut and fails to parse; callers skip it.
pub fn read_tail(path: &std::path::Path, cap: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len > cap {
        f.seek(SeekFrom::Start(len - cap))?;
    }
    let mut buf = Vec::new();
    f.take(cap).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    Assistant,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptUsage {
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// Total cache-write tokens, whatever their TTL.
    pub cache_creation_input_tokens: u64,
    /// The subset of the above written with a 1-hour TTL, which bills at 2x
    /// base input instead of 1.25x. Zero when the transcript carries no
    /// `cache_creation` breakdown, which prices everything at the 5-minute
    /// rate — the documented default.
    pub cache_creation_1h_input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone)]
pub enum TranscriptBlock {
    Text(String),
    ToolUse { name: String, input: Value },
    ToolResult { content: String, is_error: bool },
}

#[derive(Debug, Clone)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    /// The API message id. One assistant turn is written as one JSONL line per
    /// content block, every line repeating the same `usage`, so this is what
    /// tells those lines apart from genuinely separate turns.
    pub id: Option<String>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<TranscriptUsage>,
    pub content: Vec<TranscriptBlock>,
}

#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    WaitingForTask,
    Message(TranscriptMessage),
}

pub fn parse_line(line: &str) -> Option<TranscriptEvent> {
    let entry: Value = serde_json::from_str(line).ok()?;

    if is_waiting_for_task(&entry) {
        return Some(TranscriptEvent::WaitingForTask);
    }

    let msg = entry.get("message")?;
    let role = message_role(&entry, msg)?;

    let content = msg
        .get("content")
        .and_then(|v| v.as_array())
        .map(|blocks| blocks.iter().filter_map(parse_block).collect())
        .unwrap_or_default();

    Some(TranscriptEvent::Message(TranscriptMessage {
        role,
        id: msg
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        model: msg
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        stop_reason: msg
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        usage: msg.get("usage").and_then(parse_usage),
        content,
    }))
}

fn is_waiting_for_task(entry: &Value) -> bool {
    if entry.get("type").and_then(|v| v.as_str()) != Some("progress") {
        return false;
    }

    match entry.get("data") {
        Some(Value::String(s)) => s.contains("waiting_for_task"),
        Some(Value::Object(map)) => map.values().any(|v| {
            v.as_str()
                .map(|s| s.contains("waiting_for_task"))
                .unwrap_or(false)
        }),
        _ => false,
    }
}

fn message_role(entry: &Value, msg: &Value) -> Option<TranscriptRole> {
    let role = msg
        .get("role")
        .and_then(|v| v.as_str())
        .or_else(|| entry.get("type").and_then(|v| v.as_str()))?;

    match role {
        "assistant" => Some(TranscriptRole::Assistant),
        "user" => Some(TranscriptRole::User),
        _ => None,
    }
}

fn parse_usage(value: &Value) -> Option<TranscriptUsage> {
    Some(TranscriptUsage {
        input_tokens: value.get("input_tokens")?.as_u64().unwrap_or(0),
        cache_read_input_tokens: value
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation_input_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation_1h_input_tokens: value
            .get("cache_creation")
            .and_then(|v| v.get("ephemeral_1h_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

fn parse_block(block: &Value) -> Option<TranscriptBlock> {
    match block.get("type").and_then(|v| v.as_str())? {
        "text" => block
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| TranscriptBlock::Text(s.to_string())),
        "tool_use" => Some(TranscriptBlock::ToolUse {
            name: block
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => Some(TranscriptBlock::ToolResult {
            content: block
                .get("content")
                .and_then(extract_text_content)
                .unwrap_or_default(),
            is_error: block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }),
        _ => None,
    }
}

fn extract_text_content(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }

    let blocks = value.as_array()?;
    let mut parts = Vec::new();
    for block in blocks {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_real_fixture_line() {
        let line = include_str!("../../../tests/fixtures/real-transcript-line.json");
        let Some(TranscriptEvent::Message(msg)) = parse_line(line.trim()) else {
            panic!("expected message event");
        };
        assert_eq!(msg.role, TranscriptRole::Assistant);
        assert_eq!(msg.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(msg.model.as_deref(), Some("claude-sonnet-4-6-20260401"));
        assert_eq!(msg.content.len(), 2);
    }

    #[test]
    fn parse_legacy_fixture_line() {
        // Fixture lives at the workspace root, not in this crate.
        let line = include_str!("../../../tests/fixtures/legacy-transcript-line.json");
        let Some(TranscriptEvent::Message(msg)) = parse_line(line.trim()) else {
            panic!("expected message event");
        };
        assert_eq!(msg.role, TranscriptRole::Assistant);
        assert_eq!(msg.stop_reason.as_deref(), Some("end_turn"));
        assert!(msg.usage.is_some());
    }

    #[test]
    fn parse_waiting_for_task_progress() {
        let line = r#"{"type":"progress","data":"waiting_for_task"}"#;
        assert!(matches!(
            parse_line(line),
            Some(TranscriptEvent::WaitingForTask)
        ));
    }
}

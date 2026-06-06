use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    Assistant,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptUsage {
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
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
        let line = include_str!("../tests/fixtures/real-transcript-line.json");
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
        let line = include_str!("../tests/fixtures/legacy-transcript-line.json");
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

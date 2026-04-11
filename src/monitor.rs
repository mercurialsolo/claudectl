use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

use crate::session::{ClaudeSession, SessionStatus};

/// Read new JSONL entries since last offset, accumulate token stats.
pub fn update_tokens(session: &mut ClaudeSession) {
    let Some(ref path) = session.jsonl_path else {
        return;
    };

    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

    if session.jsonl_offset > 0 && session.jsonl_offset >= file_len {
        return;
    }

    if session.jsonl_offset > 0 && file.seek(SeekFrom::Start(session.jsonl_offset)).is_err() {
        return;
    }

    let reader = BufReader::new(&file);
    let mut last_type = String::new();
    let mut last_stop_reason = String::new();
    let mut is_waiting_for_task = false;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if !line.contains("\"type\"") {
            continue;
        }

        // Detect "waiting_for_task" progress events (user confirmation needed)
        if line.contains("waiting_for_task") {
            is_waiting_for_task = true;
        }

        // Extract message type — resets waiting_for_task when conversation continues
        if line.contains("\"type\":\"user\"") || line.contains("\"type\": \"user\"") {
            last_type = "user".to_string();
            last_stop_reason.clear();
            is_waiting_for_task = false;
        } else if line.contains("\"type\":\"assistant\"")
            || line.contains("\"type\": \"assistant\"")
        {
            last_type = "assistant".to_string();
            is_waiting_for_task = false;
        }

        // Parse assistant messages for stop_reason and usage
        if line.contains("\"usage\"") || line.contains("\"stop_reason\"") {
            if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
                // Extract stop_reason: "end_turn" = done, "tool_use" = still working
                if let Some(reason) = entry
                    .get("message")
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|v| v.as_str())
                {
                    last_stop_reason = reason.to_string();
                }

                // Extract token usage
                if let Some(usage) = entry.get("message").and_then(|m| m.get("usage")) {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_create = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    session.total_input_tokens += input + cache_read + cache_create;
                    session.total_output_tokens += output;
                    session.cache_read_tokens += cache_read;
                    session.cache_write_tokens += cache_create;

                    // Track context window: the input_tokens of the LAST API call
                    // represents the current prompt/context size
                    let context_size = input + cache_read + cache_create;
                    if context_size > 0 {
                        session.context_tokens = context_size;
                    }

                    if session.model.is_empty() {
                        if let Some(model) = entry
                            .get("message")
                            .and_then(|m| m.get("model"))
                            .and_then(|v| v.as_str())
                        {
                            session.model = shorten_model(model);
                        }
                    }
                }
            }
        }
    }

    session.jsonl_offset = file_len;

    // Use the JSONL file's mtime as "last activity" — reliable, no timestamp parsing needed
    if let Some(ref path) = session.jsonl_path {
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(modified) = meta.modified() {
                let mtime_ms = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                session.last_message_ts = mtime_ms;
            }
        }
    }

    // Set context window max based on model
    session.context_max = model_context_max(&session.model);

    // Compute cost estimate based on model pricing
    session.cost_usd = estimate_cost(session);

    infer_status(session, &last_type, &last_stop_reason, is_waiting_for_task);
}

fn infer_status(
    session: &mut ClaudeSession,
    last_msg_type: &str,
    last_stop_reason: &str,
    is_waiting_for_task: bool,
) {
    // CPU is the strongest real-time signal — if the process is burning CPU,
    // it's processing regardless of what the JSONL says (JSONL can lag).
    if session.cpu_percent > 5.0 {
        session.status = SessionStatus::Processing;
        return;
    }

    // NeedsInput: JSONL says waiting_for_task and CPU is low (confirmed idle)
    if is_waiting_for_task {
        session.status = SessionStatus::NeedsInput;
        return;
    }

    if last_msg_type == "assistant" && last_stop_reason == "end_turn" {
        // Claude finished its turn — waiting for user input
        // But if it's been a long time (>10 min), mark as Idle
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let age_mins = (now_ms.saturating_sub(session.last_message_ts)) / 60_000;

        if age_mins > 10 {
            session.status = SessionStatus::Idle;
        } else {
            session.status = SessionStatus::WaitingInput;
        }
        return;
    }

    if last_msg_type == "assistant" && last_stop_reason == "tool_use" {
        // Claude called a tool. If CPU is low and some time has passed,
        // it's likely waiting for user to approve/deny the tool (permission prompt).
        // The permission prompt doesn't emit waiting_for_task — detect via CPU + age.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let age_secs = (now_ms.saturating_sub(session.last_message_ts)) / 1000;

        if session.cpu_percent < 2.0 && age_secs > 5 {
            // Low CPU + tool_use was >5s ago = stuck on permission prompt
            session.status = SessionStatus::NeedsInput;
        } else {
            session.status = SessionStatus::Processing;
        }
        return;
    }

    if last_msg_type == "user" {
        // User sent a message, Claude hasn't finished responding
        if session.cpu_percent > 1.0 {
            session.status = SessionStatus::Processing;
        } else {
            // Low CPU + user message pending — might be waiting for API or stalled
            session.status = SessionStatus::Processing;
        }
        return;
    }

    session.status = SessionStatus::Idle;
}

/// Estimate USD cost based on token usage and model.
fn estimate_cost(session: &ClaudeSession) -> f64 {
    // Plain input tokens = total_input - cache_read - cache_write
    let plain_input = session
        .total_input_tokens
        .saturating_sub(session.cache_read_tokens)
        .saturating_sub(session.cache_write_tokens);

    let (input_per_m, output_per_m, cache_read_per_m, cache_write_per_m) =
        if session.model.contains("opus") {
            (15.0, 75.0, 1.875, 18.75)
        } else if session.model.contains("sonnet") {
            (3.0, 15.0, 0.375, 3.75)
        } else if session.model.contains("haiku") {
            (0.80, 4.0, 0.10, 1.0)
        } else {
            // Default to opus pricing (conservative)
            (15.0, 75.0, 1.875, 18.75)
        };

    (plain_input as f64 / 1_000_000.0) * input_per_m
        + (session.total_output_tokens as f64 / 1_000_000.0) * output_per_m
        + (session.cache_read_tokens as f64 / 1_000_000.0) * cache_read_per_m
        + (session.cache_write_tokens as f64 / 1_000_000.0) * cache_write_per_m
}

/// Max context window tokens by model.
fn model_context_max(model: &str) -> u64 {
    if model.contains("opus") {
        // Opus 4.6 with extended thinking supports up to 1M
        1_000_000
    } else {
        // Sonnet, Haiku, and other models default to 200k
        200_000
    }
}

fn shorten_model(model: &str) -> String {
    if model.contains("opus") {
        if model.contains("4-6") {
            "opus-4.6".into()
        } else {
            "opus".into()
        }
    } else if model.contains("sonnet") {
        if model.contains("4-6") {
            "sonnet-4.6".into()
        } else {
            "sonnet".into()
        }
    } else if model.contains("haiku") {
        "haiku".into()
    } else {
        model.to_string()
    }
}

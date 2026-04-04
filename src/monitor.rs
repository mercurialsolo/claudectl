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
    let mut last_ts: u64 = 0;
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

        // Extract timestamp from any typed message
        if line.contains("\"timestamp\"") {
            // Quick extraction: find "timestamp":"..." and parse ISO to epoch ms
            if let Some(ts) = extract_timestamp(&line) {
                last_ts = ts;
            }
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
    if last_ts > 0 {
        session.last_message_ts = last_ts;
    }

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
    // Paused:     waiting_for_task event (needs user confirmation to proceed)
    // Processing: high CPU, or tool_use in progress, or user message pending
    // Waiting:    end_turn + recent (done, needs user's next prompt)
    // Idle:       stale or no data

    // Paused takes priority — Claude is blocked on user confirmation
    if is_waiting_for_task {
        session.status = SessionStatus::Paused;
        return;
    }

    if session.cpu_percent > 5.0 {
        session.status = SessionStatus::Processing;
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
        // Claude called a tool — still processing
        session.status = SessionStatus::Processing;
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

    let cost = (plain_input as f64 / 1_000_000.0) * input_per_m
        + (session.total_output_tokens as f64 / 1_000_000.0) * output_per_m
        + (session.cache_read_tokens as f64 / 1_000_000.0) * cache_read_per_m
        + (session.cache_write_tokens as f64 / 1_000_000.0) * cache_write_per_m;

    cost
}

/// Extract epoch ms from a JSONL timestamp field like "2026-04-03T20:51:59.169Z"
fn extract_timestamp(line: &str) -> Option<u64> {
    let marker = "\"timestamp\":\"";
    let start = line.find(marker)? + marker.len();
    let end = line[start..].find('"')? + start;
    let ts_str = &line[start..end];

    // Parse ISO 8601: "2026-04-03T20:51:59.169Z"
    let parts: Vec<&str> = ts_str.split('T').collect();
    if parts.len() != 2 {
        return None;
    }

    let date_parts: Vec<u64> = parts[0].split('-').filter_map(|s| s.parse().ok()).collect();
    if date_parts.len() != 3 {
        return None;
    }

    let time_str = parts[1].trim_end_matches('Z');
    let time_parts: Vec<&str> = time_str.split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }

    let hour: u64 = time_parts[0].parse().ok()?;
    let min: u64 = time_parts[1].parse().ok()?;
    let sec_parts: Vec<&str> = time_parts[2].split('.').collect();
    let sec: u64 = sec_parts[0].parse().ok()?;
    let ms: u64 = if sec_parts.len() > 1 {
        let frac = sec_parts[1];
        let padded = format!("{:0<3}", &frac[..frac.len().min(3)]);
        padded.parse().unwrap_or(0)
    } else {
        0
    };

    // Rough epoch calculation (good enough for age comparison)
    let (y, m, d) = (date_parts[0], date_parts[1], date_parts[2]);
    let days = (y - 1970) * 365 + (y - 1969) / 4 // leap years approx
        + match m {
            1 => 0, 2 => 31, 3 => 59, 4 => 90, 5 => 120, 6 => 151,
            7 => 181, 8 => 212, 9 => 243, 10 => 273, 11 => 304, 12 => 334,
            _ => 0,
        }
        + d - 1;
    let epoch_ms = days * 86400 * 1000 + hour * 3600 * 1000 + min * 60 * 1000 + sec * 1000 + ms;

    Some(epoch_ms)
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

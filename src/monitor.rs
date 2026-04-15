use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

use serde_json::Value;

use crate::models;
use crate::session::{ClaudeSession, SessionStatus, SubagentRollup, TelemetryStatus};
use crate::transcript::{TranscriptBlock, TranscriptEvent, TranscriptRole, parse_line};

#[derive(Default)]
struct UsageRollup {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cost_usd: f64,
    usage_metrics_available: bool,
    cost_estimate_unverified: bool,
}

impl UsageRollup {
    fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// Read new JSONL entries since last offset, accumulate token stats.
pub fn update_tokens(session: &mut ClaudeSession) {
    // Seed from persisted state so status inference works on ticks with no new JSONL.
    let mut last_type = session.last_msg_type.clone();
    let mut last_stop_reason = session.last_stop_reason.clone();
    let mut is_waiting_for_task = session.is_waiting_for_task;
    let mut saw_non_empty_line = false;
    let mut recognized_events = 0usize;
    let mut saw_parent_usage = false;
    let jsonl_path = session.jsonl_path.clone();

    match jsonl_path.as_ref() {
        Some(path) => {
            let mut file = match File::open(path) {
                Ok(f) => f,
                Err(_) => {
                    session.telemetry_status = TelemetryStatus::UnreadableTranscript;
                    finalize_usage(
                        session,
                        &last_type,
                        &last_stop_reason,
                        is_waiting_for_task,
                        false,
                    );
                    return;
                }
            };

            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

            if file_len == 0 {
                session.telemetry_status = TelemetryStatus::Pending;
            } else {
                if session.jsonl_offset > file_len {
                    session.jsonl_offset = 0;
                    session.own_input_tokens = 0;
                    session.own_output_tokens = 0;
                    session.own_cache_read_tokens = 0;
                    session.own_cache_write_tokens = 0;
                    // Reset persisted inference state on file truncation
                    last_type.clear();
                    last_stop_reason.clear();
                    is_waiting_for_task = false;
                }

                if session.jsonl_offset < file_len {
                    if session.jsonl_offset > 0
                        && file.seek(SeekFrom::Start(session.jsonl_offset)).is_err()
                    {
                        finalize_usage(
                            session,
                            &last_type,
                            &last_stop_reason,
                            is_waiting_for_task,
                            false,
                        );
                        return;
                    }

                    let reader = BufReader::new(&file);

                    for line in reader.lines() {
                        let line = match line {
                            Ok(l) => l,
                            Err(_) => break,
                        };

                        if line.trim().is_empty() {
                            continue;
                        }
                        saw_non_empty_line = true;

                        let Some(event) = parse_line(&line) else {
                            continue;
                        };
                        recognized_events += 1;

                        match event {
                            TranscriptEvent::WaitingForTask => {
                                is_waiting_for_task = true;
                            }
                            TranscriptEvent::Message(message) => {
                                is_waiting_for_task = false;
                                last_type = match message.role {
                                    TranscriptRole::Assistant => "assistant".to_string(),
                                    TranscriptRole::User => "user".to_string(),
                                };

                                if let Some(reason) = message.stop_reason {
                                    last_stop_reason = reason;
                                } else {
                                    // Claude Code sometimes writes assistant messages
                                    // with stop_reason: null when a tool_use block is
                                    // awaiting user approval.  Infer from content.
                                    let has_tool_use = message
                                        .content
                                        .iter()
                                        .any(|b| matches!(b, TranscriptBlock::ToolUse { .. }));
                                    if has_tool_use {
                                        last_stop_reason = "tool_use".to_string();
                                    } else {
                                        last_stop_reason.clear();
                                    }
                                }

                                if let Some(usage) = message.usage {
                                    let input = usage.input_tokens;
                                    let cache_read = usage.cache_read_input_tokens;
                                    let cache_create = usage.cache_creation_input_tokens;
                                    let output = usage.output_tokens;

                                    session.own_input_tokens += input + cache_read + cache_create;
                                    session.own_output_tokens += output;
                                    session.own_cache_read_tokens += cache_read;
                                    session.own_cache_write_tokens += cache_create;
                                    saw_parent_usage = true;

                                    // Track context window: the input_tokens of the LAST API call
                                    // represents the current prompt/context size
                                    let context_size = input + cache_read + cache_create;
                                    if context_size > 0 {
                                        session.context_tokens = context_size;
                                    }
                                }

                                if let Some(model) = message.model {
                                    session.model = shorten_model(&model);
                                }

                                for block in message.content {
                                    match &block {
                                        TranscriptBlock::ToolUse { name, input } => {
                                            record_tool_usage(name, input, session);
                                            // Track pending tool for rule-based auto-actions
                                            session.pending_tool_name = Some(name.clone());
                                            session.pending_tool_input = input
                                                .get("command")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string());
                                            // Track pending file path for conflict detection
                                            session.pending_file_path = if matches!(
                                                name.as_str(),
                                                "Edit" | "Write" | "NotebookEdit"
                                            ) {
                                                input
                                                    .get("file_path")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string())
                                            } else {
                                                None
                                            };
                                        }
                                        TranscriptBlock::ToolResult {
                                            is_error, content, ..
                                        } => {
                                            session.last_tool_error = *is_error;
                                            if *is_error {
                                                let truncated = if content.len() > 256 {
                                                    format!("{}...", &content[..256])
                                                } else {
                                                    content.clone()
                                                };
                                                let tool_name = session
                                                    .pending_tool_name
                                                    .clone()
                                                    .unwrap_or_else(|| "?".into());
                                                session.last_error_message =
                                                    Some(truncated.clone());
                                                session.recent_errors.push(
                                                    crate::session::ErrorEntry {
                                                        tool_name,
                                                        message: truncated,
                                                    },
                                                );
                                                if session.recent_errors.len() > 5 {
                                                    session.recent_errors.remove(0);
                                                }
                                            } else {
                                                session.last_error_message = None;
                                            }
                                            // Tool was executed — no longer pending
                                            session.pending_tool_name = None;
                                            session.pending_tool_input = None;
                                            session.pending_file_path = None;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }

                if recognized_events > 0 || session.telemetry_status.is_available() {
                    session.telemetry_status = TelemetryStatus::Available;
                } else if saw_non_empty_line {
                    session.telemetry_status = TelemetryStatus::UnsupportedTranscript;
                } else {
                    session.telemetry_status = TelemetryStatus::Pending;
                }

                session.jsonl_offset = file_len;
            }

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
        None => {
            session.telemetry_status = TelemetryStatus::MissingTranscript;
        }
    }

    finalize_usage(
        session,
        &last_type,
        &last_stop_reason,
        is_waiting_for_task,
        saw_parent_usage,
    );
}

fn finalize_usage(
    session: &mut ClaudeSession,
    last_type: &str,
    last_stop_reason: &str,
    is_waiting_for_task: bool,
    saw_parent_usage: bool,
) {
    let resolved_profile = models::resolve(&session.model);
    session.context_max = resolved_profile.profile.context_max;
    session.model_profile_source = resolved_profile.source.label().to_string();

    let subagent_rollup = refresh_subagent_rollups(session);
    session.subagent_input_tokens = subagent_rollup.total_input_tokens();
    session.subagent_output_tokens = subagent_rollup.output_tokens;
    session.subagent_cache_read_tokens = subagent_rollup.cache_read_tokens;
    session.subagent_cache_write_tokens = subagent_rollup.cache_write_tokens;
    session.subagent_count = session.subagent_rollups.len();

    session.total_input_tokens = session.own_input_tokens + session.subagent_input_tokens;
    session.total_output_tokens = session.own_output_tokens + session.subagent_output_tokens;
    session.cache_read_tokens = session.own_cache_read_tokens + session.subagent_cache_read_tokens;
    session.cache_write_tokens =
        session.own_cache_write_tokens + session.subagent_cache_write_tokens;

    let own_usage_metrics_available = saw_parent_usage
        || session.own_input_tokens > 0
        || session.own_output_tokens > 0
        || session.own_cache_read_tokens > 0
        || session.own_cache_write_tokens > 0;
    let (own_cost, own_cost_unverified) = estimate_cost_components(
        &session.model,
        session.own_input_tokens,
        session.own_output_tokens,
        session.own_cache_read_tokens,
        session.own_cache_write_tokens,
    );
    session.cost_usd = own_cost + subagent_rollup.cost_usd;
    session.usage_metrics_available =
        own_usage_metrics_available || subagent_rollup.usage_metrics_available;
    session.cost_estimate_unverified = (own_usage_metrics_available && own_cost_unverified)
        || subagent_rollup.cost_estimate_unverified;

    // Persist for next tick (so status inference works when no new JSONL arrives).
    session.last_msg_type = last_type.to_string();
    session.last_stop_reason = last_stop_reason.to_string();
    session.is_waiting_for_task = is_waiting_for_task;

    infer_status(session, last_type, last_stop_reason, is_waiting_for_task);
}

pub fn infer_status(
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

    if !session.telemetry_status.is_available() && last_msg_type.is_empty() {
        session.status = SessionStatus::Unknown;
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
#[allow(dead_code)]
pub fn estimate_cost(session: &ClaudeSession) -> f64 {
    estimate_cost_components(
        &session.model,
        session.total_input_tokens,
        session.total_output_tokens,
        session.cache_read_tokens,
        session.cache_write_tokens,
    )
    .0
}

/// Max context window tokens by model.
pub fn model_context_max(model: &str) -> u64 {
    models::resolve(model).profile.context_max
}

/// Extract tool usage stats and file paths from tool_use content blocks.
fn record_tool_usage(tool_name: &str, input: &Value, session: &mut ClaudeSession) {
    if tool_name.is_empty() {
        return;
    }

    session
        .tool_usage
        .entry(tool_name.to_string())
        .or_default()
        .calls += 1;

    if matches!(tool_name, "Edit" | "Write" | "NotebookEdit") {
        if let Some(path) = input.get("file_path").and_then(|p| p.as_str()) {
            *session.files_modified.entry(path.to_string()).or_insert(0) += 1;
        }
    }
}

pub fn shorten_model(model: &str) -> String {
    models::shorten_model(model)
}

fn refresh_subagent_rollups(session: &mut ClaudeSession) -> UsageRollup {
    for path in session.active_subagent_jsonl_paths.clone() {
        let rollup = session.subagent_rollups.entry(path.clone()).or_default();
        update_subagent_rollup(&path, rollup, &session.model);
    }

    let mut totals = UsageRollup::default();
    for rollup in session.subagent_rollups.values() {
        totals.input_tokens += rollup.input_tokens;
        totals.output_tokens += rollup.output_tokens;
        totals.cache_read_tokens += rollup.cache_read_tokens;
        totals.cache_write_tokens += rollup.cache_write_tokens;
        totals.cost_usd += rollup.cost_usd;
        totals.usage_metrics_available |= rollup.usage_metrics_available;
        totals.cost_estimate_unverified |= rollup.cost_estimate_unverified;
    }
    totals
}

fn update_subagent_rollup(
    path: &std::path::Path,
    rollup: &mut SubagentRollup,
    default_model: &str,
) {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return,
    };

    let file_len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if rollup.jsonl_offset > file_len {
        *rollup = SubagentRollup::default();
    }

    if rollup.jsonl_offset >= file_len {
        rollup.jsonl_offset = file_len;
        return;
    }

    if rollup.jsonl_offset > 0 && file.seek(SeekFrom::Start(rollup.jsonl_offset)).is_err() {
        return;
    }

    let mut current_model = if rollup.model.is_empty() {
        default_model.to_string()
    } else {
        rollup.model.clone()
    };

    let reader = BufReader::new(&file);
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let Some(TranscriptEvent::Message(message)) = parse_line(&line) else {
            continue;
        };

        if let Some(model) = message.model {
            current_model = shorten_model(&model);
            rollup.model = current_model.clone();
        }

        let Some(usage) = message.usage else {
            continue;
        };

        rollup.input_tokens += usage.input_tokens;
        rollup.output_tokens += usage.output_tokens;
        rollup.cache_read_tokens += usage.cache_read_input_tokens;
        rollup.cache_write_tokens += usage.cache_creation_input_tokens;
        rollup.usage_metrics_available = true;

        let input_with_cache =
            usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
        let model_for_cost = if current_model.is_empty() {
            default_model
        } else {
            current_model.as_str()
        };
        let (delta_cost, unverified) = estimate_cost_components(
            model_for_cost,
            input_with_cache,
            usage.output_tokens,
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
        );
        rollup.cost_usd += delta_cost;
        rollup.cost_estimate_unverified |= unverified;
    }

    rollup.jsonl_offset = file_len;
}

fn estimate_cost_components(
    model: &str,
    total_input_tokens: u64,
    total_output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> (f64, bool) {
    let plain_input = total_input_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_write_tokens);
    let resolved = models::resolve(model);

    let cost = (plain_input as f64 / 1_000_000.0) * resolved.profile.input_per_m
        + (total_output_tokens as f64 / 1_000_000.0) * resolved.profile.output_per_m
        + (cache_read_tokens as f64 / 1_000_000.0) * resolved.profile.cache_read_per_m
        + (cache_write_tokens as f64 / 1_000_000.0) * resolved.profile.cache_write_per_m;

    (
        cost,
        resolved.source == models::ModelProfileSource::Fallback,
    )
}

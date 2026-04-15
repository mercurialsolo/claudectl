use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionStatus {
    NeedsInput,   // Blocked — waiting for user to approve/confirm (permission prompt)
    Processing,   // Actively generating or executing tools
    WaitingInput, // Done responding, waiting for user's next prompt
    Unknown,      // Process is alive, but transcript telemetry is unavailable
    Idle,         // No recent activity, stale session
    Finished,     // Process exited
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsInput => write!(f, "Needs Input"),
            Self::Processing => write!(f, "Processing"),
            Self::WaitingInput => write!(f, "Waiting"),
            Self::Unknown => write!(f, "Unknown"),
            Self::Idle => write!(f, "Idle"),
            Self::Finished => write!(f, "Finished"),
        }
    }
}

impl SessionStatus {
    pub fn sort_key(&self) -> u8 {
        match self {
            Self::NeedsInput => 0,
            Self::Processing => 1,
            Self::WaitingInput => 2,
            Self::Unknown => 3,
            Self::Idle => 4,
            Self::Finished => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryStatus {
    Pending,
    Available,
    MissingTranscript,
    UnreadableTranscript,
    UnsupportedTranscript,
}

impl TelemetryStatus {
    pub fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Available => "Available",
            Self::MissingTranscript => "No transcript",
            Self::UnreadableTranscript => "Unreadable transcript",
            Self::UnsupportedTranscript => "Unsupported transcript",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Available => "Available",
            Self::MissingTranscript => "No transcript",
            Self::UnreadableTranscript => "Unreadable",
            Self::UnsupportedTranscript => "Unsupported",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RawSession {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "startedAt")]
    pub started_at: u64,
}

#[derive(Debug, Clone)]
pub struct ClaudeSession {
    pub pid: u32,
    #[allow(dead_code)]
    pub session_id: String,
    pub cwd: String,
    pub project_name: String,
    pub started_at: u64,
    pub elapsed: Duration,
    pub tty: String,
    pub status: SessionStatus,
    pub cpu_percent: f32,
    pub cpu_history: Vec<f32>, // Last N CPU readings for smoothing
    pub mem_mb: f64,
    pub own_input_tokens: u64,
    pub own_output_tokens: u64,
    pub own_cache_read_tokens: u64,
    pub own_cache_write_tokens: u64,
    pub subagent_input_tokens: u64,
    pub subagent_output_tokens: u64,
    pub subagent_cache_read_tokens: u64,
    pub subagent_cache_write_tokens: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub model: String,
    pub command_args: String,
    pub session_name: String,
    pub jsonl_path: Option<PathBuf>,
    pub jsonl_offset: u64,
    pub last_message_ts: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub context_tokens: u64,
    pub context_max: u64,
    pub prev_cost_usd: f64,
    pub burn_rate_per_hr: f64,
    pub subagent_count: usize,
    pub active_subagent_count: usize,
    pub active_subagent_jsonl_paths: Vec<PathBuf>,
    pub subagent_rollups: HashMap<PathBuf, SubagentRollup>,
    pub activity_history: Vec<u8>, // Ring buffer of status levels (0-7) for sparkline, one per tick
    pub files_modified: HashMap<String, u32>, // file path -> edit count
    pub tool_usage: HashMap<String, ToolStats>, // tool name -> call count & tokens
    pub worktree_id: Option<String>, // Resolved git toplevel + git-dir, for conflict detection
    pub telemetry_status: TelemetryStatus,
    pub usage_metrics_available: bool,
    pub cost_estimate_unverified: bool,
    pub model_profile_source: String,
    /// Persisted across ticks so status inference works when no new JSONL arrives.
    pub last_msg_type: String,
    pub last_stop_reason: String,
    pub is_waiting_for_task: bool,
    /// Pending tool call details for rule-based auto-actions.
    pub pending_tool_name: Option<String>,
    pub pending_tool_input: Option<String>, // Extracted command string (for Bash)
    pub pending_file_path: Option<String>,  // File path for pending Edit/Write/NotebookEdit
    pub has_file_conflict: bool,            // Pending file edit conflicts with another session
    pub last_tool_error: bool,
    pub last_error_message: Option<String>,
    pub recent_errors: Vec<ErrorEntry>, // Last 5 errors (ring buffer)
}

/// A captured tool error with context.
#[derive(Debug, Clone)]
pub struct ErrorEntry {
    pub tool_name: String,
    pub message: String,
}

/// Per-tool usage statistics.
#[derive(Debug, Clone, Default)]
pub struct ToolStats {
    pub calls: u32,
}

#[derive(Debug, Clone, Default)]
pub struct SubagentRollup {
    pub jsonl_offset: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub model: String,
    pub cost_estimate_unverified: bool,
    pub usage_metrics_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    Active,
    Completed,
}

#[derive(Debug, Clone)]
pub struct SubagentBreakdown {
    pub label: String,
    pub state: SubagentState,
    pub count: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub usage_metrics_available: bool,
    pub cost_estimate_unverified: bool,
}

impl SubagentBreakdown {
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn state_label(&self) -> String {
        match self.state {
            SubagentState::Active => "Active".to_string(),
            SubagentState::Completed if self.count > 1 => format!("Completed ({})", self.count),
            SubagentState::Completed => "Completed".to_string(),
        }
    }

    pub fn display_label(&self) -> String {
        if self.state == SubagentState::Completed && self.label == "completed" && self.count > 1 {
            format!("completed ({})", self.count)
        } else {
            self.label.clone()
        }
    }

    pub fn format_tokens(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        let total = self.total_input_tokens() + self.output_tokens;
        if total == 0 {
            return "-".to_string();
        }
        format_count(self.total_input_tokens()) + "/" + &format_count(self.output_tokens)
    }

    pub fn format_cost(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        if self.cost_usd < 0.01 {
            return "-".to_string();
        }
        if self.cost_usd < 1.0 {
            format!(
                "${:.2}{}",
                self.cost_usd,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "${:.1}{}",
                self.cost_usd,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        }
    }
}

impl ClaudeSession {
    pub fn from_raw(raw: RawSession) -> Self {
        let project_name = raw.cwd.rsplit('/').next().unwrap_or("unknown").to_string();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let elapsed_ms = now_ms.saturating_sub(raw.started_at);
        let elapsed = Duration::from_millis(elapsed_ms);

        Self {
            pid: raw.pid,
            session_id: raw.session_id,
            cwd: raw.cwd,
            project_name,
            started_at: raw.started_at,
            elapsed,
            tty: String::new(),
            status: SessionStatus::Idle,
            cpu_percent: 0.0,
            cpu_history: Vec::new(),
            mem_mb: 0.0,
            own_input_tokens: 0,
            own_output_tokens: 0,
            own_cache_read_tokens: 0,
            own_cache_write_tokens: 0,
            subagent_input_tokens: 0,
            subagent_output_tokens: 0,
            subagent_cache_read_tokens: 0,
            subagent_cache_write_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            model: String::new(),
            command_args: String::new(),
            session_name: String::new(),
            jsonl_path: None,
            jsonl_offset: 0,
            last_message_ts: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 0.0,
            context_tokens: 0,
            context_max: 0,
            prev_cost_usd: 0.0,
            burn_rate_per_hr: 0.0,
            subagent_count: 0,
            active_subagent_count: 0,
            active_subagent_jsonl_paths: Vec::new(),
            subagent_rollups: HashMap::new(),
            activity_history: Vec::new(),
            files_modified: HashMap::new(),
            tool_usage: HashMap::new(),
            worktree_id: None,
            telemetry_status: TelemetryStatus::Pending,
            usage_metrics_available: false,
            cost_estimate_unverified: false,
            model_profile_source: "built-in".into(),
            last_msg_type: String::new(),
            last_stop_reason: String::new(),
            is_waiting_for_task: false,
            pending_tool_name: None,
            pending_tool_input: None,
            pending_file_path: None,
            has_file_conflict: false,
            last_tool_error: false,
            last_error_message: None,
            recent_errors: Vec::new(),
        }
    }

    /// Record current status into the activity sparkline ring buffer.
    /// Max 15 entries (one per tick, at 2s default = 30s of history).
    pub fn record_activity(&mut self) {
        let level = match self.status {
            SessionStatus::Processing => 7,
            SessionStatus::NeedsInput => 4,
            SessionStatus::WaitingInput => 2,
            SessionStatus::Unknown => 2,
            SessionStatus::Idle => 1,
            SessionStatus::Finished => 0,
        };
        self.activity_history.push(level);
        if self.activity_history.len() > 15 {
            self.activity_history.remove(0);
        }
    }

    /// Render the sparkline as unicode block characters.
    pub fn format_sparkline(&self) -> String {
        const BLOCKS: &[char] = &[
            ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}',
            '\u{2587}', '\u{2588}',
        ];
        if self.activity_history.is_empty() {
            return String::from("-");
        }
        self.activity_history
            .iter()
            .map(|&level| BLOCKS[level.min(8) as usize])
            .collect()
    }

    pub fn display_name(&self) -> &str {
        if !self.session_name.is_empty() {
            &self.session_name
        } else {
            &self.project_name
        }
    }

    pub fn format_subagent_summary(&self) -> String {
        if self.subagent_count == 0 {
            return "0".to_string();
        }
        if self.active_subagent_count == 0 || self.active_subagent_count == self.subagent_count {
            return self.subagent_count.to_string();
        }
        format!(
            "{} total ({} active)",
            self.subagent_count, self.active_subagent_count
        )
    }

    pub fn subagent_breakdown(&self) -> Vec<SubagentBreakdown> {
        if self.subagent_rollups.is_empty() {
            return Vec::new();
        }

        let active_paths: HashSet<&PathBuf> = self.active_subagent_jsonl_paths.iter().collect();
        let mut active_rows = Vec::new();
        let mut completed_rows = Vec::new();

        for (path, rollup) in &self.subagent_rollups {
            let row = SubagentBreakdown {
                label: subagent_label(path),
                state: if active_paths.contains(path) {
                    SubagentState::Active
                } else {
                    SubagentState::Completed
                },
                count: 1,
                input_tokens: rollup.input_tokens,
                output_tokens: rollup.output_tokens,
                cache_read_tokens: rollup.cache_read_tokens,
                cache_write_tokens: rollup.cache_write_tokens,
                cost_usd: rollup.cost_usd,
                usage_metrics_available: rollup.usage_metrics_available,
                cost_estimate_unverified: rollup.cost_estimate_unverified,
            };

            if row.state == SubagentState::Active {
                active_rows.push(row);
            } else {
                completed_rows.push(row);
            }
        }

        active_rows.sort_by(|a, b| a.label.cmp(&b.label));

        let mut rows = Vec::new();
        if !completed_rows.is_empty() {
            let mut aggregate = SubagentBreakdown {
                label: "completed".to_string(),
                state: SubagentState::Completed,
                count: completed_rows.len(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.0,
                usage_metrics_available: false,
                cost_estimate_unverified: false,
            };

            for row in completed_rows {
                aggregate.input_tokens += row.input_tokens;
                aggregate.output_tokens += row.output_tokens;
                aggregate.cache_read_tokens += row.cache_read_tokens;
                aggregate.cache_write_tokens += row.cache_write_tokens;
                aggregate.cost_usd += row.cost_usd;
                aggregate.usage_metrics_available |= row.usage_metrics_available;
                aggregate.cost_estimate_unverified |= row.cost_estimate_unverified;
            }

            rows.push(aggregate);
        }

        rows.extend(active_rows);
        rows
    }

    pub fn format_elapsed(&self) -> String {
        let secs = self.elapsed.as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{h:02}:{m:02}:{s:02}")
        } else {
            format!("{m:02}:{s:02}")
        }
    }

    pub fn format_tokens(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        let total = self.total_input_tokens + self.total_output_tokens;
        if total == 0 {
            return String::from("-");
        }
        format_count(self.total_input_tokens) + "/" + &format_count(self.total_output_tokens)
    }

    pub fn format_mem(&self) -> String {
        if self.mem_mb < 1.0 {
            return String::from("-");
        }
        format!("{:.0}M", self.mem_mb)
    }

    pub fn format_cost(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        if self.cost_usd < 0.01 {
            return String::from("-");
        }
        if self.cost_usd < 1.0 {
            format!(
                "${:.2}{}",
                self.cost_usd,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "${:.1}{}",
                self.cost_usd,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        }
    }

    pub fn context_percent(&self) -> f64 {
        if !self.usage_metrics_available {
            return 0.0;
        }
        if self.context_max == 0 || self.context_tokens == 0 {
            return 0.0;
        }
        (self.context_tokens as f64 / self.context_max as f64) * 100.0
    }

    /// Format context as "450k/1M 45%" or a visual bar
    pub fn format_context(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        if self.context_tokens == 0 {
            return String::from("-");
        }
        let pct = self.context_percent();
        format!("{}%", pct as u32)
    }

    /// Visual bar for context usage: ████░░ 62%
    pub fn format_context_bar(&self, width: usize) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        let pct = self.context_percent();
        if pct == 0.0 {
            return String::from("-");
        }
        let filled = ((pct / 100.0) * width as f64).round() as usize;
        let empty = width.saturating_sub(filled);
        format!(
            "{}{} {}%",
            "█".repeat(filled),
            "░".repeat(empty),
            pct as u32
        )
    }

    /// Produce a JSON-serializable value for --json export.
    pub fn to_json_value(&self) -> serde_json::Value {
        let cost_usd = if self.usage_metrics_available {
            serde_json::json!((self.cost_usd * 100.0).round() / 100.0)
        } else {
            serde_json::Value::Null
        };
        let burn_rate = if self.usage_metrics_available {
            serde_json::json!((self.burn_rate_per_hr * 100.0).round() / 100.0)
        } else {
            serde_json::Value::Null
        };
        let context_pct = if self.usage_metrics_available {
            serde_json::json!((self.context_percent() * 100.0).round() / 100.0)
        } else {
            serde_json::Value::Null
        };
        let tokens_in = if self.usage_metrics_available {
            serde_json::json!(self.total_input_tokens)
        } else {
            serde_json::Value::Null
        };
        let tokens_out = if self.usage_metrics_available {
            serde_json::json!(self.total_output_tokens)
        } else {
            serde_json::Value::Null
        };

        serde_json::json!({
            "pid": self.pid,
            "project": self.display_name(),
            "status": self.status.to_string(),
            "telemetry": {
                "state": self.telemetry_status.label(),
                "usage_metrics_available": self.usage_metrics_available,
            },
            "estimate": {
                "verified": !self.cost_estimate_unverified,
                "profile_source": self.model_profile_source,
            },
            "context_pct": context_pct,
            "cost_usd": cost_usd,
            "burn_rate_per_hr": burn_rate,
            "elapsed_secs": self.elapsed.as_secs(),
            "cpu": self.cpu_percent,
            "mem_mb": (self.mem_mb * 100.0).round() / 100.0,
            "tokens_in": tokens_in,
            "tokens_out": tokens_out,
            "subagents": self.subagent_count,
            "active_subagents": self.active_subagent_count,
            "subagent_breakdown": self.subagent_breakdown().into_iter().map(|row| {
                serde_json::json!({
                    "label": row.display_label(),
                    "state": row.state_label(),
                    "count": row.count,
                    "tokens_in": if row.usage_metrics_available {
                        serde_json::json!(row.total_input_tokens())
                    } else {
                        serde_json::Value::Null
                    },
                    "tokens_out": if row.usage_metrics_available {
                        serde_json::json!(row.output_tokens)
                    } else {
                        serde_json::Value::Null
                    },
                    "cost_usd": if row.usage_metrics_available {
                        serde_json::json!((row.cost_usd * 100.0).round() / 100.0)
                    } else {
                        serde_json::Value::Null
                    },
                })
            }).collect::<Vec<_>>(),
            "last_error": self.last_error_message,
            "recent_errors": self.recent_errors.iter().map(|e| {
                serde_json::json!({
                    "tool": e.tool_name,
                    "message": e.message,
                })
            }).collect::<Vec<_>>(),
            "files_modified": self.files_modified,
            "tool_usage": self.tool_usage.iter().map(|(k, v)| {
                (k.clone(), serde_json::json!({"calls": v.calls}))
            }).collect::<serde_json::Map<String, serde_json::Value>>(),
        })
    }

    pub fn format_burn_rate(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        if self.burn_rate_per_hr < 0.01 {
            return String::from("-");
        }
        if self.burn_rate_per_hr < 1.0 {
            format!(
                "${:.2}/h{}",
                self.burn_rate_per_hr,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "${:.1}/h{}",
                self.burn_rate_per_hr,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        }
    }

    pub fn telemetry_label(&self) -> &'static str {
        self.telemetry_status.label()
    }

    pub fn has_usage_metrics(&self) -> bool {
        self.usage_metrics_available
    }
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn subagent_label(path: &Path) -> String {
    let components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();

    if let Some(tasks_idx) = components.iter().position(|component| component == "tasks") {
        let relative = &components[tasks_idx + 1..];
        if !relative.is_empty() {
            let mut label = relative.join("/");
            if let Some(stripped) = label.strip_suffix(".jsonl") {
                label = stripped.to_string();
            }
            return label;
        }
    }

    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("subagent")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session() -> ClaudeSession {
        ClaudeSession::from_raw(RawSession {
            pid: 1,
            session_id: "session-1".into(),
            cwd: "/tmp/project".into(),
            started_at: 0,
        })
    }

    #[test]
    fn subagent_breakdown_groups_completed_and_lists_active_rows() {
        let mut session = make_session();
        let completed = PathBuf::from("/tmp/claude-1/-tmp-project/session-1/tasks/agent-1.jsonl");
        let active =
            PathBuf::from("/tmp/claude-1/-tmp-project/session-1/tasks/nested/agent-2.jsonl");

        session.active_subagent_jsonl_paths = vec![active.clone()];
        session.subagent_rollups.insert(
            completed,
            SubagentRollup {
                input_tokens: 10_000,
                output_tokens: 2_000,
                cost_usd: 0.25,
                usage_metrics_available: true,
                ..SubagentRollup::default()
            },
        );
        session.subagent_rollups.insert(
            active,
            SubagentRollup {
                input_tokens: 40_000,
                output_tokens: 8_000,
                cost_usd: 1.5,
                usage_metrics_available: true,
                ..SubagentRollup::default()
            },
        );

        let rows = session.subagent_breakdown();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].display_label(), "completed");
        assert_eq!(rows[0].state, SubagentState::Completed);
        assert_eq!(rows[0].count, 1);
        assert_eq!(rows[0].format_tokens(), "10.0k/2.0k");
        assert_eq!(rows[1].display_label(), "nested/agent-2");
        assert_eq!(rows[1].state, SubagentState::Active);
        assert_eq!(rows[1].format_cost(), "$1.5");
    }

    #[test]
    fn subagent_breakdown_collapses_multiple_completed_rows() {
        let mut session = make_session();

        for name in ["agent-1.jsonl", "agent-2.jsonl"] {
            let path = PathBuf::from(format!("/tmp/claude-1/-tmp-project/session-1/tasks/{name}"));
            session.subagent_rollups.insert(
                path,
                SubagentRollup {
                    input_tokens: 10_000,
                    output_tokens: 1_000,
                    cost_usd: 0.2,
                    usage_metrics_available: true,
                    ..SubagentRollup::default()
                },
            );
        }

        let rows = session.subagent_breakdown();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].display_label(), "completed (2)");
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].format_tokens(), "20.0k/2.0k");
    }
}

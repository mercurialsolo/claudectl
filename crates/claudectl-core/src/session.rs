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
    /// Read by the send hotkey (#510), which is why this no longer carries an
    /// `allow(dead_code)` — see the #465 audit for why those expire.
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
    /// The subset of `own_cache_write_tokens` written with a 1-hour TTL, which
    /// bills at 2x base input rather than 1.25x.
    pub own_cache_write_1h_tokens: u64,
    /// The message id whose usage was counted most recently.
    ///
    /// Claude Code writes one JSONL line per content block of an assistant
    /// turn — `thinking`, `text`, `tool_use` — and repeats the turn's whole
    /// `usage` on each. Counting every line charged the same tokens two or
    /// three times over. Duplicate lines are always adjacent (verified across
    /// 4814 turns in 25 transcripts), so remembering one id is enough, and it
    /// has to live on the session because parsing is incremental and a turn's
    /// lines can straddle two reads.
    pub last_usage_msg_id: Option<String>,
    pub subagent_input_tokens: u64,
    pub subagent_output_tokens: u64,
    pub subagent_cache_read_tokens: u64,
    pub subagent_cache_write_tokens: u64,
    /// 1-hour-TTL subset of `subagent_cache_write_tokens`.
    pub subagent_cache_write_1h_tokens: u64,
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
    /// EWMA-smoothed burn rate ($/hr) used for budget forecasting. Damps the
    /// tick-to-tick flicker of `burn_rate_per_hr`. See `forecast` (#370).
    pub burn_rate_ewma: f64,
    /// Recent instantaneous burn samples ($/hr), capped at `BURN_SAMPLE_CAP`,
    /// for the p10/p90 ETA band. In-memory only (rebuilt at runtime).
    pub burn_samples: std::collections::VecDeque<f64>,
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
    // ── Cognitive health tracking ────────────────────────────────────
    /// Cumulative tokens at each Edit/Write event (for efficiency trending).
    pub total_tokens_at_edit_count: u64,
    /// Number of Edit/Write events (for averaging tokens-per-edit).
    pub edit_event_count: u32,
    /// Baseline tokens-per-edit, frozen after first 5 edits.
    pub baseline_tokens_per_edit: Option<f64>,
    /// Error count ring buffer: one entry per window (~10s each).
    pub error_counts_per_window: Vec<u32>, // max 10 entries
    /// Accumulator for current error window.
    pub current_window_errors: u32,
    /// Ticks since last window flush.
    pub window_tick_counter: u32,
    /// Baseline error rate (errors per window), frozen after 3 windows.
    pub baseline_error_rate: Option<f64>,
    /// File reads since last edit: path -> read count. Reset when file is edited.
    pub file_reads_since_edit: HashMap<String, u32>,
    /// All-time error count.
    pub total_error_count: u32,
    /// Cached composite decay score (0-100), recomputed each tick.
    pub decay_score: u32,
    /// If set, this session is from a remote worker (not local).
    /// Terminal actions (approve, kill, etc.) are disabled for remote sessions;
    /// switching attaches over ssh when the fields below are known.
    pub worker_origin: Option<String>,
    /// Which coding agent this session belongs to — `claude-code`, `codex`
    /// (#524). Defaults to `claude-code` in every existing constructor, so
    /// nothing that predates the field changes behaviour.
    ///
    /// The type keeps its name. A Codex session fills the same fields and
    /// flows through the same views, and renaming a struct that is public API
    /// across three crates buys nothing a field does not.
    pub agent_family: String,
    /// The tmux pane (`session:window.pane`) this session runs in, on the
    /// machine that owns it. Filled by the relay's collector, which ships it
    /// to peers so they can attach; `None` when the session isn't in tmux.
    pub tmux_target: Option<String>,
    /// Remote sessions only: the login on the owning machine, as that machine
    /// advertised it. `None` from a peer too old to send it.
    pub remote_user: Option<String>,
    /// Remote sessions only: the address this machine reaches the owning peer
    /// at, from the relay connection. `None` when the relay doesn't know it.
    pub remote_host: Option<String>,
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
    /// The 1-hour-TTL subset of `cache_write_tokens`, billed at 2x base input.
    pub cache_write_1h_tokens: u64,
    /// The last turn counted, so a turn written as several JSONL lines is not
    /// charged once per line. Same reason as `ClaudeSession::last_usage_msg_id`.
    pub last_usage_msg_id: Option<String>,
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
            own_cache_write_1h_tokens: 0,
            last_usage_msg_id: None,
            subagent_input_tokens: 0,
            subagent_output_tokens: 0,
            subagent_cache_read_tokens: 0,
            subagent_cache_write_tokens: 0,
            subagent_cache_write_1h_tokens: 0,
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
            burn_rate_ewma: 0.0,
            burn_samples: std::collections::VecDeque::new(),
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
            total_tokens_at_edit_count: 0,
            edit_event_count: 0,
            baseline_tokens_per_edit: None,
            error_counts_per_window: Vec::new(),
            current_window_errors: 0,
            window_tick_counter: 0,
            baseline_error_rate: None,
            file_reads_since_edit: HashMap::new(),
            total_error_count: 0,
            decay_score: 0,
            worker_origin: None,
            // Every existing caller of `from_raw` is a Claude Code session;
            // `from_codex_rollout` overrides this.
            agent_family: crate::agent::vocab::CLAUDE_CODE.to_string(),
            tmux_target: None,
            remote_user: None,
            remote_host: None,
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

        // Flush error window every 5 ticks (~10s at default 2s interval)
        self.window_tick_counter += 1;
        if self.window_tick_counter >= 5 {
            self.error_counts_per_window
                .push(self.current_window_errors);
            if self.error_counts_per_window.len() > 10 {
                self.error_counts_per_window.remove(0);
            }
            // Freeze baseline error rate after 3 windows
            if self.baseline_error_rate.is_none() && self.error_counts_per_window.len() >= 3 {
                let sum: u32 = self.error_counts_per_window.iter().sum();
                self.baseline_error_rate =
                    Some(sum as f64 / self.error_counts_per_window.len() as f64);
            }
            self.current_window_errors = 0;
            self.window_tick_counter = 0;
        }
    }

    /// Fold the current instantaneous `burn_rate_per_hr` into the EWMA and the
    /// rolling sample window. Call once per refresh after burn is recomputed.
    /// `interval_secs` is the refresh cadence (shapes the EWMA half-life).
    pub fn record_burn_sample(&mut self, interval_secs: f64) {
        let sample = self.burn_rate_per_hr;
        let alpha = crate::forecast::alpha_for_half_life(
            crate::forecast::BURN_HALF_LIFE_SECS,
            interval_secs,
        );
        let prev = if self.burn_samples.is_empty() {
            None
        } else {
            Some(self.burn_rate_ewma)
        };
        self.burn_rate_ewma = crate::forecast::ewma(prev, sample, alpha);
        self.burn_samples.push_back(sample);
        while self.burn_samples.len() > crate::forecast::BURN_SAMPLE_CAP {
            self.burn_samples.pop_front();
        }
    }

    /// Forecast time-to-budget for this session given a per-session cap, as a
    /// p10/p90 interval. `None` when the session isn't burning or has no cap.
    pub fn budget_eta(&self, budget_usd: f64) -> crate::forecast::EtaBand {
        let headroom = budget_usd - self.cost_usd;
        let samples: Vec<f64> = self.burn_samples.iter().copied().collect();
        crate::forecast::budget_eta_band(headroom, self.burn_rate_ewma, &samples)
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

    /// Whether this session is from a remote worker (not local).
    pub fn is_remote(&self) -> bool {
        self.worker_origin.is_some()
    }

    /// Whether this is a Claude Code session (#524).
    ///
    /// The question to ask when the answer is about Claude Code's *file
    /// layout* — `~/.claude/projects` transcripts, `/tmp/claude-<uid>`
    /// subagent tasks — rather than about permission to act. A remote Claude
    /// session is still a Claude Code session; `is_actionable` is what says
    /// you cannot touch it.
    pub fn is_claude_code(&self) -> bool {
        self.agent_family == crate::agent::vocab::CLAUDE_CODE
    }

    /// Whether claudectl can *act* on this session — send input, interrupt,
    /// approve, kill (#524).
    ///
    /// False for two different reasons, which is why this is one predicate
    /// rather than scattered checks:
    ///
    /// * A **remote** session's process is on another machine.
    /// * A **non-Claude-Code** session has no process claudectl can address.
    ///   For Codex specifically this is not "not implemented yet": the
    ///   ChatGPT app runs one `codex … app-server` for every session at once,
    ///   so there is no pid that means *this* session, and the pid we do hold
    ///   is 0. Signalling it would reach the caller's own process group.
    ///
    /// Read it as the question "may I touch this?", not "is this local?" —
    /// `is_remote` answers the latter and is still the right check for
    /// display and fleet labelling.
    pub fn is_actionable(&self) -> bool {
        self.unactionable_reason().is_none()
    }

    /// Why this session cannot be acted on, as a line fit for a status bar —
    /// or `None` when it can.
    ///
    /// The two reasons are genuinely different and a caller that collapses
    /// them tells the user something false, so the reason travels with the
    /// verdict rather than being re-derived at each of the gate sites.
    pub fn unactionable_reason(&self) -> Option<&'static str> {
        if self.is_remote() {
            return Some("Remote session — action not available");
        }
        if !self.is_claude_code() {
            // Deliberately not "not supported yet". With the ChatGPT app one
            // `codex … app-server` serves every session, so there is no pid
            // that means this one; see `agent::codex`.
            return Some("Codex session — claudectl has no process to act on");
        }
        None
    }

    /// Whether this session's `cost_usd` means anything (#524).
    ///
    /// Codex is subscription-billed and its transcripts record tokens only,
    /// so a Codex session's cost is *absent*, not zero. Anything summing or
    /// rendering cost should ask this rather than trusting a 0.0 that cannot
    /// be told apart from a session that genuinely cost nothing.
    pub fn has_known_cost(&self) -> bool {
        self.agent_family != crate::agent::vocab::CODEX
    }

    /// Build a session from a parsed Codex rollout transcript (#524).
    ///
    /// `pid` is 0 and `is_actionable()` is false — see that method for why
    /// there is no process to hold. `cost_usd` stays 0 with
    /// `usage_metrics_available` false, so it reads as absent rather than
    /// free.
    pub fn from_codex_rollout(summary: &crate::agent::codex::RolloutSummary) -> Self {
        use crate::agent::codex::Lifecycle;

        let mut session = Self::from_raw(RawSession {
            pid: 0,
            session_id: summary.session_id.clone(),
            cwd: summary.cwd.clone(),
            started_at: summary.started_at_secs.saturating_mul(1000),
        });

        session.agent_family = crate::agent::vocab::CODEX.to_string();
        session.status = match summary.lifecycle {
            Lifecycle::Working => SessionStatus::Processing,
            // `task_complete` ends a turn, not a session: the agent is done
            // responding and waiting for the next prompt.
            Lifecycle::Idle => SessionStatus::WaitingInput,
            Lifecycle::Unknown => SessionStatus::Unknown,
        };

        session.own_input_tokens = summary.input_tokens;
        session.own_output_tokens = summary.output_tokens;
        session.own_cache_read_tokens = summary.cached_input_tokens;
        session.total_input_tokens = summary.input_tokens;
        session.total_output_tokens = summary.output_tokens;
        session.cache_read_tokens = summary.cached_input_tokens;
        // Codex measures its context window against `total_tokens`.
        session.context_tokens = summary.total_tokens;
        session.context_max = summary.context_window.unwrap_or(0);

        if let Some(model) = &summary.model {
            session.model = model.clone();
        }
        session.pending_tool_name = summary.last_tool.clone();
        session.tool_usage = summary
            .last_tool
            .as_ref()
            .map(|t| {
                let mut m = HashMap::new();
                m.insert(
                    t.clone(),
                    ToolStats {
                        calls: summary.tool_calls,
                    },
                );
                m
            })
            .unwrap_or_default();

        // Absent, not free. See `has_known_cost`.
        session.cost_usd = 0.0;
        session.usage_metrics_available = false;
        session.cost_estimate_unverified = true;
        session.model_profile_source = "codex (tokens only, no cost)".to_string();
        session.jsonl_path = Some(summary.path.clone());

        session
    }

    /// Build a ClaudeSession from remote JSON (as received via heartbeat/HTTP).
    pub fn from_remote_json(worker_id: &str, json: &serde_json::Value) -> Option<Self> {
        let pid = json.get("pid")?.as_u64()? as u32;
        let project = json.get("project")?.as_str()?;
        let status_str = json
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        let status = match status_str {
            "Needs Input" => SessionStatus::NeedsInput,
            "Processing" => SessionStatus::Processing,
            "Waiting" => SessionStatus::WaitingInput,
            "Idle" => SessionStatus::Idle,
            "Finished" => SessionStatus::Finished,
            _ => SessionStatus::Unknown,
        };

        let elapsed_secs = json
            .get("elapsed_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut session = Self::from_raw(RawSession {
            pid,
            session_id: format!("remote-{worker_id}-{pid}"),
            cwd: project.to_string(),
            started_at: now_ms.saturating_sub(elapsed_secs * 1000),
        });
        session.status = status;
        session.worker_origin = Some(worker_id.to_string());
        session.project_name = format!("[{worker_id}] {project}");
        // Both end up in an ssh command line, and a peer chooses them. Anything
        // that could read as an option or carry shell syntax is dropped, which
        // leaves the session looking like one from a peer too old to send it.
        let text = |key: &str, extra: &str| {
            json.get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty() && !s.starts_with('-'))
                .filter(|s| {
                    s.chars().all(|c| {
                        c.is_ascii_alphanumeric() || "._-".contains(c) || extra.contains(c)
                    })
                })
                .map(str::to_string)
        };
        session.tmux_target = text("tmux_target", ": ");
        session.remote_user = text("os_user", "");

        // Populate metrics from JSON when available
        if let Some(cost) = json.get("cost_usd").and_then(|v| v.as_f64()) {
            session.cost_usd = cost;
            session.usage_metrics_available = true;
        }
        if let Some(burn) = json.get("burn_rate_per_hr").and_then(|v| v.as_f64()) {
            session.burn_rate_per_hr = burn;
        }
        if let Some(ctx) = json.get("context_pct").and_then(|v| v.as_f64()) {
            // Reverse-engineer context_tokens/context_max from percentage
            session.context_max = 200_000; // reasonable default
            session.context_tokens = ((ctx / 100.0) * session.context_max as f64) as u64;
        }
        if let Some(t_in) = json.get("tokens_in").and_then(|v| v.as_u64()) {
            session.total_input_tokens = t_in;
            session.usage_metrics_available = true;
        }
        if let Some(t_out) = json.get("tokens_out").and_then(|v| v.as_u64()) {
            session.total_output_tokens = t_out;
        }
        if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
            session.model = model.to_string();
        }
        if let Some(subs) = json.get("subagents").and_then(|v| v.as_u64()) {
            session.subagent_count = subs as usize;
        }
        if let Some(decay) = json.get("decay_score").and_then(|v| v.as_u64()) {
            session.decay_score = decay as u32;
        }

        Some(session)
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
            // Carried to peers so a fleet lease authority can renew this
            // session's leases while it's alive (#569).
            "session_id": self.session_id,
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
            "decay_score": if self.usage_metrics_available { serde_json::json!(self.decay_score) } else { serde_json::Value::Null },
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
            "worker_origin": self.worker_origin,
            "tmux_target": self.tmux_target,
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

/// Truncate a string to at most `max_bytes` bytes, landing on a valid
/// UTF-8 character boundary. Returns the original string if already short enough.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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

    // ── #524 agent family and the action gate ──────────────────────────

    fn codex_summary() -> crate::agent::codex::RolloutSummary {
        crate::agent::codex::RolloutSummary {
            session_id: "01a12c5d".into(),
            cwd: "/Users/dev/project".into(),
            started_at_secs: 1_700_000_000,
            last_event_secs: 1_700_000_100,
            input_tokens: 33_434,
            cached_input_tokens: 22_912,
            output_tokens: 215,
            total_tokens: 33_649,
            context_window: Some(272_000),
            model: Some("gpt-6-astra".into()),
            lifecycle: crate::agent::codex::Lifecycle::Idle,
            tool_calls: 3,
            last_tool: Some("Bash".into()),
            path: std::path::PathBuf::from("/Users/dev/.codex/sessions/2026/10/11/r.jsonl"),
        }
    }

    #[test]
    fn a_local_claude_session_is_actionable() {
        let s = make_session();
        assert!(s.is_claude_code());
        assert!(s.is_actionable());
        assert_eq!(s.unactionable_reason(), None);
        assert!(s.has_known_cost());
    }

    /// The safety property this whole phase turns on: nothing may offer to
    /// send input to, interrupt, or kill a Codex session. There is no pid
    /// that means *this* session — with the ChatGPT app one `app-server`
    /// process serves them all — and the pid we hold is 0, which
    /// `kill(0, sig)` aims at the caller's own process group.
    #[test]
    fn a_codex_session_is_never_actionable() {
        let s = ClaudeSession::from_codex_rollout(&codex_summary());
        assert!(!s.is_claude_code());
        assert!(
            !s.is_actionable(),
            "a Codex session must never be actionable — see from_codex_rollout"
        );
        assert_eq!(s.pid, 0, "there is no process to hold");
        let reason = s.unactionable_reason().expect("must carry a reason");
        assert!(
            reason.to_lowercase().contains("codex"),
            "the reason must name Codex rather than claiming the session is remote: {reason}"
        );
    }

    /// A remote session and a Codex session are unactionable for different
    /// reasons, and a caller that collapses them tells the user something
    /// false.
    #[test]
    fn remote_and_codex_give_different_reasons() {
        let mut remote = make_session();
        remote.worker_origin = Some("mini".into());
        let codex = ClaudeSession::from_codex_rollout(&codex_summary());

        assert_ne!(remote.unactionable_reason(), codex.unactionable_reason());
        assert!(
            remote.unactionable_reason().unwrap().contains("Remote"),
            "the remote wording must not regress"
        );
        // A remote session is still a Claude Code session.
        assert!(remote.is_claude_code());
    }

    #[test]
    fn a_codex_session_carries_tokens_and_context_but_no_cost() {
        let s = ClaudeSession::from_codex_rollout(&codex_summary());
        assert_eq!(s.own_input_tokens, 33_434);
        assert_eq!(s.own_output_tokens, 215);
        assert_eq!(s.own_cache_read_tokens, 22_912);
        assert_eq!(
            s.context_tokens, 33_649,
            "Codex measures its window against total_tokens"
        );
        assert_eq!(s.context_max, 272_000);

        assert_eq!(s.cost_usd, 0.0);
        assert!(
            !s.has_known_cost(),
            "Codex is subscription-billed; its cost is absent, not zero — a \
             0.0 that reads as known would understate nothing but would let a \
             fleet total claim completeness it does not have"
        );
        assert!(!s.usage_metrics_available);
    }

    #[test]
    fn a_codex_session_maps_lifecycle_to_status() {
        use crate::agent::codex::Lifecycle;
        let mut summary = codex_summary();

        summary.lifecycle = Lifecycle::Working;
        assert_eq!(
            ClaudeSession::from_codex_rollout(&summary).status,
            SessionStatus::Processing
        );

        // `task_complete` ends a turn, not a session.
        summary.lifecycle = Lifecycle::Idle;
        assert_eq!(
            ClaudeSession::from_codex_rollout(&summary).status,
            SessionStatus::WaitingInput
        );

        summary.lifecycle = Lifecycle::Unknown;
        assert_eq!(
            ClaudeSession::from_codex_rollout(&summary).status,
            SessionStatus::Unknown
        );
    }

    /// The rollout is the transcript. `resolve_jsonl_paths` must not later
    /// point this at a `~/.claude/projects` file.
    #[test]
    fn a_codex_session_keeps_its_rollout_as_its_transcript() {
        let s = ClaudeSession::from_codex_rollout(&codex_summary());
        assert_eq!(
            s.jsonl_path.as_deref(),
            Some(codex_summary().path.as_path())
        );
        assert_eq!(s.project_name, "project");
        assert_eq!(s.model, "gpt-6-astra");
        assert_eq!(s.pending_tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn record_burn_sample_smooths_and_caps_window() {
        let mut s = make_session();
        // A steady burn settles the EWMA near the true rate and fills the window.
        for _ in 0..crate::forecast::BURN_SAMPLE_CAP + 10 {
            s.burn_rate_per_hr = 6.0;
            s.record_burn_sample(2.0);
        }
        assert_eq!(s.burn_samples.len(), crate::forecast::BURN_SAMPLE_CAP);
        assert!(
            (s.burn_rate_ewma - 6.0).abs() < 0.5,
            "ewma: {}",
            s.burn_rate_ewma
        );
    }

    #[test]
    fn budget_eta_forecasts_time_to_cap() {
        let mut s = make_session();
        for _ in 0..20 {
            s.burn_rate_per_hr = 5.0;
            s.record_burn_sample(2.0);
        }
        s.cost_usd = 5.0; // $5 spent of a $15 cap ⇒ $10 headroom at ~$5/hr ≈ 2h
        let band = s.budget_eta(15.0);
        let mid = band.mid_hours.expect("burning, so an ETA exists");
        assert!((mid - 2.0).abs() < 0.4, "eta hours: {mid}");
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

    // ── Cognitive health tracking tests ──────────────────────────────

    #[test]
    fn error_window_flush() {
        let mut s = make_session();
        s.current_window_errors = 3;
        // Call record_activity 5 times to trigger one window flush
        for _ in 0..5 {
            s.record_activity();
        }
        assert_eq!(s.error_counts_per_window.len(), 1);
        assert_eq!(s.error_counts_per_window[0], 3);
        assert_eq!(s.current_window_errors, 0);
        assert_eq!(s.window_tick_counter, 0);
    }

    #[test]
    fn baseline_error_rate_freezes() {
        let mut s = make_session();
        // Simulate 3 windows of errors
        for errors in [2, 3, 4] {
            s.current_window_errors = errors;
            for _ in 0..5 {
                s.record_activity();
            }
        }
        assert_eq!(s.error_counts_per_window.len(), 3);
        let baseline = s.baseline_error_rate.expect("baseline should be set");
        // baseline = (2+3+4)/3 = 3.0
        assert!((baseline - 3.0).abs() < 0.01);

        // Add another window — baseline should NOT change
        s.current_window_errors = 10;
        for _ in 0..5 {
            s.record_activity();
        }
        assert_eq!(s.baseline_error_rate.unwrap(), baseline);
    }

    // ── Remote session tests ────────────────────────────────────────

    #[test]
    fn local_session_is_not_remote() {
        let s = make_session();
        assert!(!s.is_remote());
        assert!(s.worker_origin.is_none());
    }

    #[test]
    fn from_remote_json_parses_basic_fields() {
        let json = serde_json::json!({
            "pid": 42,
            "project": "backend",
            "status": "Processing",
            "cost_usd": 1.23,
            "elapsed_secs": 600,
            "tokens_in": 50000,
            "tokens_out": 10000,
        });
        let session = ClaudeSession::from_remote_json("macbook-02", &json).unwrap();
        assert!(session.is_remote());
        assert_eq!(session.worker_origin.as_deref(), Some("macbook-02"));
        assert_eq!(session.pid, 42);
        assert_eq!(session.project_name, "[macbook-02] backend");
        assert_eq!(session.status, SessionStatus::Processing);
        assert!((session.cost_usd - 1.23).abs() < 0.01);
        assert_eq!(session.total_input_tokens, 50000);
        assert_eq!(session.total_output_tokens, 10000);
        assert!(session.usage_metrics_available);
    }

    #[test]
    fn from_remote_json_handles_all_statuses() {
        for (label, expected) in [
            ("Needs Input", SessionStatus::NeedsInput),
            ("Processing", SessionStatus::Processing),
            ("Waiting", SessionStatus::WaitingInput),
            ("Idle", SessionStatus::Idle),
            ("Finished", SessionStatus::Finished),
            ("SomethingElse", SessionStatus::Unknown),
        ] {
            let json = serde_json::json!({"pid": 1, "project": "p", "status": label});
            let session = ClaudeSession::from_remote_json("w", &json).unwrap();
            assert_eq!(session.status, expected, "status mismatch for {label}");
        }
    }

    #[test]
    fn from_remote_json_returns_none_on_missing_fields() {
        // Missing pid
        let json = serde_json::json!({"project": "x", "status": "Idle"});
        assert!(ClaudeSession::from_remote_json("w", &json).is_none());

        // Missing project
        let json = serde_json::json!({"pid": 1, "status": "Idle"});
        assert!(ClaudeSession::from_remote_json("w", &json).is_none());
    }

    #[test]
    fn remote_session_display_name_shows_worker_prefix() {
        let json = serde_json::json!({"pid": 1, "project": "api-server", "status": "Idle"});
        let session = ClaudeSession::from_remote_json("laptop-01", &json).unwrap();
        assert_eq!(session.display_name(), "[laptop-01] api-server");
    }

    #[test]
    fn from_remote_json_reads_attach_fields() {
        let json = serde_json::json!({
            "pid": 1, "project": "p", "status": "Idle",
            "tmux_target": "claude-staffai:0.0", "os_user": "baradas",
        });
        let session = ClaudeSession::from_remote_json("mini", &json).unwrap();
        assert_eq!(session.tmux_target.as_deref(), Some("claude-staffai:0.0"));
        assert_eq!(session.remote_user.as_deref(), Some("baradas"));
    }

    #[test]
    fn from_remote_json_drops_attach_fields_that_could_be_ssh_options() {
        let json = serde_json::json!({
            "pid": 1, "project": "p", "status": "Idle",
            "tmux_target": "-x", "os_user": "-oProxyCommand=sh",
        });
        let session = ClaudeSession::from_remote_json("mini", &json).unwrap();
        assert!(session.tmux_target.is_none());
        assert!(session.remote_user.is_none());

        let json = serde_json::json!({
            "pid": 1, "project": "p", "status": "Idle", "os_user": "bob;rm",
        });
        let session = ClaudeSession::from_remote_json("mini", &json).unwrap();
        assert!(session.remote_user.is_none());
    }

    #[test]
    fn from_remote_json_from_an_older_peer_has_no_attach_fields() {
        let json = serde_json::json!({"pid": 1, "project": "p", "status": "Idle"});
        let session = ClaudeSession::from_remote_json("mini", &json).unwrap();
        assert!(session.tmux_target.is_none());
        assert!(session.remote_user.is_none());
    }

    #[test]
    fn remote_session_json_includes_worker_origin() {
        let json = serde_json::json!({"pid": 1, "project": "test", "status": "Idle"});
        let session = ClaudeSession::from_remote_json("remote-w", &json).unwrap();
        let output = session.to_json_value();
        assert_eq!(
            output.get("worker_origin").and_then(|v| v.as_str()),
            Some("remote-w")
        );
    }
}

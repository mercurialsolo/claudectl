use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use claudectl_core::discovery;
use claudectl_core::helpers::{
    create_aggregate_session, dirs_home, fire_notification, fire_webhook,
};
use claudectl_core::hooks::{HookEvent, HookRegistry};
use claudectl_core::launch::{self, LaunchRequest};
use claudectl_core::monitor;
use claudectl_core::process;
use claudectl_core::session::{ClaudeSession, SessionStatus};
use claudectl_core::terminals;
use claudectl_core::theme::Theme;

pub const SORT_COLUMNS: &[&str] = &["Status", "Context", "Cost", "$/hr", "Elapsed"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    All,
    NeedsInput,
    Processing,
    WaitingInput,
    Unknown,
    Idle,
    Finished,
}

impl StatusFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::NeedsInput,
            Self::NeedsInput => Self::Processing,
            Self::Processing => Self::WaitingInput,
            Self::WaitingInput => Self::Unknown,
            Self::Unknown => Self::Idle,
            Self::Idle => Self::Finished,
            Self::Finished => Self::All,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "needsinput" | "needs-input" => Some(Self::NeedsInput),
            "processing" => Some(Self::Processing),
            "waiting" | "waitinginput" | "waiting-input" => Some(Self::WaitingInput),
            "unknown" => Some(Self::Unknown),
            "idle" => Some(Self::Idle),
            "finished" => Some(Self::Finished),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::NeedsInput => "Needs Input",
            Self::Processing => "Processing",
            Self::WaitingInput => "Waiting",
            Self::Unknown => "Unknown",
            Self::Idle => "Idle",
            Self::Finished => "Finished",
        }
    }

    fn matches(self, status: SessionStatus) -> bool {
        match self {
            Self::All => true,
            Self::NeedsInput => status == SessionStatus::NeedsInput,
            Self::Processing => status == SessionStatus::Processing,
            Self::WaitingInput => status == SessionStatus::WaitingInput,
            Self::Unknown => status == SessionStatus::Unknown,
            Self::Idle => status == SessionStatus::Idle,
            Self::Finished => status == SessionStatus::Finished,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusFilter {
    All,
    Attention,
    OverBudget,
    HighContext,
    UnknownTelemetry,
    Conflict,
}

impl FocusFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Attention,
            Self::Attention => Self::OverBudget,
            Self::OverBudget => Self::HighContext,
            Self::HighContext => Self::UnknownTelemetry,
            Self::UnknownTelemetry => Self::Conflict,
            Self::Conflict => Self::All,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "attention" => Some(Self::Attention),
            "overbudget" | "over-budget" => Some(Self::OverBudget),
            "highcontext" | "high-context" => Some(Self::HighContext),
            "unknowntelemetry" | "unknown-telemetry" => Some(Self::UnknownTelemetry),
            "conflict" | "conflicts" => Some(Self::Conflict),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Attention => "Attention",
            Self::OverBudget => "Over Budget",
            Self::HighContext => "High Context",
            Self::UnknownTelemetry => "Unknown Telemetry",
            Self::Conflict => "Conflict",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchField {
    Cwd,
    Prompt,
    Resume,
}

impl LaunchField {
    fn next(self) -> Self {
        match self {
            Self::Cwd => Self::Prompt,
            Self::Prompt => Self::Resume,
            Self::Resume => Self::Resume,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Cwd => Self::Cwd,
            Self::Prompt => Self::Cwd,
            Self::Resume => Self::Prompt,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cwd => "cwd",
            Self::Prompt => "prompt",
            Self::Resume => "resume",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchForm {
    pub field: LaunchField,
    pub cwd: String,
    pub prompt: String,
    pub resume: String,
}

impl Default for LaunchForm {
    fn default() -> Self {
        Self {
            field: LaunchField::Cwd,
            cwd: ".".into(),
            prompt: String::new(),
            resume: String::new(),
        }
    }
}

impl LaunchForm {
    pub fn active_buffer(&self) -> &str {
        match self.field {
            LaunchField::Cwd => &self.cwd,
            LaunchField::Prompt => &self.prompt,
            LaunchField::Resume => &self.resume,
        }
    }

    fn active_buffer_mut(&mut self) -> &mut String {
        match self.field {
            LaunchField::Cwd => &mut self.cwd,
            LaunchField::Prompt => &mut self.prompt,
            LaunchField::Resume => &mut self.resume,
        }
    }

    fn advance(&mut self) {
        self.field = self.field.next();
    }

    fn retreat(&mut self) {
        self.field = self.field.prev();
    }

    fn is_last_field(&self) -> bool {
        self.field == LaunchField::Resume
    }

    pub fn status_hint(&self) -> String {
        format!(
            "New session [{}] Enter next, Tab move, Ctrl+Enter launch, Esc cancel",
            self.field.label()
        )
    }

    fn request(&self) -> Result<LaunchRequest, String> {
        launch::prepare(
            &self.cwd,
            Some(self.prompt.as_str()),
            Some(self.resume.as_str()),
        )
    }

    pub fn summary(&self) -> String {
        let cwd = compact_value(&self.cwd, ".");
        let prompt = if self.prompt.trim().is_empty() {
            "skip".to_string()
        } else {
            "set".to_string()
        };
        let resume = compact_value(&self.resume, "skip");
        format!("cwd={cwd} | prompt={prompt} | resume={resume}")
    }
}

fn compact_value(value: &str, empty_label: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return empty_label.to_string();
    }

    const MAX_LEN: usize = 24;
    if trimmed.chars().count() <= MAX_LEN {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(MAX_LEN - 1).collect();
        format!("{prefix}…")
    }
}

pub struct App {
    pub sessions: Vec<ClaudeSession>,
    pub table_state: TableState,
    pub should_quit: bool,
    pub status_msg: String,
    pub pending_kill: Option<u32>,
    pub input_mode: bool,
    pub input_buffer: String,
    pub input_target_pid: Option<u32>,
    // ── Role-bind input mode (#307) ──────────────────────────────────────
    /// When true, keystrokes accumulate in `role_bind_buffer` instead of
    /// triggering normal-mode handlers. Entered via Ctrl+R on the
    /// dashboard; Esc cancels, Enter commits via `Actions::bind_bus_role`.
    pub role_bind_mode: bool,
    pub role_bind_buffer: String,
    pub role_bind_target_pid: Option<u32>,
    pub role_bind_target_cwd: Option<String>,
    pub notify: bool,
    pub prev_statuses: HashMap<u32, SessionStatus>,
    pub show_help: bool,
    pub sort_column: usize,
    pub auto_approve: HashSet<u32>,
    pub pending_auto_approve: Option<u32>,
    /// PID awaiting override reason (1=always safe, 2=one-time, 3=brain is wrong).
    pub pending_override_reason: Option<u32>,
    pub finished_at: HashMap<u32, std::time::Instant>, // When PIDs were first seen as Finished
    pub debug: bool,
    pub debug_timings: DebugTimings,
    pub grouped_view: bool,
    pub detail_panel: bool, // Show expanded detail for selected session
    pub webhook_url: Option<String>,
    pub webhook_filter: Option<Vec<String>>, // Only fire on these status names
    pub launch_mode: bool,                   // Capturing launch wizard fields
    pub launch_form: LaunchForm,
    pub search_mode: bool,
    pub search_buffer: String,
    pub search_query: String,
    pub status_filter: StatusFilter,
    pub focus_filter: FocusFilter,
    pub budget_usd: Option<f64>,     // Per-session budget
    pub kill_on_budget: bool,        // Auto-kill when budget exceeded
    pub budget_warned: HashSet<u32>, // PIDs that have been warned at 80%
    pub budget_killed: HashSet<u32>, // PIDs that have been killed
    pub theme: Theme,
    pub weekly_summary: claudectl_core::history::WeeklySummary,
    pub weekly_summary_tick: u32, // Refresh every N ticks
    pub hooks: HookRegistry,
    pub daily_limit: Option<f64>,
    pub weekly_limit: Option<f64>,
    pub daily_alert_fired: bool, // Prevent repeated alerts per app session
    pub weekly_alert_fired: bool,
    pub context_warn_threshold: u8, // 0-100, fires on_context_high hook
    pub context_warned: HashSet<u32>, // PIDs that have been warned (reset if context drops below threshold)
    pub needs_input_since: HashMap<u32, std::time::Instant>, // When each PID entered NeedsInput
    pub conflict_pids: HashSet<u32>,  // PIDs that share a working directory with another session
    pub conflict_alerted: HashSet<String>, // cwds that have already triggered a conflict alert
    pub file_conflict_pids: HashSet<u32>, // PIDs involved in file-level conflicts
    pub file_conflicts: HashMap<String, Vec<u32>>, // file path → PIDs that modified it
    pub file_conflict_alerted: HashSet<String>, // Files already alerted
    pub file_conflicts_enabled: bool, // Config: detect file-level conflicts
    pub auto_deny_file_conflicts: bool, // Config: auto-deny conflicting writes
    pub demo_mode: bool,
    pub demo_tick: u32,
    pub demo_highlight: Option<crate::demo::DemoHighlightState>,
    pub session_recordings: HashMap<u32, String>, // pid -> output_path for active recordings
    pub rules: Vec<claudectl_core::rules::AutoRule>,
    pub auto_actions_fired: HashMap<u32, std::time::Instant>, // Debounce: pid -> last action time
    pub last_rule_action: Option<String>,                     // Last auto-action status for display
    pub health_thresholds: claudectl_core::health::HealthThresholds,
    pub brain_config: Option<claudectl_core::config::BrainConfig>,
    /// Stateful brain driver, swapped in by `main.rs` when the brain is
    /// configured. Held as `Box<dyn BrainDriver>` (not `Arc`) because every
    /// method needs `&mut`. `None` when the brain is off.
    pub brain_driver: Option<Box<dyn claudectl_core::runtime::BrainDriver>>,
    pub idle_config: claudectl_core::config::IdleConfig,
    pub last_user_interaction: std::time::Instant,
    pub idle_mode_active: bool,
    pub idle_tasks_launched: Vec<String>,
    pub idle_report: Vec<String>,
    // Coordination layer (feature-gated)
    #[cfg(feature = "coord")]
    pub coord_leases: Vec<claudectl_core::runtime::LeaseSummary>,
    #[cfg(feature = "coord")]
    pub coord_handoffs: Vec<claudectl_core::runtime::HandoffSummary>,
    #[cfg(feature = "coord")]
    pub coord_lease_sessions: HashSet<String>,
    #[cfg(feature = "coord")]
    pub coord_handoff_sessions: HashSet<String>,
    #[cfg(feature = "coord")]
    pub coord_interrupt_targets: HashSet<String>,
    #[cfg(feature = "coord")]
    pub coord_pending_interrupts: Vec<claudectl_core::runtime::InterruptSummary>,
    #[cfg(feature = "coord")]
    pub coord_tick: u32,
    // Relay peers panel (feature-gated)
    #[cfg(feature = "relay")]
    pub show_peers_panel: bool,
    // relay_peers is populated when relay serve is active and rendered by
    // ui::peers::render_peers_panel when show_peers_panel is true. Currently
    // a stub — rendering integration is wired when the relay serve loop runs
    // inside the TUI (not yet connected to the TUI render loop).
    #[cfg(feature = "relay")]
    #[allow(dead_code)]
    pub relay_peers: Vec<crate::ui::peers::PeerDisplayInfo>,
    /// Remote sessions received from connected worker peers (relay heartbeats).
    #[cfg(feature = "relay")]
    pub remote_sessions: Vec<claudectl_core::session::ClaudeSession>,

    // ── Skills & Hive overlay state ────────────────────────────────────────
    /// Whether the skills/hive overlay is open.
    pub show_skills: bool,
    /// Which tab is currently active inside the overlay.
    pub skills_tab: SkillsTab,
    /// Currently selected index into `skills`.
    pub skills_selected: usize,
    /// Discovered skills (refreshed when the overlay opens or `r` is pressed).
    pub skills: Vec<claudectl_core::skills::DiscoveredSkill>,
    /// Semantic keys (`skill:<name>`) for skills already present in the hive store.
    pub shared_skill_keys: std::collections::HashSet<String>,
    /// Transient status message shown in the overlay footer.
    pub skills_status_msg: Option<String>,
    /// True when a `claudectl relay serve` subprocess has been started from the TUI.
    pub hive_listener_running: bool,
    /// Local peer identity, populated when the Hive tab is opened.
    pub hive_identity: Option<String>,
    /// Known peers from the local relay state (peer id, optional last address).
    pub hive_known_peers: Vec<(String, Option<String>)>,
    /// Last invite generated from the TUI (held in memory only).
    pub hive_last_invite: Option<HiveInvite>,
    /// When true, the overlay captures text input for a join code.
    pub hive_join_input_mode: bool,
    /// Buffer for the join input.
    pub hive_join_buffer: String,

    // ── Brain review overlay state ─────────────────────────────────────────
    /// Whether the brain review/scorecard overlay is open.
    pub show_brain: bool,
    /// Which tab is currently active inside the brain overlay.
    pub brain_tab: BrainTab,
    /// Currently selected index into `brain_queue`.
    pub brain_review_selected: usize,
    /// Prioritized review candidates (refreshed when the overlay opens or `r` is pressed).
    pub brain_queue: Vec<claudectl_core::runtime::ReviewItemSummary>,
    /// All decision records loaded for the scorecard view.
    pub brain_decisions_cache: Vec<claudectl_core::runtime::DecisionSummary>,
    /// Transient status message shown in the overlay footer.
    pub brain_status_msg: Option<String>,
    /// When true, the overlay captures text input for a canonical-note.
    pub brain_note_input_mode: bool,
    /// Buffer for the in-progress note.
    pub brain_note_buffer: String,

    /// UI ↔ runtime contract (epic #279, issue #275). `App::new` starts with
    /// an in-memory `MockRuntime`; `main` swaps in the live runtime at
    /// startup. Call sites prefer `self.runtime.{view,actions,...}.method()`
    /// over `crate::brain::*` / `crate::coord::*` so that future TUI
    /// extraction is a mechanical file move.
    pub runtime: claudectl_core::runtime::Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillsTab {
    Skills,
    Hive,
}

impl SkillsTab {
    pub fn toggle(self) -> Self {
        match self {
            Self::Skills => Self::Hive,
            Self::Hive => Self::Skills,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrainTab {
    Scorecard,
    Review,
}

impl BrainTab {
    pub fn toggle(self) -> Self {
        match self {
            Self::Scorecard => Self::Review,
            Self::Review => Self::Scorecard,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HiveInvite {
    pub relay_code: String,
    pub invite_link: String,
    pub word_phrase: String,
}

#[derive(Default, Clone)]
pub struct DebugTimings {
    pub scan_ms: f64,
    pub ps_ms: f64,
    pub jsonl_ms: f64,
    pub total_ms: f64,
    // Rolling averages (last 10 ticks)
    history: Vec<(f64, f64, f64, f64)>,
}

impl DebugTimings {
    pub fn record(&mut self, scan: f64, ps: f64, jsonl: f64, total: f64) {
        self.scan_ms = scan;
        self.ps_ms = ps;
        self.jsonl_ms = jsonl;
        self.total_ms = total;
        self.history.push((scan, ps, jsonl, total));
        if self.history.len() > 10 {
            self.history.remove(0);
        }
    }

    pub fn avg_total_ms(&self) -> f64 {
        if self.history.is_empty() {
            return 0.0;
        }
        self.history.iter().map(|h| h.3).sum::<f64>() / self.history.len() as f64
    }

    pub fn format(&self) -> String {
        format!(
            "tick: {:.1}ms (avg {:.1}ms) | scan: {:.1}ms | ps: {:.1}ms | jsonl: {:.1}ms",
            self.total_ms,
            self.avg_total_ms(),
            self.scan_ms,
            self.ps_ms,
            self.jsonl_ms,
        )
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// Project a live `ClaudeSession` to the core `SessionSnapshot` DTO the
/// runtime traits accept. Used by `BrainDriver` call sites that have the
/// live values in memory already.
fn snapshot_from(session: &ClaudeSession) -> claudectl_core::runtime::SessionSnapshot {
    claudectl_core::runtime::SessionSnapshot {
        session_id: session.session_id.clone(),
        pid: session.pid,
        cwd: session.cwd.clone(),
        project_name: session.project_name.clone(),
        status: session.status.to_string(),
        cost_usd: session.cost_usd,
        context_tokens: session.context_tokens,
        context_max: session.context_max,
        last_message_ts: session.last_message_ts,
    }
}

/// Build a runtime `ObservationInput` from the live session + an observed-
/// action label. Centralizes the projection so call sites don't repeat the
/// field plumbing (cf. the 5 sites that used to call
/// `brain::decisions::log_observation` directly).
fn observation_from(
    session: &ClaudeSession,
    action: &str,
) -> claudectl_core::runtime::ObservationInput {
    claudectl_core::runtime::ObservationInput {
        session_pid: session.pid,
        project: session.display_name().to_string(),
        tool: session.pending_tool_name.clone(),
        command: session.pending_tool_input.clone(),
        observed_action: action.to_string(),
    }
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            sessions: Vec::new(),
            table_state: TableState::default(),
            should_quit: false,
            status_msg: String::new(),
            pending_kill: None,
            input_mode: false,
            role_bind_mode: false,
            role_bind_buffer: String::new(),
            role_bind_target_pid: None,
            role_bind_target_cwd: None,
            input_buffer: String::new(),
            input_target_pid: None,
            notify: false,
            prev_statuses: HashMap::new(),
            show_help: false,
            sort_column: 0,
            auto_approve: HashSet::new(),
            pending_auto_approve: None,
            pending_override_reason: None,
            finished_at: HashMap::new(),
            debug: false,
            debug_timings: DebugTimings::default(),
            grouped_view: false,
            detail_panel: false,
            webhook_url: None,
            webhook_filter: None,
            launch_mode: false,
            launch_form: LaunchForm::default(),
            search_mode: false,
            search_buffer: String::new(),
            search_query: String::new(),
            status_filter: StatusFilter::All,
            focus_filter: FocusFilter::All,
            budget_usd: None,
            kill_on_budget: false,
            budget_warned: HashSet::new(),
            budget_killed: HashSet::new(),
            theme: Theme::from_mode(claudectl_core::theme::ThemeMode::Dark),
            weekly_summary: claudectl_core::history::weekly_summary(),
            weekly_summary_tick: 0,
            hooks: HookRegistry::new(),
            daily_limit: None,
            weekly_limit: None,
            daily_alert_fired: false,
            weekly_alert_fired: false,
            context_warn_threshold: 75,
            context_warned: HashSet::new(),
            needs_input_since: HashMap::new(),
            conflict_pids: HashSet::new(),
            conflict_alerted: HashSet::new(),
            file_conflict_pids: HashSet::new(),
            file_conflicts: HashMap::new(),
            file_conflict_alerted: HashSet::new(),
            file_conflicts_enabled: true,
            auto_deny_file_conflicts: false,
            demo_mode: false,
            demo_tick: 0,
            demo_highlight: None,
            session_recordings: HashMap::new(),
            rules: Vec::new(),
            auto_actions_fired: HashMap::new(),
            last_rule_action: None,
            health_thresholds: claudectl_core::health::HealthThresholds::default(),
            brain_config: None,
            brain_driver: None,
            runtime: claudectl_core::runtime::MockRuntime::default().into_runtime(),
            idle_config: claudectl_core::config::IdleConfig::default(),
            last_user_interaction: std::time::Instant::now(),
            idle_mode_active: false,
            idle_tasks_launched: Vec::new(),
            idle_report: Vec::new(),
            #[cfg(feature = "coord")]
            coord_leases: Vec::new(),
            #[cfg(feature = "coord")]
            coord_handoffs: Vec::new(),
            #[cfg(feature = "coord")]
            coord_lease_sessions: HashSet::new(),
            #[cfg(feature = "coord")]
            coord_handoff_sessions: HashSet::new(),
            #[cfg(feature = "coord")]
            coord_interrupt_targets: HashSet::new(),
            #[cfg(feature = "coord")]
            coord_pending_interrupts: Vec::new(),
            #[cfg(feature = "coord")]
            coord_tick: 0,
            #[cfg(feature = "relay")]
            show_peers_panel: false,
            #[cfg(feature = "relay")]
            relay_peers: Vec::new(),
            #[cfg(feature = "relay")]
            remote_sessions: Vec::new(),
            show_skills: false,
            skills_tab: SkillsTab::Skills,
            skills_selected: 0,
            skills: Vec::new(),
            shared_skill_keys: std::collections::HashSet::new(),
            skills_status_msg: None,
            hive_listener_running: false,
            hive_identity: None,
            hive_known_peers: Vec::new(),
            hive_last_invite: None,
            hive_join_input_mode: false,
            hive_join_buffer: String::new(),
            show_brain: false,
            brain_tab: BrainTab::Scorecard,
            brain_review_selected: 0,
            brain_queue: Vec::new(),
            brain_decisions_cache: Vec::new(),
            brain_status_msg: None,
            brain_note_input_mode: false,
            brain_note_buffer: String::new(),
        };
        #[cfg(feature = "coord")]
        app.coord_refresh();
        app.refresh();
        if app.visible_session_count() > 0 {
            app.table_state.select(Some(0));
        }
        app
    }

    pub fn refresh(&mut self) {
        let tick_start = std::time::Instant::now();

        if self.demo_mode {
            self.refresh_demo();
            if self.debug {
                let total_elapsed = tick_start.elapsed();
                self.debug_timings
                    .record(0.0, 0.0, 0.0, total_elapsed.as_secs_f64() * 1000.0);
            }
            return;
        }

        // Discover which PIDs have session files
        let scan_start = std::time::Instant::now();
        let discovered = discovery::scan_sessions();
        let scan_elapsed = scan_start.elapsed();

        // Build a map of existing sessions by PID for state preservation
        let mut existing: HashMap<u32, ClaudeSession> =
            self.sessions.drain(..).map(|s| (s.pid, s)).collect();

        // Merge: reuse existing session state (jsonl_offset, tokens, cost, cpu_history)
        // or create new from discovered
        let mut new_pids: Vec<u32> = Vec::new();
        let mut sessions: Vec<ClaudeSession> = discovered
            .into_iter()
            .map(|new| {
                if let Some(mut prev) = existing.remove(&new.pid) {
                    // Preserve accumulated state, update ephemeral fields
                    prev.elapsed = new.elapsed;
                    prev.started_at = new.started_at;
                    // cwd/project_name/session_id don't change
                    prev
                } else {
                    // Brand new session
                    new_pids.push(new.pid);
                    new
                }
            })
            .collect();

        // Enrich with ps data (CPU, MEM, TTY, command args) + filter dead PIDs
        let ps_start = std::time::Instant::now();
        process::fetch_and_enrich(&mut sessions);
        let ps_elapsed = ps_start.elapsed();

        // Resolve JSONL paths (only for sessions that don't have one yet)
        for session in &mut sessions {
            if session.jsonl_path.is_none() {
                discovery::resolve_jsonl_paths(std::slice::from_mut(session));
            }
        }

        // Scan for subagents
        discovery::scan_subagents(&mut sessions);

        // Resolve git worktree identity (for conflict detection, runs once per session)
        discovery::resolve_worktree_ids(&mut sessions);

        // Snapshot previous cost for burn rate BEFORE reading new JSONL data
        for session in &mut sessions {
            session.prev_cost_usd = session.cost_usd;
        }

        // Read JSONL incrementally (only new bytes since last offset)
        let jsonl_start = std::time::Instant::now();
        for session in &mut sessions {
            monitor::update_tokens(session);
        }
        let jsonl_elapsed = jsonl_start.elapsed();

        // Compute burn rate from cost delta (skip first tick where prev_cost is 0)
        for session in &mut sessions {
            if session.prev_cost_usd > 0.001 {
                let delta = session.cost_usd - session.prev_cost_usd;
                if delta > 0.001 {
                    session.burn_rate_per_hr = delta * 1800.0;
                } else {
                    // Decay burn rate toward zero when no new cost
                    session.burn_rate_per_hr *= 0.5;
                    if session.burn_rate_per_hr < 0.01 {
                        session.burn_rate_per_hr = 0.0;
                    }
                }
            }
        }

        // Budget enforcement
        if let Some(budget) = self.budget_usd {
            for session in &sessions {
                let pct = session.cost_usd / budget * 100.0;

                // Warn at 80%
                if (80.0..100.0).contains(&pct) && !self.budget_warned.contains(&session.pid) {
                    self.budget_warned.insert(session.pid);
                    self.status_msg = format!(
                        "BUDGET WARNING: {} at {:.0}% (${:.2}/${:.2})",
                        session.display_name(),
                        pct,
                        session.cost_usd,
                        budget
                    );
                    fire_notification(&format!("{} budget {:.0}%", session.display_name(), pct));
                    self.hooks.fire(HookEvent::BudgetWarning, session);
                }

                // Kill at 100%
                if pct >= 100.0 && !self.budget_killed.contains(&session.pid) {
                    self.budget_killed.insert(session.pid);
                    if self.kill_on_budget {
                        let _ = self.runtime.actions.terminate_session(session.pid);
                        self.status_msg = format!(
                            "BUDGET EXCEEDED: Killed {} (${:.2}/${:.2})",
                            session.display_name(),
                            session.cost_usd,
                            budget
                        );
                    } else {
                        self.status_msg = format!(
                            "BUDGET EXCEEDED: {} at ${:.2}/{:.2} — use --kill-on-budget to auto-kill",
                            session.display_name(),
                            session.cost_usd,
                            budget
                        );
                    }
                    fire_notification(&format!("{} exceeded budget!", session.display_name()));
                    self.hooks.fire(HookEvent::BudgetExceeded, session);
                }
            }
        }

        // Context threshold warnings
        if self.context_warn_threshold > 0 {
            let threshold = self.context_warn_threshold as f64;
            for session in &sessions {
                let pct = session.context_percent();
                if pct >= threshold && !self.context_warned.contains(&session.pid) {
                    self.context_warned.insert(session.pid);
                    self.status_msg = format!(
                        "CONTEXT HIGH: {} at {:.0}% of context window",
                        session.display_name(),
                        pct
                    );
                    fire_notification(&format!(
                        "{} context at {:.0}%",
                        session.display_name(),
                        pct
                    ));
                    self.hooks.fire(HookEvent::ContextHigh, session);
                } else if pct < threshold && self.context_warned.contains(&session.pid) {
                    // Reset warning if context dropped (e.g., after /compact)
                    self.context_warned.remove(&session.pid);
                }
            }
        }

        // Record activity for sparkline and cache decay score
        for session in &mut sessions {
            session.record_activity();
            session.decay_score =
                claudectl_core::health::compute_decay_score(session, &self.health_thresholds);
        }

        // Track when sessions first appear as Finished, remove after 30s
        let now = std::time::Instant::now();
        for session in &sessions {
            if session.status == SessionStatus::Finished
                && !self.finished_at.contains_key(&session.pid)
            {
                self.finished_at.insert(session.pid, now);
                // Record to history on first Finished detection
                claudectl_core::history::record_session(session);
            }
        }
        sessions.retain(|s| {
            if s.status == SessionStatus::Finished {
                if let Some(&t) = self.finished_at.get(&s.pid) {
                    return now.duration_since(t).as_secs() < 30;
                }
            }
            true
        });
        // Clean up old finished_at entries + their session files
        let expired: Vec<u32> = self
            .finished_at
            .iter()
            .filter(|(_, t)| now.duration_since(**t).as_secs() >= 60)
            .map(|(pid, _)| *pid)
            .collect();
        for pid in &expired {
            let session_file = dirs_home()
                .join(".claude/sessions")
                .join(format!("{pid}.json"));
            let _ = std::fs::remove_file(session_file);
        }
        self.finished_at
            .retain(|_, t| now.duration_since(*t).as_secs() < 60);

        // Sort
        self.apply_sort(&mut sessions);

        // Notifications and webhooks: check for status transitions
        for session in &sessions {
            let prev = self.prev_statuses.get(&session.pid).copied();
            let changed = prev.is_some() && prev != Some(session.status);

            if !changed {
                continue;
            }

            claudectl_core::logger::log(
                "DEBUG",
                &format!(
                    "session {}: status {} -> {}",
                    session.display_name(),
                    prev.unwrap(),
                    session.status
                ),
            );

            // Desktop notification on NeedsInput
            if self.notify && session.status == SessionStatus::NeedsInput {
                fire_notification(&session.project_name);
            }

            // Webhook on status change
            if let Some(ref url) = self.webhook_url {
                let new_status = session.status.to_string();
                let should_fire = match &self.webhook_filter {
                    Some(filter) => filter.iter().any(|f| f.eq_ignore_ascii_case(&new_status)),
                    None => true,
                };
                if should_fire {
                    claudectl_core::logger::log(
                        "DEBUG",
                        &format!(
                            "webhook fired for {} -> {}",
                            session.display_name(),
                            new_status
                        ),
                    );
                    fire_webhook(
                        url,
                        session,
                        prev.map(|p| p.to_string()).unwrap_or_default(),
                    );
                }
            }

            // Event hooks
            self.hooks.fire_with_status(
                HookEvent::StatusChange,
                session,
                &prev.unwrap().to_string(),
                &session.status.to_string(),
            );

            match session.status {
                SessionStatus::NeedsInput => {
                    self.hooks.fire(HookEvent::NeedsInput, session);
                }
                SessionStatus::Finished => {
                    self.hooks.fire(HookEvent::Finished, session);
                }
                SessionStatus::Idle => {
                    self.hooks.fire(HookEvent::Idle, session);
                }
                _ => {}
            }
        }

        // Fire hooks for newly discovered sessions
        for session in sessions.iter().filter(|s| new_pids.contains(&s.pid)) {
            self.hooks.fire(HookEvent::SessionStart, session);
        }

        // Track NeedsInput wait times
        let now_instant = std::time::Instant::now();
        for session in &sessions {
            if session.status == SessionStatus::NeedsInput {
                // Record when it first entered NeedsInput
                self.needs_input_since
                    .entry(session.pid)
                    .or_insert(now_instant);
            } else {
                // Clear if no longer NeedsInput
                self.needs_input_since.remove(&session.pid);
            }
        }
        // Clean up entries for sessions that no longer exist
        let active_pids: HashSet<u32> = sessions.iter().map(|s| s.pid).collect();
        self.needs_input_since
            .retain(|pid, _| active_pids.contains(pid));

        // Conflict detection: find sessions sharing the same git worktree
        // Uses worktree_id (git show-toplevel) so different worktrees don't false-positive
        self.conflict_pids.clear();
        let mut wt_sessions: HashMap<&str, Vec<u32>> = HashMap::new();
        for session in &sessions {
            if session.status != SessionStatus::Finished {
                let key = session.worktree_id.as_deref().unwrap_or(&session.cwd);
                wt_sessions.entry(key).or_default().push(session.pid);
            }
        }
        for (wt, pids) in &wt_sessions {
            if pids.len() >= 2 {
                for &pid in pids {
                    self.conflict_pids.insert(pid);
                }
                // Fire hook once per worktree conflict (not on every tick)
                if !self.conflict_alerted.contains(*wt) {
                    self.conflict_alerted.insert(wt.to_string());
                    let project = sessions
                        .iter()
                        .find(|s| s.pid == pids[0])
                        .map(|s| s.display_name())
                        .unwrap_or("unknown");
                    self.status_msg =
                        format!("CONFLICT: {} sessions sharing {}", pids.len(), project);
                    fire_notification(&format!("{} sessions in {}", pids.len(), project));
                    if let Some(session) = sessions.iter().find(|s| s.pid == pids[0]) {
                        self.hooks.fire(HookEvent::ConflictDetected, session);
                    }
                }
            }
        }
        // Clear alerts for worktrees that no longer have conflicts
        self.conflict_alerted.retain(|wt| {
            wt_sessions
                .get(wt.as_str())
                .map(|pids| pids.len() >= 2)
                .unwrap_or(false)
        });

        // File-level conflict detection: find files edited by multiple sessions
        self.file_conflict_pids.clear();
        self.file_conflicts.clear();
        // Reset has_file_conflict on all sessions
        for session in &mut sessions {
            session.has_file_conflict = false;
        }

        if self.file_conflicts_enabled {
            // Build file → PIDs map from files_modified across active sessions
            let mut file_pids: HashMap<String, Vec<u32>> = HashMap::new();
            for session in &sessions {
                if session.status == SessionStatus::Finished {
                    continue;
                }
                for file in session.files_modified.keys() {
                    file_pids.entry(file.clone()).or_default().push(session.pid);
                }
                // Also consider pending file edits (predictive conflict)
                if let Some(ref pending) = session.pending_file_path {
                    file_pids
                        .entry(pending.clone())
                        .or_default()
                        .push(session.pid);
                }
            }

            // Deduplicate PIDs per file (a session may appear twice if it both modified and is pending)
            for pids in file_pids.values_mut() {
                pids.sort_unstable();
                pids.dedup();
            }

            // Record conflicts where 2+ sessions touch the same file
            for (file, pids) in &file_pids {
                if pids.len() >= 2 {
                    for &pid in pids {
                        self.file_conflict_pids.insert(pid);
                    }
                    self.file_conflicts.insert(file.clone(), pids.clone());

                    // Mark sessions with pending file conflicts
                    for session in &mut sessions {
                        if let Some(ref pending) = session.pending_file_path {
                            if pending == file && pids.contains(&session.pid) {
                                session.has_file_conflict = true;
                            }
                        }
                    }

                    // Fire alert once per conflicting file
                    if !self.file_conflict_alerted.contains(file) {
                        self.file_conflict_alerted.insert(file.clone());
                        let names: Vec<&str> = pids
                            .iter()
                            .filter_map(|pid| {
                                sessions
                                    .iter()
                                    .find(|s| s.pid == *pid)
                                    .map(|s| s.display_name())
                            })
                            .collect();
                        let short = file.rsplit('/').next().unwrap_or(file);
                        self.status_msg =
                            format!("FILE CONFLICT: {} edited by {}", short, names.join(", "));
                        fire_notification(&format!("File conflict: {short}"));
                        if let Some(session) = sessions.iter().find(|s| s.pid == pids[0]) {
                            self.hooks.fire(HookEvent::ConflictDetected, session);
                        }
                    }
                }
            }

            // Clear alerts for files no longer in conflict
            self.file_conflict_alerted
                .retain(|f| self.file_conflicts.contains_key(f));
        }

        // Update prev_statuses
        self.prev_statuses = sessions.iter().map(|s| (s.pid, s.status)).collect();

        self.sessions = sessions;

        // Append remote sessions from relay peers (if relay feature active)
        #[cfg(feature = "relay")]
        {
            for remote in &self.remote_sessions {
                self.sessions.push(remote.clone());
            }
        }

        self.normalize_selection();

        // Record debug timings
        if self.debug {
            let total_elapsed = tick_start.elapsed();
            self.debug_timings.record(
                scan_elapsed.as_secs_f64() * 1000.0,
                ps_elapsed.as_secs_f64() * 1000.0,
                jsonl_elapsed.as_secs_f64() * 1000.0,
                total_elapsed.as_secs_f64() * 1000.0,
            );
        }
    }

    fn apply_sort(&self, sessions: &mut [ClaudeSession]) {
        match self.sort_column {
            0 => sessions.sort_by(|a, b| {
                a.status.sort_key().cmp(&b.status.sort_key()).then_with(|| {
                    // Within NeedsInput, sort by longest waiting first
                    if a.status == SessionStatus::NeedsInput {
                        let a_wait = self.wait_duration(a.pid).unwrap_or_default();
                        let b_wait = self.wait_duration(b.pid).unwrap_or_default();
                        b_wait.cmp(&a_wait)
                    } else {
                        b.elapsed.cmp(&a.elapsed)
                    }
                })
            }),
            1 => sessions.sort_by(|a, b| {
                b.context_percent()
                    .partial_cmp(&a.context_percent())
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            2 => sessions.sort_by(|a, b| {
                b.cost_usd
                    .partial_cmp(&a.cost_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            3 => sessions.sort_by(|a, b| {
                b.burn_rate_per_hr
                    .partial_cmp(&a.burn_rate_per_hr)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            4 => sessions.sort_by_key(|s| std::cmp::Reverse(s.elapsed)),
            _ => {}
        }
    }

    pub fn cycle_sort(&mut self) {
        self.sort_column = (self.sort_column + 1) % SORT_COLUMNS.len();
        self.status_msg = format!("Sort: {}", SORT_COLUMNS[self.sort_column]);
        let mut sessions = std::mem::take(&mut self.sessions);
        self.apply_sort(&mut sessions);
        self.sessions = sessions;
    }

    fn refresh_demo(&mut self) {
        self.demo_tick += 1;
        let mut sessions = crate::demo::generate_sessions(self.demo_tick);

        // When the Skills & Hive view is open during a demo, scripted
        // navigation: cycle selection on the Skills tab, then flip to Hive
        // around tick 6, then back to Skills around tick 12.
        if self.show_skills {
            let phase = self.demo_tick % 14;
            match phase {
                1..=5 => {
                    self.skills_tab = SkillsTab::Skills;
                    if !self.skills.is_empty() {
                        self.skills_selected =
                            ((phase as usize - 1) % self.skills.len()).min(self.skills.len() - 1);
                    }
                }
                6 => {
                    self.skills_tab = SkillsTab::Hive;
                    self.skills_status_msg = Some("Hive: 2 peers connected".into());
                }
                7..=11 => {
                    self.skills_tab = SkillsTab::Hive;
                }
                _ => {
                    self.skills_tab = SkillsTab::Skills;
                    self.skills_status_msg = None;
                }
            }
        }

        // Track NeedsInput wait times (same as real mode)
        let now_instant = std::time::Instant::now();
        for session in &sessions {
            if session.status == SessionStatus::NeedsInput {
                self.needs_input_since
                    .entry(session.pid)
                    .or_insert(now_instant);
            } else {
                self.needs_input_since.remove(&session.pid);
            }
        }

        // Conflict detection using worktree_id
        self.conflict_pids.clear();
        let mut wt_sessions: HashMap<&str, Vec<u32>> = HashMap::new();
        for session in &sessions {
            if session.status != SessionStatus::Finished {
                let key = session.worktree_id.as_deref().unwrap_or(&session.cwd);
                wt_sessions.entry(key).or_default().push(session.pid);
            }
        }
        for pids in wt_sessions.values() {
            if pids.len() >= 2 {
                for &pid in pids {
                    self.conflict_pids.insert(pid);
                }
            }
        }

        // Scripted demo events: rules, brain, routing, health alerts
        if let Some(event) = crate::demo::demo_event(self.demo_tick) {
            self.status_msg = event.message.clone();
            match event.kind {
                crate::demo::EventKind::RuleAction => {
                    self.last_rule_action = Some(event.message);
                }
                crate::demo::EventKind::BrainSuggestion | crate::demo::EventKind::BrainOverride => {
                    // Show brain activity via status message
                }
                crate::demo::EventKind::Route | crate::demo::EventKind::HealthAlert => {}
                crate::demo::EventKind::HiveSync | crate::demo::EventKind::HiveInfluence => {}
            }
        }

        // Update demo peers panel and remote sessions
        #[cfg(feature = "relay")]
        {
            self.relay_peers = crate::demo::demo_peers(self.demo_tick);
            // Auto-show peers panel on first hive sync event
            if self.demo_tick % 32 == 14 && !self.show_peers_panel {
                self.show_peers_panel = true;
            }
            // Demo remote sessions from connected peers
            self.remote_sessions.clear();
            if self.demo_tick % 32 >= 14 {
                let remote_json = serde_json::json!({
                    "pid": 99001, "project": "backend",
                    "status": "Processing", "cost_usd": 1.4,
                    "elapsed_secs": 320, "context_pct": 42.0,
                });
                if let Some(s) = ClaudeSession::from_remote_json("ci-runner-9d1e", &remote_json) {
                    self.remote_sessions.push(s);
                }
            }
            if self.demo_tick % 32 >= 28 {
                let remote_json = serde_json::json!({
                    "pid": 99002, "project": "frontend",
                    "status": "Needs Input", "cost_usd": 0.32,
                    "elapsed_secs": 150,
                });
                if let Some(s) = ClaudeSession::from_remote_json("alice-mbp-f3a1", &remote_json) {
                    self.remote_sessions.push(s);
                }
            }
        }

        // Inject fake brain pending suggestions so the status bar shows brain activity.
        // Demo mode flows through the BrainDriver trait's set_pending escape hatch
        // rather than mutating an engine field directly — same path the real brain
        // would take.
        if let Some(ref mut driver) = self.brain_driver {
            driver.clear_pending();
            let phase = self.demo_tick % 32;
            if (9..=12).contains(&phase) {
                if let Some(s) = sessions
                    .iter()
                    .find(|s| s.status == SessionStatus::NeedsInput)
                {
                    driver.set_pending(claudectl_core::runtime::PendingSuggestion {
                        pid: s.pid,
                        action: "approve".into(),
                        message: s.pending_tool_input.clone(),
                        reasoning: "Safe build command, no side effects".into(),
                        confidence: 0.92,
                        suggested_at: 0,
                    });
                }
            }
            if (14..=16).contains(&phase) {
                if let Some(s) = sessions
                    .iter()
                    .find(|s| s.status == SessionStatus::NeedsInput)
                {
                    driver.set_pending(claudectl_core::runtime::PendingSuggestion {
                        pid: s.pid,
                        action: "deny".into(),
                        message: s.pending_tool_input.clone(),
                        reasoning: "Destructive operation, needs manual review".into(),
                        confidence: 0.87,
                        suggested_at: 0,
                    });
                }
            }
        }

        // ── Demo highlight reel support ────────────────────────────────
        // Ensure demo sessions have JSONL paths so the session recorder can attach.
        // Drip-feed scripted events for sessions that are actively being recorded.
        let highlight = self
            .demo_highlight
            .get_or_insert_with(crate::demo::DemoHighlightState::new);

        for session in &mut sessions {
            let path = highlight.ensure_jsonl(session.pid).clone();
            session.jsonl_path = Some(path);
        }

        // Feed new JSONL events only into sessions being recorded.
        // When the script is exhausted, mark the PID for auto-stop.
        let recording_pids: Vec<u32> = self.session_recordings.keys().copied().collect();
        let mut finished_pids: Vec<u32> = Vec::new();
        for pid in recording_pids {
            if !highlight.drip_feed(pid) {
                finished_pids.push(pid);
            }
        }

        // Auto-stop recordings whose scripts are done
        for pid in finished_pids {
            if let Some(path) = self.session_recordings.remove(&pid) {
                self.status_msg = format!("Recording complete → {path}");
            }
        }

        // Compute decay scores for demo sessions (same as real refresh path)
        for session in &mut sessions {
            session.decay_score =
                claudectl_core::health::compute_decay_score(session, &self.health_thresholds);
        }

        self.sessions = sessions;
        self.normalize_selection();
    }

    pub fn tick(&mut self) {
        self.status_msg.clear();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for session in &mut self.sessions {
            let elapsed_ms = now_ms.saturating_sub(session.started_at);
            session.elapsed = std::time::Duration::from_millis(elapsed_ms);
        }

        self.refresh();
        self.run_auto_actions();

        // Check idle mode transition
        self.check_idle_mode();

        // Refresh weekly summary every ~30s (15 ticks at 2s interval)
        self.weekly_summary_tick += 1;
        if self.weekly_summary_tick >= 15 {
            self.weekly_summary_tick = 0;
            self.weekly_summary = claudectl_core::history::weekly_summary();
            self.check_aggregate_budgets();
        }

        // Refresh coordination state every ~6s (3 ticks at 2s interval)
        #[cfg(feature = "coord")]
        {
            self.coord_tick += 1;
            if self.coord_tick >= 3 {
                self.coord_tick = 0;
                self.coord_refresh();
            }
        }
    }

    /// Get how long a session has been waiting for input, if applicable.
    pub fn wait_duration(&self, pid: u32) -> Option<std::time::Duration> {
        self.needs_input_since
            .get(&pid)
            .map(|since| since.elapsed())
    }

    /// Format wait duration as a compact string (e.g., "2m 34s").
    pub fn format_wait_time(&self, pid: u32) -> Option<String> {
        let dur = self.wait_duration(pid)?;
        let secs = dur.as_secs();
        if secs < 60 {
            Some(format!("{secs}s"))
        } else {
            Some(format!("{}m {}s", secs / 60, secs % 60))
        }
    }

    /// Refresh cached coordination state from the runtime.
    ///
    /// The `expire_stale_*` calls remain direct because they're side-effects
    /// on the SQLite store (bookkeeping), not part of the read-only
    /// `CoordView` surface. The actual list queries go through the runtime
    /// trait so the binary-coord coupling stays one layer thick.
    #[cfg(feature = "coord")]
    pub fn coord_refresh(&mut self) {
        // Bookkeeping (expire stale leases + interrupts). Best-effort; the
        // orchestrator logs failures internally and never propagates them.
        self.runtime.orchestrator.expire_stale();

        self.coord_leases = self.runtime.coord.active_leases();
        self.coord_handoffs = self.runtime.coord.pending_handoffs();
        self.coord_pending_interrupts = self.runtime.coord.pending_interrupts();

        self.coord_lease_sessions = self
            .coord_leases
            .iter()
            .map(|l| l.owner_session_id.clone())
            .collect();
        self.coord_handoff_sessions = self
            .coord_handoffs
            .iter()
            .flat_map(|h| {
                let mut ids = vec![h.from_session_id.clone()];
                if let Some(ref to) = h.to_session_id {
                    ids.push(to.clone());
                }
                ids
            })
            .collect();
        self.coord_interrupt_targets = self
            .coord_pending_interrupts
            .iter()
            .map(|i| i.target_session_id.clone())
            .collect();
    }

    #[cfg(feature = "coord")]
    pub fn session_has_lease(&self, session_id: &str) -> bool {
        self.coord_lease_sessions.contains(session_id)
    }

    #[cfg(feature = "coord")]
    pub fn session_has_handoff(&self, session_id: &str) -> bool {
        self.coord_handoff_sessions.contains(session_id)
    }

    #[cfg(feature = "coord")]
    pub fn session_has_interrupt(&self, session_id: &str) -> bool {
        self.coord_interrupt_targets.contains(session_id)
    }

    /// Compute budget exhaustion ETA based on current burn rate.
    /// Returns (spent, limit, eta_string, urgency) where urgency is 0=safe, 1=warn, 2=critical.
    pub fn budget_eta(&self) -> Option<(f64, f64, String, u8)> {
        let live_cost: f64 = self.sessions.iter().map(|s| s.cost_usd).sum();
        let total_burn: f64 = self.sessions.iter().map(|s| s.burn_rate_per_hr).sum();

        // Prefer daily limit, fall back to per-session budget
        let (spent, limit) = if let Some(daily) = self.daily_limit {
            (self.weekly_summary.today_cost_usd + live_cost, daily)
        } else if let Some(budget) = self.budget_usd {
            // For per-session budget, show the session closest to limit
            if let Some(session) = self.sessions.iter().max_by(|a, b| {
                (a.cost_usd / budget)
                    .partial_cmp(&(b.cost_usd / budget))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }) {
                (session.cost_usd, budget)
            } else {
                return None;
            }
        } else {
            return None;
        };

        let remaining = limit - spent;
        if remaining <= 0.0 {
            return Some((spent, limit, "exceeded".into(), 2));
        }
        if total_burn < 0.01 {
            return Some((spent, limit, "safe".into(), 0));
        }

        let hours_left = remaining / total_burn;
        let mins_left = (hours_left * 60.0) as u64;
        let eta_str = if mins_left >= 120 {
            format!("{}h {}m", mins_left / 60, mins_left % 60)
        } else {
            format!("{}m", mins_left)
        };

        let urgency = if mins_left <= 30 {
            2
        } else if mins_left <= 120 {
            1
        } else {
            0
        };
        Some((spent, limit, eta_str, urgency))
    }

    fn check_aggregate_budgets(&mut self) {
        let ws = &self.weekly_summary;

        // Also include cost from currently live sessions (not yet in history)
        let live_cost: f64 = self.sessions.iter().map(|s| s.cost_usd).sum();

        // Daily limit check
        if let Some(daily_limit) = self.daily_limit {
            let today_total = ws.today_cost_usd + live_cost;
            let pct = today_total / daily_limit * 100.0;

            if pct >= 80.0 && !self.daily_alert_fired {
                self.daily_alert_fired = true;
                self.status_msg = format!(
                    "DAILY BUDGET: ${:.2}/${:.2} ({:.0}%)",
                    today_total, daily_limit, pct
                );
                fire_notification(&format!("Daily budget at {:.0}%", pct));

                // Fire hooks with a synthetic session containing aggregate data
                let mut dummy = create_aggregate_session(today_total, daily_limit, "daily");
                self.hooks.fire(HookEvent::BudgetWarning, &dummy);

                if pct >= 100.0 {
                    dummy.cost_usd = today_total;
                    self.hooks.fire(HookEvent::BudgetExceeded, &dummy);
                }
            }
        }

        // Weekly limit check
        if let Some(weekly_limit) = self.weekly_limit {
            let week_total = ws.cost_usd + live_cost;
            let pct = week_total / weekly_limit * 100.0;

            if pct >= 80.0 && !self.weekly_alert_fired {
                self.weekly_alert_fired = true;
                self.status_msg = format!(
                    "WEEKLY BUDGET: ${:.2}/${:.2} ({:.0}%)",
                    week_total, weekly_limit, pct
                );
                fire_notification(&format!("Weekly budget at {:.0}%", pct));

                let mut dummy = create_aggregate_session(week_total, weekly_limit, "weekly");
                self.hooks.fire(HookEvent::BudgetWarning, &dummy);

                if pct >= 100.0 {
                    dummy.cost_usd = week_total;
                    self.hooks.fire(HookEvent::BudgetExceeded, &dummy);
                }
            }
        }
    }

    fn check_idle_mode(&mut self) {
        if !self.idle_config.enabled {
            return;
        }
        let idle_threshold = std::time::Duration::from_secs(self.idle_config.after_idle_mins * 60);
        let was_idle = self.idle_mode_active;
        self.idle_mode_active = self.last_user_interaction.elapsed() > idle_threshold;

        if self.idle_mode_active && !was_idle {
            claudectl_core::logger::log("IDLE", "Entering idle mode");
        }
    }

    /// Check if currently in idle mode (used by other systems like lifecycle restart).
    #[allow(dead_code)]
    pub fn is_idle(&self) -> bool {
        self.idle_mode_active
    }

    fn run_auto_actions(&mut self) {
        // In demo mode, events are scripted in refresh_demo() — skip real execution
        if self.demo_mode {
            return;
        }

        // Legacy per-PID auto-approve (toggled with 'a' key)
        let legacy_pids: Vec<u32> = self
            .sessions
            .iter()
            .filter(|s| s.status == SessionStatus::NeedsInput && self.auto_approve.contains(&s.pid))
            .map(|s| s.pid)
            .collect();

        for pid in legacy_pids {
            if let Some(session) = self.sessions.iter().find(|s| s.pid == pid) {
                let _ = self
                    .runtime
                    .actions
                    .log_observation(observation_from(session, "user_approve"));
                match terminals::approve_session(session) {
                    Ok(()) => self.status_msg = format!("Auto-approved {}", session.display_name()),
                    Err(e) => self.status_msg = format!("Auto-approve error: {e}"),
                }
            }
        }

        // Built-in file conflict auto-deny: deny writes to files being edited by another session
        if self.auto_deny_file_conflicts {
            let conflict_candidates: Vec<(u32, String, String)> = self
                .sessions
                .iter()
                .filter(|s| {
                    s.status == SessionStatus::NeedsInput
                        && s.has_file_conflict
                        && s.pending_file_path.is_some()
                })
                .filter_map(|s| {
                    let file = s.pending_file_path.as_ref()?;
                    let other_pids = self.file_conflicts.get(file)?;
                    let other_name = other_pids
                        .iter()
                        .filter(|&&p| p != s.pid)
                        .find_map(|pid| {
                            self.sessions
                                .iter()
                                .find(|o| o.pid == *pid)
                                .map(|o| format!("{} (PID {})", o.display_name(), o.pid))
                        })
                        .unwrap_or_else(|| "another session".into());
                    Some((s.pid, file.clone(), other_name))
                })
                .collect();

            for (pid, file, other) in conflict_candidates {
                // Debounce
                if let Some(last) = self.auto_actions_fired.get(&pid) {
                    if last.elapsed().as_secs() < 5 {
                        continue;
                    }
                }
                if let Some(session) = self.sessions.iter().find(|s| s.pid == pid) {
                    // Log passive observation: conflict auto-deny
                    let _ = self
                        .runtime
                        .actions
                        .log_observation(observation_from(session, "conflict_deny"));
                    let short = file.rsplit('/').next().unwrap_or(&file);
                    let msg = format!("File {short} is being edited by {other}");
                    match self.runtime.actions.inject_text(&session.session_id, &msg) {
                        Ok(()) => {
                            let status = format!(
                                "File conflict: denied {} edit to {short}",
                                session.display_name()
                            );
                            claudectl_core::logger::log("CONFLICT", &status);
                            self.status_msg = status;
                        }
                        Err(e) => {
                            self.status_msg = format!("File conflict deny error: {e}");
                        }
                    }
                    self.auto_actions_fired
                        .insert(pid, std::time::Instant::now());
                }
            }
        }

        // Rule-based auto-actions
        if !self.rules.is_empty() {
            let candidates: Vec<u32> = self
                .sessions
                .iter()
                .filter(|s| {
                    matches!(
                        s.status,
                        SessionStatus::NeedsInput | SessionStatus::WaitingInput
                    )
                })
                .filter(|s| !self.auto_approve.contains(&s.pid)) // Legacy takes priority
                .map(|s| s.pid)
                .collect();

            for pid in candidates {
                // Debounce: don't re-fire within 3 seconds for same PID
                if let Some(last) = self.auto_actions_fired.get(&pid) {
                    if last.elapsed().as_secs() < 3 {
                        continue;
                    }
                }

                let session = match self.sessions.iter().find(|s| s.pid == pid) {
                    Some(s) => s,
                    None => continue,
                };

                let result = claudectl_core::rules::evaluate(&self.rules, session);
                let Some(rule_match) = result else {
                    continue;
                };

                // Log passive observation: static rule fired
                let obs_action = format!("rule_{}", rule_match.action.label());
                let _ = self
                    .runtime
                    .actions
                    .log_observation(observation_from(session, &obs_action));

                let msg = claudectl_core::rules::execute(&rule_match, session);
                match msg {
                    Ok(status) => {
                        claudectl_core::logger::log("AUTO", &status);
                        self.last_rule_action = Some(status.clone());
                        self.status_msg = status;
                    }
                    Err(e) => {
                        self.status_msg = format!("Rule error: {e}");
                    }
                }

                self.auto_actions_fired
                    .insert(pid, std::time::Instant::now());
            }
        } // end if !self.rules.is_empty()

        // Brain inference (opt-in, runs after rules)
        if let Some(ref mut driver) = self.brain_driver {
            // Collect deny-only rules for override checking
            let deny_rules: Vec<_> = self
                .rules
                .iter()
                .filter(|r| r.action == claudectl_core::rules::RuleAction::Deny)
                .cloned()
                .collect();

            let snapshots: Vec<_> = self.sessions.iter().map(snapshot_from).collect();
            let actions = driver.tick(&snapshots, &deny_rules);
            for (_pid, msg) in actions {
                claudectl_core::logger::log("BRAIN", &msg);
                self.status_msg = msg;
            }

            driver.cleanup(&snapshots);

            // Deliver pending mailbox messages to sessions waiting for input.
            // The orchestrator resolves SessionSnapshot back to live sessions
            // internally; we project once here.
            let snapshots: Vec<_> = self.sessions.iter().map(snapshot_from).collect();
            let deliveries = self.runtime.orchestrator.deliver_mailbox(&snapshots);
            for (_pid, msg) in deliveries {
                claudectl_core::logger::log("MAILBOX", &msg);
                self.status_msg = msg;
            }
        }

        // Deliver pending typed interrupts from the coordination bus. The
        // orchestrator handles the SQLite connection internally.
        #[cfg(feature = "coord")]
        {
            let snapshots: Vec<_> = self.sessions.iter().map(snapshot_from).collect();
            let deliveries = self.runtime.orchestrator.deliver_interrupts(&snapshots);
            for (_intr_id, msg) in deliveries {
                claudectl_core::logger::log("INTERRUPT", &msg);
                self.status_msg = msg;
            }
        }
    }

    pub fn handle_auto_approve(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        if session.is_remote() {
            self.status_msg = "Remote session \u{2014} action not available".into();
            return;
        }
        let pid = session.pid;
        let name = session.display_name().to_string();

        if self.pending_auto_approve == Some(pid) {
            if self.auto_approve.contains(&pid) {
                self.auto_approve.remove(&pid);
                self.status_msg = format!("Auto-approve OFF for {name}");
            } else {
                self.auto_approve.insert(pid);
                self.status_msg = format!("Auto-approve ON for {name}");
            }
            self.pending_auto_approve = None;
        } else {
            self.pending_auto_approve = Some(pid);
            let action = if self.auto_approve.contains(&pid) {
                "disable"
            } else {
                "enable"
            };
            self.status_msg = format!("Press a again to {action} auto-approve for {name}");
        }
    }

    pub fn cancel_pending_auto_approve(&mut self) {
        self.pending_auto_approve = None;
    }

    pub fn next(&mut self) {
        let len = self.visible_session_count();
        if len == 0 {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i >= len - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let len = self.visible_session_count();
        if len == 0 {
            return;
        }
        let i = match self.table_state.selected() {
            Some(0) => len - 1,
            Some(i) => i - 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn selected_session(&self) -> Option<&ClaudeSession> {
        let visible = self.visible_session_indices();
        let selected = self.table_state.selected()?;
        let session_idx = *visible.get(selected)?;
        self.sessions.get(session_idx)
    }

    pub fn handle_kill(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        if session.is_remote() {
            self.status_msg = "Remote session \u{2014} action not available".into();
            return;
        }
        let pid = session.pid;
        let name = session.display_name().to_string();

        if self.pending_kill == Some(pid) {
            match self.runtime.actions.terminate_session(pid) {
                Ok(()) => {
                    self.status_msg = format!("Killed {name} (PID {pid})");
                    self.auto_approve.remove(&pid);
                    // Don't delete session file yet — let the Finished tombstone show for 30s.
                    // The file will be cleaned up when the tombstone expires.
                    self.refresh();
                }
                Err(e) => self.status_msg = format!("Kill failed: {e}"),
            }
            self.pending_kill = None;
        } else {
            self.pending_kill = Some(pid);
            self.status_msg = format!("Kill {name} (PID {pid})? Press d again to confirm");
        }
    }

    pub fn cancel_pending_kill(&mut self) {
        if self.pending_kill.is_some() {
            self.pending_kill = None;
            self.status_msg = "Kill cancelled".into();
        }
    }

    /// Handle a key event. Returns false if the application should quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.last_user_interaction = std::time::Instant::now();

        // Transition out of idle mode on any key press
        if self.idle_mode_active {
            self.idle_mode_active = false;
            if !self.idle_report.is_empty() {
                let report = self.idle_report.join("; ");
                self.status_msg = format!("Idle report: {report}");
                self.idle_report.clear();
            }
            self.idle_tasks_launched.clear();
        }

        // Help overlay: any key dismisses
        if self.show_help {
            self.show_help = false;
            return true;
        }

        // Launch mode: capture directory for new session
        if self.launch_mode {
            self.handle_launch_key(key);
            return true;
        }

        if self.search_mode {
            self.handle_search_key(key);
            return true;
        }

        // Input mode: capture text for sending to a session
        if self.input_mode {
            self.handle_input_key(key);
            return true;
        }

        // Role-bind mode: capture role name for the selected session (#307)
        if self.role_bind_mode {
            self.handle_role_bind_key(key);
            return true;
        }

        // Skills overlay: dedicated keymap (j/k navigate, s share, h serve, r rescan, Esc/K close)
        if self.show_skills {
            self.handle_skills_key(key);
            return true;
        }

        // Brain overlay: dedicated keymap (j/k navigate, Tab switch, m mark, n note, r refresh, Esc/B close)
        if self.show_brain {
            self.handle_brain_key(key);
            return true;
        }

        // Override reason prompt: waiting for 1/2/3/Esc
        if self.pending_override_reason.is_some() {
            match key.code {
                KeyCode::Char('1') => {
                    self.handle_brain_accept_with_reason(Some("always_safe"));
                }
                KeyCode::Char('2') => {
                    self.handle_brain_accept_with_reason(Some("one_time_exception"));
                }
                KeyCode::Char('3') => {
                    self.handle_brain_accept_with_reason(Some("brain_is_wrong"));
                }
                KeyCode::Esc => {
                    self.pending_override_reason = None;
                    self.status_msg = "Override cancelled".into();
                }
                _ => {}
            }
            return true;
        }

        // Normal mode
        self.handle_normal_key(key);
        !self.should_quit
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if let Some(pid) = self.input_target_pid {
                    if let Some(session) = self.sessions.iter().find(|s| s.pid == pid) {
                        // Log passive observation: user sent manual input
                        let _ = self
                            .runtime
                            .actions
                            .log_observation(observation_from(session, "user_input"));
                        let text = format!("{}\n", self.input_buffer);
                        match self.runtime.actions.inject_text(&session.session_id, &text) {
                            Ok(()) => {
                                self.status_msg = format!("Sent to {}", session.display_name())
                            }
                            Err(e) => self.status_msg = format!("Error: {e}"),
                        }
                    }
                }
                self.input_mode = false;
                self.input_buffer.clear();
                self.input_target_pid = None;
            }
            KeyCode::Esc => {
                self.input_mode = false;
                self.input_buffer.clear();
                self.input_target_pid = None;
                self.status_msg = "Input cancelled".into();
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.search_query = self.search_buffer.trim().to_string();
                self.search_mode = false;
                self.normalize_selection();
                if self.search_query.is_empty() {
                    self.status_msg = "Search cleared".into();
                } else {
                    self.status_msg = format!("Search: {}", self.search_query);
                }
            }
            KeyCode::Esc => {
                self.search_mode = false;
                self.search_buffer.clear();
                self.status_msg = "Search cancelled".into();
            }
            KeyCode::Backspace => {
                self.search_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.search_buffer.push(c);
            }
            _ => {}
        }
    }

    pub fn open_skills_overlay(&mut self) {
        self.refresh_skills();
        self.refresh_hive_view();
        self.skills_selected = 0;
        self.skills_status_msg = None;
        self.hive_join_input_mode = false;
        self.hive_join_buffer.clear();
        self.show_skills = true;
    }

    pub fn refresh_skills(&mut self) {
        let cwd = std::env::current_dir().ok();
        self.skills = claudectl_core::skills::discover(cwd.as_deref());
        self.shared_skill_keys = self.runtime.hive.shared_skill_keys();
        if self.skills_selected >= self.skills.len() {
            self.skills_selected = self.skills.len().saturating_sub(1);
        }
    }

    pub fn refresh_hive_view(&mut self) {
        let snapshot = self.runtime.hive.hive_view_snapshot();
        self.hive_identity = snapshot.identity;
        self.hive_known_peers = snapshot.peers;
    }

    // ── Brain review overlay ──────────────────────────────────────────────

    pub fn open_brain_overlay(&mut self) {
        self.refresh_brain();
        self.brain_review_selected = 0;
        self.brain_status_msg = None;
        self.brain_note_input_mode = false;
        self.brain_note_buffer.clear();
        self.brain_tab = BrainTab::Scorecard;
        self.show_brain = true;
    }

    pub fn refresh_brain(&mut self) {
        self.brain_decisions_cache = self.runtime.review.all_decisions();
        self.brain_queue = self.runtime.review.review_queue();
        if self.brain_review_selected >= self.brain_queue.len() {
            self.brain_review_selected = self.brain_queue.len().saturating_sub(1);
        }
    }

    fn handle_brain_key(&mut self, key: KeyEvent) {
        if self.brain_note_input_mode {
            self.handle_brain_note_input(key);
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('M'), _) | (KeyCode::Char('q'), _) => {
                self.show_brain = false;
                self.brain_status_msg = None;
                return;
            }
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                self.brain_tab = self.brain_tab.toggle();
                self.brain_status_msg = None;
                return;
            }
            (KeyCode::Char('r'), _) => {
                self.refresh_brain();
                self.brain_status_msg = Some("Refreshed.".into());
                return;
            }
            _ => {}
        }

        if matches!(self.brain_tab, BrainTab::Review) {
            self.handle_brain_review_tab_key(key);
        }
    }

    fn handle_brain_review_tab_key(&mut self, key: KeyEvent) {
        if self.brain_queue.is_empty() {
            return;
        }
        let last = self.brain_queue.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.brain_review_selected < last {
                    self.brain_review_selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.brain_review_selected > 0 {
                    self.brain_review_selected -= 1;
                }
            }
            KeyCode::Char('g') | KeyCode::Home => self.brain_review_selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.brain_review_selected = last,
            KeyCode::Char('m') => self.mark_selected_canonical(None),
            KeyCode::Char('n') => {
                self.brain_note_input_mode = true;
                self.brain_note_buffer.clear();
                self.brain_status_msg = Some("Type a note, Enter to save, Esc to cancel.".into());
            }
            KeyCode::Char('s') | KeyCode::Right | KeyCode::Char('l')
                if self.brain_review_selected < last =>
            {
                self.brain_review_selected += 1;
                self.brain_status_msg = Some("Skipped.".into());
            }
            _ => {}
        }
    }

    fn mark_selected_canonical(&mut self, note: Option<&str>) {
        let Some(item) = self.brain_queue.get(self.brain_review_selected) else {
            return;
        };
        // DecisionSummary stores the id as a String; empty == "no decision_id".
        let id = item.decision.id.clone();
        if id.is_empty() {
            self.brain_status_msg = Some("No decision_id — older record, can't mark.".into());
            return;
        }
        match self
            .runtime
            .actions
            .mark_canonical(&id, note.map(String::from))
        {
            Ok(()) => {
                self.brain_status_msg = Some(if note.is_some() {
                    format!("Marked canonical with note: {id}")
                } else {
                    format!("Marked canonical: {id}")
                });
                // Drop the marked item and advance selection naturally.
                self.brain_queue.remove(self.brain_review_selected);
                if self.brain_review_selected >= self.brain_queue.len() {
                    self.brain_review_selected = self.brain_queue.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.brain_status_msg = Some(format!("Could not mark canonical: {e}"));
            }
        }
    }

    fn handle_brain_note_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let note = self.brain_note_buffer.trim().to_string();
                self.brain_note_input_mode = false;
                self.brain_note_buffer.clear();
                if note.is_empty() {
                    self.brain_status_msg = Some("Empty note — not saved.".into());
                } else {
                    self.mark_selected_canonical(Some(&note));
                }
            }
            KeyCode::Esc => {
                self.brain_note_input_mode = false;
                self.brain_note_buffer.clear();
                self.brain_status_msg = Some("Note cancelled.".into());
            }
            KeyCode::Backspace => {
                self.brain_note_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.brain_note_buffer.push(c);
            }
            _ => {}
        }
    }

    fn handle_skills_key(&mut self, key: KeyEvent) {
        if self.hive_join_input_mode {
            self.handle_hive_join_input(key);
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('K'), _) | (KeyCode::Char('q'), _) => {
                self.show_skills = false;
                self.skills_status_msg = None;
                return;
            }
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                self.skills_tab = self.skills_tab.toggle();
                self.skills_status_msg = None;
                return;
            }
            _ => {}
        }

        match self.skills_tab {
            SkillsTab::Skills => self.handle_skills_tab_key(key),
            SkillsTab::Hive => self.handle_hive_tab_key(key),
        }
    }

    fn handle_skills_tab_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('j'), _) | (KeyCode::Down, _)
                if !self.skills.is_empty() && self.skills_selected + 1 < self.skills.len() =>
            {
                self.skills_selected += 1;
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) if self.skills_selected > 0 => {
                self.skills_selected -= 1;
            }
            (KeyCode::Char('r'), _) => {
                self.refresh_skills();
                self.skills_status_msg = Some(format!("Rescanned: {} skills", self.skills.len()));
            }
            (KeyCode::Char('s'), _) => {
                self.share_selected_skill();
            }
            _ => {}
        }
    }

    fn handle_hive_tab_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('h'), _) => {
                self.start_hive_listener();
                self.refresh_hive_view();
            }
            (KeyCode::Char('i'), _) => {
                self.generate_hive_invite();
            }
            (KeyCode::Char('J'), _) => {
                self.hive_join_input_mode = true;
                self.hive_join_buffer.clear();
                self.skills_status_msg =
                    Some("Paste invite (relay code, link, or word phrase); Enter to join".into());
            }
            (KeyCode::Char('r'), _) => {
                self.refresh_hive_view();
                self.skills_status_msg =
                    Some(format!("Known peers: {}", self.hive_known_peers.len()));
            }
            _ => {}
        }
    }

    fn handle_hive_join_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let code = self.hive_join_buffer.trim().to_string();
                self.hive_join_input_mode = false;
                if code.is_empty() {
                    self.skills_status_msg = Some("Join cancelled (empty)".into());
                    return;
                }
                match spawn_relay_join(&code) {
                    Ok(()) => {
                        self.skills_status_msg = Some(format!(
                            "Join started (claudectl relay join {} detached)",
                            short_id(&code)
                        ));
                    }
                    Err(e) => {
                        self.skills_status_msg = Some(format!("Join failed: {e}"));
                    }
                }
                self.hive_join_buffer.clear();
            }
            KeyCode::Esc => {
                self.hive_join_input_mode = false;
                self.hive_join_buffer.clear();
                self.skills_status_msg = Some("Join cancelled".into());
            }
            KeyCode::Backspace => {
                self.hive_join_buffer.pop();
            }
            KeyCode::Char(c) if self.hive_join_buffer.len() < 256 => {
                self.hive_join_buffer.push(c);
            }
            _ => {}
        }
    }

    fn generate_hive_invite(&mut self) {
        match generate_invite_via_cli() {
            Ok(invite) => {
                self.skills_status_msg = Some(format!("Invite: {}", invite.relay_code));
                self.hive_last_invite = Some(invite);
            }
            Err(e) => {
                self.skills_status_msg = Some(format!("Invite failed: {e}"));
            }
        }
    }

    fn share_selected_skill(&mut self) {
        let Some(skill) = self.skills.get(self.skills_selected).cloned() else {
            self.skills_status_msg = Some("No skill selected".into());
            return;
        };
        if !cfg!(feature = "hive") {
            self.skills_status_msg = Some("hive feature disabled in this build".into());
            return;
        }
        if !skill.within_share_limit() {
            self.skills_status_msg = Some("Skill exceeds 32kb share limit".into());
            return;
        }
        if self.shared_skill_keys.contains(&skill.semantic_key()) {
            self.skills_status_msg = Some("Already shared".into());
            return;
        }
        match self.runtime.hive.share_skill(&skill) {
            Ok(unit_id) => {
                self.shared_skill_keys.insert(skill.semantic_key());
                self.skills_status_msg = Some(format!(
                    "Shared '{}' → unit {}",
                    skill.name,
                    short_id(&unit_id)
                ));
            }
            Err(e) => {
                self.skills_status_msg = Some(format!("Share failed: {e}"));
            }
        }
    }

    fn start_hive_listener(&mut self) {
        if !cfg!(feature = "relay") {
            self.skills_status_msg =
                Some("relay feature not built — rebuild with --features relay,hive".into());
            return;
        }
        if self.hive_listener_running {
            self.skills_status_msg = Some("Hive listener already running".into());
            return;
        }
        match spawn_relay_serve() {
            Ok(()) => {
                self.hive_listener_running = true;
                self.skills_status_msg =
                    Some("Hive listener started (claudectl relay serve detached)".into());
            }
            Err(e) => {
                self.skills_status_msg = Some(format!("Start failed: {e}"));
            }
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                self.should_quit = true;
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.next();
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.previous();
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                // Bus role bind (#307). Ctrl+R because plain `r` is refresh.
                // Match before the unconditional `r` arm below, otherwise
                // the wildcard modifier swallows the Control modifier.
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_role_bind_mode();
            }
            (KeyCode::Char('r'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.refresh();
            }
            (KeyCode::Char('R'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.toggle_session_recording();
            }
            (KeyCode::Char('d'), _) | (KeyCode::Char('x'), _) => {
                self.cancel_pending_auto_approve();
                self.handle_kill();
            }
            (KeyCode::Char('y'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_approve();
            }
            (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.toggle_brain_gate();
            }
            (KeyCode::Char('b'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_brain_accept();
            }
            (KeyCode::Char('B'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_brain_reject();
            }
            (KeyCode::Char('i'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_input_mode();
            }
            (KeyCode::Char('c'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_compact();
            }
            (KeyCode::Char('?'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.show_help = !self.show_help;
            }
            (KeyCode::Char('K'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.open_skills_overlay();
            }
            (KeyCode::Char('M'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.open_brain_overlay();
            }
            (KeyCode::Char('s'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.cycle_sort();
            }
            (KeyCode::Char('f'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.cycle_status_filter();
            }
            (KeyCode::Char('v'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.cycle_focus_filter();
            }
            (KeyCode::Char('z'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.clear_filters();
            }
            (KeyCode::Char('/'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_search_mode();
            }
            (KeyCode::Char('a'), _) => {
                self.cancel_pending_kill();
                self.handle_auto_approve();
            }
            (KeyCode::Char('n'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.enter_launch_mode();
            }
            (KeyCode::Char('g'), _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.grouped_view = !self.grouped_view;
                self.status_msg = if self.grouped_view {
                    "Grouped by project".into()
                } else {
                    "Flat view".into()
                };
            }
            (KeyCode::Enter, _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.detail_panel = !self.detail_panel;
            }
            (KeyCode::Tab, _) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.handle_switch_terminal();
            }
            #[cfg(feature = "relay")]
            (KeyCode::Char('p'), KeyModifiers::NONE) => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
                self.show_peers_panel = !self.show_peers_panel;
                self.status_msg = if self.show_peers_panel {
                    "Peers panel enabled".into()
                } else {
                    "Peers panel disabled".into()
                };
            }
            _ => {
                self.cancel_pending_kill();
                self.cancel_pending_auto_approve();
            }
        }
    }

    fn handle_launch_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit_launch_form();
            }
            KeyCode::Enter => {
                if self.launch_form.is_last_field() {
                    self.submit_launch_form();
                } else {
                    self.launch_form.advance();
                    self.status_msg = self.launch_form.status_hint();
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                self.launch_form.advance();
                self.status_msg = self.launch_form.status_hint();
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.launch_form.retreat();
                self.status_msg = self.launch_form.status_hint();
            }
            KeyCode::Esc => {
                self.launch_mode = false;
                self.launch_form = LaunchForm::default();
                self.status_msg = "Launch cancelled".into();
            }
            KeyCode::Backspace => {
                self.launch_form.active_buffer_mut().pop();
            }
            KeyCode::Char(c) => {
                self.launch_form.active_buffer_mut().push(c);
            }
            _ => {}
        }
    }

    fn enter_launch_mode(&mut self) {
        self.launch_mode = true;
        self.launch_form = LaunchForm::default();
        self.status_msg = self.launch_form.status_hint();
    }

    fn submit_launch_form(&mut self) {
        let request = match self.launch_form.request() {
            Ok(request) => request,
            Err(err) => {
                self.launch_form.field = LaunchField::Cwd;
                self.status_msg = format!("Launch failed: {err}");
                return;
            }
        };

        match launch::launch(&request) {
            Ok(target) => {
                self.launch_mode = false;
                self.launch_form = LaunchForm::default();
                self.status_msg = format!(
                    "Launched session in {target} at {}{}",
                    request.cwd_path.display(),
                    request.option_summary()
                );
            }
            Err(err) => {
                self.status_msg = format!("Launch failed: {err}");
            }
        }
    }

    fn enter_search_mode(&mut self) {
        self.search_mode = true;
        self.search_buffer = self.search_query.clone();
    }

    pub fn clear_filters(&mut self) {
        self.status_filter = StatusFilter::All;
        self.focus_filter = FocusFilter::All;
        self.search_query.clear();
        self.search_buffer.clear();
        self.search_mode = false;
        self.normalize_selection();
        self.status_msg = "Filters cleared".into();
    }

    pub fn cycle_status_filter(&mut self) {
        self.status_filter = self.status_filter.next();
        self.normalize_selection();
        self.status_msg = format!("Status filter: {}", self.status_filter.label());
    }

    pub fn cycle_focus_filter(&mut self) {
        self.focus_filter = self.focus_filter.next();
        self.normalize_selection();
        self.status_msg = format!("Focus filter: {}", self.focus_filter.label());
    }

    pub fn has_active_filters(&self) -> bool {
        self.status_filter != StatusFilter::All
            || self.focus_filter != FocusFilter::All
            || !self.search_query.trim().is_empty()
    }

    pub fn filter_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.status_filter != StatusFilter::All {
            parts.push(format!("status={}", self.status_filter.label()));
        }
        if self.focus_filter != FocusFilter::All {
            parts.push(format!("focus={}", self.focus_filter.label()));
        }
        if !self.search_query.trim().is_empty() {
            parts.push(format!("search=\"{}\"", self.search_query));
        }
        if parts.is_empty() {
            "filters: none".to_string()
        } else {
            format!("filters: {}", parts.join(" | "))
        }
    }

    pub fn visible_session_indices(&self) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(idx, session)| self.matches_filters(session).then_some(idx))
            .collect()
    }

    pub fn visible_sessions(&self) -> Vec<&ClaudeSession> {
        self.visible_session_indices()
            .into_iter()
            .filter_map(|idx| self.sessions.get(idx))
            .collect()
    }

    pub fn visible_session_count(&self) -> usize {
        self.visible_session_indices().len()
    }

    fn normalize_selection(&mut self) {
        let len = self.visible_session_count();
        if len == 0 {
            self.table_state.select(None);
        } else if self.table_state.selected().is_none() {
            self.table_state.select(Some(0));
        } else if let Some(sel) = self.table_state.selected() {
            if sel >= len {
                self.table_state.select(Some(len - 1));
            }
        }
    }

    fn matches_filters(&self, session: &ClaudeSession) -> bool {
        self.status_filter.matches(session.status)
            && self.matches_focus_filter(session)
            && self.matches_search_query(session)
    }

    fn matches_focus_filter(&self, session: &ClaudeSession) -> bool {
        let over_budget = self
            .budget_usd
            .map(|budget| session.has_usage_metrics() && session.cost_usd >= budget)
            .unwrap_or(false);
        let high_context = session.has_usage_metrics()
            && session.context_percent() >= self.context_warn_threshold as f64;
        let unknown_telemetry = !session.has_usage_metrics();
        let conflict = self.conflict_pids.contains(&session.pid);

        match self.focus_filter {
            FocusFilter::All => true,
            FocusFilter::Attention => {
                session.status == SessionStatus::NeedsInput
                    || over_budget
                    || high_context
                    || unknown_telemetry
                    || conflict
            }
            FocusFilter::OverBudget => over_budget,
            FocusFilter::HighContext => high_context,
            FocusFilter::UnknownTelemetry => unknown_telemetry,
            FocusFilter::Conflict => conflict,
        }
    }

    fn matches_search_query(&self, session: &ClaudeSession) -> bool {
        let query = self.search_query.trim();
        if query.is_empty() {
            return true;
        }

        let query = query.to_ascii_lowercase();
        let fields = [
            session.display_name().to_string(),
            session.project_name.clone(),
            session.model.clone(),
            session.cwd.clone(),
            session.session_id.clone(),
        ];

        fields
            .iter()
            .any(|field| field.to_ascii_lowercase().contains(&query))
    }

    fn handle_approve(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.is_remote() {
                self.status_msg = "Remote session \u{2014} action not available".into();
                return;
            }
            if session.status == SessionStatus::NeedsInput {
                // Log passive observation: user approved without brain involvement
                let _ = self
                    .runtime
                    .actions
                    .log_observation(observation_from(session, "user_approve"));
                match terminals::approve_session(session) {
                    Ok(()) => self.status_msg = format!("Approved {}", session.display_name()),
                    Err(e) => self.status_msg = format!("Error: {e}"),
                }
            } else {
                self.status_msg = "Session is not waiting for input".into();
            }
        }
    }

    fn handle_brain_accept(&mut self) {
        self.handle_brain_accept_with_reason(None);
    }

    fn handle_brain_accept_with_reason(&mut self, override_reason: Option<&str>) {
        // Clone session data first to avoid borrow conflict with brain_engine
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if session.is_remote() {
            self.status_msg = "Remote session \u{2014} action not available".into();
            return;
        }
        let pid = session.pid;
        let Some(ref mut driver) = self.brain_driver else {
            self.status_msg = "Brain is not enabled".into();
            return;
        };
        // Get suggestion before accept (for logging)
        let suggestion = driver.pending_for(pid);
        let Some(sg) = suggestion else {
            self.status_msg = "No brain suggestion pending for this session".into();
            return;
        };

        // If brain suggested deny and no override reason yet, prompt for one
        if sg.action == "deny" && override_reason.is_none() {
            self.pending_override_reason = Some(pid);
            self.status_msg =
                "Override reason: [1] Always safe  [2] One-time exception  [3] Brain is wrong  [Esc] Cancel"
                    .into();
            return;
        }

        if let Some(msg) = driver.accept(pid) {
            let _ = self
                .runtime
                .actions
                .log_decision(claudectl_core::runtime::LogDecisionInput {
                    session_pid: pid,
                    project: session.display_name().to_string(),
                    tool: session.pending_tool_name.clone(),
                    command: session.pending_tool_input.clone(),
                    suggestion: sg,
                    user_action: "accept".into(),
                    decision_type: claudectl_core::runtime::DecisionScope::Session,
                    override_reason: override_reason.map(String::from),
                });
            claudectl_core::logger::log("BRAIN", &format!("Accepted: {msg}"));
            self.status_msg = msg;
        }
        self.pending_override_reason = None;
    }

    fn handle_brain_reject(&mut self) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if session.is_remote() {
            self.status_msg = "Remote session \u{2014} action not available".into();
            return;
        }
        let pid = session.pid;
        let Some(ref mut driver) = self.brain_driver else {
            self.status_msg = "Brain is not enabled".into();
            return;
        };
        if let Some(suggestion) = driver.reject(pid) {
            let log_input = claudectl_core::runtime::LogDecisionInput {
                session_pid: pid,
                project: session.display_name().to_string(),
                tool: session.pending_tool_name.clone(),
                command: session.pending_tool_input.clone(),
                suggestion: suggestion.clone(),
                user_action: "reject".into(),
                decision_type: claudectl_core::runtime::DecisionScope::Session,
                override_reason: None,
            };
            let _ = self.runtime.actions.log_decision(log_input);
            let msg = format!(
                "Rejected brain suggestion: {} ({})",
                suggestion.action, suggestion.reasoning,
            );
            claudectl_core::logger::log("BRAIN", &msg);
            self.status_msg = msg;
        } else {
            self.status_msg = "No brain suggestion pending for this session".into();
        }
    }

    fn toggle_brain_gate(&mut self) {
        use claudectl_core::runtime::BrainGateMode;
        let current = self.runtime.brain.gate_mode();
        // Toggle: On → Off, Off → On, Auto → Off. The wizard flips through
        // runtime.actions so the on-disk format stays in sync with what
        // BrainView reports next refresh.
        let next = match current {
            BrainGateMode::On => BrainGateMode::Off,
            BrainGateMode::Off => BrainGateMode::On,
            BrainGateMode::Auto => BrainGateMode::Off,
        };
        if let Err(e) = self.runtime.actions.set_gate_mode(next) {
            self.status_msg = format!("Brain: gate-mode update failed: {e}");
            return;
        }

        let description = match next {
            BrainGateMode::On => "active — evaluating tool calls",
            BrainGateMode::Off => "disabled — normal permission flow",
            BrainGateMode::Auto => "auto — automatic decisions",
        };
        self.status_msg = format!("Brain: {description}");
        claudectl_core::logger::log("BRAIN", &format!("Gate mode toggled: {current} → {next}"));
    }

    fn toggle_session_recording(&mut self) {
        let info = self
            .selected_session()
            .map(|s| (s.pid, s.display_name().to_string(), s.jsonl_path.is_some()));
        let Some((pid, name, has_jsonl)) = info else {
            return;
        };

        // Per-session toggle: if this session is recording, stop just this one
        if self.session_recordings.contains_key(&pid) {
            let path = self.session_recordings.remove(&pid).unwrap_or_default();
            self.status_msg = format!("Recording stopped → {path}");
            return;
        }

        // Start recording the selected session
        if !has_jsonl {
            self.status_msg = "Cannot record — no JSONL file for this session".into();
            return;
        }
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let path = format!("{}-{}-{}.gif", name, pid, epoch);
        self.session_recordings.insert(pid, path.clone());
        self.status_msg = format!("Recording {name} → {path} (R to stop)");
    }

    fn handle_compact(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.is_remote() {
                self.status_msg = "Remote session \u{2014} action not available".into();
                return;
            }
            match session.status {
                SessionStatus::WaitingInput | SessionStatus::Idle => {
                    match self
                        .runtime
                        .actions
                        .inject_text(&session.session_id, "/compact\n")
                    {
                        Ok(()) => {
                            self.status_msg = format!("Sent /compact to {}", session.display_name())
                        }
                        Err(e) => self.status_msg = format!("Compact error: {e}"),
                    }
                }
                SessionStatus::NeedsInput => {
                    self.status_msg =
                        "Cannot compact — session is waiting for permission approval".into();
                }
                SessionStatus::Processing => {
                    self.status_msg =
                        "Cannot compact — session is processing (wait until idle)".into();
                }
                SessionStatus::Unknown => {
                    self.status_msg =
                        "Cannot compact — transcript telemetry is unavailable for this session"
                            .into();
                }
                SessionStatus::Finished => {
                    self.status_msg = "Cannot compact — session has finished".into();
                }
            }
        }
    }

    fn enter_input_mode(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.is_remote() {
                self.status_msg = "Remote session \u{2014} action not available".into();
                return;
            }
        }
        let info = self
            .selected_session()
            .map(|s| (s.pid, s.display_name().to_string()));
        if let Some((pid, name)) = info {
            self.input_mode = true;
            self.input_buffer.clear();
            self.input_target_pid = Some(pid);
            self.status_msg = format!("Input to {name} (Enter to send, Esc to cancel): ");
        }
    }

    /// Open the role-bind prompt for the selected session (#307). Captures
    /// the session's pid and cwd at entry time so a refresh tick or row
    /// move during typing can't change the target.
    fn enter_role_bind_mode(&mut self) {
        let Some(session) = self.selected_session() else {
            self.status_msg = "No session selected".into();
            return;
        };
        if session.is_remote() {
            self.status_msg = "Remote session \u{2014} bind locally instead".into();
            return;
        }
        let pid = session.pid;
        let cwd = session.cwd.clone();
        let name = session.display_name().to_string();
        self.role_bind_mode = true;
        self.role_bind_buffer.clear();
        self.role_bind_target_pid = Some(pid);
        self.role_bind_target_cwd = Some(cwd);
        self.status_msg =
            format!("Bind role for {name} (pid={pid}, Enter to bind, Esc to cancel): ");
    }

    fn handle_role_bind_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let role = self.role_bind_buffer.trim().to_string();
                let pid = self.role_bind_target_pid;
                let cwd = self.role_bind_target_cwd.clone();
                self.role_bind_mode = false;
                self.role_bind_buffer.clear();
                self.role_bind_target_pid = None;
                self.role_bind_target_cwd = None;
                if role.is_empty() {
                    self.status_msg = "Role name required".into();
                    return;
                }
                let (Some(pid), Some(cwd)) = (pid, cwd) else {
                    self.status_msg = "Lost bind target — re-select the session".into();
                    return;
                };
                match self.runtime.actions.bind_bus_role(&role, &cwd, pid) {
                    Ok(()) => {
                        self.status_msg = format!("Bound role {role} -> pid={pid} cwd={cwd}");
                    }
                    Err(e) => {
                        self.status_msg = format!("Bind failed: {e}");
                    }
                }
            }
            KeyCode::Esc => {
                self.role_bind_mode = false;
                self.role_bind_buffer.clear();
                self.role_bind_target_pid = None;
                self.role_bind_target_cwd = None;
                self.status_msg = "Role bind cancelled".into();
            }
            KeyCode::Backspace => {
                self.role_bind_buffer.pop();
            }
            // Role names are short, alpha-numeric with - and _. Cap at 64
            // so a runaway paste can't take the prompt hostage.
            KeyCode::Char(c)
                if self.role_bind_buffer.len() < 64
                    && (c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
            {
                self.role_bind_buffer.push(c);
            }
            _ => {}
        }
    }

    fn handle_switch_terminal(&mut self) {
        if let Some(session) = self.selected_session() {
            if session.is_remote() {
                self.status_msg = "Remote session \u{2014} action not available".into();
                return;
            }
            match terminals::switch_to_terminal(session) {
                Ok(()) => {
                    self.status_msg = format!("Switched to {}", session.display_name());
                }
                Err(e) => {
                    self.status_msg = format!("Error: {e}");
                }
            }
        } else {
            self.status_msg = "No session selected".into();
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub name: String,
    pub session_count: usize,
    pub active_count: usize,
    pub total_cost: f64,
    pub avg_context_pct: f64,
}

impl App {
    pub fn project_groups(&self) -> Vec<ProjectGroup> {
        let mut groups: HashMap<String, Vec<&ClaudeSession>> = HashMap::new();
        for s in self.visible_sessions() {
            groups.entry(s.project_name.clone()).or_default().push(s);
        }

        let mut result: Vec<ProjectGroup> = groups
            .into_iter()
            .map(|(name, sessions)| {
                let active_count = sessions
                    .iter()
                    .filter(|s| {
                        matches!(
                            s.status,
                            SessionStatus::Processing | SessionStatus::NeedsInput
                        )
                    })
                    .count();
                let total_cost: f64 = sessions.iter().map(|s| s.cost_usd).sum();
                let avg_context_pct = if sessions.is_empty() {
                    0.0
                } else {
                    sessions.iter().map(|s| s.context_percent()).sum::<f64>()
                        / sessions.len() as f64
                };
                ProjectGroup {
                    name,
                    session_count: sessions.len(),
                    active_count,
                    total_cost,
                    avg_context_pct,
                }
            })
            .collect();

        result.sort_by(|a, b| {
            b.total_cost
                .partial_cmp(&a.total_cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        result
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Hive/relay shell-out helpers — kept at module scope so the App methods stay
// short. The read-side helpers (skill-key collection, hive snapshot) and the
// write-side helper (share skill) moved into `runtime::hive::LiveHiveActions`
// so the future TUI crate (#275) can hold them through the trait surface.
// ────────────────────────────────────────────────────────────────────────────

/// Detach a `claudectl relay serve` child so the TUI keeps running.
#[cfg(feature = "relay")]
fn spawn_relay_serve() -> Result<(), String> {
    use std::process::{Command, Stdio};
    Command::new(std::env::current_exe().unwrap_or_else(|_| "claudectl".into()))
        .args(["relay", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "relay"))]
fn spawn_relay_serve() -> Result<(), String> {
    Err("relay feature not built".into())
}

/// Detach a `claudectl relay join <code>` child so the TUI keeps running.
#[cfg(feature = "relay")]
fn spawn_relay_join(code: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    Command::new(std::env::current_exe().unwrap_or_else(|_| "claudectl".into()))
        .args(["relay", "join", code])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "relay"))]
fn spawn_relay_join(_code: &str) -> Result<(), String> {
    Err("relay feature not built".into())
}

/// Shell out to `claudectl relay invite --json` and parse the result. We use
/// the existing CLI path rather than re-implementing because invite generation
/// has multiple components (crypto, LAN-IP detection, encoding) that already
/// live there.
#[cfg(feature = "relay")]
fn generate_invite_via_cli() -> Result<HiveInvite, String> {
    use std::process::Command;
    let bin = std::env::current_exe().map_err(|e| e.to_string())?;
    let output = Command::new(bin)
        .args(["--json", "relay", "invite", "--words"])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| e.to_string())?;
    let relay_code = parsed
        .get("relay_code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let invite_link = parsed
        .get("invite_link")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let word_phrase = parsed
        .get("word_phrase")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if relay_code.is_empty() {
        return Err("invite payload missing relay_code".into());
    }
    Ok(HiveInvite {
        relay_code,
        invite_link,
        word_phrase,
    })
}

#[cfg(not(feature = "relay"))]
fn generate_invite_via_cli() -> Result<HiveInvite, String> {
    Err("relay feature not built".into())
}

fn short_id(id: &str) -> String {
    if id.len() <= 12 {
        id.to_string()
    } else {
        format!("{}…", &id[..11])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudectl_core::session::{RawSession, TelemetryStatus};

    fn make_session(
        pid: u32,
        project: &str,
        model: &str,
        status: SessionStatus,
        cost_usd: f64,
        context_pct: f64,
        telemetry_available: bool,
    ) -> ClaudeSession {
        let raw = RawSession {
            pid,
            session_id: format!("session-{pid}"),
            cwd: format!("/tmp/{project}"),
            started_at: 0,
        };
        let mut session = ClaudeSession::from_raw(raw);
        session.project_name = project.to_string();
        session.model = model.to_string();
        session.status = status;
        session.cost_usd = cost_usd;
        session.context_max = 100;
        session.context_tokens = context_pct as u64;
        session.telemetry_status = if telemetry_available {
            TelemetryStatus::Available
        } else {
            TelemetryStatus::MissingTranscript
        };
        session.usage_metrics_available = telemetry_available;
        session
    }

    fn make_test_app() -> App {
        let mut app = App::new();
        app.sessions = vec![
            make_session(
                11,
                "blocked-api",
                "sonnet-4.6",
                SessionStatus::NeedsInput,
                2.0,
                40.0,
                true,
            ),
            make_session(
                12,
                "hot-cost",
                "opus-4.6",
                SessionStatus::Processing,
                7.5,
                30.0,
                true,
            ),
            make_session(
                13,
                "high-context",
                "haiku",
                SessionStatus::WaitingInput,
                1.0,
                90.0,
                true,
            ),
            make_session(
                14,
                "unknown-metrics",
                "",
                SessionStatus::Unknown,
                0.0,
                0.0,
                false,
            ),
        ];
        app.budget_usd = Some(5.0);
        app.context_warn_threshold = 75;
        app.conflict_pids.insert(13);
        app.normalize_selection();
        app
    }

    #[test]
    fn status_filter_returns_only_matching_sessions() {
        let mut app = make_test_app();
        app.status_filter = StatusFilter::NeedsInput;
        let visible: Vec<u32> = app.visible_sessions().iter().map(|s| s.pid).collect();
        assert_eq!(visible, vec![11]);
    }

    #[test]
    fn focus_filter_attention_matches_high_signal_sessions() {
        let mut app = make_test_app();
        app.focus_filter = FocusFilter::Attention;
        let visible: Vec<u32> = app.visible_sessions().iter().map(|s| s.pid).collect();
        assert_eq!(visible, vec![11, 12, 13, 14]);
    }

    #[test]
    fn search_query_matches_project_and_model() {
        let mut app = make_test_app();
        app.search_query = "sonnet".into();
        let visible: Vec<u32> = app.visible_sessions().iter().map(|s| s.pid).collect();
        assert_eq!(visible, vec![11]);

        app.search_query = "unknown-metrics".into();
        let visible: Vec<u32> = app.visible_sessions().iter().map(|s| s.pid).collect();
        assert_eq!(visible, vec![14]);
    }

    #[test]
    fn normalize_selection_clamps_to_filtered_session_count() {
        let mut app = make_test_app();
        app.table_state.select(Some(3));
        app.status_filter = StatusFilter::NeedsInput;
        app.normalize_selection();
        assert_eq!(app.table_state.selected(), Some(0));
        assert_eq!(app.selected_session().map(|s| s.pid), Some(11));
    }

    #[test]
    fn launch_wizard_starts_with_cli_defaults() {
        let mut app = App::new();
        app.enter_launch_mode();

        assert!(app.launch_mode);
        assert_eq!(app.launch_form.field, LaunchField::Cwd);
        assert_eq!(app.launch_form.cwd, ".");
        assert!(app.launch_form.prompt.is_empty());
        assert!(app.launch_form.resume.is_empty());
    }

    #[test]
    fn launch_wizard_moves_between_fields() {
        let mut app = App::new();
        app.enter_launch_mode();

        app.handle_launch_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.launch_form.field, LaunchField::Prompt);

        app.handle_launch_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.launch_form.field, LaunchField::Resume);

        app.handle_launch_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.launch_form.field, LaunchField::Prompt);
    }

    #[test]
    fn invalid_launch_keeps_wizard_open_and_reports_error() {
        let mut app = App::new();
        app.enter_launch_mode();
        app.launch_form.cwd = "/tmp/claudectl-this-path-should-not-exist".into();
        app.launch_form.field = LaunchField::Resume;

        app.submit_launch_form();

        assert!(app.launch_mode);
        assert_eq!(app.launch_form.field, LaunchField::Cwd);
        assert!(
            app.status_msg
                .starts_with("Launch failed: Directory not found:")
        );
    }
}

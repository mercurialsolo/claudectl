use std::collections::{HashMap, HashSet};

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

// Behavior-preserving decomposition of the former monolithic app.rs.
// Each submodule holds an `impl App` block grouped by concern.
mod actions;
mod demo;
mod filters;
mod input;
mod overlays;
mod update;

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
    /// Minimum time between desktop notifications that share the same key.
    /// Suppresses flapping (e.g. a session oscillating in/out of NeedsInput).
    pub notify_cooldown: std::time::Duration,
    /// Last time a notification fired, keyed by event (e.g. "needs-input:<pid>").
    pub last_notified: HashMap<String, std::time::Instant>,
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
    /// Active narrated guided tour (#373). `Some` only under `claudectl demo`.
    pub demo_tour: Option<crate::demo::DemoTour>,
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
    /// Session ids that are supervisor task attempts (latest attempt per task),
    /// used to badge them `T` in the session table (#368).
    #[cfg(feature = "coord")]
    pub coord_task_sessions: HashSet<String>,
    #[cfg(feature = "coord")]
    pub coord_handoff_sessions: HashSet<String>,
    #[cfg(feature = "coord")]
    pub coord_interrupt_targets: HashSet<String>,
    #[cfg(feature = "coord")]
    pub coord_pending_interrupts: Vec<claudectl_core::runtime::InterruptSummary>,
    #[cfg(feature = "coord")]
    pub coord_tick: u32,
    /// Supervisor task ledger (#368). Populated by `coord_refresh`.
    #[cfg(feature = "coord")]
    pub coord_tasks: Vec<claudectl_core::runtime::TaskSummary>,
    /// When true, the draw loop renders the full-screen Supervisor panel.
    #[cfg(feature = "coord")]
    pub show_supervisor: bool,
    #[cfg(feature = "coord")]
    pub supervisor_selected: usize,
    #[cfg(feature = "coord")]
    pub supervisor_status_msg: Option<String>,
    /// Task index armed for cancel — the first `c` arms, the second confirms.
    #[cfg(feature = "coord")]
    pub supervisor_pending_cancel: Option<usize>,
    /// Local view of the supervisor drain marker, toggled by `d`.
    #[cfg(feature = "coord")]
    pub supervisor_draining: bool,
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

/// Decide whether a notification keyed by some event may fire now, given when it
/// last fired. Pure (takes `now` rather than reading the clock) so the cooldown
/// rule can be unit-tested deterministically.
pub fn should_notify(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    cooldown: std::time::Duration,
) -> bool {
    match last {
        Some(t) => now.duration_since(t) >= cooldown,
        None => true,
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
            notify_cooldown: std::time::Duration::from_secs(30),
            last_notified: HashMap::new(),
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
            demo_tour: None,
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
            coord_task_sessions: HashSet::new(),
            #[cfg(feature = "coord")]
            coord_handoff_sessions: HashSet::new(),
            #[cfg(feature = "coord")]
            coord_interrupt_targets: HashSet::new(),
            #[cfg(feature = "coord")]
            coord_pending_interrupts: Vec::new(),
            #[cfg(feature = "coord")]
            coord_tick: 0,
            #[cfg(feature = "coord")]
            coord_tasks: Vec::new(),
            #[cfg(feature = "coord")]
            supervisor_pending_cancel: None,
            #[cfg(feature = "coord")]
            supervisor_draining: false,
            #[cfg(feature = "coord")]
            show_supervisor: false,
            #[cfg(feature = "coord")]
            supervisor_selected: 0,
            #[cfg(feature = "coord")]
            supervisor_status_msg: None,
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

    /// Emit a desktop notification, gated by the master `notify` toggle and a
    /// per-key cooldown. Every notification routes through here so a single
    /// `notify = false` silences every category, and a flapping condition
    /// (same `key`) cannot re-fire faster than `notify_cooldown`.
    fn notify_user(&mut self, key: &str, message: &str) {
        if !self.notify {
            return;
        }
        let now = std::time::Instant::now();
        if !should_notify(
            self.last_notified.get(key).copied(),
            now,
            self.notify_cooldown,
        ) {
            return;
        }
        self.last_notified.insert(key.to_string(), now);
        fire_notification(message);
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

    /// Open the full-screen Supervisor task panel (#368).
    #[cfg(feature = "coord")]
    pub fn open_supervisor_overlay(&mut self) {
        self.coord_refresh();
        self.supervisor_selected = 0;
        self.supervisor_status_msg = None;
        self.show_supervisor = true;
    }

    #[cfg(feature = "coord")]
    fn handle_supervisor_retry(&mut self) {
        let Some(task) = self.coord_tasks.get(self.supervisor_selected) else {
            return;
        };
        let id = task.id.clone();
        let name = task.name.clone();
        match self.runtime.actions.retry_task(&id) {
            Ok(()) => {
                self.supervisor_status_msg = Some(format!("Re-queued {name}"));
                self.coord_refresh();
            }
            Err(e) => self.supervisor_status_msg = Some(format!("Retry failed: {e}")),
        }
    }

    #[cfg(feature = "coord")]
    fn handle_supervisor_approve(&mut self) {
        let Some(task) = self.coord_tasks.get(self.supervisor_selected) else {
            return;
        };
        let id = task.id.clone();
        let name = task.name.clone();
        match self.runtime.actions.approve_task(&id) {
            Ok(()) => {
                self.supervisor_status_msg = Some(format!("Approved {name} (→ DONE)"));
                self.coord_refresh();
            }
            Err(e) => self.supervisor_status_msg = Some(format!("Approve failed: {e}")),
        }
    }

    #[cfg(feature = "coord")]
    fn handle_supervisor_cancel(&mut self, was_armed: Option<usize>) {
        let Some(task) = self.coord_tasks.get(self.supervisor_selected) else {
            return;
        };
        let id = task.id.clone();
        let name = task.name.clone();
        if was_armed == Some(self.supervisor_selected) {
            // Second press on the same row — execute.
            match self.runtime.actions.cancel_task(&id) {
                Ok(()) => {
                    self.supervisor_status_msg = Some(format!("Cancelled {name}"));
                    self.coord_refresh();
                }
                Err(e) => self.supervisor_status_msg = Some(format!("Cancel failed: {e}")),
            }
        } else {
            // First press — arm and prompt for confirmation.
            self.supervisor_pending_cancel = Some(self.supervisor_selected);
            self.supervisor_status_msg = Some(format!("Press c again to cancel {name}"));
        }
    }

    #[cfg(feature = "coord")]
    fn handle_supervisor_drain_toggle(&mut self) {
        let target = !self.supervisor_draining;
        match self.runtime.actions.set_supervisor_drain(target) {
            Ok(()) => {
                self.supervisor_draining = target;
                self.supervisor_status_msg = Some(if target {
                    "Draining — reconciler will stop issuing new assignments".into()
                } else {
                    "Drain cleared — new assignments resume".into()
                });
            }
            Err(e) => self.supervisor_status_msg = Some(format!("Drain toggle failed: {e}")),
        }
    }

    #[cfg(feature = "coord")]
    pub fn session_has_lease(&self, session_id: &str) -> bool {
        self.coord_lease_sessions.contains(session_id)
    }

    /// Whether this session is the latest attempt of a supervisor task (#368).
    #[cfg(feature = "coord")]
    pub fn session_is_task(&self, session_id: &str) -> bool {
        self.coord_task_sessions.contains(session_id)
    }

    #[cfg(feature = "coord")]
    pub fn session_has_handoff(&self, session_id: &str) -> bool {
        self.coord_handoff_sessions.contains(session_id)
    }

    #[cfg(feature = "coord")]
    pub fn session_has_interrupt(&self, session_id: &str) -> bool {
        self.coord_interrupt_targets.contains(session_id)
    }

    /// Check if currently in idle mode (used by other systems like lifecycle restart).
    #[allow(dead_code)]
    pub fn is_idle(&self) -> bool {
        self.idle_mode_active
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

    // ── Brain review overlay ──────────────────────────────────────────────
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn should_notify_fires_when_never_fired() {
        let now = std::time::Instant::now();
        assert!(should_notify(None, now, std::time::Duration::from_secs(60)));
    }

    #[test]
    fn should_notify_suppresses_within_cooldown() {
        // A flapping session re-entering NeedsInput 10s after the last ping
        // must stay silent under a 60s cooldown.
        let base = std::time::Instant::now();
        let now = base + std::time::Duration::from_secs(10);
        assert!(!should_notify(
            Some(base),
            now,
            std::time::Duration::from_secs(60)
        ));
    }

    #[test]
    fn should_notify_fires_after_cooldown() {
        let base = std::time::Instant::now();
        let now = base + std::time::Duration::from_secs(70);
        assert!(should_notify(
            Some(base),
            now,
            std::time::Duration::from_secs(60)
        ));
    }

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

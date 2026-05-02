#![allow(dead_code)]

pub mod accept;
pub mod archive;
pub mod cli;
pub mod distiller;
pub mod effectiveness;
pub mod exposure;
pub mod feedback;
#[cfg(feature = "relay")]
pub mod gossip;
pub mod injection;
pub mod merger;
pub mod store;
pub mod trust;

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// Knowledge unit — the atom of shared learning
// ────────────────────────────────────────────────────────────────────────────

/// What kind of knowledge this is — determines whether it should be shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeCategory {
    /// Tool approval patterns, error handling, safety rules — universal best practices.
    BestPractice,
    /// Instruction scaffolding, prompt patterns, planning vs execution strategies.
    Technique,
    /// Model selection, delegation patterns, agent orchestration choices.
    WorkflowPattern,
    /// Time-of-day habits, approval speed, cost tolerance — personal operating style.
    Personal,
}

impl KnowledgeCategory {
    /// Whether this category should be shared with peers by default.
    pub fn is_shareable(&self) -> bool {
        !matches!(self, Self::Personal)
    }

    /// Whether this category is allowed by a user's share_categories config.
    /// Empty allow_list = share all shareable categories.
    pub fn is_allowed_by(&self, allow_list: &[String]) -> bool {
        if !self.is_shareable() {
            return false;
        }
        if allow_list.is_empty() {
            return true; // empty = no restriction
        }
        allow_list.iter().any(|s| s == self.label())
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::BestPractice => "best_practice",
            Self::Technique => "technique",
            Self::WorkflowPattern => "workflow",
            Self::Personal => "personal",
        }
    }
}

/// A single piece of shareable knowledge derived from brain distillation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeUnit {
    /// Unique ID: `ku_{epoch}_{counter}`
    pub id: String,
    /// What scope this knowledge applies to.
    pub scope: KnowledgeScope,
    /// What kind of knowledge — determines shareability.
    #[serde(default = "default_category")]
    pub category: KnowledgeCategory,
    /// The type and content of the knowledge.
    pub content: KnowledgeContent,
    /// How many local decisions back this knowledge.
    pub evidence_count: u32,
    /// Distillation confidence (0.0 to 1.0).
    pub confidence: f64,
    /// Which peer originated this knowledge.
    pub source_peer: String,
    /// When first created (epoch secs).
    pub originated_at: u64,
    /// When last validated by the originator (epoch secs).
    pub last_validated_at: u64,
    /// How many peers have accepted this knowledge.
    pub propagation_count: u32,
    /// Monotonic version — incremented when the originator updates this unit.
    pub version: u32,
    /// How long (seconds) before this unit is considered stale and decays.
    /// Defaults from `default_revalidation_interval(content)` for old records
    /// that didn't write the field.
    #[serde(default)]
    pub revalidation_interval_secs: u64,
    /// Rollout state for prompt injection (#223). Controls what fraction of
    /// brain prompts include this unit, so freshly-distilled knowledge can be
    /// validated against outcomes before going wide.
    #[serde(default)]
    pub injection_state: InjectionState,
    /// Cumulative stats from prompts that included this unit. Used by the
    /// state machine to advance Canary → Staged → Live or roll back to Draft.
    #[serde(default)]
    pub injection_stats: InjectionStats,
}

fn default_category() -> KnowledgeCategory {
    KnowledgeCategory::BestPractice
}

// ────────────────────────────────────────────────────────────────────────────
// Injection state machine (#223)
// ────────────────────────────────────────────────────────────────────────────

/// Rollout state for a knowledge unit's prompt injection.
///
/// New distillations start at `Canary` so they're validated against outcomes
/// before reaching every prompt. The default for *deserialised* units is
/// `Live` — preserves the pre-#223 behaviour for units that already exist on
/// disk without this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InjectionState {
    /// Not injected at all. Used for units quarantined after repeated bad
    /// outcomes, or for distillations that haven't yet been promoted.
    Draft,
    /// Injected into ~10% of prompts (sampled by pid).
    Canary,
    /// Injected into ~50% of prompts.
    Staged,
    /// Injected into every eligible prompt.
    /// Pre-#223 units (no field on disk) deserialise as Live — they were
    /// already at full rollout, so this keeps their behaviour stable.
    #[default]
    Live,
}

impl InjectionState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Canary => "canary",
            Self::Staged => "staged",
            Self::Live => "live",
        }
    }

    /// Sampling rate for this state (out of 10).
    pub fn sample_buckets(&self) -> u8 {
        match self {
            Self::Draft => 0,
            Self::Canary => 1,
            Self::Staged => 5,
            Self::Live => 10,
        }
    }
}

/// Cumulative outcome stats for a knowledge unit's prompt injections.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InjectionStats {
    /// Total times this unit appeared in a brain prompt.
    #[serde(default)]
    pub injected_count: u64,
    /// Decisions where the brain's suggestion was accepted by the user.
    #[serde(default)]
    pub accepted_count: u64,
    /// Decisions where the brain's suggestion was overridden by the user.
    #[serde(default)]
    pub overridden_count: u64,
    /// Last time this unit was injected (epoch secs).
    #[serde(default)]
    pub last_injected_at: u64,
    /// Last time we received an outcome for this unit (epoch secs).
    #[serde(default)]
    pub last_outcome_at: u64,
}

impl InjectionStats {
    /// Win rate over decided injections; 0.0 when there's no signal yet.
    pub fn win_rate(&self) -> f64 {
        let decided = self.accepted_count + self.overridden_count;
        if decided == 0 {
            return 0.0;
        }
        self.accepted_count as f64 / decided as f64
    }

    /// Total decided outcomes recorded against this unit.
    pub fn decided(&self) -> u64 {
        self.accepted_count + self.overridden_count
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Staleness & decay (#224)
// ────────────────────────────────────────────────────────────────────────────

/// Default revalidation interval per content type, in seconds.
/// Patterns and accuracy stats live longer than insights or temporal observations
/// because the underlying behavior is more durable.
pub fn default_revalidation_interval(content: &KnowledgeContent) -> u64 {
    const DAY: u64 = 86_400;
    match content {
        // Durable preferences: 30 days
        KnowledgeContent::Pattern { .. }
        | KnowledgeContent::ToolAccuracy { .. }
        | KnowledgeContent::PromotedRule { .. }
        | KnowledgeContent::ApproachCluster { .. } => 30 * DAY,
        // Outcomes age moderately (workflows shift): 14 days
        KnowledgeContent::ApproachOutcome { .. } => 14 * DAY,
        // Time-of-day and friction insights: 7 days — they reflect current habits
        KnowledgeContent::Temporal { .. } | KnowledgeContent::Insight { .. } => 7 * DAY,
        // Shared artifacts: tied to versioned bodies; revalidate quarterly
        KnowledgeContent::Skill { .. }
        | KnowledgeContent::Command { .. }
        | KnowledgeContent::HookConfig { .. } => 90 * DAY,
    }
}

/// Effective confidence after time-based decay.
///
/// Decay is applied per overdue revalidation interval: each interval past
/// `last_validated_at + revalidation_interval_secs` multiplies confidence by 0.9.
/// Returns the original confidence when the unit is fresh (or interval is 0).
pub fn effective_confidence(unit: &KnowledgeUnit, now: u64) -> f64 {
    let interval = if unit.revalidation_interval_secs == 0 {
        default_revalidation_interval(&unit.content)
    } else {
        unit.revalidation_interval_secs
    };
    if interval == 0 {
        return unit.confidence;
    }
    let age = now.saturating_sub(unit.last_validated_at);
    if age <= interval {
        return unit.confidence;
    }
    // Number of full decay rounds: 1 round for ages in (interval, 2*interval],
    // 2 rounds for (2*interval, 3*interval], etc. `(age - 1) / interval` is
    // safe because age > interval ≥ 1 here, and avoids the off-by-one at
    // exact integer multiples.
    let overdue_intervals = (age - 1) / interval;
    const MAX_DECAY_ROUNDS: u32 = 50;
    let n = overdue_intervals.min(MAX_DECAY_ROUNDS as u64) as i32;
    unit.confidence * 0.9_f64.powi(n)
}

/// Whether the unit is past its revalidation deadline.
pub fn is_stale(unit: &KnowledgeUnit, now: u64) -> bool {
    let interval = if unit.revalidation_interval_secs == 0 {
        default_revalidation_interval(&unit.content)
    } else {
        unit.revalidation_interval_secs
    };
    if interval == 0 {
        return false;
    }
    now.saturating_sub(unit.last_validated_at) > interval
}

/// Scope determines where knowledge applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum KnowledgeScope {
    /// Applies to all projects and languages.
    Universal,
    /// Applies to a specific programming language.
    Language(String),
    /// Applies to a specific project (by slug).
    Project(String),
}

// ────────────────────────────────────────────────────────────────────────────
// Content size limits for shared artifacts
// ────────────────────────────────────────────────────────────────────────────

/// Maximum size for a shared skill body (32 KB).
pub const MAX_SKILL_BYTES: usize = 32 * 1024;
/// Maximum size for a shared command body (16 KB).
pub const MAX_COMMAND_BYTES: usize = 16 * 1024;
/// Maximum size for a shared hook config JSON (4 KB).
pub const MAX_HOOK_CONFIG_BYTES: usize = 4 * 1024;

/// Compatibility requirements for a shared artifact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtifactRequires {
    /// CLI binaries that must be on PATH (e.g., ["claudectl", "jq"]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cli: Vec<String>,
    /// Target OS labels (e.g., ["macos", "linux"]). Empty = any OS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    /// Minimum claudectl version (e.g., "0.42.0"). None = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
}

/// The actual knowledge payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnowledgeContent {
    /// A distilled preference pattern (from PreferencePattern).
    Pattern {
        tool: String,
        command_pattern: Option<String>,
        preferred_action: String,
        accept_rate: f64,
        sample_count: u32,
        conditions: Vec<String>,
    },
    /// Per-tool accuracy statistics (from ToolAccuracy).
    ToolAccuracy {
        tool: String,
        total: u32,
        correct: u32,
        confidence_threshold: f64,
    },
    /// A temporal behavior pattern (from TemporalPattern).
    Temporal { description: String, strength: f64 },
    /// A detected friction/error/cost insight (from Insight).
    Insight {
        category: String,
        severity: String,
        summary: String,
        suggestion: Option<String>,
    },
    /// A promoted rule from coord memory.
    PromotedRule { rule: String, source_type: String },
    /// A shared skill (markdown with YAML frontmatter).
    Skill {
        name: String,
        description: String,
        version: String,
        /// Full markdown content (frontmatter + body). Capped at MAX_SKILL_BYTES.
        body: String,
        /// Compatibility requirements (CLIs, OS, version).
        #[serde(default)]
        requires: ArtifactRequires,
    },
    /// A shared slash command (markdown with YAML frontmatter).
    Command {
        name: String,
        description: String,
        args: Option<String>,
        /// Full markdown content (frontmatter + body). Capped at MAX_COMMAND_BYTES.
        body: String,
        /// Compatibility requirements (CLIs, OS, version).
        #[serde(default)]
        requires: ArtifactRequires,
    },
    /// A shared hook configuration (declarative JSON, no executables).
    HookConfig {
        /// Hook event type (e.g., "PreToolUse", "PostToolUse").
        event: String,
        /// Matcher pattern (e.g., "Bash|Write|Edit").
        matcher: String,
        description: String,
        /// Sanitized hook config JSON (no secrets). Capped at MAX_HOOK_CONFIG_BYTES.
        config_json: String,
        /// Compatibility requirements (CLIs, OS, version).
        #[serde(default)]
        requires: ArtifactRequires,
    },
    /// Outcome statistics for a recurring approach (#220 baselining).
    /// `approach_ref` matches the semantic_key of the underlying Pattern/Rule/Skill
    /// so outcomes can be joined back to the approach they describe and shared
    /// across peers as evidence — not as a new approach itself.
    ApproachOutcome {
        approach_ref: String,
        success_rate: f64,
        sample_count: u32,
        #[serde(default)]
        median_cost_usd: Option<f64>,
        #[serde(default)]
        median_duration_ms: Option<u64>,
        #[serde(default)]
        conditions: Vec<String>,
    },
    /// A cluster of competing approaches to the same problem (#221).
    /// `problem_key` is a stable identifier (typically the underlying tool
    /// or canonical task name) that groups variants. Variants are not
    /// collapsed during merge — they are unioned across peers so the gate
    /// can present alternatives instead of one winning answer.
    ApproachCluster {
        problem_key: String,
        variants: Vec<ApproachVariant>,
    },
}

/// One competing approach within an `ApproachCluster`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproachVariant {
    /// Free-text summary the gate-time prompt shows to the LLM.
    pub approach_summary: String,
    /// When this variant tends to win (project, language, time-of-day, …).
    #[serde(default)]
    pub conditions: Vec<String>,
    /// How many decisions back this variant.
    pub evidence: u32,
    /// Peers that have contributed evidence for this variant.
    #[serde(default)]
    pub contributing_peers: Vec<String>,
    /// Optional pointer to an `ApproachOutcome` semantic_key for ranking.
    #[serde(default)]
    pub outcome_ref: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Semantic key — used for dedup and merge
// ────────────────────────────────────────────────────────────────────────────

/// Truncate a string to at most `max_chars` characters, safe for multi-byte UTF-8.
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Compute a semantic key for dedup/merge.
/// Same knowledge from different peers produces the same semantic key.
pub fn semantic_key(unit: &KnowledgeUnit) -> String {
    let scope_part = match &unit.scope {
        KnowledgeScope::Universal => "universal".to_string(),
        KnowledgeScope::Language(l) => format!("lang:{l}"),
        KnowledgeScope::Project(p) => format!("proj:{p}"),
    };
    let content_part = match &unit.content {
        KnowledgeContent::Pattern {
            tool,
            command_pattern,
            ..
        } => {
            let cmd = command_pattern.as_deref().unwrap_or("*");
            format!("pattern:{tool}:{cmd}")
        }
        KnowledgeContent::ToolAccuracy { tool, .. } => format!("accuracy:{tool}"),
        KnowledgeContent::Temporal { description, .. } => {
            format!("temporal:{}", truncate_chars(description, 40))
        }
        KnowledgeContent::Insight {
            category, summary, ..
        } => {
            format!("insight:{category}:{}", truncate_chars(summary, 40))
        }
        KnowledgeContent::PromotedRule { rule, .. } => {
            format!("rule:{}", truncate_chars(rule, 40))
        }
        KnowledgeContent::Skill { name, .. } => {
            format!("skill:{}", name.to_lowercase().replace(' ', "-"))
        }
        KnowledgeContent::Command { name, .. } => {
            format!("command:{}", name.to_lowercase())
        }
        KnowledgeContent::HookConfig { event, matcher, .. } => {
            format!("hook:{}:{}", event.to_lowercase(), matcher.to_lowercase())
        }
        KnowledgeContent::ApproachOutcome { approach_ref, .. } => {
            format!("outcome:{approach_ref}")
        }
        KnowledgeContent::ApproachCluster { problem_key, .. } => {
            format!("cluster:{problem_key}")
        }
    };
    format!("{scope_part}/{content_part}")
}

// ────────────────────────────────────────────────────────────────────────────
// Sharing filter — user-controlled exclusions
// ────────────────────────────────────────────────────────────────────────────

/// User-configurable filter for what knowledge to share.
/// Built from HiveConfig's share_categories, exclude_tools, exclude_commands.
#[derive(Debug, Clone, Default)]
pub struct SharingFilter {
    /// Allowed categories (empty = all shareable).
    pub allow_categories: Vec<String>,
    /// Tools to exclude (exact match on tool name).
    pub exclude_tools: Vec<String>,
    /// Command substrings to exclude.
    pub exclude_commands: Vec<String>,
    /// Content types to exclude from sharing (e.g., "skill", "command", "hook").
    pub exclude_content_types: Vec<String>,
}

impl SharingFilter {
    /// Build from HiveConfig.
    pub fn from_config(cfg: &crate::config::HiveConfig) -> Self {
        SharingFilter {
            allow_categories: cfg.share_categories.clone(),
            exclude_tools: cfg.exclude_tools.clone(),
            exclude_commands: cfg.exclude_commands.clone(),
            exclude_content_types: cfg.exclude_content_types.clone(),
        }
    }

    /// Check if a knowledge unit passes the user's sharing filter.
    pub fn allows(&self, unit: &KnowledgeUnit) -> bool {
        // Category check
        if !unit.category.is_allowed_by(&self.allow_categories) {
            return false;
        }

        // Tool exclusion
        if let KnowledgeContent::Pattern {
            ref tool,
            ref command_pattern,
            ..
        } = unit.content
        {
            if self.exclude_tools.iter().any(|t| t == tool) {
                return false;
            }
            if let Some(cmd) = command_pattern {
                if self
                    .exclude_commands
                    .iter()
                    .any(|exc| cmd.contains(exc.as_str()))
                {
                    return false;
                }
            }
        }

        // ToolAccuracy tool exclusion
        if let KnowledgeContent::ToolAccuracy { ref tool, .. } = unit.content {
            if self.exclude_tools.iter().any(|t| t == tool) {
                return false;
            }
        }

        // Content type exclusions for shared artifacts
        match &unit.content {
            KnowledgeContent::Skill { .. }
                if self.exclude_content_types.iter().any(|t| t == "skill") =>
            {
                return false;
            }
            KnowledgeContent::Command { name, .. } => {
                if self.exclude_content_types.iter().any(|t| t == "command") {
                    return false;
                }
                if self
                    .exclude_commands
                    .iter()
                    .any(|exc| name.contains(exc.as_str()))
                {
                    return false;
                }
            }
            KnowledgeContent::HookConfig { .. }
                if self.exclude_content_types.iter().any(|t| t == "hook") =>
            {
                return false;
            }
            _ => {}
        }

        true
    }
}

// ────────────────────────────────────────────────────────────────────────────
// ID generation
// ────────────────────────────────────────────────────────────────────────────

static KU_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn gen_ku_id() -> String {
    let epoch = epoch_secs();
    let seq = KU_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ku_{epoch}_{seq}")
}

pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ────────────────────────────────────────────────────────────────────────────
// On/off override (mirrors brain's gate-mode pattern)
// ────────────────────────────────────────────────────────────────────────────

/// Path to the hive mode override file (`~/.claudectl/hive/mode`).
pub fn mode_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("mode")
}

/// Read the mode override. Returns `Some("on")`, `Some("off")`, or `None` when
/// the file is absent (i.e., fall back to config).
pub fn read_mode_override() -> Option<String> {
    let path = mode_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim().to_lowercase();
    if trimmed == "on" || trimmed == "off" {
        Some(trimmed)
    } else {
        None
    }
}

/// Set the mode override. Pass `"on"` or `"off"` to force; `"clear"` removes the
/// override and falls back to the config flag.
pub fn write_mode_override(mode: &str) -> std::io::Result<()> {
    let path = mode_path();
    if mode == "clear" {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, mode)
}

/// True if the hive should be active. The mode file (when present) overrides
/// the config flag. When both are absent, hive is inactive.
pub fn is_active(cfg: Option<&crate::config::HiveConfig>) -> bool {
    match read_mode_override().as_deref() {
        Some("on") => true,
        Some("off") => false,
        _ => cfg.map(|c| c.enabled).unwrap_or(false),
    }
}

/// Local identity for hive knowledge (used when relay feature is not enabled).
/// Returns hostname or "local" as a fallback.
pub fn local_identity() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        })
        .unwrap_or_else(|| "local".into())
}

// ────────────────────────────────────────────────────────────────────────────
// Broadcast channel for triggering gossip after distillation (relay only)
// ────────────────────────────────────────────────────────────────────────────

#[cfg(feature = "relay")]
use std::sync::Mutex;

#[cfg(feature = "relay")]
static HIVE_BROADCAST_TX: Mutex<Option<std::sync::mpsc::Sender<u32>>> = Mutex::new(None);

/// Set the broadcast channel (called once during relay startup).
#[cfg(feature = "relay")]
pub fn set_broadcast_channel(tx: std::sync::mpsc::Sender<u32>) {
    if let Ok(mut guard) = HIVE_BROADCAST_TX.lock() {
        *guard = Some(tx);
    }
}

/// Signal that new knowledge units are available for gossip.
#[cfg(feature = "relay")]
pub fn signal_new_knowledge(count: u32) {
    if let Ok(guard) = HIVE_BROADCAST_TX.lock() {
        if let Some(ref tx) = *guard {
            let _ = tx.send(count);
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Hook config sanitization
// ────────────────────────────────────────────────────────────────────────────

/// Environment variables safe to keep in shared hook configs.
const SAFE_ENV_VARS: &[&str] = &["HOME", "PWD", "PATH", "CLAUDE_PLUGIN_ROOT", "USER", "SHELL"];

/// Credential-like key prefixes (case-insensitive match).
const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "auth_token",
    "access_key",
    "private_key",
];

/// Sanitize a hook config string before sharing: strip credentials, unsafe env
/// vars, and absolute user paths. Returns the sanitized version.
pub fn sanitize_hook_config(input: &str) -> String {
    let mut lines: Vec<String> = Vec::new();

    for line in input.lines() {
        let mut sanitized = sanitize_credentials(line);
        sanitized = sanitize_env_vars(&sanitized);
        sanitized = sanitize_user_paths(&sanitized);
        lines.push(sanitized);
    }

    lines.join("\n")
}

/// Replace credential-like `key=value`, `key: value`, `"key": "value"` patterns.
fn sanitize_credentials(line: &str) -> String {
    let lower = line.to_lowercase();
    for key in CREDENTIAL_KEYS {
        // Check for JSON-style: "key": "value" or "key":"value"
        if lower.contains(key) {
            // Check "key": "value" pattern
            if let Some(pos) = lower.find(key) {
                let after_key = &line[pos + key.len()..];
                let after_key_trimmed = after_key.trim_start_matches('"').trim_start();
                if after_key_trimmed.starts_with(':') || after_key_trimmed.starts_with('=') {
                    let sep_char = if after_key_trimmed.starts_with(':') {
                        ':'
                    } else {
                        '='
                    };
                    let before = &line[..pos];
                    // Find the key boundary (include quotes if present)
                    let key_start = if before.ends_with('"') {
                        before.len() - 1
                    } else {
                        pos
                    };
                    let after_sep = &after_key_trimmed[1..].trim_start();
                    let redacted = if let Some(stripped) = after_sep.strip_prefix('"') {
                        // JSON string value — find closing quote
                        if let Some(end) = stripped.find('"') {
                            let rest_start =
                                pos + key.len() + (after_key.len() - after_sep.len()) + 2 + end;
                            format!(
                                "{}\"{key}\"{sep_char} \"REDACTED\"{}",
                                &line[..key_start],
                                &line[rest_start..]
                            )
                        } else {
                            format!("{}\"{key}\"{sep_char} \"REDACTED\"", &line[..key_start])
                        }
                    } else {
                        // Unquoted value — redact to end of token
                        let end = after_sep
                            .find(|c: char| c.is_whitespace() || c == ',' || c == '}')
                            .unwrap_or(after_sep.len());
                        let rest_start =
                            pos + key.len() + (after_key.len() - after_sep.len()) + end;
                        format!(
                            "{}{}{}REDACTED{}",
                            &line[..pos],
                            key,
                            sep_char,
                            &line[rest_start..]
                        )
                    };
                    return redacted;
                }
            }
        }
    }
    line.to_string()
}

/// Replace `$VAR` and `${VAR}` references with `$REDACTED` / `${REDACTED}`
/// unless the variable name is in the safe list.
fn sanitize_env_vars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            if chars[i + 1] == '{' {
                // ${VAR} form
                if let Some(close) = chars[i + 2..].iter().position(|&c| c == '}') {
                    let var_name: String = chars[i + 2..i + 2 + close].iter().collect();
                    if is_env_var_name(&var_name) {
                        if SAFE_ENV_VARS.contains(&var_name.as_str()) {
                            result.push_str(&format!("${{{var_name}}}"));
                        } else {
                            result.push_str("${REDACTED}");
                        }
                        i += 3 + close; // skip past }
                        continue;
                    }
                }
            } else if chars[i + 1].is_ascii_uppercase() || chars[i + 1] == '_' {
                // $VAR form
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_')
                {
                    end += 1;
                }
                let var_name: String = chars[start..end].iter().collect();
                if is_env_var_name(&var_name) {
                    if SAFE_ENV_VARS.contains(&var_name.as_str()) {
                        result.push('$');
                        result.push_str(&var_name);
                    } else {
                        result.push_str("$REDACTED");
                    }
                    i = end;
                    continue;
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Check if a string looks like an env var name (uppercase + underscores + digits).
fn is_env_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Replace `/Users/<name>` and `/home/<name>` with `$HOME`.
fn sanitize_user_paths(input: &str) -> String {
    let mut result = input.to_string();
    for prefix in &["/Users/", "/home/"] {
        while let Some(pos) = result.find(prefix) {
            let after = &result[pos + prefix.len()..];
            let username_end = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '_')
                .unwrap_or(after.len());
            if username_end > 0 {
                let end = pos + prefix.len() + username_end;
                result = format!("{}$HOME{}", &result[..pos], &result[end..]);
            } else {
                break;
            }
        }
    }
    result
}

// ────────────────────────────────────────────────────────────────────────────
// Compatibility checking
// ────────────────────────────────────────────────────────────────────────────

/// A compatibility issue found when checking an artifact against the local environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatIssue {
    /// A required CLI binary is not on PATH.
    MissingCli(String),
    /// The artifact targets a different OS.
    WrongOs {
        current: String,
        required: Vec<String>,
    },
    /// The local claudectl version is too old.
    VersionTooOld { current: String, required: String },
}

impl CompatIssue {
    /// Short label for display in the COMPAT column.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::MissingCli(_) => "!cli",
            Self::WrongOs { .. } => "!os",
            Self::VersionTooOld { .. } => "!ver",
        }
    }

    /// Whether this issue should block installation (vs just warn).
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::WrongOs { .. } | Self::VersionTooOld { .. })
    }
}

impl std::fmt::Display for CompatIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCli(cli) => write!(f, "missing CLI: {cli}"),
            Self::WrongOs { current, required } => {
                write!(
                    f,
                    "OS mismatch: running {current}, requires {}",
                    required.join("/")
                )
            }
            Self::VersionTooOld { current, required } => {
                write!(f, "claudectl {current} too old, requires >= {required}")
            }
        }
    }
}

/// Check an artifact's requirements against the local environment.
pub fn check_compatibility(requires: &ArtifactRequires) -> Vec<CompatIssue> {
    let mut issues = Vec::new();

    for cli in &requires.cli {
        if !is_cli_available(cli) {
            issues.push(CompatIssue::MissingCli(cli.clone()));
        }
    }

    if !requires.os.is_empty() {
        let current = current_os_label();
        if !requires.os.iter().any(|o| o == current) {
            issues.push(CompatIssue::WrongOs {
                current: current.to_string(),
                required: requires.os.clone(),
            });
        }
    }

    if let Some(min_ver) = &requires.min_version {
        let current = env!("CARGO_PKG_VERSION");
        if !version_gte(current, min_ver) {
            issues.push(CompatIssue::VersionTooOld {
                current: current.to_string(),
                required: min_ver.clone(),
            });
        }
    }

    issues
}

/// Check if a CLI binary is available on PATH.
pub fn is_cli_available(name: &str) -> bool {
    // Reject names with path separators or shell metacharacters
    if name.contains('/') || name.contains('\\') || name.contains(';') || name.is_empty() {
        return false;
    }
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Current OS label for compatibility matching.
pub fn current_os_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

/// Simple semver comparison: is `current` >= `required`?
/// Compares major.minor.patch numerically. Non-numeric parts are treated as 0.
fn version_gte(current: &str, required: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        let parts: Vec<u32> = s
            .split('.')
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        (
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
            parts.get(2).copied().unwrap_or(0),
        )
    };
    parse(current) >= parse(required)
}

/// Shell builtins and common shell syntax tokens to exclude from CLI detection.
const SHELL_BUILTINS: &[&str] = &[
    "if", "then", "else", "fi", "for", "do", "done", "while", "until", "case", "esac", "in",
    "echo", "printf", "cd", "export", "set", "unset", "local", "return", "exit", "source", ".",
    "true", "false", "test", "[", "[[", "read", "shift", "eval", "exec", "trap", "wait", "sleep",
    "cat", "head", "tail", "grep", "sed", "awk", "tr", "cut", "sort", "uniq", "wc", "tee", "mkdir",
    "rm", "cp", "mv", "ls", "touch", "chmod", "chown", "ln", "find", "xargs",
];

/// Detect CLI dependencies by scanning bash code blocks in a markdown body.
pub fn detect_cli_deps(body: &str) -> Vec<String> {
    let mut deps = std::collections::BTreeSet::new();
    let mut in_bash_block = false;

    for line in body.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("```bash") || trimmed.starts_with("```sh") {
            in_bash_block = true;
            continue;
        }
        if trimmed.starts_with("```") && in_bash_block {
            in_bash_block = false;
            continue;
        }

        if !in_bash_block {
            continue;
        }

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Extract the first token (the command being run)
        // Handle pipes: each segment after | is a new command
        for segment in trimmed.split('|') {
            let segment = segment.trim();
            // Strip leading env var assignments (KEY=val cmd)
            let cmd_part = skip_env_assignments(segment);
            if let Some(cmd) = cmd_part.split_whitespace().next() {
                // Strip path prefix if present
                let base = cmd.rsplit('/').next().unwrap_or(cmd);
                if !base.is_empty()
                    && !SHELL_BUILTINS.contains(&base)
                    && base
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    deps.insert(base.to_string());
                }
            }
        }
    }

    deps.into_iter().collect()
}

/// Skip leading `KEY=value` assignments to find the actual command.
fn skip_env_assignments(s: &str) -> &str {
    let mut rest = s;
    loop {
        let trimmed = rest.trim_start();
        // Check for KEY=... pattern (uppercase start, has =)
        if let Some(eq_pos) = trimmed.find('=') {
            let key = &trimmed[..eq_pos];
            if !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                // Skip past the value
                let after_eq = &trimmed[eq_pos + 1..];
                // Quoted value
                if let Some(inner) = after_eq.strip_prefix('"') {
                    if let Some(end) = inner.find('"') {
                        rest = &inner[end + 1..];
                        continue;
                    }
                } else if let Some(inner) = after_eq.strip_prefix('\'') {
                    if let Some(end) = inner.find('\'') {
                        rest = &inner[end + 1..];
                        continue;
                    }
                } else {
                    // Unquoted — next whitespace
                    let end = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
                    rest = &after_eq[end..];
                    continue;
                }
            }
        }
        return trimmed;
    }
}

/// Detect OS requirements from body content heuristics.
pub fn detect_os_deps(body: &str) -> Vec<String> {
    let mut os_set = std::collections::BTreeSet::new();

    let lower = body.to_lowercase();

    // macOS signals
    if lower.contains("brew ") || lower.contains("brew install") || lower.contains("/usr/local/bin")
    {
        os_set.insert("macos".to_string());
    }

    // Linux signals
    if lower.contains("apt-get")
        || lower.contains("apt install")
        || lower.contains("systemctl")
        || lower.contains("yum install")
        || lower.contains("dnf install")
    {
        os_set.insert("linux".to_string());
    }

    os_set.into_iter().collect()
}

/// Get the `requires` field from a KnowledgeContent, if it's an artifact type.
pub fn get_requires(content: &KnowledgeContent) -> Option<&ArtifactRequires> {
    match content {
        KnowledgeContent::Skill { requires, .. }
        | KnowledgeContent::Command { requires, .. }
        | KnowledgeContent::HookConfig { requires, .. } => Some(requires),
        _ => None,
    }
}

/// Compute a short compatibility label for display.
/// Returns "ok", "!cli", "!os", "!ver", or "?" (no requirements declared).
pub fn compat_label(content: &KnowledgeContent) -> &'static str {
    match get_requires(content) {
        None => "—",
        Some(req) if req.cli.is_empty() && req.os.is_empty() && req.min_version.is_none() => "?",
        Some(req) => {
            let issues = check_compatibility(req);
            if issues.is_empty() {
                "ok"
            } else {
                // Return the first (most important) issue label
                issues[0].short_label()
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Display helpers
// ────────────────────────────────────────────────────────────────────────────

impl KnowledgeContent {
    /// One-line summary for display.
    pub fn summary_line(&self) -> String {
        match self {
            Self::Pattern {
                tool,
                command_pattern,
                preferred_action,
                accept_rate,
                ..
            } => {
                let cmd = command_pattern.as_deref().unwrap_or("*");
                format!(
                    "[{tool}, {cmd}] {preferred_action} ({:.0}%)",
                    accept_rate * 100.0
                )
            }
            Self::ToolAccuracy {
                tool,
                total,
                correct,
                ..
            } => {
                let pct = if *total > 0 {
                    (*correct as f64 / *total as f64) * 100.0
                } else {
                    0.0
                };
                format!("[{tool}] accuracy {correct}/{total} ({pct:.0}%)")
            }
            Self::Temporal {
                description,
                strength,
                ..
            } => {
                format!("temporal: {description} (strength {strength:.2})")
            }
            Self::Insight {
                category,
                severity,
                summary,
                ..
            } => {
                format!("[{severity}] {category}: {summary}")
            }
            Self::PromotedRule { rule, .. } => {
                format!("rule: {rule}")
            }
            Self::Skill {
                name,
                version,
                body,
                ..
            } => {
                format!("skill: {name} v{version} ({} bytes)", body.len())
            }
            Self::Command { name, args, .. } => {
                let arg_str = args.as_deref().unwrap_or("");
                if arg_str.is_empty() {
                    format!("command: /{name}")
                } else {
                    format!("command: /{name} {arg_str}")
                }
            }
            Self::HookConfig {
                event,
                matcher,
                description,
                ..
            } => {
                format!("hook: {event}[{matcher}] — {description}")
            }
            Self::ApproachOutcome {
                approach_ref,
                success_rate,
                sample_count,
                ..
            } => {
                format!(
                    "outcome: {approach_ref} — {:.0}% over {sample_count}",
                    success_rate * 100.0
                )
            }
            Self::ApproachCluster {
                problem_key,
                variants,
            } => {
                format!("cluster: {problem_key} ({} variants)", variants.len())
            }
        }
    }
}

impl std::fmt::Display for KnowledgeScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Universal => write!(f, "universal"),
            Self::Language(l) => write!(f, "language:{l}"),
            Self::Project(p) => write!(f, "project:{p}"),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pattern_unit(tool: &str, cmd: Option<&str>) -> KnowledgeUnit {
        KnowledgeUnit {
            id: "ku_1".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Pattern {
                tool: tool.into(),
                command_pattern: cmd.map(|s| s.into()),
                preferred_action: "approve".into(),
                accept_rate: 0.95,
                sample_count: 20,
                conditions: vec![],
            },
            evidence_count: 20,
            confidence: 0.95,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        }
    }

    #[test]
    fn semantic_key_pattern() {
        let unit = sample_pattern_unit("Bash", Some("cargo test"));
        assert_eq!(semantic_key(&unit), "universal/pattern:Bash:cargo test");
    }

    #[test]
    fn semantic_key_pattern_no_command() {
        let unit = sample_pattern_unit("Read", None);
        assert_eq!(semantic_key(&unit), "universal/pattern:Read:*");
    }

    #[test]
    fn semantic_key_with_scope() {
        let mut unit = sample_pattern_unit("Bash", Some("cargo fmt"));
        unit.scope = KnowledgeScope::Language("rust".into());
        assert_eq!(semantic_key(&unit), "lang:rust/pattern:Bash:cargo fmt");

        unit.scope = KnowledgeScope::Project("claudectl".into());
        assert_eq!(semantic_key(&unit), "proj:claudectl/pattern:Bash:cargo fmt");
    }

    #[test]
    fn semantic_key_accuracy() {
        let unit = KnowledgeUnit {
            id: "ku_2".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::ToolAccuracy {
                tool: "Bash".into(),
                total: 100,
                correct: 85,
                confidence_threshold: 0.7,
            },
            evidence_count: 100,
            confidence: 0.85,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        };
        assert_eq!(semantic_key(&unit), "universal/accuracy:Bash");
    }

    #[test]
    fn knowledge_unit_serde_roundtrip() {
        let unit = sample_pattern_unit("Bash", Some("cargo test"));
        let json = serde_json::to_string(&unit).unwrap();
        let back: KnowledgeUnit = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "ku_1");
        assert_eq!(back.confidence, 0.95);
        assert_eq!(back.source_peer, "peer-a");
    }

    #[test]
    fn content_summary_line() {
        let unit = sample_pattern_unit("Bash", Some("cargo test"));
        let line = unit.content.summary_line();
        assert!(line.contains("Bash"));
        assert!(line.contains("cargo test"));
        assert!(line.contains("approve"));
    }

    #[test]
    fn scope_display() {
        assert_eq!(KnowledgeScope::Universal.to_string(), "universal");
        assert_eq!(
            KnowledgeScope::Language("rust".into()).to_string(),
            "language:rust"
        );
        assert_eq!(
            KnowledgeScope::Project("foo".into()).to_string(),
            "project:foo"
        );
    }

    #[test]
    fn gen_ku_id_unique() {
        let a = gen_ku_id();
        let b = gen_ku_id();
        assert_ne!(a, b);
        assert!(a.starts_with("ku_"));
    }

    #[test]
    fn semantic_key_multibyte_utf8_no_panic() {
        // CJK characters are 3 bytes each — 14 chars = 42 bytes, truncation at
        // char boundary 40 would panic with byte-offset slicing
        let long_cjk = "这是一个用来测试多字节截断的临时模式描述文本超长";
        let unit = KnowledgeUnit {
            id: "ku_utf8".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Temporal {
                description: long_cjk.to_string(),
                strength: 0.9,
            },
            evidence_count: 5,
            confidence: 0.9,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        };
        // Must not panic — truncates at char boundary, not byte boundary
        let key = semantic_key(&unit);
        assert!(key.starts_with("universal/temporal:"));
    }

    #[test]
    fn semantic_key_emoji_no_panic() {
        let emoji_text = "Error streak detected in tests 🎉🎊🎈🎁🎆🎇 more text here";
        let unit = KnowledgeUnit {
            id: "ku_emoji".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Insight {
                category: "error_loop".into(),
                severity: "warning".into(),
                summary: emoji_text.to_string(),
                suggestion: None,
            },
            evidence_count: 3,
            confidence: 0.7,
            source_peer: "peer-b".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        };
        let key = semantic_key(&unit);
        assert!(key.starts_with("universal/insight:error_loop:"));
    }

    // ── New content type tests ─────────────────────────────────────────

    fn sample_skill_unit() -> KnowledgeUnit {
        KnowledgeUnit {
            id: "ku_skill_1".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::Technique,
            content: KnowledgeContent::Skill {
                name: "Session Monitoring".into(),
                description: "Monitors sessions".into(),
                version: "0.31.0".into(),
                body: "---\nname: Session Monitoring\n---\nContent here".into(),
                requires: ArtifactRequires::default(),
            },
            evidence_count: 1,
            confidence: 1.0,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        }
    }

    fn sample_command_unit() -> KnowledgeUnit {
        KnowledgeUnit {
            id: "ku_cmd_1".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::Technique,
            content: KnowledgeContent::Command {
                name: "brain".into(),
                description: "Toggle brain gate".into(),
                args: Some("[on|off|auto|status]".into()),
                body: "---\nname: brain\n---\nContent".into(),
                requires: ArtifactRequires::default(),
            },
            evidence_count: 1,
            confidence: 1.0,
            source_peer: "peer-b".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        }
    }

    fn sample_hook_unit() -> KnowledgeUnit {
        KnowledgeUnit {
            id: "ku_hook_1".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::WorkflowPattern,
            content: KnowledgeContent::HookConfig {
                event: "PreToolUse".into(),
                matcher: "Bash|Write|Edit".into(),
                description: "Brain gate hook".into(),
                config_json: r#"{"command": "brain-gate.sh", "timeout": 5000}"#.into(),
                requires: ArtifactRequires::default(),
            },
            evidence_count: 1,
            confidence: 1.0,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: crate::hive::InjectionState::Live,
            injection_stats: crate::hive::InjectionStats {
                injected_count: 0,
                accepted_count: 0,
                overridden_count: 0,
                last_injected_at: 0,
                last_outcome_at: 0,
            },
        }
    }

    #[test]
    fn semantic_key_skill() {
        let unit = sample_skill_unit();
        assert_eq!(semantic_key(&unit), "universal/skill:session-monitoring");
    }

    #[test]
    fn semantic_key_command() {
        let unit = sample_command_unit();
        assert_eq!(semantic_key(&unit), "universal/command:brain");
    }

    #[test]
    fn semantic_key_hook() {
        let unit = sample_hook_unit();
        assert_eq!(
            semantic_key(&unit),
            "universal/hook:pretooluse:bash|write|edit"
        );
    }

    #[test]
    fn summary_line_skill() {
        let unit = sample_skill_unit();
        let line = unit.content.summary_line();
        assert!(line.contains("Session Monitoring"));
        assert!(line.contains("v0.31.0"));
        assert!(line.contains("bytes"));
    }

    #[test]
    fn summary_line_command() {
        let unit = sample_command_unit();
        let line = unit.content.summary_line();
        assert!(line.contains("/brain"));
        assert!(line.contains("[on|off|auto|status]"));
    }

    #[test]
    fn summary_line_command_no_args() {
        let content = KnowledgeContent::Command {
            name: "sessions".into(),
            description: "List sessions".into(),
            args: None,
            body: "body".into(),
            requires: ArtifactRequires::default(),
        };
        let line = content.summary_line();
        assert_eq!(line, "command: /sessions");
    }

    #[test]
    fn summary_line_hook() {
        let unit = sample_hook_unit();
        let line = unit.content.summary_line();
        assert!(line.contains("PreToolUse"));
        assert!(line.contains("Bash|Write|Edit"));
    }

    #[test]
    fn serde_roundtrip_skill() {
        let unit = sample_skill_unit();
        let json = serde_json::to_string(&unit).unwrap();
        let back: KnowledgeUnit = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "ku_skill_1");
        if let KnowledgeContent::Skill { name, version, .. } = &back.content {
            assert_eq!(name, "Session Monitoring");
            assert_eq!(version, "0.31.0");
        } else {
            panic!("expected Skill variant");
        }
    }

    #[test]
    fn serde_roundtrip_command() {
        let unit = sample_command_unit();
        let json = serde_json::to_string(&unit).unwrap();
        let back: KnowledgeUnit = serde_json::from_str(&json).unwrap();
        if let KnowledgeContent::Command { name, args, .. } = &back.content {
            assert_eq!(name, "brain");
            assert_eq!(args.as_deref(), Some("[on|off|auto|status]"));
        } else {
            panic!("expected Command variant");
        }
    }

    #[test]
    fn serde_roundtrip_hook() {
        let unit = sample_hook_unit();
        let json = serde_json::to_string(&unit).unwrap();
        let back: KnowledgeUnit = serde_json::from_str(&json).unwrap();
        if let KnowledgeContent::HookConfig { event, matcher, .. } = &back.content {
            assert_eq!(event, "PreToolUse");
            assert_eq!(matcher, "Bash|Write|Edit");
        } else {
            panic!("expected HookConfig variant");
        }
    }

    #[test]
    fn sharing_filter_excludes_skills() {
        let filter = SharingFilter {
            exclude_content_types: vec!["skill".into()],
            ..Default::default()
        };
        let unit = sample_skill_unit();
        assert!(!filter.allows(&unit));

        // Commands should still be allowed
        let cmd = sample_command_unit();
        assert!(filter.allows(&cmd));
    }

    #[test]
    fn sharing_filter_excludes_commands() {
        let filter = SharingFilter {
            exclude_content_types: vec!["command".into()],
            ..Default::default()
        };
        let cmd = sample_command_unit();
        assert!(!filter.allows(&cmd));

        // Skills should still be allowed
        let skill = sample_skill_unit();
        assert!(filter.allows(&skill));
    }

    #[test]
    fn sharing_filter_excludes_hooks() {
        let filter = SharingFilter {
            exclude_content_types: vec!["hook".into()],
            ..Default::default()
        };
        let hook = sample_hook_unit();
        assert!(!filter.allows(&hook));
    }

    #[test]
    fn sharing_filter_allows_all_by_default() {
        let filter = SharingFilter::default();
        assert!(filter.allows(&sample_skill_unit()));
        assert!(filter.allows(&sample_command_unit()));
        assert!(filter.allows(&sample_hook_unit()));
    }

    #[test]
    fn sharing_filter_command_name_exclusion() {
        let filter = SharingFilter {
            exclude_commands: vec!["brain".into()],
            ..Default::default()
        };
        let cmd = sample_command_unit();
        assert!(!filter.allows(&cmd));
    }

    // ── Sanitization tests ─────────────────────────────────────────────

    #[test]
    fn sanitize_strips_env_vars() {
        let input = "cmd: $API_KEY and ${SECRET_TOKEN}";
        let result = sanitize_hook_config(input);
        assert!(result.contains("$REDACTED"));
        assert!(result.contains("${REDACTED}"));
        assert!(!result.contains("API_KEY"));
        assert!(!result.contains("SECRET_TOKEN"));
    }

    #[test]
    fn sanitize_keeps_safe_vars() {
        let input = "path: $HOME/.claudectl and ${CLAUDE_PLUGIN_ROOT}/hooks";
        let result = sanitize_hook_config(input);
        assert!(result.contains("$HOME"));
        assert!(result.contains("${CLAUDE_PLUGIN_ROOT}"));
    }

    #[test]
    fn sanitize_strips_absolute_paths() {
        let input = "file: /Users/barada/.claudectl/config";
        let result = sanitize_hook_config(input);
        assert!(result.contains("$HOME"));
        assert!(!result.contains("/Users/barada"));
    }

    #[test]
    fn sanitize_strips_home_paths() {
        let input = "file: /home/ubuntu/.config/thing";
        let result = sanitize_hook_config(input);
        assert!(result.contains("$HOME"));
        assert!(!result.contains("/home/ubuntu"));
    }

    #[test]
    fn sanitize_strips_credentials() {
        let input = r#""api_key": "sk-1234567890""#;
        let result = sanitize_hook_config(input);
        assert!(result.contains("REDACTED"));
        assert!(!result.contains("sk-1234567890"));
    }

    #[test]
    fn sanitize_strips_unquoted_credentials() {
        let input = "token=abc123def";
        let result = sanitize_hook_config(input);
        assert!(result.contains("REDACTED"));
        assert!(!result.contains("abc123def"));
    }

    #[test]
    fn sanitize_preserves_safe_content() {
        let input = r#"{"command": "brain-gate.sh", "timeout": 5000}"#;
        let result = sanitize_hook_config(input);
        assert_eq!(result, input);
    }

    // ── Compatibility tests ────────────────────────────────────────────

    #[test]
    fn artifact_requires_serde_roundtrip() {
        let req = ArtifactRequires {
            cli: vec!["claudectl".into(), "jq".into()],
            os: vec!["macos".into()],
            min_version: Some("0.42.0".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ArtifactRequires = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cli, vec!["claudectl", "jq"]);
        assert_eq!(back.os, vec!["macos"]);
        assert_eq!(back.min_version.as_deref(), Some("0.42.0"));
    }

    #[test]
    fn artifact_requires_default_empty() {
        let req = ArtifactRequires::default();
        assert!(req.cli.is_empty());
        assert!(req.os.is_empty());
        assert!(req.min_version.is_none());
    }

    #[test]
    fn artifact_requires_skips_empty_on_serialize() {
        let req = ArtifactRequires::default();
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn artifact_requires_backward_compat_deserialize() {
        // Old units without `requires` should deserialize with empty default
        let json =
            r#"{"type":"skill","name":"Test","description":"Test","version":"1.0","body":"x"}"#;
        let content: KnowledgeContent = serde_json::from_str(json).unwrap();
        if let KnowledgeContent::Skill { requires, .. } = content {
            assert!(requires.cli.is_empty());
            assert!(requires.os.is_empty());
        } else {
            panic!("expected Skill");
        }
    }

    #[test]
    fn version_gte_basic() {
        assert!(version_gte("0.42.0", "0.42.0"));
        assert!(version_gte("0.43.0", "0.42.0"));
        assert!(version_gte("1.0.0", "0.99.99"));
        assert!(!version_gte("0.41.0", "0.42.0"));
        assert!(!version_gte("0.42.0", "0.42.1"));
    }

    #[test]
    fn version_gte_partial() {
        assert!(version_gte("1.0", "0.42.0"));
        assert!(version_gte("1", "0"));
    }

    #[test]
    fn current_os_label_not_unknown() {
        let label = current_os_label();
        assert!(label == "macos" || label == "linux" || label == "windows");
    }

    #[test]
    fn is_cli_available_rejects_path_traversal() {
        assert!(!is_cli_available("../../../etc/passwd"));
        assert!(!is_cli_available("foo;rm -rf /"));
        assert!(!is_cli_available(""));
    }

    #[test]
    fn detect_cli_deps_basic() {
        let body = r#"
# Usage

```bash
claudectl --list
jq '.sessions[]' output.json
cargo test --all
```

Some text.
"#;
        let deps = detect_cli_deps(body);
        assert!(deps.contains(&"claudectl".to_string()));
        assert!(deps.contains(&"jq".to_string()));
        assert!(deps.contains(&"cargo".to_string()));
    }

    #[test]
    fn detect_cli_deps_skips_builtins() {
        let body = r#"
```bash
echo "hello"
cd /tmp
export FOO=bar
if [ -f test ]; then echo ok; fi
```
"#;
        let deps = detect_cli_deps(body);
        assert!(deps.is_empty());
    }

    #[test]
    fn detect_cli_deps_handles_pipes() {
        let body = r#"
```bash
claudectl --json | jq '.[] | .cost'
```
"#;
        let deps = detect_cli_deps(body);
        assert!(deps.contains(&"claudectl".to_string()));
        assert!(deps.contains(&"jq".to_string()));
    }

    #[test]
    fn detect_cli_deps_skips_non_bash_blocks() {
        let body = r#"
```python
import json
```

```bash
claudectl --list
```

```rust
fn main() {}
```
"#;
        let deps = detect_cli_deps(body);
        assert_eq!(deps, vec!["claudectl"]);
    }

    #[test]
    fn detect_cli_deps_handles_env_assignments() {
        let body = r#"
```bash
FOO=bar claudectl --list
```
"#;
        let deps = detect_cli_deps(body);
        assert!(deps.contains(&"claudectl".to_string()));
    }

    #[test]
    fn detect_os_deps_macos() {
        let body = "Install with: brew install claudectl";
        assert_eq!(detect_os_deps(body), vec!["macos"]);
    }

    #[test]
    fn detect_os_deps_linux() {
        let body = "Install with: apt-get install tool";
        assert_eq!(detect_os_deps(body), vec!["linux"]);
    }

    #[test]
    fn detect_os_deps_none() {
        let body = "Just run claudectl";
        assert!(detect_os_deps(body).is_empty());
    }

    #[test]
    fn compat_issue_display() {
        let issue = CompatIssue::MissingCli("jq".into());
        assert_eq!(format!("{issue}"), "missing CLI: jq");
        assert_eq!(issue.short_label(), "!cli");
        assert!(!issue.is_blocking());

        let issue = CompatIssue::WrongOs {
            current: "macos".into(),
            required: vec!["linux".into()],
        };
        assert!(issue.is_blocking());
        assert_eq!(issue.short_label(), "!os");
    }

    #[test]
    fn check_compatibility_no_requirements() {
        let req = ArtifactRequires::default();
        assert!(check_compatibility(&req).is_empty());
    }

    #[test]
    fn check_compatibility_missing_cli() {
        let req = ArtifactRequires {
            cli: vec!["this_binary_surely_does_not_exist_xyz".into()],
            ..Default::default()
        };
        let issues = check_compatibility(&req);
        assert_eq!(issues.len(), 1);
        assert!(
            matches!(&issues[0], CompatIssue::MissingCli(s) if s == "this_binary_surely_does_not_exist_xyz")
        );
    }

    #[test]
    fn check_compatibility_version_ok() {
        let req = ArtifactRequires {
            min_version: Some("0.1.0".into()),
            ..Default::default()
        };
        // Current version is always >= 0.1.0
        assert!(check_compatibility(&req).is_empty());
    }

    #[test]
    fn check_compatibility_version_too_high() {
        let req = ArtifactRequires {
            min_version: Some("99.99.99".into()),
            ..Default::default()
        };
        let issues = check_compatibility(&req);
        assert_eq!(issues.len(), 1);
        assert!(matches!(&issues[0], CompatIssue::VersionTooOld { .. }));
    }

    #[test]
    fn get_requires_returns_none_for_pattern() {
        let unit = sample_pattern_unit("Bash", Some("test"));
        assert!(get_requires(&unit.content).is_none());
    }

    #[test]
    fn get_requires_returns_some_for_skill() {
        let unit = sample_skill_unit();
        assert!(get_requires(&unit.content).is_some());
    }

    #[test]
    fn compat_label_no_requirements() {
        let unit = sample_skill_unit();
        // Default requires is empty — should show "?"
        assert_eq!(compat_label(&unit.content), "?");
    }

    // ── Staleness / decay (#224) ───────────────────────────────────────

    #[test]
    fn default_revalidation_interval_per_content_type() {
        let pat = sample_pattern_unit("Bash", Some("ls"));
        let day = 86_400_u64;
        assert_eq!(default_revalidation_interval(&pat.content), 30 * day);

        let temporal = KnowledgeContent::Temporal {
            description: "x".into(),
            strength: 0.5,
        };
        assert_eq!(default_revalidation_interval(&temporal), 7 * day);

        let outcome = KnowledgeContent::ApproachOutcome {
            approach_ref: "pattern:Bash:ls".into(),
            success_rate: 0.9,
            sample_count: 10,
            median_cost_usd: None,
            median_duration_ms: None,
            conditions: vec![],
        };
        assert_eq!(default_revalidation_interval(&outcome), 14 * day);

        let skill = sample_skill_unit();
        assert_eq!(default_revalidation_interval(&skill.content), 90 * day);
    }

    #[test]
    fn effective_confidence_returns_full_when_fresh() {
        let mut unit = sample_pattern_unit("Bash", Some("ls"));
        unit.last_validated_at = 1_000_000;
        // now == validation_at, age = 0 → no decay
        assert!((effective_confidence(&unit, 1_000_000) - unit.confidence).abs() < 1e-9);
    }

    #[test]
    fn effective_confidence_decays_when_overdue() {
        let mut unit = sample_pattern_unit("Bash", Some("ls"));
        unit.confidence = 1.0;
        unit.last_validated_at = 0;
        unit.revalidation_interval_secs = 100;

        // age = 50 (under interval) → no decay
        assert!((effective_confidence(&unit, 50) - 1.0).abs() < 1e-9);

        // age = 150 (1 interval over) → 0.9
        assert!((effective_confidence(&unit, 150) - 0.9).abs() < 1e-9);

        // age = 250 (2 intervals over) → 0.81
        assert!((effective_confidence(&unit, 250) - 0.81).abs() < 1e-9);

        // age = 350 (3 intervals over) → 0.729
        assert!((effective_confidence(&unit, 350) - 0.729).abs() < 1e-3);
    }

    #[test]
    fn effective_confidence_falls_back_to_default_interval() {
        let mut unit = sample_pattern_unit("Bash", Some("ls"));
        unit.revalidation_interval_secs = 0; // sentinel: use default
        unit.last_validated_at = 0;
        unit.confidence = 1.0;

        let day = 86_400;
        // Pattern default = 30 days. At 30 days exactly: still fresh.
        assert!((effective_confidence(&unit, 30 * day) - 1.0).abs() < 1e-9);

        // At 60 days (1 interval over): 0.9
        assert!((effective_confidence(&unit, 60 * day) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn is_stale_after_interval() {
        let mut unit = sample_pattern_unit("Bash", Some("ls"));
        unit.revalidation_interval_secs = 100;
        unit.last_validated_at = 0;

        assert!(!is_stale(&unit, 50));
        assert!(!is_stale(&unit, 100));
        assert!(is_stale(&unit, 200));
    }
}

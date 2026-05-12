#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::brain::client::BrainSuggestion;

// ────────────────────────────────────────────────────────────────────────────
// Re-exports from sub-modules so that existing `brain::decisions::*` paths
// continue to resolve without changes to callers.
// ────────────────────────────────────────────────────────────────────────────

#[allow(unused_imports)]
pub use super::preferences::{
    DistilledPreferences, PreferenceCondition, PreferencePattern, TemporalPattern, ToolAccuracy,
    adaptive_threshold, backfill_outcomes, distill_preferences, format_preference_summary,
    load_preferences, load_preferences_for_project,
};

#[allow(unused_imports)]
pub use super::retrieval::{format_few_shot_examples, retrieve_similar};

// Re-export save functions for use within the brain crate (used by maybe_distill_background)
pub(super) use super::preferences::{save_preferences, save_project_preferences};

// ────────────────────────────────────────────────────────────────────────────
// Atomics and constants
// ────────────────────────────────────────────────────────────────────────────

/// Counter for decisions logged this process lifetime (avoids reading file to check).
static DECISION_COUNT: AtomicU32 = AtomicU32::new(0);
/// Guard to prevent concurrent distillation threads.
static DISTILLING: AtomicBool = AtomicBool::new(false);
/// Monotonic counter for decision_id uniqueness within a process.
static DECISION_ID_COUNTER: AtomicU32 = AtomicU32::new(0);

/// How often to re-distill preferences (every N decisions).
const DISTILL_INTERVAL: u32 = 10;

/// Minimum number of per-project decisions before using project-specific preferences.
const MIN_PROJECT_DECISIONS: usize = 10;

// ────────────────────────────────────────────────────────────────────────────
// Core types
// ────────────────────────────────────────────────────────────────────────────

/// Whether a decision was made for a single session or for orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionType {
    /// Normal per-session decision (approve, deny, send).
    Session,
    /// Cross-session orchestration decision (spawn, route, terminate).
    Orchestration,
}

impl DecisionType {
    pub fn label(&self) -> &'static str {
        match self {
            DecisionType::Session => "session",
            DecisionType::Orchestration => "orchestration",
        }
    }

    pub fn from_label(s: &str) -> Self {
        match s {
            "orchestration" => DecisionType::Orchestration,
            _ => DecisionType::Session,
        }
    }
}

/// A single decision record: what the brain suggested and what the user did.
#[derive(Debug, Clone)]
pub struct DecisionRecord {
    pub timestamp: String,
    pub pid: u32,
    pub project: String,
    pub tool: Option<String>,
    pub command: Option<String>,
    pub brain_action: String,
    pub brain_confidence: f64,
    pub brain_reasoning: String,
    pub user_action: String, // "accept", "reject", "auto", "deny_rule_override"
    pub context: Option<DecisionContext>,
    pub outcome: Option<DecisionOutcome>,
    /// Whether this was a session or orchestration decision.
    /// Defaults to Session for backwards compatibility with old records.
    pub decision_type: DecisionType,
    /// Epoch seconds when the brain suggestion was created.
    /// None for old records or observations. Used by time-to-correct analysis.
    pub suggested_at: Option<u64>,
    /// Epoch seconds when the user acted on the suggestion.
    pub resolved_at: Option<u64>,
    /// Why the user overrode a brain denial (if applicable).
    pub override_reason: Option<String>,
    /// Stable id for outcome attribution (#220 baselining). None on records
    /// written before the field existed; outcomes for those can't be joined.
    pub decision_id: Option<String>,
}

/// Generate a unique decision id.
pub fn gen_decision_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let pid = std::process::id();
    let seq = DECISION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("dec_{secs}_{pid}_{seq}")
}

/// Outcome of a decision, backfilled during distillation by looking at
/// consecutive same-PID records and resolved test-runner outcomes.
#[derive(Debug, Clone)]
pub enum DecisionOutcome {
    Success,
    Error(String),
    /// A test-runner command failed within the attribution window after this
    /// edit was approved (#238). Carries the failing command for diagnostics.
    /// Weighted more strongly than `Error` in distillation because a broken
    /// build is a stronger negative signal than a transient tool error.
    TestFailed(String),
}

/// Snapshot of session state captured at decision time.
/// Stored in JSONL for rich distillation. NOT sent to LLM directly.
#[derive(Debug, Clone)]
pub struct DecisionContext {
    pub cost_usd: f64,
    pub context_pct: u8,
    pub last_tool_error: bool,
    pub error_message: Option<String>,
    pub model: String,
    pub elapsed_secs: u64,
    pub files_modified_count: u32,
    pub total_tool_calls: u32,
    pub has_file_conflict: bool,
    pub status: String,
    pub burn_rate_per_hr: f64,
    pub recent_error_count: u8,
    pub subagent_count: u8,
    /// Hour of day (0-23) when this decision was made. Used for time-of-day
    /// preference distillation. None for records from before this field existed.
    pub hour: Option<u8>,
}

impl DecisionRecord {
    /// Whether this decision represents a positive outcome (user agreed or auto-executed).
    pub fn is_positive(&self) -> bool {
        matches!(
            self.user_action.as_str(),
            "accept" | "auto" | "user_approve" | "rule_approve"
        )
    }

    /// Whether this decision represents a negative outcome (user disagreed).
    pub fn is_negative(&self) -> bool {
        matches!(
            self.user_action.as_str(),
            "reject" | "deny_rule_override" | "rule_deny" | "conflict_deny"
        )
    }

    /// Whether this is a passive observation (brain was NOT involved).
    pub fn is_observation(&self) -> bool {
        matches!(
            self.user_action.as_str(),
            "user_approve"
                | "user_input"
                | "rule_approve"
                | "rule_deny"
                | "rule_send"
                | "conflict_deny"
        )
    }
}

#[derive(Debug, Default)]
pub struct DecisionStats {
    pub total: u32,
    pub accepted: u32,
    pub rejected: u32,
    pub auto_executed: u32,
    pub observations: u32,
}

impl DecisionStats {
    pub fn accuracy_pct(&self) -> f64 {
        let decided = self.accepted + self.rejected;
        if decided == 0 {
            return 0.0;
        }
        (self.accepted as f64 / decided as f64) * 100.0
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Path helpers
// ────────────────────────────────────────────────────────────────────────────

pub(super) fn decisions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claudectl").join("brain")
}

fn decisions_path() -> PathBuf {
    decisions_dir().join("decisions.jsonl")
}

/// Convert a project name to a filesystem-safe slug.
/// Returns "unknown" for empty or whitespace-only names.
pub(super) fn project_slug(project: &str) -> String {
    let slug: String = project
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .to_lowercase();
    if slug.is_empty() || slug.chars().all(|c| c == '_') {
        "unknown".to_string()
    } else {
        slug
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Time helpers
// ────────────────────────────────────────────────────────────────────────────

fn timestamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO-ish format without chrono dependency
    format!("{secs}")
}

/// Compute the current local hour (0-23) without chrono.
/// Uses libc::localtime_r for timezone-aware hour so that work-hours
/// pattern detection aligns with the user's actual schedule.
pub(super) fn current_hour() -> u8 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    local_hour_from_epoch(secs as i64)
}

pub(super) fn local_hour_from_epoch(epoch_secs: i64) -> u8 {
    #[cfg(unix)]
    {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&epoch_secs, &mut tm) };
        tm.tm_hour as u8
    }
    #[cfg(not(unix))]
    {
        // Fallback to UTC on non-unix platforms
        ((epoch_secs as u64 % 86400) / 3600) as u8
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Context snapshot
// ────────────────────────────────────────────────────────────────────────────

/// Build a JSON snapshot of session state for embedding in a JSONL record.
fn snapshot_context(session: &crate::session::ClaudeSession) -> serde_json::Value {
    let context_pct = if session.context_max > 0 {
        ((session.context_tokens as f64 / session.context_max as f64) * 100.0) as u8
    } else {
        0
    };
    serde_json::json!({
        "cost_usd": session.cost_usd,
        "context_pct": context_pct,
        "last_tool_error": session.last_tool_error,
        "error_message": session.last_error_message.as_deref().map(|m| crate::session::truncate_str(m, 100)),
        "model": session.model,
        "elapsed_secs": session.elapsed.as_secs(),
        "files_modified_count": session.files_modified.len() as u32,
        "total_tool_calls": session.tool_usage.values().map(|t| t.calls).sum::<u32>(),
        "has_file_conflict": session.has_file_conflict,
        "status": session.status.to_string(),
        "burn_rate_per_hr": session.burn_rate_per_hr,
        "recent_error_count": session.recent_errors.len() as u8,
        "subagent_count": session.subagent_count as u8,
        "hour": current_hour(),
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Logging
// ────────────────────────────────────────────────────────────────────────────

/// Log a brain decision (suggestion + user response) to the local JSONL file.
/// `decision_type` distinguishes session-level vs orchestration-level decisions.
#[allow(clippy::too_many_arguments)]
pub fn log_decision(
    pid: u32,
    project: &str,
    tool: Option<&str>,
    command: Option<&str>,
    suggestion: &BrainSuggestion,
    user_action: &str,
    session: Option<&crate::session::ClaudeSession>,
    decision_type: DecisionType,
    override_reason: Option<&str>,
) {
    let resolved_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let decision_id = gen_decision_id();
    let mut record = serde_json::json!({
        "ts": timestamp_now(),
        "pid": pid,
        "project": project,
        "tool": tool,
        "command": command,
        "brain_action": suggestion.action.label(),
        "brain_confidence": suggestion.confidence,
        "brain_reasoning": suggestion.reasoning,
        "user_action": user_action,
        "decision_type": decision_type.label(),
        "suggested_at": suggestion.suggested_at,
        "resolved_at": resolved_at,
        "override_reason": override_reason,
        "decision_id": decision_id,
    });
    if let Some(s) = session {
        record["context"] = snapshot_context(s);
    }

    let path = decisions_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(
            file,
            "{}",
            serde_json::to_string(&record).unwrap_or_default()
        );
    }

    // #223 injection feedback loop: attribute this decision's outcome back to
    // every hive unit that appeared in the prompt for `pid`. No-op when hive
    // wasn't injected for this session (no pending stash).
    #[cfg(feature = "hive")]
    crate::hive::feedback::record_outcome(pid, user_action);

    // Re-distill preferences in a background thread every Nth decision.
    // The file append above is fast (single write), but distillation reads
    // the full history and computes patterns — must not block the TUI.
    maybe_distill_background();
}

/// Log a passive observation: a user action the brain was NOT involved in.
/// These provide ground-truth training data — what the user does when
/// deciding on their own. Same JSONL format so distillation picks them up.
pub fn log_observation(
    pid: u32,
    project: &str,
    tool: Option<&str>,
    command: Option<&str>,
    observed_action: &str, // "user_approve", "user_input", "rule_approve", "rule_deny", etc.
    session: Option<&crate::session::ClaudeSession>,
) {
    let decision_id = gen_decision_id();
    let mut record = serde_json::json!({
        "ts": timestamp_now(),
        "pid": pid,
        "project": project,
        "tool": tool,
        "command": command,
        "brain_action": null,
        "brain_confidence": 0.0,
        "brain_reasoning": "",
        "user_action": observed_action,
        "decision_id": decision_id,
    });
    if let Some(s) = session {
        record["context"] = snapshot_context(s);
    }

    let path = decisions_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(
            file,
            "{}",
            serde_json::to_string(&record).unwrap_or_default()
        );
    }

    maybe_distill_background();
}

// ────────────────────────────────────────────────────────────────────────────
// Background distillation
// ────────────────────────────────────────────────────────────────────────────

/// Spawn a background thread to re-distill preferences if the interval has been reached.
/// Uses atomic guards to avoid blocking the main thread and prevent concurrent distillation.
fn maybe_distill_background() {
    let count = DECISION_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count % DISTILL_INTERVAL != 0 {
        return;
    }

    // Prevent concurrent distillation (compare_exchange: only one thread wins)
    if DISTILLING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return; // Another distillation is already running
    }

    std::thread::spawn(|| {
        let all = read_all_decisions();
        if !all.is_empty() {
            // Global distillation
            let prefs = distill_preferences(&all);
            let _ = save_preferences(&prefs);

            // Per-project distillation for projects with enough data
            let mut projects: HashMap<String, Vec<DecisionRecord>> = HashMap::new();
            for d in &all {
                projects
                    .entry(d.project.to_lowercase())
                    .or_default()
                    .push(d.clone());
            }
            for (project, decisions) in &projects {
                if decisions.len() >= MIN_PROJECT_DECISIONS {
                    let proj_prefs = distill_preferences(decisions);
                    let _ = save_project_preferences(project, &proj_prefs);

                    // Promote high-confidence patterns to coordination memory
                    #[cfg(feature = "coord")]
                    {
                        let _ =
                            crate::coord::promotion::promote_from_preferences(project, &proj_prefs);
                    }
                }
            }

            // Mine and persist the anti-pattern library (#201). Cheap to run
            // alongside distillation; reads decision history we already have.
            let library = super::sequences::mine_antipatterns(&all);
            let _ = super::sequences::save_library(&library);

            // Generate insights if insights mode is on
            if super::insights::read_insights_mode() == "on" {
                let insights = super::insights::generate_insights(&all, &prefs);
                let mut state = super::insights::load_state();
                let _ = super::insights::merge_insights(insights, &mut state);
                let _ = super::insights::save_state(&state);
            }

            // Export knowledge units to hive store for sharing
            #[cfg(feature = "hive")]
            {
                let cfg = crate::config::Config::load();
                if crate::hive::is_active(cfg.hive.as_ref()) {
                    let hive_cfg = cfg.hive.clone().unwrap_or_default();
                    let thresholds = crate::hive::distiller::ExportThresholds {
                        min_pattern_evidence: hive_cfg.export_min_evidence,
                        min_tool_decisions: hive_cfg.export_min_tool_decisions,
                        ..Default::default()
                    };
                    #[cfg(feature = "relay")]
                    let local_id = crate::relay::load_or_create_identity().0;
                    #[cfg(not(feature = "relay"))]
                    let local_id = crate::hive::local_identity();
                    let mut store = crate::hive::store::HiveStore::load();
                    let units = crate::hive::distiller::distill_to_knowledge_stable(
                        &prefs,
                        &local_id,
                        None,
                        &thresholds,
                        &store,
                    );
                    let _count = units.len() as u32;
                    for unit in units {
                        store.insert(unit);
                    }

                    // Compact: enforce TTL, max_units, stale peer cleanup
                    let trust_store =
                        crate::hive::trust::TrustStore::load_with_default(hive_cfg.default_trust);
                    let evicted = store.compact(
                        hive_cfg.knowledge_ttl_days,
                        hive_cfg.max_units,
                        hive_cfg.stale_peer_days,
                        Some(&trust_store),
                    );
                    if !evicted.is_empty() {
                        // Archive evicted units to cold storage (optional)
                        let archived = crate::hive::archive::archive_units(&evicted).unwrap_or(0);
                        crate::logger::log(
                            "HIVE",
                            &format!(
                                "compacted: {} evicted, {} archived (max {})",
                                evicted.len(),
                                archived,
                                hive_cfg.max_units
                            ),
                        );
                    }

                    let _ = store.save();

                    // Signal the relay to broadcast new knowledge to peers
                    #[cfg(feature = "relay")]
                    if _count > 0 {
                        crate::hive::signal_new_knowledge(_count);
                    }
                }
            }
        }
        DISTILLING.store(false, Ordering::Release);
    });
}

// ────────────────────────────────────────────────────────────────────────────
// Reading decisions and stats
// ────────────────────────────────────────────────────────────────────────────

/// Read decision stats for display.
pub fn read_stats() -> DecisionStats {
    let path = decisions_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return DecisionStats::default(),
    };

    let mut total = 0u32;
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut auto_executed = 0u32;
    let mut observations = 0u32;

    for line in content.lines() {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        total += 1;
        match json.get("user_action").and_then(|v| v.as_str()) {
            Some("accept") => accepted += 1,
            Some("reject") => rejected += 1,
            Some("auto") => auto_executed += 1,
            Some(
                "user_approve" | "user_input" | "rule_approve" | "rule_deny" | "rule_send"
                | "conflict_deny",
            ) => observations += 1,
            _ => {}
        }
    }

    DecisionStats {
        total,
        accepted,
        rejected,
        auto_executed,
        observations,
    }
}

/// Clear all decision history and distilled preferences.
pub fn forget() -> Result<(), String> {
    let path = decisions_path();
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("failed to delete {}: {e}", path.display()))?;
    }
    let pref_path = decisions_dir().join("preferences.json");
    if pref_path.exists() {
        let _ = fs::remove_file(&pref_path);
    }
    // Also clean per-project preference files
    let proj_dir = decisions_dir().join("preferences");
    if proj_dir.is_dir() {
        let _ = fs::remove_dir_all(&proj_dir);
    }
    Ok(())
}

pub fn read_all_decisions() -> Vec<DecisionRecord> {
    let path = decisions_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter_map(|line| {
            let json: serde_json::Value = serde_json::from_str(line).ok()?;
            let context = json.get("context").and_then(|ctx| {
                Some(DecisionContext {
                    cost_usd: ctx.get("cost_usd")?.as_f64()?,
                    context_pct: ctx.get("context_pct")?.as_u64()? as u8,
                    last_tool_error: ctx.get("last_tool_error")?.as_bool()?,
                    error_message: ctx
                        .get("error_message")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    model: ctx.get("model")?.as_str()?.to_string(),
                    elapsed_secs: ctx.get("elapsed_secs")?.as_u64()?,
                    files_modified_count: ctx.get("files_modified_count")?.as_u64()? as u32,
                    total_tool_calls: ctx.get("total_tool_calls")?.as_u64()? as u32,
                    has_file_conflict: ctx.get("has_file_conflict")?.as_bool()?,
                    status: ctx.get("status")?.as_str()?.to_string(),
                    burn_rate_per_hr: ctx.get("burn_rate_per_hr")?.as_f64()?,
                    recent_error_count: ctx.get("recent_error_count")?.as_u64()? as u8,
                    subagent_count: ctx.get("subagent_count")?.as_u64()? as u8,
                    // Backwards-compatible: old records won't have "hour" field
                    hour: ctx.get("hour").and_then(|v| v.as_u64()).map(|v| v as u8),
                })
            });
            // Backwards-compatible: old records won't have "decision_type" field
            let decision_type = json
                .get("decision_type")
                .and_then(|v| v.as_str())
                .map(DecisionType::from_label)
                .unwrap_or(DecisionType::Session);
            Some(DecisionRecord {
                timestamp: json.get("ts")?.to_string(),
                pid: json.get("pid")?.as_u64()? as u32,
                project: json.get("project")?.as_str()?.to_string(),
                tool: json
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                command: json
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                // Handle null brain_action (observations log it as null)
                brain_action: json
                    .get("brain_action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                brain_confidence: json
                    .get("brain_confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                brain_reasoning: json
                    .get("brain_reasoning")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                user_action: json.get("user_action")?.as_str()?.to_string(),
                context,
                outcome: None, // Backfilled during distillation
                decision_type,
                // Backwards-compatible: old records won't have these fields
                suggested_at: json.get("suggested_at").and_then(|v| v.as_u64()),
                resolved_at: json.get("resolved_at").and_then(|v| v.as_u64()),
                override_reason: json
                    .get("override_reason")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                decision_id: json
                    .get("decision_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            })
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::RuleAction;

    fn make_suggestion() -> BrainSuggestion {
        BrainSuggestion {
            action: RuleAction::Approve,
            message: None,
            reasoning: "safe command".into(),
            confidence: 0.95,
            suggested_at: 0,
        }
    }

    fn make_decision(tool: &str, project: &str, user_action: &str) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some("test cmd".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: user_action.into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: None,
        }
    }

    fn make_context(cost_usd: f64, context_pct: u8, last_tool_error: bool) -> DecisionContext {
        DecisionContext {
            cost_usd,
            context_pct,
            last_tool_error,
            error_message: if last_tool_error {
                Some("test error".to_string())
            } else {
                None
            },
            model: "sonnet".into(),
            elapsed_secs: 60,
            files_modified_count: 2,
            total_tool_calls: 10,
            has_file_conflict: false,
            status: "Working".into(),
            burn_rate_per_hr: 1.0,
            recent_error_count: if last_tool_error { 1 } else { 0 },
            subagent_count: 0,
            hour: None,
        }
    }

    fn make_context_with_hour(
        cost_usd: f64,
        context_pct: u8,
        last_tool_error: bool,
        hour: u8,
    ) -> DecisionContext {
        DecisionContext {
            hour: Some(hour),
            ..make_context(cost_usd, context_pct, last_tool_error)
        }
    }

    fn make_orchestration_decision(tool: &str, project: &str, user_action: &str) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".into(),
            pid: 0,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some("test cmd".into()),
            brain_action: "spawn".into(),
            brain_confidence: 0.85,
            brain_reasoning: "orchestration test".into(),
            user_action: user_action.into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Orchestration,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: None,
        }
    }

    #[test]
    fn log_and_read_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");

        // Write directly to a temp path
        let record = serde_json::json!({
            "user_action": "accept",
            "brain_action": "approve",
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();

        let record2 = serde_json::json!({
            "user_action": "reject",
            "brain_action": "approve",
        });
        writeln!(file, "{}", serde_json::to_string(&record2).unwrap()).unwrap();
        drop(file);

        // Parse the file
        let content = fs::read_to_string(&path).unwrap();
        let mut accepted = 0;
        let mut rejected = 0;
        for line in content.lines() {
            let json: serde_json::Value = serde_json::from_str(line).unwrap();
            match json["user_action"].as_str() {
                Some("accept") => accepted += 1,
                Some("reject") => rejected += 1,
                _ => {}
            }
        }
        assert_eq!(accepted, 1);
        assert_eq!(rejected, 1);
    }

    #[test]
    fn stats_accuracy() {
        let stats = DecisionStats {
            total: 10,
            accepted: 8,
            rejected: 2,
            auto_executed: 0,
            observations: 0,
        };
        assert!((stats.accuracy_pct() - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_accuracy_no_decisions() {
        let stats = DecisionStats::default();
        assert!((stats.accuracy_pct() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn suggestion_label_used() {
        let s = make_suggestion();
        assert_eq!(s.action.label(), "approve");
    }

    #[test]
    fn decision_record_outcome_classification() {
        let accept = make_decision("Bash", "proj", "accept");
        assert!(accept.is_positive());
        assert!(!accept.is_negative());
        assert!(!accept.is_observation());

        let reject = make_decision("Bash", "proj", "reject");
        assert!(!reject.is_positive());
        assert!(reject.is_negative());
        assert!(!reject.is_observation());

        let auto = make_decision("Bash", "proj", "auto");
        assert!(auto.is_positive());
        assert!(!auto.is_negative());
        assert!(!auto.is_observation());

        let deny_override = make_decision("Bash", "proj", "deny_rule_override");
        assert!(!deny_override.is_positive());
        assert!(deny_override.is_negative());
    }

    // ── Passive observation tests ─────────────────────────────────────

    #[test]
    fn observation_user_approve_is_positive() {
        let d = make_decision("Read", "proj", "user_approve");
        assert!(d.is_positive());
        assert!(!d.is_negative());
        assert!(d.is_observation());
    }

    #[test]
    fn observation_rule_approve_is_positive() {
        let d = make_decision("Bash", "proj", "rule_approve");
        assert!(d.is_positive());
        assert!(d.is_observation());
    }

    #[test]
    fn observation_rule_deny_is_negative() {
        let d = make_decision("Bash", "proj", "rule_deny");
        assert!(d.is_negative());
        assert!(d.is_observation());
    }

    #[test]
    fn observation_conflict_deny_is_negative() {
        let d = make_decision("Write", "proj", "conflict_deny");
        assert!(d.is_negative());
        assert!(d.is_observation());
    }

    #[test]
    fn observation_user_input_is_observation() {
        let d = make_decision("Bash", "proj", "user_input");
        assert!(d.is_observation());
        // user_input is neither approve nor deny
        assert!(!d.is_positive());
        assert!(!d.is_negative());
    }

    // ── Snapshot context tests ────────────────────────────────────────

    #[test]
    fn test_snapshot_context_fields() {
        use crate::session::{ClaudeSession, SessionStatus};
        use std::collections::HashMap;
        use std::time::Duration;

        let mut tool_usage = HashMap::new();
        tool_usage.insert("Bash".to_string(), crate::session::ToolStats { calls: 5 });
        tool_usage.insert("Read".to_string(), crate::session::ToolStats { calls: 3 });

        let mut files = HashMap::new();
        files.insert("src/main.rs".to_string(), 2u32);

        let session = ClaudeSession {
            pid: 42,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            project_name: "test-proj".into(),
            started_at: 0,
            elapsed: Duration::from_secs(120),
            tty: "/dev/pts/0".into(),
            status: SessionStatus::Processing,
            cpu_percent: 50.0,
            cpu_history: vec![],
            mem_mb: 100.0,
            own_input_tokens: 1000,
            own_output_tokens: 500,
            own_cache_read_tokens: 0,
            own_cache_write_tokens: 0,
            subagent_input_tokens: 0,
            subagent_output_tokens: 0,
            subagent_cache_read_tokens: 0,
            subagent_cache_write_tokens: 0,
            total_input_tokens: 1000,
            total_output_tokens: 500,
            model: "sonnet".into(),
            command_args: "".into(),
            session_name: "test".into(),
            jsonl_path: None,
            jsonl_offset: 0,
            last_message_ts: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 3.50,
            context_tokens: 80000,
            context_max: 100000,
            prev_cost_usd: 3.0,
            burn_rate_per_hr: 2.5,
            subagent_count: 1,
            active_subagent_count: 0,
            active_subagent_jsonl_paths: vec![],
            subagent_rollups: HashMap::new(),
            activity_history: vec![],
            files_modified: files,
            tool_usage,
            worktree_id: None,
            telemetry_status: crate::session::TelemetryStatus::Available,
            usage_metrics_available: true,
            cost_estimate_unverified: false,
            model_profile_source: "builtin".into(),
            last_msg_type: "".into(),
            last_stop_reason: "".into(),
            is_waiting_for_task: false,
            pending_tool_name: None,
            pending_tool_input: None,
            pending_file_path: None,
            has_file_conflict: false,
            last_tool_error: true,
            last_error_message: Some("command failed".into()),
            recent_errors: vec![crate::session::ErrorEntry {
                tool_name: "Bash".into(),
                message: "exit code 1".into(),
            }],
            total_tokens_at_edit_count: 0,
            edit_event_count: 0,
            baseline_tokens_per_edit: None,
            error_counts_per_window: vec![],
            current_window_errors: 0,
            window_tick_counter: 0,
            baseline_error_rate: None,
            file_reads_since_edit: HashMap::new(),
            total_error_count: 0,
            decay_score: 0,
            worker_origin: None,
        };

        let ctx = snapshot_context(&session);

        // Verify all 13 original fields + hour
        assert_eq!(ctx["cost_usd"].as_f64().unwrap(), 3.5);
        assert_eq!(ctx["context_pct"].as_u64().unwrap(), 80);
        assert!(ctx["last_tool_error"].as_bool().unwrap());
        assert_eq!(ctx["error_message"].as_str().unwrap(), "command failed");
        assert_eq!(ctx["model"].as_str().unwrap(), "sonnet");
        assert_eq!(ctx["elapsed_secs"].as_u64().unwrap(), 120);
        assert_eq!(ctx["files_modified_count"].as_u64().unwrap(), 1);
        assert_eq!(ctx["total_tool_calls"].as_u64().unwrap(), 8); // 5+3
        assert!(!ctx["has_file_conflict"].as_bool().unwrap());
        assert_eq!(ctx["status"].as_str().unwrap(), "Processing");
        assert_eq!(ctx["burn_rate_per_hr"].as_f64().unwrap(), 2.5);
        assert_eq!(ctx["recent_error_count"].as_u64().unwrap(), 1);
        assert_eq!(ctx["subagent_count"].as_u64().unwrap(), 1);
        // Hour should be present (0-23)
        let hour = ctx["hour"].as_u64().unwrap();
        assert!(hour < 24, "hour should be 0-23, got {hour}");
    }

    #[test]
    fn test_backward_compat_no_context() {
        // Simulate a JSONL record without the "context" field (old format)
        let json_str = r#"{"ts":"123","pid":1,"project":"proj","tool":"Bash","command":"ls","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept"}"#;
        let json: serde_json::Value = serde_json::from_str(json_str).unwrap();

        // Parse context — should be None
        let context = json.get("context").and_then(|ctx| {
            Some(DecisionContext {
                cost_usd: ctx.get("cost_usd")?.as_f64()?,
                context_pct: ctx.get("context_pct")?.as_u64()? as u8,
                last_tool_error: ctx.get("last_tool_error")?.as_bool()?,
                error_message: None,
                model: ctx.get("model")?.as_str()?.to_string(),
                elapsed_secs: ctx.get("elapsed_secs")?.as_u64()?,
                files_modified_count: ctx.get("files_modified_count")?.as_u64()? as u32,
                total_tool_calls: ctx.get("total_tool_calls")?.as_u64()? as u32,
                has_file_conflict: ctx.get("has_file_conflict")?.as_bool()?,
                status: ctx.get("status")?.as_str()?.to_string(),
                burn_rate_per_hr: ctx.get("burn_rate_per_hr")?.as_f64()?,
                recent_error_count: ctx.get("recent_error_count")?.as_u64()? as u8,
                subagent_count: ctx.get("subagent_count")?.as_u64()? as u8,
                hour: ctx.get("hour").and_then(|v| v.as_u64()).map(|v| v as u8),
            })
        });
        assert!(context.is_none());

        // Also verify the record still parses with null brain_action (observation)
        let obs_str = r#"{"ts":"124","pid":1,"project":"proj","tool":"Bash","command":"ls","brain_action":null,"brain_confidence":0.0,"brain_reasoning":"","user_action":"user_approve"}"#;
        let obs_json: serde_json::Value = serde_json::from_str(obs_str).unwrap();
        let brain_action = obs_json
            .get("brain_action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        assert_eq!(brain_action, "");

        // Verify decision_type defaults to Session for old records
        let decision_type = json
            .get("decision_type")
            .and_then(|v| v.as_str())
            .map(DecisionType::from_label)
            .unwrap_or(DecisionType::Session);
        assert_eq!(decision_type, DecisionType::Session);
    }

    // ── Decision type tests ──────────────────────────────────────────

    #[test]
    fn test_decision_type_labels() {
        assert_eq!(DecisionType::Session.label(), "session");
        assert_eq!(DecisionType::Orchestration.label(), "orchestration");
    }

    #[test]
    fn test_decision_type_from_label() {
        assert_eq!(DecisionType::from_label("session"), DecisionType::Session);
        assert_eq!(
            DecisionType::from_label("orchestration"),
            DecisionType::Orchestration
        );
        // Unknown defaults to Session
        assert_eq!(DecisionType::from_label("unknown"), DecisionType::Session);
        assert_eq!(DecisionType::from_label(""), DecisionType::Session);
    }

    #[test]
    fn test_orchestration_decision_tagged() {
        let d = make_orchestration_decision("Bash", "proj", "accept");
        assert_eq!(d.decision_type, DecisionType::Orchestration);
        assert_eq!(d.brain_action, "spawn");
    }

    #[test]
    fn test_session_decision_tagged() {
        let d = make_decision("Bash", "proj", "accept");
        assert_eq!(d.decision_type, DecisionType::Session);
    }

    #[test]
    fn test_backward_compat_decision_type() {
        // Old records without decision_type should default to Session
        let json_str = r#"{"ts":"123","pid":1,"project":"proj","tool":"Bash","command":"ls","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept"}"#;
        let json: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let dt = json
            .get("decision_type")
            .and_then(|v| v.as_str())
            .map(DecisionType::from_label)
            .unwrap_or(DecisionType::Session);
        assert_eq!(dt, DecisionType::Session);
    }

    #[test]
    fn test_backward_compat_no_hour_in_context() {
        // Old context records without hour field → hour should be None
        let json_str = r#"{"cost_usd":1.0,"context_pct":50,"last_tool_error":false,"model":"sonnet","elapsed_secs":60,"files_modified_count":2,"total_tool_calls":10,"has_file_conflict":false,"status":"Working","burn_rate_per_hr":1.0,"recent_error_count":0,"subagent_count":0}"#;
        let ctx: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let hour: Option<u8> = ctx.get("hour").and_then(|v| v.as_u64()).map(|v| v as u8);
        assert!(hour.is_none());
    }

    #[test]
    fn test_current_hour_is_valid() {
        let hour = current_hour();
        assert!(hour < 24, "current_hour() returned {hour}, expected 0-23");
    }

    #[test]
    fn test_hour_captured_in_context() {
        // The make_context_with_hour helper sets the hour field
        let ctx = make_context_with_hour(1.0, 50, false, 14);
        assert_eq!(ctx.hour, Some(14));
    }
}

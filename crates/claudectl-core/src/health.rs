#![allow(dead_code)]

use crate::session::ClaudeSession;

/// Tunable thresholds for the health checks below. Lives here (not in
/// `config`) because health is a foundational `claudectl-core` concern; the
/// binary's `config::Config` re-exports this type and parses TOML overrides
/// against it.
#[derive(Debug, Clone)]
pub struct HealthThresholds {
    pub cache_critical_pct: f64,
    pub cache_warning_pct: f64,
    pub cache_min_tokens: u64,
    pub cost_spike_critical: f64,
    pub cost_spike_warning: f64,
    pub loop_max_calls: u32,
    pub stall_min_cost: f64,
    pub stall_min_minutes: u64,
    pub context_critical_pct: f64,
    pub context_warning_pct: f64,
    pub decay_compaction_pct: f64,
    pub efficiency_critical_factor: f64,
    pub error_accel_factor: f64,
    pub repetition_threshold: u32,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            cache_critical_pct: 10.0,
            cache_warning_pct: 30.0,
            cache_min_tokens: 10_000,
            cost_spike_critical: 5.0,
            cost_spike_warning: 2.5,
            loop_max_calls: 10,
            stall_min_cost: 5.0,
            stall_min_minutes: 10,
            context_critical_pct: 90.0,
            context_warning_pct: 80.0,
            decay_compaction_pct: 50.0,
            efficiency_critical_factor: 2.0,
            error_accel_factor: 2.0,
            repetition_threshold: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub icon: &'static str,
    pub name: &'static str,
    pub severity: Severity,
    pub message: String,
}

/// Run all health checks against a session. Returns warnings sorted by severity.
pub fn check_session(session: &ClaudeSession, t: &HealthThresholds) -> Vec<HealthCheck> {
    let mut checks = Vec::new();

    if let Some(c) = check_cache_health(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_cost_spike(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_loop_detection(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_stalled(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_context_saturation(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_cognitive_decay(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_proactive_compaction(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_token_efficiency(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_error_acceleration(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_repetition(session, t) {
        checks.push(c);
    }

    // Sort: Critical first, then Warning, then Info
    checks.sort_by_key(|c| match c.severity {
        Severity::Critical => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    });

    checks
}

/// Return the most severe health icon for display in the table, or empty string if healthy.
pub fn status_icon(session: &ClaudeSession, t: &HealthThresholds) -> &'static str {
    let checks = check_session(session, t);
    match checks.first() {
        Some(c) if c.severity == Severity::Critical => c.icon,
        Some(c) if c.severity == Severity::Warning => c.icon,
        _ => "",
    }
}

/// Format a compact health summary for the status bar.
pub fn format_health_summary(sessions: &[ClaudeSession], t: &HealthThresholds) -> Option<String> {
    let mut warnings = 0;
    let mut criticals = 0;
    let mut worst_msg = String::new();

    for session in sessions {
        for check in check_session(session, t) {
            match check.severity {
                Severity::Critical => {
                    criticals += 1;
                    if worst_msg.is_empty() {
                        worst_msg =
                            format!("{} {}: {}", check.icon, session.display_name(), check.name);
                    }
                }
                Severity::Warning => warnings += 1,
                Severity::Info => {}
            }
        }
    }

    if criticals == 0 && warnings == 0 {
        return None;
    }

    let count = criticals + warnings;
    Some(format!(
        "{} health issue{} | {}",
        count,
        if count == 1 { "" } else { "s" },
        worst_msg,
    ))
}

// ────────────────────────────────────────────────────────────────────────────
// Individual health checks
// ────────────────────────────────────────────────────────────────────────────

/// Detect low cache hit ratio (e.g., cache TTL bug causing 12x cost).
fn check_cache_health(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let total_input = session.total_input_tokens;
    let cache_read = session.cache_read_tokens;

    if total_input < t.cache_min_tokens {
        return None;
    }

    let hit_ratio = cache_read as f64 / total_input as f64;
    let critical_threshold = t.cache_critical_pct / 100.0;
    let warning_threshold = t.cache_warning_pct / 100.0;

    if hit_ratio < critical_threshold {
        Some(HealthCheck {
            icon: "🔥",
            name: "low cache",
            severity: Severity::Critical,
            message: format!(
                "Cache hit ratio is {:.0}% — expected >50% for long sessions. \
                 Possible cache TTL issue (check telemetry settings).",
                hit_ratio * 100.0
            ),
        })
    } else if hit_ratio < warning_threshold {
        Some(HealthCheck {
            icon: "⚠",
            name: "low cache",
            severity: Severity::Warning,
            message: format!(
                "Cache hit ratio is {:.0}% — below typical range. \
                 May indicate cache TTL or model configuration issue.",
                hit_ratio * 100.0
            ),
        })
    } else {
        None
    }
}

/// Detect burn rate spikes — paying more for less output.
fn check_cost_spike(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    if session.cost_usd < 1.0 || session.burn_rate_per_hr <= 0.0 {
        return None;
    }

    let elapsed_hrs = session.elapsed.as_secs_f64() / 3600.0;
    if elapsed_hrs < 0.01 {
        return None;
    }
    let avg_rate = session.cost_usd / elapsed_hrs;

    if avg_rate <= 0.0 {
        return None;
    }

    let spike_factor = session.burn_rate_per_hr / avg_rate;

    if spike_factor > t.cost_spike_critical {
        Some(HealthCheck {
            icon: "💸",
            name: "cost spike",
            severity: Severity::Critical,
            message: format!(
                "Burn rate ${:.1}/hr is {:.0}x the session average ${:.1}/hr.",
                session.burn_rate_per_hr, spike_factor, avg_rate,
            ),
        })
    } else if spike_factor > t.cost_spike_warning {
        Some(HealthCheck {
            icon: "💰",
            name: "cost spike",
            severity: Severity::Warning,
            message: format!(
                "Burn rate ${:.1}/hr is {:.1}x the session average.",
                session.burn_rate_per_hr, spike_factor,
            ),
        })
    } else {
        None
    }
}

/// Detect tool error loops — same tool failing repeatedly.
fn check_loop_detection(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    if !session.last_tool_error {
        return None;
    }

    let max_calls = session
        .tool_usage
        .values()
        .map(|ts| ts.calls)
        .max()
        .unwrap_or(0);

    if max_calls >= t.loop_max_calls && session.last_tool_error {
        let tool_name = session
            .tool_usage
            .iter()
            .max_by_key(|(_, ts)| ts.calls)
            .map(|(name, _)| name.as_str())
            .unwrap_or("?");

        Some(HealthCheck {
            icon: "🔄",
            name: "looping",
            severity: Severity::Warning,
            message: format!(
                "{tool_name} called {max_calls} times with recent errors — may be stuck in a retry loop.",
            ),
        })
    } else {
        None
    }
}

/// Detect stalled sessions — high cost but no file output.
fn check_stalled(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    if session.cost_usd < t.stall_min_cost {
        return None;
    }

    let files_edited: u32 = session.files_modified.values().sum();
    let elapsed_mins = session.elapsed.as_secs() / 60;

    if files_edited == 0 && elapsed_mins > t.stall_min_minutes {
        Some(HealthCheck {
            icon: "🐌",
            name: "stalled",
            severity: Severity::Warning,
            message: format!(
                "Spent ${:.1} over {} min with no file edits.",
                session.cost_usd, elapsed_mins,
            ),
        })
    } else {
        None
    }
}

/// Detect context window saturation.
fn check_context_saturation(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    if session.context_max == 0 {
        return None;
    }

    let pct = (session.context_tokens as f64 / session.context_max as f64) * 100.0;

    if pct > t.context_critical_pct {
        Some(HealthCheck {
            icon: "🧠",
            name: "context full",
            severity: Severity::Critical,
            message: format!(
                "Context at {:.0}% — session may degrade or auto-compact. \
                 Consider spawning a fresh session.",
                pct,
            ),
        })
    } else if pct > t.context_warning_pct {
        Some(HealthCheck {
            icon: "🧠",
            name: "context high",
            severity: Severity::Warning,
            message: format!("Context at {:.0}% — approaching limit.", pct),
        })
    } else {
        None
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Cognitive decay checks
// ────────────────────────────────────────────────────────────────────────────

/// Compute a composite cognitive decay score (0-100) from multiple signals.
pub fn compute_decay_score(session: &ClaudeSession, _t: &HealthThresholds) -> u32 {
    let mut score: f64 = 0.0;

    // Context contribution: 0-40 points (linear from 40% to 100%)
    let ctx_pct = session.context_percent();
    if ctx_pct > 40.0 {
        score += ((ctx_pct - 40.0) / 60.0) * 40.0;
    }

    // Error acceleration contribution: 0-25 points
    if let Some(baseline) = session.baseline_error_rate {
        if baseline > 0.0 && session.error_counts_per_window.len() >= 2 {
            let recent_count = session.error_counts_per_window.len().min(3);
            let recent: f64 = session
                .error_counts_per_window
                .iter()
                .rev()
                .take(recent_count)
                .sum::<u32>() as f64
                / recent_count as f64;
            let ratio = recent / baseline;
            score += (ratio - 1.0).clamp(0.0, 1.0) * 25.0;
        }
    }

    // Token efficiency contribution: 0-20 points
    if let Some(baseline) = session.baseline_tokens_per_edit {
        if baseline > 0.0 && session.edit_event_count > 5 {
            let current =
                session.total_tokens_at_edit_count as f64 / session.edit_event_count as f64;
            let ratio = current / baseline;
            score += (ratio - 1.0).clamp(0.0, 1.0) * 20.0;
        }
    }

    // Repetition contribution: 0-15 points
    let max_rereads = session
        .file_reads_since_edit
        .values()
        .copied()
        .max()
        .unwrap_or(0);
    if max_rereads >= 2 {
        score += ((max_rereads as f64 - 1.0) / 4.0).min(1.0) * 15.0;
    }

    (score.round() as u32).min(100)
}

/// Composite cognitive decay check — wraps the decay score into a HealthCheck.
fn check_cognitive_decay(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let score = compute_decay_score(session, t);
    if score >= 80 {
        Some(HealthCheck {
            icon: "⊘",
            name: "severe decay",
            severity: Severity::Critical,
            message: format!(
                "Decay score {}/100 — session is severely compromised. Restart with fresh context.",
                score,
            ),
        })
    } else if score >= 60 {
        Some(HealthCheck {
            icon: "◉",
            name: "significant decay",
            severity: Severity::Warning,
            message: format!(
                "Decay score {}/100 — consider restarting. Generate a state transfer summary first.",
                score,
            ),
        })
    } else if score >= 30 {
        Some(HealthCheck {
            icon: "◐",
            name: "early decay",
            severity: Severity::Info,
            message: format!(
                "Decay score {}/100 — consider /compact with preservation notes.",
                score,
            ),
        })
    } else {
        None
    }
}

/// Suggest proactive compaction at moderate context usage (before degradation starts).
fn check_proactive_compaction(
    session: &ClaudeSession,
    t: &HealthThresholds,
) -> Option<HealthCheck> {
    if session.context_max == 0 {
        return None;
    }

    let pct = session.context_percent();
    if pct > t.decay_compaction_pct && pct <= t.context_warning_pct {
        Some(HealthCheck {
            icon: "📋",
            name: "consider compact",
            severity: Severity::Info,
            message: format!(
                "Context at {:.0}% — research shows degradation begins here. Consider /compact.",
                pct,
            ),
        })
    } else {
        None
    }
}

/// Detect token efficiency degradation — spending more tokens per file edit over time.
fn check_token_efficiency(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let baseline = session.baseline_tokens_per_edit?;
    if baseline < 100.0 || session.edit_event_count < 8 {
        return None;
    }

    let current = session.total_tokens_at_edit_count as f64 / session.edit_event_count as f64;
    let ratio = current / baseline;

    if ratio > t.efficiency_critical_factor {
        Some(HealthCheck {
            icon: "📉",
            name: "low efficiency",
            severity: Severity::Warning,
            message: format!(
                "Tokens per edit is {:.1}x baseline — agent is working harder for less output.",
                ratio,
            ),
        })
    } else {
        None
    }
}

/// Detect error rate acceleration — errors are increasing over time.
fn check_error_acceleration(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let baseline = session.baseline_error_rate?;
    if baseline <= 0.0 || session.error_counts_per_window.len() < 4 {
        return None;
    }

    let recent_count = session.error_counts_per_window.len().min(3);
    let recent: f64 = session
        .error_counts_per_window
        .iter()
        .rev()
        .take(recent_count)
        .sum::<u32>() as f64
        / recent_count as f64;
    let ratio = recent / baseline;

    if ratio > t.error_accel_factor {
        Some(HealthCheck {
            icon: "⚠",
            name: "error acceleration",
            severity: Severity::Warning,
            message: format!(
                "Error rate is {:.1}x baseline — agent may be stuck or confused.",
                ratio,
            ),
        })
    } else {
        None
    }
}

/// Detect file re-reads without intervening edits — possible confusion or looping.
fn check_repetition(session: &ClaudeSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let max_rereads = session
        .file_reads_since_edit
        .values()
        .copied()
        .max()
        .unwrap_or(0);

    if max_rereads >= t.repetition_threshold {
        let file = session
            .file_reads_since_edit
            .iter()
            .max_by_key(|(_, v)| *v)
            .map(|(k, _)| {
                // Show just filename
                k.rsplit('/').next().unwrap_or(k)
            })
            .unwrap_or("?");
        Some(HealthCheck {
            icon: "🔁",
            name: "repetition",
            severity: Severity::Warning,
            message: format!(
                "{} read {} times without editing — agent may be looping.",
                file, max_rereads,
            ),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{RawSession, SessionStatus, TelemetryStatus};

    fn defaults() -> HealthThresholds {
        HealthThresholds::default()
    }

    fn make_session() -> ClaudeSession {
        let raw = RawSession {
            pid: 1,
            session_id: "test".into(),
            cwd: "/tmp/test".into(),
            started_at: 0,
        };
        let mut s = ClaudeSession::from_raw(raw);
        s.status = SessionStatus::Processing;
        s.telemetry_status = TelemetryStatus::Available;
        s.model = "opus".into();
        s
    }

    #[test]
    fn healthy_session_no_warnings() {
        let s = make_session();
        assert!(check_session(&s, &defaults()).is_empty());
    }

    #[test]
    fn low_cache_critical() {
        let mut s = make_session();
        s.total_input_tokens = 100_000;
        s.cache_read_tokens = 5_000; // 5% hit ratio
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "low cache" && c.severity == Severity::Critical)
        );
    }

    #[test]
    fn low_cache_warning() {
        let mut s = make_session();
        s.total_input_tokens = 100_000;
        s.cache_read_tokens = 20_000; // 20% hit ratio
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "low cache" && c.severity == Severity::Warning)
        );
    }

    #[test]
    fn healthy_cache_no_warning() {
        let mut s = make_session();
        s.total_input_tokens = 100_000;
        s.cache_read_tokens = 60_000; // 60% hit ratio
        assert!(check_cache_health(&s, &defaults()).is_none());
    }

    #[test]
    fn context_saturation_critical() {
        let mut s = make_session();
        s.context_tokens = 190_000;
        s.context_max = 200_000;
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "context full" && c.severity == Severity::Critical)
        );
    }

    #[test]
    fn context_saturation_warning() {
        let mut s = make_session();
        s.context_tokens = 170_000;
        s.context_max = 200_000;
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "context high" && c.severity == Severity::Warning)
        );
    }

    #[test]
    fn stalled_detection() {
        let mut s = make_session();
        s.cost_usd = 10.0;
        s.elapsed = std::time::Duration::from_secs(15 * 60);
        // No files modified
        let checks = check_session(&s, &defaults());
        assert!(checks.iter().any(|c| c.name == "stalled"));
    }

    #[test]
    fn status_icon_returns_worst() {
        let mut s = make_session();
        s.context_tokens = 190_000;
        s.context_max = 200_000;
        assert_eq!(status_icon(&s, &defaults()), "🧠");
    }

    #[test]
    fn status_icon_empty_when_healthy() {
        let s = make_session();
        assert_eq!(status_icon(&s, &defaults()), "");
    }

    #[test]
    fn sorted_by_severity() {
        let mut s = make_session();
        s.total_input_tokens = 100_000;
        s.cache_read_tokens = 5_000; // Critical cache
        s.context_tokens = 170_000;
        s.context_max = 200_000; // Warning context
        let checks = check_session(&s, &defaults());
        assert!(checks.len() >= 2);
        assert_eq!(checks[0].severity, Severity::Critical);
    }

    #[test]
    fn custom_thresholds_change_trigger() {
        let mut s = make_session();
        s.total_input_tokens = 100_000;
        s.cache_read_tokens = 8_000; // 8% hit ratio — critical at default 10%

        // With defaults, this is critical
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "low cache" && c.severity == Severity::Critical)
        );

        // With relaxed threshold, this should only be a warning
        let mut relaxed = defaults();
        relaxed.cache_critical_pct = 5.0;
        let checks = check_session(&s, &relaxed);
        assert!(
            checks
                .iter()
                .any(|c| c.name == "low cache" && c.severity == Severity::Warning)
        );
        assert!(
            !checks
                .iter()
                .any(|c| c.name == "low cache" && c.severity == Severity::Critical)
        );
    }

    #[test]
    fn custom_context_thresholds() {
        let mut s = make_session();
        s.context_tokens = 170_000;
        s.context_max = 200_000; // 85% — warning at default 80%

        // With defaults, this triggers warning
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "context high" && c.severity == Severity::Warning)
        );

        // With tighter threshold (84%), 85% usage should trigger critical
        let mut tight = defaults();
        tight.context_critical_pct = 84.0;
        let checks = check_session(&s, &tight);
        assert!(
            checks
                .iter()
                .any(|c| c.name == "context full" && c.severity == Severity::Critical)
        );
    }

    // ── Cognitive decay tests ────────────────────────────────────────

    #[test]
    fn proactive_compaction_fires_at_50pct() {
        let mut s = make_session();
        s.context_tokens = 110_000;
        s.context_max = 200_000; // 55%
        s.usage_metrics_available = true;
        let check = check_proactive_compaction(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "consider compact");
        assert_eq!(c.severity, Severity::Info);
    }

    #[test]
    fn proactive_compaction_silent_below_threshold() {
        let mut s = make_session();
        s.context_tokens = 70_000;
        s.context_max = 200_000; // 35%
        assert!(check_proactive_compaction(&s, &defaults()).is_none());
    }

    #[test]
    fn token_efficiency_detects_degradation() {
        let mut s = make_session();
        s.baseline_tokens_per_edit = Some(1000.0);
        s.edit_event_count = 10;
        s.total_tokens_at_edit_count = 25_000; // 2500 per edit = 2.5x baseline
        let check = check_token_efficiency(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "low efficiency");
        assert_eq!(c.severity, Severity::Warning);
    }

    #[test]
    fn token_efficiency_silent_when_healthy() {
        let mut s = make_session();
        s.baseline_tokens_per_edit = Some(1000.0);
        s.edit_event_count = 10;
        s.total_tokens_at_edit_count = 12_000; // 1200 per edit = 1.2x baseline
        assert!(check_token_efficiency(&s, &defaults()).is_none());
    }

    #[test]
    fn error_acceleration_detects_increase() {
        let mut s = make_session();
        s.baseline_error_rate = Some(1.0);
        s.error_counts_per_window = vec![1, 1, 1, 2, 3, 4]; // rising
        let check = check_error_acceleration(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "error acceleration");
        assert_eq!(c.severity, Severity::Warning);
    }

    #[test]
    fn error_acceleration_silent_when_stable() {
        let mut s = make_session();
        s.baseline_error_rate = Some(1.0);
        s.error_counts_per_window = vec![1, 1, 1, 1]; // stable
        assert!(check_error_acceleration(&s, &defaults()).is_none());
    }

    #[test]
    fn repetition_detects_rereads() {
        let mut s = make_session();
        s.file_reads_since_edit
            .insert("/tmp/test/src/main.rs".into(), 4);
        let check = check_repetition(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "repetition");
        assert_eq!(c.severity, Severity::Warning);
        assert!(c.message.contains("main.rs"));
    }

    #[test]
    fn repetition_silent_below_threshold() {
        let mut s = make_session();
        s.file_reads_since_edit.insert("/tmp/test/foo.rs".into(), 2);
        assert!(check_repetition(&s, &defaults()).is_none());
    }

    #[test]
    fn decay_score_zero_for_fresh_session() {
        let s = make_session();
        assert_eq!(compute_decay_score(&s, &defaults()), 0);
    }

    #[test]
    fn decay_score_context_only_contribution() {
        let mut s = make_session();
        s.context_tokens = 140_000;
        s.context_max = 200_000; // 70%
        s.usage_metrics_available = true;
        let score = compute_decay_score(&s, &defaults());
        // 70% context: (70-40)/60 * 40 = 20 points
        assert_eq!(score, 20);
    }

    #[test]
    fn decay_score_high_for_saturated_session() {
        let mut s = make_session();
        s.context_tokens = 180_000;
        s.context_max = 200_000; // 90% → 33 context points
        s.usage_metrics_available = true;
        s.baseline_error_rate = Some(1.0);
        s.error_counts_per_window = vec![1, 1, 1, 3, 4, 5]; // accelerating
        s.file_reads_since_edit
            .insert("/tmp/test/main.rs".into(), 5); // repetition
        let score = compute_decay_score(&s, &defaults());
        assert!(score >= 60, "expected >= 60, got {score}");
    }

    #[test]
    fn cognitive_decay_check_critical_at_80() {
        let mut s = make_session();
        s.context_tokens = 200_000;
        s.context_max = 200_000; // 100% → 40 context points
        s.usage_metrics_available = true;
        s.baseline_tokens_per_edit = Some(1000.0);
        s.edit_event_count = 10;
        s.total_tokens_at_edit_count = 20_000; // 2x baseline → 20 efficiency points
        s.baseline_error_rate = Some(1.0);
        s.error_counts_per_window = vec![1, 1, 1, 3, 4, 5]; // → 25 error points
        let check = check_cognitive_decay(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "severe decay");
        assert_eq!(c.severity, Severity::Critical);
        assert!(c.icon == "⊘");
    }
}

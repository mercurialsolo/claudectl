// Injection feedback loop (#223): which hive units appeared in a brain prompt,
// and what happened to the brain's suggestion afterwards.
//
// Flow:
//   1. `select_for_injection` filters units by `InjectionState` + sampling.
//   2. After the prompt is built, `stash_pending(pid, ids)` writes a small
//      pid-keyed file recording which units were used.
//   3. When `log_decision` records the user's response, it calls
//      `record_outcome(pid, user_action)` which reads the pending file,
//      updates each unit's `injection_stats`, and clears the file.
//   4. After each outcome, `advance_state` runs and may promote/demote units.
//
// File-based stashing keeps the engine signature clean — `log_decision` has
// many call sites and we don't want to thread unit_ids through every one.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::store::HiveStore;
use super::{InjectionState, KnowledgeUnit, epoch_secs};

// ────────────────────────────────────────────────────────────────────────────
// Promotion thresholds — opinionated defaults, kept in code for now
// ────────────────────────────────────────────────────────────────────────────

/// Min decided outcomes (accepted + overridden) before Canary can promote.
const CANARY_MIN_DECIDED: u64 = 20;
/// Min decided outcomes before Staged can promote.
const STAGED_MIN_DECIDED: u64 = 50;
/// Win-rate threshold for promotion (≥ this advances state).
const PROMOTE_WIN_RATE: f64 = 0.70;
/// Win-rate threshold for demotion (< this rolls state back to Draft).
const DEMOTE_WIN_RATE: f64 = 0.40;
/// Min decided outcomes before demotion is allowed (avoid jumpy rollbacks).
const DEMOTE_MIN_DECIDED: u64 = 10;

// ────────────────────────────────────────────────────────────────────────────
// Sampling
// ────────────────────────────────────────────────────────────────────────────

/// Decide whether a unit should be injected for the given pid.
/// Sampling is deterministic per (pid, unit_id) so the same prompt produces
/// the same injection set on retry.
pub fn should_inject(state: InjectionState, pid: u32, unit_id: &str) -> bool {
    let buckets = state.sample_buckets();
    if buckets == 0 {
        return false;
    }
    if buckets >= 10 {
        return true;
    }
    // Cheap deterministic hash — FNV-style — over (pid, unit_id).
    let mut h = 1469598103934665603_u64;
    for b in pid.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    for b in unit_id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    (h % 10) < buckets as u64
}

// ────────────────────────────────────────────────────────────────────────────
// Pending-injection stash
// ────────────────────────────────────────────────────────────────────────────

fn pending_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("pending")
}

fn pending_path(pid: u32) -> PathBuf {
    pending_dir().join(format!("{pid}.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingInjection {
    pid: u32,
    stashed_at: u64,
    unit_ids: Vec<String>,
}

/// Record which units were injected into the prompt for `pid`. Overwrites any
/// previous stash for the same pid (a new prompt supersedes the old).
pub fn stash_pending(pid: u32, unit_ids: &[String]) -> std::io::Result<()> {
    if unit_ids.is_empty() {
        // Clear any stale stash so an empty injection doesn't accidentally
        // get attributed to a later decision.
        let _ = fs::remove_file(pending_path(pid));
        return Ok(());
    }
    let dir = pending_dir();
    fs::create_dir_all(&dir)?;
    let entry = PendingInjection {
        pid,
        stashed_at: epoch_secs(),
        unit_ids: unit_ids.to_vec(),
    };
    let json = serde_json::to_string(&entry)
        .map_err(|e| std::io::Error::other(format!("serialize: {e}")))?;
    let path = pending_path(pid);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)
}

/// Load and clear the pending stash for `pid`. Returns the unit ids that were
/// in the prompt for `pid`'s most recent brain query, or an empty Vec.
fn take_pending(pid: u32) -> Vec<String> {
    let path = pending_path(pid);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    // Best-effort delete; if it fails we'll re-attribute next time.
    let _ = fs::remove_file(&path);
    serde_json::from_str::<PendingInjection>(&raw)
        .map(|e| e.unit_ids)
        .unwrap_or_default()
}

// ────────────────────────────────────────────────────────────────────────────
// Outcome recording
// ────────────────────────────────────────────────────────────────────────────

fn classify_outcome(user_action: &str) -> Option<bool> {
    match user_action {
        // Accepted = brain's suggestion was followed (or rule agreed).
        "accept" | "auto" | "rule_approve" | "user_approve" => Some(true),
        // Overridden = user rejected what brain proposed.
        "reject" | "deny_rule_override" | "rule_deny" | "conflict_deny" => Some(false),
        // Ambiguous (e.g. user_input) — don't attribute outcome.
        _ => None,
    }
}

/// Apply the outcome of decision for `pid` to every unit that was in the
/// prompt. No-op if there's no pending stash or the action is ambiguous.
pub fn record_outcome(pid: u32, user_action: &str) {
    let outcome = match classify_outcome(user_action) {
        Some(b) => b,
        None => {
            // Still consume the stash so it doesn't carry over.
            let _ = take_pending(pid);
            return;
        }
    };
    let unit_ids = take_pending(pid);
    if unit_ids.is_empty() {
        return;
    }

    // Update the persistent store. Loading + saving the full store is
    // acceptable here — this runs after a decision, not in the hot path.
    let mut store = HiveStore::load();
    let now = epoch_secs();
    let mut touched: HashMap<String, InjectionState> = HashMap::new();

    for id in &unit_ids {
        if let Some(unit) = store.get(id).cloned() {
            let mut updated = unit;
            if outcome {
                updated.injection_stats.accepted_count += 1;
            } else {
                updated.injection_stats.overridden_count += 1;
            }
            updated.injection_stats.last_outcome_at = now;
            let new_state = advance_state(&updated.injection_state, &updated.injection_stats);
            updated.injection_state = new_state;
            touched.insert(id.clone(), new_state);
            store.insert(updated);
        }
    }

    if !touched.is_empty() {
        // Best-effort save — the store is local-only, never block the hook.
        let _ = store.save();
        let _ = log_event(pid, &unit_ids, outcome, &touched);
    }
}

/// Bump injection counters for a set of units, called right after a prompt is
/// built. Saves the store to persist the increment.
pub fn record_injections(unit_ids: &[String]) {
    if unit_ids.is_empty() {
        return;
    }
    let mut store = HiveStore::load();
    let now = epoch_secs();
    let mut changed = false;
    for id in unit_ids {
        if let Some(unit) = store.get(id).cloned() {
            let mut updated = unit;
            updated.injection_stats.injected_count += 1;
            updated.injection_stats.last_injected_at = now;
            store.insert(updated);
            changed = true;
        }
    }
    if changed {
        let _ = store.save();
    }
}

// ────────────────────────────────────────────────────────────────────────────
// State machine
// ────────────────────────────────────────────────────────────────────────────

/// Decide the next state for a unit given its current state + accumulated
/// stats. Pure function — does not mutate the unit.
pub fn advance_state(current: &InjectionState, stats: &super::InjectionStats) -> InjectionState {
    let win_rate = stats.win_rate();
    let decided = stats.decided();

    // Demotion path — same threshold for every state, except Draft (already at
    // the bottom). A low win rate after sufficient evidence rolls back to
    // Draft so the unit stops appearing until it's re-validated.
    if !matches!(current, InjectionState::Draft)
        && decided >= DEMOTE_MIN_DECIDED
        && win_rate < DEMOTE_WIN_RATE
    {
        return InjectionState::Draft;
    }

    match current {
        InjectionState::Draft => InjectionState::Draft, // promotion is manual
        InjectionState::Canary => {
            if decided >= CANARY_MIN_DECIDED && win_rate >= PROMOTE_WIN_RATE {
                InjectionState::Staged
            } else {
                InjectionState::Canary
            }
        }
        InjectionState::Staged => {
            if decided >= STAGED_MIN_DECIDED && win_rate >= PROMOTE_WIN_RATE {
                InjectionState::Live
            } else {
                InjectionState::Staged
            }
        }
        InjectionState::Live => InjectionState::Live,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Event log (for autopsy / metrics)
// ────────────────────────────────────────────────────────────────────────────

fn events_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("feedback_events.jsonl")
}

fn log_event(
    pid: u32,
    unit_ids: &[String],
    accepted: bool,
    state_changes: &HashMap<String, InjectionState>,
) -> std::io::Result<()> {
    let path = events_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let states: Vec<_> = state_changes
        .iter()
        .map(|(id, s)| serde_json::json!({ "unit_id": id, "new_state": s.label() }))
        .collect();
    let record = serde_json::json!({
        "ts": epoch_secs(),
        "pid": pid,
        "accepted": accepted,
        "unit_ids": unit_ids,
        "state_changes": states,
    });
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{record}")
}

// ────────────────────────────────────────────────────────────────────────────
// Filter helper used by injection.rs
// ────────────────────────────────────────────────────────────────────────────

/// Whether a unit passes the rollout sampling for `pid`. Returns true for
/// every unit when `pid` is None (used by callers that don't have a session
/// context, e.g. CLI listings).
pub fn passes_rollout(unit: &KnowledgeUnit, pid: Option<u32>) -> bool {
    match pid {
        None => true,
        Some(p) => should_inject(unit.injection_state, p, &unit.id),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::{InjectionStats, KnowledgeContent, KnowledgeScope};

    fn unit(id: &str, state: InjectionState) -> KnowledgeUnit {
        KnowledgeUnit {
            id: id.into(),
            scope: KnowledgeScope::Universal,
            category: crate::hive::KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Pattern {
                tool: "Bash".into(),
                command_pattern: Some("ls".into()),
                preferred_action: "approve".into(),
                accept_rate: 0.9,
                sample_count: 10,
                conditions: vec![],
            },
            evidence_count: 10,
            confidence: 0.9,
            source_peer: "peer".into(),
            originated_at: 0,
            last_validated_at: 0,
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 0,
            injection_state: state,
            injection_stats: InjectionStats::default(),
        }
    }

    #[test]
    fn draft_never_injects() {
        for pid in [1u32, 1234, 999_999] {
            assert!(!should_inject(InjectionState::Draft, pid, "ku_x"));
        }
    }

    #[test]
    fn live_always_injects() {
        for pid in [1u32, 1234, 999_999] {
            assert!(should_inject(InjectionState::Live, pid, "ku_x"));
        }
    }

    #[test]
    fn canary_samples_roughly_ten_percent() {
        let mut hits = 0;
        let n = 1000;
        for pid in 0..n {
            if should_inject(InjectionState::Canary, pid as u32, "ku_x") {
                hits += 1;
            }
        }
        // Expect ~10%; allow 5% – 16% to keep test deterministic but not flaky.
        assert!(
            hits > 50 && hits < 160,
            "canary samples out of range: {hits}/{n}"
        );
    }

    #[test]
    fn staged_samples_roughly_half() {
        let mut hits = 0;
        let n = 1000;
        for pid in 0..n {
            if should_inject(InjectionState::Staged, pid as u32, "ku_x") {
                hits += 1;
            }
        }
        assert!(
            hits > 400 && hits < 600,
            "staged samples out of range: {hits}/{n}"
        );
    }

    #[test]
    fn sampling_is_deterministic() {
        // Same (pid, id) → same answer on repeated calls.
        let pid = 42;
        let id = "ku_repro";
        let first = should_inject(InjectionState::Canary, pid, id);
        for _ in 0..50 {
            assert_eq!(should_inject(InjectionState::Canary, pid, id), first);
        }
    }

    #[test]
    fn classify_known_actions() {
        assert_eq!(classify_outcome("accept"), Some(true));
        assert_eq!(classify_outcome("auto"), Some(true));
        assert_eq!(classify_outcome("reject"), Some(false));
        assert_eq!(classify_outcome("deny_rule_override"), Some(false));
        assert_eq!(classify_outcome("user_input"), None);
        assert_eq!(classify_outcome("unknown"), None);
    }

    #[test]
    fn advance_canary_promotes_with_signal() {
        let stats = InjectionStats {
            injected_count: 30,
            accepted_count: 18,
            overridden_count: 4, // 22 decided, 18/22 ≈ 0.82
            ..Default::default()
        };
        assert_eq!(
            advance_state(&InjectionState::Canary, &stats),
            InjectionState::Staged
        );
    }

    #[test]
    fn advance_canary_holds_without_evidence() {
        let stats = InjectionStats {
            accepted_count: 5,
            overridden_count: 1, // only 6 decided (< 20)
            ..Default::default()
        };
        assert_eq!(
            advance_state(&InjectionState::Canary, &stats),
            InjectionState::Canary
        );
    }

    #[test]
    fn advance_staged_promotes_to_live() {
        let stats = InjectionStats {
            accepted_count: 40,
            overridden_count: 12, // 52 decided, 40/52 ≈ 0.77
            ..Default::default()
        };
        assert_eq!(
            advance_state(&InjectionState::Staged, &stats),
            InjectionState::Live
        );
    }

    #[test]
    fn advance_demotes_on_low_win_rate() {
        let stats = InjectionStats {
            accepted_count: 3,
            overridden_count: 9, // 12 decided, 25% win
            ..Default::default()
        };
        assert_eq!(
            advance_state(&InjectionState::Canary, &stats),
            InjectionState::Draft
        );
        assert_eq!(
            advance_state(&InjectionState::Staged, &stats),
            InjectionState::Draft
        );
        assert_eq!(
            advance_state(&InjectionState::Live, &stats),
            InjectionState::Draft
        );
    }

    #[test]
    fn advance_does_not_demote_without_evidence() {
        let stats = InjectionStats {
            accepted_count: 1,
            overridden_count: 4, // 5 decided (< DEMOTE_MIN_DECIDED)
            ..Default::default()
        };
        assert_eq!(
            advance_state(&InjectionState::Canary, &stats),
            InjectionState::Canary
        );
    }

    #[test]
    fn passes_rollout_with_no_pid_is_unconditional() {
        let u = unit("ku_test", InjectionState::Canary);
        assert!(passes_rollout(&u, None));
        let d = unit("ku_test", InjectionState::Draft);
        assert!(passes_rollout(&d, None));
    }

    #[test]
    fn injection_stats_win_rate() {
        let s = InjectionStats {
            accepted_count: 7,
            overridden_count: 3,
            ..Default::default()
        };
        assert!((s.win_rate() - 0.7).abs() < 1e-9);

        let empty = InjectionStats::default();
        assert_eq!(empty.win_rate(), 0.0); // no division-by-zero
    }
}

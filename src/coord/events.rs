// Allow dead_code: the v1 event schema is the public contract CI gates,
// Slack bots, and dashboards will read; the supervisor emits these from
// `commands.rs` once PR8 unifies the headless event stream.
#![allow(dead_code)]
//! v1 supervisor event schema (#349, RFC §10).
//!
//! Frozen at v1, additive-only. The schema lives here so consumers can
//! depend on a single source of truth: every `Event` carries `v: 1` and
//! a `type` discriminator. New event types are added; existing ones
//! never have fields removed or renamed.
//!
//! Emission paths:
//!
//! - `claudectl --watch --json` (NDJSON over stdout)
//! - Webhook delivery (when configured)
//! - The coord `events` table (durable replay for crash recovery)
//!
//! Three event families ship in this PR:
//!
//! - `task.transition` — every state-machine move.
//! - `task.verification` — every verifier verdict.
//! - `task.escalated` — every transition into NeedsHuman.
//!
//! Adding events later is fine; renaming or removing fields is a
//! breaking change to the contract and requires a v2.

use serde::{Deserialize, Serialize};

/// The frozen envelope every event ships in. The `v: 1` field is the
/// single byte consumers should branch on if they ever need to handle
/// multiple schema versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Schema version. Always 1 in this PR; bumped only on a
    /// non-additive change.
    pub v: u32,
    /// Event variant; discriminator for the `payload`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// ISO 8601 UTC timestamp.
    pub at: String,
    /// Variant payload.
    #[serde(flatten)]
    pub payload: EventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventPayload {
    Transition(Transition),
    Verification(Verification),
    Escalated(Escalated),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub task_id: String,
    pub from: String,
    pub to: String,
    pub cause: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub task_id: String,
    pub attempt_id: String,
    pub kind: String,
    pub verdict: String,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalated {
    pub task_id: String,
    pub reason: String,
    pub addressed_to: String,
}

impl Event {
    pub fn transition(at: String, t: Transition) -> Self {
        Self {
            v: 1,
            event_type: "task.transition".into(),
            at,
            payload: EventPayload::Transition(t),
        }
    }
    pub fn verification(at: String, v: Verification) -> Self {
        Self {
            v: 1,
            event_type: "task.verification".into(),
            at,
            payload: EventPayload::Verification(v),
        }
    }
    pub fn escalated(at: String, e: Escalated) -> Self {
        Self {
            v: 1,
            event_type: "task.escalated".into(),
            at,
            payload: EventPayload::Escalated(e),
        }
    }

    /// Serialize as one NDJSON line (no trailing newline; the caller
    /// adds it). Frozen contract: any change to field naming or order
    /// is breaking.
    pub fn to_ndjson(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| format!("encode event: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_event_round_trips() {
        let e = Event::transition(
            "2026-06-10T05:30:00Z".into(),
            Transition {
                task_id: "task_42".into(),
                from: "ASSIGNED".into(),
                to: "RUNNING".into(),
                cause: "spawned".into(),
            },
        );
        let json = e.to_ndjson().unwrap();
        // Frozen contract: `v`, `type`, `at`, and the inlined payload
        // fields all live at the top level.
        assert!(json.contains(r#""v":1"#));
        assert!(json.contains(r#""type":"task.transition""#));
        assert!(json.contains(r#""task_id":"task_42""#));
        assert!(json.contains(r#""from":"ASSIGNED""#));
        assert!(json.contains(r#""to":"RUNNING""#));
        assert!(json.contains(r#""cause":"spawned""#));
        // Re-parse and confirm round-trip preserves the discriminator.
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.v, 1);
        assert_eq!(parsed.event_type, "task.transition");
    }

    #[test]
    fn verification_event_carries_cost_and_kind() {
        let e = Event::verification(
            "2026-06-10T05:30:01Z".into(),
            Verification {
                task_id: "task_42".into(),
                attempt_id: "attempt_1".into(),
                kind: "agent".into(),
                verdict: "FAIL".into(),
                cost_usd: 0.18,
            },
        );
        let json = e.to_ndjson().unwrap();
        assert!(json.contains(r#""kind":"agent""#));
        assert!(json.contains(r#""verdict":"FAIL""#));
        assert!(json.contains(r#""cost_usd":0.18"#));
    }

    #[test]
    fn escalated_event_names_recipient_role() {
        let e = Event::escalated(
            "2026-06-10T05:30:02Z".into(),
            Escalated {
                task_id: "task_42".into(),
                reason: "retries_exhausted".into(),
                addressed_to: "operator".into(),
            },
        );
        let json = e.to_ndjson().unwrap();
        assert!(json.contains(r#""addressed_to":"operator""#));
        assert!(json.contains(r#""reason":"retries_exhausted""#));
    }
}

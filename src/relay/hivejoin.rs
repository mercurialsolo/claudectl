//! The hive join handshake (#434).
//!
//! `join_policy` is something only the host can enforce. A joiner holding a
//! valid PSK can pair and talk regardless — so "admitted to the hive" has to be
//! a decision the host makes and records, not a property of the link. That
//! decision travels as two messages:
//!
//! ```text
//! joiner ──HiveJoinRequest{hive_id?}──▶ host
//! joiner ◀──HiveJoinResult{state}───── host
//! ```
//!
//! `decide_join` is the whole policy, kept here rather than in the serve loop so
//! it can be tested against a temporary `HOME` without a socket.

use serde::{Deserialize, Serialize};

use super::{MessageType, RelayMessage, epoch_ms, gen_msg_id};
use crate::hive::identity::{HiveIdentity, JoinPolicy};
use crate::hive::membership::{self, Admission, Roster};

/// What a joiner asks for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequestPayload {
    /// The hive the joiner believes it is joining.
    ///
    /// Optional on purpose: an invite *link* names the hive, but a relay code
    /// and a word phrase cannot carry one (nine bytes, all of them spent on the
    /// address and PSK). When it is present and wrong, the host refuses — that
    /// catches a link meant for a different hive. When it is absent, the host
    /// answers for whatever hive it runs.
    #[serde(default)]
    pub hive_id: Option<String>,
    /// What the joiner calls itself, for the owner reading the queue. Advisory.
    #[serde(default)]
    pub label: Option<String>,
}

/// How the host answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinResultState {
    /// In the hive. Knowledge will flow.
    Member,
    /// Queued for the owner to decide on. Nothing flows yet.
    Pending,
    /// Not joining. `reason` says why.
    Refused,
}

impl JoinResultState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JoinResultState::Member => "member",
            JoinResultState::Pending => "pending",
            JoinResultState::Refused => "refused",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinResultPayload {
    pub state: JoinResultState,
    #[serde(default)]
    pub hive_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Build the request a joiner sends after pairing.
pub fn build_join_request(from: &str, hive_id: Option<&str>, label: Option<&str>) -> RelayMessage {
    let payload = JoinRequestPayload {
        hive_id: hive_id.map(|s| s.to_string()),
        label: label.map(|s| s.to_string()),
    };
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::HiveJoinRequest,
        from_peer: from.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// Build the host's answer.
pub fn build_join_result(from: &str, result: &JoinResultPayload) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::HiveJoinResult,
        from_peer: from.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
    }
}

/// Decide what to do about a join request, and record the decision.
///
/// `peer_id` must be the **authenticated** peer id the connection resolved to,
/// never the `from_peer` string off the message — it becomes a filename.
///
/// Order matters: an existing member is answered before the policy is consulted,
/// so re-running `hive join` is idempotent and refreshes a joiner whose own
/// record still says `pending` (that is how a joiner learns it was approved
/// while it was not connected).
pub fn decide_join(
    identity: Option<&HiveIdentity>,
    roster: &Roster,
    peer_id: &str,
    request: &JoinRequestPayload,
) -> JoinResultPayload {
    let refused = |reason: &str| JoinResultPayload {
        state: JoinResultState::Refused,
        hive_id: None,
        name: None,
        reason: Some(reason.to_string()),
    };

    let Some(identity) = identity else {
        return refused(
            "this machine is not running a named hive — ask its owner to run \
             `claudectl hive identity set --name <name>`",
        );
    };

    if !membership::is_valid_member_id(peer_id) {
        return refused("that peer id cannot be recorded");
    }

    // A link for someone else's hive must not quietly join this one.
    if let Some(asked) = request.hive_id.as_deref() {
        if asked != identity.hive_id {
            return refused(&format!(
                "this machine runs hive {} ({}), not {asked}",
                identity.name, identity.hive_id
            ));
        }
    }

    let admitted = JoinResultPayload {
        state: JoinResultState::Member,
        hive_id: Some(identity.hive_id.clone()),
        name: Some(identity.name.clone()),
        reason: None,
    };

    if roster.is_member(peer_id) {
        return admitted;
    }

    // `effective_join_policy` is what downgrades an unacknowledged `open` to
    // `invite` (#432's Q5 gate), so reading it here means an unconfirmed `open`
    // cannot admit anyone.
    match identity.effective_join_policy() {
        // Both admit on pairing today — see the note in docs/relay.md. The PSK
        // is per-host rather than per-invite, so the host genuinely cannot tell
        // which link a peer used; the policies differ in owner intent and in
        // what gets advertised over LAN, not yet in what the host enforces.
        JoinPolicy::Invite | JoinPolicy::Open => {
            match roster.admit(peer_id, &identity.hive_id, Admission::Policy) {
                Ok(_) => admitted,
                Err(e) => refused(&format!("could not record membership: {e}")),
            }
        }
        JoinPolicy::Ask => {
            // Already queued? Say so again rather than stacking duplicates.
            if let Some(existing) = roster.pending_for(peer_id) {
                return JoinResultPayload {
                    state: JoinResultState::Pending,
                    hive_id: Some(identity.hive_id.clone()),
                    name: Some(identity.name.clone()),
                    reason: Some(format!(
                        "already waiting for the owner to decide (request {})",
                        existing.id
                    )),
                };
            }
            match roster.record_request(peer_id, &identity.hive_id, request.label.clone()) {
                Ok(entry) => JoinResultPayload {
                    state: JoinResultState::Pending,
                    hive_id: Some(identity.hive_id.clone()),
                    name: Some(identity.name.clone()),
                    reason: Some(format!(
                        "this hive admits by approval — request {} is waiting for its owner",
                        entry.id
                    )),
                },
                Err(e) => refused(&format!("could not record the join request: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::identity::HiveIdentity;
    use crate::hive::membership::{MemberState, RequestState};

    const PEER: &str = "laptop-a3f2";

    fn hive(policy: JoinPolicy, acknowledged: bool) -> HiveIdentity {
        HiveIdentity {
            hive_id: "hv_3a9f21".into(),
            name: "barrys-hive".into(),
            description: None,
            join_policy: policy,
            created_ms: 1_700_000_000_000,
            open_acknowledged_ms: if acknowledged { Some(1) } else { None },
        }
    }

    fn roster() -> (tempfile::TempDir, Roster) {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        (tmp, r)
    }

    fn ask_for(hive_id: Option<&str>) -> JoinRequestPayload {
        JoinRequestPayload {
            hive_id: hive_id.map(|s| s.into()),
            label: None,
        }
    }

    #[test]
    fn an_invite_hive_admits_a_paired_peer_that_asks() {
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Invite, false);
        let out = decide_join(Some(&id), &r, PEER, &ask_for(Some("hv_3a9f21")));
        assert_eq!(out.state, JoinResultState::Member);
        assert!(r.is_member(PEER));
    }

    #[test]
    fn an_ask_hive_queues_instead_of_admitting() {
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Ask, false);
        let out = decide_join(Some(&id), &r, PEER, &ask_for(Some("hv_3a9f21")));

        // This is #434's acceptance line: holding the link is not being let in.
        assert_eq!(out.state, JoinResultState::Pending);
        assert!(!r.is_member(PEER), "a pending peer is not a member");

        let queued = r.pending_for(PEER).expect("the request was queued");
        assert_eq!(r.request_state(&queued), RequestState::Pending);
        assert_eq!(queued.hive_id, "hv_3a9f21");
    }

    #[test]
    fn asking_an_ask_hive_twice_does_not_stack_requests() {
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Ask, false);
        assert_eq!(
            decide_join(Some(&id), &r, PEER, &ask_for(None)).state,
            JoinResultState::Pending
        );
        let second = decide_join(Some(&id), &r, PEER, &ask_for(None));
        assert_eq!(second.state, JoinResultState::Pending);
        assert!(second.reason.unwrap().contains("already waiting"));
        assert_eq!(r.pending_requests().len(), 1);
    }

    #[test]
    fn approving_then_asking_again_answers_member() {
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Ask, false);
        decide_join(Some(&id), &r, PEER, &ask_for(None));
        let queued = r.pending_for(PEER).unwrap();
        r.admit(&queued.peer_id, &id.hive_id, Admission::Approved)
            .unwrap();

        // How a joiner whose own record still says `pending` finds out.
        let out = decide_join(Some(&id), &r, PEER, &ask_for(None));
        assert_eq!(out.state, JoinResultState::Member);
    }

    #[test]
    fn a_denied_peer_does_not_become_a_member_by_asking_again() {
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Ask, false);
        decide_join(Some(&id), &r, PEER, &ask_for(None));
        let queued = r.pending_for(PEER).unwrap();
        r.deny(&queued).unwrap();

        let out = decide_join(Some(&id), &r, PEER, &ask_for(None));
        assert_eq!(out.state, JoinResultState::Pending);
        assert!(!r.is_member(PEER), "a denial is not undone by re-asking");
    }

    #[test]
    fn a_link_for_another_hive_is_refused() {
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Invite, false);
        let out = decide_join(Some(&id), &r, PEER, &ask_for(Some("hv_somebody_else")));
        assert_eq!(out.state, JoinResultState::Refused);
        assert!(out.reason.unwrap().contains("hv_somebody_else"));
        assert!(!r.is_member(PEER));
    }

    #[test]
    fn a_request_without_a_hive_id_is_answered_by_whatever_hive_runs_here() {
        // A relay code and a word phrase cannot carry a hive id, and #434's
        // acceptance says all three formats must work.
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Invite, false);
        let out = decide_join(Some(&id), &r, PEER, &ask_for(None));
        assert_eq!(out.state, JoinResultState::Member);
        assert_eq!(out.hive_id.as_deref(), Some("hv_3a9f21"));
    }

    #[test]
    fn an_unnamed_machine_refuses_rather_than_inventing_a_hive() {
        let (_t, r) = roster();
        let out = decide_join(None, &r, PEER, &ask_for(None));
        assert_eq!(out.state, JoinResultState::Refused);
        assert!(out.reason.unwrap().contains("not running a named hive"));
    }

    #[test]
    fn an_unacknowledged_open_hive_is_decided_on_its_effective_policy() {
        // #432's Q5 gate: a hand-edited `open` that was never confirmed falls
        // back to `invite`. Both admit on request today, so what this pins is
        // that `decide_join` reads `effective_join_policy` at all — the moment
        // the two rules diverge, an unconfirmed `open` must not get the open one.
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Open, false);
        assert_eq!(id.effective_join_policy(), JoinPolicy::Invite);
        let out = decide_join(Some(&id), &r, PEER, &ask_for(None));
        assert_eq!(out.state, JoinResultState::Member);
        assert_eq!(r.list_members()[0].admission, Admission::Policy);
    }

    #[test]
    fn a_peer_id_that_cannot_be_recorded_is_refused_before_anything_is_written() {
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Invite, true);
        let out = decide_join(Some(&id), &r, "../../etc/passwd", &ask_for(None));
        assert_eq!(out.state, JoinResultState::Refused);
        assert!(r.list_members().is_empty());
        assert!(r.pending_requests().is_empty());
    }

    #[test]
    fn the_request_survives_the_wire() {
        let msg = build_join_request("laptop-a3f2", Some("hv_1"), Some("my-box"));
        assert_eq!(msg.msg_type, MessageType::HiveJoinRequest);
        let back: JoinRequestPayload = serde_json::from_value(msg.payload).unwrap();
        assert_eq!(back.hive_id.as_deref(), Some("hv_1"));
        assert_eq!(back.label.as_deref(), Some("my-box"));
    }

    #[test]
    fn a_refusal_reaches_the_joiner_as_an_error_not_a_silent_nothing() {
        let payload = JoinResultPayload {
            state: JoinResultState::Refused,
            hive_id: None,
            name: None,
            reason: Some("not running a named hive".into()),
        };
        let msg = build_join_result("host-1", &payload);
        assert_eq!(msg.msg_type, MessageType::HiveJoinResult);
        let err = crate::hive::cli::apply_join_result("host-1", &msg.payload).unwrap_err();
        assert!(err.contains("not running a named hive"));
    }

    #[test]
    fn member_state_names_match_the_wire_words() {
        // `apply_join_result` matches on these strings, so a rename on either
        // side has to break something rather than silently stop recording.
        assert_eq!(
            JoinResultState::Member.as_str(),
            MemberState::Member.as_str()
        );
        assert_eq!(
            JoinResultState::Pending.as_str(),
            MemberState::Pending.as_str()
        );
    }
}

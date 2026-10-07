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
use crate::hive::membership::{self, Admission, Role, Roster};

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
    /// A `hive.read:<hive-name>` capability token, for joining as a reader
    /// (#435, §7.5).
    ///
    /// Not advisory — this one is checked. It is the owner's explicit
    /// authorisation for a peer to receive knowledge without contributing any,
    /// so a valid grant admits directly rather than queueing, whatever the
    /// `join_policy` is: the owner already decided when they minted it.
    #[serde(default)]
    pub grant: Option<String>,
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
    /// What the joiner may do, once admitted (#435). Absent means contributor,
    /// which is what every admission before #435 was.
    #[serde(default)]
    pub role: Option<String>,
}

/// Build the request a joiner sends after pairing.
pub fn build_join_request(
    from: &str,
    hive_id: Option<&str>,
    label: Option<&str>,
    grant: Option<&str>,
) -> RelayMessage {
    let payload = JoinRequestPayload {
        hive_id: hive_id.map(|s| s.to_string()),
        label: label.map(|s| s.to_string()),
        grant: grant.map(|s| s.to_string()),
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
        role: None,
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

    let admitted = |role: Role, reason: Option<String>| JoinResultPayload {
        state: JoinResultState::Member,
        hive_id: Some(identity.hive_id.clone()),
        name: Some(identity.name.clone()),
        reason,
        role: Some(role.as_str().to_string()),
    };

    // Already in? Answer with the role the roster holds, which is what makes
    // re-running `hive join` idempotent for a reader as well as a contributor.
    if let Some(role) = roster.role(peer_id) {
        return admitted(role, None);
    }

    // A `hive.read` grant is the owner's explicit authorisation, so it is
    // checked before `join_policy` and admits directly — including on an `ask`
    // hive, where queueing a peer the owner already wrote a grant for would be
    // asking them the same question twice.
    if let Some(token) = request.grant.as_deref() {
        return match verify_reader_grant(token, &identity.name) {
            Ok(grant_id) => match roster.admit_as(
                peer_id,
                &identity.hive_id,
                Admission::Grant,
                Role::Reader,
                Some(grant_id),
            ) {
                Ok(_) => admitted(
                    Role::Reader,
                    Some("admitted as a reader on a hive.read grant".to_string()),
                ),
                Err(e) => refused(&format!("could not record membership: {e}")),
            },
            // Deliberately not detailed: the same opaque refusal the rest of
            // the grant surface gives, so this cannot be used to probe which
            // grant ids exist (RFC §3.3).
            Err(()) => refused(
                "that hive.read grant was refused — it may be unknown, expired, revoked, \
                 or scoped to a different hive",
            ),
        };
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
                Ok(_) => admitted(Role::Contributor, None),
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
                    role: None,
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
                    role: None,
                },
                Err(e) => refused(&format!("could not record the join request: {e}")),
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Refusing knowledge (#435)
// ────────────────────────────────────────────────────────────────────────────

/// Why a batch of knowledge was refused, and how much of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeRejectedPayload {
    pub reason: String,
    /// How many units were dropped, so the sender can see the scale of what it
    /// thought it was contributing.
    #[serde(default)]
    pub units: usize,
}

/// Build the refusal a host sends when it will not take a peer's knowledge.
///
/// §7.5's acceptance asks for a rejection "at the protocol level, not merely
/// ignored". This is that: a reader that believes it is contributing learns
/// otherwise, and the alternative — dropping the units and logging locally —
/// is indistinguishable at the sender from a network fault.
pub fn build_knowledge_rejected(from: &str, reason: &str, units: usize) -> RelayMessage {
    let payload = KnowledgeRejectedPayload {
        reason: reason.to_string(),
        units,
    };
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::KnowledgeRejected,
        from_peer: from.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// How many units a `KnowledgeSync` payload carries, for the refusal's count.
///
/// Best-effort: the payload is already being refused, so a shape this cannot
/// read is reported as zero rather than failing the refusal itself.
pub fn count_units(payload: &serde_json::Value) -> usize {
    payload
        .get("units")
        .and_then(|u| u.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// The reason out of a refusal, for printing.
pub fn rejection_reason(payload: &serde_json::Value) -> String {
    let reason = payload
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("no reason given");
    match payload.get("units").and_then(|u| u.as_u64()) {
        Some(n) if n > 0 => format!("{reason} ({n} unit(s) dropped)"),
        _ => reason.to_string(),
    }
}

/// Verify a `hive.read:<hive_name>` token, returning the grant id it proved.
///
/// `Err(())` carries no detail on purpose: the caller turns it into one opaque
/// refusal, exactly as the query surface does, so a joiner cannot learn which
/// grant ids exist by watching which refusals differ.
///
/// The scope is built from *this* hive's name, so a grant for another hive fails
/// the scope check rather than being compared against something the joiner sent.
/// One consequence worth knowing: renaming a hive invalidates every outstanding
/// reader grant, because the name is the scope qualifier.
fn verify_reader_grant(token: &str, hive_name: &str) -> Result<String, ()> {
    use crate::access::scope::Scope;

    let store = crate::access::grant::GrantStore::open_default().map_err(|_| ())?;
    let secret = crate::access::token::load_or_create_secret(store.root()).map_err(|_| ())?;
    let want = Scope::HiveRead(hive_name.to_string());
    let grant = store
        .verify(
            &secret,
            token,
            Some(&want),
            crate::hive::identity::epoch_ms(),
        )
        .map_err(|_| ())?;
    // Audited like any other use of a grant, so an owner can see a reader
    // being admitted on it in `access audit`.
    let _ = store.record_use(
        &grant.grant_id,
        Some("hive.join"),
        crate::hive::identity::epoch_ms(),
    );
    Ok(grant.grant_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::identity::HiveIdentity;
    use crate::hive::membership::{MemberState, RequestState, Role};

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
            grant: None,
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
        let msg = build_join_request("laptop-a3f2", Some("hv_1"), Some("my-box"), None);
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
            role: None,
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

    // ── Reader admission and refusal (#435, §7.5) ─────────────────────────

    #[test]
    fn a_reader_already_on_the_roster_is_answered_as_a_reader() {
        // The idempotence path: re-running `hive join` must not quietly report a
        // reader as a full member, which is what a bare `is_member` check would
        // have done.
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Ask, false);
        r.admit_as(
            PEER,
            &id.hive_id,
            Admission::Grant,
            Role::Reader,
            Some("g_abc".into()),
        )
        .unwrap();

        let out = decide_join(Some(&id), &r, PEER, &ask_for(None));
        assert_eq!(out.state, JoinResultState::Member);
        assert_eq!(out.role.as_deref(), Some("reader"));
    }

    #[test]
    fn a_contributor_already_on_the_roster_is_answered_as_a_contributor() {
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Ask, false);
        r.admit(PEER, &id.hive_id, Admission::Approved).unwrap();
        let out = decide_join(Some(&id), &r, PEER, &ask_for(None));
        assert_eq!(out.role.as_deref(), Some("contributor"));
    }

    #[test]
    fn an_unverifiable_grant_is_refused_opaquely_and_admits_nobody() {
        // No grant store exists under this roster's root, so verification
        // cannot succeed. What matters is that it refuses rather than falling
        // through to the join policy — a bogus grant must not get someone in as
        // a contributor by the back door.
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Invite, false);
        let request = JoinRequestPayload {
            hive_id: None,
            label: None,
            grant: Some("cctl_deadbeef_0000000000000000".into()),
        };
        let out = decide_join(Some(&id), &r, PEER, &request);

        assert_eq!(out.state, JoinResultState::Refused);
        assert!(!r.is_member(PEER), "a refused grant must admit nobody");
        // Opaque: it must not say which of unknown/expired/revoked/wrong-hive.
        let reason = out.reason.unwrap();
        assert!(reason.contains("hive.read grant was refused"), "{reason}");
    }

    #[test]
    fn a_bogus_grant_on_an_invite_hive_does_not_fall_through_to_the_policy() {
        // Belt and braces on the branch order: `invite` would otherwise admit
        // this peer as a contributor, which is strictly more than they asked
        // for and the opposite of fail-closed.
        let (_t, r) = roster();
        let id = hive(JoinPolicy::Invite, false);
        let request = JoinRequestPayload {
            hive_id: None,
            label: None,
            grant: Some("not-even-a-token".into()),
        };
        assert_eq!(
            decide_join(Some(&id), &r, PEER, &request).state,
            JoinResultState::Refused
        );
        assert!(r.list_members().is_empty());
    }

    #[test]
    fn a_grant_bearing_request_survives_the_wire() {
        let msg = build_join_request("laptop-a3f2", None, None, Some("cctl_abc_def"));
        let back: JoinRequestPayload = serde_json::from_value(msg.payload).unwrap();
        assert_eq!(back.grant.as_deref(), Some("cctl_abc_def"));
    }

    #[test]
    fn a_pre_435_host_answering_without_a_role_reads_as_contributor() {
        // Forward compatibility in the direction that matters: a host that does
        // not know about roles only ever admitted contributors, so an absent
        // field must mean contributor, not reader.
        let payload = serde_json::json!({
            "state": "member",
            "hive_id": "hv_1",
            "name": "barrys-hive",
        });
        // Goes through the joiner's own reader, which is what records the role.
        assert!(crate::hive::cli::apply_join_result("host-1", &payload).is_ok());
    }

    #[test]
    fn a_refusal_names_a_reason_and_counts_what_it_dropped() {
        let msg = build_knowledge_rejected("host-1", "readers do not contribute", 3);
        assert_eq!(msg.msg_type, MessageType::KnowledgeRejected);

        let back: KnowledgeRejectedPayload = serde_json::from_value(msg.payload.clone()).unwrap();
        assert_eq!(back.units, 3);
        assert_eq!(back.reason, "readers do not contribute");

        // And it renders for the sender, which is the point of sending it.
        let rendered = rejection_reason(&msg.payload);
        assert!(rendered.contains("readers do not contribute"), "{rendered}");
        assert!(rendered.contains("3 unit(s)"), "{rendered}");
    }

    #[test]
    fn counting_units_tolerates_a_payload_it_cannot_read() {
        // The batch is already being refused; a shape this cannot parse must not
        // make the refusal itself fail.
        assert_eq!(count_units(&serde_json::json!({"units": [1, 2]})), 2);
        assert_eq!(count_units(&serde_json::json!({"units": "nope"})), 0);
        assert_eq!(count_units(&serde_json::json!({})), 0);
        assert_eq!(count_units(&serde_json::Value::Null), 0);
    }

    #[test]
    fn a_refusal_with_no_reason_still_says_something() {
        assert_eq!(
            rejection_reason(&serde_json::json!({})),
            "no reason given",
            "silence on the wire must not become silence on the terminal"
        );
    }
}

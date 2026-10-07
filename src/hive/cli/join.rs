//! Hive membership from the CLI (#434): the owner's queue, and the joiner's
//! record of where it stands.
//!
//! The connecting half lives in `relay::cli` — it needs sockets and PSKs. This
//! half only touches `hive::membership`, so it stays on the hive side of the
//! line and keeps working under `--no-default-features --features hive` for
//! everything that does not involve a peer.

use std::io;

use crate::hive::membership::{self, Admission, JoinRequest, MemberState, Membership};

/// Record what a host said about our join request.
///
/// Called from two places that look unrelated but are the same event: `hive
/// join` waiting on the reply, and a relay loop receiving it later because the
/// owner approved a queued request while we were not connected. That is why the
/// membership file is written here rather than by the command — the approval
/// usually arrives long after `hive join` has exited.
///
/// Returns a line worth printing, or `None` when nothing changed.
pub fn apply_join_result(
    from_peer: &str,
    payload: &serde_json::Value,
) -> Result<Option<String>, String> {
    #[derive(serde::Deserialize)]
    struct Incoming {
        state: String,
        #[serde(default)]
        hive_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    }

    let incoming: Incoming = serde_json::from_value(payload.clone())
        .map_err(|e| format!("unreadable hive join result: {e}"))?;

    let state = match incoming.state.as_str() {
        "member" => MemberState::Member,
        "pending" => MemberState::Pending,
        "refused" => {
            let reason = incoming
                .reason
                .unwrap_or_else(|| "no reason given".to_string());
            return Err(format!("{from_peer} refused the join request: {reason}"));
        }
        other => return Err(format!("unknown hive join state '{other}'")),
    };

    let hive_id = incoming
        .hive_id
        .ok_or("the host answered without naming a hive")?;

    // An unchanged answer is not news. Re-running `hive join` on a hive we are
    // already in should be quiet rather than chatty.
    if let Ok(Some(existing)) = membership::load_membership() {
        if existing.hive_id == hive_id && existing.state == state {
            return Ok(None);
        }
    }

    let record = Membership {
        hive_id: hive_id.clone(),
        name: incoming.name.clone(),
        joined_via: from_peer.to_string(),
        state,
        joined_ms: crate::hive::identity::epoch_ms(),
    };
    membership::save_membership(&record)?;

    let label = incoming.name.unwrap_or(hive_id);
    Ok(Some(match state {
        MemberState::Member => format!("Joined hive \"{label}\" — knowledge will now be shared."),
        MemberState::Pending => format!(
            "Asked to join \"{label}\" — waiting for its owner to approve. \
             Nothing is shared until they do."
        ),
    }))
}

/// `claudectl hive requests`
pub fn cmd_requests(json_mode: bool) -> io::Result<()> {
    let now = crate::hive::identity::epoch_ms();
    let pending = membership::pending_requests();
    let members = membership::list_members();

    if json_mode {
        let out = serde_json::json!({
            "pending": pending.iter().map(|r| serde_json::json!({
                "id": r.id,
                "peer_id": r.peer_id,
                "hive_id": r.hive_id,
                "requested_ms": r.requested_ms,
                "label": r.peer_label,
            })).collect::<Vec<_>>(),
            "members": members.iter().map(|m| serde_json::json!({
                "peer_id": m.peer_id,
                "hive_id": m.hive_id,
                "admitted_ms": m.admitted_ms,
                "admission": m.admission.as_str(),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return Ok(());
    }

    match crate::hive::identity::load() {
        Ok(Some(h)) => println!(
            "Hive \"{}\" ({}), join_policy={}",
            h.name,
            h.hive_id,
            h.effective_join_policy().as_str()
        ),
        Ok(None) => {
            println!("This machine has no named hive, so nobody can ask to join it.");
            println!("  claudectl hive identity set --name <name>");
            return Ok(());
        }
        Err(e) => return Err(io::Error::other(e)),
    }
    println!();

    if pending.is_empty() {
        println!("No join requests waiting.");
    } else {
        println!("{} waiting for you:", pending.len());
        println!();
        println!("  {:<28} {:<22} ASKED", "PEER", "REQUEST");
        println!("  {}", "─".repeat(72));
        for r in &pending {
            println!(
                "  {:<28} {:<22} {}",
                r.peer_id,
                r.id,
                claudectl_core::helpers::fmt_ms_at(r.requested_ms, now)
            );
        }
        println!();
        println!("  claudectl hive requests approve <peer>");
        println!("  claudectl hive requests deny <peer>");
    }

    println!();
    if members.is_empty() {
        println!("No members yet.");
    } else {
        println!("{} member(s):", members.len());
        println!();
        println!("  {:<28} {:<14} ADMITTED", "PEER", "HOW");
        println!("  {}", "─".repeat(72));
        for m in &members {
            println!(
                "  {:<28} {:<14} {}",
                m.peer_id,
                m.admission.as_str(),
                claudectl_core::helpers::fmt_ms_at(m.admitted_ms, now)
            );
        }
    }

    Ok(())
}

/// `claudectl hive requests approve|deny <peer>`
pub fn cmd_decide_request(peer: &str, approve: bool, json_mode: bool) -> io::Result<()> {
    let hive = match crate::hive::identity::load() {
        Ok(Some(h)) => h,
        Ok(None) => {
            return Err(io::Error::other(
                "this machine has no named hive, so there is nothing to approve into",
            ));
        }
        Err(e) => return Err(io::Error::other(e)),
    };

    let entry: JoinRequest = membership::pending_for(peer).ok_or_else(|| {
        io::Error::other(format!(
            "no join request from '{peer}' is waiting — `claudectl hive requests` lists the ones that are"
        ))
    })?;

    if approve {
        // Admit first, then notify. If the notification fails the peer is still
        // a member and will be told on its next request; if it were the other
        // way round a lost write would leave a peer believing it was admitted.
        membership::admit(&entry.peer_id, &hive.hive_id, Admission::Approved)
            .map_err(io::Error::other)?;
        let told = notify_decision(&entry.peer_id, &hive, true);
        if json_mode {
            println!(
                "{}",
                serde_json::json!({
                    "peer_id": entry.peer_id,
                    "request": entry.id,
                    "decision": "approved",
                    "notified": told,
                })
            );
        } else {
            println!("Approved {} into \"{}\".", entry.peer_id, hive.name);
            if told {
                println!("  Told them — they will start exchanging knowledge.");
            } else {
                println!(
                    "  Could not reach them right now; they will find out when they next\n  \
                     connect or re-run `claudectl hive join`."
                );
            }
        }
    } else {
        membership::deny(&entry).map_err(io::Error::other)?;
        if json_mode {
            println!(
                "{}",
                serde_json::json!({
                    "peer_id": entry.peer_id,
                    "request": entry.id,
                    "decision": "denied",
                })
            );
        } else {
            println!("Denied {}.", entry.peer_id);
            println!("  They stay paired but get nothing from the hive.");
        }
    }

    Ok(())
}

/// Tell a peer we decided, best-effort, over a one-shot connection.
///
/// Returns whether it got through. Never fails the decision: the record on disk
/// is what membership means, and the peer re-asks on its next `hive join`.
#[cfg(feature = "relay")]
fn notify_decision(
    peer_id: &str,
    hive: &crate::hive::identity::HiveIdentity,
    member: bool,
) -> bool {
    use crate::relay::hivejoin::{JoinResultPayload, JoinResultState};

    let identity = crate::relay::load_or_create_identity();
    let payload = JoinResultPayload {
        state: if member {
            JoinResultState::Member
        } else {
            JoinResultState::Refused
        },
        hive_id: Some(hive.hive_id.clone()),
        name: Some(hive.name.clone()),
        reason: None,
    };
    let msg = crate::relay::hivejoin::build_join_result(identity.as_str(), &payload);
    crate::relay::cli::send_message_to_peer(peer_id, &identity, &msg).is_ok()
}

#[cfg(not(feature = "relay"))]
fn notify_decision(
    _peer_id: &str,
    _hive: &crate::hive::identity::HiveIdentity,
    _member: bool,
) -> bool {
    false
}

/// The membership line `hive status` shows.
pub fn membership_line() -> Option<String> {
    match membership::load_membership() {
        Ok(Some(m)) => {
            let label = m.name.clone().unwrap_or_else(|| m.hive_id.clone());
            Some(match m.state {
                MemberState::Member => {
                    format!("Hive membership: in \"{label}\" via {}", m.joined_via)
                }
                MemberState::Pending => format!(
                    "Hive membership: asked to join \"{label}\" via {} — awaiting owner approval",
                    m.joined_via
                ),
            })
        }
        Ok(None) => None,
        Err(e) => Some(format!("Hive membership: unreadable — {e}")),
    }
}

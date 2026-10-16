//! Fleet-wide leases (#569).
//!
//! One peer is the lease authority (`[relay] lease_authority`). It keeps every
//! fleet lease in its own coord store, so a fleet lease is an ordinary lease
//! row whose owner reads `<session>@<peer>`. A conflict, a FIFO queue, a TTL:
//! all of it is the machine-local machinery from part 1, run on one machine.
//! Nothing is replicated, so there is no ordering or merge to get wrong.
//!
//! - A requester sends `lease_claim`, `lease_release` or `lease_list` over a
//!   one-shot dial, and the authority answers on the same connection with a
//!   `lease_result`. All four are sent only on a link that agreed
//!   `lease-fleet`.
//! - `--wait` is a poll, not a held connection: the authority keeps one reply
//!   handle per peer, so two held requests from one machine would clobber each
//!   other. The first poll joins the queue and gets a waiter id; later polls
//!   present it.
//! - Renewal needs no message. The authority renews a fleet lease while the
//!   owning peer's heartbeat lists the session as running. A session that
//!   ends, or a machine that stops heartbeating, stops renewing, and the
//!   lease expires at its TTL. Expiry is computed on the authority's clock
//!   only.
//! - A requester picks the lease id. A retry after a lost reply then finds its
//!   own lease and is told it's granted, instead of creating a second one.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};

use super::features::{self, Features};
use super::{MessageType, PeerId, RelayMessage, epoch_ms, gen_msg_id};
use crate::coord::store;
use crate::coord::types::{Lease, LeaseMode, LeaseStatus};

/// Separates the session from the peer in a fleet lease's owner.
pub const OWNER_SEP: char = '@';

/// How long a requester waits for one answer before retrying.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times a requester sends one request before giving up. A retry is
/// safe: a claim names its lease id, so a repeat is recognised.
pub const REQUEST_ATTEMPTS: u32 = 3;

const MAX_SESSION_LEN: usize = 128;
const MAX_TOKEN_LEN: usize = 96;

/// The owner a fleet lease records: which session, on which machine.
pub fn fleet_owner(session: &str, peer: &str) -> String {
    format!("{session}{OWNER_SEP}{peer}")
}

/// Whether a lease belongs to the fleet rather than to one machine.
pub fn is_fleet_owner(owner: &str) -> bool {
    owner.contains(OWNER_SEP)
}

/// The owners an authority's sweep treats as alive: its own sessions, bare
/// (its machine-local leases) and as `<session>@<self>`, plus every session
/// each still-reporting peer advertised in its heartbeat.
///
/// `peers` pairs the peer id the authority authenticated with the session
/// list that peer sent, never a peer id read out of the payload.
pub fn live_owners<'a>(
    me: &str,
    local_sessions: impl IntoIterator<Item = String>,
    peers: impl IntoIterator<Item = (&'a str, &'a [Value])>,
) -> HashSet<String> {
    let mut live = HashSet::new();
    for s in local_sessions {
        live.insert(fleet_owner(&s, me));
        live.insert(s);
    }
    for (peer, sessions) in peers {
        for s in sessions {
            if let Some(id) = s.get("session_id").and_then(Value::as_str) {
                live.insert(fleet_owner(id, peer));
            }
        }
    }
    live
}

// ── Requests ────────────────────────────────────────────────────────────

/// A claim as it travels to the authority.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimRequest {
    pub request_id: String,
    pub lease_id: String,
    pub session: String,
    pub kind: String,
    pub value: String,
    pub mode: LeaseMode,
    pub ttl_secs: u64,
    pub reason: String,
    pub wait: bool,
    pub waiter_id: Option<String>,
}

impl ClaimRequest {
    pub fn to_payload(&self) -> Value {
        let mut p = json!({
            "request_id": self.request_id,
            "lease_id": self.lease_id,
            "session": self.session,
            "kind": self.kind,
            "value": self.value,
            "mode": self.mode.as_str(),
            "ttl_secs": self.ttl_secs,
            "reason": self.reason,
            "wait": self.wait,
        });
        if let Some(w) = &self.waiter_id {
            p["waiter_id"] = json!(w);
        }
        p
    }

    /// Read and validate a claim. Everything here came from a peer, so every
    /// string is bounded and checked before it reaches the store.
    pub fn from_payload(p: &Value) -> Result<Self, String> {
        let s = |key: &str| p.get(key).and_then(Value::as_str).unwrap_or("");
        let request_id = token(s("request_id"), "request_id")?;
        let lease_id = token(s("lease_id"), "lease_id")?;
        let session = session_id(s("session"))?;
        let (kind, value) = (s("kind"), s("value"));
        store::validate_resource(kind, value)?;
        let mode = LeaseMode::parse(s("mode")).ok_or("mode must be exclusive or advisory")?;
        let ttl_secs = p
            .get("ttl_secs")
            .and_then(Value::as_u64)
            .filter(|t| (1..=store::MAX_LEASE_TTL_SECS).contains(t))
            .ok_or("ttl_secs out of range")?;
        let reason: String = s("reason")
            .chars()
            .filter(|c| !c.is_control())
            .take(256)
            .collect();
        let waiter_id = match p.get("waiter_id").and_then(Value::as_str) {
            Some(w) => Some(token(w, "waiter_id")?),
            None => None,
        };
        Ok(ClaimRequest {
            request_id,
            lease_id,
            session,
            kind: kind.to_string(),
            value: value.to_string(),
            mode,
            ttl_secs,
            reason,
            wait: p.get("wait").and_then(Value::as_bool) == Some(true),
            waiter_id,
        })
    }
}

/// An id-like string: printable ASCII without spaces or `@`, bounded.
fn token(raw: &str, what: &str) -> Result<String, String> {
    let ok = !raw.is_empty()
        && raw.len() <= MAX_TOKEN_LEN
        && raw.bytes().all(|b| b.is_ascii_graphic() && b != b'@');
    if ok {
        Ok(raw.to_string())
    } else {
        Err(format!("invalid {what}"))
    }
}

/// A session id: printable, bounded, and without `@`, which separates the
/// session from the peer in a fleet owner.
fn session_id(raw: &str) -> Result<String, String> {
    let ok = !raw.is_empty()
        && raw.len() <= MAX_SESSION_LEN
        && !raw.contains(OWNER_SEP)
        && !raw.chars().any(char::is_control);
    if ok {
        Ok(raw.to_string())
    } else {
        Err("invalid session".to_string())
    }
}

// ── The authority ───────────────────────────────────────────────────────

/// Answer one request on the authority. `requester` is the peer the request
/// came from, as the relay authenticated it. The reply always carries the
/// request's `request_id` and an `outcome`.
pub fn handle(
    conn: &Connection,
    msg_type: MessageType,
    payload: &Value,
    requester: &str,
    live: &HashSet<String>,
) -> Value {
    let request_id = payload
        .get("request_id")
        .and_then(Value::as_str)
        .and_then(|r| token(r, "request_id").ok())
        .unwrap_or_default();
    let mut reply = match msg_type {
        MessageType::LeaseClaim => handle_claim(conn, payload, requester, live),
        MessageType::LeaseRelease => handle_release(conn, payload, requester),
        MessageType::LeaseList => handle_list(conn, live),
        _ => Err("not a lease request".to_string()),
    }
    .unwrap_or_else(|e| json!({"outcome": "error", "message": e}));
    reply["request_id"] = json!(request_id);
    reply
}

fn handle_claim(
    conn: &Connection,
    payload: &Value,
    requester: &str,
    live: &HashSet<String>,
) -> Result<Value, String> {
    let req = ClaimRequest::from_payload(payload)?;
    store::sweep_leases(conn, live)?;
    let owner = fleet_owner(&req.session, requester);

    // A retry after a lost reply: this exact lease already exists.
    if let Some(existing) = store::get_lease(conn, &req.lease_id)? {
        return if existing.owner_session_id == owner && existing.status == LeaseStatus::Active {
            Ok(json!({"outcome": "granted", "lease": existing}))
        } else {
            Err(format!("lease id {} is already in use", req.lease_id))
        };
    }

    let lease = Lease {
        id: req.lease_id.clone(),
        owner_session_id: owner.clone(),
        owner_agent: "claude-code".into(),
        resource_kind: req.kind.clone(),
        resource_value: req.value.clone(),
        mode: req.mode,
        reason: req.reason.clone(),
        acquired_at: crate::logger::timestamp_now(),
        expires_at: Some(store::iso_after_secs(req.ttl_secs)),
        status: LeaseStatus::Active,
        ttl_secs: Some(req.ttl_secs),
    };

    let blocked = match store::claim_or_queue(conn, &lease, req.waiter_id.as_deref())? {
        store::ClaimOutcome::Granted => {
            return Ok(json!({"outcome": "granted", "lease": lease}));
        }
        store::ClaimOutcome::WaiterGone => {
            return Ok(json!({"outcome": "waiter_gone"}));
        }
        store::ClaimOutcome::Conflict(c) => json!({
            "holder": c.owner_session_id,
            "holder_lease": c.id,
            "expires_at": c.expires_at,
        }),
        store::ClaimOutcome::Queued { ahead } => json!({"ahead": ahead}),
    };

    if !req.wait {
        let mut r = blocked;
        r["outcome"] = json!("conflict");
        return Ok(r);
    }
    let waiter_id = match req.waiter_id {
        Some(w) => w,
        None => store::enqueue_waiter(conn, &owner, &req.kind, &req.value)?,
    };
    let mut r = blocked;
    r["outcome"] = json!("queued");
    r["waiter_id"] = json!(waiter_id);
    Ok(r)
}

fn handle_release(conn: &Connection, payload: &Value, requester: &str) -> Result<Value, String> {
    let s = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or("");
    let lease_id = token(s("lease_id"), "lease_id")?;
    let session = session_id(s("session"))?;
    let Some(lease) = store::get_lease(conn, &lease_id)? else {
        return Ok(json!({"outcome": "not_found"}));
    };
    // Only the machine and session that hold a fleet lease can release it.
    if lease.owner_session_id != fleet_owner(&session, requester) {
        return Ok(json!({"outcome": "not_owner", "holder": lease.owner_session_id}));
    }
    if store::release_lease(conn, &lease_id)? {
        Ok(json!({"outcome": "released"}))
    } else {
        Ok(json!({"outcome": "not_active", "status": lease.status.as_str()}))
    }
}

fn handle_list(conn: &Connection, live: &HashSet<String>) -> Result<Value, String> {
    store::sweep_leases(conn, live)?;
    let leases: Vec<Lease> = store::list_leases(conn, Some(LeaseStatus::Active))?
        .into_iter()
        .filter(|l| is_fleet_owner(&l.owner_session_id))
        .collect();
    Ok(json!({"outcome": "list", "leases": leases}))
}

/// This machine's part in fleet leases, decided once when `relay serve`
/// starts from `[relay] lease_authority`.
pub struct Authority {
    me: String,
    is_authority: bool,
}

impl Authority {
    pub fn new(configured: Option<&str>, me: &str) -> Self {
        Authority {
            me: me.to_string(),
            is_authority: configured == Some(me),
        }
    }

    pub fn is_authority(&self) -> bool {
        self.is_authority
    }

    /// Owners alive right now: this machine's sessions, and each peer's
    /// sessions from a heartbeat recent enough to still count.
    pub fn live(&self, local: &[Value], reg: &super::mesh::PeerRegistry) -> HashSet<String> {
        let now = epoch_ms();
        let stale_ms = claudectl_core::fleet::WORKER_STALE_SECS * 1000;
        let local_ids = local
            .iter()
            .filter_map(|s| s.get("session_id").and_then(Value::as_str))
            .map(str::to_string);
        let peers = reg
            .all_worker_states()
            .iter()
            .filter(|(_, ws)| now.saturating_sub(ws.last_updated) <= stale_ms)
            .map(|(peer, ws)| (peer.as_str(), ws.sessions.as_slice()));
        live_owners(&self.me, local_ids, peers)
    }

    /// Answer one lease request from `from`, the peer the relay authenticated.
    pub fn answer(
        &self,
        from: &PeerId,
        msg: &RelayMessage,
        local: &[Value],
        reg: &super::mesh::PeerRegistry,
    ) -> Value {
        let request_id = msg.payload.get("request_id").cloned().unwrap_or_default();
        if !self.is_authority {
            return json!({
                "request_id": request_id,
                "outcome": "error",
                "message": format!("{} is not the lease authority", self.me),
            });
        }
        match store::open() {
            Ok(conn) => handle(
                &conn,
                msg.msg_type,
                &msg.payload,
                from.as_str(),
                &self.live(local, reg),
            ),
            Err(e) => json!({
                "request_id": request_id,
                "outcome": "error",
                "message": format!("lease store unavailable: {e}"),
            }),
        }
    }

    /// Renew and expire fleet leases. Called on every session collection, so
    /// a fleet lease is renewed every few seconds while its peer reports the
    /// session, not only when a request happens to arrive.
    pub fn sweep(&self, local: &[Value], reg: &super::mesh::PeerRegistry) {
        if !self.is_authority {
            return;
        }
        if let Ok(conn) = store::open() {
            let _ = store::sweep_leases(&conn, &self.live(local, reg));
        }
    }
}

/// The message that carries a reply back to the requester.
pub fn result_message(identity: &str, payload: Value) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::LeaseResult,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload,
    }
}

/// Whether a message is a fleet lease request this build answers.
pub fn is_request(t: MessageType) -> bool {
    matches!(
        t,
        MessageType::LeaseClaim | MessageType::LeaseRelease | MessageType::LeaseList
    )
}

// ── The requester ───────────────────────────────────────────────────────

/// Send one request to the authority and wait for its answer. Retried up to
/// `REQUEST_ATTEMPTS` times when no answer comes: the authority keeps one
/// reply handle per peer, so a concurrent request from this machine can take
/// ours, and a retry is safe because a claim names its lease id.
pub fn request(
    authority: &str,
    identity: &PeerId,
    msg_type: MessageType,
    payload: Value,
) -> Result<Value, String> {
    let request_id = payload
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let psk = super::load_peer_psk(authority)
        .ok_or_else(|| format!("lease authority '{authority}' is not paired with this machine"))?;
    let candidates = super::cli::dial_candidates(authority);
    if candidates.is_empty() {
        return Err(format!(
            "no address on record for lease authority '{authority}'"
        ));
    }
    let msg = RelayMessage {
        id: gen_msg_id(),
        msg_type,
        from_peer: identity.as_str().to_string(),
        timestamp: epoch_ms(),
        payload,
    };

    let mut last_err = String::new();
    for _ in 0..REQUEST_ATTEMPTS {
        for addr in &candidates {
            let (remote, registry) = match super::cli::try_connect_with(
                *addr,
                &psk,
                identity,
                &super::protocol::DialIntent::one_shot(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            if remote != authority {
                return Err(format!(
                    "{addr} is {remote}, not the lease authority {authority}"
                ));
            }
            let agreed = registry
                .lock()
                .ok()
                .and_then(|r| r.get_peer(&remote).map(|c| c.features.clone()))
                .unwrap_or_default();
            check_supported(authority, &agreed)?;
            registry
                .lock()
                .map_err(|_| "registry lock poisoned".to_string())?
                .send_to(&remote, &msg)?;
            match await_result(&registry, &request_id, REQUEST_TIMEOUT) {
                Some(reply) => return Ok(reply),
                None => {
                    last_err = format!("no answer from lease authority '{authority}'");
                    break; // retry from the first address
                }
            }
        }
    }
    Err(last_err)
}

/// Refuse to send a lease request to a peer that never agreed `lease-fleet`.
/// It would drop the message (or, before #559, the link), so say why here.
pub fn check_supported(authority: &str, agreed: &Features) -> Result<(), String> {
    if agreed.has(features::LEASE_FLEET) {
        Ok(())
    } else {
        Err(format!(
            "lease authority '{authority}' runs a claudectl without fleet leases; upgrade it"
        ))
    }
}

fn await_result(
    registry: &std::sync::Arc<std::sync::Mutex<super::mesh::PeerRegistry>>,
    request_id: &str,
    budget: Duration,
) -> Option<Value> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let next = registry.lock().ok().and_then(|r| r.try_recv_message());
        let Some((_, msg)) = next else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        if msg.msg_type == MessageType::LeaseResult
            && msg.payload.get("request_id").and_then(Value::as_str) == Some(request_id)
        {
            return Some(msg.payload);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(lease_id: &str, session: &str, value: &str, wait: bool) -> ClaimRequest {
        ClaimRequest {
            request_id: format!("r-{lease_id}"),
            lease_id: lease_id.into(),
            session: session.into(),
            kind: "res".into(),
            value: value.into(),
            mode: LeaseMode::Exclusive,
            ttl_secs: 300,
            reason: String::new(),
            wait,
            waiter_id: None,
        }
    }

    fn ask(conn: &Connection, req: &ClaimRequest, peer: &str) -> Value {
        handle(
            conn,
            MessageType::LeaseClaim,
            &req.to_payload(),
            peer,
            &HashSet::new(),
        )
    }

    fn outcome(v: &Value) -> &str {
        v["outcome"].as_str().unwrap_or("")
    }

    #[test]
    fn a_claim_round_trips_through_its_payload() {
        let mut c = claim("l1", "sess-a", "dev-db", true);
        c.waiter_id = Some("wait_1".into());
        assert_eq!(ClaimRequest::from_payload(&c.to_payload()).unwrap(), c);
    }

    #[test]
    fn peer_supplied_fields_are_validated() {
        let ok = claim("l1", "sess-a", "dev-db", false).to_payload();
        for (key, bad) in [
            ("session", json!("a@b")),
            ("session", json!("")),
            ("session", json!("x".repeat(MAX_SESSION_LEN + 1))),
            ("lease_id", json!("has space")),
            ("lease_id", json!("")),
            ("request_id", json!("x".repeat(MAX_TOKEN_LEN + 1))),
            ("kind", json!("BAD")),
            ("value", json!("")),
            ("mode", json!("shared")),
            ("ttl_secs", json!(0)),
            ("ttl_secs", json!(store::MAX_LEASE_TTL_SECS + 1)),
            ("waiter_id", json!("w@x")),
        ] {
            let mut p = ok.clone();
            p[key] = bad.clone();
            assert!(ClaimRequest::from_payload(&p).is_err(), "{key}={bad}");
        }
    }

    #[test]
    fn the_authority_grants_and_records_the_requesting_peer() {
        let conn = store::open_memory();
        let r = ask(&conn, &claim("l1", "sess-a", "dev-db", false), "mini");
        assert_eq!(outcome(&r), "granted");
        assert_eq!(r["request_id"], "r-l1");
        let lease = store::get_lease(&conn, "l1").unwrap().unwrap();
        assert_eq!(lease.owner_session_id, "sess-a@mini");
    }

    #[test]
    fn two_peers_cannot_hold_the_same_resource() {
        let conn = store::open_memory();
        ask(&conn, &claim("l1", "sess-a", "dev-db", false), "laptop");
        let r = ask(&conn, &claim("l2", "sess-b", "dev-db", false), "mini");
        assert_eq!(outcome(&r), "conflict");
        assert_eq!(r["holder"], "sess-a@laptop");
    }

    /// The same session name on two machines is two different owners.
    #[test]
    fn the_same_session_name_on_two_peers_is_two_owners() {
        let conn = store::open_memory();
        ask(&conn, &claim("l1", "worker", "dev-db", false), "laptop");
        let r = ask(&conn, &claim("l2", "worker", "dev-db", false), "mini");
        assert_eq!(outcome(&r), "conflict");
    }

    #[test]
    fn a_retried_claim_is_recognised_not_duplicated() {
        let conn = store::open_memory();
        let req = claim("l1", "sess-a", "dev-db", false);
        assert_eq!(outcome(&ask(&conn, &req, "mini")), "granted");
        let again = ask(&conn, &req, "mini");
        assert_eq!(outcome(&again), "granted");
        let n = store::list_leases(&conn, Some(LeaseStatus::Active))
            .unwrap()
            .len();
        assert_eq!(n, 1);
        // Another peer reusing that id is refused, not handed the lease.
        assert_eq!(outcome(&ask(&conn, &req, "laptop")), "error");
    }

    #[test]
    fn a_waiting_claim_queues_and_is_granted_when_released() {
        let conn = store::open_memory();
        ask(&conn, &claim("l1", "sess-a", "dev-db", false), "laptop");
        let first = ask(&conn, &claim("l2", "sess-b", "dev-db", true), "mini");
        assert_eq!(outcome(&first), "queued");
        let waiter = first["waiter_id"].as_str().unwrap().to_string();

        let mut poll = claim("l2", "sess-b", "dev-db", true);
        poll.waiter_id = Some(waiter.clone());
        assert_eq!(outcome(&ask(&conn, &poll, "mini")), "queued");

        let released = handle(
            &conn,
            MessageType::LeaseRelease,
            &json!({"request_id": "rel", "lease_id": "l1", "session": "sess-a"}),
            "laptop",
            &HashSet::new(),
        );
        assert_eq!(outcome(&released), "released");
        assert_eq!(outcome(&ask(&conn, &poll, "mini")), "granted");
    }

    #[test]
    fn only_the_holding_peer_and_session_can_release() {
        let conn = store::open_memory();
        ask(&conn, &claim("l1", "sess-a", "dev-db", false), "laptop");
        for (session, peer) in [("sess-a", "mini"), ("sess-x", "laptop")] {
            let r = handle(
                &conn,
                MessageType::LeaseRelease,
                &json!({"request_id": "rel", "lease_id": "l1", "session": session}),
                peer,
                &HashSet::new(),
            );
            assert_eq!(outcome(&r), "not_owner", "{session}@{peer}");
        }
        assert_eq!(
            store::get_lease(&conn, "l1").unwrap().unwrap().status,
            LeaseStatus::Active
        );
    }

    #[test]
    fn the_list_holds_only_fleet_leases() {
        let conn = store::open_memory();
        ask(&conn, &claim("l1", "sess-a", "dev-db", false), "mini");
        // A machine-local lease on the authority itself.
        let local = claim("l2", "local", "other", false);
        let mut l = Lease {
            id: local.lease_id.clone(),
            owner_session_id: "local".into(),
            owner_agent: "claude-code".into(),
            resource_kind: "res".into(),
            resource_value: "other".into(),
            mode: LeaseMode::Exclusive,
            reason: String::new(),
            acquired_at: crate::logger::timestamp_now(),
            expires_at: Some(store::iso_after_secs(300)),
            status: LeaseStatus::Active,
            ttl_secs: Some(300),
        };
        store::upsert_lease(&conn, &l).unwrap();
        l.id = "l3".into();
        let r = handle(
            &conn,
            MessageType::LeaseList,
            &json!({"request_id": "ls"}),
            "mini",
            &HashSet::new(),
        );
        let ids: Vec<&str> = r["leases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["l1"]);
    }

    /// The renewal path: a peer's heartbeat lists the session, so its fleet
    /// lease survives a lapsed expiry; once the session is gone, it expires.
    #[test]
    fn a_fleet_lease_lives_while_its_peer_reports_the_session() {
        let conn = store::open_memory();
        ask(&conn, &claim("l1", "sess-a", "dev-db", false), "mini");
        conn.execute(
            "UPDATE leases SET expires_at = '2000-01-01T00:00:00Z' WHERE id = 'l1'",
            [],
        )
        .unwrap();

        let heartbeat = vec![json!({"pid": 1, "session_id": "sess-a"})];
        let live = live_owners("laptop", Vec::new(), [("mini", heartbeat.as_slice())]);
        store::sweep_leases(&conn, &live).unwrap();
        assert_eq!(
            store::get_lease(&conn, "l1").unwrap().unwrap().status,
            LeaseStatus::Active
        );

        conn.execute(
            "UPDATE leases SET expires_at = '2000-01-01T00:00:00Z' WHERE id = 'l1'",
            [],
        )
        .unwrap();
        let live = live_owners("laptop", Vec::new(), [("mini", &[][..])]);
        store::sweep_leases(&conn, &live).unwrap();
        assert_eq!(
            store::get_lease(&conn, "l1").unwrap().unwrap().status,
            LeaseStatus::Expired
        );
    }

    /// A session id on one peer must not renew the same id on another.
    #[test]
    fn a_session_on_one_peer_does_not_renew_another_peers_lease() {
        let hb = vec![json!({"session_id": "worker"})];
        let live = live_owners(
            "authority",
            vec!["local".to_string()],
            [("mini", hb.as_slice())],
        );
        assert!(live.contains("worker@mini"));
        assert!(!live.contains("worker@laptop"));
        assert!(live.contains("local@authority"));
        assert!(live.contains("local"));
    }

    #[test]
    fn an_unknown_request_type_is_an_error_reply() {
        let conn = store::open_memory();
        let r = handle(
            &conn,
            MessageType::Heartbeat,
            &json!({"request_id": "x"}),
            "mini",
            &HashSet::new(),
        );
        assert_eq!(outcome(&r), "error");
        assert_eq!(r["request_id"], "x");
    }

    #[test]
    fn a_peer_without_the_feature_is_refused_before_sending() {
        assert!(check_supported("old-mini", &Features::default()).is_err());
        let (_, agreed) =
            features::negotiate(&json!({"proto": 1, "features": ["lease-fleet"]})).unwrap();
        assert!(check_supported("new-mini", &agreed).is_ok());
    }
}

//! The gossip half of the relay loops: one copy of the inbound handling, the
//! membership gates, and the periodic sync tick.
//!
//! Before #455 this logic lived inline in `cmd_serve` and nowhere else, which
//! is how three gaps went unnoticed for as long as they did:
//!
//! - **A peer that dialled out never gossiped.** `run_connect_loop` handled
//!   heartbeats and join results; it neither sent its units nor merged the ones
//!   it received. So `relay join` produced a connection that could not carry
//!   knowledge in either direction.
//! - **There was no catch-up.** `cmd_serve` synced only on the distillation
//!   signal, so a peer that connected a minute after a distillation never
//!   received that unit — there was no second chance.
//! - **The duplication between the two loops is what let the first two
//!   diverge**, so the fix is one module both loops call rather than a second
//!   copy of the arms.
//!
//! The sync is a timer rather than a connect event because
//! [`GossipEngine::generate_sync_messages`] is already incremental —
//! `PeerSyncState.units_sent` records what each peer has been sent, so a tick
//! with nothing new to say produces no messages. That makes a plain interval
//! self-healing: it covers connect, reconnect, a peer that was gated and has
//! since been approved, and a unit distilled while the link was down, without
//! any of those needing their own trigger.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{MessageType, PeerId, RelayMessage};
use crate::hive::gossip::GossipEngine;
use crate::hive::membership::{MemberState, Membership, Role, Roster};
use crate::hive::store::HiveStore;

/// How often each loop offers its peers whatever they have not been sent.
///
/// Short enough that joining a hive feels immediate, long enough that an idle
/// mesh is quiet: a tick with nothing unsent reads two small files
/// (`ExposureStore`, `TrustStore`) and sends nothing.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(12);

/// Everything a relay loop needs to take part in the hive.
///
/// The three `Option`s are independent and each `None` means "ungated", which
/// is what keeps #434 and #435 from breaking setups that predate them:
///
/// - `store`/`engine` are `None` when the hive is switched off in config.
/// - `roster` is `None` until a hive is *named*, so every paired peer is
///   treated exactly as it was before #434.
/// - `membership` is `None` unless this machine has joined someone else's
///   hive.
pub struct HiveSync {
    store: Option<HiveStore>,
    engine: Option<GossipEngine>,
    /// Who may take part in *our* hive. The host side of the gate.
    roster: Option<Roster>,
    /// Our own standing in the hive we joined. The dialling side of the gate —
    /// a reader must not *send*, and that has to be enforced here as well as at
    /// the host, or #435 only holds in one direction.
    membership: Option<Membership>,
    /// Where `membership` is read from. Explicit so a test never reads `HOME`,
    /// the same reason `Roster::at` takes a root.
    membership_path: PathBuf,
    last_sync: Option<Instant>,
}

impl HiveSync {
    /// `named` is whether a hive identity exists; it is the caller's because
    /// `cmd_serve` has already loaded the identity to advertise it.
    pub fn new(cfg: &crate::config::HiveConfig, identity: &str, named: bool) -> Self {
        let enabled = crate::hive::is_active(Some(cfg));
        let engine = enabled.then(|| {
            let mut engine =
                GossipEngine::new(identity, cfg.max_propagation, cfg.knowledge_ttl_days);
            engine.set_sharing_filter(crate::hive::SharingFilter::from_config(cfg));
            if let Some(mode) = crate::hive::exposure::ShareMode::parse(&cfg.share_mode) {
                engine.set_share_mode(mode);
            }
            engine
        });
        let membership_path = crate::hive::membership::membership_path();
        Self {
            store: enabled.then(HiveStore::load),
            engine,
            roster: named.then(Roster::local),
            membership: load_membership_quietly(&membership_path),
            membership_path,
            last_sync: None,
        }
    }

    /// A gate-only instance: no store, no engine, so nothing merges and nothing
    /// is sent. Used by the tests to exercise the refusal paths, which are the
    /// half that must hold whether or not the hive is switched on.
    #[cfg(test)]
    fn gates_only(roster: Option<Roster>, membership: Option<Membership>) -> Self {
        Self {
            store: None,
            engine: None,
            roster,
            membership,
            // Nothing reads this in the gate tests, and pointing it at a path
            // that cannot exist is what keeps that true.
            membership_path: PathBuf::from("/nonexistent/membership.json"),
            last_sync: None,
        }
    }

    /// Gate-only, but with a live engine and store rooted at `path`, so a test
    /// can watch the re-offer actually happen.
    #[cfg(test)]
    fn with_engine(path: PathBuf, store: HiveStore) -> Self {
        Self {
            store: Some(store),
            engine: Some(GossipEngine::new_empty("local", 5, 30)),
            roster: None,
            membership: None,
            membership_path: path,
            last_sync: None,
        }
    }

    pub fn store(&self) -> Option<&HiveStore> {
        self.store.as_ref()
    }

    /// Re-read our membership file, and if our standing changed, re-offer
    /// everything to the host.
    ///
    /// It changes under us: a `HiveJoinResult` arriving from an owner who has
    /// just approved a queued request rewrites it mid-run, and the role it
    /// carries is what decides whether the next tick may send. Loading it once
    /// at startup would leave a peer approved at 10:00 still gated at 18:00.
    ///
    /// The change is also the only sound trigger for re-offering a batch the
    /// host refused. `units_sent` is recorded when a batch is *built* — there
    /// is no acknowledgement to wait for — so a refusal leaves us believing in
    /// a delivery that never happened, persisted, which is what used to
    /// withhold the pre-approval units from a peer forever once it was
    /// admitted. The tempting fix is to reset on the `KnowledgeRejected`
    /// itself, but a refusal says nothing changed *here*: a peer paired for
    /// delegation alone, against a host with a named hive, would then re-offer
    /// its whole store every tick for as long as both stayed up. Our own
    /// standing changing is the event that actually means "try again".
    pub fn refresh_membership(&mut self) {
        let fresh = load_membership_quietly(&self.membership_path);
        if fresh == self.membership {
            return;
        }
        if let Some(host) = fresh.as_ref().map(|m| m.joined_via.clone()) {
            if let Some(engine) = self.engine.as_mut() {
                if engine.forget_peer(&host) {
                    println!(
                        "[{}] our hive standing changed — re-offering knowledge to {host}",
                        crate::logger::timestamp_now()
                    );
                }
            }
        }
        self.membership = fresh;
    }

    /// The hive we joined, if we are an admitted member of one.
    fn my_host(&self) -> Option<&str> {
        self.membership
            .as_ref()
            .filter(|m| m.state == MemberState::Member)
            .map(|m| m.joined_via.as_str())
    }

    /// May this peer be *sent* hive knowledge?
    ///
    /// Any member of our hive, readers included — receiving without
    /// contributing is the whole point of a reader (#435, §7.5) — plus the host
    /// of the hive we joined, which our own roster says nothing about.
    fn may_receive(&self, peer_id: &str) -> bool {
        if self.my_host() == Some(peer_id) {
            return true;
        }
        match &self.roster {
            None => true,
            Some(r) => r.may_receive(peer_id),
        }
    }

    /// May this peer *contribute* hive knowledge to us?
    ///
    /// Contributors of our hive, and the host of a hive we joined. A reader of
    /// *our* hive is refused here and allowed in `may_receive`, and that
    /// asymmetry is the feature. Our own role does not come into it: being a
    /// reader somewhere constrains what we send, not what we accept.
    fn may_contribute(&self, peer_id: &str) -> bool {
        if self.my_host() == Some(peer_id) {
            return true;
        }
        match &self.roster {
            None => true,
            Some(r) => r.may_contribute(peer_id),
        }
    }

    /// May *we* push knowledge to this peer?
    ///
    /// The mirror of `may_contribute`, asked of ourselves. A reader that sends
    /// anyway would be refused on the wire by a host running #435, but a
    /// reader is supposed to be read-only whether or not the other end
    /// enforces it — and a peer still `Pending` has not been admitted at all.
    fn may_send_to(&self, peer_id: &str) -> bool {
        if let Some(m) = &self.membership {
            if m.joined_via == peer_id && (m.role == Role::Reader || m.state != MemberState::Member)
            {
                return false;
            }
        }
        self.may_receive(peer_id)
    }

    /// The subset of `connected` this machine may gossip to.
    pub fn targets(&self, connected: Vec<PeerId>) -> Vec<PeerId> {
        connected
            .into_iter()
            .filter(|p| self.may_send_to(p.as_str()))
            .collect()
    }

    /// Offer every eligible peer whatever it has not been sent.
    ///
    /// Safe to call as often as you like: the engine tracks what each peer
    /// already has, so a tick with nothing new returns an empty vec.
    pub fn sync(&mut self, connected: Vec<PeerId>) -> Vec<(PeerId, RelayMessage)> {
        let targets = self.targets(connected);
        if targets.is_empty() {
            return Vec::new();
        }
        let (Some(engine), Some(store)) = (self.engine.as_mut(), self.store.as_ref()) else {
            return Vec::new();
        };
        let msgs = engine.generate_sync_messages(store, &targets);
        for (target, _) in &msgs {
            println!(
                "[{}] KnowledgeSync to {}",
                crate::logger::timestamp_now(),
                target
            );
        }
        msgs
    }

    /// `sync`, but only once per [`SYNC_INTERVAL`].
    ///
    /// The first call always syncs, so a loop that has just connected does not
    /// wait out an interval before saying anything.
    pub fn sync_if_due(
        &mut self,
        now: Instant,
        connected: Vec<PeerId>,
    ) -> Vec<(PeerId, RelayMessage)> {
        if let Some(last) = self.last_sync {
            if now.duration_since(last) < SYNC_INTERVAL {
                return Vec::new();
            }
        }
        self.last_sync = Some(now);
        self.refresh_membership();
        self.sync(connected)
    }

    /// Handle one inbound message if it is ours.
    ///
    /// `None` means "not a hive message, you deal with it", which is what lets
    /// both loops call this ahead of their own `match` without either needing
    /// arms of its own.
    pub fn handle_inbound(
        &mut self,
        from_peer: &PeerId,
        msg: &RelayMessage,
        identity: &str,
        connected: &[PeerId],
    ) -> Option<Vec<(PeerId, RelayMessage)>> {
        match msg.msg_type {
            MessageType::KnowledgeSync => Some(self.on_sync(from_peer, msg, identity, connected)),
            MessageType::KnowledgeRequest => Some(self.on_request(from_peer, msg)),
            MessageType::KnowledgeSnapshot => Some(self.on_snapshot(from_peer, msg)),
            MessageType::KnowledgeRejected => {
                eprintln!(
                    "[{}] {} refused our knowledge: {}",
                    crate::logger::timestamp_now(),
                    from_peer,
                    super::hivejoin::rejection_reason(&msg.payload)
                );
                // Reported and nothing more. The re-offer happens when our own
                // standing changes (see `refresh_membership`), never on the
                // refusal itself — a peer the host will always refuse would
                // otherwise re-offer its whole store every tick forever.
                Some(Vec::new())
            }
            _ => None,
        }
    }

    fn on_sync(
        &mut self,
        from_peer: &PeerId,
        msg: &RelayMessage,
        identity: &str,
        connected: &[PeerId],
    ) -> Vec<(PeerId, RelayMessage)> {
        // #434: a peer that is not in the hive does not get to contribute to
        // it. Dropping inbound units matters as much as refusing to send: an
        // `ask` hive that gated only its own sends would still merge whatever
        // an unapproved peer pushed.
        // The host of a hive we joined is entitled to send us knowledge —
        // receiving it is what joining was *for* — and our own roster has no
        // entry for it, so it is exempted from the gate rather than refused by
        // it.
        let gate = if self.my_host() == Some(from_peer.as_str()) {
            None
        } else {
            self.roster.as_ref()
        };
        if let Some(rejection) =
            super::hivejoin::knowledge_refusal(gate, from_peer.as_str(), identity, &msg.payload)
        {
            // Refused on the wire, not merely dropped (#435): a reader that
            // believes it is contributing and is silently ignored cannot tell
            // that from a network fault.
            println!(
                "[{}] KnowledgeSync from {} refused — {}",
                crate::logger::timestamp_now(),
                from_peer,
                super::hivejoin::rejection_reason(&rejection.payload)
            );
            return vec![(from_peer.clone(), rejection)];
        }
        let (Some(engine), Some(store)) = (self.engine.as_mut(), self.store.as_mut()) else {
            return Vec::new();
        };
        let (stats, accepted) = engine.handle_sync(store, msg);
        println!(
            "[{}] KnowledgeSync from {}: {} accepted, {} rejected",
            crate::logger::timestamp_now(),
            from_peer,
            stats.accepted,
            stats.rejected
        );
        let installed = crate::hive::cli::auto_accept_units(&accepted, None);
        if installed > 0 {
            println!(
                "[{}] Auto-installed {installed} artifact(s)",
                crate::logger::timestamp_now()
            );
        }
        if accepted.is_empty() {
            return Vec::new();
        }
        let targets = self.targets(connected.to_vec());
        let Some(engine) = self.engine.as_mut() else {
            return Vec::new();
        };
        engine.propagate(&accepted, from_peer, &targets)
    }

    fn on_request(
        &mut self,
        from_peer: &PeerId,
        msg: &RelayMessage,
    ) -> Vec<(PeerId, RelayMessage)> {
        // Asking for a snapshot is *receiving*, so a reader may.
        if !self.may_receive(from_peer.as_str()) {
            println!(
                "[{}] KnowledgeRequest from {} refused — not a member of this hive",
                crate::logger::timestamp_now(),
                from_peer
            );
            return Vec::new();
        }
        let (Some(engine), Some(store)) = (self.engine.as_ref(), self.store.as_ref()) else {
            return Vec::new();
        };
        engine
            .handle_request(store, msg)
            .into_iter()
            .map(|snap| (from_peer.clone(), snap))
            .collect()
    }

    fn on_snapshot(
        &mut self,
        from_peer: &PeerId,
        msg: &RelayMessage,
    ) -> Vec<(PeerId, RelayMessage)> {
        if !self.may_contribute(from_peer.as_str()) {
            println!(
                "[{}] KnowledgeSnapshot from {} refused — not a contributor to this hive",
                crate::logger::timestamp_now(),
                from_peer
            );
            return Vec::new();
        }
        let (Some(engine), Some(store)) = (self.engine.as_mut(), self.store.as_mut()) else {
            return Vec::new();
        };
        let (stats, merged) = engine.handle_snapshot(store, msg);
        println!(
            "[{}] KnowledgeSnapshot from {}: {} accepted",
            crate::logger::timestamp_now(),
            from_peer,
            stats.accepted
        );
        let installed = crate::hive::cli::auto_accept_units(&merged, None);
        if installed > 0 {
            println!(
                "[{}] Auto-installed {installed} artifact(s)",
                crate::logger::timestamp_now()
            );
        }
        Vec::new()
    }
}

/// An unreadable membership file costs the gate, not the relay.
///
/// Failing closed here would be worse than it sounds: `None` is also what every
/// machine that never joined a hive has, so a hard error would take down
/// gossip for a setup that has nothing to do with membership.
fn load_membership_quietly(path: &std::path::Path) -> Option<Membership> {
    match crate::hive::membership::load_membership_from(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("warning: membership unreadable, gossiping ungated: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hive::membership::Admission;

    fn peers(ids: &[&str]) -> Vec<PeerId> {
        ids.iter().map(|s| PeerId(s.to_string())).collect()
    }

    fn membership(host: &str, role: Role, state: MemberState) -> Membership {
        Membership {
            hive_id: "h-1".into(),
            name: Some("theirs".into()),
            role,
            joined_via: host.into(),
            state,
            joined_ms: 0,
        }
    }

    #[test]
    fn with_no_hive_of_our_own_every_peer_is_a_target() {
        let h = HiveSync::gates_only(None, None);
        assert_eq!(h.targets(peers(&["a", "b"])).len(), 2);
    }

    #[test]
    fn a_reader_never_sends_to_the_host_it_reads_from() {
        let h = HiveSync::gates_only(
            None,
            Some(membership("host-1", Role::Reader, MemberState::Member)),
        );
        let got = h.targets(peers(&["host-1", "other"]));
        assert_eq!(
            got,
            peers(&["other"]),
            "a reader must not push units upstream"
        );
    }

    #[test]
    fn a_contributor_does_send_to_the_host_it_joined() {
        let h = HiveSync::gates_only(
            None,
            Some(membership("host-1", Role::Contributor, MemberState::Member)),
        );
        assert_eq!(h.targets(peers(&["host-1"])), peers(&["host-1"]));
    }

    #[test]
    fn a_pending_request_does_not_send_either() {
        // We asked to join and nobody has decided yet. Sending now would push
        // units at a host that is about to refuse them.
        let h = HiveSync::gates_only(
            None,
            Some(membership(
                "host-1",
                Role::Contributor,
                MemberState::Pending,
            )),
        );
        assert!(h.targets(peers(&["host-1"])).is_empty());
    }

    #[test]
    fn a_reader_still_accepts_what_the_host_sends_it() {
        // Our role constrains what we send, never what we accept — otherwise
        // "read-only member" would receive nothing, which is the opposite of
        // what it means.
        let h = HiveSync::gates_only(
            None,
            Some(membership("host-1", Role::Reader, MemberState::Member)),
        );
        assert!(h.may_contribute("host-1"));
        assert!(h.may_receive("host-1"));
    }

    #[test]
    fn the_host_of_a_hive_we_joined_is_not_judged_by_our_own_roster() {
        // Both machines can name a hive. Theirs is not on our roster and never
        // will be, so a roster-only gate would refuse the knowledge we joined
        // in order to receive.
        let tmp = tempfile::tempdir().unwrap();
        let h = HiveSync::gates_only(
            Some(Roster::at(tmp.path())),
            Some(membership("host-1", Role::Contributor, MemberState::Member)),
        );
        assert!(
            !h.may_receive("stranger"),
            "our roster still gates everyone else"
        );
        assert!(h.may_contribute("host-1"));
        assert_eq!(
            h.targets(peers(&["host-1", "stranger"])),
            peers(&["host-1"])
        );
    }

    #[test]
    fn the_first_tick_syncs_and_the_next_one_waits() {
        let mut h = HiveSync::gates_only(None, None);
        let t0 = Instant::now();
        assert!(h.last_sync.is_none());
        h.sync_if_due(t0, Vec::new());
        assert_eq!(
            h.last_sync,
            Some(t0),
            "the first tick must not wait out an interval"
        );
        let soon = t0 + SYNC_INTERVAL - Duration::from_millis(1);
        h.sync_if_due(soon, Vec::new());
        assert_eq!(
            h.last_sync,
            Some(t0),
            "a tick inside the interval is a no-op"
        );
        let due = t0 + SYNC_INTERVAL;
        h.sync_if_due(due, Vec::new());
        assert_eq!(h.last_sync, Some(due));
    }

    #[test]
    fn non_hive_messages_are_handed_back_to_the_caller() {
        let mut h = HiveSync::gates_only(None, None);
        let msg = RelayMessage {
            id: "m1".into(),
            msg_type: MessageType::Heartbeat,
            from_peer: "a".into(),
            timestamp: 0,
            payload: serde_json::json!({}),
        };
        assert!(
            h.handle_inbound(&PeerId("a".into()), &msg, "me", &[])
                .is_none()
        );
    }

    // ---------------------------------------------------------------------
    // The hive membership gate (#434, #435), moved here with the gate itself.
    //
    // These are the teeth of `join_policy`: a peer that is only *pending* must
    // neither receive knowledge nor be able to push any. Policy correctness is
    // tested in `relay::hivejoin`; what is tested here is that the gate the
    // relay loops actually call agrees with the roster.

    /// A hive with no roster and no membership: the pre-#434 world.
    fn open_mesh() -> HiveSync {
        HiveSync::gates_only(None, None)
    }

    /// A hive whose roster is the one on disk at `root`.
    fn gated(root: &std::path::Path) -> HiveSync {
        HiveSync::gates_only(Some(Roster::at(root)), None)
    }

    fn names(v: &[PeerId]) -> Vec<&str> {
        v.iter().map(|p| p.as_str()).collect()
    }

    #[test]
    fn an_unnamed_hive_gates_nothing() {
        // The no-regression case: everyone who never named a hive keeps the
        // pre-#434 behaviour exactly.
        let connected = peers(&["a-1", "b-2"]);
        assert_eq!(
            names(&open_mesh().targets(connected.clone())),
            vec!["a-1", "b-2"]
        );
        assert!(open_mesh().may_contribute("a-1"));
        assert!(open_mesh().may_receive("a-1"));
        assert!(open_mesh().may_contribute("nobody-ever-heard-of"));
    }

    #[test]
    fn a_named_hive_sends_only_to_members() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        r.admit("a-1", "hv_1", Admission::Policy).unwrap();

        let connected = peers(&["a-1", "b-2", "c-3"]);
        assert_eq!(
            names(&gated(tmp.path()).targets(connected)),
            vec!["a-1"],
            "only the admitted peer is a sync target"
        );
    }

    #[test]
    fn a_pending_peer_is_gated_in_both_directions() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        r.record_request("b-2", "hv_1", None).unwrap();

        // Outbound: not a target.
        assert!(
            gated(tmp.path()).targets(peers(&["b-2"])).is_empty(),
            "a pending peer must not be sent knowledge"
        );
        // Inbound: its units are dropped. Gating only one direction would let an
        // unapproved peer poison the hive while receiving nothing.
        assert!(
            !gated(tmp.path()).may_contribute("b-2"),
            "a pending peer must not be able to contribute either"
        );
    }

    #[test]
    fn approving_opens_the_gate_the_serve_loop_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        let entry = r.record_request("b-2", "hv_1", None).unwrap();
        assert!(!gated(tmp.path()).may_contribute("b-2"));

        r.admit(&entry.peer_id, "hv_1", Admission::Approved)
            .unwrap();

        assert!(gated(tmp.path()).may_contribute("b-2"));
        assert_eq!(
            names(&gated(tmp.path()).targets(peers(&["b-2"]))),
            vec!["b-2"]
        );
    }

    #[test]
    fn a_denied_peer_stays_gated() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        let entry = r.record_request("b-2", "hv_1", None).unwrap();
        r.deny(&entry).unwrap();
        assert!(!gated(tmp.path()).may_contribute("b-2"));
        assert!(gated(tmp.path()).targets(peers(&["b-2"])).is_empty());
    }

    #[test]
    fn a_reader_receives_but_may_not_contribute() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        r.admit_as(
            "reader-1",
            "hv_1",
            crate::hive::membership::Admission::Grant,
            crate::hive::membership::Role::Reader,
            Some("g_abc".into()),
        )
        .unwrap();

        // This asymmetry is §7.5: participation without symmetry.
        assert!(
            gated(tmp.path()).may_receive("reader-1"),
            "a reader must receive knowledge — that is what it is for"
        );
        assert!(
            !gated(tmp.path()).may_contribute("reader-1"),
            "a reader must never be able to contribute"
        );
        // And it is a sync target, unlike a pending peer.
        assert_eq!(
            names(&gated(tmp.path()).targets(peers(&["reader-1"]))),
            vec!["reader-1"]
        );
    }

    #[test]
    fn a_contributor_and_a_reader_are_both_targets_but_only_one_may_push() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        r.admit(
            "writer-1",
            "hv_1",
            crate::hive::membership::Admission::Policy,
        )
        .unwrap();
        r.admit_as(
            "reader-1",
            "hv_1",
            crate::hive::membership::Admission::Grant,
            crate::hive::membership::Role::Reader,
            None,
        )
        .unwrap();

        let selected = gated(tmp.path()).targets(peers(&["writer-1", "reader-1", "stranger"]));
        let mut targets = names(&selected);
        targets.sort();
        assert_eq!(targets, vec!["reader-1", "writer-1"]);

        assert!(gated(tmp.path()).may_contribute("writer-1"));
        assert!(!gated(tmp.path()).may_contribute("reader-1"));
    }

    #[test]
    fn an_unknown_peer_is_gated_by_a_named_hive() {
        // A peer may be paired without ever having asked to join — pairing is
        // not membership.
        let tmp = tempfile::tempdir().unwrap();
        assert!(!gated(tmp.path()).may_contribute("never-asked"));
        assert!(!gated(tmp.path()).may_receive("never-asked"));
    }

    // ---------------------------------------------------------------------
    // Re-offering a refused batch (#455).

    fn write_membership(path: &std::path::Path, state: MemberState) {
        let m = membership("host-1", Role::Contributor, state);
        crate::hive::membership::save_membership_to(path, &m).unwrap();
    }

    fn one_unit_store() -> HiveStore {
        let mut store = HiveStore::load_from(std::path::Path::new("/nonexistent"));
        store.insert(crate::hive::KnowledgeUnit {
            id: "ku_1".into(),
            scope: crate::hive::KnowledgeScope::Universal,
            category: crate::hive::KnowledgeCategory::BestPractice,
            content: crate::hive::KnowledgeContent::Temporal {
                description: "mornings are for refactors".into(),
                strength: 0.9,
            },
            evidence_count: 5,
            confidence: 0.9,
            source_peer: "local".into(),
            originated_at: crate::hive::epoch_secs(),
            last_validated_at: crate::hive::epoch_secs(),
            propagation_count: 0,
            version: 1,
            revalidation_interval_secs: 2_592_000,
            injection_state: Default::default(),
            injection_stats: Default::default(),
            sharing_consent: None,
        });
        store
    }

    #[test]
    fn being_admitted_re_offers_what_the_host_refused_while_we_were_pending() {
        // The whole point: a batch refused before approval must not be lost.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("membership.json");
        write_membership(&path, MemberState::Pending);

        let mut h = HiveSync::with_engine(path.clone(), one_unit_store());
        h.refresh_membership();
        assert!(
            h.targets(peers(&["host-1"])).is_empty(),
            "a pending peer does not send"
        );

        // Pretend the gate was open for one tick and the host refused the
        // batch: the engine has recorded it as delivered either way.
        let sent = h.sync(peers(&["host-1"]));
        assert!(sent.is_empty(), "still gated, so nothing was built");

        // Force the recording the way a real pre-approval send would.
        write_membership(&path, MemberState::Member);
        h.refresh_membership();
        assert_eq!(h.sync(peers(&["host-1"])).len(), 1, "admitted, so it sends");
        assert!(
            h.sync(peers(&["host-1"])).is_empty(),
            "and does not repeat itself"
        );

        // Now the real case: the host refused that batch, and our standing
        // changes again. The slate is reset and the unit is offered afresh.
        write_membership(&path, MemberState::Pending);
        h.refresh_membership();
        write_membership(&path, MemberState::Member);
        h.refresh_membership();
        assert_eq!(
            h.sync(peers(&["host-1"])).len(),
            1,
            "a change in our standing re-offers the batch"
        );
    }

    #[test]
    fn re_reading_an_unchanged_membership_does_not_re_offer() {
        // The guard against a retry storm: `refresh_membership` runs every
        // tick, and resetting the slate each time would resend the whole store
        // every 12 seconds forever.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("membership.json");
        write_membership(&path, MemberState::Member);

        let mut h = HiveSync::with_engine(path.clone(), one_unit_store());
        h.refresh_membership();
        assert_eq!(h.sync(peers(&["host-1"])).len(), 1);

        for _ in 0..5 {
            h.refresh_membership();
            assert!(
                h.sync(peers(&["host-1"])).is_empty(),
                "an unchanged membership must not re-offer"
            );
        }
    }

    #[test]
    fn a_refusal_alone_does_not_re_offer() {
        // A peer paired for delegation only, no membership file, against a host
        // with a named hive: the host refuses it every time and nothing on our
        // side ever changes. Re-offering on the refusal would put the whole
        // store back on the wire every tick, on both machines, forever.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("membership.json");
        let mut h = HiveSync::with_engine(path, one_unit_store());

        assert_eq!(h.sync(peers(&["host-1"])).len(), 1, "ungated, so it sends");

        let refusal = super::super::hivejoin::build_knowledge_rejected(
            "host-1",
            "you are not a member of this hive",
            1,
        );
        for _ in 0..5 {
            h.handle_inbound(&PeerId("host-1".into()), &refusal, "local", &[]);
            h.refresh_membership();
            assert!(
                h.sync(peers(&["host-1"])).is_empty(),
                "a refusal is not a reason to try again"
            );
        }
    }
}

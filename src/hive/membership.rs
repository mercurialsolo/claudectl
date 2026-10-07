//! Hive membership — who is in the hive, and who is still asking (#434).
//!
//! Two sides of the same question, kept apart because they answer to different
//! owners:
//!
//! * **Host side** — [`Roster`]: `members/<peer>.json`, one create-only file per
//!   admitted peer. Existence *is* membership, so the gossip gate is a single
//!   `stat` rather than a parse of a roster, and admission is monotonic: no lock,
//!   no read-modify-write, no lost update when `relay serve` admits a peer while
//!   `hive requests approve` runs in another process. Same
//!   monotonic-fact-as-create-only-file pattern as the escalation verdicts in
//!   `query::escalate` (#446).
//! * **Joiner side** — [`Membership`]: `membership.json`, the single hive this
//!   machine has joined. One hive per machine; §7.4 is silent on more than one,
//!   and a joiner gossiping into two hives would have to decide which one a unit
//!   came from, so the ceiling is stated here rather than discovered later.
//!
//! A pending request is an *absence*: it is in `join-requests.jsonl`, has no
//! member file, and has no `.denied` tombstone. Both terminal states are
//! create-only files, so a decision cannot be taken twice.
//!
//! This module depends on nothing behind the `relay` feature — it has to build
//! under `--no-default-features --features hive`, which is why the peer-id
//! grammar is inlined below instead of calling `relay::is_valid_peer_id`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::{epoch_ms, hive_dir};

/// Where this machine records the hive it has joined.
pub fn membership_path() -> PathBuf {
    hive_dir().join("membership.json")
}

// ────────────────────────────────────────────────────────────────────────────
// Peer ids as path segments
// ────────────────────────────────────────────────────────────────────────────

/// The peer-id grammar, kept in step with `relay::is_valid_peer_id` by a test
/// rather than by a call — that function sits behind the `relay` feature and
/// this module must build without it.
///
/// A peer id is used as a path segment, so this grammar is also the traversal
/// guard: with no `/` and no `.` permitted, neither `..` nor an absolute path
/// can be spelled.
pub fn is_valid_member_id(peer_id: &str) -> bool {
    !peer_id.is_empty()
        && peer_id.len() <= 128
        && peer_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ────────────────────────────────────────────────────────────────────────────
// Records
// ────────────────────────────────────────────────────────────────────────────

/// Why a peer is in the hive. Audit, not policy — nothing branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// Admitted on request, because the policy admits on pairing.
    Policy,
    /// Admitted by the owner deciding on a queued request.
    Approved,
    /// Already paired when the hive was first named. See [`Roster::grandfather`].
    Grandfathered,
}

impl Admission {
    pub fn as_str(&self) -> &'static str {
        match self {
            Admission::Policy => "policy",
            Admission::Approved => "approved",
            Admission::Grandfathered => "grandfathered",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub peer_id: String,
    pub hive_id: String,
    pub admitted_ms: u64,
    pub admission: Admission,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub id: String,
    pub peer_id: String,
    pub hive_id: String,
    pub requested_ms: u64,
    /// What the joiner called itself, when the link told it a name. Advisory.
    #[serde(default)]
    pub peer_label: Option<String>,
}

/// Where a request stands. Both terminal states are create-only files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    Pending,
    Approved,
    Denied,
}

impl RequestState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestState::Pending => "pending",
            RequestState::Approved => "approved",
            RequestState::Denied => "denied",
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Host side: the roster
// ────────────────────────────────────────────────────────────────────────────

/// The host-side roster, rooted at an explicit directory.
///
/// A struct rather than free functions reading `HOME`, because otherwise every
/// test here would have to mutate the process environment to isolate itself —
/// and `cargo test` runs threads in parallel, so that races. #433 hit exactly
/// that flake through `brain::decisions`; this shape makes it impossible.
#[derive(Debug, Clone)]
pub struct Roster {
    root: PathBuf,
}

impl Roster {
    /// A roster rooted at an explicit directory.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The roster for this machine.
    pub fn local() -> Self {
        Self::at(hive_dir())
    }

    pub fn members_dir(&self) -> PathBuf {
        self.root.join("members")
    }

    pub fn requests_path(&self) -> PathBuf {
        self.root.join("join-requests.jsonl")
    }

    pub fn denials_dir(&self) -> PathBuf {
        self.root.join("join-requests")
    }

    fn member_path(&self, peer_id: &str) -> Option<PathBuf> {
        if is_valid_member_id(peer_id) {
            Some(self.members_dir().join(format!("{peer_id}.json")))
        } else {
            None
        }
    }

    /// The denial tombstone is keyed on the **peer**, not on the request id.
    ///
    /// The owner denies a peer, not one particular ask. Keyed on the request id
    /// instead, a peer that asked twice and was denied once would still show as
    /// pending from its earlier ask — which is how this was first written, and
    /// what `asking_twice_before_anyone_answers_shows_one_row` catches. It also
    /// makes denial symmetric with `admit`, which is per-peer too.
    fn denial_path(&self, peer_id: &str) -> Option<PathBuf> {
        if is_valid_member_id(peer_id) {
            Some(self.denials_dir().join(format!("{peer_id}.denied")))
        } else {
            None
        }
    }

    /// Is this peer in our hive?
    ///
    /// The gossip gate calls this once per peer per tick, so it stays a `stat`.
    pub fn is_member(&self, peer_id: &str) -> bool {
        self.member_path(peer_id).is_some_and(|p| p.exists())
    }

    /// Admit a peer. Create-only: `Ok(false)` means they were already a member.
    ///
    /// `EEXIST` is the cross-process guarantee — two processes admitting the same
    /// peer at once cannot both believe they were first, and neither can clobber
    /// the other's `admitted_ms`. That matters because `relay serve` admits on
    /// request while `hive requests approve` runs as a separate process.
    pub fn admit(
        &self,
        peer_id: &str,
        hive_id: &str,
        admission: Admission,
    ) -> Result<bool, String> {
        let path = self
            .member_path(peer_id)
            .ok_or_else(|| format!("'{peer_id}' is not a usable peer id"))?;
        let dir = self.members_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

        let member = Member {
            peer_id: peer_id.to_string(),
            hive_id: hive_id.to_string(),
            admitted_ms: epoch_ms(),
            admission,
        };
        let body = serde_json::to_vec_pretty(&member)
            .map_err(|e| format!("cannot serialise member record: {e}"))?;

        match create_new_0600(&path) {
            Ok(mut f) => {
                f.write_all(&body)
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                f.sync_data()
                    .map_err(|e| format!("cannot flush {}: {e}", path.display()))?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(format!("cannot create {}: {e}", path.display())),
        }
    }

    /// Every admitted peer, sorted.
    pub fn list_members(&self) -> Vec<Member> {
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(self.members_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(body) = fs::read_to_string(&path) {
                    if let Ok(m) = serde_json::from_str::<Member>(&body) {
                        out.push(m);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        out
    }

    /// Admit every already-paired peer, once, when a hive is first named.
    ///
    /// Without this, naming a hive would silently stop gossip for everyone who
    /// was already paired: they would keep syncing right up until the owner gave
    /// the hive a name, then go quiet with nothing to point at. Those peers were
    /// trusted before the hive had a name, and naming it does not withdraw that.
    ///
    /// Create-only member files make this safe to call more than once — a peer
    /// admitted some other way keeps the admission it already had.
    ///
    /// A peer the owner has already *denied* is skipped. Today that cannot
    /// happen — denials need a named hive and this only runs on first naming —
    /// but leniency that could overturn an explicit refusal is worth closing at
    /// the function rather than relying on its one caller to stay careful.
    pub fn grandfather(&self, hive_id: &str, paired_peers: &[String]) -> Vec<String> {
        let denied: std::collections::HashSet<String> = self
            .list_requests()
            .into_iter()
            .filter(|r| self.request_state(r) == RequestState::Denied)
            .map(|r| r.peer_id)
            .collect();

        let mut admitted = Vec::new();
        for peer in paired_peers {
            if denied.contains(peer) {
                continue;
            }
            if let Ok(true) = self.admit(peer, hive_id, Admission::Grandfathered) {
                admitted.push(peer.clone());
            }
        }
        admitted
    }

    /// Record a join request. Returns the stored record.
    ///
    /// One `write(2)` under `O_APPEND`, so a concurrent append from another
    /// process cannot interleave inside the line.
    pub fn record_request(
        &self,
        peer_id: &str,
        hive_id: &str,
        peer_label: Option<String>,
    ) -> Result<JoinRequest, String> {
        if !is_valid_member_id(peer_id) {
            return Err(format!("'{peer_id}' is not a usable peer id"));
        }
        let entry = JoinRequest {
            id: gen_request_id(),
            peer_id: peer_id.to_string(),
            hive_id: hive_id.to_string(),
            requested_ms: epoch_ms(),
            peer_label,
        };

        fs::create_dir_all(&self.root)
            .map_err(|e| format!("cannot create {}: {e}", self.root.display()))?;
        let path = self.requests_path();

        let mut line = serde_json::to_string(&entry)
            .map_err(|e| format!("cannot serialise join request: {e}"))?;
        line.push('\n');

        let mut f =
            open_append_0600(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        f.write_all(line.as_bytes())
            .map_err(|e| format!("cannot append to {}: {e}", path.display()))?;
        f.sync_data()
            .map_err(|e| format!("cannot flush {}: {e}", path.display()))?;

        Ok(entry)
    }

    /// Every request ever recorded, oldest first. A malformed line is skipped
    /// rather than failing the read — one bad append must not hide the queue.
    pub fn list_requests(&self) -> Vec<JoinRequest> {
        let Ok(body) = fs::read_to_string(self.requests_path()) else {
            return Vec::new();
        };
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<JoinRequest>(l).ok())
            .collect()
    }

    /// Where a request stands now.
    pub fn request_state(&self, entry: &JoinRequest) -> RequestState {
        if self.is_member(&entry.peer_id) {
            return RequestState::Approved;
        }
        if self.denial_path(&entry.peer_id).is_some_and(|p| p.exists()) {
            return RequestState::Denied;
        }
        RequestState::Pending
    }

    /// The newest pending request from each peer.
    ///
    /// A peer that asks twice before anyone answers should appear once; the
    /// later ask is the live one.
    pub fn pending_requests(&self) -> Vec<JoinRequest> {
        let mut newest: std::collections::BTreeMap<String, JoinRequest> =
            std::collections::BTreeMap::new();
        for entry in self.list_requests() {
            if self.request_state(&entry) != RequestState::Pending {
                continue;
            }
            newest
                .entry(entry.peer_id.clone())
                .and_modify(|held| {
                    if entry.requested_ms >= held.requested_ms {
                        *held = entry.clone();
                    }
                })
                .or_insert(entry);
        }
        newest.into_values().collect()
    }

    /// The newest pending request from `peer_id`, if any.
    pub fn pending_for(&self, peer_id: &str) -> Option<JoinRequest> {
        self.pending_requests()
            .into_iter()
            .find(|r| r.peer_id == peer_id)
    }

    /// Deny a request. Create-only, so a denial cannot race an approval into
    /// both states.
    pub fn deny(&self, entry: &JoinRequest) -> Result<(), String> {
        match self.request_state(entry) {
            RequestState::Pending => {}
            decided => {
                return Err(format!("{} is already {}", entry.peer_id, decided.as_str()));
            }
        }
        let path = self
            .denial_path(&entry.peer_id)
            .ok_or_else(|| format!("'{}' is not a usable peer id", entry.peer_id))?;
        let dir = self.denials_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        match create_new_0600(&path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(format!("{} is already denied", entry.peer_id))
            }
            Err(e) => Err(format!("cannot create {}: {e}", path.display())),
        }
    }
}

/// Mint a request id.
///
/// Clock-derived like `gen_hive_id`, plus a counter so two requests inside the
/// same millisecond stay distinguishable in the log.
fn gen_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("jr_{}_{}", epoch_ms(), SEQ.fetch_add(1, Ordering::Relaxed))
}

// The free functions below are what the rest of the binary calls: the local
// roster, rooted under `HOME`.

pub fn is_member(peer_id: &str) -> bool {
    Roster::local().is_member(peer_id)
}

pub fn admit(peer_id: &str, hive_id: &str, admission: Admission) -> Result<bool, String> {
    Roster::local().admit(peer_id, hive_id, admission)
}

pub fn list_members() -> Vec<Member> {
    Roster::local().list_members()
}

pub fn grandfather(hive_id: &str, paired_peers: &[String]) -> Vec<String> {
    Roster::local().grandfather(hive_id, paired_peers)
}

pub fn record_request(
    peer_id: &str,
    hive_id: &str,
    peer_label: Option<String>,
) -> Result<JoinRequest, String> {
    Roster::local().record_request(peer_id, hive_id, peer_label)
}

pub fn pending_requests() -> Vec<JoinRequest> {
    Roster::local().pending_requests()
}

pub fn pending_for(peer_id: &str) -> Option<JoinRequest> {
    Roster::local().pending_for(peer_id)
}

pub fn deny(entry: &JoinRequest) -> Result<(), String> {
    Roster::local().deny(entry)
}

// ────────────────────────────────────────────────────────────────────────────
// Joiner side: which hive this machine has joined
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    /// The host has queued our request for its owner to decide on.
    Pending,
    /// The host has admitted us.
    Member,
}

impl MemberState {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemberState::Pending => "pending",
            MemberState::Member => "member",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Membership {
    pub hive_id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// The peer we joined through — the machine that answers for this hive.
    pub joined_via: String,
    pub state: MemberState,
    pub joined_ms: u64,
}

/// Read this machine's membership. `Ok(None)` means we have joined no hive.
///
/// A malformed file is an error, never silently downgraded to "no membership" —
/// that would quietly stop gossip with nothing to point at.
pub fn load_membership() -> Result<Option<Membership>, String> {
    load_membership_from(&membership_path())
}

pub fn load_membership_from(path: &Path) -> Result<Option<Membership>, String> {
    let body = match fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    serde_json::from_str(&body)
        .map(Some)
        .map_err(|e| format!("{} is not valid membership JSON: {e}", path.display()))
}

/// Write this machine's membership, atomically.
pub fn save_membership(m: &Membership) -> Result<(), String> {
    save_membership_to(&membership_path(), m)
}

pub fn save_membership_to(path: &Path, m: &Membership) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let body =
        serde_json::to_vec_pretty(m).map_err(|e| format!("cannot serialise membership: {e}"))?;

    let tmp = path.with_extension("json.tmp");
    {
        let mut f =
            fs::File::create(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        set_0600(&f);
        f.write_all(&body)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        f.sync_data()
            .map_err(|e| format!("cannot flush {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// Forget this machine's membership.
pub fn clear_membership() -> Result<bool, String> {
    let path = membership_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// File helpers
// ────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn set_0600(f: &fs::File) {
    use std::os::unix::fs::PermissionsExt;
    let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_0600(_f: &fs::File) {}

/// Create a file that must not already exist, 0600 before the first byte.
fn create_new_0600(path: &Path) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn open_append_0600(path: &Path) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> (tempfile::TempDir, Roster) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = Roster::at(tmp.path());
        (tmp, r)
    }

    #[test]
    fn a_peer_is_not_a_member_until_admitted() {
        let (_tmp, r) = roster();
        assert!(!r.is_member("laptop-a3f2"));
        assert!(r.admit("laptop-a3f2", "hv_1", Admission::Policy).unwrap());
        assert!(r.is_member("laptop-a3f2"));
    }

    #[test]
    fn admitting_twice_is_not_an_error_but_is_not_a_second_admission() {
        let (_tmp, r) = roster();
        assert!(r.admit("laptop-a3f2", "hv_1", Admission::Policy).unwrap());
        // The create-only backstop: `false`, not an error, and the original
        // record survives.
        assert!(!r.admit("laptop-a3f2", "hv_1", Admission::Approved).unwrap());
        let members = r.list_members();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].admission, Admission::Policy);
    }

    #[test]
    fn a_recorded_request_is_pending_and_confers_no_membership() {
        let (_tmp, r) = roster();
        let entry = r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        assert_eq!(r.request_state(&entry), RequestState::Pending);
        // The whole point of `ask`: asking is not joining.
        assert!(!r.is_member("laptop-a3f2"));
        assert_eq!(r.pending_requests().len(), 1);
    }

    #[test]
    fn approving_makes_the_request_approved_and_the_peer_a_member() {
        let (_tmp, r) = roster();
        let entry = r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        r.admit(&entry.peer_id, "hv_1", Admission::Approved)
            .unwrap();
        assert_eq!(r.request_state(&entry), RequestState::Approved);
        assert!(r.is_member("laptop-a3f2"));
        assert!(r.pending_requests().is_empty());
    }

    #[test]
    fn denying_leaves_no_membership_and_clears_the_queue() {
        let (_tmp, r) = roster();
        let entry = r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        r.deny(&entry).unwrap();
        assert_eq!(r.request_state(&entry), RequestState::Denied);
        assert!(!r.is_member("laptop-a3f2"));
        assert!(r.pending_requests().is_empty());
    }

    #[test]
    fn a_decision_cannot_be_taken_twice() {
        let (_tmp, r) = roster();
        let entry = r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        r.deny(&entry).unwrap();
        assert!(r.deny(&entry).unwrap_err().contains("already denied"));

        let other = r.record_request("desk-b1", "hv_1", None).unwrap();
        r.admit("desk-b1", "hv_1", Admission::Approved).unwrap();
        assert!(r.deny(&other).unwrap_err().contains("already approved"));
    }

    #[test]
    fn asking_twice_before_anyone_answers_shows_one_row() {
        let (_tmp, r) = roster();
        r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        let pending = r.pending_requests();
        assert_eq!(pending.len(), 1, "one peer, one row");
        // And denying the live one settles the peer, rather than leaving the
        // earlier ask behind as a second pending row.
        r.deny(&pending[0]).unwrap();
        assert!(
            r.pending_requests().is_empty(),
            "a settled peer leaves no pending row"
        );
    }

    #[test]
    fn two_requests_in_the_same_millisecond_get_distinct_ids() {
        let (_tmp, r) = roster();
        let a = r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        let b = r.record_request("desk-b1", "hv_1", None).unwrap();
        assert_ne!(a.id, b.id, "the queue log must not conflate two asks");
    }

    #[test]
    fn grandfathering_admits_paired_peers_once() {
        let (_tmp, r) = roster();
        let paired = vec!["laptop-a3f2".to_string(), "desk-b1".to_string()];
        let first = r.grandfather("hv_1", &paired);
        assert_eq!(first.len(), 2);
        assert!(r.is_member("laptop-a3f2") && r.is_member("desk-b1"));
        // Called again (a rename, say) it admits nobody new.
        assert!(r.grandfather("hv_1", &paired).is_empty());
    }

    #[test]
    fn grandfathering_does_not_resurrect_a_denied_peer() {
        let (_tmp, r) = roster();
        let entry = r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        r.deny(&entry).unwrap();
        // A denied peer is still *paired*, so it would appear in the grandfather
        // list. Leniency must not overturn an explicit refusal.
        let admitted = r.grandfather("hv_1", &["laptop-a3f2".to_string(), "desk-b1".to_string()]);
        assert_eq!(admitted, vec!["desk-b1".to_string()]);
        assert!(!r.is_member("laptop-a3f2"));
        assert!(r.is_member("desk-b1"));
    }

    #[test]
    fn a_peer_id_that_is_not_a_safe_path_segment_is_refused() {
        let (_tmp, r) = roster();
        for bad in [
            "..",
            "../escape",
            "a/b",
            "with.dot",
            "with space",
            "",
            "tilde~",
        ] {
            assert!(
                r.admit(bad, "hv_1", Admission::Policy).is_err(),
                "{bad:?} must not become a filename"
            );
            assert!(!r.is_member(bad));
        }
        assert!(
            r.admit(&"x".repeat(129), "hv_1", Admission::Policy)
                .is_err()
        );
    }

    #[test]
    fn a_malformed_line_does_not_hide_the_rest_of_the_queue() {
        let (_tmp, r) = roster();
        r.record_request("laptop-a3f2", "hv_1", None).unwrap();
        let mut f = open_append_0600(&r.requests_path()).unwrap();
        f.write_all(b"{ this is not json\n").unwrap();
        drop(f);
        r.record_request("desk-b1", "hv_1", None).unwrap();
        assert_eq!(r.pending_requests().len(), 2);
    }

    #[test]
    fn membership_round_trips_and_absence_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("membership.json");
        assert!(load_membership_from(&path).unwrap().is_none());

        let m = Membership {
            hive_id: "hv_3a9f21".into(),
            name: Some("barrys-hive".into()),
            joined_via: "laptop-a3f2".into(),
            state: MemberState::Pending,
            joined_ms: 1_700_000_000_000,
        };
        save_membership_to(&path, &m).unwrap();
        let back = load_membership_from(&path).unwrap().unwrap();
        assert_eq!(back.hive_id, "hv_3a9f21");
        assert_eq!(back.state, MemberState::Pending);
        assert_eq!(back.joined_via, "laptop-a3f2");
    }

    #[test]
    fn a_malformed_membership_file_is_an_error_not_an_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("membership.json");
        fs::write(&path, "{ not json").unwrap();
        // Downgrading this to `None` would stop gossip with nothing to point at.
        assert!(load_membership_from(&path).is_err());
    }

    /// The inlined grammar must agree with the one the relay enforces, or a peer
    /// the relay accepts could be unrepresentable in the roster.
    #[cfg(feature = "relay")]
    #[test]
    fn the_inlined_peer_id_grammar_agrees_with_the_relay() {
        for candidate in [
            "laptop-a3f2",
            "desk_b1",
            "UPPER123",
            "a",
            "",
            "..",
            "a/b",
            "with.dot",
            "with space",
            "tilde~",
            "plus+one",
            "at@sign",
            &"x".repeat(128),
            &"x".repeat(129),
        ] {
            assert_eq!(
                is_valid_member_id(candidate),
                crate::relay::is_valid_peer_id(candidate),
                "disagreement on {candidate:?}"
            );
        }
    }
}

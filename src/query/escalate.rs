//! The owner's escalation queue — §4.3's "anything else" row.
//!
//! When classification lands in the middle band, §4.3 says: "**Escalate**:
//! queue for the owner, return 'pending review.'" This is the queue. One
//! append-only file, `~/.claudectl/access/escalations.jsonl`, with the same
//! 0600-from-creation discipline `audit.jsonl` has — a queue of third-party
//! questions is as sensitive as a log of them.
//!
//! # The lifecycle
//!
//! #430 shipped the queue alone: the append, the `esc_<hex>` id handed back to
//! the caller, and `claudectl access escalations` to read it. #446 closed the
//! loop — a verdict, a caller-facing poll, and an expiry — without giving up the
//! append-only property above.
//!
//! The trick is that **state lives beside the queue, not in it.** A decision is
//! one file at `escalation-verdicts/<id>.json`, created with `create_new`, so:
//!
//! - the queue file is still only ever appended to;
//! - "decided twice" is `EEXIST` from the filesystem rather than a lost write,
//!   across processes and with no lock — `access escalations approve` runs in
//!   the owner's shell, `query serve` answers the poll, and neither can clobber
//!   the other. This is the same reasoning that makes a grant's revocation a
//!   create-only tombstone rather than a mutable field;
//! - `expired` needs no file at all. It is derived from the row's timestamp and
//!   [`crate::query::thresholds::ESCALATION_TTL_MS`], so there is no sweeper
//!   process and a caller's poll never mutates the owner's queue.
//!
//! A verdict always outranks the clock: an owner who decided on the last day
//! decided it, and the caller sees that rather than an expiry that landed while
//! they were polling.
//!
//! # Why the record keeps the full question
//!
//! `audit.jsonl` truncates a question to 512 bytes, because it is a log and a
//! log is read in bulk. An escalation is read *one at a time by a person
//! deciding what to do about it*, and a decision made on a truncated question
//! is a worse decision. The size is already bounded by the grant's daily
//! budget — a holder cannot queue more rows than they have queries.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::access::set_owner_only;

use super::jev::Classification;

/// One queued query awaiting the owner.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Escalation {
    pub id: String,
    pub ts_ms: u64,
    pub grant_id: String,
    pub project: String,
    /// The question in full — see the module note on why this is not truncated.
    pub question: String,
    /// All five numbers. The owner is reviewing *because* of these, so the
    /// record that asks for their attention has to say what caused it.
    pub classification: Classification,
    /// The index fingerprint this question was classified against.
    ///
    /// `#[serde(default)]` because #430 shipped rows without it: without the
    /// default every pre-existing line fails to parse, `read` skips it, and the
    /// queue silently empties — losing exactly the questions this issue exists
    /// to answer. An empty fingerprint means "queued before the field existed".
    #[serde(default)]
    pub fingerprint: String,
}

/// What the owner decided. Written once, never edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictState {
    Approved,
    Denied,
}

impl VerdictState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
}

/// The owner's decision on one escalation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Verdict {
    pub state: VerdictState,
    pub ts_ms: u64,
    /// The owner's own words, shown to the caller alongside the spans. Optional
    /// because approving without comment is the common case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Where one escalation stands, from the caller's point of view.
///
/// `Expired` is **derived**, never stored: a pending row older than the TTL is
/// expired by arithmetic. Storing it would need a sweeper process, or a
/// write-on-read that turns polling into mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum EscalationState {
    Pending,
    Approved { note: Option<String>, ts_ms: u64 },
    Denied { note: Option<String>, ts_ms: u64 },
    Expired,
}

impl EscalationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending_review",
            Self::Approved { .. } => "approved",
            Self::Denied { .. } => "denied",
            Self::Expired => "expired",
        }
    }

    /// Whether the owner has already decided. An undecided row can still be
    /// approved or denied; a decided one cannot be re-decided.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// Mint an escalation id. 48 bits — a collision is two rows sharing an id in
/// an append-only file, which is a display nuisance, not a clobber.
pub fn gen_escalation_id() -> String {
    format!("esc_{}", crate::relay::crypto::random_hex(6))
}

/// The append-only queue file.
pub struct EscalationQueue {
    path: PathBuf,
}

impl EscalationQueue {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        EscalationQueue { path: path.into() }
    }

    /// The queue beside the grants whose queries fill it.
    pub fn in_access_dir(root: &Path) -> Self {
        Self::new(root.join("escalations.jsonl"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record and return it.
    ///
    /// Append, never rewrite: a queue the surface can rewrite is a queue a
    /// surface bug can empty, and this file is the only trace of a question
    /// that was neither answered nor denied.
    pub fn push(&self, entry: &Escalation) -> Result<(), String> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| "escalation path has no parent".to_string())?;
        fs::create_dir_all(dir).map_err(|e| format!("create access dir: {e}"))?;
        let line = serde_json::to_string(entry).map_err(|e| format!("encode escalation: {e}"))?;
        let mut opts = fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&self.path)
            .map_err(|e| format!("open escalation queue: {e}"))?;
        // `mode` applies only on creation, so repair a file an earlier run left
        // wider — the same unconditional repair `append_audit` does.
        set_owner_only(&self.path)?;
        // **One** `write(2)`, newline included. `writeln!` on an unbuffered
        // `File` issues two — one for the content, one for the newline — and
        // `QueryServer` is thread-per-connection, so two holders escalating at
        // once interleaved into `{..A}{..B}\n\n`. `read` skips the merged line,
        // so **both** escalations vanished while both callers held an `esc_`
        // id that would never appear. Under `O_APPEND` a single write lands at
        // one offset atomically, which is what makes the append safe without a
        // lock. Measured: 150 corrupt lines in 4000 with `writeln!`, 0 with
        // this.
        f.write_all(format!("{line}\n").as_bytes())
            .map_err(|e| format!("write escalation: {e}"))
    }

    /// Every readable record, oldest first.
    ///
    /// An unparseable line is skipped rather than fatal, so one bad row cannot
    /// hide the queue — the same choice `read_audit` and `GrantStore::list`
    /// make.
    pub fn read(&self) -> Vec<Escalation> {
        let Ok(body) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Escalation>(l).ok())
            .collect()
    }

    /// One record by id, or `None`.
    ///
    /// First match wins. Ids are 48 bits, so a duplicate is a display nuisance
    /// the module doc already accepts; resolving it to the *oldest* row keeps
    /// the id stable once a caller holds it.
    pub fn find(&self, id: &str) -> Option<Escalation> {
        self.read().into_iter().find(|e| e.id == id)
    }

    /// Directory holding one verdict file per decided escalation.
    fn verdicts_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(|d| d.join("escalation-verdicts"))
            .unwrap_or_else(|| PathBuf::from("escalation-verdicts"))
    }

    /// A verdict path, or `None` for an id that could escape the directory.
    ///
    /// The same validation gate grant ids pass: an id is interpolated into a
    /// path, so `../` or a separator in it is a traversal.
    fn verdict_path(&self, id: &str) -> Option<PathBuf> {
        if !is_valid_escalation_id(id) {
            return None;
        }
        Some(self.verdicts_dir().join(format!("{id}.json")))
    }

    /// The owner's decision on one escalation, if they have made one.
    pub fn verdict(&self, id: &str) -> Option<Verdict> {
        let path = self.verdict_path(id)?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Record a decision. Fails if one is already recorded.
    ///
    /// `create_new` is load-bearing: the filesystem refusing a second create is
    /// what makes deciding twice an error instead of a silent overwrite, with no
    /// lock and across processes. `access escalations approve` and `deny` run in
    /// a different process from `query serve`, so a read-then-write check here
    /// would race.
    pub fn decide(
        &self,
        id: &str,
        state: VerdictState,
        note: Option<String>,
        now_ms: u64,
    ) -> Result<Verdict, String> {
        let path = self
            .verdict_path(id)
            .ok_or_else(|| format!("invalid escalation id: {id}"))?;
        let dir = self.verdicts_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("create verdicts dir: {e}"))?;
        let verdict = Verdict {
            state,
            ts_ms: now_ms,
            note,
        };
        let line = serde_json::to_string(&verdict).map_err(|e| format!("encode verdict: {e}"))?;
        let mut f = crate::access::create_new_private(&path).map_err(|e| {
            if e.starts_with("already exists") {
                format!("escalation {id} has already been decided")
            } else {
                e
            }
        })?;
        f.write_all(line.as_bytes())
            .map_err(|e| format!("write verdict: {e}"))?;
        f.sync_data().map_err(|e| format!("sync verdict: {e}"))?;
        Ok(verdict)
    }

    /// Where a record stands now, folding in the derived expiry.
    ///
    /// A verdict always wins over the clock: an owner who approved a row on its
    /// last day decided it, and the caller should see that rather than an
    /// expiry that arrived while they were polling.
    pub fn state(&self, entry: &Escalation, now_ms: u64, ttl_ms: u64) -> EscalationState {
        match self.verdict(&entry.id) {
            Some(v) => match v.state {
                VerdictState::Approved => EscalationState::Approved {
                    note: v.note,
                    ts_ms: v.ts_ms,
                },
                VerdictState::Denied => EscalationState::Denied {
                    note: v.note,
                    ts_ms: v.ts_ms,
                },
            },
            None if now_ms.saturating_sub(entry.ts_ms) > ttl_ms => EscalationState::Expired,
            None => EscalationState::Pending,
        }
    }
}

/// Whether `id` is a well-formed escalation id.
///
/// `esc_` plus 12 lowercase hex, which is what [`gen_escalation_id`] mints.
/// Checked rather than trusted because the id arrives from a URL path and is
/// then interpolated into a filename.
pub fn is_valid_escalation_id(id: &str) -> bool {
    let Some(hex) = id.strip_prefix("esc_") else {
        return false;
    };
    hex.len() == 12
        && hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::jev::Intent;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "claudectl-esc-{tag}-{}-{}",
            std::process::id(),
            crate::access::epoch_ms()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample(id: &str, question: &str) -> Escalation {
        Escalation {
            id: id.into(),
            ts_ms: 1_791_200_000_000,
            grant_id: "gr_abc123".into(),
            project: "claudectl".into(),
            question: question.into(),
            classification: Classification {
                intent: Intent::Structure,
                intent_confidence: 0.44,
                answerable_from_docs: 0.55,
                seeks_sensitive: 0.03,
                injection_attempt: 0.01,
                scope_match: 0.88,
                input_tokens: 600,
            },
            fingerprint: "fnv1a:deadbeef".into(),
        }
    }

    #[test]
    fn an_id_is_prefixed_and_forty_eight_bits() {
        let id = gen_escalation_id();
        assert!(id.starts_with("esc_"), "{id}");
        assert_eq!(id.len(), 4 + 12);
        assert!(id[4..].chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        assert_ne!(id, gen_escalation_id());
    }

    #[test]
    fn records_round_trip_in_order() {
        let dir = tmpdir("order");
        let q = EscalationQueue::in_access_dir(&dir);
        assert!(q.read().is_empty());
        q.push(&sample("esc_000000000001", "first")).unwrap();
        q.push(&sample("esc_000000000002", "second")).unwrap();
        let rows = q.read();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].question, "first");
        assert_eq!(rows[1].id, "esc_000000000002");
        // The five numbers survive the round trip — they are the reason the row
        // exists.
        assert!((rows[0].classification.answerable_from_docs - 0.55).abs() < 1e-9);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_full_question_is_kept_past_the_audit_truncation() {
        let dir = tmpdir("full");
        let q = EscalationQueue::in_access_dir(&dir);
        let long = "x".repeat(2_000);
        q.push(&sample("esc_000000000003", &long)).unwrap();
        assert_eq!(q.read()[0].question.len(), 2_000);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_corrupt_line_does_not_hide_the_queue() {
        let dir = tmpdir("corrupt");
        let q = EscalationQueue::in_access_dir(&dir);
        q.push(&sample("esc_000000000004", "good")).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(q.path())
            .unwrap()
            .write_all(b"{truncated\n")
            .unwrap();
        q.push(&sample("esc_000000000005", "also good")).unwrap();
        let rows = q.read();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].question, "also good");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_queue_file_is_owner_only_and_repaired_if_widened() {
        let dir = tmpdir("mode");
        let q = EscalationQueue::in_access_dir(&dir);
        q.push(&sample("esc_000000000006", "q")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(q.path(), fs::Permissions::from_mode(0o644)).unwrap();
            q.push(&sample("esc_000000000007", "q2")).unwrap();
            let mode = fs::metadata(q.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{mode:o}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_pushes_never_merge_two_rows_into_one() {
        // `writeln!` on an unbuffered `File` is *two* `write(2)` calls — one
        // for the content, one for the newline — and `QueryServer` is
        // thread-per-connection. Under `O_APPEND` those fragments interleave,
        // producing `{..A}{..B}\n\n`: `read` skips the merged line and **both**
        // escalations vanish, while both callers hold an `esc_` id that will
        // never appear in the queue.
        //
        // Measured on this machine before the fix: 150 corrupt lines in 4000.
        // `write_all` is a loop over `write`, but a line this size is one
        // `write(2)` in practice, and a single `write(2)` under `O_APPEND` is
        // positionally atomic — which is what makes the lock-free append safe.
        // This test, not the reasoning, is the evidence.
        let dir = tmpdir("concurrent");
        let q = std::sync::Arc::new(EscalationQueue::in_access_dir(&dir));
        let per_thread = 250;

        let mut handles = Vec::new();
        for t in 0..4 {
            let q = std::sync::Arc::clone(&q);
            handles.push(std::thread::spawn(move || {
                for i in 0..per_thread {
                    // Long questions, so a split would be unmistakable.
                    let question = format!("t{t}-{i}-{}", "x".repeat(300));
                    q.push(&sample(&format!("esc_{t:06}{i:06}"), &question))
                        .expect("push");
                }
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }

        let rows = q.read();
        let raw = fs::read_to_string(q.path()).unwrap();
        // Every line parsed, and none was lost to a merge.
        assert_eq!(
            rows.len(),
            4 * per_thread,
            "{} of {} rows survived",
            rows.len(),
            4 * per_thread
        );
        assert_eq!(
            raw.lines().filter(|l| !l.trim().is_empty()).count(),
            4 * per_thread,
            "a stranded newline or a merged line is in the file"
        );
        assert!(!raw.contains("}{"), "two records landed on one line");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_row_written_before_the_fingerprint_field_existed_still_parses() {
        // #430 shipped rows with no `fingerprint`. Without `serde(default)` every
        // one of them fails to parse, `read` skips it, and the queue silently
        // empties — losing the questions #446 exists to answer.
        let dir = tmpdir("compat");
        let q = EscalationQueue::in_access_dir(&dir);
        let legacy = r#"{"id":"esc_0000000000ff","ts_ms":1791200000000,"grant_id":"gr_abc123","project":"claudectl","question":"how is auth structured?","classification":{"intent":"structure","intent_confidence":0.44,"answerable_from_docs":0.55,"seeks_sensitive":0.03,"injection_attempt":0.01,"scope_match":0.88,"input_tokens":600}}"#;
        fs::create_dir_all(&dir).unwrap();
        fs::write(q.path(), format!("{legacy}\n")).unwrap();
        let rows = q.read();
        assert_eq!(rows.len(), 1, "a pre-#446 row was dropped");
        assert_eq!(
            rows[0].fingerprint, "",
            "absent means empty, not a parse failure"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_decision_cannot_be_made_twice() {
        // The invariant the whole verdict store exists for: `create_new` makes
        // the filesystem refuse the second write, across processes, with no lock.
        let dir = tmpdir("twice");
        let q = EscalationQueue::in_access_dir(&dir);
        let e = sample("esc_00000000000a", "q");
        q.push(&e).unwrap();

        q.decide(&e.id, VerdictState::Approved, Some("yes".into()), 10)
            .expect("first decision lands");
        let second = q.decide(&e.id, VerdictState::Denied, None, 20);
        assert!(second.is_err(), "a second decision overwrote the first");
        assert!(
            second.unwrap_err().contains("already been decided"),
            "the error should say why"
        );
        // And the first verdict is intact — not clobbered by the attempt.
        let v = q.verdict(&e.id).expect("verdict still there");
        assert_eq!(v.state, VerdictState::Approved);
        assert_eq!(v.note.as_deref(), Some("yes"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expiry_is_derived_and_flips_at_the_boundary() {
        let dir = tmpdir("expiry");
        let q = EscalationQueue::in_access_dir(&dir);
        let mut e = sample("esc_00000000000b", "q");
        e.ts_ms = 1_000_000;
        q.push(&e).unwrap();
        let ttl = 1_000;

        assert_eq!(q.state(&e, e.ts_ms, ttl), EscalationState::Pending);
        // Exactly at the TTL is still pending — the check is `>`, not `>=`.
        assert_eq!(q.state(&e, e.ts_ms + ttl, ttl), EscalationState::Pending);
        assert_eq!(
            q.state(&e, e.ts_ms + ttl + 1, ttl),
            EscalationState::Expired
        );
        // Nothing was written to reach that conclusion.
        assert!(!q.verdicts_dir().exists(), "expiry wrote a file");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_verdict_outranks_the_clock() {
        // An owner who approved on the last day decided it. The caller should
        // see the decision, not an expiry that arrived while they polled.
        let dir = tmpdir("outrank");
        let q = EscalationQueue::in_access_dir(&dir);
        let mut e = sample("esc_00000000000c", "q");
        e.ts_ms = 1_000_000;
        q.push(&e).unwrap();
        q.decide(&e.id, VerdictState::Approved, None, e.ts_ms + 10)
            .unwrap();
        let far_future = e.ts_ms + 10_000_000;
        assert!(
            matches!(
                q.state(&e, far_future, 1_000),
                EscalationState::Approved { .. }
            ),
            "expiry overrode a recorded decision"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_id_that_could_escape_the_directory_is_refused() {
        let dir = tmpdir("traversal");
        let q = EscalationQueue::in_access_dir(&dir);
        for bad in [
            "../escape",
            "esc_../../x",
            "esc_short",
            "esc_ABCDEF123456",
            "esc_zzzzzzzzzzzz",
            "",
        ] {
            assert!(!is_valid_escalation_id(bad), "{bad} passed validation");
            assert!(
                q.decide(bad, VerdictState::Approved, None, 1).is_err(),
                "{bad} was accepted"
            );
            assert!(q.verdict(bad).is_none(), "{bad} resolved to a file");
        }
        assert!(is_valid_escalation_id(&gen_escalation_id()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_id_has_no_record_and_no_verdict() {
        let dir = tmpdir("unknown");
        let q = EscalationQueue::in_access_dir(&dir);
        q.push(&sample("esc_00000000000d", "q")).unwrap();
        assert!(q.find("esc_00000000000e").is_none());
        assert!(q.verdict("esc_00000000000e").is_none());
        assert!(q.find("esc_00000000000d").is_some());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_verdict_file_is_owner_only() {
        let dir = tmpdir("vmode");
        let q = EscalationQueue::in_access_dir(&dir);
        let e = sample("esc_00000000000f", "q");
        q.push(&e).unwrap();
        q.decide(&e.id, VerdictState::Denied, None, 1).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = q.verdict_path(&e.id).unwrap();
            let mode = fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{mode:o}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reading_an_absent_queue_is_empty_and_creates_nothing() {
        let dir = tmpdir("absent");
        let q = EscalationQueue::in_access_dir(&dir);
        assert!(q.read().is_empty());
        assert!(!q.path().exists());
        fs::remove_dir_all(&dir).ok();
    }
}

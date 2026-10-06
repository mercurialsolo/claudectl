//! The owner's escalation queue — §4.3's "anything else" row.
//!
//! When classification lands in the middle band, §4.3 says: "**Escalate**:
//! queue for the owner, return 'pending review.'" This is the queue. One
//! append-only file, `~/.claudectl/access/escalations.jsonl`, with the same
//! 0600-from-creation discipline `audit.jsonl` has — a queue of third-party
//! questions is as sensitive as a log of them.
//!
//! # What ships here, and what does not
//!
//! Ships: the append, the `esc_<hex>` id handed back to the caller, and
//! `claudectl access escalations` to read the queue.
//!
//! **Not shipped, and deliberately:** approve, deny, notify, and the resume
//! path that turns an approved escalation into an answer. That is a
//! notification system plus a state machine on a durable record, and it would
//! more than double this change. #430 asks for a queue and a "pending review"
//! response; it gets exactly those, and the follow-up is filed rather than
//! left looking finished.
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
        writeln!(f, "{line}").map_err(|e| format!("write escalation: {e}"))
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
    fn reading_an_absent_queue_is_empty_and_creates_nothing() {
        let dir = tmpdir("absent");
        let q = EscalationQueue::in_access_dir(&dir);
        assert!(q.read().is_empty());
        assert!(!q.path().exists());
        fs::remove_dir_all(&dir).ok();
    }
}

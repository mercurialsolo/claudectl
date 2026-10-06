//! Capability grants — scoped, expiring, revocable read-only access for a
//! third party (#427, open-cluster RFC §3.2, §3.3, §5).
//!
//! Everything else in claudectl assumes one trust level: you, on your
//! machines. A relay PSK is symmetric, and the coordinator's bearer token is a
//! single shared secret with no identity, no scope and no per-grant
//! revocation. This module adds the missing primitive — a participant *less*
//! trusted than you, bounded by capability rather than good manners.
//!
//! Layout under `~/.claudectl/access/`:
//!
//! ```text
//! secret                      HMAC key, 0600, never leaves the machine
//! grants/<grant_id>.json      one record per grant
//! audit.jsonl                 append-only, allowed and denied alike
//! escalations.jsonl           queries classification queued for the owner (#430)
//! jev-spend.json              this month's classification spend (#430)
//! ```
//!
//! This module is the spine: mint, verify, list, audit, revoke. It opens no
//! port of its own — the surface that presents these credentials is
//! `src/query/` (#429), which injects a `GrantStore` and secret rather than
//! opening the default store per request. #431 added the enforcement of
//! `rate_limit_per_min` and `daily_query_budget` — `charge_daily_budget` and
//! `verify_detailed` here, the ordering and the refusals in `query::core`. #430
//! added two more files under this directory and two unsigned fields on a grant
//! (`flagged_ms`, `flag_reason`) — written by `query::classify`'s routing, read
//! by `access list`.
//!
//! Gated behind the `relay` feature because the MAC comes from
//! `relay::crypto`, which keeps the "no new dependency, no JWT library, no
//! asymmetric crypto" property. The minimal `--no-default-features --features
//! hive` build simply has no access surface.

pub mod cli;
pub mod grant;
pub mod scope;
pub mod token;

use std::fs;
use std::path::{Path, PathBuf};

pub use grant::{Grant, GrantStore, new_grant};
pub use scope::Scope;
// `grant::AuditEntry` stays reachable by its full path rather than re-exported
// here. Two consumers now: `query::core` writes them and `cli::cmd_audit`
// renders them, both by full path.

/// Restrict a file to its owner. No-op off unix.
///
/// Shared rather than per-file: three files under `~/.claudectl/access` now
/// need the same discipline — `audit.jsonl`, `escalations.jsonl` and
/// `jev-spend.json` — and a fourth copy of it is a fourth chance to forget.
pub(crate) fn set_owner_only(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Create a file owner-only *at creation*, so there is no window in which it
/// exists with a wider mode.
///
/// `File::create` opens `0o666 & ~umask` — 0644 on a default umask — and a
/// later `chmod` does not revoke descriptors another process already holds.
/// Setting the mode in the `open(2)` call closes that race.
pub(crate) fn create_private(path: &Path) -> Result<fs::File, String> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))
}

/// Create a file that must not already exist, 0600 from its first byte.
///
/// [`create_private`] truncates, which is right for a marker whose content is
/// irrelevant and wrong for a record that carries one. `create_new` makes
/// `EEXIST` the filesystem's answer to a second write, so "decided twice" is a
/// refusal rather than a clobber — the property an escalation verdict needs and
/// a revocation tombstone does not.
pub(crate) fn create_new_private(path: &Path) -> Result<fs::File, String> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!("already exists: {}", path.display())
        } else {
            format!("create {}: {e}", path.display())
        }
    })
}

/// Maximum grant-id length, matching `relay::is_valid_peer_id`'s shape.
const MAX_GRANT_ID_LEN: usize = 64;

/// `~/.claudectl/access`, or an error when `HOME` is unset.
///
/// Every other store in the codebase falls back to `/tmp` when `HOME` is
/// missing. This one must not: `/tmp` is world-writable, and the secret is read
/// back with `read_to_string`, which follows symlinks. Someone who pre-places
/// `/tmp/.claudectl/access/secret` — or a symlink there — before the first
/// `access grant` would be supplying the key every grant MAC derives from, and
/// could then mint valid tokens. Refusing is the only safe answer.
pub fn access_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| {
        "HOME is not set, and the access store will not fall back to a \
         world-writable directory for an HMAC key"
            .to_string()
    })?;
    if home.trim().is_empty() {
        return Err("HOME is empty, so there is nowhere safe to keep the access secret".into());
    }
    Ok(PathBuf::from(home).join(".claudectl").join("access"))
}

/// Whether a grant id is safe to interpolate into a filename.
///
/// Grant ids arrive from argv (`access revoke <id>`) and from presented tokens,
/// so this runs before any path join. Mirrors `relay::is_valid_peer_id` but
/// also rejects `..` outright.
pub fn is_valid_grant_id(grant_id: &str) -> bool {
    !grant_id.is_empty()
        && grant_id.len() <= MAX_GRANT_ID_LEN
        && !grant_id.contains("..")
        && grant_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Mint a fresh grant id. 24 bits of randomness, so callers must handle a
/// collision — `GrantStore::create` refuses to overwrite.
pub fn gen_grant_id() -> String {
    format!("gr_{}", crate::relay::crypto::random_hex(3))
}

pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// What a caller is told when access is refused.
///
/// Deliberately opaque and carries no detail. RFC §3.3 requires a missing
/// scope to look like `404`, not `403`, so that an unauthorized caller cannot
/// enumerate which projects exist; the same reasoning applies to telling
/// "no such grant" apart from "revoked" or "expired".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Returned by `GrantStore::verify`; see the note there.
pub enum AccessError {
    Denied,
}

impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "denied")
    }
}

impl std::error::Error for AccessError {}

/// Why a verification actually failed. Goes to the audit log only — never to
/// the caller. This is the owner's diagnostic, not the third party's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    MalformedToken,
    UnknownGrant,
    UnreadableGrant,
    BadMac,
    Revoked,
    Expired,
    MissingScope,
    /// The grant's per-minute token bucket was empty (#431, RFC §4.8).
    ///
    /// Unlike every reason above, this one is written by the *caller* rather
    /// than by `verify` — the limit is checked after verification, so that a
    /// bucket is only ever created for a grant id that has already proved it
    /// holds a valid token.
    RateLimited,
    /// The grant's `daily_query_budget` was spent for the current UTC day.
    BudgetExhausted,
    /// A `get_doc` named a path the index does not carry.
    ///
    /// The caller is told the same opaque `404` a missing scope gets — what
    /// this reason exists for is the *owner's* side: without it a holder
    /// enumerating doc paths spent the budget while `access audit` showed
    /// nothing, so `access list` and `access audit` disagreed and neither
    /// could be reconciled with the other.
    NotIndexed,
    /// Classification judged the query to be seeking secrets (#430, §4.3).
    ///
    /// These four, like `RateLimited` and `NotIndexed`, are written by the
    /// caller rather than by `verify` — classification runs after a token has
    /// already verified, which is what keeps §4.4's "Jev never sees a query it
    /// has no business seeing" true.
    SeeksSensitive,
    /// Classification judged the query to be an instruction-override attempt.
    /// The only reason that also flags the grant.
    InjectionAttempt,
    /// Classification judged the query to be about another codebase.
    WrongProject,
    /// Classification judged the query not to be about this project's code or
    /// practices at all.
    OutOfScope,
    /// An escalation could not be appended to the owner's queue.
    ///
    /// The budget has already been charged by the time the queue is written,
    /// so without a line here an unwritable queue moved the counter and left
    /// no record — the same `access list` / `access audit` disagreement
    /// `NotIndexed` exists to prevent.
    QueueUnwritable,
}

impl DenyReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            DenyReason::MalformedToken => "malformed_token",
            DenyReason::UnknownGrant => "unknown_grant",
            DenyReason::UnreadableGrant => "unreadable_grant",
            DenyReason::BadMac => "bad_mac",
            DenyReason::Revoked => "revoked",
            DenyReason::Expired => "expired",
            DenyReason::MissingScope => "missing_scope",
            DenyReason::RateLimited => "rate_limited",
            DenyReason::BudgetExhausted => "budget_exhausted",
            DenyReason::NotIndexed => "not_indexed",
            DenyReason::SeeksSensitive => "seeks_sensitive",
            DenyReason::InjectionAttempt => "injection_attempt",
            DenyReason::WrongProject => "wrong_project",
            DenyReason::OutOfScope => "out_of_scope",
            DenyReason::QueueUnwritable => "queue_unwritable",
        }
    }
}

/// Milliseconds in a UTC day, for the daily-budget rollover.
pub const MS_PER_DAY: u64 = 86_400_000;

/// Which UTC day `now_ms` falls in.
///
/// Days rather than a rolling 24-hour window: "500 queries a day" is what an
/// owner setting `daily_query_budget` means, and a rolling window would need
/// per-request timestamps rather than one counter.
pub fn utc_day(now_ms: u64) -> u64 {
    now_ms / MS_PER_DAY
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_grant_ids_are_valid_and_shaped_like_the_rfc() {
        let id = gen_grant_id();
        assert!(id.starts_with("gr_"));
        // `gr_` + 3 bytes hex-encoded = 3 + 6.
        assert_eq!(id.len(), 9, "got {id}");
        assert!(is_valid_grant_id(&id));
    }

    #[test]
    fn generated_grant_ids_differ() {
        let a = gen_grant_id();
        let b = gen_grant_id();
        assert_ne!(a, b, "24 bits should not collide on two draws");
    }

    #[test]
    fn grant_id_validation_rejects_path_traversal_and_junk() {
        for bad in [
            "",
            "..",
            "../../etc/passwd",
            "gr_../x",
            "gr/1",
            "gr 1",
            "gr\n1",
            "gr.1",
            "gr:1",
        ] {
            assert!(!is_valid_grant_id(bad), "{bad:?} must be rejected");
        }
        assert!(!is_valid_grant_id(&"a".repeat(MAX_GRANT_ID_LEN + 1)));
    }

    #[test]
    fn grant_id_validation_accepts_the_real_shape() {
        for ok in ["gr_7f2a1b", "gr-7f2a1b", "GR_7F2A1B", "a"] {
            assert!(is_valid_grant_id(ok), "{ok} should be accepted");
        }
    }

    #[test]
    fn deny_reasons_are_stable_strings() {
        // These land in audit.jsonl, so they are a format other tools read.
        assert_eq!(DenyReason::BadMac.as_str(), "bad_mac");
        assert_eq!(DenyReason::MissingScope.as_str(), "missing_scope");
        assert_eq!(DenyReason::UnknownGrant.as_str(), "unknown_grant");
    }

    #[test]
    fn access_error_carries_no_detail() {
        // If this ever gains a variant or a payload, the 404-not-403 property
        // in RFC §3.3 needs re-checking at every call site.
        assert_eq!(AccessError::Denied.to_string(), "denied");
    }
}

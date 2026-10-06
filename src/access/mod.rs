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
//! ```
//!
//! This module is the spine: mint, verify, list, audit, revoke. It opens no
//! port of its own — the surface that presents these credentials is
//! `src/query/` (#429), which injects a `GrantStore` and secret rather than
//! opening the default store per request. #431 adds enforcement of the
//! `rate_limit_per_min` / `daily_query_budget` fields this module already
//! persists; both are written and neither is checked.
//!
//! Gated behind the `relay` feature because the MAC comes from
//! `relay::crypto`, which keeps the "no new dependency, no JWT library, no
//! asymmetric crypto" property. The minimal `--no-default-features --features
//! hive` build simply has no access surface.

pub mod cli;
pub mod grant;
pub mod scope;
pub mod token;

use std::path::PathBuf;

pub use grant::{Grant, GrantStore, new_grant};
pub use scope::Scope;
// `grant::AuditEntry` stays reachable by its full path rather than re-exported
// here — `query::core` reaches it by full path, which is the only consumer.

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
        }
    }
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

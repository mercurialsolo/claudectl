//! Grant tokens: the server secret, the MAC, and token mint/parse (#427, RFC §3.2).
//!
//! A token is `cctl_<grant_id>_<mac>`, where the MAC is
//! `HMAC-SHA256(server_secret, canonical(grant_id, scopes, expires_ms))`
//! truncated to 128 bits and hex-encoded. `relay::crypto` already has SHA-256
//! and HMAC-SHA256 inline, so there is no JWT library and no asymmetric crypto
//! here.
//!
//! **What the MAC covers, and why it matters.** `scopes` and `expires_ms` are
//! signed; `revoked`, `last_used_ms` and `use_count` are not. So revoking is a
//! one-field write that takes effect immediately with nothing to restart,
//! while widening a grant's scopes or pushing out its expiry *invalidates the
//! token* and forces a re-grant. That asymmetry is the design, not an
//! oversight: accounting is mutable, authority is frozen.
//!
//! Note the verification order. RFC §3.2 says "parse, recompute the MAC,
//! constant-time compare, then load the grant file", which cannot work — the
//! MAC covers `scopes` and `expires_ms`, which exist only in the file. The
//! real order is parse → load → recompute → compare → check revoked/expiry.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::relay::crypto;

use super::scope::Scope;

/// Token prefix, mirroring the `cctl://` invite-link namespace.
const TOKEN_PREFIX: &str = "cctl_";

/// MAC length in bytes before hex-encoding. 128 bits is plenty for a bearer
/// credential that is also bounded by expiry, revocation and a rate limit.
const MAC_BYTES: usize = 16;

/// Hex chars in a well-formed MAC.
const MAC_HEX_LEN: usize = MAC_BYTES * 2;

/// Filename of the HMAC key inside the access dir.
const SECRET_FILE: &str = "secret";

/// The canonical byte string the MAC is computed over.
///
/// Scopes are sorted and joined with `\n`, and the three fields are themselves
/// `\n`-separated. Sorting makes the MAC independent of the order scopes were
/// typed in; the separator is safe because `scope::validate_qualifier` rejects
/// whitespace and `\n`. Naive concatenation would collide — `["a", "bc"]` and
/// `["ab", "c"]` would sign identically.
pub fn canonical_payload(grant_id: &str, scopes: &[Scope], expires_ms: u64) -> String {
    let mut rendered: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    rendered.sort();
    format!("{grant_id}\n{}\n{expires_ms}", rendered.join("\n"))
}

/// Compute the hex MAC for a grant's signed fields.
pub fn compute_mac(secret: &[u8; 32], grant_id: &str, scopes: &[Scope], expires_ms: u64) -> String {
    let payload = canonical_payload(grant_id, scopes, expires_ms);
    let full = crypto::hmac_sha256(secret, payload.as_bytes());
    crypto::hex_encode(&full[..MAC_BYTES])
}

/// Assemble the token handed to the third party. Shown once.
pub fn mint(secret: &[u8; 32], grant_id: &str, scopes: &[Scope], expires_ms: u64) -> String {
    let mac = compute_mac(secret, grant_id, scopes, expires_ms);
    format!("{TOKEN_PREFIX}{grant_id}_{mac}")
}

/// The two halves of a presented token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToken {
    pub grant_id: String,
    pub mac: String,
}

/// Split `cctl_<grant_id>_<mac>` without assuming the grant id is underscore-free.
///
/// Grant ids look like `gr_7f2a1b`, so they *do* contain an underscore and
/// `split('_')` would mis-slice. Taking the MAC off the right end with
/// `rsplit_once` is what makes the format unambiguous.
pub fn parse(token: &str) -> Result<ParsedToken, String> {
    let token = token.trim();
    let body = token
        .strip_prefix(TOKEN_PREFIX)
        .ok_or_else(|| format!("token must start with '{TOKEN_PREFIX}'"))?;
    let (grant_id, mac) = body
        .rsplit_once('_')
        .ok_or_else(|| "token is missing its MAC segment".to_string())?;

    if grant_id.is_empty() {
        return Err("token has an empty grant id".into());
    }
    if !super::is_valid_grant_id(grant_id) {
        return Err(format!("token has a malformed grant id '{grant_id}'"));
    }
    if mac.len() != MAC_HEX_LEN
        || !mac
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    {
        return Err(format!(
            "token MAC must be {MAC_HEX_LEN} lowercase hex chars, got {}",
            mac.len()
        ));
    }

    Ok(ParsedToken {
        grant_id: grant_id.to_string(),
        mac: mac.to_string(),
    })
}

/// Constant-time check of a presented MAC against the recomputed one (#426).
pub fn mac_matches(presented: &str, expected: &str) -> bool {
    crypto::ct_eq(presented.as_bytes(), expected.as_bytes())
}

// ────────────────────────────────────────────────────────────────────────────
// Server secret
// ────────────────────────────────────────────────────────────────────────────

/// Load the HMAC key, creating it on first use.
///
/// SECURITY NOTE: this uses `crypto::try_generate_psk`, which fails rather
/// than falling back to a timestamp+pid seed. That fallback is defensible for
/// a short-lived LAN pairing code; it is not defensible for the root key that
/// every third-party-facing grant MAC derives from. A predictable key here
/// would let an attacker mint their own tokens.
pub fn load_or_create_secret(access_dir: &Path) -> Result<[u8; 32], String> {
    let path = secret_path(access_dir);

    // Only a genuinely absent secret may be minted over. Any other read
    // failure — EACCES, EIO, a directory sitting at that path — must propagate,
    // because falling through to the mint below would `rename` a fresh key over
    // a secret that was merely unreadable and kill every live grant silently.
    match fs::read_to_string(&path) {
        Ok(hex) => {
            let bytes = crypto::hex_decode(hex.trim())
                .map_err(|e| format!("access secret at {} is corrupt: {e}", path.display()))?;
            if bytes.len() != 32 {
                return Err(format!(
                    "access secret at {} is {} bytes, expected 32 — move it aside \
                     to mint a new one (every existing token stops verifying)",
                    path.display(),
                    bytes.len()
                ));
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            return Ok(out);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "cannot read the access secret at {}: {e} — refusing to mint a \
                 new one, which would invalidate every live grant",
                path.display()
            ));
        }
    }

    let secret = crypto::try_generate_psk().map_err(|e| {
        // `try_generate_psk` reads /dev/urandom, so this is also the path a
        // non-unix host takes. Failing closed is right — a predictable root key
        // would let anyone mint tokens — but say why rather than surfacing a
        // bare ENOENT for a file the operator has never heard of.
        if cfg!(unix) {
            format!("cannot generate an access secret without a secure RNG: {e}")
        } else {
            format!(
                "capability grants need /dev/urandom for the HMAC key, which \
                 this platform does not provide ({e}); `claudectl access` is \
                 unix-only for now"
            )
        }
    })?;
    write_secret(&path, &secret)?;
    Ok(secret)
}

pub fn secret_path(access_dir: &Path) -> PathBuf {
    access_dir.join(SECRET_FILE)
}

/// Write the secret 0600, atomically, never leaving a readable window.
///
/// The existing `save_peer_psk` writes content first and chmods after, which
/// leaves the secret world-readable for an instant. Here the mode is part of
/// the `open(2)` call, so the temp file never exists at a wider mode at all,
/// and the rename then publishes a file that was never readable. Setting the
/// mode after `File::create` would not be enough: a chmod cannot revoke a
/// descriptor another process already holds.
fn write_secret(path: &Path, secret: &[u8; 32]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("access secret path {} has no parent", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("create access dir: {e}"))?;

    let tmp = dir.join(".secret.tmp");
    {
        // The mode goes in the open(2) call, not a chmod after it. `File::create`
        // opens 0666 & ~umask — 0644 typically — and a later chmod does not
        // revoke a descriptor another process already opened in that window, so
        // chmod-after would hand the root HMAC key to anyone watching.
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| format!("create temp secret: {e}"))?;

        f.write_all(crypto::hex_encode(secret).as_bytes())
            .map_err(|e| format!("write temp secret: {e}"))?;
        f.sync_data().map_err(|e| format!("sync secret: {e}"))?;
    }

    fs::rename(&tmp, path).map_err(|e| format!("publish secret: {e}"))?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(items: &[&str]) -> Vec<Scope> {
        items.iter().map(|s| s.parse().unwrap()).collect()
    }

    const SECRET: [u8; 32] = [7u8; 32];

    #[test]
    fn mint_and_parse_round_trip() {
        let s = scopes(&["project.query:claudectl"]);
        let token = mint(&SECRET, "gr_7f2a1b", &s, 1_793_802_482_180);
        assert!(token.starts_with("cctl_gr_7f2a1b_"));

        let parsed = parse(&token).unwrap();
        assert_eq!(parsed.grant_id, "gr_7f2a1b");
        assert_eq!(parsed.mac.len(), MAC_HEX_LEN);
        assert_eq!(
            parsed.mac,
            compute_mac(&SECRET, "gr_7f2a1b", &s, 1_793_802_482_180)
        );
    }

    #[test]
    fn parse_keeps_the_underscore_in_the_grant_id() {
        // `split('_')` would slice this wrong — the id itself has an underscore.
        let token = mint(&SECRET, "gr_7f2a1b", &scopes(&["project.docs:p"]), 1);
        assert_eq!(parse(&token).unwrap().grant_id, "gr_7f2a1b");
    }

    #[test]
    fn mac_is_independent_of_scope_order() {
        let a = scopes(&["project.query:p", "project.docs:p"]);
        let b = scopes(&["project.docs:p", "project.query:p"]);
        assert_eq!(
            compute_mac(&SECRET, "gr_1", &a, 100),
            compute_mac(&SECRET, "gr_1", &b, 100),
            "typing scopes in a different order must not change the token"
        );
    }

    #[test]
    fn canonical_payload_does_not_collide_on_concatenation() {
        // The classic naive-concat collision: ["a","bc"] vs ["ab","c"].
        // Our qualifiers can't contain the delimiter, so distinct scope sets
        // must always produce distinct payloads.
        let one = canonical_payload("gr_1", &scopes(&["project.query:a"]), 1);
        let two = canonical_payload("gr_1", &scopes(&["project.query:ab"]), 1);
        assert_ne!(one, two);

        // Field boundaries are unambiguous too: a grant id ending in a digit
        // must not blur into the expiry.
        assert_ne!(
            canonical_payload("gr_1", &scopes(&["project.query:a"]), 23),
            canonical_payload("gr_12", &scopes(&["project.query:a"]), 3),
        );
    }

    #[test]
    fn mac_changes_when_any_signed_field_changes() {
        let base = compute_mac(&SECRET, "gr_1", &scopes(&["project.query:p"]), 100);
        // Different scope set.
        assert_ne!(
            base,
            compute_mac(&SECRET, "gr_1", &scopes(&["project.docs:p"]), 100)
        );
        // Widened scope set.
        assert_ne!(
            base,
            compute_mac(
                &SECRET,
                "gr_1",
                &scopes(&["project.query:p", "project.docs:p"]),
                100
            )
        );
        // Different qualifier.
        assert_ne!(
            base,
            compute_mac(&SECRET, "gr_1", &scopes(&["project.query:other"]), 100)
        );
        // Pushed-out expiry.
        assert_ne!(
            base,
            compute_mac(&SECRET, "gr_1", &scopes(&["project.query:p"]), 200)
        );
        // Different grant id.
        assert_ne!(
            base,
            compute_mac(&SECRET, "gr_2", &scopes(&["project.query:p"]), 100)
        );
    }

    #[test]
    fn mac_changes_with_the_secret() {
        let s = scopes(&["project.query:p"]);
        assert_ne!(
            compute_mac(&SECRET, "gr_1", &s, 100),
            compute_mac(&[8u8; 32], "gr_1", &s, 100),
        );
    }

    #[test]
    fn parse_rejects_malformed_tokens() {
        // Wrong prefix.
        assert!(parse("ccl_gr_1_abcdef").is_err());
        assert!(parse("gr_1_abcdef").is_err());
        // No MAC segment at all.
        assert!(parse("cctl_gr1").is_err());
        // MAC wrong length.
        assert!(parse("cctl_gr_1_abc").is_err());
        assert!(parse(&format!("cctl_gr_1_{}", "a".repeat(MAC_HEX_LEN + 1))).is_err());
        // Uppercase hex — we only ever emit lowercase.
        assert!(parse(&format!("cctl_gr_1_{}", "A".repeat(MAC_HEX_LEN))).is_err());
        // Non-hex.
        assert!(parse(&format!("cctl_gr_1_{}", "z".repeat(MAC_HEX_LEN))).is_err());
        // Empty grant id.
        assert!(parse(&format!("cctl__{}", "a".repeat(MAC_HEX_LEN))).is_err());
    }

    #[test]
    fn parse_rejects_a_grant_id_that_could_escape_the_grants_dir() {
        let mac = "a".repeat(MAC_HEX_LEN);
        assert!(parse(&format!("cctl_../../etc/passwd_{mac}")).is_err());
        assert!(parse(&format!("cctl_.._{mac}")).is_err());
    }

    #[test]
    fn parse_tolerates_surrounding_whitespace() {
        let token = mint(&SECRET, "gr_7f2a1b", &scopes(&["project.query:p"]), 1);
        let padded = format!("  {token}\n");
        assert_eq!(parse(&padded).unwrap(), parse(&token).unwrap());
    }

    #[test]
    fn mac_matches_is_exact() {
        let m = compute_mac(&SECRET, "gr_1", &scopes(&["project.query:p"]), 1);
        assert!(mac_matches(&m, &m));
        let mut wrong = m.clone();
        wrong.replace_range(0..1, if m.starts_with('0') { "1" } else { "0" });
        assert!(!mac_matches(&wrong, &m));
        // Prefix must not pass.
        assert!(!mac_matches(&m[..m.len() - 1], &m));
    }

    #[test]
    fn secret_is_created_once_and_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_secret(dir.path()).unwrap();
        let second = load_or_create_secret(dir.path()).unwrap();
        assert_eq!(first, second, "a second call must not re-mint the key");
        assert!(secret_path(dir.path()).exists());
    }

    #[test]
    fn secret_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        load_or_create_secret(dir.path()).unwrap();
        assert!(!dir.path().join(".secret.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn nothing_in_the_access_dir_is_ever_group_or_world_readable() {
        // The mode has to be part of open(2). A chmod after `File::create`
        // leaves a window at 0644, and chmod cannot revoke a descriptor
        // another process already opened during it.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load_or_create_secret(dir.path()).unwrap();

        for entry in fs::read_dir(dir.path()).unwrap().flatten() {
            if entry.metadata().unwrap().is_dir() {
                continue;
            }
            let mode = entry.metadata().unwrap().permissions().mode() & 0o077;
            assert_eq!(
                mode,
                0,
                "{} grants group/other access",
                entry.path().display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn secret_is_owner_only_on_disk() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load_or_create_secret(dir.path()).unwrap();
        let mode = fs::metadata(secret_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    #[test]
    fn corrupt_secret_is_an_error_not_a_silent_re_mint() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(secret_path(dir.path()), "not hex at all").unwrap();
        // Silently minting a fresh key would invalidate every live grant
        // without saying so.
        assert!(load_or_create_secret(dir.path()).is_err());
    }

    #[test]
    fn an_unreadable_secret_is_an_error_not_a_silent_re_mint() {
        // A directory at the secret's path reads as something other than
        // NotFound on every platform. The old code fell through on any read
        // error and renamed a fresh key over the existing secret, killing
        // every live grant without a word.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(secret_path(dir.path())).unwrap();
        let err = load_or_create_secret(dir.path()).unwrap_err();
        assert!(err.contains("refusing to mint"), "got {err}");
    }

    #[test]
    fn short_secret_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(secret_path(dir.path()), "abcdef").unwrap();
        let err = load_or_create_secret(dir.path()).unwrap_err();
        assert!(err.contains("expected 32"), "got {err}");
    }
}

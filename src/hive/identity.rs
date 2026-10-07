//! Hive identity — a name, a description and a join policy (#432, RFC §7.1–7.2).
//!
//! Before this, a hive had no identity at all: it was the transitive closure of
//! your relay peers, so you could not name one, advertise one, or join one *as
//! such*. Naming is therefore a prerequisite for the rest of #424 rather than a
//! nicety — you cannot advertise what has no name.
//!
//! # Absent is the default, and it is not an error
//!
//! [`load`] returns `Option<HiveIdentity>`, and `None` means "this machine's
//! hive is unnamed". That is the acceptance criterion made structural: every
//! future consumer — #433's LAN advertiser, #434's invite links — writes
//! `if let Some(id) = load()?`, so the no-name path is the code that already
//! shipped, untouched. A user who never runs `hive identity set` sees no
//! behavioural change whatsoever.
//!
//! A *malformed* file is a different thing and is an error, not a `None`. An
//! identity that silently reverted to unnamed because a byte got mangled would
//! stop advertising without saying so.
//!
//! # Why the name is validated to the scope grammar
//!
//! #435 will mint `hive.read:<name>` capability grants, whose qualifier is
//! `[A-Za-z0-9._-]`. A hive called `barry's hive` could be named here and then
//! never granted against, so the same grammar is enforced now — #435 then
//! inherits no names it cannot express, which is cheaper than discovering it
//! later and having to migrate people's hive names.
//!
//! The check is **spelled out here rather than calling
//! `access::scope::validate_qualifier`**, because `src/access/` is gated behind
//! the `relay` feature and this module is not: the sync-only
//! `--no-default-features --features hive` build has a hive but no capability
//! grants. A `relay`-gated test asserts the two agree on a corpus of names, so
//! the duplication cannot drift silently.

use std::fs;
use std::path::PathBuf;

/// How a peer is allowed to join this hive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinPolicy {
    /// A link or code is required. The default, and the only one that needs no
    /// further thought.
    Invite,
    /// The owner approves each request individually.
    Ask,
    /// Any discoverer may join. LAN only, and gated — see
    /// [`HiveIdentity::open_is_acknowledged`].
    Open,
}

impl JoinPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Invite => "invite",
            Self::Ask => "ask",
            Self::Open => "open",
        }
    }

    /// The warning an owner must see before choosing this policy.
    ///
    /// `None` for the policies that need no warning, so a caller can branch on
    /// "does this require consent" without matching the variants again.
    pub fn warning(&self) -> Option<&'static str> {
        match self {
            Self::Invite | Self::Ask => None,
            Self::Open => Some(
                "join_policy = open lets ANY machine that can see your LAN broadcast \
                 join this hive without approval. Joining a hive means receiving your \
                 distilled preferences and insights — the patterns the brain learned \
                 from how you work. On a shared or untrusted network (an office, a \
                 cafe, a conference) that is everyone on it.",
            ),
        }
    }
}

/// This machine's hive identity.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HiveIdentity {
    pub hive_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub join_policy: JoinPolicy,
    pub created_ms: u64,
    /// When the owner explicitly consented to `join_policy: open`.
    ///
    /// The policy field alone is not enough to advertise `open`. `hive identity
    /// set --join-policy open` warns and requires confirmation, and records the
    /// consent here; a hand-edited `identity.json` that says `"open"` without
    /// this has not been consented to and **#433 must not broadcast it as
    /// open**. See [`Self::open_is_acknowledged`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_acknowledged_ms: Option<u64>,
}

impl HiveIdentity {
    /// Whether `open` may actually be advertised.
    ///
    /// True for every policy that is not `open`; for `open`, true only when the
    /// owner's consent was recorded. A consumer that advertises the policy
    /// should treat `false` as "fall back to `invite` and say why" rather than
    /// as an error — the hive is still perfectly usable, just not open.
    pub fn open_is_acknowledged(&self) -> bool {
        self.join_policy != JoinPolicy::Open || self.open_acknowledged_ms.is_some()
    }

    /// The policy to actually act on, which is `invite` for an unacknowledged
    /// `open`. Fail closed: a policy nobody consented to must not be the
    /// permissive one.
    pub fn effective_join_policy(&self) -> JoinPolicy {
        if self.open_is_acknowledged() {
            self.join_policy
        } else {
            JoinPolicy::Invite
        }
    }
}

/// Mint a hive id. 24 bits, like a grant id — this names a hive, it is not a
/// secret, and a collision between two unrelated machines costs a display
/// ambiguity rather than any access.
pub fn gen_hive_id() -> String {
    // Derived from the clock rather than `relay::crypto::random_hex`, which
    // lives behind the `relay` feature this module deliberately does not need.
    // A hive id names a hive; nothing authenticates against it, so
    // unpredictability buys nothing here.
    let mixed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) ^ d.as_secs())
        .unwrap_or_default();
    format!("hv_{:06x}", mixed & 0xff_ffff)
}

pub fn hive_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claudectl").join("hive")
}

/// `~/.claudectl/hive/identity.json`.
pub fn identity_path() -> PathBuf {
    hive_dir().join("identity.json")
}

/// Milliseconds since the epoch.
///
/// `hive::epoch_secs` is seconds and `access::epoch_ms` sits behind the `relay`
/// feature, so this module carries its own. §7.2's record is in milliseconds.
pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Validate a hive name.
///
/// Delegates to the capability-scope grammar so #435 can mint
/// `hive.read:<name>` against anything nameable here. Also caps the length: the
/// name rides #433's UDP datagram, which has an MTU to respect.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("a hive name must not be empty".into());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "a hive name must be at most {MAX_NAME_LEN} bytes (this one is {})",
            name.len()
        ));
    }
    if let Some(bad) = name.chars().find(|c| !is_name_char(*c)) {
        return Err(format!(
            "a hive name may only contain ASCII letters, digits, '-', '_' and '.' — \
             {bad:?} is not allowed. The name has to be usable as a capability scope \
             qualifier, because #435 grants `hive.read:<name>`."
        ));
    }
    Ok(())
}

/// The scope-qualifier character set, kept in step with
/// `access::scope::validate_qualifier` by a test rather than by a call.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

/// Longest acceptable hive name, and longest description.
///
/// Both ride #433's LAN datagram alongside the existing identity and version
/// fields. A UDP announcement that fragments is an announcement that gets
/// dropped, so the ceilings are here rather than discovered there.
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_DESCRIPTION_LEN: usize = 200;

/// Read this machine's hive identity.
///
/// `Ok(None)` means unnamed, which is the default and not a failure.
/// `Err` means there is a file and it could not be understood — never silently
/// downgraded to unnamed, because that would stop advertisement without saying
/// so.
pub fn load() -> Result<Option<HiveIdentity>, String> {
    load_from(&identity_path())
}

/// [`load`] against an explicit path, for tests.
pub fn load_from(path: &PathBuf) -> Result<Option<HiveIdentity>, String> {
    let body = match fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let id: HiveIdentity = serde_json::from_str(&body).map_err(|e| {
        format!(
            "{} is not a readable hive identity ({e}). Fix or delete it — \
             claudectl will not fall back to 'unnamed', because that would \
             silently stop advertising.",
            path.display()
        )
    })?;
    validate_name(&id.name)
        .map_err(|e| format!("{} holds an unusable name: {e}", path.display()))?;
    Ok(Some(id))
}

/// Write the identity atomically (temp file + rename, 0600).
///
/// Atomic for the same reason `fleet.json` and the knowledge store are: #433's
/// advertiser will read this on a tick, and a reader must never see a half-file.
pub fn save(identity: &HiveIdentity) -> Result<(), String> {
    save_to(&identity_path(), identity)
}

/// [`save`] against an explicit path, for tests.
pub fn save_to(path: &PathBuf, identity: &HiveIdentity) -> Result<(), String> {
    validate_name(&identity.name)?;
    let dir = path
        .parent()
        .ok_or_else(|| "identity path has no parent".to_string())?;
    fs::create_dir_all(dir).map_err(|e| format!("create hive dir: {e}"))?;
    let body =
        serde_json::to_string_pretty(identity).map_err(|e| format!("encode hive identity: {e}"))?;
    let tmp = dir.join(".identity.json.tmp");
    fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    // 0600 before the rename, so the file is never briefly readable at the
    // final path with a wider mode. A hive name is not a secret, but the
    // description can name private work.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).map_err(|e| format!("rename into {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "claudectl-hiveid-{tag}-{}-{}",
            std::process::id(),
            epoch_ms()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample(name: &str) -> HiveIdentity {
        HiveIdentity {
            hive_id: "hv_3a9f21".into(),
            name: name.into(),
            description: Some("Rust CLI practices".into()),
            join_policy: JoinPolicy::Invite,
            created_ms: 1_791_210_482_180,
            open_acknowledged_ms: None,
        }
    }

    #[test]
    fn an_absent_identity_is_none_rather_than_an_error() {
        // The acceptance criterion: a hive with no name behaves exactly as
        // before, so "no file" must be a clean `None` on the happy path.
        let dir = tmpdir("absent");
        let p = dir.join("identity.json");
        assert_eq!(load_from(&p), Ok(None));
        assert!(!p.exists(), "reading created a file");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_malformed_identity_is_an_error_not_a_silent_unnamed() {
        // Downgrading to `None` here would stop advertisement without saying so.
        let dir = tmpdir("malformed");
        let p = dir.join("identity.json");
        fs::write(&p, b"{not json").unwrap();
        let got = load_from(&p);
        assert!(got.is_err(), "a corrupt identity read as {got:?}");
        assert!(
            got.unwrap_err().contains("will not fall back"),
            "the error should explain why it is not None"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn records_round_trip() {
        let dir = tmpdir("round");
        let p = dir.join("identity.json");
        let id = sample("barrys-hive");
        save_to(&p, &id).unwrap();
        assert_eq!(load_from(&p), Ok(Some(id)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_identity_file_is_owner_only() {
        let dir = tmpdir("mode");
        let p = dir.join("identity.json");
        save_to(&p, &sample("barrys-hive")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{mode:o}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_name_must_be_usable_as_a_scope_qualifier() {
        // #435 mints `hive.read:<name>`, so anything nameable here must be
        // grantable there.
        for ok in ["barrys-hive", "hive.one", "Hive_2", "a"] {
            assert!(validate_name(ok).is_ok(), "{ok} was refused");
        }
        for bad in [
            "",
            "   ",
            "barry's hive",
            "has space",
            "slash/es",
            "colon:s",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(validate_name(&"x".repeat(MAX_NAME_LEN)).is_ok());
        assert!(validate_name(&"x".repeat(MAX_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn a_name_that_cannot_round_trip_is_refused_on_write_too() {
        // Not only on the CLI path — anything reaching `save` is validated, so a
        // programmatic caller cannot persist a name `load` would then reject.
        let dir = tmpdir("writeval");
        let p = dir.join("identity.json");
        let mut bad = sample("fine");
        bad.name = "not fine".into();
        assert!(save_to(&p, &bad).is_err());
        assert!(!p.exists(), "an invalid identity was written anyway");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unacknowledged_open_policy_falls_back_to_invite() {
        // A hand-edited file saying "open" has had no confirmation, so it must
        // not be treated as open. Fail closed.
        let mut id = sample("barrys-hive");
        id.join_policy = JoinPolicy::Open;
        id.open_acknowledged_ms = None;
        assert!(!id.open_is_acknowledged());
        assert_eq!(id.effective_join_policy(), JoinPolicy::Invite);

        id.open_acknowledged_ms = Some(1_791_210_482_190);
        assert!(id.open_is_acknowledged());
        assert_eq!(id.effective_join_policy(), JoinPolicy::Open);
    }

    #[test]
    fn the_cautious_policies_need_no_acknowledgement() {
        for p in [JoinPolicy::Invite, JoinPolicy::Ask] {
            let mut id = sample("barrys-hive");
            id.join_policy = p;
            id.open_acknowledged_ms = None;
            assert!(id.open_is_acknowledged(), "{p:?} demanded consent");
            assert_eq!(id.effective_join_policy(), p);
            assert!(p.warning().is_none(), "{p:?} warns for no reason");
        }
        assert!(JoinPolicy::Open.warning().is_some());
    }

    #[test]
    fn policies_serialize_as_the_spec_spells_them() {
        // §7.2's JSON is a published shape, and #433 puts it on the wire.
        let dir = tmpdir("wire");
        let p = dir.join("identity.json");
        let mut id = sample("barrys-hive");
        id.join_policy = JoinPolicy::Ask;
        save_to(&p, &id).unwrap();
        let raw = fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"join_policy\": \"ask\""), "{raw}");
        // An absent acknowledgement is omitted, not written as null.
        assert!(!raw.contains("open_acknowledged_ms"), "{raw}");
        fs::remove_dir_all(&dir).ok();
    }

    /// The duplication guard.
    ///
    /// `validate_name` spells out the scope-qualifier grammar instead of calling
    /// `access::scope::validate_qualifier`, because `src/access/` is behind the
    /// `relay` feature and this module is not. Where both exist, they must agree
    /// exactly — otherwise #435 inherits a hive name it cannot mint a grant for,
    /// or refuses one it could have.
    #[cfg(feature = "relay")]
    #[test]
    fn the_name_grammar_agrees_with_the_real_scope_qualifier() {
        let corpus = [
            "barrys-hive",
            "hive.one",
            "Hive_2",
            "a",
            "A1._-",
            "barry's hive",
            "has space",
            "slash/es",
            "colon:s",
            "emoji\u{1f41d}",
            "tab\there",
            "plus+sign",
            "at@sign",
            "tilde~",
        ];
        for name in corpus {
            let mine = validate_name(name).is_ok();
            let theirs = crate::access::scope::validate_qualifier(name).is_ok();
            assert_eq!(
                mine, theirs,
                "{name:?}: hive says {mine}, access::scope says {theirs} — the two \
                 grammars have drifted"
            );
        }
    }

    #[test]
    fn an_id_is_prefixed_and_twenty_four_bits() {
        let id = gen_hive_id();
        assert!(id.starts_with("hv_"), "{id}");
        assert_eq!(id.len(), 3 + 6);
        assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()), "{id}");
    }
}

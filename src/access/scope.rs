//! Capability scopes — `<resource>.<verb>:<qualifier>` (#427, RFC §3.3).
//!
//! Every verb here was read-only until #537, and the read-only property is the
//! point of the access model: a grant lets someone look, never touch.
//!
//! #537 needs one verb that is **not** read-only — `credential.use`, which asks
//! the *owner's* machine to perform an authenticated step on a delegated task's
//! behalf, so a worker can reach a credential that never leaves the owner.
//! The issue asks for the grammar to be "extended, not loosened", so rather
//! than letting a blanket claim quietly become false:
//!
//! - [`Scope::is_read_only`] states the property per scope, so it is checkable
//!   instead of implied by a comment;
//! - the read-only surfaces assert it. `query::core` derives the scope it
//!   requires from the project it serves, so a `credential.use` grant could
//!   never have satisfied it — but the assertion makes that a property of the
//!   code rather than a consequence of how the derivation happens to be
//!   written today.
//!
//! A grant carrying `credential.use` therefore cannot read a project, and a
//! grant carrying `project.query` cannot spend a credential, whatever else a
//! future surface does.
//!
//! The qualifier grammar is deliberately narrow because scopes are signed into
//! the grant MAC (see `token.rs`). The canonical MAC input joins scopes with
//! `\n`, so a qualifier containing whitespace, `\n` or `:` would make that
//! encoding ambiguous and two different scope sets could produce the same MAC.
//! `validate_qualifier` is what keeps the delimiter unambiguous.

use std::fmt;

/// Maximum qualifier length. A project or hive name, not a sentence.
const MAX_QUALIFIER_LEN: usize = 128;

/// A single capability, as stored in a grant and signed into its MAC.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Ask natural-language questions about a project (RFC §4).
    ProjectQuery(String),
    /// Retrieve published doc spans from a project verbatim.
    ProjectDocs(String),
    /// Session status / cost / burn for one project.
    ///
    /// Defined by the RFC but issued to nobody by default — see open question
    /// Q8. `access grant` refuses it; the variant exists so the surface in #429
    /// has something to match against if that decision changes.
    FleetRead(String),
    /// Read exposed knowledge units from a named hive. Needs #424.
    HiveRead(String),
    /// Mesh as a hive peer. Needs #424.
    HiveJoin(String),
    /// Ask the owner's machine to perform one configured credentialed step
    /// (#537). The qualifier is the **credential name** the owner configured,
    /// not a project — the secret itself never reaches the grant holder.
    ///
    /// The only non-read-only verb in the grammar. See the module docs.
    CredentialUse(String),
}

impl Scope {
    /// The `<resource>.<verb>` half, without the qualifier.
    pub fn verb(&self) -> &'static str {
        match self {
            Scope::ProjectQuery(_) => "project.query",
            Scope::ProjectDocs(_) => "project.docs",
            Scope::FleetRead(_) => "fleet.read",
            Scope::HiveRead(_) => "hive.read",
            Scope::HiveJoin(_) => "hive.join",
            Scope::CredentialUse(_) => "credential.use",
        }
    }

    /// Whether this scope can only observe.
    ///
    /// Stated per scope so the read-only surfaces can assert it, rather than
    /// resting on the module-wide claim that #537 made false. A new variant has
    /// to answer this question explicitly, which is the point: the compiler
    /// asks, instead of the reviewer having to notice.
    pub fn is_read_only(&self) -> bool {
        match self {
            Scope::ProjectQuery(_)
            | Scope::ProjectDocs(_)
            | Scope::FleetRead(_)
            | Scope::HiveRead(_)
            | Scope::HiveJoin(_) => true,
            // Spends the owner's credential. Observing is not what it does.
            Scope::CredentialUse(_) => false,
        }
    }

    /// The project or hive this scope is bound to.
    pub fn qualifier(&self) -> &str {
        match self {
            Scope::ProjectQuery(q)
            | Scope::ProjectDocs(q)
            | Scope::FleetRead(q)
            | Scope::HiveRead(q)
            | Scope::HiveJoin(q)
            | Scope::CredentialUse(q) => q,
        }
    }

    /// Why this scope cannot be issued yet, for the CLI's error message.
    ///
    /// `None` means `claudectl access grant` will mint it. `fleet.read` is Q8
    /// ("scope defined, issued to nobody"). `hive.read` became issuable in #435.
    /// Keeping `hive.join` parseable but unissuable means a grant file written by
    /// a later version still loads here.
    ///
    /// This is the single place that list lives. There was a companion
    /// `is_issuable` predicate carrying a second copy of it; #429 removed it
    /// when the module stopped carrying `#![allow(dead_code)]` and it turned
    /// out to have no caller.
    pub fn unissuable_reason(&self) -> Option<&'static str> {
        match self {
            Scope::ProjectQuery(_) | Scope::ProjectDocs(_) => None,
            Scope::FleetRead(_) => Some(
                "fleet.read is defined but issued to nobody by default \
                 (open-cluster RFC Q8) — a third party reviewing your project \
                 should not see your live session costs",
            ),
            // #435: issuable now that #432 gave hives a name to qualify on.
            // The qualifier is the hive *name*, so renaming a hive invalidates
            // outstanding reader grants — `hive identity set --name` warns when
            // any are still live.
            Scope::HiveRead(_) => None,
            Scope::HiveJoin(_) => Some(
                "hive.join is not issued as a grant — a hive is joined with an \
                 invite (`claudectl hive join`), and `hive.read` is the scope that \
                 grants membership without contribution",
            ),
            // Parseable now, mintable when the callback surface lands. Same
            // reasoning as `fleet.read`: a grant file written by a later
            // version still loads here, and a scope nothing honours yet must
            // not be issuable, or an owner could mint a capability that
            // silently does nothing.
            Scope::CredentialUse(_) => Some(
                "credential.use is defined but not issued yet (#537) — the \
                 callback surface that honours it is not shipped, so a grant \
                 carrying it would authorise nothing",
            ),
        }
    }

    /// Build a scope from a bare verb plus a qualifier supplied separately.
    ///
    /// This is the `--scopes project.query --project claudectl` path.
    pub fn from_verb_and_qualifier(verb: &str, qualifier: &str) -> Result<Self, String> {
        validate_qualifier(qualifier)?;
        let q = qualifier.to_string();
        match verb {
            "project.query" => Ok(Scope::ProjectQuery(q)),
            "project.docs" => Ok(Scope::ProjectDocs(q)),
            "fleet.read" => Ok(Scope::FleetRead(q)),
            "hive.read" => Ok(Scope::HiveRead(q)),
            "hive.join" => Ok(Scope::HiveJoin(q)),
            "credential.use" => Ok(Scope::CredentialUse(q)),
            other => Err(format!(
                "unknown scope '{other}' (expected one of: project.query, \
                 project.docs, fleet.read, hive.read, hive.join, credential.use)"
            )),
        }
    }

    /// Parse a fully-qualified `verb:qualifier` scope.
    ///
    /// `default_qualifier` fills in for a bare verb, so `--scopes` accepts both
    /// `project.query` (with `--project`) and `project.query:claudectl`
    /// through one code path.
    pub fn parse_with_default(text: &str, default_qualifier: Option<&str>) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("empty scope".into());
        }
        match text.split_once(':') {
            Some((verb, qualifier)) => {
                Scope::from_verb_and_qualifier(verb.trim(), qualifier.trim())
            }
            None => match default_qualifier {
                Some(q) => Scope::from_verb_and_qualifier(text, q),
                None => Err(format!(
                    "scope '{text}' has no qualifier — pass --project, or write \
                     it as '{text}:<project>'"
                )),
            },
        }
    }
}

/// Reject anything that would make the MAC's `\n`-joined encoding ambiguous,
/// or that could escape a directory when used in a path.
pub fn validate_qualifier(qualifier: &str) -> Result<(), String> {
    if qualifier.is_empty() {
        return Err("scope qualifier is empty".into());
    }
    if qualifier.len() > MAX_QUALIFIER_LEN {
        return Err(format!(
            "scope qualifier is {} chars, limit is {MAX_QUALIFIER_LEN}",
            qualifier.len()
        ));
    }
    if !qualifier
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!(
            "scope qualifier '{qualifier}' may only contain ASCII letters, \
             digits, '-', '_' and '.'"
        ));
    }
    // `..` would be a path-traversal gift to any future per-project file.
    if qualifier.contains("..") {
        return Err(format!(
            "scope qualifier '{qualifier}' may not contain '..'"
        ));
    }
    Ok(())
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.verb(), self.qualifier())
    }
}

impl std::str::FromStr for Scope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scope::parse_with_default(s, None)
    }
}

// Scopes live in the grant JSON as plain strings (RFC §3.2), so serde goes
// through Display / FromStr rather than deriving a tagged enum.
impl serde::Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_display_and_parse() {
        for text in [
            "project.query:claudectl",
            "project.docs:claudectl",
            "fleet.read:claudectl",
            "hive.read:team-hive",
            "hive.join:team-hive",
            "credential.use:npm-registry",
        ] {
            let scope: Scope = text.parse().expect("should parse");
            assert_eq!(scope.to_string(), text);
        }
    }

    /// The grammar's read-only claim, now that exactly one verb breaks it
    /// (#537). A new variant has to answer `is_read_only` explicitly, and this
    /// is where the answer is recorded — so adding a second acting verb is a
    /// deliberate edit here, not a silent one.
    #[test]
    fn only_credential_use_can_act() {
        let read_only = [
            Scope::ProjectQuery("p".into()),
            Scope::ProjectDocs("p".into()),
            Scope::FleetRead("p".into()),
            Scope::HiveRead("h".into()),
            Scope::HiveJoin("h".into()),
        ];
        for scope in &read_only {
            assert!(scope.is_read_only(), "{scope} should be read-only");
        }
        assert!(
            !Scope::CredentialUse("npm".into()).is_read_only(),
            "credential.use spends the owner's credential; it is not read-only"
        );
    }

    /// `query::core` asserts the scope it requires is read-only. That
    /// assertion can only hold if the scopes it builds — derived from the
    /// served project — are read-only, which is what this pins.
    #[test]
    fn the_scopes_the_query_surface_derives_are_read_only() {
        for scope in [
            Scope::ProjectQuery("served-project".into()),
            Scope::ProjectDocs("served-project".into()),
        ] {
            assert!(scope.is_read_only(), "{scope}");
        }
    }

    /// A scope nothing honours yet must not be mintable, or an owner could
    /// hand out a capability that authorises nothing. Same reasoning as
    /// `fleet.read`.
    #[test]
    fn credential_use_is_not_issuable_until_its_surface_ships() {
        let reason = Scope::CredentialUse("npm".into())
            .unissuable_reason()
            .expect("credential.use must not be mintable yet");
        assert!(reason.contains("#537"), "{reason}");
    }

    /// The qualifier is a credential *name*, so it goes through the same
    /// narrowing as every other qualifier — it reaches the MAC payload.
    #[test]
    fn a_credential_name_is_validated_like_any_qualifier() {
        assert!(Scope::from_verb_and_qualifier("credential.use", "npm-registry").is_ok());
        for bad in ["", "has space", "colon:inside", "../escape", "dot..dot"] {
            assert!(
                Scope::from_verb_and_qualifier("credential.use", bad).is_err(),
                "{bad:?} should be refused as a credential name"
            );
        }
    }

    #[test]
    fn bare_verb_takes_the_default_qualifier() {
        let scope = Scope::parse_with_default("project.query", Some("claudectl")).unwrap();
        assert_eq!(scope, Scope::ProjectQuery("claudectl".into()));
    }

    #[test]
    fn explicit_qualifier_wins_over_the_default() {
        let scope = Scope::parse_with_default("project.docs:other", Some("claudectl")).unwrap();
        assert_eq!(scope, Scope::ProjectDocs("other".into()));
    }

    #[test]
    fn bare_verb_without_a_default_is_an_error() {
        let err = Scope::parse_with_default("project.query", None).unwrap_err();
        assert!(err.contains("--project"), "got {err}");
    }

    #[test]
    fn unknown_verb_is_rejected_and_lists_the_valid_ones() {
        let err = Scope::parse_with_default("project.write:x", None).unwrap_err();
        assert!(err.contains("unknown scope"), "got {err}");
        assert!(err.contains("project.query"), "got {err}");
    }

    #[test]
    fn nothing_can_write_to_a_project_a_fleet_or_a_hive() {
        for text in [
            "project.write:x",
            "project.edit:x",
            "fleet.write:x",
            "hive.write:x",
        ] {
            assert!(text.parse::<Scope>().is_err(), "{text} must not parse");
        }
        // #537 added `credential.use`, the one verb that acts. It acts on the
        // *owner's* machine, through a step the owner configured — it is still
        // no way to write to a project, a fleet or a hive, which is what the
        // cases above pin.
        assert!("credential.use:npm".parse::<Scope>().is_ok());
        assert!(
            !"credential.use:npm"
                .parse::<Scope>()
                .unwrap()
                .is_read_only()
        );
    }

    #[test]
    fn issuable_scopes_are_query_docs_and_hive_read() {
        assert!(
            Scope::ProjectQuery("p".into())
                .unissuable_reason()
                .is_none()
        );
        assert!(Scope::ProjectDocs("p".into()).unissuable_reason().is_none());
        // #435 made this one mintable; #432 gave it a name to qualify on.
        assert!(Scope::HiveRead("h".into()).unissuable_reason().is_none());
        // Parseable so later-written grant files still load, but not mintable.
        assert!(Scope::FleetRead("p".into()).unissuable_reason().is_some());
        assert!(Scope::HiveJoin("h".into()).unissuable_reason().is_some());
        // #537: parseable, and mintable once the callback surface ships.
        assert!(
            Scope::CredentialUse("npm".into())
                .unissuable_reason()
                .is_some()
        );
    }

    #[test]
    fn unissuable_scopes_explain_themselves() {
        assert!(
            Scope::ProjectQuery("p".into())
                .unissuable_reason()
                .is_none()
        );
        assert!(
            Scope::FleetRead("p".into())
                .unissuable_reason()
                .unwrap()
                .contains("Q8")
        );
        // Points at how a hive is actually joined, rather than at an epic that
        // has since shipped.
        let why = Scope::HiveJoin("h".into()).unissuable_reason().unwrap();
        assert!(why.contains("hive join"), "got {why}");
        assert!(why.contains("hive.read"), "got {why}");
    }

    // The qualifier grammar exists to keep the MAC's `\n` join unambiguous.
    // These are the characters that would break it.

    #[test]
    fn qualifier_rejects_mac_delimiters_and_whitespace() {
        for bad in ["a\nb", "a b", "a\tb", "a:b", "\n", " "] {
            assert!(
                validate_qualifier(bad).is_err(),
                "{bad:?} must be rejected — it would make the MAC input ambiguous"
            );
        }
    }

    #[test]
    fn qualifier_rejects_empty_and_overlong() {
        assert!(validate_qualifier("").is_err());
        assert!(validate_qualifier(&"a".repeat(MAX_QUALIFIER_LEN)).is_ok());
        assert!(validate_qualifier(&"a".repeat(MAX_QUALIFIER_LEN + 1)).is_err());
    }

    #[test]
    fn qualifier_rejects_path_traversal() {
        assert!(validate_qualifier("..").is_err());
        assert!(validate_qualifier("../etc").is_err());
        assert!(validate_qualifier("a..b").is_err());
        // A single dot is fine — project names have them.
        assert!(validate_qualifier("my.project").is_ok());
    }

    #[test]
    fn qualifier_accepts_ordinary_names() {
        for ok in ["claudectl", "my-project", "my_project", "proj.v2", "a1"] {
            assert!(validate_qualifier(ok).is_ok(), "{ok} should be accepted");
        }
    }

    #[test]
    fn serde_round_trips_as_a_plain_string() {
        let scope = Scope::ProjectQuery("claudectl".into());
        let json = serde_json::to_string(&scope).unwrap();
        // RFC §3.2 shows scopes as plain strings in the grant file.
        assert_eq!(json, "\"project.query:claudectl\"");
        let back: Scope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, scope);
    }

    #[test]
    fn serde_rejects_a_malformed_scope_string() {
        assert!(serde_json::from_str::<Scope>("\"project.write:x\"").is_err());
        assert!(serde_json::from_str::<Scope>("\"garbage\"").is_err());
    }
}

//! Capability scopes — `<resource>.<verb>:<qualifier>` (#427, RFC §3.3).
//!
//! Every verb is read-only. There is deliberately no write verb in this
//! grammar, which is the point of the whole access model: a grant can let
//! someone look, never touch.
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
        }
    }

    /// The project or hive this scope is bound to.
    pub fn qualifier(&self) -> &str {
        match self {
            Scope::ProjectQuery(q)
            | Scope::ProjectDocs(q)
            | Scope::FleetRead(q)
            | Scope::HiveRead(q)
            | Scope::HiveJoin(q) => q,
        }
    }

    /// Whether `claudectl access grant` will mint this scope today.
    ///
    /// `fleet.read` is Q8 ("scope defined, issued to nobody"); the hive scopes
    /// need the named-hive work in #424. Keeping them parseable but unissuable
    /// means a grant file written by a later version still loads here.
    pub fn is_issuable(&self) -> bool {
        matches!(self, Scope::ProjectQuery(_) | Scope::ProjectDocs(_))
    }

    /// Why this scope cannot be issued yet, for the CLI's error message.
    pub fn unissuable_reason(&self) -> Option<&'static str> {
        match self {
            Scope::ProjectQuery(_) | Scope::ProjectDocs(_) => None,
            Scope::FleetRead(_) => Some(
                "fleet.read is defined but issued to nobody by default \
                 (open-cluster RFC Q8) — a third party reviewing your project \
                 should not see your live session costs",
            ),
            Scope::HiveRead(_) | Scope::HiveJoin(_) => {
                Some("hive scopes need named-hive identity, which is #424")
            }
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
            other => Err(format!(
                "unknown scope '{other}' (expected one of: project.query, \
                 project.docs, fleet.read, hive.read, hive.join)"
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
        ] {
            let scope: Scope = text.parse().expect("should parse");
            assert_eq!(scope.to_string(), text);
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
    fn there_is_no_write_verb() {
        for text in [
            "project.write:x",
            "project.edit:x",
            "fleet.write:x",
            "hive.write:x",
        ] {
            assert!(text.parse::<Scope>().is_err(), "{text} must not parse");
        }
    }

    #[test]
    fn only_query_and_docs_are_issuable_today() {
        assert!(Scope::ProjectQuery("p".into()).is_issuable());
        assert!(Scope::ProjectDocs("p".into()).is_issuable());
        // Parseable so later-written grant files still load, but not mintable.
        assert!(!Scope::FleetRead("p".into()).is_issuable());
        assert!(!Scope::HiveRead("h".into()).is_issuable());
        assert!(!Scope::HiveJoin("h".into()).is_issuable());
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
        assert!(
            Scope::HiveJoin("h".into())
                .unissuable_reason()
                .unwrap()
                .contains("#424")
        );
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

//! The denylist (#428, RFC §4.2).
//!
//! `git.rs` answers "is it tracked". This answers "is it excluded". Both halves
//! are needed, because tracked is not the same as publishable: someone can
//! commit a `.env`, and a transcript or a brain decision log dropped into a
//! repo is a tracked `.jsonl` like any other.
//!
//! The list is deliberately coarse and deny-first. A file that matches is never
//! read, never indexed, and never counted as a doc — matching happens before
//! any `read_to_string`.

use std::path::Path;

/// Filenames that are never indexed, matched case-insensitively on the whole
/// file name.
const DENIED_NAMES: &[&str] = &[
    ".env",
    ".envrc",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    "credentials",
    "secrets.yml",
    "secrets.yaml",
];

/// Filename prefixes that are never indexed. Catches `.env.local`,
/// `.env.production` and friends without enumerating them.
const DENIED_PREFIXES: &[&str] = &[".env", "id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"];

/// Extensions that are never indexed, matched case-insensitively.
///
/// `jsonl` is the load-bearing one: session transcripts and the brain decision
/// log are both JSONL, so one rule covers two of the three "never" classes in
/// RFC §4.2. The index has no use for line-delimited data anyway — it answers
/// structural questions from prose and signatures.
const DENIED_EXTENSIONS: &[&str] = &[
    "jsonl", // transcripts, brain decision logs
    "pem", "key", "p12", "pfx", "crt", "cer", "der", "keystore", "jks", "asc", "gpg", "kdbx",
    "sqlite", "sqlite3", "db", // coord.db / bus.db shaped things
];

/// Path segments that are never descended into. A directory here excludes
/// everything beneath it.
///
/// These are tracked-but-not-documentation trees. `.claude` and `.claudectl`
/// matter most: a repo that commits its own agent state would otherwise publish
/// decision logs, autopsies and session policy through the index.
const DENIED_DIR_SEGMENTS: &[&str] = &[
    ".claude",
    ".claudectl",
    ".git",
    ".ssh",
    ".gnupg",
    "node_modules",
    "target",
    "vendor",
    ".venv",
    "__pycache__",
];

/// Why a path was excluded, for the stats a build reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    Name,
    Extension,
    Directory,
}

/// Whether this tracked path must be kept out of the index.
///
/// Takes a work-tree-relative path. Every segment is checked, so a nested
/// `app/.claude/decisions.jsonl` is caught by the directory rule as well as the
/// extension rule.
pub fn denied(rel: &Path) -> Option<DenyReason> {
    for component in rel.components() {
        let seg = component.as_os_str().to_string_lossy().to_lowercase();
        if DENIED_DIR_SEGMENTS.contains(&seg.as_str()) {
            return Some(DenyReason::Directory);
        }
    }

    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if DENIED_NAMES.contains(&name.as_str()) {
        return Some(DenyReason::Name);
    }
    if DENIED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Some(DenyReason::Name);
    }

    let ext = rel
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !ext.is_empty() && DENIED_EXTENSIONS.contains(&ext.as_str()) {
        return Some(DenyReason::Extension);
    }

    None
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn d(p: &str) -> Option<DenyReason> {
        denied(&PathBuf::from(p))
    }

    #[test]
    fn env_files_are_denied_in_every_spelling() {
        assert_eq!(d(".env"), Some(DenyReason::Name));
        assert_eq!(d(".env.local"), Some(DenyReason::Name));
        assert_eq!(d(".env.production"), Some(DenyReason::Name));
        assert_eq!(d("config/.env"), Some(DenyReason::Name));
        assert_eq!(d(".envrc"), Some(DenyReason::Name));
    }

    #[test]
    fn jsonl_is_denied_because_transcripts_and_brain_logs_are_jsonl() {
        assert_eq!(d("decisions.jsonl"), Some(DenyReason::Extension));
        assert_eq!(d("transcript.jsonl"), Some(DenyReason::Extension));
        assert_eq!(d("docs/anything.jsonl"), Some(DenyReason::Extension));
        assert_eq!(d("knowledge.JSONL"), Some(DenyReason::Extension));
    }

    #[test]
    fn keys_and_certificates_are_denied() {
        for p in [
            "server.pem",
            "tls.key",
            "bundle.p12",
            "cert.crt",
            "backup.kdbx",
            "id_rsa",
            "id_rsa.pub",
            "id_ed25519",
        ] {
            assert!(d(p).is_some(), "{p} should be denied");
        }
    }

    #[test]
    fn agent_state_directories_are_denied_at_any_depth() {
        assert_eq!(
            d(".claudectl/brain/decisions.json"),
            Some(DenyReason::Directory)
        );
        assert_eq!(d(".claude/settings.json"), Some(DenyReason::Directory));
        assert_eq!(
            d("apps/web/.claude/skills/x.md"),
            Some(DenyReason::Directory)
        );
        assert_eq!(d("target/doc/index.md"), Some(DenyReason::Directory));
        assert_eq!(d("node_modules/pkg/README.md"), Some(DenyReason::Directory));
    }

    #[test]
    fn databases_are_denied() {
        assert_eq!(d("coord.db"), Some(DenyReason::Extension));
        assert_eq!(d("bus.sqlite3"), Some(DenyReason::Extension));
    }

    #[test]
    fn documentation_is_allowed() {
        for p in [
            "README.md",
            "CLAUDE.md",
            "docs/open-cluster.md",
            "docs/index.md",
            "src/lib.rs",
            "crates/core/src/session.rs",
            "Cargo.toml",
        ] {
            assert_eq!(d(p), None, "{p} should be indexable");
        }
    }

    #[test]
    fn a_file_merely_containing_a_denied_word_is_allowed() {
        // Substring matching would be too blunt — these are legitimate docs.
        assert_eq!(d("docs/environment.md"), None);
        assert_eq!(d("docs/keys-and-tokens.md"), None);
        assert_eq!(d("src/claudectl_notes.md"), None);
    }

    #[test]
    fn case_does_not_let_anything_through() {
        assert!(d(".ENV").is_some());
        assert!(d("Server.PEM").is_some());
        assert!(d("TARGET/x.md").is_some());
    }
}

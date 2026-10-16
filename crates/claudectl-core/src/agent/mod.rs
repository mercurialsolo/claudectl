//! Agent families — what claudectl knows about coding agents other than
//! Claude Code.
//!
//! The hive's substrate is agent-neutral: gossip, merge, trust and
//! membership move `KnowledgeUnit`s and know nothing about sessions. Its two
//! *ends* are not — capture runs through a Claude Code hook and application
//! lands in the brain's own prompt. This module holds the neutral pieces
//! that let a foreign agent's experience reach the same substrate.
//!
//! See `docs/cross-agent-hive.md` (epic #521) for the design, and §9 of that
//! document for the measurements the tables here are built from.

pub mod vocab;

use std::path::PathBuf;

use crate::helpers::dirs_home;

/// Every agent family claudectl knows how to reason about.
pub const KNOWN_FAMILIES: &[&str] = &[vocab::CLAUDE_CODE, vocab::CODEX];

/// The home directory an agent family keeps its configuration, skills and
/// transcripts in — `~/.claude` or `~/.codex`.
///
/// `None` for a family this build knows nothing about, which is how an
/// artifact requiring a future agent stays correctly unsatisfiable rather
/// than accidentally matching.
pub fn home_for(family: &str) -> Option<PathBuf> {
    match family {
        vocab::CLAUDE_CODE => Some(dirs_home().join(".claude")),
        vocab::CODEX => Some(dirs_home().join(".codex")),
        _ => None,
    }
}

/// Whether an agent family looks present on this machine.
///
/// Presence means *its state directory exists* — `~/.claude`, `~/.codex` —
/// not that its binary is on PATH. The directory is the better signal for
/// what claudectl does with the answer: a skill installs into that directory
/// and is read from it on the agent's next run, so an agent whose config
/// exists but whose binary lives somewhere unusual (a version manager, a
/// wrapper script, a GUI app bundle) should still receive artifacts.
pub fn is_family_present(family: &str) -> bool {
    home_for(family).is_some_and(|p| p.is_dir())
}

/// Which known agent families look present on this machine.
pub fn present_families() -> Vec<&'static str> {
    KNOWN_FAMILIES
        .iter()
        .copied()
        .filter(|f| is_family_present(f))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_families_all_resolve_to_a_home() {
        for family in KNOWN_FAMILIES {
            assert!(
                home_for(family).is_some(),
                "{family} is listed as known but has no home directory"
            );
        }
    }

    /// An unknown family must not resolve, so an artifact requiring a future
    /// agent stays unsatisfiable rather than matching by accident.
    #[test]
    fn unknown_family_has_no_home_and_is_absent() {
        assert!(home_for("some-future-agent").is_none());
        assert!(!is_family_present("some-future-agent"));
        assert!(!is_family_present(""));
    }

    #[test]
    fn homes_are_distinct_and_dotted() {
        let claude = home_for(vocab::CLAUDE_CODE).unwrap();
        let codex = home_for(vocab::CODEX).unwrap();
        assert_ne!(claude, codex);
        assert!(claude.ends_with(".claude"));
        assert!(codex.ends_with(".codex"));
    }

    #[test]
    fn present_families_is_a_subset_of_known() {
        for family in present_families() {
            assert!(KNOWN_FAMILIES.contains(&family));
        }
    }
}

#![allow(dead_code)]
//! Project context index — what a read-only query can be answered from
//! (#428, open-cluster RFC §4.2).
//!
//! This is the security boundary of the query surface, and it is a boundary by
//! construction rather than by policy. The index is built from the project's
//! committed tree, so *"is this published?"* reduces to *"is it tracked by git
//! and not excluded?"*. If something is not in the index, no classification
//! outcome and no prompt injection can reach it, because there is no code path
//! from a query to an unindexed file.
//!
//! Two rules make that true, and both are enforced structurally:
//!
//! 1. **`git ls-files` is the only source of paths.** No `read_dir`, no
//!    directory walk anywhere in this module. See `git.rs`.
//! 2. **No-git is an error, never a fallback.** Falling back to reading the
//!    directory would index `.env`. See `IndexError`.
//!
//! On top of those, `deny.rs` excludes tracked-but-unpublishable files and
//! `module_map.rs` emits signatures and doc comments but never function bodies.
//!
//! Scope note: this module builds the substrate and nothing more. Retrieval,
//! ranking and the query surface itself are #429; rate limits and budgets are
//! #431. There is deliberately no CLI here.

pub mod deny;
pub mod docs;
pub mod exposure;
pub mod git;
pub mod module_map;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use exposure::{Category, IndexExposure, ShareMode};
pub use git::IndexError;

/// A skill, as published to the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub source: String,
}

/// A hive knowledge unit, as published to the index.
///
/// Carries the rendered summary rather than the whole unit, so none of the
/// unit's bookkeeping (source peer, injection stats, consent) leaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitEntry {
    pub id: String,
    pub category: String,
    pub summary: String,
}

/// What was skipped while building, so a caller can tell an empty index from a
/// broken one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub tracked_files: usize,
    /// Excluded by `deny.rs`.
    pub denied: usize,
    /// Tracked but unreadable or not valid UTF-8.
    pub unreadable: usize,
    /// Categories turned off by exposure.
    pub categories_hidden: Vec<String>,
}

/// The built index. Everything a read-only query may be answered from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextIndex {
    /// Canonicalized work-tree root this was built from.
    pub root: String,
    pub claude_md: Vec<docs::DocSection>,
    pub readme: Vec<docs::DocSection>,
    pub docs: Vec<docs::DocSection>,
    pub module_map: Vec<module_map::ModuleEntry>,
    pub skills: Vec<SkillEntry>,
    pub hive_units: Vec<UnitEntry>,
    pub stats: IndexStats,
}

/// Markdown extensions treated as documentation.
const DOC_EXTENSIONS: &[&str] = &["md", "markdown"];

impl ContextIndex {
    /// Build an index for the repository containing `root`.
    ///
    /// Takes a path, not a project name: project names are many-to-one onto
    /// directories (every worktree of a repo shares a basename), so resolving a
    /// grant's `project.query:<name>` to a directory is #429's problem, not
    /// this module's.
    pub fn build(root: &Path) -> Result<Self, IndexError> {
        let mode = exposure::mode_from_config();
        let gate = IndexExposure::load();
        Self::build_with(root, &gate, mode)
    }

    /// Build with an explicit exposure gate and mode. The seam tests use.
    pub fn build_with(
        root: &Path,
        gate: &IndexExposure,
        mode: ShareMode,
    ) -> Result<Self, IndexError> {
        let repo = git::repo_root(root)?;
        let tracked = git::tracked_files(&repo)?;

        let mut index = ContextIndex {
            root: repo.display().to_string(),
            ..Default::default()
        };
        index.stats.tracked_files = tracked.len();

        for category in Category::ALL {
            if !gate.is_exposed(*category, mode) {
                index.stats.categories_hidden.push(category.label().into());
            }
        }

        let want = |c: Category| gate.is_exposed(c, mode);

        for rel in &tracked {
            if deny::denied(rel).is_some() {
                index.stats.denied += 1;
                continue;
            }
            if !git::is_readable_file(&repo, rel) {
                // A submodule gitlink, or a path that vanished since ls-files.
                index.stats.unreadable += 1;
                continue;
            }

            let rel_str = rel.display().to_string();
            let ext = rel
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();

            let is_md = DOC_EXTENSIONS.contains(&ext.as_str());
            let is_rs = ext == "rs";
            if !is_md && !is_rs {
                continue;
            }

            let target = if is_md {
                match classify_markdown(rel) {
                    Some(c) => c,
                    None => continue,
                }
            } else {
                Category::ModuleMap
            };
            if !want(target) {
                continue;
            }

            let Some(body) = read_text(&repo.join(rel)) else {
                index.stats.unreadable += 1;
                continue;
            };

            match target {
                Category::ClaudeMd => index.claude_md.extend(docs::sections(&rel_str, &body)),
                Category::Readme => index.readme.extend(docs::sections(&rel_str, &body)),
                Category::Docs => index.docs.extend(docs::sections(&rel_str, &body)),
                Category::ModuleMap => {
                    let entry = module_map::module_entry(&rel_str, &body);
                    // A file with no public surface and no header adds nothing.
                    if !entry.items.is_empty() || !entry.module_doc.is_empty() {
                        index.module_map.push(entry);
                    }
                }
                _ => {}
            }
        }

        if want(Category::Skills) {
            let tracked_set: HashSet<PathBuf> = tracked.iter().cloned().collect();
            index.skills = collect_skills(&repo, &tracked_set);
        }
        if want(Category::HiveUnits) {
            index.hive_units = collect_hive_units(&repo);
        }

        Ok(index)
    }

    /// A stable content fingerprint, for cache keys and determinism checks.
    ///
    /// FNV-1a rather than SHA-256: `relay::crypto` is behind the `relay`
    /// feature and this module is not gated, and the same reasoning as
    /// `coord::resume`'s tree hash applies — this is a cache key, not an auth
    /// primitive. If it ever becomes security-load-bearing, move `sha256` into
    /// `claudectl-core` first rather than taking a dependency here.
    pub fn fingerprint(&self) -> String {
        let body = serde_json::to_string(self).unwrap_or_default();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in body.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("fnv1a:{hash:016x}")
    }

    /// Whether the index holds nothing a query could be answered from.
    pub fn is_empty(&self) -> bool {
        self.claude_md.is_empty()
            && self.readme.is_empty()
            && self.docs.is_empty()
            && self.module_map.is_empty()
            && self.skills.is_empty()
            && self.hive_units.is_empty()
    }
}

/// Which documentation category a markdown path belongs to.
///
/// Anything outside `docs/` that is not `CLAUDE.md` or `README.md` is skipped:
/// a stray markdown file somewhere in the tree has not been offered up as
/// documentation, and the default is documentation-grade context.
fn classify_markdown(rel: &Path) -> Option<Category> {
    let name = rel.file_name()?.to_string_lossy().to_uppercase();
    let in_docs_dir = rel
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase() == "docs")
        .unwrap_or(false);

    if name == "CLAUDE.MD" {
        Some(Category::ClaudeMd)
    } else if name == "README.MD" {
        Some(Category::Readme)
    } else if in_docs_dir {
        Some(Category::Docs)
    } else {
        None
    }
}

/// Read a tracked file as text, returning `None` for non-UTF-8.
fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
}

/// Project skills, intersected with the tracked set.
///
/// `skills::discover` reads the filesystem directly and sweeps three roots:
/// `~/.claude/skills`, each installed plugin's skills, and
/// `<project>/.claude/skills`. Publishing its output as-is would drive a hole
/// straight through this module's first rule — an *untracked* skill dropped
/// into the project would be published, and so would the operator's personal
/// global skills, which say more about their machine than about this project.
///
/// Intersecting with the tracked set fixes both at once: a skill outside the
/// repo is not in that set, so user-level and plugin skills fall away without
/// needing a source filter.
fn collect_skills(repo: &Path, tracked: &std::collections::HashSet<PathBuf>) -> Vec<SkillEntry> {
    let mut out: Vec<SkillEntry> = claudectl_core::skills::discover(Some(repo))
        .into_iter()
        .filter(|s| {
            s.path
                .strip_prefix(repo)
                .map(|rel| tracked.contains(rel))
                .unwrap_or(false)
        })
        .map(|s| SkillEntry {
            name: s.name,
            description: s.description,
            source: s.source.label().to_string(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup();
    out
}

/// Locally-originated, shareable, exposed knowledge units scoped to this
/// project or to everything.
///
/// Categories are matched on the enum, not on a string — `KnowledgeCategory`
/// serializes `WorkflowPattern` as `"workflow_pattern"` but its `label()`
/// returns `"workflow"`, and matching the type sidesteps that entirely.
fn collect_hive_units(repo: &Path) -> Vec<UnitEntry> {
    #[cfg(feature = "hive")]
    {
        use crate::hive::{KnowledgeCategory, KnowledgeScope};

        let project = repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let store = crate::hive::store::HiveStore::load();
        let gate = crate::hive::exposure::ExposureStore::load();
        let mode = match exposure::mode_from_config() {
            ShareMode::Auto => crate::hive::exposure::ShareMode::Auto,
            ShareMode::Manual => crate::hive::exposure::ShareMode::Manual,
        };

        // Only knowledge this machine originated. A unit that arrived by
        // gossip is a peer's, and republishing it onward to a third party
        // would pass along something they never consented to share.
        let local_id = local_identity();

        let mut out: Vec<UnitEntry> = store
            .all_units()
            .into_iter()
            .filter(|u| u.source_peer == local_id)
            .filter(|u| {
                matches!(
                    u.category,
                    KnowledgeCategory::BestPractice
                        | KnowledgeCategory::Technique
                        | KnowledgeCategory::WorkflowPattern
                )
            })
            .filter(|u| match &u.scope {
                KnowledgeScope::Universal => true,
                KnowledgeScope::Project(p) => *p == project,
                // Language-scoped units are not project documentation.
                KnowledgeScope::Language(_) => false,
            })
            .filter(|u| gate.is_exposed(&u.id, mode))
            .map(|u| UnitEntry {
                id: u.id.clone(),
                category: format!("{:?}", u.category),
                summary: store.semantic_key_for(u),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
    #[cfg(not(feature = "hive"))]
    {
        let _ = repo;
        Vec::new()
    }
}

/// This machine's peer identity, for the locally-originated filter.
///
/// Mirrors the cfg pair the rest of the codebase uses for this: relay owns the
/// real identity, and hive has a hostname-derived fallback when relay is off.
#[cfg(feature = "hive")]
fn local_identity() -> String {
    #[cfg(feature = "relay")]
    {
        crate::relay::load_or_create_identity().0
    }
    #[cfg(not(feature = "relay"))]
    {
        crate::hive::local_identity()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test support
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    fn git(root: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Build a throwaway git repo with `files` committed.
    ///
    /// Returns `None` when git is unavailable, so tests skip rather than fail
    /// on a box without it. This is the first `git init` in the repo's tests;
    /// the `-c` flags keep it independent of whatever global git config CI has
    /// (or lacks) — an unset `user.email` makes `git commit` fail outright, and
    /// a configured signing key would prompt.
    pub fn git_fixture(files: &[(&str, &str)]) -> Option<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir().ok()?;
        let root = dir.path().to_path_buf();

        if !git(&root, &["init", "-q"]) {
            return None;
        }
        for (rel, body) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok()?;
            }
            std::fs::write(&path, body).ok()?;
        }
        if !git(&root, &["add", "-A"]) {
            return None;
        }
        let committed = git(
            &root,
            &[
                "-c",
                "user.name=claudectl-test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "fixture",
            ],
        );
        if !committed {
            return None;
        }
        Some((dir, root))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use exposure::ExposureState;

    fn auto() -> (IndexExposure, ShareMode) {
        (IndexExposure::default(), ShareMode::Auto)
    }

    /// The acceptance test from #428: a fixture project that *contains* every
    /// forbidden thing, asserting none of it reaches the index.
    ///
    /// The forbidden files are committed *into the project*, not left under
    /// `~/.claudectl` — a file that only lives outside the tree would be absent
    /// by construction and would prove nothing.
    #[test]
    fn no_forbidden_content_reaches_the_index() {
        let Some((_dir, root)) = tests_support::git_fixture(&[
            (
                "CLAUDE.md",
                "# Project\n\nCLAUDE_SENTINEL conventions here.\n",
            ),
            ("README.md", "# Readme\n\nREADME_SENTINEL public blurb.\n"),
            (
                "docs/design.md",
                "# Design\n\nDOCS_SENTINEL design notes.\n",
            ),
            (
                "src/lib.rs",
                "//! Lib header.\n\n/// Public thing.\npub fn api() {\n    let s = \"BODY_SENTINEL\";\n    drop(s);\n}\n",
            ),
            // Tracked, but must be denied by name.
            (".env", "API_KEY=ENV_SENTINEL\n"),
            // Tracked, but .jsonl — brain decision log shape.
            ("decisions.jsonl", "{\"command\":\"BRAIN_SENTINEL\"}\n"),
            // Tracked, but .jsonl — transcript shape.
            ("transcript.jsonl", "{\"text\":\"TRANSCRIPT_SENTINEL\"}\n"),
            // Tracked agent-state directory.
            (".claudectl/brain/notes.md", "# n\n\nSTATE_SENTINEL\n"),
            (".gitignore", "target/\nuntracked.md\n"),
        ]) else {
            eprintln!("skip: git unavailable");
            return;
        };

        // Untracked: present on disk, never in the index.
        std::fs::write(root.join("untracked.md"), "# U\n\nUNTRACKED_SENTINEL\n").unwrap();
        // Gitignored and untracked.
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/x.md"), "# I\n\nIGNORED_SENTINEL\n").unwrap();

        let (gate, mode) = auto();
        let index = ContextIndex::build_with(&root, &gate, mode).expect("should build");
        let blob = serde_json::to_string(&index).unwrap();

        // One serialized-blob assertion catches a leak through any field,
        // including fields added later.
        for forbidden in [
            "ENV_SENTINEL",
            "BRAIN_SENTINEL",
            "TRANSCRIPT_SENTINEL",
            "STATE_SENTINEL",
            "UNTRACKED_SENTINEL",
            "IGNORED_SENTINEL",
            "BODY_SENTINEL",
        ] {
            assert!(
                !blob.contains(forbidden),
                "{forbidden} leaked into the index"
            );
        }

        // And the things that *should* be there, so this is not passing by
        // building an empty index.
        for expected in [
            "CLAUDE_SENTINEL",
            "README_SENTINEL",
            "DOCS_SENTINEL",
            "pub fn api",
            "Lib header",
        ] {
            assert!(blob.contains(expected), "{expected} missing from the index");
        }
    }

    #[test]
    fn an_untracked_file_and_a_denied_file_are_excluded_by_different_mechanisms() {
        let Some((_dir, root)) =
            tests_support::git_fixture(&[("README.md", "# r\n\nok\n"), (".env", "K=V\n")])
        else {
            eprintln!("skip: git unavailable");
            return;
        };
        let (gate, mode) = auto();
        let index = ContextIndex::build_with(&root, &gate, mode).unwrap();

        // `.env` was tracked, so ls-files returned it and deny.rs rejected it.
        assert!(index.stats.denied >= 1, "{:?}", index.stats);
        assert_eq!(index.stats.tracked_files, 2);
    }

    #[test]
    fn build_refuses_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# x\n\nbody\n").unwrap();
        let (gate, mode) = auto();

        // Must not index the directory just because there are files in it.
        let err = ContextIndex::build_with(dir.path(), &gate, mode);
        assert!(err.is_err(), "a non-repo must not produce an index");
    }

    #[test]
    fn the_index_is_deterministic() {
        let Some((_dir, root)) = tests_support::git_fixture(&[
            ("CLAUDE.md", "# a\n\nx\n"),
            ("docs/b.md", "# b\n\ny\n"),
            ("docs/a.md", "# a\n\nz\n"),
            ("src/lib.rs", "//! h\n\npub fn f() {}\n"),
        ]) else {
            eprintln!("skip: git unavailable");
            return;
        };
        let (gate, mode) = auto();
        let a = ContextIndex::build_with(&root, &gate, mode).unwrap();
        let b = ContextIndex::build_with(&root, &gate, mode).unwrap();
        assert_eq!(a, b, "two builds of the same tree must be identical");
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert!(a.fingerprint().starts_with("fnv1a:"));
    }

    #[test]
    fn markdown_is_routed_by_where_it_lives() {
        let Some((_dir, root)) = tests_support::git_fixture(&[
            ("CLAUDE.md", "# c\n\nCONVENTIONS\n"),
            ("README.md", "# r\n\nPUBLIC\n"),
            ("docs/x.md", "# x\n\nDESIGN\n"),
            // Outside docs/ and not a known name: not offered as documentation.
            ("src/notes.md", "# n\n\nSTRAY\n"),
        ]) else {
            eprintln!("skip: git unavailable");
            return;
        };
        let (gate, mode) = auto();
        let index = ContextIndex::build_with(&root, &gate, mode).unwrap();

        assert!(
            index
                .claude_md
                .iter()
                .any(|s| s.body.contains("CONVENTIONS"))
        );
        assert!(index.readme.iter().any(|s| s.body.contains("PUBLIC")));
        assert!(index.docs.iter().any(|s| s.body.contains("DESIGN")));
        let blob = serde_json::to_string(&index).unwrap();
        assert!(!blob.contains("STRAY"), "a stray .md should not be indexed");
    }

    #[test]
    fn hiding_a_category_removes_it_and_records_why() {
        let Some((_dir, root)) = tests_support::git_fixture(&[
            ("CLAUDE.md", "# c\n\nKEEP_ME\n"),
            ("src/lib.rs", "//! header\n\npub fn gone() {}\n"),
        ]) else {
            eprintln!("skip: git unavailable");
            return;
        };

        let mut gate = IndexExposure::default();
        gate.set(Category::ModuleMap, ExposureState::Hide);
        let index = ContextIndex::build_with(&root, &gate, ShareMode::Auto).unwrap();

        assert!(index.module_map.is_empty(), "module map should be hidden");
        assert!(
            index
                .stats
                .categories_hidden
                .contains(&"module_map".to_string())
        );
        // Other categories still present.
        let blob = serde_json::to_string(&index).unwrap();
        assert!(blob.contains("KEEP_ME"));
        assert!(!blob.contains("pub fn gone"));
    }

    #[test]
    fn manual_mode_publishes_nothing_until_a_category_is_opted_in() {
        let Some((_dir, root)) = tests_support::git_fixture(&[
            ("CLAUDE.md", "# c\n\nCONVENTIONS\n"),
            ("README.md", "# r\n\nPUBLIC\n"),
        ]) else {
            eprintln!("skip: git unavailable");
            return;
        };

        let gate = IndexExposure::default();
        let index = ContextIndex::build_with(&root, &gate, ShareMode::Manual).unwrap();
        assert!(
            index.is_empty(),
            "manual mode defaults every category off: {index:?}"
        );
        assert_eq!(index.stats.categories_hidden.len(), Category::ALL.len());

        // Opting one in exposes exactly that one.
        let mut gate = IndexExposure::default();
        gate.set(Category::Readme, ExposureState::Expose);
        let index = ContextIndex::build_with(&root, &gate, ShareMode::Manual).unwrap();
        let blob = serde_json::to_string(&index).unwrap();
        assert!(blob.contains("PUBLIC"));
        assert!(!blob.contains("CONVENTIONS"));
    }

    #[test]
    fn a_repo_with_no_documentation_builds_an_empty_index_rather_than_failing() {
        let Some((_dir, root)) = tests_support::git_fixture(&[("Cargo.toml", "[package]\n")])
        else {
            eprintln!("skip: git unavailable");
            return;
        };
        // Hive units and skills come from outside the repo (the operator's
        // hive store and skill roots), so hide them to assert on what this
        // repo actually publishes.
        let mut gate = IndexExposure::default();
        gate.set(Category::HiveUnits, ExposureState::Hide);
        gate.set(Category::Skills, ExposureState::Hide);

        let index = ContextIndex::build_with(&root, &gate, ShareMode::Auto).unwrap();
        assert!(index.is_empty(), "{index:?}");
        assert_eq!(index.stats.tracked_files, 1);
    }

    #[test]
    fn an_untracked_project_skill_is_not_published() {
        // skills::discover reads the filesystem directly, so without the
        // tracked-set intersection an untracked skill would publish.
        let Some((_dir, root)) = tests_support::git_fixture(&[("README.md", "# r\n\nok\n")]) else {
            eprintln!("skip: git unavailable");
            return;
        };
        let skills = root.join(".claude").join("skills").join("sneaky");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(
            skills.join("SKILL.md"),
            "---\nname: sneaky\ndescription: SKILL_SENTINEL\n---\n\nbody\n",
        )
        .unwrap();

        let (gate, mode) = auto();
        let index = ContextIndex::build_with(&root, &gate, mode).unwrap();
        let blob = serde_json::to_string(&index).unwrap();
        assert!(
            !blob.contains("SKILL_SENTINEL"),
            "an untracked skill must not be published: {blob}"
        );
    }

    #[test]
    fn building_from_a_subdirectory_indexes_the_whole_repo() {
        let Some((_dir, root)) = tests_support::git_fixture(&[
            ("CLAUDE.md", "# c\n\nROOT_DOC\n"),
            ("deep/nested/keep.txt", "x"),
        ]) else {
            eprintln!("skip: git unavailable");
            return;
        };
        let (gate, mode) = auto();
        let index = ContextIndex::build_with(&root.join("deep/nested"), &gate, mode).unwrap();
        let blob = serde_json::to_string(&index).unwrap();
        assert!(blob.contains("ROOT_DOC"), "should index from the repo root");
    }

    /// Index this very repository and assert the result looks like
    /// documentation rather than a filesystem dump.
    ///
    /// A self-index is the one fixture that cannot drift from reality: real
    /// `.gitignore` rules, real source files, and whatever the working tree
    /// happens to be carrying.
    #[test]
    fn indexing_this_repo_publishes_docs_and_no_secrets() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let Ok(index) = ContextIndex::build_with(here, &IndexExposure::default(), ShareMode::Auto)
        else {
            eprintln!("skip: not a git work tree");
            return;
        };

        assert!(!index.claude_md.is_empty(), "CLAUDE.md should be indexed");
        assert!(!index.docs.is_empty(), "docs/ should be indexed");
        assert!(!index.module_map.is_empty(), "module map should be built");

        let blob = serde_json::to_string(&index).unwrap();
        for forbidden in [
            "ANTHROPIC_API_KEY",
            "BEGIN RSA PRIVATE KEY",
            "BEGIN OPENSSH PRIVATE KEY",
            "sk-ant-",
        ] {
            assert!(
                !blob.contains(forbidden),
                "{forbidden:?} reached the index built from this repo"
            );
        }

        eprintln!(
            "self-index: {} tracked, {} denied, {} unreadable, {} doc sections, {} modules",
            index.stats.tracked_files,
            index.stats.denied,
            index.stats.unreadable,
            index.claude_md.len() + index.readme.len() + index.docs.len(),
            index.module_map.len(),
        );
    }
}

//! The tracked-file gate (#428, RFC §4.2).
//!
//! The index's security property is that *"is this file published?"* reduces to
//! *"is it tracked by git and not excluded?"* — a question with a crisp answer
//! rather than a heuristic. That only holds if nothing in this module opens a
//! file by a path git did not hand back. So `tracked_files` is the sole source
//! of paths for the whole indexer: no `read_dir`, no directory walk, anywhere.
//!
//! **No-git is an error, not a fallback.** `coord/resume.rs` degrades to an
//! mtime hash when git is unavailable, because a stale tree hash is tolerable.
//! Here a fallback would mean indexing whatever happens to be on disk — `.env`
//! included — which is the one failure mode this module must not have.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Why an index could not be built.
///
/// A real enum rather than a `String` so #429 can match on `NotARepo` to say
/// something useful instead of pattern-matching an error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// `git` is not on PATH.
    GitUnavailable,
    /// The directory is not inside a git work tree.
    NotARepo { root: PathBuf },
    /// git ran and failed for some other reason.
    GitFailed { detail: String },
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::GitUnavailable => write!(
                f,
                "git is not available, and the context index refuses to fall \
                 back to reading the directory — that would index untracked \
                 files"
            ),
            IndexError::NotARepo { root } => write!(
                f,
                "{} is not a git work tree; the index is built from the \
                 committed tree, so there is nothing to publish",
                root.display()
            ),
            IndexError::GitFailed { detail } => write!(f, "git failed: {detail}"),
        }
    }
}

impl std::error::Error for IndexError {}

/// Run a git subcommand in `root`, returning stdout bytes.
///
/// `core.quotePath=false` keeps non-ASCII paths literal instead of
/// `"\303\251"`-escaped. stderr is discarded — the exit status is the signal.
fn run_git(root: &Path, args: &[&str]) -> Result<Vec<u8>, IndexError> {
    let out = Command::new("git")
        .arg("-c")
        .arg("core.quotePath=false")
        .args(args)
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .map_err(|_| IndexError::GitUnavailable)?;

    if !out.status.success() {
        return Err(IndexError::GitFailed {
            detail: format!("git {} exited {}", args.join(" "), out.status),
        });
    }
    Ok(out.stdout)
}

/// The repository work-tree root containing `start`.
pub fn repo_root(start: &Path) -> Result<PathBuf, IndexError> {
    let stdout = run_git(start, &["rev-parse", "--show-toplevel"]).map_err(|e| match e {
        // `rev-parse` failing here almost always means "not a repo", which is
        // worth saying precisely rather than as a generic git failure.
        IndexError::GitFailed { .. } => IndexError::NotARepo {
            root: start.to_path_buf(),
        },
        other => other,
    })?;
    let text = String::from_utf8_lossy(&stdout).trim().to_string();
    if text.is_empty() {
        return Err(IndexError::NotARepo {
            root: start.to_path_buf(),
        });
    }
    Ok(PathBuf::from(text))
}

/// Every file tracked in `root`, as paths relative to the work-tree root.
///
/// `--cached` only: the index, not the working tree. An uncommitted new file is
/// not published, and neither is a modified-but-tracked file's untracked
/// sibling. Sorted, so an index built twice is byte-identical.
///
/// Note this deliberately *does* return a tracked file that also matches a
/// `.gitignore` pattern — adding a file and then ignoring it leaves it tracked,
/// and tracked is the definition of published. The denylist in `deny.rs` is
/// what stops a committed `.env` from reaching the index.
pub fn tracked_files(root: &Path) -> Result<Vec<PathBuf>, IndexError> {
    let stdout = run_git(root, &["ls-files", "-z", "--cached", "--full-name"])?;

    let mut out: Vec<PathBuf> = stdout
        .split(|b| *b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| PathBuf::from(String::from_utf8_lossy(chunk).into_owned()))
        .collect();

    out.sort();
    out.dedup();
    Ok(out)
}

/// Whether a tracked path is a regular readable file.
///
/// A submodule appears in `ls-files` as a single gitlink entry that resolves to
/// a directory. Skipping non-files keeps the indexer from trying to read one.
pub fn is_readable_file(root: &Path, rel: &Path) -> bool {
    let full = root.join(rel);
    full.is_file()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_a_repo_is_an_error_rather_than_a_directory_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();

        // The whole point: with no repo there is no allowlist, so there must be
        // no result at all rather than "everything in the directory".
        match tracked_files(dir.path()) {
            Err(IndexError::NotARepo { .. }) | Err(IndexError::GitFailed { .. }) => {}
            Err(IndexError::GitUnavailable) => {} // no git on this box
            Ok(files) => panic!("expected an error, got {files:?}"),
        }
    }

    #[test]
    fn not_a_repo_error_text_explains_the_refusal() {
        let err = IndexError::NotARepo {
            root: PathBuf::from("/tmp/x"),
        };
        assert!(err.to_string().contains("committed tree"));
        assert!(
            IndexError::GitUnavailable
                .to_string()
                .contains("untracked files")
        );
    }

    #[test]
    fn tracked_files_lists_only_what_is_in_the_index() {
        let Some((_dir, root)) = crate::context::tests_support::git_fixture(&[
            ("README.md", "# hi"),
            ("docs/a.md", "a"),
        ]) else {
            eprintln!("skip: git unavailable");
            return;
        };

        // Untracked file: present on disk, absent from the index.
        std::fs::write(root.join("untracked.md"), "nope").unwrap();

        let files = tracked_files(&root).unwrap();
        assert!(files.contains(&PathBuf::from("README.md")), "{files:?}");
        assert!(files.contains(&PathBuf::from("docs/a.md")), "{files:?}");
        assert!(
            !files.contains(&PathBuf::from("untracked.md")),
            "an untracked file must never appear: {files:?}"
        );
    }

    #[test]
    fn tracked_files_is_sorted_and_deduped() {
        let Some((_dir, root)) = crate::context::tests_support::git_fixture(&[
            ("z.md", "z"),
            ("a.md", "a"),
            ("m/b.md", "b"),
        ]) else {
            eprintln!("skip: git unavailable");
            return;
        };
        let files = tracked_files(&root).unwrap();
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted, "order must be stable for a stable index");
    }

    #[test]
    fn repo_root_resolves_from_a_subdirectory() {
        let Some((_dir, root)) = crate::context::tests_support::git_fixture(&[("a/b/c.md", "c")])
        else {
            eprintln!("skip: git unavailable");
            return;
        };
        let deep = root.join("a/b");
        let found = repo_root(&deep).unwrap();
        // macOS puts tempdirs under /var -> /private/var, so compare canonically.
        assert_eq!(
            found.canonicalize().unwrap(),
            root.canonicalize().unwrap(),
            "repo_root from a subdir should find the work-tree root"
        );
    }

    #[test]
    fn a_directory_is_not_a_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("f.md"), "x").unwrap();
        assert!(is_readable_file(dir.path(), Path::new("f.md")));
        assert!(!is_readable_file(dir.path(), Path::new("sub")));
        assert!(!is_readable_file(dir.path(), Path::new("missing")));
    }
}

//! A git worktree per supervisor task (#570).
//!
//! A task with a `git_ref` runs in its own worktree on its own branch, so
//! parallel tasks over one repo never edit the same checkout. The worktree
//! lives under `~/.claudectl/worktrees/<task-id>`, outside the repo, so it
//! never shows up in that repo's `git status`.
//!
//! - **Reused, not recreated.** A retry or a resume of the same task finds
//!   its worktree and keeps working in it.
//! - **Removed after DONE only if clean.** An agent that left uncommitted
//!   changes left its work there, and removing the worktree would destroy it.
//!   The branch is always kept.
//! - **No git means no worktree:** a `git_ref` on a non-repo is an error, not
//!   a silent fallback to the plain cwd.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A worktree a task runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

/// Where task `task_id`'s worktree goes.
pub fn path_for(task_id: &str) -> PathBuf {
    claudectl_core::helpers::dirs_home()
        .join(".claudectl")
        .join("worktrees")
        .join(task_id)
}

/// The branch a task gets when it doesn't name one: `claudectl/<name>-<id>`,
/// with the name reduced to lowercase letters, digits and `-`.
pub fn default_branch(task_name: &str, task_id: &str) -> String {
    let mut name: String = task_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while name.contains("--") {
        name = name.replace("--", "-");
    }
    let name = name.trim_matches('-');
    let name = if name.is_empty() { "task" } else { name };
    // `task_<secs>_<pid>_<n>`: the part after the prefix is what's unique.
    let body = task_id.split_once('_').map_or(task_id, |(_, rest)| rest);
    let short: String = body
        .chars()
        .rev()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("claudectl/{}-{short}", &name[..name.len().min(40)])
}

/// Check a ref or branch name before it goes on a git command line. It comes
/// from a tasks file; a leading `-` would read as an option.
pub fn validate_ref(what: &str, value: &str) -> Result<(), String> {
    let ok = !value.is_empty()
        && value.len() <= 200
        && !value.starts_with('-')
        && value.chars().all(|c| {
            c.is_ascii_graphic() && !matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
        && !value.contains("..");
    if ok {
        Ok(())
    } else {
        Err(format!("{what} '{value}' is not a usable git ref"))
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The repo `cwd` is in.
pub fn repo_root(cwd: &Path) -> Result<PathBuf, String> {
    git(cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|e| format!("{} is not in a git repo: {e}", cwd.display()))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Make sure the task's worktree exists at `path` on `branch`, starting from
/// `git_ref`, in the repo containing `cwd`. Reuses one already there.
pub fn ensure(cwd: &Path, git_ref: &str, branch: &str, path: &Path) -> Result<Worktree, String> {
    validate_ref("git_ref", git_ref)?;
    validate_ref("branch", branch)?;
    let root = repo_root(cwd)?;
    let found = Worktree {
        path: path.to_path_buf(),
        branch: branch.to_string(),
    };

    if path.exists() {
        // Ours if it's a checkout of this repo rooted exactly at `path`.
        let top = git(path, &["rev-parse", "--show-toplevel"]).ok();
        let common = git(path, &["rev-parse", "--git-common-dir"]).ok();
        let root_common = git(&root, &["rev-parse", "--git-common-dir"]).ok();
        let same_repo = match (&common, &root_common) {
            (Some(a), Some(b)) => {
                same_dir(&path.join(a), &root.join(b)) || same_dir(Path::new(a), Path::new(b))
            }
            _ => false,
        };
        if top.as_deref().is_some_and(|t| same_dir(Path::new(t), path)) && same_repo {
            return Ok(found);
        }
        return Err(format!(
            "{} exists and is not this task's worktree; not touching it",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let path_s = path.to_string_lossy().to_string();
    let branch_exists = git(
        &root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    let result = if branch_exists {
        // A retry after the worktree was removed: check the branch out again.
        git(&root, &["worktree", "add", &path_s, branch])
    } else {
        git(&root, &["worktree", "add", "-b", branch, &path_s, git_ref])
    };
    result.map_err(|e| format!("git worktree add for {branch} at {git_ref}: {e}"))?;
    Ok(found)
}

/// What `cleanup` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Cleanup {
    Removed,
    /// Left in place because it holds uncommitted changes.
    KeptDirty,
    Missing,
}

/// Remove a finished task's worktree if nothing would be lost; keep the
/// branch either way.
pub fn cleanup(path: &Path) -> Result<Cleanup, String> {
    if !path.exists() {
        return Ok(Cleanup::Missing);
    }
    let dirty = git(path, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Ok(Cleanup::KeptDirty);
    }
    let root = git(path, &["rev-parse", "--git-common-dir"])?;
    let main = Path::new(&root)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf());
    let main = if main.is_relative() {
        path.join(main)
    } else {
        main
    };
    git(&main, &["worktree", "remove", &path.to_string_lossy()])?;
    Ok(Cleanup::Removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cctl-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn repo(tag: &str) -> PathBuf {
        let d = scratch(tag).join("repo");
        std::fs::create_dir_all(&d).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            git(&d, &args).unwrap();
        }
        std::fs::write(d.join("README"), "hi\n").unwrap();
        git(&d, &["add", "."]).unwrap();
        git(&d, &["commit", "-q", "-m", "init"]).unwrap();
        d
    }

    #[test]
    fn a_worktree_is_created_on_its_branch_from_the_ref() {
        let r = repo("create");
        let wt = r.parent().unwrap().join("wt");
        let got = ensure(&r, "main", "claudectl/t-1", &wt).unwrap();
        assert_eq!(got.path, wt);
        assert!(wt.join("README").exists());
        assert_eq!(
            git(&wt, &["branch", "--show-current"]).unwrap(),
            "claudectl/t-1"
        );
        // The main checkout is untouched.
        assert_eq!(git(&r, &["branch", "--show-current"]).unwrap(), "main");
    }

    #[test]
    fn an_existing_worktree_is_reused() {
        let r = repo("reuse");
        let wt = r.parent().unwrap().join("wt");
        ensure(&r, "main", "claudectl/t-2", &wt).unwrap();
        std::fs::write(wt.join("work.txt"), "in progress").unwrap();
        ensure(&r, "main", "claudectl/t-2", &wt).unwrap();
        assert!(
            wt.join("work.txt").exists(),
            "reuse must not reset the worktree"
        );
    }

    #[test]
    fn a_removed_worktree_comes_back_on_its_existing_branch() {
        let r = repo("again");
        let wt = r.parent().unwrap().join("wt");
        ensure(&r, "main", "claudectl/t-3", &wt).unwrap();
        std::fs::write(wt.join("f"), "x").unwrap();
        git(&wt, &["add", "f"]).unwrap();
        git(&wt, &["commit", "-q", "-m", "work"]).unwrap();
        assert_eq!(cleanup(&wt).unwrap(), Cleanup::Removed);
        ensure(&r, "main", "claudectl/t-3", &wt).unwrap();
        assert!(
            wt.join("f").exists(),
            "the branch's commit should be checked out"
        );
    }

    #[test]
    fn a_directory_that_isnt_ours_is_left_alone() {
        let r = repo("foreign");
        let wt = r.parent().unwrap().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("precious"), "x").unwrap();
        assert!(ensure(&r, "main", "claudectl/t-4", &wt).is_err());
        assert!(wt.join("precious").exists());
    }

    #[test]
    fn cleanup_keeps_a_worktree_with_uncommitted_work() {
        let r = repo("dirty");
        let wt = r.parent().unwrap().join("wt");
        ensure(&r, "main", "claudectl/t-5", &wt).unwrap();
        std::fs::write(wt.join("HELLO.txt"), "ok").unwrap();
        assert_eq!(cleanup(&wt).unwrap(), Cleanup::KeptDirty);
        assert!(wt.join("HELLO.txt").exists());
    }

    #[test]
    fn cleanup_removes_a_clean_worktree_and_keeps_the_branch() {
        let r = repo("clean");
        let wt = r.parent().unwrap().join("wt");
        ensure(&r, "main", "claudectl/t-6", &wt).unwrap();
        assert_eq!(cleanup(&wt).unwrap(), Cleanup::Removed);
        assert!(!wt.exists());
        assert!(git(&r, &["rev-parse", "--verify", "refs/heads/claudectl/t-6"]).is_ok());
        assert_eq!(cleanup(&wt).unwrap(), Cleanup::Missing);
    }

    #[test]
    fn a_ref_on_a_non_repo_is_an_error_not_a_fallback() {
        let d = scratch("norepo");
        assert!(ensure(&d, "main", "claudectl/t-7", &d.join("wt")).is_err());
    }

    #[test]
    fn refs_that_could_be_options_or_odd_syntax_are_refused() {
        for bad in ["-x", "--force", "a b", "a..b", "a~1", "", "x:y"] {
            assert!(validate_ref("git_ref", bad).is_err(), "{bad}");
        }
        for good in [
            "main",
            "origin/main",
            "v1.2.3",
            "claudectl/t-1",
            "abc123def",
        ] {
            assert!(validate_ref("git_ref", good).is_ok(), "{good}");
        }
    }

    #[test]
    fn default_branches_are_tidy_and_unique_per_task() {
        assert_eq!(
            default_branch("Fix the login bug!", "task_1792_77_3"),
            "claudectl/fix-the-login-bug-1792773"
        );
        assert_eq!(default_branch("***", "t1"), "claudectl/task-t1");
        assert!(validate_ref("branch", &default_branch("a/b c", "task_9")).is_ok());
    }
}

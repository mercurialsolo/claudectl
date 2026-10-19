//! Which bound role a task with no `role` should go to (#596 follow-up).
//!
//! A task declares where it runs (`cwd`) and, optionally, the files it is
//! about (`paths`). A role bound to a subtree *owns* the work under it
//! (`bus::roles::owner_of_path`). This module applies that ownership to a
//! task, so the reconciler can mail the task to the role instead of
//! spawning a fresh session once placement has decided the task starts on
//! this machine:
//!
//! 1. The *targets* are the task's `paths` under its `cwd`, or the `cwd`
//!    itself when it names none.
//! 2. The *candidates* are the roles whose selector covers every target,
//!    keeping only those on the longest selector: the same rule as
//!    `owner_of_path`, applied to a set.
//! 3. One live candidate is the answer. Several live candidates (two
//!    `/role` sessions at one repo root, say) are told apart by *focus*:
//!    the one whose session's recent edits concentrate in the deepest
//!    directory enclosing every target. A tie, or no focus that encloses
//!    the targets, is no answer.
//!
//! No answer means the reconciler does what it always did: spawn. A role
//! that is not live (its pid gone and no running session answering to it)
//! is never chosen, so a task is not parked in a mailbox nobody reads.
//!
//! The function is pure given its inputs. The roster carries each role's
//! selector as a canonical stem and its focus as canonical directories (the
//! sensor's job); the task's own paths are canonicalized here through the
//! `canon` the caller passes, `paths::canonicalize_deep` in production and
//! the identity in tests, so `/tmp/repo` and `/private/tmp/repo` meet.
//!
//! Only `cwd` and declared `paths` take part. The prompt is not scanned for
//! path-like words: a guess there would route work by accident, and the
//! operator can name the paths in one line.

use std::path::{Path, PathBuf};

use claudectl_core::paths::normalize_lexically;

use super::supervisor::RoleView;

/// Where a routed task goes, and why, for the transition ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub role: String,
    pub why: String,
}

/// The paths a task is about, absolute and lexically normalized. A path
/// that leaves `cwd` through `..` is dropped: it is not this task's work,
/// and it must not pick an owner outside the repo. No `paths` means the
/// `cwd` itself.
pub fn targets(cwd: &Path, paths: &[String]) -> Vec<PathBuf> {
    let cwd = normalize_lexically(cwd);
    let mut out: Vec<PathBuf> = paths
        .iter()
        .map(|p| {
            let p = Path::new(p);
            let joined = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            normalize_lexically(&joined)
        })
        .filter(|p| p.starts_with(&cwd))
        .collect();
    if out.is_empty() {
        out.push(cwd);
    }
    out.sort();
    out.dedup();
    out
}

/// The role that should take a task at `cwd` about `paths`, given the
/// bound roles and what their sessions are doing. `None` means spawn.
pub fn route(
    cwd: &Path,
    paths: &[String],
    roles: &[RoleView],
    canon: &dyn Fn(&Path) -> PathBuf,
) -> Option<Routed> {
    let targets = targets(cwd, paths);
    let about = describe(cwd, &targets);
    let canon_targets: Vec<PathBuf> = targets.iter().map(|t| canon(t)).collect();
    let target_strs: Vec<String> = canon_targets
        .iter()
        .map(|t| t.to_string_lossy().into_owned())
        .collect();

    // Owners: every target covered, longest selector wins.
    let mut best_len = 0usize;
    let mut owners: Vec<&RoleView> = Vec::new();
    for r in roles {
        if !target_strs
            .iter()
            .all(|t| crate::bus::roles::stem_covers(&r.stem, r.bare, t))
        {
            continue;
        }
        let len = r.stem.len();
        if len > best_len {
            best_len = len;
            owners.clear();
        }
        if len == best_len {
            owners.push(r);
        }
    }
    let live: Vec<&RoleView> = owners.iter().copied().filter(|r| r.live).collect();
    match live.len() {
        0 => None,
        1 => {
            let r = live[0];
            let why = if owners.len() == 1 {
                format!("{} owns {about}", r.name)
            } else {
                format!("{} is the only running owner of {about}", r.name)
            };
            Some(Routed {
                role: r.name.clone(),
                why,
            })
        }
        _ => {
            // Several live owners on the same selector: the deepest focus
            // that encloses every target decides, if exactly one does.
            let mut best_depth = 0usize;
            let mut chosen: Vec<(&RoleView, PathBuf)> = Vec::new();
            for r in &live {
                for f in &r.focus {
                    if !canon_targets.iter().all(|t| t.starts_with(f)) {
                        continue;
                    }
                    let depth = f.components().count();
                    if depth > best_depth {
                        best_depth = depth;
                        chosen.clear();
                    }
                    if depth == best_depth && !chosen.iter().any(|(c, _)| c.name == r.name) {
                        chosen.push((r, f.clone()));
                    }
                }
            }
            match chosen.as_slice() {
                [(r, focus)] => Some(Routed {
                    role: r.name.clone(),
                    why: format!(
                        "{} is working in {}, which holds {about}",
                        r.name,
                        relative(&canon(cwd), focus)
                    ),
                }),
                _ => None,
            }
        }
    }
}

/// `src/relay` or `src/relay/peer.rs and 1 more`, or `its cwd`.
fn describe(cwd: &Path, targets: &[PathBuf]) -> String {
    let cwd_n = normalize_lexically(cwd);
    match targets {
        [only] if *only == cwd_n => "its cwd".to_string(),
        [only] => relative(cwd, only),
        [first, rest @ ..] => format!("{} and {} more", relative(cwd, first), rest.len()),
        [] => "its cwd".to_string(),
    }
}

/// `path` relative to `cwd` when it is under it, else as given.
fn relative(cwd: &Path, path: &Path) -> String {
    let cwd_n = normalize_lexically(cwd);
    let p = normalize_lexically(path);
    match p.strip_prefix(&cwd_n) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str, stem: &str, live: bool, focus: &[&str]) -> RoleView {
        RoleView {
            name: name.into(),
            stem: stem.into(),
            bare: false,
            live,
            focus: focus.iter().map(PathBuf::from).collect(),
        }
    }

    fn same(p: &Path) -> PathBuf {
        p.to_path_buf()
    }

    fn pick(cwd: &str, paths: &[&str], roles: &[RoleView]) -> Option<Routed> {
        let paths: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
        route(Path::new(cwd), &paths, roles, &same)
    }

    const CWD: &str = "/work/repo";

    #[test]
    fn targets_default_to_the_cwd_and_drop_escapes() {
        assert_eq!(
            targets(Path::new(CWD), &[]),
            vec![PathBuf::from("/work/repo")]
        );
        assert_eq!(
            targets(
                Path::new(CWD),
                &["src/relay/peer.rs".into(), "../other/x".into()]
            ),
            vec![PathBuf::from("/work/repo/src/relay/peer.rs")]
        );
        // An absolute path under the cwd is kept; `.` folds away.
        assert_eq!(
            targets(Path::new(CWD), &["/work/repo/./src".into()]),
            vec![PathBuf::from("/work/repo/src")]
        );
    }

    #[test]
    fn no_owner_means_spawn() {
        let roles = [role("other", "/work/elsewhere", true, &[])];
        assert_eq!(pick(CWD, &[], &roles), None);
    }

    #[test]
    fn one_live_owner_takes_it() {
        let roles = [role("repo", CWD, true, &[])];
        let r = pick(CWD, &[], &roles).unwrap();
        assert_eq!(r.role, "repo");
        assert_eq!(r.why, "repo owns its cwd");
    }

    #[test]
    fn a_dead_owner_is_never_chosen() {
        let roles = [role("repo", CWD, false, &[])];
        assert_eq!(pick(CWD, &[], &roles), None);
    }

    #[test]
    fn the_most_specific_selector_owns_the_paths() {
        let roles = [
            role("repo", CWD, true, &[]),
            role("relay", "/work/repo/src/relay", true, &[]),
        ];
        let r = pick(CWD, &["src/relay/peer.rs"], &roles).unwrap();
        assert_eq!(r.role, "relay");
        assert_eq!(r.why, "relay owns src/relay/peer.rs");
        // Work at the root is not the relay role's.
        let r = pick(CWD, &[], &roles).unwrap();
        assert_eq!(r.role, "repo");
        // A selector covers on a `/` boundary: `/work/repo` is not `/work/repo2`.
        assert_eq!(pick("/work/repo2", &[], &roles), None);
    }

    #[test]
    fn a_bare_prefix_selector_covers_its_siblings() {
        let mut r = role("proj", "/work/proj-", true, &[]);
        r.bare = true;
        let got = pick("/work/proj-impl", &[], std::slice::from_ref(&r)).unwrap();
        assert_eq!(got.role, "proj");
    }

    #[test]
    fn paths_under_different_owners_have_no_single_owner() {
        let roles = [
            role("relay", "/work/repo/src/relay", true, &[]),
            role("bus", "/work/repo/src/bus", true, &[]),
        ];
        // Only the root could cover both, and nobody is bound there.
        assert_eq!(pick(CWD, &["src/relay/a.rs", "src/bus/b.rs"], &roles), None);
    }

    #[test]
    fn shared_selector_goes_to_the_only_live_one() {
        let roles = [role("a", CWD, false, &[]), role("b", CWD, true, &[])];
        let r = pick(CWD, &[], &roles).unwrap();
        assert_eq!(r.role, "b");
        assert_eq!(r.why, "b is the only running owner of its cwd");
    }

    #[test]
    fn shared_selector_is_decided_by_focus() {
        let roles = [
            role("a", CWD, true, &["/work/repo/src/relay"]),
            role("b", CWD, true, &["/work/repo/src/bus"]),
        ];
        let r = pick(CWD, &["src/relay/peer.rs"], &roles).unwrap();
        assert_eq!(r.role, "a");
        assert_eq!(
            r.why,
            "a is working in src/relay, which holds src/relay/peer.rs"
        );
        let r = pick(CWD, &["src/bus/mcp.rs", "src/bus/cli.rs"], &roles).unwrap();
        assert_eq!(r.role, "b");
        assert_eq!(
            r.why,
            "b is working in src/bus, which holds src/bus/cli.rs and 1 more"
        );
    }

    #[test]
    fn the_deeper_focus_wins_and_a_tie_is_no_answer() {
        let deeper = [
            role("a", CWD, true, &["/work/repo"]),
            role("b", CWD, true, &["/work/repo/src"]),
        ];
        let r = pick(CWD, &["src/x.rs"], &deeper).unwrap();
        assert_eq!(r.role, "b");
        let tie = [
            role("a", CWD, true, &["/work/repo/src"]),
            role("b", CWD, true, &["/work/repo/src"]),
        ];
        assert_eq!(pick(CWD, &["src/x.rs"], &tie), None);
        let none_encloses = [
            role("a", CWD, true, &["/work/repo/docs"]),
            role("b", CWD, true, &[]),
        ];
        assert_eq!(pick(CWD, &["src/x.rs"], &none_encloses), None);
    }

    #[test]
    fn a_focus_that_does_not_cover_every_path_does_not_decide() {
        let roles = [
            role("a", CWD, true, &["/work/repo/src/relay"]),
            role("b", CWD, true, &["/work/repo"]),
        ];
        // Both paths are under `b`'s focus (the root); only one is under `a`'s.
        let r = pick(CWD, &["src/relay/a.rs", "src/bus/b.rs"], &roles).unwrap();
        assert_eq!(r.role, "b");
    }

    /// The roster is canonical (git's toplevel, resolved selectors); the
    /// task's paths are whatever the submitter typed. `canon` brings them
    /// together, for coverage and for the focus tiebreak alike.
    #[test]
    fn the_tasks_paths_are_canonicalized_before_they_are_compared() {
        fn private(p: &Path) -> PathBuf {
            let s = p.to_string_lossy();
            PathBuf::from(s.replacen("/tmp/", "/private/tmp/", 1))
        }
        let roles = [
            role(
                "a",
                "/private/tmp/repo",
                true,
                &["/private/tmp/repo/src/relay"],
            ),
            role(
                "b",
                "/private/tmp/repo",
                true,
                &["/private/tmp/repo/src/bus"],
            ),
        ];
        let paths = vec!["src/relay/peer.rs".to_string()];
        assert_eq!(
            route(Path::new("/tmp/repo"), &paths, &roles, &same),
            None,
            "without canonicalization nothing is covered"
        );
        let r = route(Path::new("/tmp/repo"), &paths, &roles, &private).unwrap();
        assert_eq!(r.role, "a");
        assert_eq!(
            r.why,
            "a is working in src/relay, which holds src/relay/peer.rs"
        );
    }
}

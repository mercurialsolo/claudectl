//! Path normalization shared by everything that compares a path a tool was
//! given with a path git or a role binding reported (#596).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `p` with its longest existing ancestor resolved on disk and the rest
/// re-appended. `std::fs::canonicalize` fails on a path that does not exist
/// yet, so a file about to be created under a symlinked root (macOS `/tmp`
/// → `/private/tmp`, a symlinked home) would otherwise never match a
/// selector or a worktree root that *was* canonicalized. A path with no
/// existing ancestor comes back verbatim.
pub fn canonicalize_deep(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let mut rest: Vec<OsString> = Vec::new();
    let mut cur = p;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name.to_os_string());
        }
        if let Ok(c) = std::fs::canonicalize(parent) {
            let mut out = c;
            for seg in rest.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        cur = parent;
    }
    p.to_path_buf()
}

/// The ways a path may be spelled when compared against a canonical
/// directory, most literal first and deduplicated: as given; with its
/// *parent* resolved and the final name re-appended, which is the file as it
/// sits in its directory even when the file itself is a symlink elsewhere;
/// and fully resolved. A role bound to `/repo/config` should own
/// `/repo/config/prod.toml` whether or not that file links out to
/// `/etc/app/prod.toml`, and whether `/repo` is reached through a symlink.
pub fn spellings(p: &Path) -> Vec<String> {
    let normalized = normalize_lexically(p);
    let mut out = vec![normalized.to_string_lossy().into_owned()];
    for s in canonical_spellings(&normalized) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// The on-disk spellings of an already normalized path: its parent resolved
/// with the final name re-appended, then the whole path resolved. The
/// costly half of [`spellings`], for callers that try the literal spelling
/// first and only pay for these when it matched nothing.
pub fn canonical_spellings(normalized: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(2);
    let mut push = |candidate: PathBuf| {
        let s = candidate.to_string_lossy().into_owned();
        if !out.contains(&s) {
            out.push(s);
        }
    };
    if let (Some(parent), Some(name)) = (normalized.parent(), normalized.file_name()) {
        push(canonicalize_deep(parent).join(name));
    }
    push(canonicalize_deep(normalized));
    out
}

/// `p` with `.` segments dropped and `..` segments folded into the segment
/// before them, without touching the disk. A prefix test on the raw string
/// would otherwise let `/repo/../sibling/x.rs` pass as being under `/repo`,
/// and count `/repo/./src` twice at the root. A leading `..` with nothing
/// to fold into is kept.
pub fn normalize_lexically(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                let popped =
                    matches!(out.components().next_back(), Some(Component::Normal(_))) && out.pop();
                if !popped {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_deep, spellings};
    use std::path::Path;

    #[test]
    fn a_symlinked_file_keeps_its_place_in_the_tree_as_one_spelling() {
        let tmp = std::env::temp_dir().join(format!("spell-{}", std::process::id()));
        let dir = tmp.join("config");
        let real = tmp.join("elsewhere").join("prod.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "x\n").unwrap();
        let link = dir.join("prod.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let got = spellings(&link);
        let in_place = std::fs::canonicalize(&dir).unwrap().join("prod.toml");
        let resolved = std::fs::canonicalize(&real).unwrap();
        assert_eq!(got[0], link.to_string_lossy());
        assert!(
            got.contains(&in_place.to_string_lossy().into_owned()),
            "{got:?}"
        );
        assert!(
            got.contains(&resolved.to_string_lossy().into_owned()),
            "{got:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dot_and_dotdot_segments_are_folded_without_touching_the_disk() {
        use super::normalize_lexically;
        let n = |s: &str| {
            normalize_lexically(Path::new(s))
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(n("/repo/../sibling/x.rs"), "/sibling/x.rs");
        assert_eq!(n("/repo/./src/./x.rs"), "/repo/src/x.rs");
        assert_eq!(n("/repo/src/../src/x.rs"), "/repo/src/x.rs");
        assert_eq!(n("/../x"), "/../x", "nothing to fold into is kept");
        assert_eq!(n("../x"), "../x");
        assert_eq!(
            spellings(Path::new("/repo/../sibling/x.rs"))[0],
            "/sibling/x.rs"
        );
    }

    #[test]
    fn a_plain_path_has_no_duplicate_spellings() {
        let got = spellings(Path::new("/definitely-not-a-real-root-xyz/a/b"));
        assert_eq!(got, vec!["/definitely-not-a-real-root-xyz/a/b".to_string()]);
    }

    #[test]
    fn an_existing_path_is_canonicalized_as_usual() {
        let tmp = std::env::temp_dir();
        assert_eq!(
            canonicalize_deep(&tmp),
            std::fs::canonicalize(&tmp).unwrap()
        );
    }

    #[test]
    fn a_missing_child_takes_its_existing_ancestors_canonical_form() {
        let tmp = std::env::temp_dir();
        let missing = tmp.join("canon-deep-missing-dir").join("new.rs");
        let want = std::fs::canonicalize(&tmp)
            .unwrap()
            .join("canon-deep-missing-dir")
            .join("new.rs");
        assert_eq!(canonicalize_deep(&missing), want);
    }

    #[test]
    fn a_path_under_the_root_only_keeps_its_spelling() {
        // `/` always exists, so the result is `/` plus the missing segments,
        // which is the input itself.
        let p = Path::new("/definitely-not-a-real-root-xyz/a/b");
        assert_eq!(canonicalize_deep(p), p.to_path_buf());
    }
}

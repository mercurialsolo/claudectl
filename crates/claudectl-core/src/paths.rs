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

#[cfg(test)]
mod tests {
    use super::canonicalize_deep;
    use std::path::Path;

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

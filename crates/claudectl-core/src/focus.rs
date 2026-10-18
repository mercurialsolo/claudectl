//! Where in its project a session is working (#596).
//!
//! A session's *focus* is the deepest directory under its worktree root
//! that holds a strict majority of its recent edits. It is derived from
//! `Edit` / `Write` / `NotebookEdit` tool inputs alone: no LLM, no
//! heuristics about names, integers end to end. A strict majority makes the
//! answer unique by construction (two sibling directories cannot both hold
//! more than half), and the root always qualifies, so a session with any
//! in-tree edit has a focus and one with none has `None`.
//!
//! Edits outside the root are not the project's work and are left out of
//! the total, so a session that also touches `~/.claude/CLAUDE.md` is still
//! focused on the directory it edits inside the repo.
//!
//! The bus uses the focus to route work to whichever role is bound to the
//! enclosing subtree; see `bus::roles::owner_of_path` in the binary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::transcript::{TranscriptBlock, TranscriptEvent, TranscriptRole, parse_line};

/// Bytes of transcript tail scanned for edits. Recent edits say what a
/// session is doing *now*; the bus process never runs the monitor, so this
/// is a bounded read rather than an incremental one.
pub const TRANSCRIPT_SCAN_BYTES: u64 = 2 * 1024 * 1024;

/// The directory a session's edits concentrate in, relative to the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    /// Directory relative to the root, `/`-separated; `.` is the root itself.
    pub dir: String,
    /// Edits under `dir` (every edit counts once per enclosing directory).
    pub edits: u32,
    /// Edits under the root in total.
    pub total: u32,
}

impl Focus {
    /// The focus as an absolute path under `root`.
    pub fn path_under(&self, root: &Path) -> PathBuf {
        if self.dir == "." {
            root.to_path_buf()
        } else {
            root.join(&self.dir)
        }
    }
}

/// The deepest directory holding a strict majority of `edits` under `root`.
///
/// `edits` is `(file path, edit count)`; paths are matched against `root`
/// as given and against its canonical form, because tool inputs may say
/// `/tmp/…` where `git rev-parse` says `/private/tmp/…`.
pub fn focus_from_edits<'a>(
    edits: impl IntoIterator<Item = (&'a str, u32)>,
    root: &Path,
) -> Option<Focus> {
    let roots = root_forms(root);
    let mut per_dir: HashMap<String, u32> = HashMap::new();
    let mut total = 0u32;
    for (file, count) in edits {
        let Some(rel) = relative_to_any(file, &roots) else {
            continue;
        };
        let Some(dir) = parent_dir(rel) else {
            continue;
        };
        total = total.saturating_add(count);
        for ancestor in ancestors(dir) {
            *per_dir.entry(ancestor).or_insert(0) += count;
        }
    }
    if total == 0 {
        return None;
    }
    per_dir
        .into_iter()
        .filter(|(_, n)| u64::from(*n) * 2 > u64::from(total))
        .max_by_key(|(dir, _)| depth(dir))
        .map(|(dir, edits)| Focus { dir, edits, total })
}

/// Edit counts per file from the tail of a transcript, keyed by the path
/// the tool was given. Reads at most `cap` bytes; an unreadable file is an
/// empty map, since a missing focus is the honest answer there.
pub fn edits_from_transcript(jsonl_path: &Path, cap: u64) -> HashMap<String, u32> {
    let text = read_tail(jsonl_path, cap).unwrap_or_default();
    let mut edits = HashMap::new();
    for line in text.lines() {
        let Some(TranscriptEvent::Message(msg)) = parse_line(line) else {
            continue;
        };
        if msg.role != TranscriptRole::Assistant {
            continue;
        }
        for block in &msg.content {
            let TranscriptBlock::ToolUse { name, input } = block else {
                continue;
            };
            if !matches!(name.as_str(), "Edit" | "Write" | "NotebookEdit") {
                continue;
            }
            let path = input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))
                .and_then(|v| v.as_str());
            if let Some(p) = path {
                *edits.entry(p.to_string()).or_insert(0) += 1;
            }
        }
    }
    edits
}

/// The focus of a session: its transcript tail against its worktree root.
pub fn session_focus(jsonl_path: Option<&Path>, root: &Path) -> Option<Focus> {
    let edits = edits_from_transcript(jsonl_path?, TRANSCRIPT_SCAN_BYTES);
    focus_from_edits(edits.iter().map(|(p, n)| (p.as_str(), *n)), root)
}

fn read_tail(path: &Path, cap: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len > cap {
        f.seek(SeekFrom::Start(len - cap))?;
    }
    let mut buf = Vec::new();
    f.take(cap).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// The root as given and as it resolves on disk, deduplicated, with no
/// trailing slash so the prefix test below is one shape.
fn root_forms(root: &Path) -> Vec<String> {
    let mut forms = vec![trim_slash(&root.to_string_lossy())];
    if let Ok(canon) = std::fs::canonicalize(root) {
        let c = trim_slash(&canon.to_string_lossy());
        if !forms.contains(&c) {
            forms.push(c);
        }
    }
    forms
}

fn trim_slash(s: &str) -> String {
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        "/".to_string()
    } else {
        t.to_string()
    }
}

/// `file` relative to the first root it sits under, or `None` when it is
/// outside every form of the root.
fn relative_to_any<'a>(file: &'a str, roots: &[String]) -> Option<&'a str> {
    roots.iter().find_map(|root| {
        if root == "/" {
            return file.strip_prefix('/');
        }
        file.strip_prefix(root.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
    })
}

/// The directory part of a relative file path, `.` for a top-level file.
fn parent_dir(rel: &str) -> Option<&str> {
    let rel = rel.trim_end_matches('/');
    if rel.is_empty() {
        return None;
    }
    Some(match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => ".",
    })
}

/// Every directory enclosing `dir`, from the root (`.`) down to `dir`.
fn ancestors(dir: &str) -> Vec<String> {
    let mut out = vec![".".to_string()];
    if dir == "." {
        return out;
    }
    let mut acc = String::new();
    for seg in dir.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        out.push(acc.clone());
    }
    out
}

fn depth(dir: &str) -> usize {
    if dir == "." {
        0
    } else {
        dir.split('/').count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focus(edits: &[(&str, u32)], root: &str) -> Option<Focus> {
        focus_from_edits(edits.iter().copied(), Path::new(root))
    }

    #[test]
    fn the_deepest_majority_directory_is_the_focus() {
        let f = focus(
            &[
                ("/repo/src/relay/peer.rs", 3),
                ("/repo/src/relay/mesh.rs", 2),
                ("/repo/src/bus/mcp.rs", 1),
            ],
            "/repo",
        )
        .unwrap();
        assert_eq!(f.dir, "src/relay");
        assert_eq!((f.edits, f.total), (5, 6));
    }

    #[test]
    fn an_even_split_stops_at_the_common_parent() {
        let f = focus(
            &[("/repo/src/relay/peer.rs", 2), ("/repo/src/bus/mcp.rs", 2)],
            "/repo",
        )
        .unwrap();
        assert_eq!(f.dir, "src", "neither sibling holds a strict majority");
    }

    #[test]
    fn work_spread_across_the_tree_is_the_root() {
        let f = focus(
            &[
                ("/repo/README.md", 1),
                ("/repo/src/a.rs", 1),
                ("/repo/docs/b.md", 1),
            ],
            "/repo",
        )
        .unwrap();
        assert_eq!(f.dir, ".");
        assert_eq!(f.path_under(Path::new("/repo")), PathBuf::from("/repo"));
    }

    #[test]
    fn edits_outside_the_root_do_not_count() {
        let f = focus(
            &[
                ("/home/me/.claude/CLAUDE.md", 10),
                ("/repo-other/src/x.rs", 10),
                ("/repo/src/relay/peer.rs", 1),
            ],
            "/repo",
        )
        .unwrap();
        assert_eq!(f.dir, "src/relay");
        assert_eq!(f.total, 1, "a sibling repo with the same prefix is outside");
    }

    #[test]
    fn no_in_tree_edits_is_no_focus() {
        assert_eq!(focus(&[], "/repo"), None);
        assert_eq!(focus(&[("/elsewhere/x.rs", 4)], "/repo"), None);
    }

    #[test]
    fn a_trailing_slash_on_the_root_is_harmless() {
        let f = focus(&[("/repo/src/a.rs", 1)], "/repo/").unwrap();
        assert_eq!(f.dir, "src");
        assert_eq!(f.path_under(Path::new("/repo")), PathBuf::from("/repo/src"));
    }

    #[test]
    fn the_canonical_root_matches_too() {
        // Tool inputs may name the resolved form of a root that is given
        // through a symlink (macOS `/tmp` → `/private/tmp`).
        let tmp = std::env::temp_dir().join(format!("focus-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        let canon = std::fs::canonicalize(&tmp).unwrap();
        let file = format!("{}/src/lib.rs", canon.display());
        let f = focus(&[(file.as_str(), 2)], &tmp.to_string_lossy()).unwrap();
        assert_eq!(f.dir, "src");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn transcript_edits_count_only_editing_tools_by_the_assistant() {
        let tmp = std::env::temp_dir().join(format!("focus-jsonl-{}", std::process::id()));
        let lines = [
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/src/relay/peer.rs"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/repo/src/bus/mcp.rs"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"NotebookEdit","input":{"notebook_path":"/repo/src/relay/nb.ipynb"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/repo/src/relay/peer.rs"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"please edit /repo/src/bus/x.rs"}}"#,
        ];
        std::fs::write(&tmp, lines.join("\n")).unwrap();
        let edits = edits_from_transcript(&tmp, TRANSCRIPT_SCAN_BYTES);
        assert_eq!(edits.get("/repo/src/relay/peer.rs"), Some(&2));
        assert_eq!(edits.get("/repo/src/relay/nb.ipynb"), Some(&1));
        assert_eq!(
            edits.get("/repo/src/bus/mcp.rs"),
            None,
            "reads are not edits"
        );
        assert_eq!(edits.len(), 2);
        let f = session_focus(Some(&tmp), Path::new("/repo")).unwrap();
        assert_eq!(f.dir, "src/relay");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn a_capped_read_keeps_the_tail() {
        let tmp = std::env::temp_dir().join(format!("focus-tail-{}", std::process::id()));
        let old = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/old/a.rs"}}]}}"#;
        let new = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/new/b.rs"}}]}}"#;
        std::fs::write(&tmp, format!("{old}\n{new}\n")).unwrap();
        let edits = edits_from_transcript(&tmp, (new.len() + 1) as u64);
        assert_eq!(edits.get("/repo/new/b.rs"), Some(&1));
        assert_eq!(
            edits.get("/repo/old/a.rs"),
            None,
            "the head fell outside the cap"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn a_missing_transcript_is_no_focus() {
        assert_eq!(session_focus(None, Path::new("/repo")), None);
        assert_eq!(
            session_focus(Some(Path::new("/nonexistent/x.jsonl")), Path::new("/repo")),
            None
        );
    }
}

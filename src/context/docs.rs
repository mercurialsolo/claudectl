//! Markdown section extraction (#428, RFC §4.2).
//!
//! Splits a markdown file into sections at ATX headings, keeping each body
//! whole. Nothing here parses links or lists — this module decides *what text
//! is publishable*, and retrieval over it is #429's job.
//!
//! Fenced code blocks are the one exception, and they have to be. `#` begins a
//! comment in TOML, shell, Python and YAML, so a config example in a fence is
//! full of lines that look exactly like H1 headings. #428 skipped fences on
//! the grounds that this module only decides publishability; #429 found that
//! wrong, because it also decides *citations*: `docs/configuration.md`'s
//! `# `escalation_model`. Unset = no routing.` was being published as a
//! top-level heading, which both named a section after a code comment and
//! split a code block in half.
//!
//! Hand-rolled rather than taking a markdown crate, matching the house style
//! that keeps `team_policy.rs` off the `toml` crate and `skills.rs` off a YAML
//! one.

use serde::{Deserialize, Serialize};

/// One heading and the prose under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocSection {
    /// Work-tree-relative path of the source file.
    pub path: String,
    /// Enclosing headings, outermost first, including this section's own.
    /// A `###` under a `#` yields two entries; preamble before any heading
    /// yields an empty vec.
    pub heading_path: Vec<String>,
    pub body: String,
}

/// Split markdown into sections, dropping a leading YAML frontmatter block.
pub fn sections(path: &str, content: &str) -> Vec<DocSection> {
    let content = strip_frontmatter(content);

    let mut out: Vec<DocSection> = Vec::new();
    // Heading text by level, so a deeper heading can report its ancestors.
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut body = String::new();

    let flush = |out: &mut Vec<DocSection>, heading_path: &[String], body: &str| {
        let trimmed = body.trim();
        if trimmed.is_empty() && heading_path.is_empty() {
            return;
        }
        out.push(DocSection {
            path: path.to_string(),
            heading_path: heading_path.to_vec(),
            body: trimmed.to_string(),
        });
    };

    // The fence we are inside, as `(marker char, length)`. CommonMark closes a
    // fence only with the same character and at least as many of them, which
    // is what lets a ```` ``` ```` appear inside a ```` ```` ```` block.
    let mut fence: Option<(char, usize)> = None;

    for line in content.lines() {
        if let Some(open) = fence {
            if closes_fence(line, open) {
                fence = None;
            }
            body.push_str(line);
            body.push('\n');
            continue;
        }
        if let Some(open) = opens_fence(line) {
            fence = Some(open);
            body.push_str(line);
            body.push('\n');
            continue;
        }
        match heading_level(line) {
            Some((level, text)) => {
                flush(&mut out, &current, &body);
                body.clear();
                // Drop any heading at or below this level, then push.
                stack.retain(|(l, _)| *l < level);
                stack.push((level, text));
                current = stack.iter().map(|(_, t)| t.clone()).collect();
            }
            None => {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    flush(&mut out, &current, &body);
    out
}

/// Whether `line` opens a fenced code block, as `(marker, run length)`.
///
/// Up to three leading spaces are allowed, matching CommonMark — beyond that
/// it is an indented code block, which has no fence to track.
fn opens_fence(line: &str) -> Option<(char, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let run = rest.chars().take_while(|c| *c == marker).count();
    if run < 3 {
        return None;
    }
    // An info string may not contain a backtick on a backtick fence, which is
    // what keeps inline code like ``` `a` ``` from reading as a fence.
    if marker == '`' && rest[run..].contains('`') {
        return None;
    }
    Some((marker, run))
}

/// Whether `line` closes the fence opened as `open`: the same marker, at least
/// as many of them, and nothing else on the line.
fn closes_fence(line: &str, open: (char, usize)) -> bool {
    let (marker, min_run) = open;
    let trimmed = line.trim();
    let run = trimmed.chars().take_while(|c| *c == marker).count();
    run >= min_run && trimmed.len() == run
}

/// `## Heading` → `(2, "Heading")`. Requires a space after the hashes, so a
/// `#define` or a `#1` comment is not a heading.
fn heading_level(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    let text = rest.strip_prefix(' ')?.trim();
    if text.is_empty() {
        return None;
    }
    Some((hashes, text.to_string()))
}

/// Drop a leading `---` … `---` YAML block. Skills carry one and it is
/// metadata, not prose.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start_matches(['\n', '\r']);
    if !trimmed.starts_with("---") {
        return content;
    }
    let mut lines = trimmed.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim() != "---" {
        return content;
    }
    let mut consumed = first.len();
    for line in lines {
        consumed += line.len();
        if line.trim() == "---" {
            return &trimmed[consumed..];
        }
    }
    // Unterminated frontmatter: treat the whole file as prose rather than
    // silently dropping everything.
    content
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_headings_and_keeps_bodies_whole() {
        let md = "# Title\n\nintro text\n\n## Sub\n\nsub text\n";
        let got = sections("a.md", md);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].heading_path, vec!["Title"]);
        assert_eq!(got[0].body, "intro text");
        assert_eq!(got[1].heading_path, vec!["Title", "Sub"]);
        assert_eq!(got[1].body, "sub text");
    }

    #[test]
    fn heading_path_reports_ancestors_and_pops_correctly() {
        let md = "# A\n\na\n\n## B\n\nb\n\n### C\n\nc\n\n## D\n\nd\n";
        let got = sections("a.md", md);
        let paths: Vec<Vec<String>> = got.iter().map(|s| s.heading_path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                vec!["A"],
                vec!["A", "B"],
                vec!["A", "B", "C"],
                // D is a sibling of B, so C must have been popped.
                vec!["A", "D"],
            ]
        );
    }

    #[test]
    fn preamble_before_any_heading_is_kept() {
        let got = sections("a.md", "loose intro\n\n# Title\n\nbody\n");
        assert_eq!(got.len(), 2);
        assert!(got[0].heading_path.is_empty());
        assert_eq!(got[0].body, "loose intro");
    }

    #[test]
    fn empty_sections_are_dropped_but_headings_are_kept() {
        // A heading with no prose still tells you the document's shape.
        let got = sections("a.md", "# A\n\n## B\n\nbody\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].heading_path, vec!["A"]);
        assert_eq!(got[0].body, "");
    }

    #[test]
    fn frontmatter_is_dropped() {
        let md = "---\nname: a-skill\ndescription: does things\n---\n\n# Title\n\nbody\n";
        let got = sections("s.md", md);
        let all: String = got.iter().map(|s| s.body.clone()).collect();
        assert!(!all.contains("description:"), "frontmatter leaked: {all}");
        assert!(all.contains("body"));
    }

    #[test]
    fn unterminated_frontmatter_keeps_the_content() {
        // Better to over-include prose than to silently publish nothing.
        let md = "---\nname: x\n\n# Title\n\nbody\n";
        let got = sections("s.md", md);
        let all: String = got.iter().map(|s| s.body.clone()).collect();
        assert!(all.contains("body"));
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        let got = sections("a.md", "#define X 1\n#1 is a comment\n");
        assert_eq!(got.len(), 1);
        assert!(got[0].heading_path.is_empty());
        assert!(got[0].body.contains("#define"));
    }

    #[test]
    fn seven_hashes_is_not_a_heading() {
        let got = sections("a.md", "####### too deep\n");
        assert!(got[0].heading_path.is_empty());
    }

    #[test]
    fn a_hash_inside_a_code_fence_does_not_split() {
        // This asserted the opposite in #428, as a documented simplification:
        // "harmless for a publishable-text decision". #429 reversed it,
        // because sections are also the citation unit — `docs/configuration.md`
        // was publishing a section whose heading was a TOML comment, and
        // splitting a code block in half to do it.
        let got = sections("a.md", "# A\n\n```bash\n# not really a heading\n```\n");
        assert_eq!(got.len(), 1, "got {got:?}");
        assert_eq!(got[0].heading_path, vec!["A"]);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(sections("a.md", "").is_empty());
        assert!(sections("a.md", "\n\n").is_empty());
    }

    #[test]
    fn path_is_recorded_on_every_section() {
        let got = sections("docs/x.md", "# A\n\nb\n");
        assert!(got.iter().all(|s| s.path == "docs/x.md"));
    }

    /// The shape that `docs/configuration.md` actually has. A TOML comment in
    /// a fence is not a heading, and the fence is not a section boundary.
    #[test]
    fn a_comment_inside_a_code_fence_is_not_a_heading() {
        let got = sections(
            "docs/configuration.md",
            "# Configuration\n\n```toml\n# `escalation_model`. Unset = no routing.\nescalation_model = \"x\"\n```\n\nAfter.\n",
        );
        assert_eq!(got.len(), 1, "one section, not three: {got:?}");
        assert_eq!(got[0].heading_path, vec!["Configuration"]);
        assert!(
            got[0].body.contains("escalation_model = \"x\"") && got[0].body.contains("After."),
            "the fence must stay whole and the prose after it must stay in the section: {:?}",
            got[0].body
        );
    }

    #[test]
    fn a_heading_after_a_closed_fence_still_opens_a_section() {
        let got = sections("d.md", "# A\n\n```sh\n# not a heading\n```\n\n## B\n\nb\n");
        let headings: Vec<_> = got.iter().map(|s| s.heading_path.clone()).collect();
        assert_eq!(headings, vec![vec!["A"], vec!["A", "B"]], "got {got:?}");
    }

    #[test]
    fn a_tilde_fence_and_a_longer_backtick_fence_are_tracked() {
        let got = sections(
            "d.md",
            "# A\n\n~~~\n# no\n~~~\n\n````\n# also no\n```\n# still inside\n````\n",
        );
        assert_eq!(got.len(), 1, "got {got:?}");
        assert_eq!(got[0].heading_path, vec!["A"]);
    }

    #[test]
    fn an_unterminated_fence_swallows_the_rest_rather_than_inventing_headings() {
        // Markdown's own reading: an unclosed fence runs to end of file. The
        // alternative — reopening heading detection — would resurrect exactly
        // the code-comment headings this guards against.
        let got = sections("d.md", "# A\n\n```\n# one\n## two\n");
        assert_eq!(got.len(), 1, "got {got:?}");
        assert_eq!(got[0].heading_path, vec!["A"]);
    }

    #[test]
    fn inline_code_is_not_mistaken_for_a_fence() {
        let got = sections("d.md", "# A\n\nUse ``` `x` ``` inline.\n\n## B\n\nb\n");
        assert_eq!(got.len(), 2, "got {got:?}");
    }
}

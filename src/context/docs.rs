//! Markdown section extraction (#428, RFC §4.2).
//!
//! Splits a markdown file into sections at ATX headings, keeping each body
//! whole. Nothing here parses links, lists or code fences — this module decides
//! *what text is publishable*, and retrieval over it is #429's job.
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

    for line in content.lines() {
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
    fn a_hash_inside_a_code_fence_still_splits() {
        // Known simplification: we do not track fences. A `# comment` inside a
        // shell block becomes a heading. Harmless for a publishable-text
        // decision, and documented so it is not mistaken for a bug.
        let got = sections("a.md", "# A\n\n```bash\n# not really a heading\n```\n");
        assert!(got.len() >= 2);
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
}

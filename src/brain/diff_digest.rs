#![allow(dead_code)]

//! Diff digest for tool calls (#237).
//!
//! Brain gate today sees `tool_name` + `command_or_path`. That's enough to
//! say "this is an Edit on src/foo.rs" but not enough to say "this Edit drops
//! a database table" or "this Write creates a .env file with what looks like
//! a private key". This module turns a Claude Code tool_input JSON payload
//! into a structured digest the brain prompt and decision log can both use.

use serde_json::Value;

// ────────────────────────────────────────────────────────────────────────────
// Tunables
// ────────────────────────────────────────────────────────────────────────────

/// Max characters of raw patch content embedded in the prompt. Beyond this we
/// fall back to the structured digest only.
pub(crate) const MAX_INLINE_PATCH_CHARS: usize = 1200;

/// Cap on per-string risky-token scan length. Prevents pathological inputs
/// (e.g. a 5MB write) from dominating CPU; we still report the file size.
const SCAN_CAP_CHARS: usize = 32_000;

// ────────────────────────────────────────────────────────────────────────────
// Data shapes
// ────────────────────────────────────────────────────────────────────────────

/// What kind of tool call this digest describes. Mostly affects rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Bash,
    Edit,
    MultiEdit,
    Write,
    NotebookEdit,
    Other,
}

impl DiffKind {
    fn label(&self) -> &'static str {
        match self {
            DiffKind::Bash => "bash",
            DiffKind::Edit => "edit",
            DiffKind::MultiEdit => "multi_edit",
            DiffKind::Write => "write",
            DiffKind::NotebookEdit => "notebook_edit",
            DiffKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffDigest {
    pub kind_label: String,
    pub files: Vec<String>,
    /// Number of distinct edit replacements (Edit/MultiEdit).
    pub edit_count: u32,
    /// Estimated lines added / removed across all edits.
    pub lines_added: u32,
    pub lines_removed: u32,
    /// Bytes of new content (Write/Edit new_string).
    pub bytes_new: u32,
    /// Risky tokens hit, deduped, ordered by first appearance.
    pub risky_tokens: Vec<String>,
    /// Risky path classification (e.g. `.env`, `secrets/`, `migrations/`).
    pub risky_paths: Vec<String>,
    /// Short (<= MAX_INLINE_PATCH_CHARS) preview of the actual diff — empty
    /// when the patch is too large to inline.
    pub preview: String,
    /// One-line summary suitable for tight contexts.
    pub headline: String,
}

impl DiffDigest {
    /// True if this digest reflects any kind of detectable risk.
    pub fn is_risky(&self) -> bool {
        !self.risky_tokens.is_empty() || !self.risky_paths.is_empty()
    }

    /// Multi-line prompt-ready render. Compact, no leading blank lines.
    pub fn format_for_prompt(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.headline);
        if !self.files.is_empty() {
            out.push_str(&format!("\nFiles: {}", self.files.join(", ")));
        }
        if self.lines_added + self.lines_removed > 0 {
            out.push_str(&format!(
                "\nLines: +{} / -{}",
                self.lines_added, self.lines_removed
            ));
        }
        if !self.risky_paths.is_empty() {
            out.push_str(&format!(
                "\nSensitive paths: {}",
                self.risky_paths.join(", ")
            ));
        }
        if !self.risky_tokens.is_empty() {
            out.push_str(&format!("\nRisky tokens: {}", self.risky_tokens.join(", ")));
        }
        if !self.preview.is_empty() {
            out.push_str("\nPatch preview:\n");
            out.push_str(&self.preview);
        }
        out
    }

    /// JSON value for the decision log.
    pub fn to_log_json(&self) -> Value {
        serde_json::json!({
            "kind": self.kind_label,
            "files": self.files,
            "edit_count": self.edit_count,
            "lines_added": self.lines_added,
            "lines_removed": self.lines_removed,
            "bytes_new": self.bytes_new,
            "risky_tokens": self.risky_tokens,
            "risky_paths": self.risky_paths,
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Risky-token table
// ────────────────────────────────────────────────────────────────────────────

/// Tokens we care about. Order matters only for first-seen ordering in the
/// digest. Categories: destructive shell, destructive SQL, credential
/// leakage, dangerous code patterns.
const RISKY_TOKENS: &[(&str, bool /* case_insensitive */)] = &[
    // Shell / VCS destructive
    ("rm -rf", false),
    ("git push --force", false),
    ("git push -f", false),
    ("git reset --hard", false),
    ("git checkout --", false),
    ("--no-verify", false),
    ("sudo ", false),
    // SQL destructive
    ("DROP TABLE", true),
    ("DROP DATABASE", true),
    ("TRUNCATE TABLE", true),
    ("DELETE FROM", true),
    ("ALTER TABLE", true),
    // Credentials / secrets
    ("BEGIN PRIVATE KEY", false),
    ("BEGIN RSA PRIVATE KEY", false),
    ("BEGIN OPENSSH PRIVATE KEY", false),
    ("AWS_SECRET_ACCESS_KEY", true),
    ("password=", true),
    ("api_key=", true),
    ("api-key=", true),
    ("Bearer ", false),
    // Dangerous code patterns (heuristic, helpful in code review framing)
    (".unwrap()", false),
    ("panic!(", false),
    ("eval(", false),
    ("exec(", false),
    ("__import__", false),
];

/// Path components considered sensitive irrespective of content.
const RISKY_PATH_NEEDLES: &[&str] = &[
    "secrets/",
    "secret/",
    "credentials",
    "id_rsa",
    "id_ed25519",
    ".pem",
    ".key",
    "migrations/",
    "migration/",
    ".github/workflows/",
    "/prod/",
    "/production/",
    "k8s/",
    "kubernetes/",
];

// ────────────────────────────────────────────────────────────────────────────
// Public entry point
// ────────────────────────────────────────────────────────────────────────────

/// Build a digest from a tool name and a tool_input JSON value. The JSON
/// shapes follow Claude Code's hook payload conventions:
///   Bash: `{"command": "..."}`
///   Edit: `{"file_path": "...", "old_string": "...", "new_string": "..."}`
///   MultiEdit: `{"file_path": "...", "edits": [{"old_string", "new_string"}, ...]}`
///   Write: `{"file_path": "...", "content": "..."}`
///   NotebookEdit: `{"notebook_path": "...", "new_source": "..."}`
pub fn build_digest(tool_name: &str, tool_input: &Value) -> DiffDigest {
    let kind = classify(tool_name);
    let mut d = DiffDigest {
        kind_label: kind.label().to_string(),
        ..DiffDigest::default()
    };

    match kind {
        DiffKind::Bash => fill_from_bash(&mut d, tool_input),
        DiffKind::Edit => fill_from_edit(&mut d, tool_input),
        DiffKind::MultiEdit => fill_from_multi_edit(&mut d, tool_input),
        DiffKind::Write => fill_from_write(&mut d, tool_input),
        DiffKind::NotebookEdit => fill_from_notebook(&mut d, tool_input),
        DiffKind::Other => fill_from_other(&mut d, tool_name, tool_input),
    }

    classify_paths(&mut d);
    d.headline = format_headline(&kind, &d);
    d
}

fn classify(tool_name: &str) -> DiffKind {
    match tool_name {
        "Bash" => DiffKind::Bash,
        "Edit" => DiffKind::Edit,
        "MultiEdit" => DiffKind::MultiEdit,
        "Write" => DiffKind::Write,
        "NotebookEdit" => DiffKind::NotebookEdit,
        _ => DiffKind::Other,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Per-tool extractors
// ────────────────────────────────────────────────────────────────────────────

fn fill_from_bash(d: &mut DiffDigest, input: &Value) {
    let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
    scan_tokens(cmd, &mut d.risky_tokens);
    d.preview = truncate(cmd, MAX_INLINE_PATCH_CHARS);
}

fn fill_from_edit(d: &mut DiffDigest, input: &Value) {
    if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
        d.files.push(p.to_string());
    }
    let old = input
        .get("old_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new = input
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    d.edit_count = 1;
    let (added, removed) = line_delta(old, new);
    d.lines_added = added;
    d.lines_removed = removed;
    d.bytes_new = new.len() as u32;
    scan_tokens(new, &mut d.risky_tokens);
    scan_tokens(old, &mut d.risky_tokens);
    d.preview = render_unified_preview(old, new);
}

fn fill_from_multi_edit(d: &mut DiffDigest, input: &Value) {
    if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
        d.files.push(p.to_string());
    }
    let Some(edits) = input.get("edits").and_then(|v| v.as_array()) else {
        return;
    };
    let mut previews = Vec::new();
    for edit in edits {
        let old = edit
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new = edit
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let (added, removed) = line_delta(old, new);
        d.lines_added += added;
        d.lines_removed += removed;
        d.bytes_new += new.len() as u32;
        d.edit_count += 1;
        scan_tokens(new, &mut d.risky_tokens);
        scan_tokens(old, &mut d.risky_tokens);
        // Only inline the first couple of previews to stay under budget.
        if previews.len() < 3 {
            previews.push(render_unified_preview(old, new));
        }
    }
    d.preview = previews.join("\n---\n");
    d.preview = truncate(&d.preview, MAX_INLINE_PATCH_CHARS);
}

fn fill_from_write(d: &mut DiffDigest, input: &Value) {
    if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
        d.files.push(p.to_string());
    }
    let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
    d.bytes_new = content.len() as u32;
    d.lines_added = count_lines(content);
    scan_tokens(content, &mut d.risky_tokens);
    d.preview = truncate(content, MAX_INLINE_PATCH_CHARS);
}

fn fill_from_notebook(d: &mut DiffDigest, input: &Value) {
    if let Some(p) = input.get("notebook_path").and_then(|v| v.as_str()) {
        d.files.push(p.to_string());
    }
    let new_source = input
        .get("new_source")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    d.bytes_new = new_source.len() as u32;
    d.lines_added = count_lines(new_source);
    scan_tokens(new_source, &mut d.risky_tokens);
    d.preview = truncate(new_source, MAX_INLINE_PATCH_CHARS);
}

fn fill_from_other(d: &mut DiffDigest, _tool: &str, input: &Value) {
    if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
        d.files.push(p.to_string());
    }
    let preview = serde_json::to_string(input).unwrap_or_default();
    scan_tokens(&preview, &mut d.risky_tokens);
    d.preview = truncate(&preview, MAX_INLINE_PATCH_CHARS);
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

fn classify_paths(d: &mut DiffDigest) {
    for path in &d.files {
        let p_lower = path.to_lowercase();
        for needle in RISKY_PATH_NEEDLES {
            if p_lower.contains(needle) && !d.risky_paths.iter().any(|r| r == needle) {
                d.risky_paths.push((*needle).to_string());
            }
        }
        // Tail-only check for ".env" (avoid matching ".envoy" etc).
        let is_env = p_lower == ".env"
            || p_lower.ends_with("/.env")
            || p_lower.contains(".env.")
            || p_lower.contains("/.env.");
        if is_env && !d.risky_paths.iter().any(|r| r == ".env") {
            d.risky_paths.push(".env".to_string());
        }
    }
}

fn line_delta(old: &str, new: &str) -> (u32, u32) {
    // Cheap: line counts via byte newlines. Sufficient for prompt context;
    // perfect diff math isn't needed here.
    let old_lines = count_lines(old);
    let new_lines = count_lines(new);
    if new_lines >= old_lines {
        (new_lines - old_lines, 0)
    } else {
        (0, old_lines - new_lines)
    }
}

fn count_lines(s: &str) -> u32 {
    if s.is_empty() {
        return 0;
    }
    // Mirror behaviour of `wc -l`-ish counting: number of "line starts".
    let mut n = 1u32;
    for b in s.bytes() {
        if b == b'\n' {
            n += 1;
        }
    }
    // If the string ends in a newline that's a trailing terminator, not a new line.
    if s.ends_with('\n') && n > 0 {
        n -= 1;
    }
    n
}

fn scan_tokens(text: &str, out: &mut Vec<String>) {
    if text.is_empty() {
        return;
    }
    let scan_slice: &str = if text.len() > SCAN_CAP_CHARS {
        // Char-boundary-safe truncate
        let mut end = SCAN_CAP_CHARS;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };
    for (needle, case_insensitive) in RISKY_TOKENS {
        let hit = if *case_insensitive {
            scan_slice.to_lowercase().contains(&needle.to_lowercase())
        } else {
            scan_slice.contains(*needle)
        };
        if hit && !out.iter().any(|t| t == *needle) {
            out.push((*needle).to_string());
        }
    }
}

fn render_unified_preview(old: &str, new: &str) -> String {
    // Cheap synthetic preview — not a real unified diff, but compact and
    // gives the LLM the gist without invoking an external diff tool.
    let mut out = String::new();
    let max_each = MAX_INLINE_PATCH_CHARS / 2;
    if !old.is_empty() {
        out.push_str("- ");
        out.push_str(&truncate(old, max_each).replace('\n', "\n- "));
        out.push('\n');
    }
    if !new.is_empty() {
        out.push_str("+ ");
        out.push_str(&truncate(new, max_each).replace('\n', "\n+ "));
    }
    truncate(&out, MAX_INLINE_PATCH_CHARS)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut end = 0;
    for (i, _) in s.char_indices() {
        if i >= max.saturating_sub(1) {
            break;
        }
        end = i;
    }
    // Step past the boundary we recorded so we keep that char.
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    let mut out = s[..end].to_string();
    out.push('…');
    out
}

fn format_headline(kind: &DiffKind, d: &DiffDigest) -> String {
    match kind {
        DiffKind::Bash => format!("Bash command ({} risky tokens)", d.risky_tokens.len()),
        DiffKind::Edit => format!(
            "Edit on {} (+{} / -{} lines)",
            d.files.first().map(|s| s.as_str()).unwrap_or("(unknown)"),
            d.lines_added,
            d.lines_removed,
        ),
        DiffKind::MultiEdit => format!(
            "MultiEdit on {} ({} edits, +{} / -{} lines)",
            d.files.first().map(|s| s.as_str()).unwrap_or("(unknown)"),
            d.edit_count,
            d.lines_added,
            d.lines_removed,
        ),
        DiffKind::Write => format!(
            "Write {} ({} lines, {} bytes)",
            d.files.first().map(|s| s.as_str()).unwrap_or("(unknown)"),
            d.lines_added,
            d.bytes_new,
        ),
        DiffKind::NotebookEdit => format!(
            "NotebookEdit on {} ({} lines, {} bytes)",
            d.files.first().map(|s| s.as_str()).unwrap_or("(unknown)"),
            d.lines_added,
            d.bytes_new,
        ),
        DiffKind::Other => format!("Tool call ({} risky tokens)", d.risky_tokens.len()),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_digest_records_path_and_line_delta() {
        let input = json!({
            "file_path": "src/main.rs",
            "old_string": "fn foo() {\n    1\n}",
            "new_string": "fn foo() {\n    1;\n    2\n}",
        });
        let d = build_digest("Edit", &input);
        assert_eq!(d.files, vec!["src/main.rs".to_string()]);
        assert_eq!(d.edit_count, 1);
        assert!(d.lines_added >= 1);
        assert!(d.preview.contains("+"));
        assert!(d.headline.contains("Edit on src/main.rs"));
    }

    #[test]
    fn bash_digest_flags_rm_rf() {
        let input = json!({"command": "rm -rf /tmp/bad"});
        let d = build_digest("Bash", &input);
        assert!(d.risky_tokens.iter().any(|t| t == "rm -rf"));
        assert!(d.is_risky());
    }

    #[test]
    fn write_to_env_file_flags_path() {
        let input = json!({
            "file_path": ".env",
            "content": "DB_PASSWORD=hunter2\n",
        });
        let d = build_digest("Write", &input);
        assert!(d.risky_paths.iter().any(|t| t == ".env"));
        assert!(
            d.risky_tokens
                .iter()
                .any(|t| t.eq_ignore_ascii_case("password="))
        );
    }

    #[test]
    fn multiedit_aggregates_across_edits() {
        let input = json!({
            "file_path": "src/lib.rs",
            "edits": [
                {"old_string": "a", "new_string": "a\nb"},
                {"old_string": "c", "new_string": "c\nd"},
            ],
        });
        let d = build_digest("MultiEdit", &input);
        assert_eq!(d.edit_count, 2);
        assert!(d.lines_added >= 2);
    }

    #[test]
    fn drops_inline_preview_for_oversized_writes() {
        let big = "x".repeat(MAX_INLINE_PATCH_CHARS * 2);
        let input = json!({"file_path": "huge.txt", "content": big});
        let d = build_digest("Write", &input);
        assert!(d.preview.chars().count() <= MAX_INLINE_PATCH_CHARS);
        assert!(d.bytes_new as usize > MAX_INLINE_PATCH_CHARS);
    }

    #[test]
    fn sql_drop_table_caught_case_insensitively() {
        let input = json!({
            "file_path": "migrations/2026_drop.sql",
            "content": "drop table users;\n",
        });
        let d = build_digest("Write", &input);
        assert!(
            d.risky_tokens
                .iter()
                .any(|t| t.eq_ignore_ascii_case("DROP TABLE"))
        );
        assert!(d.risky_paths.iter().any(|t| t == "migrations/"));
    }

    #[test]
    fn format_for_prompt_is_compact_and_keyed() {
        let input = json!({
            "file_path": "src/main.rs",
            "old_string": "1",
            "new_string": "2",
        });
        let d = build_digest("Edit", &input);
        let s = d.format_for_prompt();
        assert!(s.contains("Edit on src/main.rs"));
        assert!(s.contains("Files: src/main.rs"));
    }
}

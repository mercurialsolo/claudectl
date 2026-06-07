//! Role suggestion from session transcripts and cwd (#309).
//!
//! Given a running Claude session (pid → jsonl path + cwd), produce a
//! ranked list of role-name candidates so the operator doesn't have to
//! invent one cold. Pure analysis: never writes a binding, never queries
//! the LLM. Heuristics, weighted and merged.
//!
//! Composed of small `from_*` analyzers — each scans one signal and
//! returns `(name, score, reason)` triples. The merger normalizes names
//! and sums scores; the top-N rows are surfaced.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use claudectl_core::transcript::{TranscriptBlock, TranscriptEvent, TranscriptRole, parse_line};

/// One ranked suggestion. `reasons` lists the signals that contributed
/// (e.g. "cwd basename", "tool fan-out: writes-heavy", "explicit mention:
/// 'acting as planner'"). UIs show these inline so the operator can sanity-
/// check before accepting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleSuggestion {
    pub name: String,
    /// Higher is better; relative within a call's response.
    pub score: u32,
    pub reasons: Vec<String>,
}

/// Cap on transcript bytes we scan. Long sessions can produce JSONL files
/// in the hundreds of MB; suggestion quality plateaus well before then,
/// and a runaway scan in the TUI hot path would freeze the dashboard.
pub const MAX_TRANSCRIPT_BYTES: u64 = 2 * 1024 * 1024;

/// Top-level entry point. Reads (at most `MAX_TRANSCRIPT_BYTES` of) the
/// transcript at `jsonl_path` and combines its signals with `cwd`.
/// Returns at most `top` rows, sorted by score descending.
pub fn suggest_for_session(
    jsonl_path: Option<&Path>,
    cwd: &Path,
    top: usize,
) -> Vec<RoleSuggestion> {
    let mut acc: HashMap<String, (u32, Vec<String>)> = HashMap::new();

    for (name, score, reason) in from_cwd_basename(cwd) {
        push(&mut acc, name, score, reason);
    }

    if let Some(path) = jsonl_path {
        let text = read_capped(path, MAX_TRANSCRIPT_BYTES).unwrap_or_default();
        let messages: Vec<_> = text
            .lines()
            .filter_map(parse_line)
            .filter_map(|e| match e {
                TranscriptEvent::Message(m) => Some(m),
                _ => None,
            })
            .collect();

        for (name, score, reason) in from_explicit_mentions(&messages) {
            push(&mut acc, name, score, reason);
        }
        for (name, score, reason) in from_tool_shape(&messages) {
            push(&mut acc, name, score, reason);
        }
        for (name, score, reason) in from_path_patterns(&messages) {
            push(&mut acc, name, score, reason);
        }
    }

    let mut suggestions: Vec<RoleSuggestion> = acc
        .into_iter()
        .map(|(name, (score, reasons))| RoleSuggestion {
            name,
            score,
            reasons,
        })
        .collect();
    suggestions.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    suggestions.truncate(top.max(1));
    suggestions
}

fn push(acc: &mut HashMap<String, (u32, Vec<String>)>, name: String, score: u32, reason: String) {
    let entry = acc.entry(name).or_insert((0, Vec::new()));
    entry.0 = entry.0.saturating_add(score);
    if !entry.1.contains(&reason) {
        entry.1.push(reason);
    }
}

fn read_capped(path: &Path, cap: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len > cap {
        // Skip to (len - cap) so we get the tail. Recent messages carry
        // the strongest "what is this session doing now" signal.
        f.seek(SeekFrom::Start(len - cap))?;
    }
    let mut buf = String::new();
    f.take(cap).read_to_string(&mut buf)?;
    Ok(buf)
}

// ─── Heuristic: cwd basename ────────────────────────────────────────────────

/// The cwd directory name is often a strong hint already (`/repo/frontend`,
/// `/repo/infra-terraform`). We normalize separators / suffix noise and
/// score it modestly so explicit transcript mentions can still beat it.
fn from_cwd_basename(cwd: &Path) -> Vec<(String, u32, String)> {
    let Some(name) = cwd.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let normalized = normalize_role_name(name);
    if normalized.is_empty() {
        return Vec::new();
    }
    vec![(
        normalized.clone(),
        3,
        format!("cwd basename: '{normalized}'"),
    )]
}

// ─── Heuristic: explicit role mentions in early messages ────────────────────

/// Phrases that often introduce a role label in the first few turns of a
/// session. Case-insensitive. Capture group 1 is the role name.
const ROLE_PHRASES: &[&str] = &[
    "you are the ",
    "you are a ",
    "you are an ",
    "acting as the ",
    "acting as a ",
    "acting as an ",
    "acting as ",
    "as the ",
    "as a ",
    "role: ",
];

/// Scan the first ~10 messages for any role-like phrase. Earlier mentions
/// score higher; we only look in user/system text to avoid the LLM's own
/// echoes inflating the count.
fn from_explicit_mentions(
    messages: &[claudectl_core::transcript::TranscriptMessage],
) -> Vec<(String, u32, String)> {
    let mut out = Vec::new();
    for msg in messages.iter().take(10) {
        if msg.role != TranscriptRole::User {
            continue;
        }
        for block in &msg.content {
            let TranscriptBlock::Text(text) = block else {
                continue;
            };
            let lower = text.to_lowercase();
            for phrase in ROLE_PHRASES {
                if let Some(idx) = lower.find(phrase) {
                    let after = &text[idx + phrase.len()..];
                    let candidate: String = after
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
                        .take(40)
                        .collect();
                    // First word only, then normalize.
                    let first_word = candidate.split_whitespace().next().unwrap_or("");
                    let normalized = normalize_role_name(first_word);
                    if !normalized.is_empty() && normalized.len() <= 32 {
                        out.push((
                            normalized.clone(),
                            10,
                            format!("explicit mention near '{phrase}{first_word}'"),
                        ));
                    }
                }
            }
        }
    }
    out
}

// ─── Heuristic: tool fan-out shape ──────────────────────────────────────────

/// Heuristic-mapping of tool counts to a coarse role archetype. Counts are
/// across the entire window we scanned.
fn from_tool_shape(
    messages: &[claudectl_core::transcript::TranscriptMessage],
) -> Vec<(String, u32, String)> {
    let mut writes = 0u32;
    let mut reads = 0u32;
    let mut bash = 0u32;
    let mut bash_test = 0u32;
    for msg in messages.iter() {
        if msg.role != TranscriptRole::Assistant {
            continue;
        }
        for block in &msg.content {
            let TranscriptBlock::ToolUse { name, input } = block else {
                continue;
            };
            match name.as_str() {
                "Write" | "Edit" | "MultiEdit" => writes += 1,
                "Read" | "Glob" | "Grep" => reads += 1,
                "Bash" => {
                    bash += 1;
                    let cmd = input
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if cmd.contains("test")
                        || cmd.contains("pytest")
                        || cmd.contains("cargo test")
                        || cmd.contains("npm test")
                        || cmd.contains("vitest")
                    {
                        bash_test += 1;
                    }
                }
                _ => {}
            }
        }
    }

    let total = writes + reads + bash;
    if total < 3 {
        return Vec::new(); // not enough signal yet
    }

    let mut out = Vec::new();
    if writes * 100 / total.max(1) >= 50 {
        out.push((
            "impl".into(),
            5,
            format!("tool fan-out: writes-heavy ({writes}/{total})"),
        ));
    }
    if reads * 100 / total.max(1) >= 60 {
        out.push((
            "reviewer".into(),
            5,
            format!("tool fan-out: reads-heavy ({reads}/{total})"),
        ));
    }
    if bash_test > 0 && bash_test * 100 / bash.max(1) >= 40 {
        out.push((
            "tester".into(),
            5,
            format!("tool fan-out: test-runs ({bash_test}/{bash})"),
        ));
    }
    out
}

// ─── Heuristic: path patterns in tool inputs ────────────────────────────────

/// Directory tokens that often telegraph a role. Match is on path
/// fragments, not exact dir names, so `apps/frontend` or `src/frontend/`
/// both contribute. Score is small per-hit; common dirs accumulate.
const PATH_TOKENS: &[(&str, &str)] = &[
    ("frontend", "frontend"),
    ("backend", "backend"),
    ("infra", "infra"),
    ("terraform", "infra"),
    ("docs", "docs"),
    ("tests", "tester"),
    ("test", "tester"),
];

/// Look at file paths in `Read`/`Write`/`Edit` tool inputs and score role
/// tokens that show up. Cwd basename hits are already handled; this is
/// the per-file granularity that tells us "session touches frontend/".
fn from_path_patterns(
    messages: &[claudectl_core::transcript::TranscriptMessage],
) -> Vec<(String, u32, String)> {
    let mut hits: HashMap<&'static str, u32> = HashMap::new();
    for msg in messages.iter() {
        if msg.role != TranscriptRole::Assistant {
            continue;
        }
        for block in &msg.content {
            let TranscriptBlock::ToolUse { input, .. } = block else {
                continue;
            };
            let path_str = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .or_else(|| input.get("notebook_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            if path_str.is_empty() {
                continue;
            }
            for (token, role) in PATH_TOKENS {
                if path_str.contains(*token) {
                    *hits.entry(*role).or_insert(0) += 1;
                }
            }
        }
    }
    hits.into_iter()
        .map(|(role, count)| {
            (
                role.to_string(),
                count.min(6), // saturate so one huge session doesn't drown other signals
                format!("path patterns: {count} hits → '{role}'"),
            )
        })
        .collect()
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Reduce a free-form string to the role-name charset (lowercase
/// alphanumerics + `-`). Strips common project suffixes that aren't
/// informative ("-app", "-service"). Empty when the input has no usable
/// characters.
fn normalize_role_name(raw: &str) -> String {
    let lowered: String = raw
        .chars()
        .map(|c| match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            ' ' | '/' | '.' => '-',
            _ => '-',
        })
        .collect();
    let cleaned = lowered
        .trim_matches('-')
        .replace("--", "-")
        .replace("--", "-"); // two passes handle "a---b" → "a-b"
    let mut name = cleaned;
    for suffix in ["-app", "-service", "-svc", "-server", "-srv", "-project"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.to_string();
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn user_text(s: &str) -> claudectl_core::transcript::TranscriptMessage {
        claudectl_core::transcript::TranscriptMessage {
            role: TranscriptRole::User,
            model: None,
            stop_reason: None,
            usage: None,
            content: vec![TranscriptBlock::Text(s.into())],
        }
    }
    fn assistant_tool(
        name: &str,
        input: serde_json::Value,
    ) -> claudectl_core::transcript::TranscriptMessage {
        claudectl_core::transcript::TranscriptMessage {
            role: TranscriptRole::Assistant,
            model: None,
            stop_reason: None,
            usage: None,
            content: vec![TranscriptBlock::ToolUse {
                name: name.into(),
                input,
            }],
        }
    }

    #[test]
    fn cwd_basename_normalized_and_suggested() {
        let suggestions =
            suggest_for_session(None, &PathBuf::from("/Users/alice/work/frontend-app"), 3);
        assert!(
            !suggestions.is_empty(),
            "expected at least one suggestion from cwd basename"
        );
        assert_eq!(suggestions[0].name, "frontend");
        assert!(
            suggestions[0]
                .reasons
                .iter()
                .any(|r| r.contains("basename"))
        );
    }

    #[test]
    fn explicit_mention_outranks_cwd_basename() {
        let messages = vec![user_text(
            "You are the planner. Coordinate the other agents.",
        )];
        let mut mentions = from_explicit_mentions(&messages);
        mentions.sort_by(|a, b| b.1.cmp(&a.1));
        assert_eq!(mentions[0].0, "planner");
        // Score should be the configured 10 for an explicit mention; the
        // cwd basename only scores 3, so a merge would rank planner first.
        assert_eq!(mentions[0].1, 10);
    }

    #[test]
    fn tool_shape_writes_heavy_suggests_impl() {
        let messages = vec![
            assistant_tool("Write", json!({"file_path": "src/lib.rs"})),
            assistant_tool("Edit", json!({"file_path": "src/lib.rs"})),
            assistant_tool("Edit", json!({"file_path": "src/main.rs"})),
            assistant_tool("Read", json!({"file_path": "Cargo.toml"})),
        ];
        let shape = from_tool_shape(&messages);
        assert!(
            shape.iter().any(|(name, _, _)| name == "impl"),
            "writes-heavy session should suggest 'impl', got: {shape:?}"
        );
    }

    #[test]
    fn path_patterns_hit_role_tokens() {
        let messages = vec![
            assistant_tool("Read", json!({"file_path": "apps/frontend/Button.tsx"})),
            assistant_tool("Edit", json!({"file_path": "apps/frontend/Card.tsx"})),
            assistant_tool("Read", json!({"file_path": "src/backend/api.py"})),
        ];
        let hits = from_path_patterns(&messages);
        assert!(
            hits.iter().any(|(name, _, _)| name == "frontend"),
            "expected frontend hit, got: {hits:?}"
        );
    }

    #[test]
    fn normalize_strips_common_suffixes_and_lowercases() {
        assert_eq!(normalize_role_name("Frontend-App"), "frontend");
        assert_eq!(normalize_role_name("Acme Service"), "acme");
        assert_eq!(normalize_role_name("My  Project!"), "my");
    }

    #[test]
    fn suggestions_capped_to_top_n() {
        let suggestions = suggest_for_session(None, &PathBuf::from("/Users/alice/work/multi"), 1);
        assert!(suggestions.len() <= 1);
    }
}

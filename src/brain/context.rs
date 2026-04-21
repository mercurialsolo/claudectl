#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::session::{self, ClaudeSession};
use crate::transcript::{self, TranscriptBlock, TranscriptEvent};

/// Compact context for the brain LLM, built from session state + recent transcript.
#[derive(Debug, Clone)]
pub struct BrainContext {
    /// One-line session summary (status, cost, context%, pending tool).
    pub session_summary: String,
    /// Recent conversation messages, compacted to fit within token budget.
    pub recent_transcript: String,
    /// The decision prompt asking the LLM what to do.
    pub decision_prompt: String,
    /// Few-shot examples from past decisions (empty if no history).
    pub few_shot_examples: String,
    /// Distilled preference summary (compact alternative to few-shot for small contexts).
    pub preference_summary: String,
    /// Global view of all active sessions (empty if only one session).
    pub global_session_map: String,
    /// Git state for the session's working directory (empty if not a git repo).
    pub git_context: String,
    /// Coordination context: leases, blockers, handoffs, memory (empty if coord feature off).
    pub coordination_context: String,
}

/// Build a compact context for the brain from a session's state and JSONL transcript.
/// Pass all sessions for cross-session awareness.
pub fn build_context(
    session: &ClaudeSession,
    all_sessions: &[ClaudeSession],
    max_tokens: u32,
) -> BrainContext {
    let session_summary = format_session_summary(session);
    let recent_transcript = read_recent_transcript(session, max_tokens);
    let decision_prompt = format_decision_prompt(session);
    let global_session_map = format_global_session_map(session.pid, all_sessions);

    let git_context = build_git_context(&session.cwd);

    BrainContext {
        session_summary,
        recent_transcript,
        decision_prompt,
        few_shot_examples: String::new(), // Set by engine after retrieval
        preference_summary: String::new(), // Set by engine from distilled preferences
        global_session_map,
        git_context,
        coordination_context: String::new(), // Set by engine from coord store
    }
}

fn format_session_summary(session: &ClaudeSession) -> String {
    let context_pct = if session.context_max > 0 {
        (session.context_tokens as f64 / session.context_max as f64 * 100.0) as u32
    } else {
        0
    };

    let mut summary = format!(
        "Project: {} | Status: {} | Model: {} | Cost: ${:.2} | Context: {}%",
        session.display_name(),
        session.status,
        session.model,
        session.cost_usd,
        context_pct,
    );

    if let Some(ref tool) = session.pending_tool_name {
        summary.push_str(&format!(" | Pending tool: {tool}"));
        if let Some(ref input) = session.pending_tool_input {
            let truncated = if input.len() > 200 {
                format!("{}...", session::truncate_str(input, 200))
            } else {
                input.clone()
            };
            summary.push_str(&format!(" | Command: {truncated}"));
        }
    }

    if session.decay_score > 0 {
        summary.push_str(&format!(" | Decay: {}/100", session.decay_score));
    }

    if session.last_tool_error {
        if let Some(ref msg) = session.last_error_message {
            let truncated = session::truncate_str(msg, 100);
            summary.push_str(&format!(" | Last tool ERRORED: {truncated}"));
        } else {
            summary.push_str(" | Last tool ERRORED");
        }
    }

    summary
}

fn format_decision_prompt(session: &ClaudeSession) -> String {
    match session.status {
        crate::session::SessionStatus::NeedsInput => {
            let tool = session.pending_tool_name.as_deref().unwrap_or("unknown");
            format!(
                "The session is waiting for approval of a '{}' tool call. \
                 Should this be approved, denied, or should a message be sent instead? \
                 Respond with JSON: {{\"action\": \"approve\"|\"deny\"|\"send\"|\"terminate\"|\"route\", \
                 \"message\": \"...\", \"reasoning\": \"...\", \"confidence\": 0.0-1.0, \
                 \"target_pid\": <pid if action is route>}}. \
                 Use 'route' to send summarized output from this session to another session.",
                tool
            )
        }
        crate::session::SessionStatus::WaitingInput => {
            "The session finished its response and is waiting for user input. \
             Should a message be sent (e.g. 'continue'), or should the session be left alone? \
             Respond with JSON: {\"action\": \"send\"|\"deny\", \
             \"message\": \"...\", \"reasoning\": \"...\", \"confidence\": 0.0-1.0}"
                .to_string()
        }
        _ => "The session is in an unexpected state. Respond with JSON: \
             {\"action\": \"deny\", \"reasoning\": \"...\", \"confidence\": 0.0}"
            .to_string(),
    }
}

/// Format a compact map of all sessions (public, for orchestration prompts).
pub fn format_global_session_map_public(sessions: &[ClaudeSession]) -> String {
    format_global_session_map(0, sessions)
}

/// Format a compact map of all active sessions for cross-session awareness.
fn format_global_session_map(current_pid: u32, sessions: &[ClaudeSession]) -> String {
    if sessions.len() <= 1 {
        return String::new();
    }

    let mut lines = Vec::new();
    for s in sessions {
        let marker = if s.pid == current_pid {
            " ← evaluating"
        } else {
            ""
        };
        let ctx_pct = if s.context_max > 0 {
            (s.context_tokens as f64 / s.context_max as f64 * 100.0) as u32
        } else {
            0
        };

        let tool_info = match &s.pending_tool_name {
            Some(tool) => {
                let cmd = s
                    .pending_tool_input
                    .as_deref()
                    .map(|c| {
                        if c.len() > 60 {
                            format!(" \"{}...\"", session::truncate_str(c, 60))
                        } else {
                            format!(" \"{c}\"")
                        }
                    })
                    .unwrap_or_default();
                format!(" [{}{}]", tool, cmd)
            }
            None => String::new(),
        };

        lines.push(format!(
            "- {} [PID {}]: {}{} (${:.1}, {}% ctx){marker}",
            s.display_name(),
            s.pid,
            s.status,
            tool_info,
            s.cost_usd,
            ctx_pct,
        ));
    }

    lines.join("\n")
}

/// Read recent transcript entries from the JSONL file, compacted to fit budget.
/// Keeps the last N full messages and summarizes older ones as one-liners.
fn read_recent_transcript(session: &ClaudeSession, max_tokens: u32) -> String {
    let Some(ref jsonl_path) = session.jsonl_path else {
        return "(no transcript available)".into();
    };

    let entries = read_all_transcript_entries(jsonl_path);
    if entries.is_empty() {
        return "(empty transcript)".into();
    }

    // Rough token estimate: 1 token ≈ 4 chars
    let max_chars = (max_tokens as usize) * 4;
    let mut lines: Vec<String> = Vec::new();

    // Process entries newest-first, keep full detail for recent ones
    let total = entries.len();
    for (i, entry) in entries.iter().enumerate().rev() {
        let is_recent = total - i <= 8; // Last 8 messages get full detail
        let line = if is_recent {
            format_entry_full(entry)
        } else {
            format_entry_compact(entry)
        };
        lines.push(line);
    }

    lines.reverse();

    // Truncate to fit budget
    let mut result = String::new();
    for line in &lines {
        if result.len() + line.len() > max_chars {
            result.push_str("\n... (earlier messages truncated)");
            break;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line);
    }

    result
}

struct TranscriptEntry {
    role: String,
    blocks: Vec<TranscriptBlock>,
}

fn read_all_transcript_entries(path: &Path) -> Vec<TranscriptEntry> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let mut entries = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }

        if let Some(event) = transcript::parse_line(&line) {
            match event {
                TranscriptEvent::Message(msg) => {
                    let role = match msg.role {
                        transcript::TranscriptRole::Assistant => "assistant".into(),
                        transcript::TranscriptRole::User => "user".into(),
                    };
                    entries.push(TranscriptEntry {
                        role,
                        blocks: msg.content,
                    });
                }
                TranscriptEvent::WaitingForTask => {
                    entries.push(TranscriptEntry {
                        role: "system".into(),
                        blocks: vec![TranscriptBlock::Text("[waiting for user input]".into())],
                    });
                }
            }
        }
    }

    entries
}

fn format_entry_full(entry: &TranscriptEntry) -> String {
    let mut parts = Vec::new();
    for block in &entry.blocks {
        match block {
            TranscriptBlock::Text(text) => {
                let truncated = if text.len() > 500 {
                    format!("{}...", session::truncate_str(text, 500))
                } else {
                    text.clone()
                };
                parts.push(truncated);
            }
            TranscriptBlock::ToolUse { name, input } => {
                let input_str = if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                    let truncated = if cmd.len() > 200 {
                        format!("{}...", session::truncate_str(cmd, 200))
                    } else {
                        cmd.to_string()
                    };
                    format!("({})", truncated)
                } else {
                    String::new()
                };
                parts.push(format!("[tool_use: {name}{input_str}]"));
            }
            TranscriptBlock::ToolResult { content, is_error } => {
                let prefix = if *is_error { "ERROR: " } else { "" };
                let truncated = if content.len() > 300 {
                    format!("{}...", session::truncate_str(content, 300))
                } else {
                    content.clone()
                };
                parts.push(format!("[tool_result: {prefix}{truncated}]"));
            }
        }
    }

    format!("[{}] {}", entry.role, parts.join(" "))
}

fn format_entry_compact(entry: &TranscriptEntry) -> String {
    let mut summary_parts = Vec::new();
    for block in &entry.blocks {
        match block {
            TranscriptBlock::Text(t) => {
                let preview = if t.len() > 60 {
                    format!("{}...", session::truncate_str(t, 60))
                } else {
                    t.clone()
                };
                summary_parts.push(format!("\"{}\"", preview));
            }
            TranscriptBlock::ToolUse { name, .. } => {
                summary_parts.push(format!("called {name}"));
            }
            TranscriptBlock::ToolResult { is_error, .. } => {
                if *is_error {
                    summary_parts.push("(error)".into());
                }
            }
        }
    }

    format!("[{}] {}", entry.role, summary_parts.join(", "))
}

// ────────────────────────────────────────────────────────────────────────────
// Git context
// ────────────────────────────────────────────────────────────────────────────

static GIT_CACHE: Mutex<Option<HashMap<String, (Instant, String)>>> = Mutex::new(None);
const GIT_CACHE_TTL: Duration = Duration::from_secs(30);

/// Build a compact git state summary for the session's CWD. Cached per-CWD with 30s TTL.
fn build_git_context(cwd: &str) -> String {
    if let Ok(mut guard) = GIT_CACHE.lock() {
        let cache = guard.get_or_insert_with(HashMap::new);
        if let Some((ts, cached)) = cache.get(cwd) {
            if ts.elapsed() < GIT_CACHE_TTL {
                return cached.clone();
            }
        }
    }

    let result = build_git_context_uncached(cwd);

    if let Ok(mut guard) = GIT_CACHE.lock() {
        let cache = guard.get_or_insert_with(HashMap::new);
        cache.insert(cwd.to_string(), (Instant::now(), result.clone()));
    }

    result
}

fn build_git_context_uncached(cwd: &str) -> String {
    // Check if we're in a git repo at all
    let is_git = run_git_cmd(cwd, &["rev-parse", "--is-inside-work-tree"]);
    if is_git.as_deref() != Some("true") {
        return String::new();
    }

    let mut lines = Vec::new();

    // Branch name (may be empty in detached HEAD, e.g. CI checkouts)
    let branch = run_git_cmd(cwd, &["branch", "--show-current"]).unwrap_or_default();
    if !branch.is_empty() {
        lines.push(format!("Branch: {branch}"));
    } else if let Some(rev) = run_git_cmd(cwd, &["rev-parse", "--short", "HEAD"]) {
        lines.push(format!("HEAD: {rev} (detached)"));
    }

    if let Some(status) = run_git_cmd(cwd, &["status", "--short"]) {
        let file_count = status.lines().count();
        if file_count > 0 {
            lines.push(format!("Uncommitted: {file_count} files changed"));
        }
    }

    if let Some(diff) = run_git_cmd(cwd, &["diff", "--stat", "--stat-width=60"]) {
        let last_line = diff.lines().last().unwrap_or("");
        if last_line.contains("changed") {
            lines.push(format!("Diff: {}", last_line.trim()));
        }
    }

    if let Some(log) = run_git_cmd(cwd, &["log", "--oneline", "-3"]) {
        if !log.is_empty() {
            lines.push("Recent commits:".to_string());
            for commit in log.lines().take(3) {
                lines.push(format!("  {commit}"));
            }
        }
    }

    if lines.is_empty() || (lines.len() == 1 && !lines[0].contains("Uncommitted")) {
        return String::new(); // No useful git state
    }

    format!("Git state:\n  {}", lines.join("\n  "))
}

fn run_git_cmd(cwd: &str, args: &[&str]) -> Option<String> {
    let child = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Format the full brain prompt by combining summary, transcript, and decision prompt.
/// Uses the prompt library (user override or built-in template).
///
/// Context budget strategy for small LLMs (Gemma4):
/// - If distilled preferences exist, use the compact summary (~200 tokens)
///   instead of raw few-shot examples (~500+ tokens)
/// - If both exist and context is generous, include both
/// - Preference summary always takes priority since it's pre-distilled
pub fn format_brain_prompt(ctx: &BrainContext) -> String {
    // Build the learning context section: prefer compact preferences,
    // fall back to raw few-shot, or use both if context budget allows
    let learning_section =
        if !ctx.preference_summary.is_empty() && !ctx.few_shot_examples.is_empty() {
            // Both available: preferences are compact, include both
            format!(
                "\n\n## Learned Preferences\n{}\n\n## Recent Examples\n{}",
                ctx.preference_summary, ctx.few_shot_examples,
            )
        } else if !ctx.preference_summary.is_empty() {
            format!("\n\n## Learned Preferences\n{}", ctx.preference_summary,)
        } else if !ctx.few_shot_examples.is_empty() {
            format!(
                "\n\n## Past Decisions (learn from these)\n{}",
                ctx.few_shot_examples,
            )
        } else {
            String::new()
        };

    let git_section = if ctx.git_context.is_empty() {
        String::new()
    } else {
        format!("\n\n## Repository State\n{}", ctx.git_context)
    };

    let global_map = if ctx.global_session_map.is_empty() {
        String::new()
    } else {
        format!("\n\n## All Active Sessions\n{}", ctx.global_session_map)
    };

    let coord_section = if ctx.coordination_context.is_empty() {
        String::new()
    } else {
        format!("\n\n## Coordination Context\n{}", ctx.coordination_context)
    };

    let template = super::prompts::load(super::prompts::ADVISORY);
    super::prompts::expand(
        &template,
        &[
            ("session_summary", &ctx.session_summary),
            ("git_context", &git_section),
            ("global_session_map", &global_map),
            ("coordination_context", &coord_section),
            ("recent_transcript", &ctx.recent_transcript),
            ("few_shot_examples", &learning_section),
            ("decision_prompt", &ctx.decision_prompt),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ClaudeSession, RawSession, SessionStatus, TelemetryStatus};

    fn make_session() -> ClaudeSession {
        let raw = RawSession {
            pid: 100,
            session_id: "test".into(),
            cwd: "/tmp/my-project".into(),
            started_at: 0,
        };
        let mut s = ClaudeSession::from_raw(raw);
        s.status = SessionStatus::NeedsInput;
        s.telemetry_status = TelemetryStatus::Available;
        s.model = "opus-4.6".into();
        s.cost_usd = 12.50;
        s.context_tokens = 50000;
        s.context_max = 200000;
        s.pending_tool_name = Some("Bash".into());
        s.pending_tool_input = Some("cargo test --release".into());
        s
    }

    #[test]
    fn session_summary_includes_key_fields() {
        let s = make_session();
        let summary = format_session_summary(&s);
        assert!(summary.contains("my-project"));
        assert!(summary.contains("Needs Input"));
        assert!(summary.contains("opus-4.6"));
        assert!(summary.contains("$12.50"));
        assert!(summary.contains("25%"));
        assert!(summary.contains("Bash"));
        assert!(summary.contains("cargo test --release"));
    }

    #[test]
    fn session_summary_shows_error_flag() {
        let mut s = make_session();
        s.last_tool_error = true;
        let summary = format_session_summary(&s);
        assert!(summary.contains("ERRORED"));
    }

    #[test]
    fn decision_prompt_for_needs_input() {
        let s = make_session();
        let prompt = format_decision_prompt(&s);
        assert!(prompt.contains("approval"));
        assert!(prompt.contains("Bash"));
        assert!(prompt.contains("approve"));
    }

    #[test]
    fn decision_prompt_for_waiting_input() {
        let mut s = make_session();
        s.status = SessionStatus::WaitingInput;
        let prompt = format_decision_prompt(&s);
        assert!(prompt.contains("waiting for user input"));
        assert!(prompt.contains("continue"));
    }

    #[test]
    fn context_with_no_jsonl_path() {
        let s = make_session();
        let ctx = build_context(&s, std::slice::from_ref(&s), 4000);
        assert!(ctx.recent_transcript.contains("no transcript"));
    }

    #[test]
    fn context_with_jsonl_file() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("test.jsonl");
        std::fs::write(
            &jsonl,
            concat!(
                r#"{"type":"assistant","message":{"role":"assistant","model":"opus","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file1.rs\nfile2.rs"}],"usage":{"input_tokens":50,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
            ),
        )
        .unwrap();

        let mut s = make_session();
        s.jsonl_path = Some(jsonl);

        let ctx = build_context(&s, std::slice::from_ref(&s), 4000);
        assert!(ctx.recent_transcript.contains("Bash"));
        assert!(ctx.recent_transcript.contains("file1.rs"));
        assert!(!ctx.session_summary.is_empty());
        assert!(!ctx.decision_prompt.is_empty());
    }

    #[test]
    fn brain_prompt_combines_all_sections() {
        let ctx = BrainContext {
            session_summary: "summary".into(),
            recent_transcript: "transcript".into(),
            decision_prompt: "decide".into(),
            few_shot_examples: String::new(),
            preference_summary: String::new(),
            global_session_map: String::new(),
            git_context: String::new(),
            coordination_context: String::new(),
        };
        let prompt = format_brain_prompt(&ctx);
        assert!(prompt.contains("summary"));
        assert!(prompt.contains("transcript"));
        assert!(prompt.contains("decide"));
    }

    #[test]
    fn global_session_map_single_session_empty() {
        let s = make_session();
        let map = format_global_session_map(s.pid, &[s]);
        assert!(map.is_empty());
    }

    #[test]
    fn global_session_map_multiple_sessions() {
        let s1 = make_session();
        let mut s2 = make_session();
        s2.pid = 200;
        s2.project_name = "other-project".into();
        s2.status = SessionStatus::Processing;
        s2.cost_usd = 3.0;

        let map = format_global_session_map(s1.pid, &[s1, s2]);
        assert!(map.contains("my-project"));
        assert!(map.contains("other-project"));
        assert!(map.contains("← evaluating"));
        assert!(map.contains("Processing"));
    }

    #[test]
    fn global_session_map_in_prompt() {
        let ctx = BrainContext {
            session_summary: "summary".into(),
            recent_transcript: "transcript".into(),
            decision_prompt: "decide".into(),
            few_shot_examples: String::new(),
            preference_summary: String::new(),
            global_session_map: "- session1: Processing\n- session2: Idle".into(),
            git_context: String::new(),
            coordination_context: String::new(),
        };
        let prompt = format_brain_prompt(&ctx);
        assert!(prompt.contains("All Active Sessions"));
        assert!(prompt.contains("session1: Processing"));
    }

    #[test]
    fn git_context_in_git_repo() {
        let cwd = env!("CARGO_MANIFEST_DIR");
        let ctx = build_git_context_uncached(cwd);
        // This project is a git repo — we should get either Branch or HEAD plus commits
        // In CI (detached HEAD), branch may be empty but HEAD + commits should exist
        assert!(
            ctx.contains("Branch:") || ctx.contains("HEAD:") || ctx.contains("Recent commits:"),
            "Expected git context in a git repo, got: {ctx:?}"
        );
    }

    #[test]
    fn git_context_empty_for_non_git() {
        let ctx = build_git_context("/tmp");
        assert!(ctx.is_empty());
    }

    #[test]
    fn git_context_in_prompt_when_present() {
        let ctx = BrainContext {
            session_summary: "summary".into(),
            recent_transcript: "transcript".into(),
            decision_prompt: "decide".into(),
            few_shot_examples: String::new(),
            preference_summary: String::new(),
            global_session_map: String::new(),
            git_context: "Git state:\n  Branch: main\n  Uncommitted: 3 files".into(),
            coordination_context: String::new(),
        };
        let prompt = format_brain_prompt(&ctx);
        assert!(prompt.contains("Repository State"));
        assert!(prompt.contains("Branch: main"));
    }

    #[test]
    fn git_context_omitted_when_empty() {
        let ctx = BrainContext {
            session_summary: "summary".into(),
            recent_transcript: "transcript".into(),
            decision_prompt: "decide".into(),
            few_shot_examples: String::new(),
            preference_summary: String::new(),
            global_session_map: String::new(),
            git_context: String::new(),
            coordination_context: String::new(),
        };
        let prompt = format_brain_prompt(&ctx);
        assert!(!prompt.contains("Repository State"));
    }
}

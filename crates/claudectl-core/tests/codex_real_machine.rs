//! Reads the *real* `~/.codex` on the machine running the tests (#524).
//!
//! Every assertion is conditional on there being something to read, because
//! CI has no Codex install — so this is a smoke test that catches a schema
//! drift on a developer machine, not a gate. It exists because the rollout
//! schema is someone else's and will move: when it does, this is the test
//! that says so, where a fixture-only suite would stay green.
//!
//! Run it with output to see what it found:
//!   cargo test -p claudectl-core --test codex_real_machine -- --nocapture

use claudectl_core::agent::codex;

/// How far back to read. See `real_rollouts_still_parse` for why.
const WINDOW_SECS: u64 = 90 * 24 * 3600;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[test]
fn real_rollouts_still_parse() {
    let root = codex::sessions_root();
    if !root.is_dir() {
        eprintln!("skipped: {} does not exist", root.display());
        return;
    }

    // A quarter of history, so there is something to parse even when no
    // session has run recently, without reading a year of transcripts —
    // 365 days took 31s on the reference machine, 90 takes a fraction of
    // that. Recent files are also the ones on the *current* schema, which is
    // what a drift check wants. The scanner itself uses
    // CODEX_SESSION_TTL_SECS.
    let summaries = codex::scan(now_secs(), WINDOW_SECS);
    if summaries.is_empty() {
        eprintln!("skipped: no rollouts under {}", root.display());
        return;
    }

    eprintln!(
        "parsed {} rollouts from {}",
        summaries.len(),
        root.display()
    );

    let mut with_cwd = 0;
    let mut with_window = 0;
    let mut with_tokens = 0;
    let mut with_tool = 0;

    for s in &summaries {
        assert!(
            !s.session_id.is_empty(),
            "every parsed rollout must have an id; {} did not",
            s.path.display()
        );
        assert!(
            s.last_event_secs >= s.started_at_secs,
            "last event precedes first in {}",
            s.path.display()
        );
        if !s.cwd.is_empty() {
            with_cwd += 1;
        }
        if s.context_window.is_some() {
            with_window += 1;
        }
        if s.total_tokens > 0 {
            with_tokens += 1;
        }
        if s.last_tool.is_some() {
            with_tool += 1;
        }
    }

    let n = summaries.len();
    eprintln!("  cwd: {with_cwd}/{n}  context_window: {with_window}/{n}");
    eprintln!("  tokens: {with_tokens}/{n}  last_tool: {with_tool}/{n}");

    // The fields the session view depends on. A schema rename would drop
    // these to zero while every fixture test stayed green, which is the
    // whole point of reading the real directory.
    assert!(
        with_cwd > 0,
        "no rollout yielded a cwd — `session_meta.cwd` may have been renamed"
    );
    assert!(
        with_tokens > 0,
        "no rollout yielded token usage — `token_count.total_token_usage` may have moved"
    );
}

#[test]
fn real_tool_names_normalize_to_the_canonical_vocabulary() {
    use claudectl_core::agent::vocab;

    if !codex::sessions_root().is_dir() {
        eprintln!("skipped: no ~/.codex/sessions");
        return;
    }
    let summaries = codex::scan(now_secs(), WINDOW_SECS);
    let tools: std::collections::BTreeSet<String> = summaries
        .iter()
        .filter_map(|s| s.last_tool.clone())
        .collect();
    if tools.is_empty() {
        eprintln!("skipped: no tool calls in any rollout");
        return;
    }
    eprintln!("canonical last-tool names seen: {tools:?}");

    for tool in &tools {
        // Whatever the name, it must be stable under re-normalization — the
        // property that lets a Codex preference share a key with a Claude one.
        let again = vocab::canonical_tool(vocab::CLAUDE_CODE, None, tool, None);
        assert_eq!(
            &again, tool,
            "a canonical name must survive re-normalization; '{tool}' did not"
        );
        // An unmapped Codex tool keeps its family prefix; a mapped one must
        // not still look like a Codex spelling.
        if !tool.starts_with("codex:") && !tool.starts_with("mcp__") {
            assert!(
                !tool.contains('/'),
                "a mapped name should carry no namespace separator: {tool}"
            );
        }
    }
}

#[test]
fn real_rollouts_become_sessions_that_cannot_be_acted_on() {
    use claudectl_core::session::ClaudeSession;

    if !codex::sessions_root().is_dir() {
        eprintln!("skipped: no ~/.codex/sessions");
        return;
    }
    let summaries = codex::scan(now_secs(), WINDOW_SECS);
    if summaries.is_empty() {
        eprintln!("skipped: no rollouts");
        return;
    }

    for summary in summaries.iter().take(50) {
        let session = ClaudeSession::from_codex_rollout(summary);
        assert!(
            !session.is_actionable(),
            "a real Codex session must never be actionable: {}",
            session.session_id
        );
        assert_eq!(session.pid, 0);
        assert!(!session.has_known_cost());
        assert!(
            session.jsonl_path.is_some(),
            "the rollout is the transcript, so the path must be set"
        );
    }
}

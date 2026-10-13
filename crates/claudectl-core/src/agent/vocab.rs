//! Canonical tool vocabulary — one name per operation, across agent families.
//!
//! Why this exists: `hive::semantic_key` keys a distilled preference on the
//! tool name (`pattern:{tool}:{cmd}`), and that key is the cross-peer merge
//! key. Two agents that call the same operation by different names therefore
//! produce two units that never merge, each holding half the evidence and
//! decaying on its own clock — which makes "learns from you once and
//! remembers everywhere" false in exactly the case the hive exists for.
//!
//! The gossip protocol carries no version, so the key's derivation cannot
//! change without silently splitting a hive between old and new binaries.
//! That rules out putting the agent family *in* the key. Instead foreign
//! names normalize **into** Claude Code's vocabulary, which is the incumbent
//! form already on the wire in every existing hive. This is not a claim that
//! those are better names; it is that renaming them is a migration.
//!
//! ## The conservative rule
//!
//! A tool is mapped only when it is genuinely the *same operation* and its
//! command payload is *comparable*. Everything else keeps a family-prefixed
//! name (`codex:exec`) and merges with nothing.
//!
//! This is deliberate: a false merge corrupts a unit that other machines
//! rely on, while a missed merge only leaves knowledge local — recoverable
//! later by extending the table. So when in doubt, do not map.
//!
//! Three Codex tools fail that test and are deliberately *not* mapped to
//! `Bash`, despite all three being shell-adjacent:
//!
//! * `exec` — a JavaScript sandbox (`input` is JS calling `tools.*`), not a
//!   shell. Mapping it would file JS snippets under the same key as shell
//!   commands.
//! * `write_stdin` — sends keystrokes to an already-running session. Its
//!   payload is `chars`, not a command, and Claude Code's `Bash` has no
//!   equivalent.
//! * `collaboration/{send_message,wait_agent,list_agents}` — messaging and
//!   waiting, not delegation.

/// Agent family for Claude Code. Its tool names are the canonical form.
pub const CLAUDE_CODE: &str = "claude-code";
/// Agent family for OpenAI Codex CLI.
pub const CODEX: &str = "codex";

/// Prefix every MCP tool name carries, in both agents' spellings.
const MCP_PREFIX: &str = "mcp__";

/// Markers in a Codex `apply_patch` envelope. Order matters: `Add` outranks
/// `Update`, because a patch that creates a file is the higher-privilege
/// operation and a preference about it must not be filed under `Edit`.
const PATCH_ADD: &str = "*** Add File";
const PATCH_UPDATE: &str = "*** Update File";
const PATCH_DELETE: &str = "*** Delete File";

/// The canonical name for a tool call, given the family that made it.
///
/// `namespace` is Codex's separate MCP namespace field (`None` for Claude
/// Code, which folds it into the name). `payload` is the call's raw argument
/// blob, used by exactly one mapping — `apply_patch`, which needs its
/// envelope marker to tell `Write` from `Edit`. Every other mapping is a
/// function of the family, namespace and name alone.
///
/// An empty or unrecognised `family` passes the name through unchanged.
/// That is not laziness: every decision record written before this module
/// existed has no family, is a Claude Code record, and must keep producing
/// the key it already produced in every hive on the network.
pub fn canonical_tool(
    family: &str,
    namespace: Option<&str>,
    name: &str,
    payload: Option<&str>,
) -> String {
    // MCP tools are the same tool on the same server in either agent, so
    // they resolve before any family table.
    if let Some(mcp) = canonical_mcp(namespace, name) {
        return mcp;
    }

    match family {
        CODEX => canonical_codex(namespace, name, payload),
        // Claude Code is canonical by definition. An unknown-but-named
        // family gets prefixed so it cannot falsely merge with a Claude
        // name it happens to share; an absent family is a legacy record.
        CLAUDE_CODE | "" => name.to_string(),
        other => qualify(other, namespace, name),
    }
}

/// The canonical name for an MCP tool call, or `None` if this is not one.
///
/// Codex emits MCP calls in two shapes and the common one is already the
/// Claude Code spelling byte-for-byte:
///
/// | `namespace` | `name` | canonical |
/// |---|---|---|
/// | *absent* | `mcp__chrome-devtools__click` | unchanged |
/// | `mcp__cua_repl` | `js` | `mcp__cua_repl__js` |
///
/// The namespace already carries the `mcp__` prefix, so the second case is a
/// join rather than a reconstruction.
///
/// There is deliberately no inverse. A server segment may itself contain
/// both `_` and `-` (`mcp__chrome-devtools__new_page`,
/// `mcp__claude_ai_Claude_Docs__batch`), so `(server, tool)` cannot be
/// recovered from the joined form — normalization only ever runs forward,
/// from the agent's own explicit fields.
fn canonical_mcp(namespace: Option<&str>, name: &str) -> Option<String> {
    if name.starts_with(MCP_PREFIX) {
        return Some(name.to_string());
    }
    let ns = namespace?;
    if ns.starts_with(MCP_PREFIX) {
        return Some(format!("{ns}__{name}"));
    }
    None
}

fn canonical_codex(namespace: Option<&str>, name: &str, payload: Option<&str>) -> String {
    // A caller that has only one string to hand — a decision record, which
    // stores a single `tool` field — may pass the namespaced form joined with
    // `/`, the way Codex's own tool listings render it. Split it back so both
    // call shapes reach the same arm. MCP names join with `__` and are already
    // resolved above, so they cannot be caught here.
    if namespace.is_none() {
        if let Some((ns, rest)) = name.split_once('/') {
            return canonical_codex(Some(ns), rest, payload);
        }
    }

    // A namespaced non-MCP tool is addressed by its pair, not its bare name:
    // `collaboration/spawn_agent` is a different tool from `spawn_agent`.
    if let Some(ns) = namespace {
        return match (ns, name) {
            // Delegation: spawning work and queuing follow-up work are both
            // what Claude Code's `Task` does.
            ("collaboration", "spawn_agent" | "followup_task") => "Task".to_string(),
            // `web/run` is Codex's single web tool; Claude Code splits search
            // from fetch. Search is the dominant use, and a missed merge on
            // fetch costs less than filing fetches under search would.
            ("web", "run") => "WebSearch".to_string(),
            // Everything else — `clock/sleep`, `collaboration/send_message`,
            // `collaboration/wait_agent` — has no counterpart worth merging.
            _ => qualify(CODEX, Some(ns), name),
        };
    }

    match name {
        // Both carry a shell command string: `{"cmd": "ls"}` and
        // `{"command": "ls", "workdir": …}` respectively.
        "exec_command" | "shell_command" => "Bash".to_string(),
        // The envelope says whether this creates or modifies.
        "apply_patch" => canonical_apply_patch(payload),
        "update_plan" => "TodoWrite".to_string(),
        "spawn_agent" => "Task".to_string(),
        "request_user_input_async" => "AskUserQuestion".to_string(),
        _ => qualify(CODEX, None, name),
    }
}

/// `Write` for a patch that adds a file, `Edit` for one that modifies, and a
/// family-prefixed name for a deletion, which merges with nothing: there is
/// no Claude Code tool for deleting a file, so there is no name to merge
/// *into*. Note this also means it will not merge with a Codex
/// `exec_command rm`, which is arguably the same operation — accepted under
/// the conservative rule above, since the alternative is filing deletions
/// under `Bash` alongside every other shell command.
///
/// Falls back to `Edit` when the payload is absent or carries no marker:
/// modification is overwhelmingly the common case (1934 of 2134 observed
/// envelopes), and `Edit` is the lower-privilege of the two names, so an
/// unparseable patch errs toward the weaker claim.
fn canonical_apply_patch(payload: Option<&str>) -> String {
    let Some(blob) = payload else {
        return "Edit".to_string();
    };
    if blob.contains(PATCH_ADD) {
        return "Write".to_string();
    }
    if blob.contains(PATCH_UPDATE) {
        return "Edit".to_string();
    }
    if blob.contains(PATCH_DELETE) {
        return format!("{CODEX}:apply_patch_delete");
    }
    "Edit".to_string()
}

/// A name that merges with nothing, because it has no counterpart.
fn qualify(family: &str, namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) => format!("{family}:{ns}/{name}"),
        None => format!("{family}:{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Claude Code passes through, including every legacy shape ──────────

    #[test]
    fn claude_code_names_are_unchanged() {
        for name in ["Bash", "Write", "Edit", "Read", "Task", "TodoWrite", "*"] {
            assert_eq!(
                canonical_tool(CLAUDE_CODE, None, name, None),
                name,
                "Claude Code is the canonical vocabulary; '{name}' must not be rewritten"
            );
        }
    }

    /// Every decision record written before this module existed carries no
    /// family. Those records are Claude Code records and must keep producing
    /// the key they already produce in every hive on the network.
    #[test]
    fn absent_family_passes_through() {
        assert_eq!(canonical_tool("", None, "Bash", None), "Bash");
        assert_eq!(canonical_tool("", None, "Edit", None), "Edit");
    }

    /// A *named* family with no table is prefixed rather than passed through,
    /// so it cannot falsely merge with a Claude Code name it happens to share.
    #[test]
    fn unknown_named_family_is_qualified() {
        assert_eq!(canonical_tool("aider", None, "Bash", None), "aider:Bash");
    }

    // ── The shell family ──────────────────────────────────────────────────

    #[test]
    fn codex_shell_tools_become_bash() {
        assert_eq!(canonical_tool(CODEX, None, "exec_command", None), "Bash");
        assert_eq!(canonical_tool(CODEX, None, "shell_command", None), "Bash");
    }

    /// The whole point of the module: the same preference, learned in two
    /// agents, has to produce one name and therefore one `semantic_key`.
    #[test]
    fn codex_shell_call_agrees_with_claude_bash() {
        assert_eq!(
            canonical_tool(CODEX, None, "exec_command", Some(r#"{"cmd":"cargo test"}"#)),
            canonical_tool(CLAUDE_CODE, None, "Bash", Some("cargo test")),
        );
    }

    /// `exec` is a JavaScript sandbox, not a shell — its `input` is JS calling
    /// `tools.*`. Mapping it to `Bash` would file JS snippets under the same
    /// key as shell commands.
    #[test]
    fn codex_exec_is_not_bash() {
        assert_eq!(canonical_tool(CODEX, None, "exec", None), "codex:exec");
    }

    /// `write_stdin` sends keystrokes to a running session; its payload is
    /// `chars`, not a command, and `Bash` has no equivalent.
    #[test]
    fn codex_write_stdin_is_not_bash() {
        assert_eq!(
            canonical_tool(CODEX, None, "write_stdin", None),
            "codex:write_stdin"
        );
    }

    // ── apply_patch splits by envelope marker ─────────────────────────────

    #[test]
    fn apply_patch_add_file_is_write() {
        let patch = "*** Begin Patch\n*** Add File: AGENTS.md\n+# Repository Guidelines\n";
        assert_eq!(
            canonical_tool(CODEX, None, "apply_patch", Some(patch)),
            "Write"
        );
    }

    #[test]
    fn apply_patch_update_file_is_edit() {
        let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n";
        assert_eq!(
            canonical_tool(CODEX, None, "apply_patch", Some(patch)),
            "Edit"
        );
    }

    /// Deletion has no Claude Code tool analogue — it happens through
    /// `Bash rm` — so it merges with nothing rather than with `Edit`.
    #[test]
    fn apply_patch_delete_file_merges_with_nothing() {
        let patch = "*** Begin Patch\n*** Delete File: stale.rs\n";
        assert_eq!(
            canonical_tool(CODEX, None, "apply_patch", Some(patch)),
            "codex:apply_patch_delete"
        );
    }

    /// A patch touching several files can carry several markers. Creating a
    /// file is the higher-privilege operation, so `Write` wins.
    #[test]
    fn apply_patch_mixed_markers_prefer_write() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n@@\n*** Add File: b.rs\n+new\n";
        assert_eq!(
            canonical_tool(CODEX, None, "apply_patch", Some(patch)),
            "Write"
        );
    }

    /// An unparseable or absent payload errs toward the weaker claim.
    #[test]
    fn apply_patch_without_marker_is_edit() {
        assert_eq!(canonical_tool(CODEX, None, "apply_patch", None), "Edit");
        assert_eq!(
            canonical_tool(CODEX, None, "apply_patch", Some("garbage")),
            "Edit"
        );
    }

    // ── MCP: mostly pass-through, never reconstructed ─────────────────────

    /// The dominant Codex shape already *is* the Claude Code spelling.
    #[test]
    fn mcp_name_already_canonical_passes_through() {
        assert_eq!(
            canonical_tool(CODEX, None, "mcp__chrome-devtools__click", None),
            "mcp__chrome-devtools__click"
        );
    }

    /// The split shape needs one join; the namespace already carries `mcp__`.
    #[test]
    fn mcp_split_shape_is_joined() {
        assert_eq!(
            canonical_tool(CODEX, Some("mcp__cua_repl"), "js", None),
            "mcp__cua_repl__js"
        );
    }

    /// An MCP preference learned in either agent must land on one key.
    #[test]
    fn mcp_call_agrees_across_agents() {
        assert_eq!(
            canonical_tool(CODEX, Some("mcp__context7"), "query_docs", None),
            canonical_tool(CLAUDE_CODE, None, "mcp__context7__query_docs", None),
        );
    }

    /// A server segment can contain both `_` and `-`, so the joined form is
    /// not splittable. Re-normalizing an already-canonical name must be a
    /// no-op rather than an attempt at the inverse.
    #[test]
    fn mcp_server_with_mixed_separators_survives_renormalization() {
        for name in [
            "mcp__chrome-devtools__new_page",
            "mcp__claude_ai_Claude_Docs__batch",
            "mcp__Parallel-search-mcp__web_fetch",
        ] {
            let once = canonical_tool(CODEX, None, name, None);
            assert_eq!(once, name);
            assert_eq!(
                canonical_tool(CLAUDE_CODE, None, &once, None),
                name,
                "normalization must be idempotent and never attempt the inverse"
            );
        }
    }

    /// An MCP call resolves before any family table, so a family we have no
    /// table for still gets its MCP tools right.
    #[test]
    fn mcp_resolves_before_the_family_table() {
        assert_eq!(
            canonical_tool("some-future-agent", None, "mcp__context7__query_docs", None),
            "mcp__context7__query_docs"
        );
    }

    // ── The remaining mapped tools ────────────────────────────────────────

    #[test]
    fn codex_update_plan_is_todowrite() {
        assert_eq!(
            canonical_tool(CODEX, None, "update_plan", None),
            "TodoWrite"
        );
    }

    #[test]
    fn codex_delegation_becomes_task() {
        assert_eq!(canonical_tool(CODEX, None, "spawn_agent", None), "Task");
        assert_eq!(
            canonical_tool(CODEX, Some("collaboration"), "spawn_agent", None),
            "Task"
        );
        assert_eq!(
            canonical_tool(CODEX, Some("collaboration"), "followup_task", None),
            "Task"
        );
    }

    /// Messaging and waiting are not delegation, so they are not `Task`.
    #[test]
    fn codex_collaboration_messaging_is_not_task() {
        assert_eq!(
            canonical_tool(CODEX, Some("collaboration"), "send_message", None),
            "codex:collaboration/send_message"
        );
        assert_eq!(
            canonical_tool(CODEX, Some("collaboration"), "wait_agent", None),
            "codex:collaboration/wait_agent"
        );
    }

    #[test]
    fn codex_user_input_is_askuserquestion() {
        assert_eq!(
            canonical_tool(CODEX, None, "request_user_input_async", None),
            "AskUserQuestion"
        );
    }

    #[test]
    fn codex_web_run_is_websearch() {
        assert_eq!(canonical_tool(CODEX, Some("web"), "run", None), "WebSearch");
    }

    /// Codex's waiting primitives have no Claude Code counterpart.
    #[test]
    fn codex_wait_primitives_merge_with_nothing() {
        assert_eq!(canonical_tool(CODEX, None, "wait", None), "codex:wait");
        assert_eq!(
            canonical_tool(CODEX, Some("clock"), "sleep", None),
            "codex:clock/sleep"
        );
    }

    /// A namespaced tool is addressed by its pair: `collaboration/spawn_agent`
    /// and a bare `spawn_agent` both map to `Task`, but an unmapped namespaced
    /// tool must not collapse onto its bare name.
    #[test]
    fn namespace_is_part_of_the_identity() {
        assert_ne!(
            canonical_tool(CODEX, Some("somens"), "thing", None),
            canonical_tool(CODEX, None, "thing", None),
        );
    }

    /// A caller holding one string — a decision record stores a single `tool`
    /// field — may pass the namespaced form joined with `/`. Both call shapes
    /// must reach the same answer, or the seam and the ingest would disagree.
    #[test]
    fn prejoined_namespace_matches_the_split_form() {
        for (ns, name) in [
            ("collaboration", "spawn_agent"),
            ("collaboration", "send_message"),
            ("web", "run"),
            ("clock", "sleep"),
        ] {
            assert_eq!(
                canonical_tool(CODEX, None, &format!("{ns}/{name}"), None),
                canonical_tool(CODEX, Some(ns), name, None),
                "pre-joined '{ns}/{name}' must match the split form"
            );
        }
    }

    /// Normalizing an already-canonical result must not change it again.
    #[test]
    fn normalization_is_idempotent() {
        for (ns, name) in [
            (None, "exec_command"),
            (None, "apply_patch"),
            (None, "update_plan"),
            (Some("mcp__cua_repl"), "js"),
            (Some("collaboration"), "spawn_agent"),
        ] {
            let once = canonical_tool(CODEX, ns, name, Some("*** Update File: x"));
            let twice = canonical_tool(CLAUDE_CODE, None, &once, None);
            assert_eq!(once, twice, "normalizing {name} twice changed the result");
        }
    }
}

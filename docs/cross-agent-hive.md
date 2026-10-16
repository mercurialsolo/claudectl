# Cross-Agent Hive — Design Specification

**Status:** Epic #521, phases #522–#528. Phases 1 (PR #530), 2 (PR #531) and 3 (PR #532) shipped; the rest proposed. The adapter-registry half of phase 3 was split out as #533. Written against the code as of `bc61737d` (v0.79.0), and against Codex CLI 0.162.1 as installed on a real machine — every claim about Codex's on-disk shape in §9 was measured, not inferred.
**Scope:** Let coding agents that are not Claude Code — starting with Codex — join a hive as full members: contribute what they learn, receive what others learned, and talk to every other member over one bus. Today the hive's substrate is agent-neutral but its two ends are not: capture happens through a Claude Code hook, and application happens into a Claude Code prompt.

## Implementation status

| Phase (§8) | Issue | Status | Module / artifact |
| --- | --- | --- | --- |
| 1. Canonical tool vocabulary | #522 | **Shipped** (PR #530) | `claudectl-core/src/agent/vocab.rs`, seam at `brain::preferences::canonical_tool_for` |
| 2. Unit provenance + artifact applicability | #523 | **Shipped** (PR #531) | `src/hive/mod.rs` (`origin_agents`, `ArtifactRequires::agents`), `hive/distiller.rs` (`OriginIndex`), `claudectl-core/src/agent/mod.rs` (presence detection) |
| 3. Codex discovery + monitor | #524 | **Shipped** (PR #532) | `claudectl-core/src/agent/codex.rs`, `session::ClaudeSession::{from_codex_rollout,is_actionable}` |
| 3b. Route everything through the adapter registry | #533 | **Not started** | `src/coord/adapter*.rs`, or `claudectl-core/src/runtime.rs` |
| 4. Hive read surface for any agent (MCP) | #525 | **Not started** | `src/hive/mcp.rs` (new), mirroring `src/query/mcp.rs` |
| 5. Codex write surface (AGENTS.md, rules) | #526 | **Not started** | `src/hive/project/` (new) |
| 6. Bus: agent + machine dimension | #527 | **Agent half shipped** (Codex `--self`, `roles.agent`); machine half not started | `src/bus/{store,roles,policy}.rs` |
| 7. Bus: relay bridge (cross-machine) | #528 | **Not started** | `src/bus/bridge.rs` (new), `src/relay/mod.rs` message type |

---

## 1. Motivation

The project's tagline is "learns from you once and remembers everywhere." *Everywhere* currently means every Claude Code session on every machine you have paired. It does not mean every agent you use.

That gap is not academic. On the machine this spec was written against there are 671 Codex rollout transcripts and a Codex approval store with 275 entries. Every approval in that store is a preference the user expressed — the exact raw material `brain/preferences.rs` distills into a `KnowledgeUnit` — and none of it reaches the hive. In the other direction, a preference the hive has learned with high confidence from a hundred Claude sessions has no way to reach a Codex session, which will ask about the same command again.

The two agents also share MCP servers, which is where this gets both worse and better than expected. Most of Codex's MCP calls already carry the exact Claude Code tool name (`mcp__chrome-devtools__click`), so those preferences would merge the moment either side contributed one — they simply never do, because Codex never contributes. The rest arrive split across two fields (`namespace: "mcp__cua_repl"`, `name: "js"`) and need one join to become the same key (§3.2). Either way it is the same tool on the same server, and the hive currently treats the two agents' experience of it as unrelated.

This spec covers three questions in order of how expensive they are to get wrong:

1. **How is knowledge represented** so that the same preference learned in two different agents merges into one unit? (§4 — the spine, and the only irreversible decision here.)
2. **How is knowledge captured** from an agent that has no PreToolUse hook? (§5)
3. **How is knowledge applied** to an agent that cannot be gated? (§6)

And one more that the user asked for directly, and that turns out to be mostly a plumbing problem rather than a design problem:

4. **How does every agent, on every machine, reach one bus?** (§7)

## 2. What already crosses the boundary

Most of the hive is agent-neutral, and it is worth being precise about that before describing what is not — the work here is smaller than "support another agent" sounds.

| Component | Agent-neutral? | Why |
| --- | --- | --- |
| `hive/store.rs`, `gossip.rs`, `merger.rs`, `trust.rs`, `membership.rs` | **Yes** | They move `KnowledgeUnit`s between peers. Nothing in them knows what a session is. |
| `KnowledgeScope` (`hive/mod.rs:384`) | **Yes** | Universal / Language / Project. No agent dimension is wanted (see §4). |
| `KnowledgeContent::{Pattern, Insight, Temporal, PromotedRule, ApproachOutcome, ApproachCluster}` | **Yes in shape** | `tool` is a free-form `String`; the rest is prose and statistics. Only the *vocabulary* is Claude-specific. |
| `KnowledgeContent::Skill` | **Yes, today** | Codex and Claude Code use the same `SKILL.md` contract — `---`-delimited frontmatter with `name` and `description`. Verified in §9.3. |
| `DecisionRecord` (`brain/decisions.rs:77`) | **Yes in shape** | `tool: Option<String>`, `command: Option<String>`, `project`, `user_action`. Nothing Claude-shaped. |
| `claudectl --brain-query --tool X --command Y --project Z` | **Yes** | A shell-callable decision endpoint. Any agent that can run a subprocess can already consult the brain. |
| `claudectl bus stdio` (`bus/mcp.rs`) | **Yes, today** | An MCP stdio server. Codex loads MCP servers from `config.toml`, so a Codex session can already `publish` and `read_inbox`. See §7.1. |

Two things that look like cross-agent support but are not:

- **`src/coord/adapter.rs`** has `AgentAdapter`, `AgentFamily::{ClaudeCode, Codex}`, and a stub `CodexAdapter`. It is the right seam and it is nearly dead: the only callers are `src/coord/cli.rs:1061` and `:1079`, both inside a `coord adapters` listing command. Nothing in discovery, monitor, brain, or hive routes through it.
- **`src/brain/agents.rs`** (`AgentConfig`, `[agents.codex]` in config, `run_agent`) is *delegate-to* Codex: shell out with a prompt, capture stdout. That is the opposite direction from observing a Codex session you did not start, and it is a different feature. Conflating the two is the most likely way to misread this spec.

## 3. The five couplings

### 3.1 Discovery

`claudectl-core/src/discovery.rs:6` hardcodes `~/.claude/sessions/*.json` (PID-named) and resolves transcripts under `~/.claude/projects/<slug>/<uuid>.jsonl`. `scan_subagents` hardcodes `/tmp/claude-{uid}`. The `ClaudeSession` type name is load-bearing across both other crates.

Codex's equivalent is richer than expected (§9.1): `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`, with `session_meta` carrying `session_id` and `cwd`, `event_msg/token_count` carrying `model_context_window` alongside full token usage, and `task_started` / `task_complete` / `turn_aborted` as status signals.

Phase 3 settled three things this section originally left open, and one of them contradicted the plan.

#### There is no process to discover

This spec and #524 both assumed Codex discovery would find processes and read their state. Measured: the ChatGPT desktop app runs Codex as **one** long-lived `codex … app-server` serving every session, alongside 27 Electron helpers. Nothing distinguishes which session a process owns. A terminal `codex` is its own process, but there is no general mapping.

So a Codex session is discovered from its transcript, `pid` is 0, and the session is **not actionable** — `ClaudeSession::is_actionable()`. This is a safety property, not a missing feature: offering "kill" on a Codex session and reaching the shared `app-server` would end every Codex session the user has open.

#### Liveness is a recency window, because there is no session-end event

`task_complete` ends a *turn*. A session the user closed is byte-identical to one sitting idle. `CODEX_SESSION_TTL_SECS` (30 minutes) is the whole liveness model, chosen from the measured distribution — 0 rollouts touched in the last hour, 1 in four, 25 in a week, 671 all time. It is bimodal, so the exact value barely matters. Its one cost: a session that crashed mid-turn reads as working until the window expires.

#### Cost is absent, not zero

**Codex rollouts carry no USD.** Codex is subscription-billed and records only tokens. `cost_usd` stays 0 with `usage_metrics_available` false and `has_known_cost()` false, so nothing can mistake "unknown" for "free". The existing cost sums are additive, so a 0 contributes nothing and no total is corrupted.

#### The type was not renamed

`ClaudeSession` keeps its name (§10 question 5, now closed). It is public API across all three crates, a Codex session fills the same fields and flows through the same views, and renaming buys nothing an `agent_family` field does not. Three predicates keep the distinctions from drifting: `is_claude_code()` for "where do this session's files live", `is_actionable()` for "may I touch it", `is_remote()` unchanged for "is this from a peer".

### 3.2 Tool vocabulary — the one that matters

`semantic_key` for a `Pattern` is `pattern:{tool}:{cmd}` (`hive/mod.rs:549-556`), and `tool` is literally `"Bash"`, `"Write"`, `"Edit"`. Codex names the same operations differently. The measured table (§9.2, from 15 000 tool calls across 120 rollouts):

| Codex `name` (and `namespace`) | Observed count | Claude Code equivalent |
| --- | --- | --- |
| `exec_command` | 7046 | `Bash` |
| `exec` | 3624 | *none* — a JS sandbox, see below |
| `write_stdin` | 1336 | *none* — keystrokes, see below |
| `apply_patch` | 1270 | `Write` or `Edit` — see below |
| `shell_command` | 671 | `Bash` |
| `update_plan` | 60 | `TodoWrite` |
| `spawn_agent`, `collaboration/{spawn_agent,followup_task}` | ~100 | `Task` |
| `collaboration/{send_message,wait_agent,list_agents}` | ~140 | *none* — messaging, not delegation |
| `request_user_input_async` | 31 | `AskUserQuestion` |
| `web/run` | 27 | `WebSearch` |
| `wait`, `clock/sleep` | 251 | *none* |

#### The rule: map only a genuinely identical operation

A tool is mapped only when it is the *same operation* **and** its command payload is *comparable*. Everything else keeps a family-prefixed name (`codex:exec`) and merges with nothing.

This is deliberately conservative. A false merge corrupts a unit other machines rely on; a missed merge only leaves knowledge local, and is recoverable later by extending the table. When in doubt, do not map.

Three Codex tools fail that test despite looking mappable, and reading their payloads is what settled it:

| Tool | Payload | Why not mapped |
| --- | --- | --- |
| `exec_command` | `{"cmd": "ls"}` | — mapped to `Bash`; a shell command string |
| `shell_command` | `{"command": "ls", "workdir": …}` | — mapped to `Bash`; same |
| `exec` | `const r = await tools.exec_command({…})` | A **JavaScript sandbox**, not a shell. Mapping it would file JS snippets under the same key as shell commands. |
| `write_stdin` | `{"session_id": 76793, "chars": ""}` | Sends keystrokes to an already-running session. Not a command, and `Bash` has no equivalent. |
| `collaboration/send_message`, `wait_agent` | — | Messaging and waiting, not delegation. Only `spawn_agent` and `followup_task` are what `Task` does. |

`web/run` is the one deliberate imprecision: it is Codex's single web tool where Claude Code splits `WebSearch` from `WebFetch`. Search is the dominant use, and a missed merge on fetch costs less than filing fetches under search would.

#### MCP tools mostly need no normalization at all

Codex emits MCP calls in **two shapes**, and the dominant one is already the Claude Code spelling byte-for-byte (§9.2):

| Codex `namespace` | Codex `name` | Count | Canonical |
| --- | --- | --- | --- |
| *(absent)* | `mcp__chrome-devtools__evaluate_script` | 139 | identical — pass through |
| `mcp__cua_repl` | `js` | 381 | `mcp__cua_repl__js` |

So the rule is: if `name` already begins with `mcp__`, it *is* the canonical name and must pass through untouched. Otherwise, when `namespace` begins with `mcp__`, join as `{namespace}__{name}` — the namespace already carries the `mcp__` prefix, so this is a join, not a reconstruction.

This matters because the Claude Code form is **not** reversibly splittable: a server segment may itself contain `_` and `-` (`mcp__chrome-devtools__new_page`, `mcp__claude_ai_Claude_Docs__batch`), so there is no way to recover `(server, tool)` from the joined string. Normalization therefore has to run forward from Codex's explicit fields and never attempt the inverse.

The practical consequence is good: most MCP tool preferences already share a key across the two agents and will merge the moment a Codex session contributes one.

#### `apply_patch` splits into `Write` and `Edit`, and the signal is present

Claude Code keeps `Write` (new file) and `Edit` (existing file) as distinct tool names in `DecisionRecord.tool`, so existing hives hold separate `pattern:Write:*` and `pattern:Edit:*` units. Mapping every `apply_patch` to `Edit` would silently merge Codex file-creation approvals into Edit units.

It does not have to be lossy. Codex's patch envelope carries the distinction explicitly, and in the sample (§9.2) it is well populated: `*** Update File` 1934, `*** Add File` 190, `*** Delete File` 10. So:

| Envelope marker | Canonical |
| --- | --- |
| `*** Add File` | `Write` |
| `*** Update File` | `Edit` |
| `*** Delete File` | `codex:apply_patch_delete` |

Deletion has no Claude Code tool analogue — it happens through `Bash rm`, which is a different operation with different risk — so it keeps a family-prefixed name and merges with nothing. That is correct rather than unfortunate.

A patch touching several files can carry several markers. Where markers disagree, prefer `Write`: a batch that creates a file is the higher-privilege operation, and a preference learned about it should not be filed under the gentler name.

The flow has exactly one seam, where `DecisionRecord.tool` becomes `PreferencePattern.tool` and then `KnowledgeContent::Pattern.tool`. Before phase 1 it read:

```rust
let tool = d.tool.clone().unwrap_or_else(|| "*".to_string());
```

and it is now `canonical_tool_for(d)`. Normalization runs on the **read** side deliberately: the decision log keeps the name the agent actually used, so the log does not lie about what happened, and a wrong table entry is fixed by re-distilling rather than being baked into every record forever.

Phase 1 also showed the seam covers more than `Pattern` — removing the normalizer splits the `ToolAccuracy` key (`accuracy:{tool}`) the same way.

### 3.3 Capture

Decisions are produced by `claude-plugin/hooks/scripts/brain-gate.sh`, a Claude Code **PreToolUse** hook. Codex has no pre-tool gate. Its only event hook is `notify`, which fires after the fact — on this machine, `notify = [<client>, "turn-ended"]`.

So **the hive cannot gate Codex the way it gates Claude Code.** That is a property of Codex, not a gap in claudectl, and the design has to accept it rather than work around it.

What Codex does have is its own approval memory: `~/.codex/rules/default.rules`, 275 entries, every one of the form

```
prefix_rule(pattern=["uv", "run", "pytest", "tests/test_prompt_schema.py"], decision="allow")
```

This is machine-written, not hand-authored — entries include full heredoc bodies and absolute-path `mkdir`s nobody types into a config file (§9.4). It is a distilled approval corpus in a format claudectl can both read and write, which makes it both a capture source and an application target.

**Caveat, flagged because phase 5 depends on it:** the file's last write on this machine was 2 Sep, and `state_5.sqlite` contains no approvals table. Current Codex may have moved approval memory somewhere else, or may simply not have had a new approval to record. Verify before building on it; phases 1–4 do not depend on it.

### 3.4 Application

`src/brain/engine.rs:388` calls `hive::injection::build_hive_context_for_session` and puts the result in `brain_ctx.hive_context` — the **brain's own LLM prompt**, not the agent's context. The agent only ever sees the brain's verdict, delivered through the hook.

For an agent with no hook, the hive has to reach it another way. Three targets, in increasing order of privilege:

1. **An MCP server** (`claudectl hive stdio`). Pull-style: the agent asks. Needs no hook, no file mutation, and works with stock Codex because Codex loads MCP servers natively. This is the right first move.
2. **A managed block in `AGENTS.md`.** Prose knowledge, pushed. There is already precedent for marker-delimited managed blocks in that file on this machine — `<!-- paper-lantern:start -->` — so the pattern is established and the convention is `<!-- claudectl-hive:start -->` … `<!-- claudectl-hive:end -->`.
3. **Emitting `prefix_rule` lines into `~/.codex/rules/`.** Native enforcement without a hook: a high-confidence "always approve `cargo test`" unit becomes a Codex rule and Codex stops asking. This is real privilege — claudectl would be writing another agent's approval store — and must be explicit opt-in, never a default, never silent.

### 3.5 Artifact applicability

`KnowledgeContent::HookConfig` is Claude-Code-only by nature: there is no Codex PreToolUse to install into. `ArtifactRequires` (`hive/mod.rs:406`) carries `cli`, `os`, and `min_version` but no agent field, so a hook unit gossiped to a Codex-only peer has no way to declare itself inapplicable. It will arrive, fail to install, and look like a bug.

## 4. The spine: one vocabulary, no agent in the key

There is **no version on the gossip wire.** `hive/gossip.rs:555` writes `version: 1` on the *unit*, not on the protocol. And `semantic_key` is the cross-peer merge key — `store.rs:84`, `distiller.rs:120`, `archive.rs:147` and the merger all key on it. Changing how it is derived splits a hive between old and new binaries, silently, with both halves believing they are in sync.

That single fact determines the design:

> **Do not add an agent dimension to `semantic_key`.** Normalize foreign tool names *into* the existing vocabulary at capture time.

The reason is the product, not the schema. "`cargo test` is always approved," learned in Codex, has to land in the *same unit* as the one learned in Claude Code. If the key carried the agent, the hive would hold two units, each with half the evidence, each decaying on its own clock, and "learns once, remembers everywhere" would be false in exactly the case the feature exists for.

Three consequences:

- **`canonical_tool(family, namespace, name, payload) -> String` is a pure function** living in `claudectl-core`, with the §3.2 tables as data. Claude Code's names are the canonical form, because they are already on the wire in every existing hive. This is not a claim that they are better names; it is that they are the incumbent ones and a rename is a migration. The `payload` argument exists only so `apply_patch` can read its envelope marker (§3.2) — every other mapping is a function of the name and namespace alone.
- **Provenance is additive, never in the key.** `KnowledgeUnit.origin_agent: Option<String>` with `#[serde(default)]`. Old peers ignore the field; new peers can show "learned in Codex, confirmed in Claude Code," which is *more* trustworthy than either alone and should eventually raise confidence rather than merely annotate it.
- **Applicability is per-artifact, not per-unit.** `ArtifactRequires.agents: Vec<String>` (`#[serde(default)]`, empty = any) on `Skill` / `Command` / `HookConfig` only. A hook unit declares `agents = ["claude-code"]` and a Codex-only peer skips it on arrival instead of failing to install it.

Two details phase 2 settled that the first draft left implicit:

**Attribution is per-approach, not per-tool.** `origin_agents` is joined onto finished units from the decision log, keyed on canonical tool *plus* command keyword — the content part of a `Pattern`'s key. Keying on the tool alone would make a Codex-only `cargo fmt` approach inherit `claude-code` simply because `Bash` is a shared tool.

**Provenance is cumulative across a merge, but only where evidence is.** A replacing merge unions both units' agent lists, since "confirmed independently in two agents" is the case the field exists to record. A *rejected* unit's origins are not folded in: its evidence did not transfer, so claiming its agent would overstate where the stored knowledge came from.

**Agent presence is detected by state directory, not by PATH.** `~/.claude` and `~/.codex` existing is the better signal for what the answer is used for — a skill installs into that directory and is read from it on the agent's next run, so an agent whose binary lives somewhere unusual (a version manager, a wrapper, a GUI bundle) should still receive artifacts.

An unnormalizable tool — Codex's `wait`, or any future agent's unique primitive — keeps its raw name prefixed with its family (`codex:wait`). It will not merge with anything, which is correct: it has no counterpart.

## 5. Observe: adapters that produce neutral records

`src/coord/adapter.rs` already declares the trait. Two things have to change for it to carry real weight.

**It has to move, or grow a core half.** `ClaudeCodeAdapter` wraps `claudectl-core`'s `discovery::scan_sessions`. A Codex adapter needs the same: rollout parsing belongs in `claudectl-core/src/agent/codex/`, not inside `src/coord/adapter_codex.rs`, because core cannot depend on the binary and the TUI needs to render Codex sessions too. The adapter in `src/coord/` stays a thin binding.

**It has to be on the path, not beside it.** Today nothing but a listing command calls it. Discovery, monitor, the brain's observe loop, and the TUI session table all need to go through `all_adapters()` for the trait to mean anything. That is the largest mechanical change in this spec and the one most likely to want its own epic.

The capability matrix already models partial support honestly (`AdapterCapabilities::minimal()`), which is what Codex needs: `discover_sessions` and `monitor_state` yes, `send_input` and `deliver_interrupt` no, until there is a mechanism for them.

## 6. Apply: project a unit into each agent's native surface

Symmetry is the wrong goal here. Each agent has a different enforcement surface, and the hive should use the best one each offers rather than the lowest common denominator.

| | Claude Code | Codex |
| --- | --- | --- |
| Gate a tool call before it runs | PreToolUse hook → `--brain-query` | **not possible** |
| Pull knowledge on demand | — | `claudectl hive stdio` (MCP) |
| Standing prose instruction | `CLAUDE.md` | `AGENTS.md` managed block |
| Standing approval | brain gate + `session-policy` | `rules/default.rules` (opt-in) |
| Install a skill | `~/.claude/skills/` | `~/.codex/skills/` |

The skills row works today except for one hardwired path: `hive/cli/share.rs:487-490` (`default_install_dir()` → `~/.claude`) and the scan roots at `claudectl-core/src/skills.rs:79-86`. Widening both to a per-family set of roots makes skill sharing cross-agent with no change to the wire format, which makes it the cheapest visible win in this spec.

## 7. One bus everyone can use

The bus today is single-machine and Claude-shaped in exactly one place. Both are smaller problems than they look.

### 7.1 Any agent, same machine — mostly works already

`claudectl bus stdio` is an MCP server. Codex loads MCP servers from `config.toml`. A Codex session can therefore already call `whoami`, `list_agents`, `publish` and `read_inbox` with **no code change** — this needs documenting more than building.

One thing does break. `bus/roles.rs:116`:

```rust
pub fn find_claude_ancestor_pid() -> Option<u32> {
    ancestor_pids().into_iter()
        .find(|pid| process_command_for(*pid).is_some_and(|c| c.to_lowercase().contains("claude")))
}
```

`bus role bind --self` walks the ancestor chain for a process whose command line contains `claude`. From a Codex session there isn't one, so the bind fails. The fix is to match a set of known agent families and return which one matched — the family is worth recording on the role anyway (§7.2).

### 7.2 Roles need an agent and a machine dimension

The `roles` table (`bus/store.rs:69-77`) is keyed on `role` alone, with `cwd_selector` and a nullable `pid` for resolution. Both are machine-local concepts, so `supervisor` on a laptop and `supervisor` on a Mac mini are the same row, and a role bound on one machine resolves to nothing on the other.

Three additive columns — `agent_family`, `origin_peer`, and a `peer_id`-qualified address form — let a message be addressed to `reviewer@mini` or `reviewer:codex` while leaving bare `reviewer` meaning what it means today. The migration pattern already exists in the file (`add_column_if_missing`, used for the `pid` column), and the coord schema's `PRAGMA user_version` gate is the precedent for refusing to run against a newer schema.

### 7.3 The relay bridge

There is **no bridging between the bus and the relay.** Verified: zero `relay::` references in `src/bus/`, zero `bus::` references in `src/relay/`, and `relay::MessageType` has no bus variant. The bus is SQLite over a local file; the relay is authenticated TCP between machines. They have never met.

A bridge is a new `MessageType::BusMessage` plus a forwarder, and two existing pieces do most of the hard part:

- **Loop prevention is already built.** `bus/policy.rs:31` defines `DEFAULT_MAX_HOPS = 8` and `validate_hop_count`, and the `messages` table already carries `hop_count`. A forwarded message increments it; the cap that exists for agent-to-agent chains works unchanged for machine-to-machine ones.
- **Trust is already built.** The relay's HMAC challenge-response and the hive's membership tiers decide who may forward. A bus bridge should refuse to forward from a `hive.read` member for the same reason `KnowledgeRejected` exists (#435): a reader contributing silently is worse than a reader being told no.

What needs deciding, and should be decided before it is built: whether the bridge is **replicating** (every peer's SQLite converges, which means conflict resolution and a durable ordering) or **forwarding** (a message addressed to a remote role is relayed on demand, and the local database stays local). Forwarding is far simpler, matches how `send_to` already works, and is almost certainly right for the first version — but the reserved-role guard (`bus/policy.rs:41`, `supervisor` and `operator` cannot be bound) means a cross-machine `supervisor` address has no owner, so that interaction needs an answer either way.

**Known trap:** one-shot relay dials have no reply path — `send_to` fails by design and `TransientReplies` is the mechanism. A bus bridge that forwards a `publish` and expects an ack will hit exactly this, and it is the same shape as the still-open `TaskHandoff` instance in epic #478.

## 8. Phasing

Ordered by irreversibility, not by size.

1. **Canonical tool vocabulary** (§4). Pure function, table-driven, no wire change, no new dependency. Testable against real rollout fixtures. Ships alone and unblocks everything else. **Do this first** — it is the only decision here that is expensive to reverse.
2. **Provenance and applicability** (`origin_agent`, `ArtifactRequires.agents`). Both `#[serde(default)]`, so old peers ignore them. Small, and wanted before any foreign unit exists.
3. **Codex discovery and monitor** in `claudectl-core`, wired through the adapter trait. The largest piece; may want its own epic. Cost fields degrade to tokens-only (§3.1).
4. **`claudectl hive stdio`** — the read surface, mirroring `src/query/mcp.rs`. Lowest-risk application path, works with stock Codex.
5. **Codex write surfaces** — `AGENTS.md` managed block, then `rules/default.rules` emission behind explicit opt-in. Depends on verifying §3.3's caveat.
6. **Bus agent/machine dimension** (§7.2). Additive columns plus a resolver change. Independent of 1–5; could go first if the bus matters more than the knowledge sharing.
7. **Bus relay bridge** (§7.3). Decide forwarding vs replication before writing code.

Skill-root widening (§6) is a one-file change and does not need a phase; it can ride with whatever lands first.

**Feature gates.** Adapters sit behind `coord`; the hive sits behind `relay`; the bus sits behind `bus`. Cross-agent hive lives in the intersection of all three, which no current CI job exercises together. That needs a build job before phase 3, or the gates will rot the way the ones in the #465 audit did.

## 9. Evidence

Everything below was measured on one machine (macOS, Codex CLI 0.162.1) on 2026-10-13. It is recorded here because the next person to touch this will otherwise re-measure it.

### 9.1 Codex session layout

```
~/.codex/sessions/YYYY/MM/DD/rollout-<ISO-ts>-<uuid>.jsonl     671 files
```

One JSON object per line, each with `timestamp`, `ordinal`, `type`, `payload`. Event types observed:

| `type` / `payload.type` | Carries |
| --- | --- |
| `session_meta` | `session_id`, `id`, `cwd`, `runtime_workspace_roots`, `parent_thread_id` |
| `event_msg` / `token_count` | `total_token_usage{input,cached_input,cache_write_input,output,reasoning_output,total}`, `last_token_usage`, `model_context_window` |
| `event_msg` / `task_started`, `task_complete`, `turn_aborted` | status transitions |
| `response_item` / `function_call` | `name`, `namespace`, `arguments`, `call_id` |
| `response_item` / `custom_tool_call` | `name`, `input`, `status`, `call_id` |
| `response_item` / `message`, `reasoning` | conversation content |
| `token_usage_record` | per-turn usage |
| `turn_context`, `world_state` | turn configuration |

`model_context_window` being present means context-pressure health checks work for Codex with no new signal. No field carries cost in USD.

### 9.2 Tool-name histogram

120 rollout files sampled across all months, ~15 000 tool calls. Counts in the §3.2 table. Kinds: `function_call` 10 101, `custom_tool_call` 4894, `local_shell_call` 0.

A second pass over 168 files recorded the **raw** `namespace` and `name` fields rather than a joined display string, which is what §3.2's MCP rule is built on:

```
  381  namespace='mcp__cua_repl'                name='js'
  139  namespace=None   name='mcp__chrome-devtools__evaluate_script'
  120  namespace=None   name='mcp__chrome-devtools__take_snapshot'
   75  namespace=None   name='mcp__chrome-devtools__click'
   10  namespace=None   name='mcp__codex_apps__github_search'
```

Two shapes, and the common one is already the Claude Code spelling. An earlier pass of this measurement printed `mcp__context7/query_docs`; the `/` was the measuring script's own join separator and is **not** a Codex delimiter. Recorded here because the mistake is easy to repeat.

The same pass counted `apply_patch` envelope markers: `*** Update File` 1934, `*** Add File` 190, `*** Delete File` 10, `*** Move File` 0.

### 9.3 Skill format comparison

```
~/.codex/skills/docker-container-ops/SKILL.md
  ---
  name: docker-container-ops
  description: Launch, manage, exec into, and listen to Docker containers. …
  ---

~/.claude/skills/braintrust/SKILL.md
  ---
  name: braintrust
  version: 1.0.0
  description: Use the Braintrust `bt` CLI for projects, traces, prompts, …
  ---
```

Same contract. Claude Code's optional `version` is absent in Codex skills and ignored by them, so a shared skill round-trips.

### 9.4 Codex approval store

```
~/.codex/rules/default.rules      275 lines, 46 KB, mtime 2 Sep
  275 x prefix_rule(...)
  275 x decision="allow"       (no deny entries)
```

No header or comment. Machine-written: entries include multi-kilobyte heredoc bodies and `mkdir -p /Users/<user>/.codex/skills/...` with absolute paths. `state_5.sqlite` tables are `threads`, `thread_sections`, `thread_attachments`, `thread_dynamic_tools`, `thread_spawn_edges`, `projects`, `project_roots`, `project_idempotency_keys`, `external_agent_config_imports`, `remote_control_enrollments`, `backfill_state`, `rollout_migration_*`, `_sqlx_migrations` — no approvals table, so the rules file remains the only approval store found.

### 9.5 Other Codex integration points

- `~/.codex/AGENTS.md` — standing instructions. Already contains marker-delimited managed blocks (`<!-- paper-lantern:start -->`), so the convention is established.
- `~/.codex/config.toml` — `mcp_servers.*` (many configured), `notify = [<client>, "turn-ended"]`, `approvals_reviewer = "auto_review"`.
- `~/.codex/session_index.jsonl` — `{id, thread_name, updated_at}` per thread. A cheap index for listing without parsing rollouts.
- `~/.codex/plugins/`, `~/.codex/memories/` — not examined; possible later surfaces.

## 10. Open questions

1. **Does current Codex still write `rules/default.rules`?** §3.3's caveat. Blocks phase 5 only.
2. **Forwarding or replication for the bus bridge?** §7.3. Decide before writing code.
3. **Who owns a cross-machine reserved role?** `supervisor` and `operator` cannot be bound (`bus/policy.rs:41`), so `supervisor@mini` has no resolution story.
4. **Does `origin_agent` affect confidence, or only display?** A unit confirmed independently in two agents is better evidence than one confirmed twice in the same agent. Saying so in the scoring is defensible; it is also a scoring change, which is a separate kind of risk from a schema change.
5. ~~**Does `ClaudeSession` get renamed?**~~ **Closed by #524:** no. An `agent_family` field plus three predicates costs less than renaming public API across three crates, and is reversible if a rename later earns itself.
6. ~~**What happens to a Codex session's cost in the fleet snapshot?**~~ **Closed by #524:** `cost_usd` is 0 with `has_known_cost()` false. The existing sums are additive so nothing is corrupted, and anything that needs to tell "unknown" from "zero" has a predicate to ask.
7. **Should the adapter registry be `runtime::SessionSource` rather than a second trait?** Raised by #533. `SessionSource` already answers "where do sessions come from", is already core-owned, and is already implemented by the binary — a second registry for the same question would be the duplication #533 exists to remove.

## Related

Epic #521 is this spec's tracking issue; phases are #522–#528.

Epic #478 (work across hosts — `TaskHandoff` is still the open instance of the no-reply-path trap §7.3 describes) · epic #424 (open cluster / named hives) · #435 (read-only members, the precedent for refusing a contribution rather than dropping it) · #465 (dead-code audit — `coord/adapter.rs` carries `#![allow(dead_code)]` and is the same shape) · #506 (LAN discovery written but never started — the same "exists but not on the path" shape as the adapter trait) · #511 (a one-shot send that is never acknowledged) · `docs/relay-and-hive.md` · `docs/AGENT_BUS.md` · `docs/open-cluster.md`

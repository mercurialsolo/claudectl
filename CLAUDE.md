# claudectl

Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you.

## Build & Test

```bash
cargo build                  # Debug build
cargo build --release        # Release build, default features (bus+coord+relay+hive) — ~6.3 MB; matches the Homebrew bottle
cargo build --release --no-default-features --features hive   # Minimal sync-only build — ~3.5 MB
cargo test                   # Run all tests
cargo clippy -- -D warnings  # Lint (warnings are errors in CI)
cargo fmt --check            # Check formatting
```

## Architecture

### Workspace layout

This is a Cargo workspace with three crates. Dependencies flow strictly downward — `claudectl → claudectl-tui → claudectl-core` — and CI enforces it (Layering grep on core, plus standalone build jobs for each crate). Adding upward arrows is what the workspace exists to prevent.

```
crates/
├── claudectl-core/    # foundational types, IO, the UI↔runtime trait contract
│                      #   session, discovery, monitor, process, transcript,
│                      #   models, theme, logger, helpers, history, terminals/,
│                      #   health, rules, config, hooks, launch, skills, runtime
└── claudectl-tui/     # terminal UI + recording + demo fixtures
                       #   app.rs, ui/*, recorder.rs, session_recorder.rs, demo.rs
                       #   Depends on claudectl-core. Never on the binary.
src/                   # the binary crate `claudectl`
                       #   main.rs + access/ + brain/ + bus/ + context/ +
                       #   coord/ + hive/ + query/ + relay/ + orchestrator +
                       #   init + commands + config.rs + brain_screen.rs.
                       #   Implements the runtime traits over the real
                       #   subsystems via `src/runtime/`.
```

**UI ↔ runtime contract** lives at `crates/claudectl-core/src/runtime.rs`. Eight traits — `SessionSource`, `BrainView`, `CoordView`, `BusView`, `Actions`, `BrainReviewView`, `Orchestrator`, `HiveActions`, plus the stateful `BrainDriver` — wrapped in a `Runtime` aggregate. Core-owned DTOs (`SessionSnapshot`, `DecisionSummary`, `LeaseSummary`, `HiveViewSnapshot`, etc.) so the contract doesn't drag brain / coord / bus types upward. The binary's `src/runtime/` provides `Live*` adapters that implement each trait over the real subsystem.

**Re-export bridge:** `src/lib.rs` and `src/main.rs` both do `pub use claudectl_core::{session, discovery, …};` and `pub use claudectl_tui::{app, demo, recorder, session_recorder, ui};` so existing `crate::session::*` / `crate::app::*` paths in the binary keep resolving without rewriting every import.

**Feature propagation:** the binary's `coord`, `relay`, `hive` features propagate to `claudectl-tui` via `claudectl-tui/coord` etc. in `[features]`, so a single `#[cfg(feature = "...")]` gate resolves consistently across both crates.

### Core modules — `claudectl-core` (`crates/claudectl-core/src/`)

- `runtime.rs` — view traits + DTOs + `Runtime` + `MockRuntime`. See above.
- `session.rs` — Session data structures and formatting
- `discovery.rs` — Scans `~/.claude/sessions/*.json` and resolves JSONL paths
- `monitor.rs` — Parses JSONL conversation logs for tokens, cost, status events
- `process.rs` — Process introspection via native `ps` (not sysinfo crate)
- `history.rs` — Session history persistence and cost analytics
- `fleet.rs` — Cluster session view: the `~/.claudectl/relay/fleet.json` snapshot `relay serve` publishes (atomic write, staleness gates) plus a side-effect-free local session collector for advertising to peers
- `health.rs` — Session health monitoring (cache ratio, cost spikes, loop detection, stalls, context saturation). Owns `HealthThresholds`.
- `rules.rs` — Auto-rule engine: match sessions by status/tool/command/project/cost, then approve/deny/send/terminate/route/spawn/delegate
- `models.rs` — Model pricing profiles (built-in + user overrides) for cost tracking
- `transcript.rs` — JSONL transcript parser (messages, tool use, tool results, usage data)
- `theme.rs` — Color theming (dark/light/monochrome, respects NO_COLOR)
- `logger.rs` — Structured diagnostic logging
- `helpers.rs` — Shared utilities (webhook, notification, kill_process, aggregate session)
- `hooks.rs` — Event hook system (shell commands fired on session events)
- `launch.rs` — Launch and resume Claude Code sessions from the TUI or CLI
- `skills.rs` — Skill registry + claude-plugin metadata
- `config.rs` — `BrainConfig` / `IdleConfig` data structs (the binary still owns TOML parsing in `src/config.rs`; only the value types live here)
- `terminals/` — Terminal backends; see below.

### TUI modules — `claudectl-tui` (`crates/claudectl-tui/src/`)

- `app.rs` — `App` state struct, refresh loop, keyboard event handling. Holds a `claudectl_core::runtime::Runtime` for all brain/coord/bus reads and writes; never touches binary-only modules directly.
- `ui/` — Render functions: `table.rs` (session list), `detail.rs` (expanded panel), `help.rs` (overlay), `status_bar.rs` (footer), `peers.rs` (relay peers panel, feature `relay`), `skills.rs` (skills overlay).
- `recorder.rs` — Dashboard recording (asciicast/GIF capture of full TUI).
- `session_recorder.rs` — Per-session highlight reel recording (extracts edits, commands, errors; strips idle time).
- `demo.rs` — Deterministic fake sessions for screenshots, recordings, demos. Includes `DemoHighlightState` and `demo_peers` (feature `relay`).

Feature flags: `coord`, `relay`, `hive` mirror the binary's same-named features and gate `App` fields and ui panels.

### Binary modules — `claudectl` (`src/`)

- `main.rs` — CLI entry point, mode dispatch (TUI, watch, JSON, list, history, stats, orchestrator, clean, brain-eval, brain-query, mode, insights, init, doctor).
- `brain_screen.rs` — Full-screen Brain Review surface (scorecard + interactive review). Stays in the binary because it depends on `brain::metrics` and `brain::risk`; `main.rs` calls `brain_screen::render_brain_screen` for the brain panel. Every other ui render goes through `claudectl_tui::ui::*`.
- `doctor.rs` — `claudectl doctor` (#326). Unified install + runtime health checklist (PATH, hooks, plugin files, brain endpoint, bus feature, bus DB, session discovery, terminal integration). Returns `Check` rows with Pass/Advisory/Fail/Skipped + fix hints; `--json` form for scripting. Replaces the scattered `--doctor` flag and `init --check` paths.
- `config.rs` — Layered TOML config: CLI flags > `.claudectl.toml` > `~/.config/claudectl/config.toml` > defaults. Re-exports `BrainConfig`, `IdleConfig`, `HealthThresholds` from `claudectl-core`.
- `orchestrator.rs` — Multi-session task runner with dependency ordering.
- `commands.rs` — CLI command dispatch shared between modes.
- `init/` — `claudectl init` opinionated onboarding wizard (5 phases: budget, brain, plugin, bus, skills); see `docs/AGENT_BUS.md` §8 and issue #257. `init/plugin_assets.rs` (#325) embeds every `claude-plugin/` file via `include_str!` so the Plugin phase writes a working install to `~/.claude/plugins/claudectl/` without a repo clone.
- `runtime/` — `Live*` adapters that implement the `claudectl-core::runtime` traits against the real subsystems (sessions, brain, coord SQLite, bus SQLite, orchestrator, hive). See `runtime/{sessions,brain,brain_driver,brain_review,coord,bus,actions,orchestrator,hive}.rs`.

**Claude Code Plugin** (`claude-plugin/`): Integrates the brain directly into Claude Code sessions.
- `hooks/scripts/brain-gate.sh` — PreToolUse hook: queries brain for approve/deny on Bash/Write/Edit calls
- `hooks/scripts/budget-check.sh` — PreToolUse hook: enforces spend limits
- `commands/` — Slash commands: `/sessions`, `/spend`, `/brain-stats`, `/brain`, `/auto-insights`
- `agents/supervisor.md` — Session health triage agent
- `skills/session-monitoring/` — Auto-activated session awareness skill

**Brain** (`src/brain/`): Local LLM auto-pilot subsystem.
- `engine.rs` — Main brain loop: observes sessions, evaluates rules, queries LLM, executes decisions
- `client.rs` — HTTP client for local LLM endpoints (ollama, llama.cpp, vLLM, LM Studio)
- `context.rs` — Builds session context summaries for LLM prompts
- `decisions.rs` — Decision logging and few-shot retrieval (learns from past corrections)
- `agents.rs` — Agent delegation support
- `mailbox.rs` — Message passing between brain and TUI
- `prompts.rs` — Prompt templates (built-in + user overrides via `~/.claudectl/brain/prompts/`)
- `evals.rs` — Eval harness for testing brain decision quality against scenarios
- `insights.rs` — Auto-insights: friction pattern detection, rule suggestions, differential tracking

**Relay** (`src/relay/`): Cross-machine TCP transport (feature-gated behind `relay`).
- `mod.rs` — PeerId, RelayMessage, MessageType, identity persistence, peer PSK storage
- `advertise.rs` — Collects this machine's sessions for the heartbeat (throttled to 5s) and publishes the fleet snapshot peers report; `relay fleet` and the TUI read it
- `crypto.rs` — Inline SHA-256 + HMAC-SHA256, PSK generation/formatting
- `protocol.rs` — NDJSON framing over TCP, HMAC challenge-response auth
- `peer.rs` — PeerConnection: connect, send, reader thread, heartbeat, reconnect
- `listener.rs` — TcpListener accept loop with auth rate limiting, max peers
- `mesh.rs` — PeerRegistry: broadcast, send_to, message dedup, heartbeat tick
- `delegation.rs` — DelegationContext, message builders for DelegateTask/TaskStatus/TaskHandoff/TaskInterrupt
- `worker.rs` — RemoteWorker: accepts delegated tasks, spawns claude sessions, reports status
- `invite.rs` — Invite codes (base32), word phrases (256-word list), invite links (cctl://), QR rendering
- `lan.rs` — UDP broadcast LAN discovery: announcer and scanner threads
- `cli.rs` — CLI dispatch for all relay subcommands (serve, invite, join, discover, delegate, etc.)

**Bus** (`src/bus/`): Agent-bus MCP server, durable role directory, and mailbox (feature-gated behind `bus`; see `docs/AGENT_BUS.md`).
- `mod.rs` — module surface
- `store.rs` — SQLite (WAL) at `~/.claudectl/bus/bus.db`: roles + messages tables (with `hop_count` column), drain + peek semantics
- `roles.rs` — Role addressing, cwd-inference, ambiguity/unbound resolution. Caller may override with `CLAUDECTL_BUS_ROLE` or `--role`
- `policy.rs` — Guardrails: subject grammar, type allowlist, body cap, leading-`/` neutralization (§9), hop cap (default 8), reserved-role guard (`supervisor`/`operator` cannot be bound)
- `mcp.rs` — rmcp stdio server exposing `whoami`, `list_agents`, `publish` (with `parent_hop`), `read_inbox` (with `peek`), plus supervisor tools `submit_task` / `list_tasks` / `task_status`
- `cli.rs` — `claudectl bus` subcommand (stdio, role bind/list, send, inbox + `--peek`, whoami, stop-hook driver, prune)

**Supervisor** (`src/coord/{supervisor,actuator,verify,resume,tasks,session_policy,hook_events,events,exporter,supervisor_cli}.rs`, `src/ingest.rs`): Durable, verified task lifecycles on top of the bus (see `docs/AGENT_BUS.md#10-supervisor` and README §Supervisor).
- `tasks.rs` — `tasks` / `task_attempts` / `task_verifications` / `task_transitions` CRUD + state machine. Every transition writes state + log in one transaction so the ledger invariant "every state has a cause" holds.
- `supervisor.rs` — **pure** reconciler. Reads desired state from coord, observed state from `Sensors`, returns `Vec<Action>`. No I/O. Health-check → `HealthAction` mapping table. `Unknown` observed status is the no-actuation backstop (RFC §6).
- `actuator.rs` — Carries out `Action`s. Three-step: insert attempt → perform side effect → log transition. Side-effect failure stops *before* the transition is recorded, so the next tick re-emits.
- `verify.rs` — `Verifier` enum (Run/Brain/Agent) + `run_verifier` dispatch. Fail-closed on missing PASS/FAIL marker. `build_retry_prompt` composes original + failure output + "fix these issues" feedback.
- `resume.rs` — Tree-state hash (git when available, mtime fallback) + recovery-prompt builder. Resume the *task*, not the session.
- `session_policy.rs` — Atomic per-session policy file at `~/.claudectl/coord/session-policy/<session>.json`. Brain-gate hook reads it on every tool call; fail-open to `Inherit` keeps the manual-upgrade gap non-breaking.
- `hook_events.rs` — `hook_events` table (latency-only signal path; JSONL tail stays authoritative).
- `events.rs` — v1 NDJSON event schema (frozen). Three families: `task.transition`, `task.verification`, `task.escalated`.
- `exporter.rs` — Hand-rolled HTTP `/metrics` server (no web framework dep). Worker thread per scrape so the tick loop never blocks. Exposes `claudectl_tasks_by_state`, `claudectl_fleet_cost_usd_total`, `claudectl_retries_total`, `claudectl_verifier_pass_rate`.
- `supervisor_cli.rs` — `claudectl supervisor` subcommand (run / submit / status / logs / cancel / drain / undrain). Hand-rolled TOML reader for the `tasks.toml` subset.
- `src/ingest.rs` — `claudectl ingest --hook <name>` subcommand. Bash hooks call `claudectl ingest --hook PostToolUse 2>/dev/null || true`. Best-effort by construction.

Coord schema is gated on `PRAGMA user_version` (`EXPECTED_COORD_SCHEMA_VERSION = 3` today). A binary that meets a newer schema refuses to start with the exact `init --upgrade` remediation in the error.

**Hive** (`src/hive/`): Gossip-based knowledge sharing across connected brains (feature-gated behind `relay`).
- `mod.rs` — KnowledgeUnit, KnowledgeScope, KnowledgeContent types, semantic key, broadcast channel
- `store.rs` — JSONL-backed knowledge store with semantic index, atomic save
- `distiller.rs` — Converts DistilledPreferences/Insights into KnowledgeUnits with thresholds
- `merger.rs` — Conflict resolution: local always wins, peer-vs-peer by confidence*evidence
- `gossip.rs` — GossipEngine: incremental sync, snapshot pagination, epidemic propagation
- `trust.rs` — PeerTrust with auto-drift, TrustTier classification, TrustStore persistence
- `injection.rs` — Brain prompt integration with trust labels, concordance checking for drift
- `cli.rs` — CLI dispatch for hive subcommands (status, knowledge, export, import, trust)

**Access** (`src/access/`): Capability grants — scoped, expiring, revocable read-only access for a third party (#427, phase 1 of the open-cluster RFC #423; feature-gated behind `relay` because the MAC comes from `relay::crypto`). See `docs/access.md`.
- `mod.rs` — `~/.claudectl/access` layout, grant-id validation/minting, opaque `AccessError::Denied`, audit-only `DenyReason`
- `scope.rs` — `<resource>.<verb>:<qualifier>` grammar; read-only verbs, `validate_qualifier` (keeps the MAC payload unambiguous, and the gate `query::http` runs a route's project segment through), `unissuable_reason` as the single list of what `access grant` will mint
- `token.rs` — `cctl_<grant_id>_<mac>` mint/parse, canonical MAC payload, HMAC key at `access/secret` (fails closed, 0600 before first byte)
- `grant.rs` — `Grant` records, atomic per-grant JSON store, `audit.jsonl`, `verify` (parse → load → MAC → revoked/expiry → scope)
- `grant.rs` also owns the #431 accounting: `charge_daily_budget` (UTC-day rollover, typed `ChargeError` so a store failure fails closed instead of answering unmetered), `verify_detailed` (`pub(crate)`; exposes `DenyReason` so only a `missing_scope` denial charges the budget), `record_query_use` (audits the question and the cited paths)
- `cli.rs` — `claudectl access` subcommand (grant / list / audit / revoke), all with `--json`. `audit` renders a QUESTION column; `cited` is `--json`-only

**Context index** (`src/context/`): The substrate a read-only project query may be answered from — open-cluster phase 2 (#428, `docs/open-cluster.md` §4.2). Gated behind `relay`, matching `src/query/`, its only consumer; the hive-unit source is additionally `#[cfg(feature = "hive")]` and yields nothing without it. There is no CLI here — the surface over it is `src/query/`.
- `mod.rs` — `ContextIndex::build(&Path)` and `build_with(root, &IndexExposure, ShareMode)`; takes a path, not a project name. Routes markdown by name and location, collects skills and hive units, `fingerprint()` is FNV-1a (`"fnv1a:<hex>"`)
- `git.rs` — `git ls-files -z --cached --full-name` is the only source of paths in the module; `repo_root`, `tracked_files` (sorted, deduped), `IndexError::{GitUnavailable, NotARepo, GitFailed}`. No git is an error, never a fallback
- `deny.rs` — the "excluded" half of "tracked and not excluded": denied names and prefixes (`.env*`, `.netrc`, ssh keys, `credentials`), extensions (`jsonl`, keys/certs, `sqlite`/`db`), and path segments (`.claude`, `.claudectl`, `target`, `node_modules`, …) at any depth. `DenyReason::{Name, Extension, Directory}` for the build stats
- `docs.rs` — splits markdown into `DocSection`s at ATX headings, each carrying its enclosing heading path; drops leading YAML frontmatter; tracks fenced code blocks, because `#` starts a comment in TOML/shell/Python and a section is the citation unit
- `module_map.rs` — `//!` headers, `///` docs and public signatures, never bodies. Separate `skip_depth` (item bodies, emit nothing) and `container_depth` (`impl` / inline `mod`, descend); `pub(crate)` and private items are dropped name and all
- `exposure.rs` — six categories (`claude_md`, `readme`, `docs`, `module_map`, `skills`, `hive_units`) persisted at `~/.claudectl/access/index-exposure.json`. Mirrors `hive::exposure` semantics with locally redefined types and identical wire values; mode comes from `Config.hive.share_mode`

**Query surface** (`src/query/`): Read-only project queries for grant holders — open-cluster phase 3 (#429, `docs/open-cluster.md` §4.5, §4.7). Verbatim spans with citations, no generation; deterministic term matching where §4.3 will put Jev. Gated behind `relay` (it needs `access`); `mcp.rs` additionally behind `bus`, which is what carries `rmcp`/Tokio/`schemars`.
- `core.rs` — `QueryCore::{ask, topics, get_doc}`. Every policy decision lives here; `http.rs` and `mcp.rs` have none of their own. **One server, one project:** the process serves the repo it started in, and a request's `<project>` is compared against that name, never resolved to a directory. The required `Scope` is derived from the *served* project, so another project's token cannot pass. Refusals are one opaque `404`; a missing bearer is the one `401`
- `core.rs` also enforces #431's guardrails: `verify` → rate limit → daily budget, in that order. The limit is checked *after* verification so the bucket map is only keyed on ids that presented a valid MAC; a throttled request never reaches the budget; both refuse with `429` + `Retry-After` rather than the usual opaque `404`, because the holder has already proved the token and a silent grant is hostile
- `rank.rs` — term overlap, integers end to end, total-order sort. Own heading ×3, ancestor heading ×1, body ×1; a term scores once per zone, not per occurrence, which bounds a span's score by the question's term count and needs no length normalizer
- `http.rs` — hand-rolled HTTP/1.1 like `relay/http.rs` and `coord/exporter.rs`. `POST …/query`, `GET …/topics`, `POST …/doc`. Auth is placed *after* route parsing, unlike relay's single global token, because which scope a request needs depends on the operation
- `mcp.rs` — `ask_project` / `list_topics` / `get_doc` over rmcp stdio, mirroring `bus/mcp.rs`, in its own current-thread Tokio runtime. This is the server half; the `--endpoint <url>` client §4.7 sketches runs on the third party's machine and is a follow-up
- `cli.rs` — `query serve` (HTTP, index built once at startup) and `query stdio` (MCP). `resolve_project` defaults to the repo directory's name and refuses to start when that cannot be a scope qualifier — every worktree with a `+` in its name

**Rate limiter** (`src/rate_limit.rs`): In-process token bucket, shared by two callers and gated on `any(bus, relay)`. `bus/mcp.rs` keys on `sender_role` at one global capacity (60/min); `query/core.rs` keys on a *verified* grant id and passes that grant's own `rate_limit_per_min` via `try_acquire_with_capacity`. A bucket keeps the capacity it was created with, so editing a grant file moves its ceiling on the next restart, not mid-window. No eviction, deliberately — both key spaces are bounded (bound roles; grants that have presented a valid MAC). `retry_after_secs` is read-only, for the `Retry-After` header.

**Terminal backends** (`crates/claudectl-core/src/terminals/`): Ghostty, Kitty, tmux, WezTerm, Warp, iTerm2, Terminal.app, Gnome Terminal, Windows Terminal — auto-detected, used for tab switching and input sending.

## Key Design Decisions

- **Minimal dependencies** — 7 runtime crates in the sync core. Startup under 50ms. **Exception:** the `bus` feature pulls rmcp + Tokio + schemars (every available MCP SDK is async) and is in the default feature set since the supervisor RFC (#342) — `cargo install` and `brew install` produce the same ~6 MB binary. The minimal sync-only build path (`--no-default-features --features hive`) remains CI-tested as the lower bound.
- **Native `ps`** over `sysinfo` crate to keep binary small.
- **Multi-signal status inference** — combines CPU usage, JSONL events, and timestamps (not just one signal).
- **Incremental JSONL parsing** — tracks file offsets, never rereads full files.
- **No async runtime** — synchronous with polling. Keeps complexity low. **Exception:** the two MCP servers — `src/bus/mcp.rs` under `claudectl bus stdio` and `src/query/mcp.rs` under `claudectl query stdio` — each build their own current-thread Tokio runtime inside their `run_stdio`. Tokio carries no `rt-multi-thread`, so a multi-thread runtime would not compile; the TUI and every other code path remain sync.
- **Deny-first rule evaluation** — deny rules always override approve/brain suggestions, regardless of config order.
- **Brain decisions are local-only** — all decision logs and few-shot examples stay on the user's machine.
- **Brain gate mode** — `~/.claudectl/brain/gate-mode` controls on/off/auto. File absent = on (default). The plugin hook and `--brain-query` both check this before querying the LLM.
- **Insights mode** — `~/.claudectl/brain/insights-mode` controls auto-generation of insights. File absent = off (opt-in). When on, insights are generated alongside preference distillation every 10 decisions.

## Conventions

- Run `cargo fmt` and `cargo clippy -- -D warnings` before committing.
- Tests live in `tests/integration_tests.rs` and `tests/unit_tests.rs`.
- Status inference logic has extensive test coverage — do not change status detection without updating tests.
- Health checks in `crates/claudectl-core/src/health.rs` have full unit test coverage — add tests for new checks.
- Terminal backends implement the pattern in `crates/claudectl-core/src/terminals/mod.rs` — add new terminals there.
- Config fields must be added to all three layers (CLI args in `main.rs`, TOML struct in `config.rs`, merge logic in `config.rs`).
- **Dependency direction:** `claudectl` (binary) depends on `claudectl-tui` and `claudectl-core`. `claudectl-tui` depends on `claudectl-core` only. `claudectl-core` depends on nothing claudectl-specific — no `crate::brain`, `crate::bus`, `crate::coord`, `crate::hive`, etc. references from inside core. CI rejects upward references via a grep guard plus standalone build jobs (`Core (standalone)`, `TUI (standalone)`).
- Foundational modules in core (and now TUI) never have `pub(crate)` on items downstream needs to call — promote to `pub` instead. The binary is a downstream consumer of both other crates.
- Brain prompt templates can be overridden by placing files in `~/.claudectl/brain/prompts/` — run `--brain-prompts` to list sources.
- Plugin hook scripts must check for `claudectl` availability and exit 0 on failure — never block Claude Code.
- Plugin commands call `claudectl` CLI modes and format output — they don't implement logic directly.

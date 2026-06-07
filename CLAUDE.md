# claudectl

Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you.

## Build & Test

```bash
cargo build                  # Debug build
cargo build --release        # Release build, default features (hive only) — ~3.5 MB
cargo build --release --features "bus,coord,relay,hive"   # Full feature set — ~6.3 MB; what the Homebrew bottle ships
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
                       #   main.rs + brain/ + bus/ + coord/ + hive/ + relay/ +
                       #   orchestrator + init + commands + config.rs +
                       #   brain_screen.rs.
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
- `store.rs` — SQLite (WAL) at `~/.claudectl/bus/bus.db`: roles + messages tables, drain-on-read semantics
- `roles.rs` — Role addressing, cwd-inference, ambiguity/unbound resolution. Caller may override with `CLAUDECTL_BUS_ROLE` or `--role`
- `policy.rs` — Phase-4 guardrails: subject grammar, type allowlist, body cap, leading-`/` neutralization (§9)
- `mcp.rs` — rmcp stdio server exposing `whoami`, `list_agents`, `publish`, `read_inbox`
- `cli.rs` — `claudectl bus` subcommand (stdio, role bind/list, send, inbox, whoami)

**Hive** (`src/hive/`): Gossip-based knowledge sharing across connected brains (feature-gated behind `relay`).
- `mod.rs` — KnowledgeUnit, KnowledgeScope, KnowledgeContent types, semantic key, broadcast channel
- `store.rs` — JSONL-backed knowledge store with semantic index, atomic save
- `distiller.rs` — Converts DistilledPreferences/Insights into KnowledgeUnits with thresholds
- `merger.rs` — Conflict resolution: local always wins, peer-vs-peer by confidence*evidence
- `gossip.rs` — GossipEngine: incremental sync, snapshot pagination, epidemic propagation
- `trust.rs` — PeerTrust with auto-drift, TrustTier classification, TrustStore persistence
- `injection.rs` — Brain prompt integration with trust labels, concordance checking for drift
- `cli.rs` — CLI dispatch for hive subcommands (status, knowledge, export, import, trust)

**Terminal backends** (`crates/claudectl-core/src/terminals/`): Ghostty, Kitty, tmux, WezTerm, Warp, iTerm2, Terminal.app, Gnome Terminal, Windows Terminal — auto-detected, used for tab switching and input sending.

## Key Design Decisions

- **Minimal dependencies** — 7 runtime crates. Binary must stay under 1MB, startup under 50ms. **Exception:** the `bus` feature deliberately relaxes this (pulls rmcp + Tokio + schemars) because every available MCP SDK is async; the default build still honors the invariant.
- **Native `ps`** over `sysinfo` crate to keep binary small.
- **Multi-signal status inference** — combines CPU usage, JSONL events, and timestamps (not just one signal).
- **Incremental JSONL parsing** — tracks file offsets, never rereads full files.
- **No async runtime** — synchronous with polling. Keeps complexity low. **Exception:** the `bus` MCP server (`src/bus/mcp.rs`) runs inside a current-thread Tokio runtime when invoked as `claudectl bus stdio`. The TUI and every other code path remain sync.
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

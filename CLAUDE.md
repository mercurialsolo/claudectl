# claudectl

Mission control for Claude Code — supervise, budget, orchestrate, and auto-pilot sessions with a local LLM brain.

## Build & Test

```bash
cargo build                  # Debug build
cargo build --release        # Release build (optimized, <1MB binary)
cargo test                   # Run all tests
cargo clippy -- -D warnings  # Lint (warnings are errors in CI)
cargo fmt --check            # Check formatting
```

## Architecture

**Core modules** (`src/`):
- `main.rs` — CLI entry point, mode dispatch (TUI, watch, JSON, list, history, stats, orchestrator, clean, doctor, brain-eval, brain-query, mode, insights, init)
- `app.rs` — TUI app state, refresh loop, keyboard event handling
- `session.rs` — Session data structures and formatting
- `discovery.rs` — Scans `~/.claude/sessions/*.json` and resolves JSONL paths
- `monitor.rs` — Parses JSONL conversation logs for tokens, cost, status events
- `process.rs` — Process introspection via native `ps` (not sysinfo crate)
- `config.rs` — Layered TOML config: CLI flags > `.claudectl.toml` > `~/.config/claudectl/config.toml` > defaults
- `history.rs` — Session history persistence and cost analytics
- `hooks.rs` — Event hook system (shell commands fired on session events)
- `orchestrator.rs` — Multi-session task runner with dependency ordering
- `health.rs` — Session health monitoring (cache ratio, cost spikes, loop detection, stalls, context saturation)
- `rules.rs` — Auto-rule engine: match sessions by status/tool/command/project/cost, then approve/deny/send/terminate/route/spawn/delegate
- `launch.rs` — Launch and resume Claude Code sessions from the TUI or CLI
- `models.rs` — Model pricing profiles (built-in + user overrides) for cost tracking
- `recorder.rs` — Dashboard recording (asciicast/GIF capture of full TUI)
- `session_recorder.rs` — Per-session highlight reel recording (extracts edits, commands, errors; strips idle time)
- `transcript.rs` — JSONL transcript parser (messages, tool use, tool results, usage data)
- `metrics.rs` — Brain effectiveness metrics: learning curve, accuracy breakdown, rules baseline comparison, false-approve rate
- `demo.rs` — Deterministic fake sessions for screenshots, recordings, and demos. Includes `DemoHighlightState` which drip-feeds scripted JSONL events so session recording works in demo mode.
- `theme.rs` — Color theming (dark/light/monochrome, respects NO_COLOR)
- `logger.rs` — Structured diagnostic logging
- `init.rs` — `--init` / `--uninstall`: writes/removes Claude Code hooks in `.claude/settings.json`

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

**Hive** (`src/hive/`): Gossip-based knowledge sharing across connected brains (feature-gated behind `relay`).
- `mod.rs` — KnowledgeUnit, KnowledgeScope, KnowledgeContent types, semantic key, broadcast channel
- `store.rs` — JSONL-backed knowledge store with semantic index, atomic save
- `distiller.rs` — Converts DistilledPreferences/Insights into KnowledgeUnits with thresholds
- `merger.rs` — Conflict resolution: local always wins, peer-vs-peer by confidence*evidence
- `gossip.rs` — GossipEngine: incremental sync, snapshot pagination, epidemic propagation
- `trust.rs` — PeerTrust with auto-drift, TrustTier classification, TrustStore persistence
- `injection.rs` — Brain prompt integration with trust labels, concordance checking for drift
- `cli.rs` — CLI dispatch for hive subcommands (status, knowledge, export, import, trust)

**TUI** (`src/ui/`): `table.rs` (session list), `detail.rs` (expanded panel), `help.rs` (overlay), `status_bar.rs` (footer), `peers.rs` (relay peers panel)

**Terminal backends** (`src/terminals/`): Ghostty, Kitty, tmux, WezTerm, Warp, iTerm2, Terminal.app, Gnome Terminal, Windows Terminal — auto-detected, used for tab switching and input sending.

## Key Design Decisions

- **Minimal dependencies** — 7 runtime crates. Binary must stay under 1MB, startup under 50ms.
- **Native `ps`** over `sysinfo` crate to keep binary small.
- **Multi-signal status inference** — combines CPU usage, JSONL events, and timestamps (not just one signal).
- **Incremental JSONL parsing** — tracks file offsets, never rereads full files.
- **No async runtime** — synchronous with polling. Keeps complexity low.
- **Deny-first rule evaluation** — deny rules always override approve/brain suggestions, regardless of config order.
- **Brain decisions are local-only** — all decision logs and few-shot examples stay on the user's machine.
- **Brain gate mode** — `~/.claudectl/brain/gate-mode` controls on/off/auto. File absent = on (default). The plugin hook and `--brain-query` both check this before querying the LLM.
- **Insights mode** — `~/.claudectl/brain/insights-mode` controls auto-generation of insights. File absent = off (opt-in). When on, insights are generated alongside preference distillation every 10 decisions.

## Conventions

- Run `cargo fmt` and `cargo clippy -- -D warnings` before committing.
- Tests live in `tests/integration_tests.rs` and `tests/unit_tests.rs`.
- Status inference logic has extensive test coverage — do not change status detection without updating tests.
- Health checks in `health.rs` have full unit test coverage — add tests for new checks.
- Terminal backends implement the pattern in `src/terminals/mod.rs` — add new terminals there.
- Config fields must be added to all three layers (CLI args in `main.rs`, TOML struct in `config.rs`, merge logic in `config.rs`).
- Brain prompt templates can be overridden by placing files in `~/.claudectl/brain/prompts/` — run `--brain-prompts` to list sources.
- Plugin hook scripts must check for `claudectl` availability and exit 0 on failure — never block Claude Code.
- Plugin commands call `claudectl` CLI modes and format output — they don't implement logic directly.

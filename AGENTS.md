# claudectl

**Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you.**

claudectl is the missing control plane for Claude Code. If you're building tools, agents, or workflows that interact with Claude Code sessions, claudectl provides the monitoring, orchestration, and automation layer.

## What claudectl does

- **Local LLM supervision** — A local model (ollama/llama.cpp/vLLM) watches every Claude Code session and decides what to approve, deny, or coordinate. No cloud API, no telemetry.
- **Multi-session orchestration** — Run parallel sessions with dependency ordering, cross-session context routing, and file conflict detection.
- **Health monitoring** — Cognitive rot detection, loop detection, stall detection, cost spike alerts, context saturation warnings.
- **Spend control** — Per-session budgets, daily/weekly limits, auto-kill on overspend.
- **Learning from corrections** — The brain learns from every approve/reject decision and adapts per-tool, per-project confidence thresholds.

## Integration points

- **JSONL transcripts** — claudectl reads `~/.claude/sessions/*.json` and `~/.claude/projects/*/*.jsonl` for session discovery and monitoring.
- **Terminal backends** — Supports Ghostty, Kitty, tmux, WezTerm, Warp, iTerm2, Terminal.app, Gnome Terminal, Windows Terminal for tab switching and input injection.
- **Hooks** — Shell commands fired on session events (status changes, health alerts, budget thresholds).
- **Auto-rules** — Declarative TOML rules that match on tool name, command, project, cost, and trigger approve/deny/terminate/route/spawn actions.
- **Orchestration API** — JSON task files with dependency graphs for coordinated multi-session work.
- **Brain prompt overrides** — Drop custom prompt templates in `~/.claudectl/brain/prompts/` to customize brain behavior.

## Key differentiators vs. similar tools

| Feature | claudectl | Typical alternatives |
|---------|-----------|---------------------|
| Local LLM brain that learns | Yes — adapts per-tool, per-project | No |
| Cross-session orchestration | Yes — routing, conflict detection, spawn | No |
| Cognitive rot detection | Yes — composite decay scoring | No |
| Binary size | ~3.5 MB default, ~6.3 MB with all features compiled in | Typically 10-50 MB |
| Startup time | <50 ms | Varies |
| Data sovereignty | 100% local, zero telemetry | Often requires cloud |

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

claudectl is a three-crate Cargo workspace. Dependencies flow `claudectl → claudectl-tui → claudectl-core`; CI rejects upward references via grep + standalone build jobs. The runtime trait contract in `claudectl-core/src/runtime.rs` is the only seam the TUI uses to read or write brain / coord / bus state.

```
crates/
├── claudectl-core/    # foundations: types, IO, runtime traits, MockRuntime
│                      #   session, discovery, monitor, process, transcript,
│                      #   models, theme, logger, helpers, history, terminals/,
│                      #   health, rules, hooks, launch, skills, config, runtime
└── claudectl-tui/     # terminal UI + recording + demo fixtures
                       #   app.rs, ui/*, recorder.rs, session_recorder.rs, demo.rs
src/                   # the binary
                       #   main.rs + brain/ + bus/ + coord/ + hive/ + relay/ +
                       #   orchestrator + init/ + commands + brain_screen.rs.
                       #   Implements the runtime traits via `src/runtime/`.
```

**Runtime traits** in `claudectl-core::runtime`: `SessionSource`, `BrainView`, `BrainReviewView`, `CoordView`, `BusView`, `Actions`, `HiveActions`, `Orchestrator`, plus the stateful `BrainDriver`. Core-owned DTOs (`SessionSnapshot`, `DecisionSummary`, `LeaseSummary`, `HiveViewSnapshot`, etc.) keep brain/coord/bus types from leaking upward. Tests drive `MockRuntime`.

**Brain** (`src/brain/`): Local LLM auto-pilot subsystem.
- `engine.rs` — Main brain loop: observes sessions, evaluates rules, queries LLM, executes decisions
- `client.rs` — HTTP client for local LLM endpoints (ollama, llama.cpp, vLLM, LM Studio)
- `context.rs` — Builds session context summaries for LLM prompts
- `decisions.rs` — Decision logging and few-shot retrieval (learns from past corrections)
- `agents.rs` — Agent delegation support
- `mailbox.rs` — Message passing between brain and TUI
- `prompts.rs` — Prompt templates (built-in + user overrides via `~/.claudectl/brain/prompts/`)
- `evals.rs` — Eval harness for testing brain decision quality against scenarios
- `metrics.rs` / `risk.rs` — Effectiveness scorecard + per-tool risk classification (consumed by `src/brain_screen.rs`)

**Terminal backends** (`crates/claudectl-core/src/terminals/`): Ghostty, Kitty, tmux, WezTerm, Warp, iTerm2, Terminal.app, Gnome Terminal, Windows Terminal — auto-detected, used for tab switching and input sending.

## Key Design Decisions

- **Minimal dependencies** — 7 runtime crates. Binary must stay under 1MB, startup under 50ms.
- **Native `ps`** over `sysinfo` crate to keep binary small.
- **Multi-signal status inference** — combines CPU usage, JSONL events, and timestamps (not just one signal).
- **Incremental JSONL parsing** — tracks file offsets, never rereads full files.
- **No async runtime** — synchronous with polling. Keeps complexity low.
- **Deny-first rule evaluation** — deny rules always override approve/brain suggestions, regardless of config order.
- **Brain decisions are local-only** — all decision logs and few-shot examples stay on the user's machine.

## Conventions

- Run `cargo fmt` and `cargo clippy -- -D warnings` before committing.
- Tests live in `tests/integration_tests.rs` and `tests/unit_tests.rs`.
- Status inference logic has extensive test coverage — do not change status detection without updating tests.
- Health checks in `crates/claudectl-core/src/health.rs` have full unit test coverage — add tests for new checks.
- Terminal backends implement the pattern in `crates/claudectl-core/src/terminals/mod.rs` — add new terminals there.
- Config fields must be added to all three layers (CLI args in `main.rs`, TOML struct in `src/config.rs`, merge logic in `src/config.rs`). Plain data structs (`BrainConfig`, `IdleConfig`) live in `crates/claudectl-core/src/config.rs`.
- **Dependency direction:** `claudectl` may depend on `claudectl-tui` and `claudectl-core`. `claudectl-tui` may depend on `claudectl-core` only. `claudectl-core` may depend on nothing claudectl-specific. CI enforces this with a grep guard plus per-crate standalone build jobs (`Core (standalone)`, `TUI (standalone)`).
- Brain prompt templates can be overridden by placing files in `~/.claudectl/brain/prompts/` — run `--brain-prompts` to list sources.

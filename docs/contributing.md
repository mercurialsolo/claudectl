# Contributing

Contributions are welcome.

## Setup

```bash
git clone https://github.com/mercurialsolo/claudectl.git
cd claudectl
cargo build                    # whole workspace
cargo test --all-targets
```

## Before Submitting

```bash
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check

# Standalone per-crate checks — same as CI. Catches creeping cross-deps
# that happen to compile inside the workspace but break the dependency
# direction.
cargo build -p claudectl-core
cargo build -p claudectl-tui
cargo test  -p claudectl-core
cargo test  -p claudectl-tui
```

## Guidelines

- **No new dependencies** without strong justification — the project stays lightweight
- **Test behavior, not implementation** — focus on what the code does
- **Match existing patterns** — look at similar code before writing new code
- **Keep commits atomic** — one logical change per commit

Not all contributions are code. Hooks, docs, config presets, terminal compatibility fixes, and packaging help are all valuable.

## Workspace layout

claudectl is a three-crate Cargo workspace. Dependencies flow strictly downward — `claudectl → claudectl-tui → claudectl-core`. The runtime trait contract in `claudectl-core/src/runtime.rs` is the only seam the TUI uses to read or write brain / coord / bus state.

```
crates/
├── claudectl-core/    # foundations: types, IO, runtime traits, MockRuntime
└── claudectl-tui/     # the terminal UI + recording + demo fixtures
src/                   # the binary: glue + brain/bus/coord/hive/relay + runtime adapters
```

CI rejects upward references in three ways: a grep guard against `crate::{brain,bus,coord,hive,relay,…}` inside `claudectl-core/src/`, plus the two standalone build jobs above. If you find yourself wanting to reach across, add a trait method to `runtime.rs` and implement it in `src/runtime/` instead.

## Where things live

### `crates/claudectl-core/src/` — foundations
| Module | Purpose |
|--------|---------|
| `runtime.rs` | UI ↔ runtime trait contract (8 traits + `BrainDriver`), DTOs, `Runtime` aggregate, `MockRuntime` |
| `session.rs` | Session data structures and formatting |
| `discovery.rs` | Session file scanning and JSONL path resolution |
| `monitor.rs` | JSONL parsing, token counting, status inference |
| `process.rs` | Process introspection via `ps` |
| `history.rs` | Session history persistence and analytics |
| `health.rs` | Session health monitoring (10 automated checks). Owns `HealthThresholds`. |
| `rules.rs` | Auto-rule engine (approve/deny/send/terminate/route/spawn/delegate) |
| `models.rs` | Model pricing profiles for cost tracking |
| `transcript.rs` | JSONL transcript parser |
| `theme.rs` | Color palette and theme modes |
| `logger.rs` | Diagnostic file logging |
| `helpers.rs` | Webhook, notification, kill_process, aggregate session |
| `hooks.rs` | Event hooks system and execution |
| `launch.rs` | Launch and resume Claude Code sessions |
| `skills.rs` | Skill registry + claude-plugin metadata |
| `config.rs` | `BrainConfig`, `IdleConfig`, `IdleTask` data structs |
| `terminals/` | Terminal-specific switching and input injection |

### `crates/claudectl-tui/src/` — terminal UI
| Module | Purpose |
|--------|---------|
| `app.rs` | `App` state struct, refresh loop, event handling — talks to brain/bus/coord only through the runtime traits |
| `ui/` | Render modules: `table`, `detail`, `help`, `status_bar`, `peers` (feature `relay`), `skills` |
| `recorder.rs` | Asciicast / GIF recording |
| `session_recorder.rs` | Per-session highlight reel generator |
| `demo.rs` | Deterministic fake sessions for demo mode + `demo_peers` (feature `relay`) |

Features: `coord`, `relay`, `hive` mirror the binary's same-named features.

### `src/` — the binary
| Module | Purpose |
|--------|---------|
| `main.rs` | CLI entry point, mode dispatch |
| `brain_screen.rs` | Full-screen Brain Review surface (kept here because it depends on `brain::metrics` + `brain::risk`) |
| `commands.rs` | Non-TUI command dispatch |
| `config.rs` | Layered TOML config parsing (CLI > project > global > defaults). Re-exports the data structs from `claudectl-core::config`. |
| `init/` | `claudectl init` onboarding wizard (5 phases) |
| `orchestrator.rs` | Multi-session task runner with dependency ordering |
| `runtime/` | `Live*` adapters wiring the runtime traits to the real subsystems |
| `brain/` | Local LLM auto-pilot (engine, decisions, insights, evals, preferences, risk, metrics) |
| `relay/` | Cross-machine TCP transport, invite codes, LAN discovery, task delegation (feature `relay`) |
| `hive/` | Gossip-based knowledge sharing, trust tiers, distillation (feature `hive`) |
| `coord/` | Multi-session coordination: leases, blockers, handoffs, interrupts (feature `coord`) |
| `bus/` | Agent bus MCP server + SQLite mailbox (feature `bus`) |

## Reporting Issues

Found a bug? [Open an issue](https://github.com/mercurialsolo/claudectl/issues/new) with `claudectl --version`, your terminal (`echo $TERM_PROGRAM`), and steps to reproduce.

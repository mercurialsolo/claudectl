# Contributing

Contributions are welcome.

## Setup

```bash
git clone https://github.com/mercurialsolo/claudectl.git
cd claudectl
cargo build
cargo test --all-targets
```

## Before Submitting

```bash
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Guidelines

- **No new dependencies** without strong justification — the project stays lightweight
- **Test behavior, not implementation** — focus on what the code does
- **Match existing patterns** — look at similar code before writing new code
- **Keep commits atomic** — one logical change per commit

Not all contributions are code. Hooks, docs, config presets, terminal compatibility fixes, and packaging help are all valuable.

## Architecture

| Module | Purpose |
|--------|---------|
| `main.rs` | CLI entry point, mode dispatch |
| `app.rs` | Core app state, refresh loop, event handling |
| `session.rs` | Session data structures and formatting |
| `discovery.rs` | Session file scanning and JSONL path resolution |
| `monitor.rs` | JSONL parsing, token counting, status inference |
| `process.rs` | Process introspection via `ps` |
| `config.rs` | TOML config file loading and layering |
| `commands.rs` | Non-TUI command dispatch (headless, autopsy, context rot) |
| `health.rs` | Session health monitoring (10 automated checks) |
| `rules.rs` | Auto-rule engine (approve/deny/send/terminate/route/spawn) |
| `models.rs` | Model pricing profiles for cost tracking |
| `transcript.rs` | JSONL transcript parser (messages, tool use, usage data) |
| `launch.rs` | Launch and resume Claude Code sessions |
| `history.rs` | Session history persistence and analytics |
| `orchestrator.rs` | Multi-session task runner with dependency ordering |
| `hooks.rs` | Event hooks system and execution |
| `init.rs` | `--init` / `--uninstall` for Claude Code hooks integration |
| `brain/` | Local LLM auto-pilot (engine, decisions, insights, evals, preferences, risk, autopsy, metrics) |
| `relay/` | Cross-machine TCP transport, invite codes, LAN discovery, task delegation (feature-gated) |
| `hive/` | Gossip-based knowledge sharing, trust tiers, distillation (feature-gated) |
| `coord/` | Multi-session coordination: leases, blockers, handoffs, interrupts, memory (feature-gated) |
| `theme.rs` | Color palette and theme modes |
| `logger.rs` | Diagnostic file logging |
| `demo.rs` | Deterministic fake sessions for demo mode |
| `recorder.rs` | Asciicast recording with tee writer |
| `session_recorder.rs` | Per-session highlight reel generator |
| `terminals/` | Terminal-specific switching and input injection |
| `ui/` | TUI rendering (table, detail, help, status bar, peers) |

## Reporting Issues

Found a bug? [Open an issue](https://github.com/mercurialsolo/claudectl/issues/new) with `claudectl --version`, your terminal (`echo $TERM_PROGRAM`), and steps to reproduce.

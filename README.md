# claudectl

A fast, lightweight TUI for monitoring and managing multiple [Claude Code](https://claude.ai/claude-code) CLI sessions running across terminals.

Built in Rust. ~1MB binary. Sub-50ms startup.

## Features

- **Live dashboard** — PID, project, status, model, TTY, elapsed time, CPU%, memory, token counts, estimated cost
- **Smart status detection** — Processing / Paused / Waiting / Idle / Finished, inferred from JSONL events (`stop_reason`, `waiting_for_task`), CPU usage, and message timestamps
- **Cost tracking** — Per-session and total USD estimates based on model pricing (Opus, Sonnet, Haiku)
- **Tab switching** — Press `Tab` to jump to a session's terminal tab (Warp, iTerm2, Terminal.app), with automatic split-pane cycling
- **Kill sessions** — Press `d` twice to terminate a runaway session
- **Non-interactive mode** — `claudectl --list` for scripts and quick checks

## Install

### From source

```bash
cargo install --path .
```

### Homebrew

```bash
brew tap mercurialsolo/tap
brew install claudectl
```

## Usage

```bash
# Launch the TUI dashboard
claudectl

# Print session list and exit
claudectl --list

# Custom refresh interval (ms)
claudectl --interval 1000
```

## Keybindings

| Key | Action |
|-----|--------|
| `j` / `k` / `↑` / `↓` | Navigate sessions |
| `Tab` / `Enter` | Switch to session's terminal tab |
| `d` | Kill session (press twice to confirm) |
| `r` | Force refresh |
| `q` / `Esc` | Quit |

## Status Colors

| Status | Color | Meaning |
|--------|-------|---------|
| **Paused** | Magenta | Waiting for user to confirm/approve a tool use |
| **Processing** | Green | Actively generating or executing tools |
| **Waiting** | Yellow | Done responding, waiting for user's next prompt |
| **Idle** | Gray | No recent activity (>10 min since last message) |
| **Finished** | Red | Process exited |

## How It Works

claudectl reads Claude Code's local data:

- **`~/.claude/sessions/*.json`** — One file per running Claude process with PID, session ID, working directory, and start time
- **`~/.claude/projects/{slug}/*.jsonl`** — Conversation logs with token usage, model info, `stop_reason`, and `waiting_for_task` events
- **`ps`** — CPU%, memory, TTY, and command args for each process

Status is inferred from multiple signals:
- `waiting_for_task` progress event → **Paused** (needs user confirmation)
- CPU > 5% or `stop_reason: tool_use` → **Processing**
- `stop_reason: end_turn` + recent activity → **Waiting**
- Last message > 10 minutes ago → **Idle**

## Terminal Support

| Terminal | Tab Switch | Split Pane Focus |
|----------|-----------|-----------------|
| **Warp** | Navigation Palette + search | Cmd+] cycling with title detection |
| **iTerm2** | AppleScript TTY matching | Via tab selection |
| **Terminal.app** | AppleScript TTY matching | Via tab selection |

**Note:** Warp tab switching requires Accessibility permission (System Settings > Privacy & Security > Accessibility). Warp's search treats `-` as a negation operator, so project names with dashes use a truncated prefix.

## Requirements

- macOS (uses `ps`, AppleScript, and macOS-specific process APIs)
- [Claude Code CLI](https://claude.ai/claude-code) installed and running
- Rust 1.86+ (to build from source)

## License

MIT

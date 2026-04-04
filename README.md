# claudectl

A fast, lightweight TUI for monitoring and managing multiple [Claude Code](https://claude.ai/claude-code) CLI sessions running across terminals.

Built in Rust. ~1MB binary. Sub-50ms startup.

[![claudectl demo](https://asciinema.org/a/899569.svg)](https://asciinema.org/a/899569)

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

| Terminal | Method | How it works |
|----------|--------|-------------|
| **Ghostty** | AppleScript | `every terminal whose working directory contains X` + `focus` — exact TTY matching, best support |
| **iTerm2** | AppleScript | Iterates windows/tabs/sessions, matches by TTY device |
| **Kitty** | Remote control | `kitty @ focus-window --match pid:X` — requires `allow_remote_control` in kitty.conf |
| **WezTerm** | CLI | `wezterm cli list --format json` + `wezterm cli activate-pane --pane-id X` |
| **Warp** | UI automation | Navigation Palette search + split pane cycling via System Events |
| **Terminal.app** | AppleScript | Iterates windows/tabs, matches by TTY device |
| **tmux** | CLI | `tmux list-panes -a` + `tmux select-pane -t X` — works inside any terminal |

### Terminal-specific notes

- **Ghostty**: Best support. Native AppleScript with working directory and TTY matching. No extra config needed.
- **Kitty**: Requires `allow_remote_control yes` (or `socket-only`) in `~/.config/kitty/kitty.conf`.
- **Warp**: Requires Accessibility permission (System Settings > Privacy & Security > Accessibility). Warp's search treats `-` as negation, so project names with dashes use a truncated prefix or resume UUID.
- **tmux**: Auto-detected when running inside tmux. Works alongside the outer terminal's support.

## Requirements

- macOS (session discovery uses `~/.claude/sessions/` and `ps`)
- [Claude Code CLI](https://claude.ai/claude-code) installed and running
- Rust 1.86+ (to build from source)

## License

MIT

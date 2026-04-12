# claudectl

A fast, lightweight TUI for monitoring and managing multiple [Claude Code](https://claude.ai/claude-code) CLI sessions running across terminals.

Built in Rust. ~1MB binary. Sub-50ms startup.

[![claudectl demo](https://asciinema.org/a/899569.svg)](https://asciinema.org/a/899569)

## Features

- **Live dashboard** — PID, project, status, context window %, cost, $/hr burn rate, elapsed time, CPU%, memory, token counts, activity sparkline
- **Smart status detection** — Processing / Needs Input / Waiting / Idle / Finished, inferred from JSONL events, CPU usage, and message timestamps
- **Cost tracking** — Per-session and total USD estimates based on model pricing (Opus, Sonnet, Haiku) with burn rate
- **Budget enforcement** — Per-session budget alerts at 80%, optional auto-kill at 100%
- **Approve/input** — Press `y` to approve permission prompts, `i` to type input to sessions
- **Auto-approve** — Press `a` twice to enable auto-approve for trusted sessions
- **Tab switching** — Press `Tab` to jump to a session's terminal tab (7 terminals supported)
- **Session launcher** — Press `n` to start a new Claude Code session from within claudectl
- **Grouped view** — Press `g` to group sessions by project with aggregate stats
- **Detail panel** — Press `Enter` to expand session details (tokens, cost, model, paths)
- **Notifications** — Desktop notifications when sessions need input (`--notify`)
- **Webhooks** — POST JSON to Slack/Discord/URL on status changes (`--webhook`)
- **Watch mode** — Stream status changes without TUI (`--watch`)
- **Session history** — Persist completed sessions and view cost analytics (`--history`, `--stats`)
- **Configuration file** — Persistent settings via `~/.config/claudectl/config.toml`
- **Theme system** — Dark, light, and monochrome themes (`--theme`, `NO_COLOR` support)
- **Task orchestration** — Run multiple Claude sessions with dependency ordering (`--run`)
- **Diagnostic logging** — Structured debug output for troubleshooting (`--log`)

## Install

### Homebrew (macOS)

```bash
brew tap mercurialsolo/tap
brew install claudectl
```

### Quick install (macOS / Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/mercurialsolo/claudectl/main/install.sh | sh
```

### From source

```bash
cargo install --path .
```

### Nix

```bash
nix run github:mercurialsolo/claudectl
```

## Usage

```bash
# Launch the TUI dashboard
claudectl

# Print session list and exit
claudectl --list

# Export JSON for scripting
claudectl --json

# Stream status changes (no TUI)
claudectl --watch
claudectl --watch --json

# Session history and cost analytics
claudectl --history --since 24h
claudectl --stats --since 7d

# Launch a new Claude session
claudectl --new --cwd ~/projects/my-app --prompt "Fix the auth bug"

# Budget enforcement
claudectl --budget 5 --kill-on-budget

# Notifications and webhooks
claudectl --notify
claudectl --webhook https://hooks.slack.com/... --webhook-on NeedsInput,Finished

# Theme and diagnostics
claudectl --theme light
claudectl --log /tmp/claudectl.log

# Run multiple tasks from a file
claudectl --run tasks.json --parallel

# Show resolved configuration
claudectl --config
```

## Configuration

claudectl loads settings from `~/.config/claudectl/config.toml` (global) and `.claudectl.toml` (per-project). CLI flags override config file values.

```toml
[defaults]
interval = 1000
notify = true
grouped = true
sort = "cost"
budget = 5.00
kill_on_budget = false

[webhook]
url = "https://hooks.slack.com/..."
events = ["NeedsInput", "Finished"]
```

## Task Orchestration

Run multiple Claude sessions with dependency ordering:

```json
{
  "tasks": [
    {
      "name": "Add auth middleware",
      "cwd": "./backend",
      "prompt": "Add JWT auth middleware to all API routes"
    },
    {
      "name": "Update tests",
      "cwd": "./backend",
      "prompt": "Update API tests for the new auth middleware",
      "depends_on": ["Add auth middleware"]
    },
    {
      "name": "Update docs",
      "cwd": "./docs",
      "prompt": "Document the new auth flow"
    }
  ]
}
```

```bash
claudectl --run tasks.json --parallel
```

## Keybindings

| Key | Action |
|-----|--------|
| `j`/`k` or `Up`/`Down` | Navigate sessions |
| `Tab` | Switch to session's terminal tab |
| `Enter` | Toggle detail panel |
| `y` | Approve (send Enter to NeedsInput session) |
| `i` | Input mode (type text to session) |
| `d`/`x` | Kill session (double-tap to confirm) |
| `a` | Toggle auto-approve (double-tap to confirm) |
| `n` | Launch new Claude session |
| `g` | Toggle grouped view by project |
| `s` | Cycle sort column (Status, Context, Cost, $/hr, Elapsed) |
| `r` | Force refresh |
| `?` | Toggle help overlay |
| `q`/`Esc` | Quit |

## Status Colors

| Status | Color | Meaning |
|--------|-------|---------|
| **Needs Input** | Magenta | Waiting for user to approve/confirm a tool use |
| **Processing** | Green | Actively generating or executing tools |
| **Waiting** | Yellow | Done responding, waiting for user's next prompt |
| **Idle** | Gray | No recent activity (>10 min since last message) |
| **Finished** | Red | Process exited |

## How It Works

claudectl reads Claude Code's local data:

- **`~/.claude/sessions/*.json`** — One file per running Claude process with PID, session ID, working directory, and start time
- **`~/.claude/projects/{slug}/*.jsonl`** — Conversation logs with token usage, model info, `stop_reason`, and `waiting_for_task` events
- **`ps`** — CPU%, memory, TTY, and command args for each process
- **`/tmp/claude-{uid}/{slug}/{sessionId}/tasks/`** — Subagent task files

Status is inferred from multiple signals:
- `waiting_for_task` progress event → **Needs Input** (needs user confirmation)
- CPU > 5% → **Processing** (overrides all other signals)
- `stop_reason: tool_use` + low CPU + age >5s → **Needs Input** (permission prompt)
- `stop_reason: end_turn` + recent activity → **Waiting**
- Last message > 10 minutes ago → **Idle**

## Terminal Support

| Terminal | Tab Switch | Approve/Input | Method |
|----------|-----------|---------------|--------|
| **Ghostty** | Background | Background | Native AppleScript API |
| **Kitty** | Background | Background | `kitty @` remote control |
| **tmux** | Background | Background | `tmux send-keys` |
| **WezTerm** | Background | - | CLI JSON API |
| **Warp** | Focus switch | Focus switch | Command Palette + System Events |
| **iTerm2** | Focus switch | Focus switch | AppleScript + System Events |
| **Terminal.app** | Focus switch | Focus switch | AppleScript + System Events |

### Terminal-specific notes

- **Ghostty**: Best support. Native AppleScript with working directory and TTY matching. No extra config needed.
- **Kitty**: Requires `allow_remote_control yes` (or `socket-only`) in `~/.config/kitty/kitty.conf`.
- **Warp**: Requires Accessibility permission (System Settings > Privacy & Security > Accessibility). Approve/input briefly switches focus to the Claude tab, sends the keystroke, then you can switch back.
- **tmux**: Auto-detected when running inside tmux. Works alongside the outer terminal's support.

## Requirements

- macOS or Linux
- [Claude Code CLI](https://claude.ai/claude-code) installed and running
- Rust 2024 edition (to build from source)

## License

MIT

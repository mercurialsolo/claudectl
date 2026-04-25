# claudectl

**Mission control for Claude Code** - supervise, budget, orchestrate, and auto-pilot sessions with a local LLM brain.

<p class="hero-tagline">
Know which agent is blocked, burning budget, waiting for approval, or stalled - and intervene without tab hunting.
</p>

<div class="proof-strip">
  <span>~1 MB binary</span>
  <span>Sub-50ms startup</span>
  <span>Zero config</span>
  <span>macOS &amp; Linux</span>
</div>

![claudectl dashboard demo](assets/claudectl-demo-hero.gif){ .terminal-screenshot }

## Install

=== "Homebrew"

    ```bash
    brew install mercurialsolo/tap/claudectl
    ```

=== "Cargo"

    ```bash
    cargo install claudectl
    ```

Then wire up Claude Code hooks and start the dashboard:

```bash
claudectl --init    # one-time setup
claudectl           # launch dashboard
```

Or try it without Claude Code running:

```bash
claudectl --demo
```

See the [Quick Start](quickstart.md) for the full walkthrough.

## Features

<div class="feature-grid" markdown>
<div class="feature-item" markdown>

### Live Dashboard

See every session's status, burn rate, context usage, activity sparkline, CPU, memory, and subagent rows in one place.

</div>
<div class="feature-item" markdown>

### Intervene Fast

Approve prompts, send input, jump to the right terminal tab, or kill a runaway session - without leaving the dashboard.

</div>
<div class="feature-item" markdown>

### Budget Enforcement

Set per-session or daily spending limits. Alert at 80%, auto-kill at 100%. Track live $/hr burn rate.

</div>
<div class="feature-item" markdown>

### Local LLM Brain

A local model (ollama/gemma) watches sessions, auto-approves safe commands, denies dangerous ones. Learns from your corrections. All on-device.

</div>
<div class="feature-item" markdown>

### Auto-Rules Engine

TOML rules to approve, deny, send, terminate, route, or spawn based on tool name, command pattern, project, or cost threshold.

</div>
<div class="feature-item" markdown>

### Health Monitoring

10 automatic checks: stalled sessions, context saturation, cache ratio, cost spikes, retry loops, cognitive decay, proactive compaction, token efficiency, error acceleration, and repetition detection - no config needed.

</div>
<div class="feature-item" markdown>

### Multi-Session Orchestration

Run dependency-ordered task graphs across sessions. Decompose prompts into parallel DAGs.

</div>
<div class="feature-item" markdown>

### Event Hooks

Trigger desktop notifications, shell commands, and webhooks when sessions need attention.

</div>
<div class="feature-item" markdown>

### Session Recording

Press `R` to record a session highlight reel as a GIF. Extracts edits, commands, errors - strips idle time.

</div>
<div class="feature-item" markdown>

### Relay & Hive Mind

Connect claudectl instances across machines. Share brain learnings, delegate tasks, and build a convergent hive mind - all peer-to-peer. [Learn more](relay.md)

</div>
<div class="feature-item" markdown>

### Headless Daemon

Run without the TUI via `--headless`. Brain, coordination, and context rot prevention stay active while you work. Attach a dashboard from another terminal anytime.

</div>
<div class="feature-item" markdown>

### Session Autopsy

Post-mortem analysis on completed sessions via `--autopsy`. Inspect what went wrong, what burned cost, and where the session stalled - after the fact.

</div>
</div>

## Screenshots

Dashboard health monitoring:

![claudectl health monitoring](assets/demo-health.gif){ .terminal-screenshot }

## Status Detection

Multi-signal inference from CPU usage, JSONL events, and timestamps:

| Status | Meaning |
|--------|---------|
| **Needs Input** | Waiting for user to approve/confirm a tool use |
| **Processing** | Actively generating or executing tools |
| **Waiting** | Done responding, waiting for user's next prompt |
| **Unknown** | Process alive, but transcript telemetry unavailable |
| **Idle** | No recent activity (>10 min) |
| **Finished** | Process exited |

## Terminal Support

| Terminal | Launch | Switch | Input | Approve |
|----------|:------:|:------:|:-----:|:-------:|
| Ghostty | - | Yes | Yes | Yes |
| tmux | Yes | Yes | Yes | Yes |
| Kitty | Yes | Yes | Yes | Yes |
| Warp | - | Yes | Yes | Yes |
| iTerm2 | - | Yes | Yes | Yes |
| Terminal.app | - | Yes | Yes | Yes |
| WezTerm | Yes | Yes | - | - |
| GNOME Terminal | Yes | - | - | - |

Run `claudectl --doctor` to verify support in your terminal. See [Terminal Support](terminal-support.md) for setup notes.

## How It Works

claudectl reads Claude Code's local data - no API keys, no network access, no modifications to Claude Code:

- **`~/.claude/sessions/*.json`** - session metadata
- **`~/.claude/projects/{slug}/*.jsonl`** - conversation logs with token usage
- **`ps`** - CPU%, memory, TTY for each process

Status inference combines multiple signals: `waiting_for_task` events, CPU usage thresholds, `stop_reason` fields, and message recency.

## Security

claudectl runs entirely locally. It does not:

- Send data to any server (unless you configure webhooks)
- Modify Claude Code's files or behavior
- Require API keys or authentication
- Run with elevated privileges

## Built With

- [Rust](https://www.rust-lang.org/) - systems language
- [ratatui](https://github.com/ratatui/ratatui) - TUI framework
- [crossterm](https://github.com/crossterm-rs/crossterm) - terminal manipulation
- [Ollama](https://ollama.com/) - local LLM inference (for brain mode)

## License

MIT

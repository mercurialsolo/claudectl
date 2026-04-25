# Reference

## Dashboard Features

- Live table: PID, project, status, context %, cost, $/hr burn rate, elapsed, CPU%, memory, tokens, sparkline
- Parent sessions expand into subagent rows (completed totals + active subagents)
- Detail panel (`Enter`) with full session metadata
- Grouped view (`g`) by project with aggregate stats
- Sort by status, context, cost, burn rate, or elapsed (`s`)
- Live triage filters: status cycle (`f`), focus cycle (`v`), text search (`/`), clear (`z`)
- Conflict detection when 2+ sessions share the same git worktree (`!!`)
- Permission wait time — shows how long sessions have been waiting, longest first

## Status Detection

Multi-signal inference from CPU usage, JSONL events, and timestamps:

| Status | Color | Meaning |
|--------|-------|---------|
| **Needs Input** | Magenta | Waiting for user to approve/confirm a tool use |
| **Processing** | Green | Actively generating or executing tools |
| **Waiting** | Yellow | Done responding, waiting for user's next prompt |
| **Unknown** | Blue | Session is alive, but transcript telemetry is missing or unsupported |
| **Idle** | Gray | No recent activity (>10 min since last message) |
| **Finished** | Red | Process exited |

## Interactive Controls

| Key | Action |
|-----|--------|
| `j`/`k` or `Up`/`Down` | Navigate sessions |
| `Tab` | Switch to session's terminal tab |
| `Enter` | Toggle detail panel |
| `y` | Approve (send Enter to NeedsInput session) |
| `i` | Input mode (type text to session) |
| `d`/`x` | Kill session (double-tap to confirm) |
| `a` | Toggle auto-approve (double-tap to confirm) |
| `n` | Launch wizard for cwd, prompt, and resume |
| `g` | Toggle grouped view by project |
| `s` | Cycle sort column |
| `f` | Cycle status filter |
| `v` | Cycle focus filter (`attention`, budget, context, telemetry, conflicts) |
| `/` | Search project/model/session text |
| `z` | Clear all active filters |
| `c` | Send /compact to session (when idle) |
| `R` | Record session highlight reel (toggle) |
| `b` | Accept brain suggestion for selected session |
| `B` | Reject brain suggestion |
| `r` | Force refresh |
| `?` | Toggle help overlay |
| `q`/`Esc` | Quit |

## CLI Reference

### Dashboard

| Flag | Description |
|------|-------------|
| (no flags) | Interactive TUI dashboard |
| `-i`, `--interval <ms>` | Refresh interval in milliseconds (default: 2000) |
| `--theme <dark\|light\|none>` | Color theme. Respects `NO_COLOR` env var |
| `--debug` | Show timing metrics in the footer |
| `--demo` | Run with fake sessions for screenshots and demos |

### Output Modes

| Flag | Description |
|------|-------------|
| `-l`, `--list` | Print session table to stdout and exit |
| `--json` | Print JSON array of sessions and exit |
| `-w`, `--watch` | Stream status changes to stdout (no TUI) |
| `--headless` | Run headless with brain, coordination, and context rot prevention active (no TUI). Attach a dashboard with `claudectl` in another terminal |
| `--format <template>` | Custom format for `--watch`. Placeholders: `{pid}`, `{project}`, `{status}`, `{cost}`, `{context}` |
| `--summary` | Show activity summary and exit |
| `--since <duration>` | Time window for `--summary`, `--history`, `--stats` (e.g., "8h", "24h", "7d"). Default: 24h |

### Filtering

| Flag | Description |
|------|-------------|
| `--filter-status <status>` | Filter by status: NeedsInput, Processing, Waiting, Finished, etc. |
| `--focus <filter>` | High-signal subset: `attention`, `over-budget`, `high-context`, `unknown-telemetry`, `conflict` |
| `--search <text>` | Search project/model/session text |

### Session Management

| Flag | Description |
|------|-------------|
| `--new` | Launch a new Claude Code session |
| `--cwd <path>` | Working directory for the new session (default: `.`) |
| `--prompt <text>` | Prompt to send to the new session |
| `--resume <session-id>` | Resume a previous session by ID |

### Budget & Notifications

| Flag | Description |
|------|-------------|
| `--budget <usd>` | Per-session budget in USD. Alert at 80%, optionally kill at 100% |
| `--kill-on-budget` | Auto-kill sessions that exceed budget (requires `--budget`) |
| `--notify` | Desktop notifications on NeedsInput transitions |
| `--webhook <url>` | Webhook URL to POST JSON on status changes |
| `--webhook-on <statuses>` | Only fire webhook on these transitions (comma-separated, e.g. "NeedsInput,Finished") |

### Brain (Local LLM)

| Flag | Description |
|------|-------------|
| `--brain` | Enable local LLM brain for session advisory |
| `--auto-run` | Auto-execute brain suggestions without confirmation |
| `--url <endpoint>` | LLM endpoint URL (maps to config `[brain] endpoint`) |
| `--brain-model <name>` | Override brain model name (maps to config `[brain] model`) |
| `--brain-eval` | Run brain eval scenarios against the LLM and report results |
| `--brain-prompts` | List brain prompt templates and their source (built-in vs user override) |
| `--brain-stats <metric>` | Brain statistics: `impact`, `learning-curve`, `accuracy`, `baseline`, `false-approve` |
| `--brain-query` | Query brain for a single tool-call decision (JSON output) |
| `--tool <name>` | Tool name for `--brain-query` (e.g., "Bash", "Write") |
| `--tool-input <input>` | Command or file path for `--brain-query` |
| `--project <name>` | Project name for `--brain-query` (default: current directory name) |
| `--mode <on\|off\|auto\|status>` | Set brain gate mode (see Brain Gate Mode below) |
| `--insights [on\|off\|status]` | Show auto-generated insights, or set insights mode. Requires `--brain`. |

### Orchestration

| Flag | Description |
|------|-------------|
| `--decompose <prompt>` | Analyze a prompt and suggest parallel sub-tasks (outputs JSON) |
| `--run <file>` | Run tasks from a JSON file |
| `--parallel` | Run independent tasks in parallel (used with `--run`) |

### Recording

| Flag | Description |
|------|-------------|
| `--record <path>` | Record the TUI as an asciicast v2 file (e.g., `--record demo.cast`) |
| `--duration <secs>` | Auto-quit after N seconds (useful with `--demo --record`) |

Press `R` on any session to record a per-session highlight reel (edits, commands, errors — idle time stripped). In `--demo` mode, a scripted coding session is drip-fed so recording works without live sessions.

### Coordination (--features coord)

Inspect multi-session coordination state. Requires `cargo install claudectl --features coord`.

| Flag | Description |
|------|-------------|
| `--coord events [N] [type]` | Show last N coordination events (default 50), optionally filtered by type |
| `--coord leases` | Show active ownership leases |
| `--coord blockers` | Show open blockers |
| `--coord handoffs` | Show handoffs |
| `--coord interrupts` | Show pending interrupts |
| `--coord memory` | List recent coordination memory records |
| `--coord "memory search <q>"` | Full-text search coordination memory |
| `--coord "promote --project <name>"` | Promote brain patterns to coordination memory |
| `--coord "prune [--days N]"` | Delete old events, resolved blockers, expired leases (default: 30 days) |

### Relay (--features relay)

Connect machines, delegate tasks. See the [full relay guide](relay.md).

| Flag | Description |
|------|-------------|
| `--relay serve` | Start the relay listener for peer connections |
| `--relay invite [--qr] [--words]` | Generate invite code, link, and word phrase |
| `--relay "join <code>"` | Connect using any invite format (code, words, or link) |
| `--relay discover` | Scan LAN for nearby claudectl instances |
| `--relay peers` | List known and connected peers |
| `--relay "delegate <peer> <prompt>"` | Delegate a task to a remote peer |
| `--relay identity` | Show this instance's relay identity |

### Hive Mind (--features hive)

Share knowledge, distill learnings. Requires relay for transport.

| Flag | Description |
|------|-------------|
| `--hive status` | Knowledge store overview (units, categories, conflicts) |
| `--hive knowledge [--from X]` | List knowledge units, filter by peer or scope |
| `--hive trust [<peer> [<level>]]` | Show or set peer trust levels |
| `--hive export` | Export all knowledge as JSON |
| `--hive "import <file>"` | Import knowledge from JSON file |
| `--hive archive` | Show cold storage archive stats |
| `--hive distill` | Run distillation pipeline (dedup, condense, curriculum) |
| `--hive curriculum` | Show distilled curriculum |

### Cleanup

| Flag | Description |
|------|-------------|
| `--clean` | Clean up old session data (JSONL transcripts, session JSON files) |
| `--older-than <duration>` | Only clean sessions older than this (e.g., "7d", "24h") |
| `--finished` | Only clean sessions that have finished |
| `--dry-run` | Show what would be removed without deleting |

### History & Diagnostics

| Flag | Description |
|------|-------------|
| `--autopsy` | Run post-mortem analysis on a completed session transcript |
| `--session <id>` | Session ID or JSONL path for `--autopsy` (defaults to most recent session) |
| `--history` | Show completed session history and exit |
| `--stats` | Show aggregated session statistics and exit |
| `--config` | Show resolved configuration and exit |
| `--config-template` | Print annotated default config template to stdout |
| `--hooks` | List configured event hooks and exit |
| `--doctor` | Diagnose terminal integration and setup |
| `--log <path>` | Write diagnostic logs to a file |

### Setup

| Flag | Description |
|------|-------------|
| `--init` | Wire up Claude Code hooks in settings and exit |
| `--uninstall` | Remove claudectl hooks from settings and exit |
| `-s`, `--scope <user\|project>` | Configuration scope (default: `user`). Matches Claude Code's `--scope` convention |

`--init` writes three hooks into Claude Code's settings:

| Hook | Matcher | Purpose |
|------|---------|---------|
| `PreToolUse` | `Bash` | Lets claudectl observe commands before execution |
| `PostToolUse` | `*` | Notifies claudectl after every tool completion |
| `Stop` | (all) | Notifies claudectl when a session ends |

The hooks call `claudectl --json` on each event. They are safe to run alongside any existing hooks — `--init` merges without overwriting.

`--uninstall` removes only claudectl hook entries, preserving all other settings and hooks. If the file becomes empty after removal, it is deleted.

| Scope | Flag | File | Committed to git? |
|-------|------|------|--------------------|
| `user` (default) | `--init` | `~/.claude/settings.json` | No (user home) |
| `project` | `--init -s project` | `.claude/settings.local.json` | No (gitignored) |

## Cost Tracking

- Per-session USD estimates (Opus, Sonnet, Haiku model pricing)
- Live $/hr burn rate
- Per-session budget alerts at 80%, auto-kill at 100%
- Daily/weekly aggregate cost tracking in title bar
- Unknown models marked as fallback estimates until overridden in config

## Themes

Dark, light, and none (`--theme`). Respects `NO_COLOR` environment variable.

## How It Works

claudectl reads Claude Code's local data — no API keys, no network access, no modifications to Claude Code:

- **`~/.claude/sessions/*.json`** — session metadata (PID, session ID, working directory, start time)
- **`~/.claude/projects/{slug}/*.jsonl`** — conversation logs with token usage and events
- **`ps`** — CPU%, memory, TTY for each process
- **`/tmp/claude-{uid}/{slug}/{sessionId}/tasks/`** — subagent task files

Status inference combines multiple signals: `waiting_for_task` events, CPU usage thresholds, `stop_reason` fields, and message recency.

### Brain Query

Query the brain for a single tool-call decision without the TUI. Used by the Claude Code plugin hook, but also useful for scripting and testing:

```bash
claudectl --brain --brain-query --tool Bash --tool-input "rm -rf /tmp"
claudectl --brain --brain-query --tool Write --tool-input "src/main.rs" --project myapp
```

Output is JSON:

```json
{"action":"deny","reasoning":"Destructive command","confidence":0.95,"source":"brain","below_threshold":false,"threshold":0.6}
```

The decision flow is: deny rules (instant) -> approve rules (instant) -> LLM query -> adaptive threshold check.

If the brain is unreachable, returns `{"action":"abstain","source":"error"}` so callers are never blocked.

### Brain Gate Mode

Control whether the brain hook evaluates tool calls:

```bash
claudectl --mode on                    # Brain evaluates tool calls (default)
claudectl --mode off                   # Disable brain — all calls pass through
claudectl --mode auto                  # Brain auto-approves above threshold
claudectl --mode status                # Show current mode
```

| Mode | Approves safe calls | Denies dangerous calls | Low-confidence calls |
|------|:---:|:---:|:---:|
| `on` | Yes | Yes | Fall through to user |
| `auto` | Yes | Yes | Auto-approve |
| `off` | No | No | Fall through to user |

Mode is stored in `~/.claudectl/brain/gate-mode`. File absent = `on` (default).

## Claude Code Plugin

claudectl includes a Claude Code plugin in `claude-plugin/` that integrates the brain directly into sessions.

### Plugin Components

| Component | Type | What it does |
|-----------|------|-------------|
| `brain-gate.sh` | PreToolUse hook | Queries the brain before Bash/Write/Edit/NotebookEdit calls |
| `budget-check.sh` | PreToolUse hook | Denies tool calls when session exceeds budget |
| `/brain` | Command | Toggle brain mode: `/brain on`, `/brain off`, `/brain auto` |
| `/sessions` | Command | Show all active sessions with status, cost, and health |
| `/spend` | Command | Cost breakdown by project and time window |
| `/brain-stats` | Command | Brain learning metrics and accuracy |
| `/auto-insights` | Command | Show or configure auto-generated workflow insights |
| Supervisor | Agent | Proactive session health triage |
| Session Monitoring | Skill | Auto-activated awareness of claudectl capabilities |

### How the brain gate hook works

1. Claude Code fires a PreToolUse event with the tool name and input
2. The hook checks `~/.claudectl/brain/gate-mode` — if `off`, exits immediately
3. Calls `claudectl --brain --brain-query --tool <name> --tool-input <input>`
4. claudectl checks static deny/approve rules first (instant, no LLM)
5. If no rule matches, queries the local LLM brain
6. Returns `{"decision":"approve"}` or `{"decision":"deny","reason":"..."}` to Claude Code

In `on` mode, low-confidence brain approvals fall through to normal permission prompts. In `auto` mode, all brain approvals execute.

## Security

claudectl runs entirely locally. It reads Claude Code's session files from disk and process data from `ps`. It does not:
- Send data to any server (unless you configure webhooks or the brain feature)
- Modify Claude Code's files or behavior
- Require API keys or authentication
- Run with elevated privileges

Webhook payloads contain session metadata (project name, cost, status). Review your webhook URL and event filters before enabling.

The brain feature sends session context to a **local** LLM endpoint (default `localhost:11434`). No data leaves your machine unless you point `--url` at a remote server.

## Comparison

claudectl was the first tool to combine local LLM supervision with multi-session orchestration for Claude Code (shipped April 2026).

| Capability | Claude Code alone | With claudectl |
|-----------|:-:|:-:|
| Local LLM auto-approve/deny | No | Brain with ollama |
| Self-improving insights | No | Friction detection, rule suggestions |
| Session health monitoring | No | 10 checks: cognitive decay, cost spikes, loops, stalls, context, cache, compaction, token efficiency, error acceleration, repetition |
| Orchestrate multi-session workflows | No | Dependency-ordered tasks |
| See status of all sessions at once | No | Live dashboard |
| Track cost per session | Manually | Live $/hr burn rate |
| Enforce spend budgets | No | Auto-kill at limit |
| File conflict detection | No | Auto-detect + brain pre-check + auto-deny |
| Headless daemon mode | No | `--headless` with brain, coordination, and context rot prevention |
| Session autopsy / post-mortem | No | `--autopsy` on completed session transcripts |
| Idle mode / unattended work | No | Run tasks while you sleep |
| Session auto-restart | No | Checkpoint + restart on context saturation |
| Task decomposition | No | Splits prompts into parallel DAGs |
| Auto-rule engine | No | Match by tool/command/project/cost |
| Approve prompts without switching | No | Press `y` |
| Record session highlight reels | No | Press `R` |
| Claude Code plugin | No | `/brain`, `/sessions`, `/spend`, `/auto-insights` |

| Cross-machine knowledge sharing | No | Peer-to-peer hive mind |
| Remote task delegation | No | Delegate to connected peers |

| Feature | claudectl | Static auto-approve tools | Cloud-based supervisors |
|---------|:---------:|:-------------------------:|:-----------------------:|
| Local LLM brain that learns your preferences | Yes | No | No |
| Cross-session orchestration + context routing | Yes | No | Varies |
| 10-check health monitoring + context rot detection | Yes | No | No |
| File conflict detection across sessions | Yes | No | No |
| Per-tool adaptive confidence thresholds | Yes | No | No |
| Task decomposition into parallel DAGs | Yes | No | No |
| Binary size | <1 MB | Varies | N/A |
| Startup time | <50 ms | Varies | N/A |
| Data stays on your machine | 100% | Usually | No |

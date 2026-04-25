# Configuration

claudectl loads settings from three layers (highest priority first):

1. **CLI flags** — override everything
2. **`.claudectl.toml`** — per-project config in the working directory
3. **`~/.config/claudectl/config.toml`** — global config

Show resolved config: `claudectl --config`

## Full Example

```toml
[defaults]
interval = 2000
notify = true
grouped = true
sort = "cost"
budget = 5.00
kill_on_budget = false

[budget]
daily_limit = 25.00
weekly_limit = 100.00

[webhook]
url = "https://hooks.slack.com/..."
events = ["NeedsInput", "Finished"]

[context]
warn_threshold = 75

[brain]
enabled = true
endpoint = "http://localhost:11434/api/generate"
model = "gemma4:e4b"
auto = false
timeout_ms = 5000
max_context_tokens = 4000
few_shot_count = 5

[models."gpt-4o"]
input_per_m = 1.25
output_per_m = 5.0
cache_read_per_m = 0.15
cache_write_per_m = 0.9
context_max = 128000
```

## Rule-Based Auto-Actions

Configure `[rules.*]` sections to automatically approve, deny, send messages, or terminate sessions based on tool name, command pattern, project, cost threshold, and error state.

Deny rules always override approve rules regardless of config order.

## Event Hooks

Run shell commands automatically when session events occur:

```toml
[hooks.on_needs_input]
run = "say 'Claude needs your attention'"

[hooks.on_finished]
run = "terminal-notifier -title 'claudectl' -message '{project} finished (${cost})'"

[hooks.on_budget_warning]
run = "curl -X POST $SLACK_WEBHOOK -d '{\"text\": \"{project} hit 80% budget (${cost})\"}'"

[hooks.on_status_change]
run = "echo '[{project}] {old_status} -> {new_status}' >> ~/claude-activity.log"
```

### Events

| Event | Trigger |
|-------|---------|
| `on_session_start` | New session discovered |
| `on_status_change` | Any status transition |
| `on_needs_input` | Session needs user approval/input |
| `on_finished` | Session process exited |
| `on_budget_warning` | Session hit 80% of budget |
| `on_budget_exceeded` | Session hit 100% of budget |
| `on_idle` | Session went idle (>10 min) |
| `on_context_high` | Context window usage crossed threshold (default 75%) |
| `on_conflict_detected` | 2+ sessions share the same working directory |

### Template Variables

`{pid}`, `{project}`, `{status}`, `{cost}`, `{model}`, `{cwd}`, `{tokens_in}`, `{tokens_out}`, `{elapsed}`, `{session_id}`, `{old_status}`, `{new_status}`, `{context_pct}`

Use `claudectl --hooks` to verify your configured hooks.

### Verified Hooks

We maintain a curated set at [mercurialsolo/claudectl-hooks](https://github.com/mercurialsolo/claudectl-hooks). To submit a hook, [open an issue](https://github.com/mercurialsolo/claudectl-hooks/issues) with the config snippet, what it solves, and any dependencies.

## Claude Code Integration

claudectl can install hooks directly into Claude Code's settings so sessions automatically notify claudectl on tool use:

```bash
claudectl --init                    # Write hooks to ~/.claude/settings.json (user scope)
claudectl --init -s project         # Write to .claude/settings.local.json instead
```

This adds `PreToolUse`, `PostToolUse`, and `Stop` hooks that call `claudectl --json` on each event. Existing settings and hooks are preserved.

To remove:

```bash
claudectl --uninstall               # Remove claudectl hooks from user settings
claudectl --uninstall -s project    # Remove from project-local settings
```

### How it works

The hooks are standard Claude Code command hooks:

```json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "Bash",
      "hooks": [{ "type": "command", "command": "claudectl --json 2>/dev/null || true", "timeout": 5 }]
    }],
    "PostToolUse": [{
      "matcher": "*",
      "hooks": [{ "type": "command", "command": "claudectl --json 2>/dev/null || true", "timeout": 5 }]
    }],
    "Stop": [{
      "matcher": "",
      "hooks": [{ "type": "command", "command": "claudectl --json 2>/dev/null || true", "timeout": 5 }]
    }]
  }
}
```

The `2>/dev/null || true` suffix ensures Claude Code is never blocked if claudectl is not installed or fails.

### Scope

The `--scope` / `-s` flag controls where hooks are written, matching Claude Code's own scope convention (`claude mcp add -s project`):

| Scope | Flag | File | Committed to git? |
|-------|------|------|--------------------|
| `user` (default) | `--init` | `~/.claude/settings.json` | No (user home) |
| `project` | `--init -s project` | `.claude/settings.local.json` | No (gitignored) |

Use `user` scope when you want claudectl active everywhere. Use `project` scope when you only want it for specific projects, or when working in a shared repo where not everyone uses claudectl.

## Brain Gate Mode

The brain gate controls whether the plugin hook queries the brain on tool calls.

```bash
claudectl --mode on                     # Default: brain evaluates, denies dangerous ops
claudectl --mode off                    # Disable brain — pass through all tool calls
claudectl --mode auto                   # Auto-approve above confidence threshold
claudectl --mode status                 # Show current mode
```

The mode is stored in `~/.claudectl/brain/gate-mode`. If the file is absent, the default is `on`.

The `/brain` command in the Claude Code plugin does the same thing:

```
/brain off     # Disable brain for exploratory work
/brain on      # Re-enable brain
/brain auto    # Full auto-approve
```

### Auto-Insights

The brain can automatically detect friction patterns and suggest workflow improvements:

```bash
claudectl --brain --insights              # View current insights
claudectl --brain --insights on           # Auto-generate every 10 decisions
claudectl --brain --insights off          # Disable auto-generation (default)
claudectl --brain --insights status       # Show current mode
```

The insights mode is stored in `~/.claudectl/brain/insights-mode`. If the file is absent, the default is `off` (opt-in). When enabled, insights are generated alongside preference distillation and tracked differentially — only new patterns are surfaced.

Detected insight types: friction patterns, error loops, context blowouts, missing rules, accuracy gaps, temporal friction, cost trends.

## Claude Code Plugin

claudectl ships with a Claude Code plugin in `claude-plugin/` at the repository root. The plugin provides:

- **PreToolUse hooks** that query the brain before Bash/Write/Edit calls
- **Slash commands** (`/sessions`, `/spend`, `/brain-stats`, `/brain`, `/auto-insights`)
- **A supervisor agent** for proactive health triage
- **A session monitoring skill** that auto-activates when relevant

The plugin and the `--init` hooks are complementary:

| Method | What it does | Best for |
|--------|-------------|----------|
| `claudectl --init` | Observability hooks (PostToolUse, Stop) | Feeding data to the TUI dashboard |
| Plugin | Brain gate hook (PreToolUse) + commands | Inline approve/deny without the TUI |

You can use both. The `--init` hooks notify claudectl of tool completions. The plugin hook queries the brain before tool execution.

## Coordination Layer (--features coord)

Multi-session coordination on a single machine. Stores events, leases, blockers, handoffs, interrupts, and memory in a local SQLite database at `~/.claudectl/coord.db`. No TOML configuration needed — inspect with `claudectl --coord <subcommand>`. See [Reference](reference.md#coordination---features-coord) for all subcommands.

## Relay & Hive Mind Configuration

Cross-machine collaboration. `relay` feature enables task delegation. `hive` feature (depends on relay) enables knowledge sharing. See [Relay & Hive Mind](relay.md) for the full guide.

```toml
[relay]
enabled = true              # start relay with TUI/brain
listen_port = 9847          # TCP port for peer connections
listen_addr = "0.0.0.0"    # bind address
max_peers = 8               # maximum connected peers
heartbeat_interval_secs = 30
reconnect_max_secs = 60
auto_connect = []           # list of "host:port" to auto-connect on startup

[hive]
enabled = true              # enable knowledge sharing (requires relay)
default_trust = 0.5         # trust level for new peers (0.0-1.0)
auto_trust_drift = true     # adjust trust based on decision concordance
max_propagation = 5         # max gossip hops
export_min_evidence = 5     # min decisions before sharing a pattern
knowledge_ttl_days = 30     # expire unvalidated knowledge
inject_unverified = true    # show low-trust knowledge in brain prompt
max_units = 500             # hard cap on stored knowledge units
max_prompt_units = 20       # cap on units injected into brain prompt
stale_peer_days = 90        # prune knowledge from peers gone this long

# Sharing controls — what knowledge to share with peers
share_categories = []       # empty = all shareable. Options: best_practice, technique, workflow
exclude_tools = []          # never share patterns for these tools (e.g., ["Write"])
exclude_commands = []       # never share patterns matching these substrings
```

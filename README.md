# claudectl

**Mission control for Claude Code.**

A local LLM watches your Claude Code sessions and decides what to approve. You press one key to record a highlight reel GIF. You orchestrate 5 sessions with dependencies. And you never tab-hunt again.

[![CI](https://github.com/mercurialsolo/claudectl/actions/workflows/ci.yml/badge.svg)](https://github.com/mercurialsolo/claudectl/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/claudectl)](https://crates.io/crates/claudectl)
[![Homebrew](https://img.shields.io/badge/homebrew-mercurialsolo%2Ftap-orange)](https://github.com/mercurialsolo/homebrew-tap)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey)]()

<sub>~1 MB binary. Sub-50ms startup. Zero config required.</sub>

[Website](https://mercurialsolo.github.io/claudectl/) | [Demo](https://asciinema.org/a/bovJrUq2vEmC08NU) | [Releases](https://github.com/mercurialsolo/claudectl/releases)

<a href="https://asciinema.org/a/bovJrUq2vEmC08NU?autoplay=1"><img src="https://asciinema.org/a/bovJrUq2vEmC08NU.svg" alt="claudectl demo" width="100%" /></a>

## Install

```bash
brew install mercurialsolo/tap/claudectl     # Homebrew (macOS / Linux)
cargo install claudectl                       # Cargo (any platform)
```

<details>
<summary>Other methods</summary>

```bash
curl -fsSL https://raw.githubusercontent.com/mercurialsolo/claudectl/main/install.sh | sh
nix run github:mercurialsolo/claudectl
git clone https://github.com/mercurialsolo/claudectl.git && cd claudectl && cargo install --path .
```

</details>

## Try it now

```bash
claudectl --demo                          # Fake sessions, no Claude needed
claudectl                                 # Live dashboard
claudectl --brain                         # Local LLM auto-pilot
claudectl --new --cwd ./myproject         # Launch a new session
claudectl --run tasks.json --parallel     # Orchestrate multiple sessions
```

## Why claudectl

| Capability | Claude Code alone | With claudectl |
|-----------|:-:|:-:|
| Local LLM auto-approve/deny | No | **Brain with ollama** |
| Session health monitoring | No | **Cache, cost spikes, loops, stalls, context** |
| Record session highlight reels | No | **Press `R`** |
| Orchestrate multi-session workflows | No | **Dependency-ordered tasks** |
| Launch/resume sessions | Separate terminal | **Press `n` or `--new`** |
| See status of all sessions at once | No | **Yes** |
| Know which session is blocked | Tab-hunt | **At a glance** |
| Track cost per session | Manually | **Live $/hr burn rate** |
| Enforce spend budgets | No | **Auto-kill at limit** |
| File conflict detection | No | **Auto-detect + auto-deny** |
| Auto-rule engine | No | **Match by tool/command/project/cost** |
| Approve prompts without switching | No | **Press `y`** |
| Get notified on stalls/blocks | No | **Desktop + webhook** |

## Local LLM Brain

A local LLM observes your sessions and suggests what to approve, deny, or terminate. It learns from your corrections. Works with any local inference server — no cloud API needed.

**Supported backends:**

| Backend | Setup | Default endpoint |
|---------|-------|-----------------|
| [ollama](https://ollama.com) | `ollama pull gemma4:e4b && ollama serve` | `localhost:11434` |
| [llama.cpp](https://github.com/ggerganov/llama.cpp) | `llama-server -m model.gguf` | `localhost:8080` |
| [vLLM](https://github.com/vllm-project/vllm) | `vllm serve gemma4` | `localhost:8000` |
| [LM Studio](https://lmstudio.ai) | Start server in UI | `localhost:1234` |

Any endpoint that accepts a JSON POST and returns generated text will work.

```bash
# ollama (default — zero config)
claudectl --brain

# llama.cpp
claudectl --brain --url http://localhost:8080/v1/chat/completions

# vLLM
claudectl --brain --url http://localhost:8000/v1/chat/completions --brain-model gemma4

# Advisory mode: brain suggests, you press b to accept or B to reject
claudectl --brain

# Auto mode: brain executes without asking
claudectl --brain --auto-run
```

Every decision is logged locally. Past decisions are retrieved as few-shot examples so the brain adapts to your preferences over time. Deny rules always override brain suggestions. All data stays on your machine.

Run `claudectl --doctor` to check if your backend is reachable. Run `claudectl --brain-eval` to test decision quality against built-in scenarios. Run `claudectl --brain-prompts` to see which prompt templates are active and whether they're built-in or user overrides.

```toml
# .claudectl.toml
[brain]
enabled = true
endpoint = "http://localhost:11434/api/generate"  # change for other backends
model = "gemma4:e4b"
auto = false
few_shot_count = 5
```

Override prompt templates by placing files in `~/.claudectl/brain/prompts/`.

## Record and Share

**Highlight reels** — Press `R` on any session. claudectl extracts file edits, bash commands, errors, and successes. Idle time and noise are stripped. Output is a shareable GIF.

**Dashboard recording** — Capture the full TUI as a GIF or asciicast:

```bash
claudectl --record session.gif             # GIF (requires agg)
claudectl --demo --record demo.gif         # One-command demo GIF for your README
```

## Orchestrate Sessions

Run coordinated tasks with dependency ordering, retries, cross-session data routing, and resumable sessions:

```json
{
  "tasks": [
    { "name": "auth", "cwd": "./backend", "prompt": "Add JWT auth middleware" },
    { "name": "tests", "cwd": "./backend", "prompt": "Update API tests for auth. Previous output: {{auth.stdout}}", "depends_on": ["auth"] },
    { "name": "docs", "cwd": "./docs", "prompt": "Document the new auth flow", "depends_on": ["auth"] }
  ]
}
```

```bash
claudectl --run tasks.json --parallel
```

## Session Health Monitoring

claudectl continuously checks each session for problems and surfaces them with severity-ranked icons in the dashboard:

- **Cache health** — detects low cache hit ratios that can silently multiply costs
- **Cost spikes** — flags when burn rate exceeds the session average
- **Loop detection** — catches tools failing repeatedly in retry loops
- **Stall detection** — sessions spending money but producing no file edits
- **Context saturation** — warns when a session approaches its context window limit

Health issues appear as icons in the session table and as a summary in the status bar. No configuration needed.

## File Conflict Detection

When multiple sessions edit the same file, claudectl detects the conflict and flags it:

- **`!F` prefix** in the session table for sessions with file-level conflicts
- **File Conflicts section** in the detail panel showing which files conflict and with which sessions
- **Predictive detection** — flags pending Edit/Write calls targeting files another session has already modified
- **Auto-deny** — optionally deny writes to conflicting files with an actionable message

```toml
# .claudectl.toml
[orchestrate]
file_conflicts = true              # Detect file-level conflicts (default: on)
auto_deny_file_conflicts = true    # Auto-deny conflicting writes (default: off)
```

File conflicts can also be matched in auto-rules:

```toml
[rules.deny_conflicts]
match_file_conflict = true
action = "deny"
message = "Another session is editing this file"
```

## Launch and Resume Sessions

Start new Claude Code sessions without leaving the dashboard:

```bash
claudectl --new --cwd ./backend                       # Launch in a directory
claudectl --new --cwd ./api --prompt "Add rate limiting"  # Launch with a prompt
claudectl --new --resume abc123                       # Resume a previous session
```

From the dashboard, press `n` to open the launch wizard (directory, prompt, resume fields).

## Auto-Rules

Define rules in `.claudectl.toml` to automatically approve, deny, terminate, or route sessions based on conditions:

```toml
[[rules]]
name = "approve-cargo"
match_tool = ["Bash"]
match_command = ["cargo"]
action = "approve"

[[rules]]
name = "deny-rm-rf"
match_command = ["rm -rf"]
action = "deny"

[[rules]]
name = "kill-runaway"
match_cost_above = 20.0
action = "terminate"
```

Rules support matching by status, tool name, command substring, project name, cost threshold, and error state. Deny rules always take precedence. Rules can also route output between sessions, spawn new sessions, or delegate to agents.

## Supervise and Control Spend

```bash
claudectl --budget 5 --kill-on-budget      # Auto-kill at $5
claudectl --notify                         # Desktop notifications on blocks/stalls
claudectl --webhook https://hooks.slack.com/... --webhook-on NeedsInput,Finished
claudectl --history --since 24h            # Review past session costs
claudectl --stats --since 24h             # Aggregated session statistics
claudectl --summary --since 8h            # Activity summary
```

From the dashboard: `y` approve, `i` input, `Tab` switch terminal, `d` kill, `n` new session, `R` record, `?` all keys.

## Filter and Search

```bash
claudectl --filter-status NeedsInput       # Only show sessions needing input
claudectl --focus attention                # High-signal triage view
claudectl --focus over-budget              # Sessions exceeding budget
claudectl --search "my-project"            # Filter by project name
claudectl --watch                          # Stream status changes (no TUI)
claudectl --watch --json                   # Stream as JSON
```

In the dashboard: `f` cycle status filters, `v` cycle focus filters, `/` search, `z` clear all filters, `g` group by project, `s` cycle sort order.

## Clean Up

```bash
claudectl --clean                          # Remove old session data
claudectl --clean --older-than 7d          # Only sessions older than 7 days
claudectl --clean --finished --dry-run     # Preview what would be removed
```

## Docs

| | |
|---|---|
| [Reference](docs/reference.md) | Dashboard features, keybindings, CLI modes, status detection |
| [Configuration](docs/configuration.md) | Config files, hooks, rules, model pricing overrides |
| [Terminal Support](docs/terminal-support.md) | Compatibility matrix and setup notes |
| [Troubleshooting](docs/troubleshooting.md) | Common issues and FAQ |
| [Contributing](docs/contributing.md) | Setup, guidelines, and architecture |
| [Changelog](CHANGELOG.md) | Release history |

## Community

Questions or ideas? [Start a Discussion](https://github.com/mercurialsolo/claudectl/discussions). Found a bug? [Open an issue](https://github.com/mercurialsolo/claudectl/issues/new).

## License

MIT

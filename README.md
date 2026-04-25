<p align="center">
  <img src="assets/logo.png" alt="claudectl" width="372">
</p>
<p align="center"><strong>Mission control for Claude Code.</strong></p>
<p align="center">Supervise, orchestrate, and connect coding agents with a local LLM brain and hive mind.</p>

[![CI](https://github.com/mercurialsolo/claudectl/actions/workflows/ci.yml/badge.svg)](https://github.com/mercurialsolo/claudectl/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/claudectl)](https://crates.io/crates/claudectl)
[![Homebrew](https://img.shields.io/badge/homebrew-mercurialsolo%2Ftap-orange)](https://github.com/mercurialsolo/homebrew-tap)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Downloads](https://img.shields.io/crates/d/claudectl)](https://crates.io/crates/claudectl)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey)]()

<sub>~1 MB binary. Sub-50ms startup. Zero config required.</sub>

[Website](https://mercurialsolo.github.io/claudectl/) | [Demo](https://asciinema.org/a/AJP33vbmHGFVW6zL) | [Blog: Why a local brain?](blog/local-brain-architecture.md) | [Releases](https://github.com/mercurialsolo/claudectl/releases)

<a href="https://asciinema.org/a/AJP33vbmHGFVW6zL?autoplay=1"><img src="https://asciinema.org/a/AJP33vbmHGFVW6zL.svg" alt="claudectl demo" width="100%" /></a>

## What it does for you

Run `claudectl --brain-stats impact` to see your numbers:

```
  ╔════════════════════════════════════════════════╗
  ║              IMPACT SCORECARD                  ║
  ║             1200 decisions tracked             ║
  ╠════════════════════════════════════════════════╣
  ║  Auto-handled                             71%  ║
  ║  ████████████████████░░░░░░░░  847/1200        ║
  ║                                                ║
  ║  Brain accuracy                          96.2%  ║
  ║  ███████████████████████████░  1154/1200       ║
  ║                                                ║
  ║  Coverage vs static rules               2.9x  ║
  ║  brain ████████████████████████████  100%      ║
  ║  rules █████████░░░░░░░░░░░░░░░░░░░  34%      ║
  ║                                                ║
  ║  Dangerous ops blocked      12  Time saved 42m  ║
  ║  2 critical | 10 high-risk | 847 auto x 3s    ║
  ║                                                ║
  ║  Learning: correction rate 8.4% ↓ 2.1% (-6pp) ║
  ╚════════════════════════════════════════════════╝
```

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

## Get started

```bash
claudectl                     # Live dashboard — see all sessions at a glance
claudectl --init              # Wire up Claude Code hooks (one-time)
claudectl --brain             # Enable local LLM auto-pilot
```

## Why claudectl

- **Local LLM auto-pilot** — a brain that learns your preferences and auto-approves/denies tool calls. No cloud API.
- **Hive mind** — knowledge distillation, archiving, and curriculum generation. Connect instances to share learnings across machines.
- **Self-improving** — detects friction patterns, suggests rules, and gets smarter with every correction.
- **Multi-session orchestration** — run parallel tasks with dependency ordering and cross-session context routing.
- **Health monitoring** — catches cognitive decay, cost spikes, error loops, and context saturation before they waste money.
- **Works everywhere** — Claude Code plugin for inline use, TUI dashboard for oversight, CLI for automation.

[Full feature comparison](docs/reference.md#comparison)

## Local LLM Brain

The brain observes all your sessions and makes real-time decisions:

- **Approve** safe tool calls automatically (reads, greps, test runs)
- **Deny** dangerous operations before they execute (force pushes, destructive commands)
- **Terminate** sessions that are looping, stalled, or burning money
- **Route** summarized output between sessions so they share context
- **Spawn** new sessions when the brain detects parallelizable work

```bash
ollama pull gemma4:e4b && ollama serve    # One-time setup
claudectl --brain                         # Advisory mode (default)
claudectl --brain --auto-run              # Auto mode: brain executes without asking
claudectl --mode auto                     # Or toggle mid-session (Ctrl+b in TUI)
```

Works with any OpenAI-compatible endpoint: [ollama](https://ollama.com), [llama.cpp](https://github.com/ggerganov/llama.cpp), [vLLM](https://github.com/vllm-project/vllm), [LM Studio](https://lmstudio.ai).

### How the brain learns

The brain learns from **everything** you do — not just brain-involved decisions, but every manual approve, reject, rule execution, and conflict resolution. All data stays on your machine.

| Level | What it learns | Example |
|-------|---------------|---------|
| **Conditional preferences** | Context-dependent rules via decision tree splits | `approve [Bash] "git push" when cost<$5 (n=8)` |
| **Outcome tracking** | Correlates decisions to detect "approved but broke" | Downweights false-positive approvals |
| **Temporal patterns** | Behavioral sequences and time-of-day behavior | `After 3+ errors: user usually denies` |
| **Per-project models** | Separate preferences per project | `[Read] always approve in frontend, usually deny in infra` |
| **Adaptive thresholds** | Per-tool confidence requirements based on accuracy | 90%+ accurate on Read = auto-execute at 0.5 confidence |

### Self-improving sessions

The brain automatically detects friction patterns and suggests workflow improvements:

```bash
claudectl --brain --insights on     # Enable auto-generation (every 10 decisions)
claudectl --brain --insights        # View current insights
```

Detects: friction patterns, error loops, context blowouts, missing rules, accuracy gaps, cost trends. Only new insights are surfaced — the system tracks what you've already seen. Use `/auto-insights` in the Claude Code plugin.

## Claude Code Plugin

Integrates the brain directly into Claude Code sessions — no TUI required.

| Component | What it does |
|-----------|-------------|
| **Brain gate hook** | Queries the brain before every Bash/Write/Edit call |
| `/brain on\|off\|auto` | Toggle brain mode mid-session (or `Ctrl+b` in TUI) |
| `/sessions` | Show all active sessions with status, cost, health |
| `/spend` | Cost breakdown by project and time window |
| `/brain-stats` | Brain learning metrics and accuracy |
| `/auto-insights` | Auto-generated workflow insights |

## Headless Mode

Run the full autonomous stack without a TUI. Attach a dashboard from another terminal.

```bash
claudectl --headless --brain --auto-run           # Human-readable events
claudectl --headless --brain --auto-run --json    # Structured JSON events
```

What runs in headless mode:
- Brain inference (approve/deny/route/spawn with adaptive confidence)
- Coordination layer (leases, interrupts, handoffs, memory)
- Context rot prevention (auto-raises compact/stop interrupts when decay detected)
- Rule evaluation and health monitoring

The TUI dashboard can run alongside -- both share state via the coordination SQLite store, brain decision logs, and session discovery.

```bash
# Background daemon
nohup claudectl --headless --brain --auto-run > ~/.claudectl/autopilot.jsonl 2>&1 &

# Attach dashboard in another terminal
claudectl
```

## Coordination Layer

Multi-agent coordination for parallel coding sessions. Prevents duplicate work, manages ownership, and routes context between agents.

Build with `cargo build --features coord` to enable.

```bash
# Ownership leases — prevent two agents from editing the same file
claudectl --coord "claim --session sess_1 --path src/app.rs --mode exclusive"
claudectl --coord "release lease_123"

# Handoffs — structured context transfer between sessions
claudectl --coord "handoff --from sess_1 --to sess_2 --task task_1 --summary 'Fix path normalization'"

# Interrupts — typed cross-agent signals with delivery modes
claudectl --coord "raise --type pause --target sess_1 --reason 'lease conflict'"
claudectl --coord "ack intr_123"

# Memory — validated patterns promoted from brain decisions
claudectl --coord "promote --project myproject"
claudectl --coord "context --session sess_1"      # Preview injected context

# Inspection
claudectl --coord leases                           # Active ownership leases
claudectl --coord interrupts                       # Pending interrupts
claudectl --coord events                           # Event audit log
claudectl --coord metrics                          # Coordination health metrics
claudectl --coord eval                             # Run 10 eval scenarios
claudectl --coord adapters                         # Registered agent adapters
```

The coordination layer stores state in a local SQLite database (`~/.claudectl/coord/coord.db`) and injects compact context into the brain's prompt before every decision.

## Hive Mind & Relay

The brain distills your decisions into shareable knowledge. Connect instances across machines to build a convergent hive mind.

```bash
# Hive knowledge is built-in — view what the brain has learned
claudectl --hive status
claudectl --hive knowledge
claudectl --hive distill              # Condense archive into curriculum

# Add relay for cross-machine networking
cargo install claudectl --features relay
claudectl --relay invite              # Generate an invite code
claudectl --relay "join YEK-AGA-YHK-QAA-BM"   # Join from another machine
claudectl --relay discover            # Scan LAN for nearby instances
```

Knowledge categories (best practices, techniques, workflow patterns) propagate automatically. Personal patterns (time-of-day habits, cost tolerance) stay local. You control what's shared:

```toml
[hive]
share_categories = ["best_practice", "technique"]
exclude_tools = ["Write"]
max_units = 500
max_prompt_units = 20
```

See the [full Relay & Hive Mind guide](docs/relay.md).

## Orchestrate Sessions

Run coordinated tasks with dependency ordering, retries, and cross-session data routing:

```json
{
  "tasks": [
    { "name": "auth", "cwd": "./backend", "prompt": "Add JWT auth middleware" },
    { "name": "tests", "cwd": "./backend", "prompt": "Update API tests. Previous: {{auth.stdout}}", "depends_on": ["auth"] },
    { "name": "docs", "cwd": "./docs", "prompt": "Document the new auth flow", "depends_on": ["auth"] }
  ]
}
```

```bash
claudectl --run tasks.json --parallel
claudectl --decompose "Add auth, write tests, update docs"   # Auto-split into parallel tasks
```

## Session Health Monitoring

Continuously checks each session and surfaces problems in the dashboard:

- **Cognitive decay** — composite 0-100 score tracking degradation over time
- **Proactive compaction** — suggests `/compact` at 50% context, before the 80/90% thresholds
- **Cost spikes** — flags when burn rate exceeds the session average
- **Loop detection** — catches tools failing repeatedly in retry loops
- **Stall detection** — sessions spending money but producing no edits
- **File conflicts** — detects when multiple sessions edit the same file

## Spend Control

```bash
claudectl --budget 5 --kill-on-budget     # Auto-kill at $5
claudectl --notify                        # Desktop notifications on blocks
claudectl --stats --since 24h            # Aggregated cost statistics
```

## Auto-Rules

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

Rules support matching by tool, command, project, cost, and error state. Deny rules always take precedence.

<details>
<summary>More features</summary>

### Idle Mode

When you step away, claudectl can run pre-configured low-risk tasks. A morning report summarizes what happened.

### Session Lifecycle

Auto-restart sessions on context saturation with checkpoint + summary handoff.

### Record and Share

Press `R` on any session for a highlight reel GIF (edits, commands, errors — idle time stripped). Or `claudectl --record demo.gif` for the full dashboard.

### Launch and Resume

`claudectl --new --cwd ./backend --prompt "Add auth"` or press `n` in the dashboard.

### Filter and Search

`--filter-status NeedsInput`, `--focus attention`, `--search "project"`, `--watch` for streaming.

</details>

## Docs

| | |
|---|---|
| [Quick Start](docs/quickstart.md) | Install, init, first dashboard |
| [Reference](docs/reference.md) | All flags, keybindings, modes |
| [Configuration](docs/configuration.md) | Config files, hooks, rules |
| [Relay & Hive Mind](docs/relay.md) | Connect instances, share knowledge |
| [Terminal Support](docs/terminal-support.md) | Compatibility matrix |
| [Troubleshooting](docs/troubleshooting.md) | Common issues and FAQ |
| [Contributing](docs/contributing.md) | Setup and guidelines |
| [Changelog](CHANGELOG.md) | Release history |

## Community

- Questions or ideas? [Start a Discussion](https://github.com/mercurialsolo/claudectl/discussions)
- Found a bug? [Open an issue](https://github.com/mercurialsolo/claudectl/issues/new)
- Share your setup in [Show & Tell](https://github.com/mercurialsolo/claudectl/discussions/categories/show-and-tell)

## License

MIT

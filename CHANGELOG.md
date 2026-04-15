# Changelog

All notable changes to claudectl are documented here.

## [0.25.0] - 2026-04-15

### Added
- File-level conflict detection: detects when multiple sessions edit the same file, with `!F` indicator in dashboard and per-file detail in the expanded panel
- Predictive conflict detection: flags pending Edit/Write tool calls that target files already modified by another session
- Auto-deny for file conflicts: `[orchestrate] auto_deny_file_conflicts = true` automatically denies writes to files being edited by another session, with actionable error message naming the conflicting session
- `match_file_conflict` rule condition: match sessions with pending file conflicts in the auto-rule engine
- `pending_file_path` tracking: Edit/Write/NotebookEdit tool calls now track the target file path for conflict detection
- `[orchestrate]` config section with `file_conflicts` (default true) and `auto_deny_file_conflicts` (default false)
- Configurable health check thresholds via `[health]` TOML section — all 5 checks (cache, cost spike, loop, stall, context) accept user-defined thresholds
- Capture actual error messages from tool results — detail panel shows "Recent Errors" section with tool name and message text
- `--config-template` flag prints a fully annotated `.claudectl.toml` with all available settings
- CLI flags grouped by purpose in `--help`: Dashboard, Output Modes, Filtering, Session Management, Budget & Notifications, Brain, Orchestration, Recording, Cleanup, History & Diagnostics

### Fixed
- Brain connection failure now shows in the TUI status bar instead of being lost to stderr

### Changed
- Orchestrator shows task plan at launch, uses `[n/total]` progress fractions and terminal-width-aware status line

## [0.24.0] - 2026-04-15

### Added
- Session health monitoring with visual icons: 🔥 low cache, 💸 cost spike, 🔄 looping, 🐌 stalled, 🧠 context full — proactively detects cache TTL bugs, cost anomalies, retry loops, and context saturation
- Health icons appear next to project name in the dashboard table, sorted by severity

## [0.23.2] - 2026-04-15

### Fixed
- Ghost sessions from PID reuse: when a Claude Code process exits and macOS reassigns the PID to another process, claudectl now correctly detects the mismatch and marks the session as Finished instead of showing stale status

## [0.23.1] - 2026-04-15

### Fixed
- Brain client now auto-detects OpenAI-compatible endpoints (/v1/chat/completions) vs ollama (/api/generate) from the URL — llama.cpp, vLLM, and LM Studio now work correctly without extra config

## [0.23.0] - 2026-04-15

### Added
- External agent integration: register agents (Codex, Aider, custom) via `[agents.*]` config, brain can delegate work to them with output capture to `.claudectl-runs/agents/`
- `RuleAction::Delegate` — brain can delegate work to named agents
- Agent output logged to `.claudectl-runs/agents/{name}.{timestamp}.log`

## [0.22.0] - 2026-04-15

### Added
- Externalized prompt library: all brain prompts loaded from `~/.claudectl/brain/prompts/` with built-in fallbacks, users can override any prompt template
- Local eval framework: `--brain-eval` runs 6 built-in scenarios (approve/deny/send) against the local LLM and reports accuracy
- Custom eval scenarios via JSON files in `~/.claudectl/brain/evals/`
- `--brain-prompts` CLI command lists all prompt templates and their source (built-in vs user override)

## [0.21.0] - 2026-04-15

### Added
- Dynamic auto-orchestration: the brain periodically evaluates all sessions and decides cross-session actions (spawn, route, terminate) without a pre-written tasks.json
- Configurable orchestration interval (default 30s) and max_sessions limit
- `--auto-run` flag (renamed from `--brain-auto`) for cleaner CLI
- Documentation for all supported LLM backends (ollama, llama.cpp, vLLM, LM Studio)

## [0.20.0] - 2026-04-15

### Added
- Spawn action: the brain can launch new Claude Code sessions with derived prompts, with configurable `max_sessions` limit (default 10)
- Persistent mailbox system: messages between sessions are queued in `~/.claudectl/brain/mailbox/` and delivered when the target session is ready (WaitingInput), preventing interruption during active work
- Smart routing: Route action queues to mailbox when target is busy, delivers directly when target is waiting

## [0.19.0] - 2026-04-15

### Added
- Cross-session awareness: the brain now sees all active sessions (project, status, pending tool, cost, context%) when evaluating any single session, enabling cross-session reasoning
- Inter-session routing: new `Route` action lets the brain send summarized output from one session to another via the local LLM, preventing context bloat in the target session
- Auto-summarization: `summarize_for_routing()` asks the local LLM to compress source output for the target's specific task context before sending

## [0.18.2] - 2026-04-15

### Added
- Brain diagnostics in `--doctor`: checks curl, ollama binary, config status, and endpoint reachability
- Startup connectivity check: when `--brain` is enabled, verifies the LLM endpoint is reachable before creating the engine; prints clear fix instructions if not
- README documentation for the brain feature: setup, activation, config, keybindings, decision learning

## [0.18.1] - 2026-04-15

### Added
- Few-shot decision learning: the brain now retrieves relevant past decisions from the local log and includes them as examples in the LLM prompt, so it learns from user corrections over time
- Configurable `few_shot_count` (default 5) in `[brain]` config section
- Relevance scoring: past decisions matching the same tool name rank highest, then same project, then most recent

## [0.18.0] - 2026-04-15

### Added
- **Local LLM brain** (opt-in): connect to ollama or any OpenAI-compatible local LLM for session advisory. Enable with `--brain` or `[brain]` config section.
- Brain context builder: compacts session transcripts into LLM prompts with configurable token budget
- Brain LLM client: communicates via curl subprocess (no new dependencies, follows webhook pattern)
- Brain inference loop: non-blocking async inference with 10-second per-PID cooldown
- Advisory UI: pending brain suggestions shown inline (`[b:approve]`), accept with `b`, reject with `B`
- Auto mode: `--brain-auto` executes suggestions without confirmation
- Decision logging: every brain suggestion + user response logged to `~/.claudectl/brain/decisions.jsonl`
- Deny rules always override brain suggestions regardless of confidence

## [0.17.1] - 2026-04-15

### Added
- Cross-session data routing: task prompts can reference `{{name.stdout}}` to inject the stdout of a completed dependency, enabling data pipelines between orchestrated sessions
- Template validation at load time catches missing tasks, missing dependencies, and unsupported fields before any task starts
- Output truncation at 32KB with `... (truncated)` marker to prevent context overflow

## [0.17.0] - 2026-04-15

### Added
- Rule-based auto-actions: configure `[rules.*]` sections in `.claudectl.toml` to automatically approve, deny, send messages, or terminate sessions based on tool name, command pattern, project, cost threshold, and error state
- Pending tool tracking: sessions now expose the tool name and command awaiting approval for rule matching and display
- Deny-first precedence: deny rules always override approve rules regardless of config order

## [0.16.2] - 2026-04-14

### Fixed
- Sessions blocked on a permission prompt now correctly show Needs Input when Claude Code writes `stop_reason: null` with a tool_use content block; the monitor infers `tool_use` from the message content instead of requiring the explicit stop_reason field

## [0.16.1] - 2026-04-14

### Fixed
- Sessions blocked on a permission prompt ("Do you want to proceed?") no longer misclassify as Idle after the first refresh tick; status inference now persists JSONL signals across ticks so tool_use-based NeedsInput survives when no new transcript data arrives

## [0.16.0] - 2026-04-14

### Added
- GNOME Terminal support on Linux for `--new` and the `n` launch wizard, with doctor output that makes the current control limitations explicit
- GNOME Terminal launch support for Ubuntu's default terminal, verified under Docker/X11
- Homebrew release automation for both macOS and Linux artifacts, updating `mercurialsolo/homebrew-tap` on tagged releases

### Fixed
- Parent sessions now keep subagent token and cost rollups even when transient task files disappear from `/tmp`
- Session detail and JSON output now distinguish active subagents from total rolled-up subagent usage
- The main dashboard now expands parent sessions into child subagent rows, with a completed-subagent aggregate plus live active subagents underneath
- Release automation now publishes to crates.io instead of stopping at GitHub release assets

## [0.15.5] - 2026-04-14

### Fixed
- Unified Claude transcript parsing across monitoring and highlight reels, so status/cost/context now come from one parser instead of separate ad-hoc readers
- Sessions with missing or unsupported transcript telemetry now show an explicit `Unknown` state with `n/a` metrics instead of looking like idle zero-cost sessions
- `--run` now tracks real child exit status, drains stdout/stderr, writes per-task logs under `.claudectl-runs/`, and fails tasks on non-zero exit instead of treating any vanished PID as success
- `n` and `--new` now launch visible Claude sessions only in supported terminals (`tmux`, Kitty, WezTerm) and fail clearly elsewhere instead of spawning detached background processes
- Cost estimation now uses a model registry with config overrides; unknown models are marked as fallback estimates instead of silently pretending pricing is verified
- `install.sh` now downloads the tagged release assets that the GitHub release workflow actually publishes

### Added
- `[models."..."]` config sections for overriding pricing and context limits per model
- Telemetry metadata in JSON and webhook outputs, including whether estimates are verified or fallback
- Shared transcript fixtures and parser tests for both current and legacy Claude JSONL shapes

## [0.13.1] - 2026-04-13

### Changed
- README updated with all v0.13.0 features in feature list, usage section, and architecture table

## [0.13.0] - 2026-04-13

### Added
- **Session highlight reel** — press `R` on any session to start recording a supercut of its activity. Parses the session's JSONL in real-time, extracts the interesting bits (file edits, bash commands, status transitions), compresses idle time, and outputs as `.gif` or `.cast`. Press `R` again to stop (#66)
- **Multiple simultaneous recordings** — press `R` on different sessions to record them all at once. Each gets its own highlight reel
- **Per-session REC indicator** — table shows `REC` prefix on recorded sessions, status bar shows count
- **Supercut format** — title card, running stats header (edits/commands/errors), paced playback, final summary card with claudectl branding
- Only highlight events make the cut: Edit, Write, Bash, Agent. Read/Grep/Glob filtered out
- Errors marked ✗ red, successes ✓ green, verbose text trimmed
- Works passively in background while TUI stays interactive
- Split terminal support — records via JSONL on disk, not terminal output

## [0.11.2] - 2026-04-13

### Added
- **Direct GIF recording** (`--record session.gif`) — specify `.gif` extension and claudectl automatically records asciicast then converts via `agg`. No manual pipeline needed (#65)
- Falls back gracefully: if `agg` not installed, saves `.cast` with install instructions
- `.cast` extension still supported for raw asciicast v2 output

## [0.11.1] - 2026-04-13

### Added
- **Live session recording** (`--record session.cast`) — captures pixel-perfect ANSI terminal output via a tee writer. Records exact colors, sparklines, and TUI layout as asciicast v2 format
- **Demo mode** (`--demo`) — deterministic fake sessions for when no real sessions are running. 8 sessions with realistic names, statuses, costs, context levels, conflicts, sparklines, tool usage, and file changes. Works with all output modes: `--demo --list`, `--demo --json`

### Fixed
- **Worktree-aware conflict detection** — sessions in different git worktrees of the same repo no longer false-positive as conflicts. Uses `git rev-parse --show-toplevel` to resolve each session's worktree identity, cached per unique cwd

## [0.10.0] - 2026-04-13

### Added
- **Remote compaction trigger** — press `c` to send `/compact` to a running Claude Code session. Only works when session is idle/waiting. Prevents context window from filling up before auto-compaction kicks in (#64)
- **Rate limit exhaustion ETA** — title bar shows `$spent/$budget (ETA: Xh Ym)` based on aggregate burn rate. Color-coded: green (>2h), yellow (<2h), red (<30m) (#57)
- **Conflict detection** — warns when 2+ sessions share the same working directory with `!!` prefix on project name. Desktop notification and `on_conflict_detected` hook (#58)
- **Context threshold hooks** — new `on_context_high` event fires when context window % crosses configurable threshold (default 75%). Resets after `/compact`. New `{context_pct}` template variable (#59)
- **Per-tool token attribution** — detail panel shows tool call counts sorted by frequency (Bash, Read, Edit, etc.). Exposed in `--json` export (#60)
- **Session cleanup command** — `claudectl --clean` with `--older-than`, `--finished`, `--dry-run` flags. Removes dead session JSON + JSONL transcripts, reports freed disk space (#61)
- **File change tracking** — detail panel shows which files each session modified (extracted from Edit/Write tool_use events in JSONL). Exposed in `--json` export (#62)
- **Permission wait time** — status column shows `Needs Input (2m 34s)` with escalating colors (yellow >1m, red >5m). NeedsInput sessions sorted by longest-waiting first (#63)
- `[context] warn_threshold` config option for context alert threshold

## [0.9.1] - 2025-04-12

### Added
- Daily and weekly aggregate cost budget alerts
- `[budget] daily_limit` and `[budget] weekly_limit` config options
- Aggregate budget hooks fire `on_budget_warning` and `on_budget_exceeded` with synthetic sessions

## [0.9.0] - 2025-04-11

### Added
- **Event hooks system** — run shell commands on session events
- 7 hook events: `on_session_start`, `on_status_change`, `on_needs_input`, `on_finished`, `on_budget_warning`, `on_budget_exceeded`, `on_idle`
- Template variables: `{pid}`, `{project}`, `{status}`, `{cost}`, `{model}`, `{cwd}`, `{tokens_in}`, `{tokens_out}`, `{elapsed}`, `{session_id}`, `{old_status}`, `{new_status}`
- Hooks configured in `[hooks.on_*]` sections of config.toml
- `claudectl --hooks` to list configured hooks
- Verified hooks repository at mercurialsolo/claudectl-hooks

## [0.8.3] - 2025-04-10

### Added
- Weekly and daily cost/token summary in TUI title bar

## [0.8.0] - 2025-04-09

### Added
- **Multi-session orchestration** — `claudectl --run tasks.json` with dependency ordering and `--parallel` flag
- **Session history** — persist completed sessions with `--history` and `--stats` commands
- **Configuration files** — `~/.config/claudectl/config.toml` (global) and `.claudectl.toml` (per-project) with layered overrides
- **Theme system** — dark, light, and monochrome themes with `NO_COLOR` support
- **Diagnostic logging** — `--log` flag for structured debug output
- **Install script and Nix flake** for easier distribution
- First-run experience with empty state hints

### Fixed
- Approve/input for Warp terminal using AppleScript with focus management

## [0.7.0] - 2025-04-07

### Added
- **Watch mode** — `claudectl --watch` streams status changes without TUI
- **Debug mode** — timing instrumentation in the footer
- **Activity sparklines** — 30-second history ring buffer per session
- **Grouped view** — press `g` to group sessions by project with aggregate stats
- **Detail panel** — press `Enter` for expanded session info (tokens, cost, model, paths)
- **Session summary** — `claudectl --summary` for what happened while you were away
- **Webhooks** — POST JSON to Slack/Discord/URL on status changes with event filtering
- **Session launcher** — press `n` or `claudectl --new` to start sessions from the TUI
- **Budget enforcement** — `--budget` with 80% warning and optional `--kill-on-budget`
- Custom output format for watch mode
- Linux support (monitoring without terminal switching)
- Stale session cleanup for dead PIDs >24h old

## [0.6.0] - 2025-04-05

### Added
- Context window % column with visual bar
- Burn rate ($/hr) column with cost decay
- Desktop notifications when sessions enter NeedsInput (`--notify`)
- Help overlay (press `?`)
- Sort and filter by status, context, cost, $/hr, elapsed (press `s`)
- JSON export (`--json`) for scripting
- Subagent tracking with +N indicator
- Auto-approve mode (press `a` twice)

### Changed
- Renamed Tokens column to In/Out for clarity

### Fixed
- 5 critical issues: performance, burn rate calc, CPU smoothing, dropped sysinfo dependency, timestamp handling

## [0.5.0] - 2025-04-03

### Added
- Quick approve — press `y` to send Enter to NeedsInput sessions
- Input mode — press `i` to type arbitrary text to sessions
- Kill sessions — press `d`/`x` (double-tap to confirm)
- NeedsInput status detection for permission prompts
- Terminal switching — press `Tab` to jump to a session's terminal

### Fixed
- JSONL session ID mapping (use sessionId before falling back to latest)
- Input sending via terminal emulator instead of raw TTY device
- Status inference: CPU priority over JSONL flags

## [0.4.0] - 2025-04-02

### Added
- Terminal support for **Ghostty**, **Kitty**, **WezTerm**, **tmux**, **Warp**, **iTerm2**, and **Terminal.app**
- Process table enrichment (CPU, MEM, TTY, elapsed) via `ps`
- Session file scanner for `~/.claude/sessions/*.json`
- JSONL tail reader for incremental token accumulation
- Status inference engine (Processing / NeedsInput / WaitingInput / Idle / Finished)
- Cost estimation with model-aware pricing (Opus, Sonnet, Haiku)
- Diff-based UI updates (only re-render changed rows)
- Configurable poll interval

## [0.1.0] - 2025-04-01

### Added
- Initial release
- Basic TUI table showing running Claude Code sessions
- Process discovery via `~/.claude/sessions/` directory
- ratatui-based terminal UI

---

## Feature Overview

### Dashboard & Monitoring
- Live TUI dashboard with PID, project, status, context %, cost, $/hr, elapsed, CPU%, MEM, tokens, sparklines
- Smart status detection: Processing, Needs Input (with wait time), Waiting, Idle, Finished
- Context window % with configurable threshold alerts
- Cost tracking with per-session and aggregate USD estimates
- Burn rate ($/hr) with budget exhaustion ETA projection
- Activity sparklines (30-second history per session)
- Weekly/daily cost summary in title bar

### Session Actions
- `y` — Approve permission prompts (send Enter)
- `i` — Send custom text input to sessions
- `c` — Trigger `/compact` on idle sessions
- `a` — Toggle auto-approve (double-tap)
- `d`/`x` — Kill sessions (double-tap to confirm)
- `n` — Launch new Claude Code sessions
- `Tab` — Switch to session's terminal

### Observability
- Per-tool token attribution (Bash, Read, Edit call counts)
- File change tracking (which files each session modified)
- Conflict detection (2+ sessions sharing same directory)
- Permission wait time tracking with color escalation
- Detail panel with full session breakdown

### Budget & Limits
- Per-session budget with 80% warning and 100% auto-kill
- Daily and weekly aggregate spend limits
- Rate limit exhaustion ETA projection
- Context threshold alerts with `on_context_high` hook

### Event Hooks
- 9 hook events: `on_session_start`, `on_status_change`, `on_needs_input`, `on_finished`, `on_budget_warning`, `on_budget_exceeded`, `on_idle`, `on_context_high`, `on_conflict_detected`
- Template variables for shell command interpolation
- Webhook integration (POST JSON to Slack/Discord/URLs)
- Desktop notifications

### Output Modes
- Interactive TUI (default)
- `--list` — print formatted table and exit
- `--json` — export session data for scripting
- `--watch` — stream status changes without TUI
- `--summary` — session activity summary
- `--history` / `--stats` — historical analytics
- `--clean` — remove old session data

### Configuration
- Global config: `~/.config/claudectl/config.toml`
- Per-project config: `.claudectl.toml`
- CLI flags override config values
- Theme system: dark, light, monochrome, NO_COLOR

### Terminal Support
- Ghostty (native AppleScript)
- Kitty (remote control API)
- tmux (send-keys)
- WezTerm (CLI JSON API)
- Warp (System Events)
- iTerm2 (AppleScript)
- Terminal.app (AppleScript)

### Task Orchestration
- `--run tasks.json` with dependency ordering
- `--parallel` for independent tasks
- Per-task budget and cwd settings

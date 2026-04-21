# Changelog

All notable changes to claudectl are documented here.

## [0.33.0] - 2026-04-21

### Added
- **Coordination layer** -- local-first coordination plane for multi-agent coding workflows (`--features coord`) (#180, #181, #182, #183)
  - **Phase 0: Event Log** -- SQLite-backed event store with typed records for leases, blockers, interrupts, handoffs, and memory. FTS5 full-text search. 19 CLI subcommands under `--coord`
  - **Phase 1: Ownership and Handoffs** -- `claim`/`release` with exclusive conflict detection, structured handoff packets with goal/artifacts/next_steps, TUI badges (`L`/`H`/`I`) and detail panel coordination section
  - **Phase 2: Interrupt Bus** -- typed interrupt delivery with 4 modes (immediate, safe_boundary, waiting_only, manual_review), deduplication, expiry, full lifecycle tracking (pending -> delivered -> acknowledged), wired into app tick loop
  - **Phase 3: Memory Promotion and Injection** -- promotes high-confidence brain patterns into typed memory records, injects compact coordination context (leases, conflicts, blockers, handoffs, memory) into brain prompts before every decision
  - **Phase 4: External Agent Adapters** -- `AgentAdapter` trait with capability negotiation, Claude Code adapter wrapping existing discovery/terminal code, Codex stub adapter
  - **Evaluation layer** -- 10 coordination eval scenarios, metrics engine computing conflict rate, handoff completion rate, interrupt delivery rate, blocker resolution time from the event log
- **`--headless` mode** -- run the full autonomous stack (brain + coordination + context rot prevention) without a TUI. Attach a dashboard from another terminal. Emits structured JSON events to stdout
  - Automatic context rot intervention: raises `compact` interrupts at decay >= 50, `stop` at >= 85
  - Periodic coordination summaries every ~30s
  - Usage: `claudectl --headless --brain --auto-run`

### Technical details
- Feature-gated behind `--features coord` -- default build is unaffected (zero cost for users who don't need coordination)
- 12 new source files in `src/coord/` (adapter, CLI, evals, injection, interrupt bus, metrics, promotion, store, types)
- SQLite with WAL mode for concurrent access between headless and TUI processes
- 454 tests passing across both build configurations

## [0.32.0] - 2026-04-20

### Added
- **6 new `--brain-stats` subcommands** completing all metrics issues (#174, #175)
  - `distribution` — decision volume by tool, risk, project, action with inline bar charts
  - `novel-rate` — how quickly the frontier of novel situations shrinks
  - `false-deny` — false-deny rate and friction cost with 30% warning threshold
  - `calibration` — confidence vs actual accuracy, ECE score, per-tool calibration gap
  - `incidents` — post-mortem of every false approval with root cause classification
  - `time-to-correct` — user reaction latency to brain suggestions (protege effect)
- **`suggested_at` timestamp** on brain suggestions for reaction latency measurement (#175)
- **Demo mode** now shows cognitive decay icons and more brain activity (#173)

### Changed
- **Refactored `decisions.rs`** (3074 lines) into 3 focused modules: `decisions.rs` (992), `preferences.rs` (1950), `retrieval.rs` (302) (#176)

## [0.31.0] - 2026-04-19

### Added
- **Claude Code plugin** — integrates the brain directly into Claude Code sessions, no TUI required (#169)
  - PreToolUse hooks: `brain-gate.sh` (auto-approve/deny) and `budget-check.sh` (spend limits)
  - Slash commands: `/sessions`, `/spend`, `/brain-stats`, `/brain`, `/auto-insights`
  - Supervisor agent for session health triage
  - Session monitoring skill (auto-activated)
- **`--init` / `--uninstall`** — one-command setup to wire up Claude Code hooks in `.claude/settings.json` (#169)
- **`--brain-query`** — standalone brain query for single tool-call decisions (JSON output), used by plugin hooks (#169)
- **`--mode on|off|auto|status`** — toggle brain gate mode mid-session without restarting (#169)
- **`-s / --scope`** — configure hooks at user or project level (#169)
- **Auto-insights** — self-improving session analysis that detects friction patterns from brain decision history (#170)
  - 7 detectors: friction patterns, error loops, context blowouts, missing rules, accuracy gaps, temporal friction, cost trends
  - Differential tracking: only new insights are surfaced (fingerprint-based dedup)
  - `--insights [on|off|status]` — view insights or enable auto-generation every 10 decisions
  - `/auto-insights` plugin command
- **Impact scorecard** — `--brain-stats impact` with visual card layout, bar charts, and headline metrics (#171)
  - Auto-approve rate, brain accuracy, coverage vs static rules, dangerous ops blocked, time saved, learning curve
- Star prompt after first successful run (#168)
- `--demo` mode for fake sessions without Claude Code (#168)

## [0.30.0] - 2026-04-18

### Added
- **Cognitive rot detection** — temporal health monitoring that detects session degradation over time, not just point-in-time snapshots (#165)
  - Composite decay score (0-100) combining context saturation, error acceleration, token efficiency decline, and file re-read repetition
  - `check_proactive_compaction` — suggests `/compact` at 50% context usage (research shows degradation begins at 40-50%), independent of existing 80/90% context thresholds
  - `check_token_efficiency` — detects when a session spends increasingly more tokens per file edit vs its frozen baseline
  - `check_error_acceleration` — detects rising error rates over sliding windows vs a frozen baseline
  - `check_repetition` — detects files being re-read repeatedly without intervening edits (agent confusion signal)
  - `check_cognitive_decay` — composite check with severity-ranked icons: `◐` early (30-59), `◉` significant (60-79), `⊘` severe (80-100)
- **Cognitive Health section** in the detail panel showing decay score, efficiency vs baseline, error trend, repetition count, and context-aware mitigation suggestions
- `decay_score` field in `--json` output for programmatic access
- Brain context now includes decay score so the LLM factors cognitive health into its decisions
- Four new configurable thresholds in `[health]`: `decay_compaction_pct`, `efficiency_critical_factor`, `error_accel_factor`, `repetition_threshold`
- Demo mode showcases cognitive decay indicators on the ml-pipeline session

## [0.29.3] - 2026-04-18

### Fixed
- Kitty terminal not detected on Linux — `detect_terminal()` now checks `KITTY_WINDOW_ID` and `TERM=xterm-kitty` env vars before falling back to `TERM_PROGRAM`. Kitty on Linux doesn't set `TERM_PROGRAM`. (#160)
- Added native env var detection for WezTerm (`WEZTERM_EXECUTABLE`) and Ghostty (`GHOSTTY_RESOURCES_DIR`) as fallbacks when `TERM_PROGRAM` is not set.
- "No TTY associated with this session" error when pressing Tab in kitty — the blanket TTY guard now only applies to terminals that match by TTY name (tmux, WezTerm, iTerm2, Terminal.app). Kitty, Ghostty, and Warp use PID/cwd-based IPC and don't need a TTY. (#160)

## [0.29.2] - 2026-04-18

### Fixed
- Active sessions showing "No transcript" when JSONL files exist on disk. `cwd_to_slug` now strips trailing slashes before encoding, and a new fallback scan searches all project directories by session ID when the slug-based lookup fails. (#161)

### Added
- `--doctor` now includes a "Transcript Discovery" section that shows each active session's cwd, computed slug, and resolved JSONL path (or the exact paths tried when resolution fails).
- Debug-level logging at the transcript discovery step, showing which paths were tried and whether the fallback scan was used.

## [0.29.1] - 2026-04-17

### Fixed
- Few-shot retrieval rejection weight now auto-calibrates based on the user's actual accept/reject ratio instead of using a hardcoded factor of 8. Rare rejections (99/1) get amplified to 12, typical ratios (90/10) produce ~9, and frequent rejecters (60/40) see the weight drop to the floor of 3. (#158)

## [0.28.0] - 2026-04-16

### Added
- `--brain-stats` CLI command with four metrics subcommands for measuring brain effectiveness:
  - `learning-curve`: rolling correction rate over decision history with ASCII chart, phase transition detection, and improvement tracking (#129)
  - `accuracy`: per-tool, per-risk-tier, per-project, and temporal accuracy breakdown (#131)
  - `baseline`: replay all decisions against a deterministic rules-only classifier and compare accuracy by risk tier, with agreement analysis (#136)
  - `false-approve`: false-approve rate on risky actions by risk tier, with worst-case audit trail (#133)
- Risk tier classification system (Low/Medium/High/Critical) based on tool type and command patterns, shared across all metrics
- `src/brain/metrics.rs` module with 19 unit tests
- Passive observation logging: brain learns from ALL user actions, not just brain-involved decisions. Manual approves (`y` key), user input (`i` key), per-PID auto-approve (`a` key), static rule execution, and file conflict auto-deny all generate learning signals
- Multi-level learning architecture with four dimensions of intelligence:
  - **Rich context logging**: every decision captures 13 session state fields (cost, context%, errors, model, elapsed time, files modified, tool calls, conflicts, burn rate, subagents) — zero inference cost
  - **Conditional preferences**: distillation now learns context-dependent rules via Gini impurity splits (e.g., "approve git push when cost<$5", "deny writes when context>80%")
  - **Outcome tracking**: correlates consecutive decisions to detect "user accepted but it broke" (downweighted) vs "user rejected and it would have broken" (reinforced)
  - **Temporal patterns**: detects error streaks, cost pressure, and context pressure as compact situational rules in the prompt

### Fixed
- Observation records (passive learning signals) were silently dropped by the parser because `brain_action` field was required but observations have `null` — now correctly parsed

## [0.26.0] - 2026-04-16

### Added
- Continuous learning system: brain now closes the feedback loop — every accept, reject, auto-execute, and deny-rule override is logged and used to improve future decisions
- Preference distillation: decision history is periodically compacted into `~/.claudectl/brain/preferences.json` with compact rules like "always approve [Read]" — uses ~200 tokens vs ~500+ for raw few-shot examples, critical for Gemma4's limited context window
- Outcome-weighted few-shot retrieval: rejected decisions score higher than accepts (corrections are the strongest learning signal), with recency bonus for newer decisions
- Adaptive confidence thresholds: per-tool accuracy tracking adjusts the auto-execution bar — high-accuracy tools get lower thresholds (0.5), low-accuracy tools require 0.95 confidence. Below-threshold suggestions are automatically demoted to advisory mode
- Smart context budgeting: when distilled preferences exist, raw few-shot count is reduced to save context for transcript and decision prompt
- Auto-mode decision logging: all auto-executed brain suggestions are now recorded to decisions.jsonl (previously only advisory-mode accept/reject was captured)
- Deny-rule override logging: when static deny rules override brain suggestions, the override is logged so the brain learns the boundaries

## [0.25.2] - 2026-04-15

### Fixed
- Permission prompt detection: sessions waiting for tool approval (e.g., Web Search "Do you want to proceed?") now immediately show "Needs Input" instead of incorrectly showing "Processing" — uses pending_tool_name as primary signal instead of relying on 5-second age threshold

## [0.25.1] - 2026-04-15

### Fixed
- UTF-8 panic when brain processes transcripts containing multi-byte characters (em dashes, unicode, etc.) — 10 unsafe byte-slice truncations replaced with char-boundary-safe helper across 4 files

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

# Changelog

All notable changes to claudectl are documented here.

## [Unreleased]

### Added — agent-bus role binding (closes #307, #310)
- **PID-keyed role bindings.** The bus `roles` table gained a nullable `pid` column. When set, the resolver walks the caller's parent process chain (depth 8 via `getppid` + native `ps`) and picks the first role bound to any ancestor pid before falling back to cwd-inference. Disambiguates "two sessions in one worktree" — different pids, same cwd, distinct roles.
- **`claudectl bus role bind <NAME> <CWD> --pid <PID>`** — explicit pid binding for orchestrator scripts.
- **`claudectl bus role bind <NAME> --self`** — auto-detects Claude's pid by walking the ancestor chain looking for a process whose `ps -o command=` contains `claude`. Captures the current cwd. Used by the new `/bind` slash command.
- **TUI `Ctrl+R`** on the selected session opens a `role>` prompt and binds the selected session's pid + cwd through the new `Actions::bind_bus_role` trait method. Detail panel now shows `Bus role: <name> (bound by pid|cwd)` so the current binding is visible at a glance.
- **`/bind <role>` plugin slash command** (`claude-plugin/commands/bind.md`) — operator types `/bind frontend` from inside a Claude session; the plugin runs `claudectl bus role bind --self frontend`.
- **`bus role list`** prints the new pid column; **`bus whoami --json`** payload gains a `pid` field.

### Internals
- Schema migration is idempotent — guarded by a `PRAGMA table_info` check before `ADD COLUMN` (SQLite has no `IF NOT EXISTS` for column adds). Existing cwd-only bindings keep working unchanged.
- `upsert_role` uses `COALESCE` on the pid update, so a re-bind that only refreshes `session_id` doesn't clobber an existing pid.
- New runtime trait method `Actions::bind_bus_role(name, cwd, pid)`; LiveActions writes through `bus::store::upsert_role`; off-bus builds return a clear error.
- 3 new bus tests covering pid precedence, fall-through, and pid-preservation on re-bind. All 24 bus tests pass.

## [0.54.0] - 2026-06-06

### Internals (workspace refactor — closes epic #279)
- **`claudectl-tui` extracted into its own crate (closes #275).** The `App` state struct (3300 LoC), every `ui/*` render module (table, detail, help, status_bar, peers, skills), the recorder pair, and the demo fixtures now live in `crates/claudectl-tui/`. Depends on `claudectl-core` only. The binary keeps `brain_screen.rs` (the full-screen Brain Review surface) because it imports `brain::metrics` and `brain::risk`.
- **Dependency direction enforced at three levels.** `claudectl → claudectl-tui → claudectl-core` is checked by (a) a grep guard against `crate::{brain,bus,coord,hive,relay,…}` inside `claudectl-core/src/`, and (b) two standalone build jobs (`Core (standalone)`, `TUI (standalone)`) that catch creeping cross-deps even when the workspace happens to compile.
- **Eight runtime traits + DTOs in `claudectl-core::runtime`** are the only surface between the TUI and the binary's brain/bus/coord subsystems: `SessionSource`, `BrainView`, `BrainReviewView`, `CoordView`, `BusView`, `Actions`, `HiveActions`, `Orchestrator`, plus the stateful `BrainDriver`. The binary's `src/runtime/` provides `Live*` adapters; `MockRuntime` drives in-crate tests.
- **`hooks.rs`, `launch.rs`, `skills.rs` moved into core** (#300), as did the `BrainConfig` and `IdleConfig` data structs (#301). The binary still owns TOML parsing and CLI flag layering; only the value types are downstreamed.
- **Feature propagation:** the binary's `coord`, `relay`, `hive` features now cascade into `claudectl-tui` via the `claudectl-tui/coord` notation in `[features]`, so the same `#[cfg(feature = "…")]` gates resolve consistently across both crates.
- **CLAUDE.md updated** to describe the post-refactor layout and the no-upward-deps rule (closes #278).

### Compatibility
- No user-facing CLI changes. Existing `crate::*` paths inside the binary continue to resolve unchanged thanks to a thin re-export bridge in `src/lib.rs` (`pub use claudectl_tui::{app, demo, recorder, session_recorder, ui};`).
- No new dependencies. `claudectl-tui` pulls only what the TUI already used (`ratatui`, `crossterm`, `serde_json`).

## [0.53.0] - 2026-06-06

### Added
- **`claudectl init` — opinionated onboarding wizard (closes #257).** Single canonical first-run flow that walks five phases in order: weekly budget cap, local-LLM brain auto-detection (probes ollama / llama.cpp / LM Studio / vLLM), Claude Code hook install, agent-bus role binding, and curated skill suggestions. Replaces the planned `claudectl setup` verb from `docs/AGENT_BUS.md` § 8 — onboarding lives in one place.
- **`claudectl init --non-interactive`** with per-phase flags (`--budget`, `--brain-url`, `--install-plugin` / `--skip-plugin`, `--bus-role` / `--bus-cwd`, `--skip-*` for every phase). For CI and dotfile automation.
- **`claudectl init --check`** — drift report. Detects each phase's current state and diffs against the recorded marker; exits non-zero when the live environment no longer matches what was onboarded.
- **`claudectl init --remove`** — uninstall every claudectl-managed artifact (hooks, marker). Phases that own user state (the bus DB, the config file's `budget` line) deliberately decline to delete it — we don't erase a user's setup, only artifacts claudectl actively manages.
- **`claudectl init --reset`** — clear the onboarding marker so the next `init` starts fresh. Doesn't touch installed artifacts.
- **`~/.claudectl/onboarding.json` marker** — durable record of which phases ran, when, and against which claudectl version. Loaded via `serde_json` with `#[serde(default)]` on optional fields so older markers stay forward-compatible.

### Changed
- **Existing `--init` / `--uninstall` flags** are now deprecated aliases. They still write/remove the hook entries (existing dotfile automation keeps working), but each prints a deprecation note pointing at the new `init` subcommand. Slated for removal one release after consolidation.

### Internals
- New `src/init/` module replacing the single-file `src/init.rs`:
  - `hooks.rs` — moved unchanged from the old `init.rs` (the hook writer the plugin phase delegates to).
  - `marker.rs` — atomic-rename `OnboardingMarker` read/write at `~/.claudectl/onboarding.json`.
  - `prompt.rs` — minimal stdin/stdout helpers (yes/no, number-or-default, line-or-default).
  - `state.rs` — environment probes for each phase. Uses `curl --max-time 1` for HTTP probes (matching the existing brain client pattern; no new deps).
  - `phases.rs` — `Phase` trait + `Budget` / `Brain` / `Plugin` / `Bus` / `Skills` impls + the ordered `registry()`. Single uniform shape so the wizard, `--check`, and `--remove` all walk the same list without per-phase branching.
  - `mod.rs` — orchestrator (`run_wizard`, `run_non_interactive`, `run_check`, `run_remove`, `run_reset`) plus the drift-comparison logic (`is_drift` treats `not_installed` and `skipped` as equivalent so the report only flags real divergence).
- 21 new unit tests (marker roundtrip, drift comparison matrix, phase registry order, role-from-cwd derivation, TOML upsert, status-label stability). Plus a 9-scenario end-to-end smoke verifying every CLI verb (non-interactive all-skipped → marker → `--check` green → tamper → `--check` drift → `--remove` cleans up settings.json and marker; legacy `--init` still works and prints the deprecation note).

### Compatibility
- The Phase trait lets every phase live in its own file with no per-phase branching in the orchestrator — adding a new phase later (e.g., "MCP plugin discovery") is one new impl plus one line in `registry()`.
- No new dependencies. The wizard's brain probe and the budget-config writer both use the project's existing patterns (`curl` shell-out, tiny TOML upsert that avoids a `toml` crate dep).

## [0.52.0] - 2026-06-06

### Added
- **Agent bus Stop-hook delivery (Trigger A, phase 5 of `docs/AGENT_BUS.md`)** — closes the loop on bus messaging. After every turn finishes, the Claude Code plugin's new `Stop` hook drains the caller's mailbox and, when mail is present, returns `decision: "block"` with the rendered messages as `additionalContext`. The agent picks the work up **in the same turn** without waiting for the user to type `/inbox`. The bus is now self-driving.
- **`claudectl bus stop-hook` subcommand** — owns the Claude Code Stop-hook output protocol. Silent + exit 0 on every failure mode (no role bound, empty inbox, missing DB, ambiguous cwd) so the hook can never block a session because of a bus problem. All logic lives in Rust (`src/bus/stop_hook.rs`) where it is unit-tested.
- **`--json` flag on `bus inbox` and `bus whoami`** — machine-readable output for tooling. `inbox --json` soft-fails on unbound/ambiguous cwds (returns `{"role":null,"messages":[],"note":"..."}`) so the Stop hook never errors out on a session that hasn't bound a role yet.
- **`claude-plugin/hooks/scripts/inbox-drain.sh`** — Stop-hook wrapper installed by the plugin. Intentionally thin: protects against the case where `claudectl` is not on PATH, then delegates to `claudectl bus stop-hook`. Wired into `hooks.json` with a 5 s timeout.

### Internals
- New `src/bus/stop_hook.rs` module owning the Stop-output schema (`StopHookResponse`, `HookSpecificOutput`) and the markdown rendering of drained messages into context. Decoupled from the CLI so the schema is independently testable.
- `dispatch_inbox` / `dispatch_whoami` in `src/bus/cli.rs` refactored to separate data-fetch from rendering. Both now share a single `fetch_inbox` helper; human and JSON paths render the same `InboxOutcome` differently.
- 5 new unit tests covering Stop-hook envelope shape, pluralization, JSON wire format, and the critical "no raw newlines inside the JSON string fields" invariant (caught a real bug in development).

### Compatibility
- `claudectl bus inbox` without `--json` is unchanged — human-readable output, errors interactively on unbound/ambiguous cwds.
- No new dependencies. Stop hook ships behind the existing `bus` feature.

## [0.51.0] - 2026-06-06

### Added
- **Agent bus (phases 1–4 of `docs/AGENT_BUS.md`)** — a durable role directory + persistent mailbox exposed as an MCP server. Running Claude Code sessions discover each other (`list_agents`), look up their own role (`whoami`), send directed messages (`publish`), and drain their inbox (`read_inbox`) at turn boundaries. Gated behind the new opt-in `bus` Cargo feature.
- **`claudectl bus` CLI** with five verbs: `stdio` (run the MCP server, what the plugin invokes), `role bind/list` (durable role addresses), `send` (directed messaging), `inbox` (drain queued messages), `whoami` (resolve the caller's role from cwd or `CLAUDECTL_BUS_ROLE`).
- **Mailbox persistence** at `~/.claudectl/bus/bus.db` (SQLite WAL). Survives restarts; the role address outlives the session it was last bound to.
- **Content sanitization at the injection boundary.** A leading `/` in a message body is neutralized before delivery so a queued message cannot smuggle a slash command into the recipient. Subject grammar, type allowlist, and an 8 KiB body cap also enforced.
- **Claude Code plugin updates.** `claude-plugin/.mcp.json` registers the bus as an MCP server; `claude-plugin/commands/inbox.md` is the new `/inbox` slash command that drains the caller's mailbox through the `read_inbox` tool.

### Changed
- **Architecture invariants in `CLAUDE.md` carve out an exception for the `bus` feature.** The bus pulls rmcp + a current-thread Tokio runtime, deliberately relaxing the no-async-runtime rule for that feature path only. Default build is unchanged at ~3.5 MB / <50 ms startup; `--features bus` is ~6.4 MB.
- **Plugin manifest version** (`claude-plugin/.claude-plugin/plugin.json`) synced from a drifted `0.48.0` back to the crate version.

### Internals
- New `src/bus/` module: `store.rs` (SQLite schema, drain-on-read), `roles.rs` (cwd inference with macOS symlink canonicalization), `policy.rs` (sanitization + validation), `mcp.rs` (rmcp stdio server), `cli.rs` (subcommand dispatch).
- 16 new unit tests covering role resolution, ambiguity, env override, priority-ordered drain, drain-once idempotency, leading-`/` neutralization, subject grammar, and type allowlist. End-to-end MCP handshake + CLI roundtrip exercised before merge.

## [0.50.0] - 2026-05-29

### Added
- **Brain Scorecard** (`claudectl --brain-stats scorecard`). One-screen periodic-review surface: north-star auto-handled accuracy, guardrails (Critical-tier false-approve count + rolling override rate), latency p50/p95/p99, few-shot cache hit rate, per-risk-tier accuracy breakdown, counterfactual summary, and review status. The single command you want to run to see whether the brain is healthy.
- **Per-risk-tier breakdown** (`--brain-stats tier`). Accuracy, false-approves, false-denies, and override rate split by `Low` / `Medium` / `High` / `Critical` tier. Critical-tier false-approves are flagged with a warning marker — they are the safety-critical number.
- **Latency report** (`--brain-stats latency`). p50/p95/p99/mean/max + ASCII distribution histogram over the new `brain_decision_ms` field. Reads gracefully on histories without instrumentation.
- **Cache hit report** (`--brain-stats cache`). Percentage of decisions handled from the few-shot store without an LLM call, over the new `cache_hit` field.
- **Counterfactual analyzer** (`--brain-stats counterfactual`). Surfaces user-overrides where the subsequent same-PID outcome failed (brain was right) or succeeded (brain over-cautious). Each entry prints a one-shot `--brain-mark-canonical <id>` command for promotion.
- **Interactive review CLI** (`claudectl --brain-review`). Walks the prioritized queue (counterfactual brain-was-right → Critical-tier false-approves → high-confidence calibration misses) one decision at a time with `m`/`n`/`s`/`d`/`q` controls. `--brain-review list` prints the queue non-interactively.
- **Canonical teaching store**. Decisions marked canonical (via the review flow or `--brain-mark-canonical <id>`) are appended to `~/.claudectl/brain/canonical.jsonl` and get a `+50` score boost in `retrieval::retrieve_similar`, so reviewed examples dominate future few-shots. Each review pass becomes supervised training signal.
- **Brain Review TUI mode** (`M` hotkey). Full-screen mode integrated with the dashboard: Scorecard tab mirrors the CLI scorecard; Review tab provides a list + detail split with `j/k` navigate, `m` mark canonical, `n` mark with inline note, `s` skip, `r` refresh, `Tab` cycle, `Esc/M/q` close. Marked items drop from the queue in-place and selection advances — triage is one keystroke per item.
- **DecisionRecord schema extensions**. `brain_decision_ms: Option<u64>`, `cache_hit: Option<bool>`, `canonical: Option<bool>` on every record. `Option`-wrapped for full backward compat with existing decision logs. New `log_decision_full(..., brain_decision_ms, cache_hit)` for instrumented call sites; the legacy `log_decision` continues to work and writes `None`.
- **Source-built `packaging/homebrew-core/claudectl.rb`** formula template with `livecheck`, `generate_completions_from_executable`, `man1` install, and a real `test do` block. Submitted to Homebrew/homebrew-core (declined for now per the self-submission notability bar — re-pursueable when star/fork/watcher thresholds are cleared).

### Changed
- **Repositioned every public-facing surface** to the new tagline: *"Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you."* Lands in the README hero, mkdocs site description, `docs/index.md`, `docs/llms.txt`, `Cargo.toml` description, clap `--help`, `flake.nix`, `AGENTS.md`, `CLAUDE.md`, `LAUNCH_POSTS.md`, `blog/posts.md`, the nixpkgs handoff README, and the GitHub repo description. Homebrew `desc` and AUR `pkgdesc` use the 73-char trimmed variant *"Orchestrate a swarm of Claude Code agents with a learning local-LLM brain"* to fit their 80-char cap.
- **Homebrew-core template** `desc` trimmed from 82 → 61 chars to clear `brew audit --strict --new --online`'s 80-char limit, and pinned to the v0.49.3 source-tarball sha256 so future bumps start from a known-good baseline instead of the `REPLACE_WITH_SHA256` placeholder.

### Internals
- Bulk-extended every `DecisionRecord` construction site across `briefing.rs`, `detectors.rs`, `insights.rs`, `metrics.rs`, `pref_store.rs`, `preferences.rs`, `retrieval.rs`, `sequences.rs`, and `hive/distiller.rs` for the three new fields. All existing tests pass without modification.
- `src/brain/review.rs` (new): `ReviewItem`, `build_queue`, `mark_by_id`, `run_interactive`, `print_queue`.
- `src/ui/brain.rs` (new): full-screen renderer mirroring the Skills & Hive K-screen pattern.

## [0.49.3] - 2026-05-27

### Added
- **`CLAUDECTL_DEMO_SKILLS=1` demo recording hook.** With this env var plus `--demo`, claudectl boots straight into the Skills & Hive mode with a scripted tab-rotation in `refresh_demo` (Skills → Hive → Skills every 14 ticks) and seeded peer/invite data so the Hive tab renders convincingly even without the `relay` feature compiled in. Lets `scripts/record-demos.sh skills` produce a deterministic GIF for launch posts.
- **`scripts/record-demos.sh skills` target** — records `docs/assets/claudectl-demo-skills.gif` (30 s, both tabs) using the same agg flags as the other demo gifs. Bundled into the `all` target.
- **`docs/assets/claudectl-demo-skills.gif`** — embedded in `docs/index.md` Screenshots and `docs/reference.md` Skills & Hive section.

## [0.49.2] - 2026-05-27

### Fixed
- **Skills & Hive footer now sticks to the bottom** of the screen. The previous version reserved 9 rows for the footer but only filled 3–4, leaving a band of empty space above the bottom border. Restructured so the body uses `Min` and the hint strip is a tight 1–2 rows pinned at the bottom; selected-skill detail (path + status) moves into the body section above the hint.

## [0.49.1] - 2026-05-27

### Changed
- **Skills & Hive is now a full-screen mode**, not a centered overlay. Pressing `K` swaps the entire frame from the session table to the Skills & Hive view; `Esc` / `K` / `q` returns to the table. Same two tabs (Skills, Hive) and same hotkeys as before.
- **`K:skills` hint added to the bottom footer** of the session table so the shortcut is discoverable. Empty-state hint (no sessions) also calls out `K`.

## [0.49.0] - 2026-05-27

### Added
- **Skills & Hive TUI overlay** — press `K` from the TUI to open a Skills & Hive panel. Two tabs (Tab to switch):
  - **Skills tab** lists every Claude Code skill on disk (scans `~/.claude/skills`, `~/.claude/plugins/*/skills`, and `<cwd>/.claude/skills`). A `✓` marker shows which skills are already shared with the local hive; `s` shares the highlighted skill via the existing hive pipeline. Honours the 32 KiB skill-share limit and surfaces a warning when a skill exceeds it.
  - **Hive tab** shows local identity, listener status, and known peers (read from `~/.claudectl/relay/peers/`). Hotkeys: `h` start hive listener (spawns detached `claudectl relay serve`), `i` generate an invite (relay code, word phrase, and invite link, shown inline), `J` join a hive via pasted code/link/words (detached `claudectl relay join`), `r` refresh peers.
- **`hive::cli::share_artifact_from_path()`** — public wrapper around the previously private CLI-only `cmd_share` so callers outside the dispatch table (the new TUI overlay) can share skills/commands/hooks without reimplementing frontmatter + scope parsing.
- **`src/skills.rs`** — new skill discovery module with YAML frontmatter parsing, source classification (user / plugin / project), and a shared-key lookup that aligns with the hive's `skill:<lowercased-name>` semantic key.

### Technical details
- Detached subprocess spawning for `relay serve` and `relay join` keeps the TUI event loop responsive; the invite generator shells out to `claudectl --json relay invite --words` and parses the JSON envelope.
- New module wired into `src/lib.rs`, `src/main.rs`, and `src/ui/mod.rs`; help overlay (`?`) gains a `K` entry.
- 585 tests passing (5 new: 3 for the skills module, 2 for the overlay rendering).

## [0.48.0] - 2026-05-12

### Added
- **Test-failure feedback loop** -- when a configured test runner (`cargo test`, `npm test`, `pytest`, `go test`, `bun test`, ...) exits non-zero, the reaper fans the failure out to the most recent brain-approved `Edit`/`Write`/`MultiEdit`/`NotebookEdit` decisions in the same project within a 5-minute window and tags them as `DecisionOutcome::TestFailed` (#238). Distillation weights `TestFailed` more strongly than transient `Error` (0.1 vs 0.3 for accepted-but-broken; 2.0 vs 1.5 for rejected-rightly), so a broken build is the strongest negative signal the brain has.
- **`test_runners` config** -- `[brain]` section accepts an override list; sensible defaults cover the major language runners. Empty list disables fan-out.
- **`continueOnBlock` for deny reasoning** -- `brain-gate.sh` emits the `hookSpecificOutput.continueOnBlock` envelope alongside the legacy `{decision, reason}` so newer Claude Code surfaces `permissionDecisionReason` and `systemMessage` into the model's next turn instead of blocking opaquely (#249). The brain stops being a wall and starts being a teacher.
- **Below-threshold approval advisory** -- uncertain approvals (below the adaptive threshold) emit `hookSpecificOutput.additionalContext` so Claude picks up the brain's hesitation without being blocked.
- **Robust hook output** -- `brain-gate.sh` now prefers `jq` for parsing the brain's response and constructing its envelope, with a manual JSON-escape fallback. Fixes a latent bug where reasoning containing quotes, backslashes, or newlines could corrupt the hook's stdout.

### Technical details
- `DecisionOutcome::TestFailed(String)` carries the failing test command; backfill overlays it onto `DecisionRecord.outcome` after the consecutive-pair pass so a marker beats a clean tool-error signal.
- `BrainConfig.test_runners: Vec<String>` parsed from `[brain]` TOML; `default_test_runners()` exposed for tests.
- Fan-out is idempotent via `create_new` on `test-failures/<decision_id>.json` markers; 5-minute attribution window, capped at 5 recent edits per failure.
- New hook envelope is unconditional -- Claude Code < 2.1.138 ignores the extra fields and falls back to the legacy deny.
- 1240 tests passing across all build configurations (12 new for this release).

## [0.45.0] - 2026-04-28

### Added
- **Configurable event log retention** -- `retention_days` in `[lifecycle]` config section controls auto-prune period (default 30 days), wired to headless auto-prune loop (#186)
- **Per-session recording toggle** -- `R` key now starts/stops recording for the selected session only, not all recordings. Recordings include a ~30-second lookback buffer of events before record-start. Output filenames include timestamps for uniqueness (#73)
- **Config validation** -- `claudectl --config-validate` reports unknown keys, unknown sections, and malformed values in config files with line numbers and actionable messages (#74)
- **Hook dry-run** -- `claudectl --init --dry-run` shows what hooks would be written to `.claude/settings.json` without modifying the file (#74)
- **Sample config generation** -- `claudectl --config-init` writes an annotated `.claudectl.toml` template in the current directory (#74)
- **False-deny friction cost** -- `--brain-stats false-deny` now shows friction cost (avg override delay, total friction time) and override reason breakdown. Brain denial overrides prompt for categorized reasons: always safe, one-time exception, or brain is wrong (#134)
- **Override reason capture** -- when accepting a brain denial, TUI prompts for override reason (1/2/3 keys) to feed back into preference distillation
- **Decision record timestamps** -- `resolved_at` field on all brain decisions enables friction latency measurement

### Technical details
- `LifecycleConfig` gains `retention_days: u64` (default 30), parsed from `[lifecycle]` TOML section
- `SessionRecorder` lookback: seeks back 50KB and aligns to line boundary before recording
- `validate_config_file()` enumerates valid keys per known section and reports unknowns
- `DecisionRecord` gains `resolved_at: Option<u64>` and `override_reason: Option<String>`, backward-compatible with old JSONL
- 677 tests passing across all build configurations

## [0.44.0] - 2026-04-27

### Added
- **Session state in heartbeats** -- relay heartbeats now carry the worker's session list, enabling cross-machine visibility (#107)
  - `WorkerState` storage in `PeerRegistry` with automatic stale worker expiry (3x heartbeat interval)
  - Backward compatible: peers running older versions send empty payloads (liveness-only)
- **HTTP coordinator API** -- lightweight raw-TCP HTTP/1.1 server for coordinator mode, zero new dependencies (#108)
  - `POST /api/heartbeat` -- receive worker session state
  - `GET /api/sessions` -- unified session list across all connected workers
  - `GET /api/workers` -- worker status summary with staleness detection
  - Bearer token auth, 1 MB body cap, background thread
  - CLI: `claudectl relay serve --http-port 9876 --auth-token <token>`
  - Config: `[relay]` section supports `http_port` and `auth_token`
- **Unified dashboard** -- remote sessions from connected workers appear in the TUI alongside local sessions (#109)
  - Remote sessions shown with `[worker-id] project` prefix
  - Terminal actions (kill, approve, input, compact, switch) gracefully blocked for remote sessions
  - Peers panel now shows session count per peer
  - Demo mode includes fake remote sessions from connected peers
- **Secure pairing** -- confirmed already complete: HMAC challenge-response, PSK, invite codes/words/links/QR, LAN discovery, rate limiting (#113)

### Technical details
- All new code feature-gated behind `--features relay` -- default build unaffected
- HTTP server uses `std::net::TcpListener` (same pattern as relay listener) -- no new runtime dependencies
- `ClaudeSession` gains `worker_origin: Option<String>`, `is_remote()`, and `from_remote_json()` for remote session hydration
- 596 tests passing across all build configurations

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

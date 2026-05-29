# claudectl Social Posts

---

## 1. r/ClaudeAI

**Title:** I built a local LLM that learns how you use Claude Code and starts auto-piloting it

**Body:**

I've been running 5-8 Claude Code sessions at a time and got tired of tab-switching to approve tool calls. So I built claudectl — a TUI that sits on top of all your sessions and lets a local LLM (ollama/llama.cpp) handle approvals for you.

The part I'm most excited about: **it learns from your corrections.**

When the brain suggests an action, you press `b` to accept or `B` to reject. Every correction gets logged. After ~10 decisions, it distills your corrections into compact preference patterns — things like "always approve cargo test" or "never allow rm -rf outside /tmp". It also tracks accuracy per tool, so if it keeps getting Bash wrong, it raises its own confidence bar before acting.

After 50+ decisions, it basically knows your style. Rejections are weighted 8x heavier than approvals so it learns your "no"s fast.

Everything runs locally. No cloud API, no telemetry. Decision logs and preferences live in `~/.claudectl/brain/`.

What it does beyond the brain:

- Dashboard showing all active sessions, status, cost, burn rate
- Health monitoring (loop detection, stalls, cost spikes, context saturation)
- File conflict detection across sessions
- Multi-session orchestration with dependency ordering
- Session highlight reel recording (GIF/asciicast)
- Approve permission prompts without leaving the dashboard

[showcase GIF]

```
brew install mercurialsolo/tap/claudectl
claudectl --demo          # try it without Claude running
claudectl --brain         # the real thing (needs ollama)
```

~1MB binary, sub-50ms startup, 7 runtime dependencies. Written in Rust.

GitHub: https://github.com/mercurialsolo/claudectl

---

## 2. r/LocalLLaMA

**Title:** Using a local model (gemma3) to auto-approve/deny Claude Code tool calls — it learns your preferences over time

**Body:**

I built a tool called claudectl that puts a local LLM between you and Claude Code's permission prompts. The idea: instead of you manually approving every `cargo test` or denying every `rm -rf`, a small local model makes the call.

**How the learning loop works:**

1. Brain observes a pending tool call (tool name, command, project, cost, recent conversation)
2. Queries your local model for a decision (approve/deny/terminate/route)
3. In advisory mode, you see the suggestion and press `b`/`B` to accept/reject
4. Every decision gets logged with full context
5. Every 10 decisions, a background distillation pass runs — groups decisions into preference patterns
6. These patterns (~200 tokens) get injected into future prompts instead of raw few-shot examples (~500+ tokens)
7. Per-tool confidence thresholds adapt: if the model keeps getting `Bash` wrong, it needs higher confidence before auto-executing

Rejections are weighted 8x vs approvals (3x), so the model learns your hard "no"s fast. Recency bonuses ensure it adapts to changing preferences.

**What the model sees per decision:**
- Project name, session status, pending tool + command
- Cost, burn rate, context window utilization
- Last 8 transcript messages (earlier ones compacted)
- All other active sessions (for cross-session reasoning)
- Distilled preference patterns from your history

After ~50 decisions you can flip to `--auto-run` mode and the brain just handles it.

**Supported backends:** ollama, llama.cpp, vLLM, LM Studio — anything that takes a JSON POST and returns text.

```bash
ollama pull gemma3 && ollama serve
claudectl --brain
```

Default model is gemma3 but anything works. The prompts are overridable by dropping files in `~/.claudectl/brain/prompts/`.

[showcase GIF]

All decision data stays on your machine. No cloud calls, no telemetry.

GitHub: https://github.com/mercurialsolo/claudectl

---

## 3. Hacker News

**Title:** Show HN: claudectl -- local LLM brain that learns to auto-pilot Claude Code sessions

**Body:**

claudectl is a terminal dashboard for supervising multiple Claude Code sessions. Its main feature is the "brain" — a local LLM (via ollama, llama.cpp, vLLM, or LM Studio) that observes your sessions and makes real-time approve/deny/terminate decisions on tool calls.

The brain learns from your corrections. Every accept/reject is logged, distilled into preference patterns every 10 decisions, and used to adapt future behavior. Accuracy is tracked per tool — if the model keeps misjudging Bash commands, it raises its confidence threshold for that tool. Rejections carry 8x the weight of approvals so hard "no"s are learned quickly.

After enough corrections (~50), you can switch to auto mode and let it run.

Other features: health monitoring (loop detection, stalls, cost spikes, context saturation), file conflict detection across sessions, multi-session orchestration with dependency ordering, session highlight reel recording.

Design constraints: ~1MB binary, sub-50ms startup, 7 runtime dependencies, no async runtime, synchronous with polling. Written in Rust.

All data stays local. No cloud API, no telemetry.

[showcase GIF]

https://github.com/mercurialsolo/claudectl

---

## 4. r/rust

**Title:** claudectl: 1MB Rust binary that auto-pilots Claude Code with a local LLM brain — 7 deps, no async, sub-50ms startup

**Body:**

I've been building claudectl — a TUI for supervising Claude Code sessions. Sharing because some of the constraints might be interesting to this community.

**Binary constraints:**
- Under 1MB release binary
- Sub-50ms startup
- 7 runtime crates total
- No async runtime — synchronous with polling
- Native `ps` for process introspection instead of the `sysinfo` crate (saves ~400KB)

**The main feature** is a "brain" — a local LLM that observes sessions and auto-approves/denies tool calls. The interesting Rust bits:

- **Incremental JSONL parsing** — tracks file offsets per session, never rereads. Sessions write large conversation logs and rescanning would kill refresh rate.
- **Deny-first rule evaluation** — deny rules always win regardless of config order. The brain can suggest approve, but a deny rule overrides it. Made this a type-level guarantee rather than runtime ordering.
- **Preference distillation** — decision logs get compacted into ~200 token preference patterns via a background pass every 10 decisions. Outcome-weighted scoring: rejections 8x, approvals 3x, with recency bonuses.
- **Multi-signal status inference** — combines CPU usage, JSONL events, and timestamps to determine if a session is processing, waiting, stalled, or needs input. No single signal is reliable alone.
- **Terminal backend abstraction** — Ghostty, Kitty, tmux, WezTerm, Warp, iTerm2, Terminal.app, Gnome Terminal, Windows Terminal. Each has different escape sequences for tab switching and input injection.

No `unsafe`. Uses `ratatui` for the TUI. Config is layered TOML (CLI flags > project > user > defaults).

[showcase GIF]

```
cargo install claudectl
claudectl --demo    # fake sessions, no Claude needed
```

GitHub: https://github.com/mercurialsolo/claudectl

Happy to answer questions about keeping the dependency count low or the no-async design.

---

## 5. r/commandline

**Title:** claudectl: orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you

**Body:**

I run multiple Claude Code sessions in parallel and needed a way to see all of them at once and handle permission prompts without switching tabs. So I built claudectl.

It's a terminal dashboard that shows every active Claude Code session with status, cost, burn rate, model, project, and pending tool calls. You can approve prompts, launch/resume sessions, and record highlight reels — all from one screen.

The headline feature: a **local LLM brain** that watches your sessions and handles approvals for you. It starts in advisory mode (suggests, you confirm), learns from your corrections, and after ~50 decisions it knows your style well enough to run on auto. All local, powered by ollama or any local inference server.

**Keybindings from the dashboard:**
- `y` — approve a permission prompt
- `n` — launch a new session
- `r` — resume a session
- `R` — record a session highlight reel
- `b`/`B` — accept/reject brain suggestion
- `q` — quit

[showcase GIF]

```
brew install mercurialsolo/tap/claudectl
claudectl --demo    # try it right now
```

~1MB binary, starts in under 50ms. Written in Rust.

GitHub: https://github.com/mercurialsolo/claudectl

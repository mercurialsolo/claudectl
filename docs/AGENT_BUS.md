# claudectl Agent Bus — Design Specification

**Status:** Draft / RFC. Phases 1–4 of §12 implemented behind the `bus` Cargo feature (see [Implementation status](#implementation-status) below). Delivery mechanism verified against the current Claude Code Hooks reference (June 2026).
**Scope:** Turn claudectl from a session monitor into a message bus that lets running Claude Code instances discover each other and coordinate, with agent-driven routing, persistent mailboxes, and centralized content/flow guardrails.

## Implementation status

| Phase (§12) | Status | Module / artifact |
| --- | --- | --- |
| 1. Roles + `whoami` + `list_agents` | **Shipped** | `src/bus/roles.rs`, `src/bus/mcp.rs` |
| 2. In-process bus MCP server, autostarted | **Partial** — runs as a stdio subprocess (`claudectl bus stdio`); the in-process autostart-with-TUI form (Unix-socket singleton) is not built yet. | `src/bus/mcp.rs`, `claude-plugin/.mcp.json` |
| 3. Provisioning (role binding + plugin install + hook capability probe) | **Folded into [`claudectl init`](https://github.com/mercurialsolo/claudectl/issues/257)** — there is no separate `claudectl setup` verb. Non-interactive equivalent ships today as `claudectl bus role bind <name> <cwd>`. | — |
| 4. Mailbox + directed `publish` / `read_inbox` | **Shipped** | `src/bus/store.rs`, `src/bus/mcp.rs` |
| 5. `Stop` hook → `read_inbox` (Trigger A) + `/inbox` fallback (Trigger B) | **Partial** — `/inbox` slash command ships; `Stop` hook + continue-in-turn not yet. | `claude-plugin/commands/inbox.md` |
| 6. Command sanitization + content validation | **Shipped (subset)** — leading-`/` neutralized, body cap, subject grammar, type allowlist. Hop/rate/echo guards not yet. | `src/bus/policy.rs` |
| 7. Subjects + `subscribe` + claim protocol | **Not started** | — |
| 8. Flow guards (rate/hop/loop/cost) + ACLs | **Not started** | — |
| 9. Managed-artifact lifecycle (update/migrate/drift) | **Not started** | — |
| 10. Supervisor + long-horizon persistence | **Not started** | — |
| 11. TUI bus view | **Not started** | — |

**Build / run:** `cargo build --release --features bus`. End-to-end smoke covered: role bind → cwd inference (macOS symlink resolution) → directed send → priority-ordered drain → leading-`/` neutralization → policy rejections. MCP handshake (`initialize` + `notifications/initialized` + `tools/list`) verified end-to-end. **Not yet exercised:** the plugin loaded inside Claude Code driving real cross-session traffic.

The dependency cost of `--features bus` is a 6.4 MB release binary (vs. the default 3.5 MB) and a current-thread Tokio runtime inside `claudectl bus stdio`. The default build is otherwise untouched.

---

## 1. Motivation

claudectl already knows every running Claude Code session (PID, cwd, status, session ID) and already knows how to inject input into a session via terminal control (`tmux send-keys`, Kitty/Ghostty remote control, etc.). Today these are exposed only as TUI keybinds, and the loop is never closed: claudectl *detects* when a session is `Waiting`, and can *inject* input, but never connects the two.

This spec closes that loop by exposing claudectl as an **MCP server**: a directory of running agents plus a persistent mailbox per agent. Running instances then coordinate themselves — the "what is my next step" decision lives in each agent's own reasoning and transcript, rather than in an external orchestrator.

### The MCP server starts with claudectl (no separate command)

The MCP server is **not** a separate `claudectl mcp` invocation the user must remember. When `claudectl` launches (TUI or any mode), it starts the bus MCP server **by default** as part of normal startup — same process, one core, the TUI and the MCP server are two frontends over it (§7). The server is up whenever claudectl is running, which is exactly when agents need it. Flags exist to disable it (`--no-bus`) or run headless server-only (`--bus-only`, for a daemon/CI context with no TUI), but the default is: start claudectl, the bus is live.

### Relationship to native agent teams (the key positioning)

Claude Code ships a native **agent teams** feature (experimental, Opus 4.6+, `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`). It already implements, *within one team's lifetime*, the exact intra-team primitives this spec's core describes: a shared task list with dependency auto-unblocking, a mailbox with automatic message delivery (no polling), and file-locked task claiming. claudectl should **not reinvent these**.

But agent teams are a **bounded-burst** tool — create → parallelize one task → synthesize → tear down, on a minutes-to-hours horizon — and are structurally unfit for long-lived operation (see §13). claudectl's differentiated role is the **durable supervision and persistence layer that agent teams lack**:

- Agent teams are the *ephemeral execution unit* (a burst of parallel work).
- claudectl is the *long-horizon layer*: a durable role directory and mailbox that outlive any team's teardown, supervision/restart of dead leads, and orchestration of repeated bursts over a project that runs for weeks, months, or longer.

So the framing is **layered, not competing**: where agents are inside one native team, the bus defers to the team's own task list/mailbox; the bus owns everything the team cannot — cross-team, cross-worktree, cross-machine, and cross-time coordination.

### Relationship to declarative routing (`workflow.toml`)

Two philosophies, evaluated as alternatives rather than merged:

- **Centralized router (`workflow.toml`):** an external process watches status, decides, and routes. Agents are passive nodes in a fixed graph.
- **Agent bus (this spec):** claudectl is a bus the agents query and drive. Routing intelligence lives in the agents.

The bus is the better fit when steps are not a fixed graph (usually the case with Claude instances). `workflow.toml` remains available as an opt-in for genuinely fixed orchestration.

---

## 2. Coordination model: publish/subscribe with optional directed send

This is the central design decision. Two candidate models:

| Model | Sender decides recipient? | Scales with agent count | Risk |
| --- | --- | --- | --- |
| **Directed send** | Yes ("A → B") | Poorly — every sender models who does what | Routing knowledge duplicated/rots |
| **Publish/subscribe** | No — broadcasts an event | Well — senders need no recipient model | Task can fall through if no subscriber claims it |

**Chosen design: pub/sub as the transport, with optional `addressed_to`, plus a claim protocol.**

Rationale: the bus's whole point is pushing the next-step decision into each agent's reasoning. Pub/sub is the consistent expression of that — the sender doesn't need to know who cares; each subscriber owns exactly one decision ("is this my concern?"). Adding an agent requires changing nothing about existing agents. Topics keep wake-cost bounded: an agent only wakes for subjects it subscribed to.

The known weakness of pub/sub — a task no one claims — is handled by a **claim/ack protocol** plus a fallback escalation, giving the decoupling of pub/sub without the dropped-task risk. Directed send remains available (`addressed_to`) for the cases where the sender genuinely knows the recipient.

> **Scope boundary with native teams.** This model governs coordination *between* agents that are not in the same native agent team — separate worktrees, separate teams, or long-lived roles. Agents *inside* one native team already coordinate through that team's shared task list and auto-delivered mailbox; the bus does not duplicate or intercept that intra-team traffic. The bus's pub/sub is the inter-team / cross-time fabric; the native task list is the intra-team fabric. They compose (§13).

### Subjects

Messages are published to a dot-delimited subject, NATS-style:

```
task.created
task.completed
review.requested
review.completed
status.update
question.raised
handoff
```

Agents subscribe to subject patterns (`task.*`, `review.requested`). Subjects are convention, not hardcoded; the policy file may constrain which roles may publish/subscribe to which subjects.

### Claim protocol (for work-bearing subjects)

1. Agent publishes `task.created` (no addressee).
2. Subscribers wake, evaluate relevance.
3. First interested agent calls `claim(message_id)`. The bus grants the claim atomically (single winner); others receive "already claimed" and drop it.
4. Claimer publishes `task.completed` (or `task.failed`) referencing the original `message_id`/`thread_id`.
5. If unclaimed after `claim_timeout`, the bus escalates: re-publish with higher priority, route to a designated `fallback` role, or notify the human via the TUI.

Non-work subjects (`status.update`) are fire-and-forget, no claim needed.

**Alignment with native Claude Code events.** Claude Code exposes `TaskCreated` and `TaskCompleted` lifecycle hooks (both can block on exit 2 — rolling back creation or preventing completion). Where agents use Claude Code's native task mechanism, the bus's `task.created` / `task.completed` subjects and the claim protocol should bind to these real events rather than being simulated, so task state on the bus stays consistent with the agent's own task state. This is an integration opportunity, not a requirement — bus subjects also work for agents not using native tasks.

---

## 3. MCP tool surface

Exposed by a `claudectl mcp` subcommand (see §7). All tools are namespaced under the claudectl MCP server each instance adds to its own config.

### Discovery

- **`list_agents()`** → live roster: `role`, `cwd`, `status` (`Waiting`/`Processing`/`NeedsInput`/`Idle`/`Finished`), `last_seen`, subscribed subjects. The discovery half — "who's up, what are they doing, what do they listen for."
- **`whoami()`** → caller's own `role` / address. Used for self-registration (§5).

### Messaging

- **`publish(subject, body, thread_id?, addressed_to?, type, priority?)`** → publish an event to the bus. `addressed_to` optionally pins a recipient (directed-send special case). `type` is a declared message type (§4).
- **`subscribe(subjects[])`** → register interest in subject patterns. Persisted per role.
- **`read_inbox(since?)`** / **`check_messages()`** → pull pending messages addressed to or matching the caller's subscriptions.
- **`claim(message_id)`** → atomically claim a work-bearing message. Returns granted/already-claimed.
- **`ack(message_id, result?)`** → acknowledge completion; closes the thread or emits a completion event.

---

## 4. Persistent mailbox

The mailbox is the core reliability primitive.

**Problem it solves:** raw injection is fire-and-forget. If the recipient is mid-turn when a message arrives, injected keystrokes land in a half-finished prompt and corrupt it. The mailbox decouples *send* from *delivery*.

- Senders write to a recipient's (or subject's) mailbox at any time.
- claudectl delivers only on the recipient's `Processing → Waiting` edge (§6).
- Messages survive a recipient restart, because the mailbox is **claudectl's state**, not keystrokes typed into a now-dead pane.

The mailbox is a property of a **role** (a persistent address), not of a session process. A restarted session re-binds to its role and finds its mailbox intact.

**Persistence:** SQLite (WAL mode) under `~/.config/claudectl/bus.db`. Chosen over flat JSON because multiple session processes may touch the store concurrently; WAL gives safe concurrent writers and crash recovery. Schema sketch:

```
messages(id, subject, type, sender_role, addressed_to,
         thread_id, body, priority, status,
         claimed_by, created_at, delivered_at, acked_at)
subscriptions(role, subject_pattern)
roles(role, cwd_selector, last_session_id, last_seen)
```

---

## 5. Self-registration / addressing

An agent needs to know its own address. Two mechanisms, first preferred:

1. **cwd/PID inference (zero agent cooperation):** the MCP call arrives from a known session; claudectl maps the calling session's cwd to a role via config selectors. Works without the agent doing anything.
2. **Explicit `--role` at launch:** `claudectl --new --role planner ...`; the agent reads it back via `whoami()`. Required as a fallback when multiple sessions share one cwd (e.g. two agents in the same worktree), where cwd inference is ambiguous.

Roles are the stable names everything else references. Session IDs are ephemeral and never used as durable addresses.

---

## 6. Notification & delivery handshake

This is **push, not periodic polling**. The agent does nothing proactive; it is woken when it next finishes a turn and has mail. Claude Code's mature hook system (verified against the current Hooks reference) makes the primary path far cleaner than terminal injection — claudectl exposes `read_inbox` as an MCP tool, and the recipient's own `Stop` hook calls it. There are two triggers; Trigger A is now strongly preferred and Trigger B is a rarely-needed fallback.

### Trigger A — Claude Code `Stop` hook → claudectl MCP tool (primary)

Claude Code's `Stop` event fires when the model finishes responding, and a hook handler may be of type `mcp_tool`, calling a tool on an already-connected MCP server. So the recipient's `Stop` hook calls claudectl's `read_inbox` MCP tool **directly** — no terminal injection, no `/dev/tty`, no slash command needed for the common case. The agent drains its mailbox the instant it goes idle, through a fully controlled, structured path.

Two delivery modes off the same hook:

- **Notify-and-idle:** the `Stop` hook surfaces any messages, the turn ends, the agent acts on its next turn.
- **Continue-in-turn (preferred when mail is waiting):** a `Stop` hook can return `decision: "block"` (or `hookSpecificOutput.additionalContext`) to *keep the conversation going* — feeding the new message into the agent so it picks the work up **in the same turn** instead of going idle. This is strictly better than the old "wait for idle, then nudge" loop: delivery is immediate at turn boundary, with no second round-trip.

This path also structurally **cannot corrupt the prompt**: command hooks run without a controlling terminal and may only surface output via the restricted `terminalSequence` allowlist (notifications/titles/bell — nothing that moves the cursor). The "never inject mid-turn" rule (§4) is enforced by the hook system itself, not just by convention.

### Trigger B — claudectl `Waiting`-edge injection (fallback only)

Used only where a `Stop` hook can't be installed — e.g. an enterprise `allowManagedHooksOnly` policy blocks user/project/plugin hooks (§8), or the agent config is otherwise not under claudectl's control. claudectl falls back to what it already does: watch the JSONL, detect the `Processing → Waiting` edge, inject a short `/inbox` nudge (§9) via the terminal path. Same barge-in safety (delivery only at idle), but less clean and more terminal-dependent than Trigger A.

### Native loop safety

The "two agents volley forever" fork-bomb has a built-in backstop independent of the bus's own hop caps: if a `Stop` hook blocks 8 times in a row within one turn, Claude Code short-circuits and ends the session (override via `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP`). The bus's hop/loop guards (§10) layer on top of this, but the runtime already prevents the degenerate case.

### Limitation: no true preemption (unchanged)

Both triggers act at **turn boundaries**, not mid-turn. The `Stop` hook fires only when the model finishes; injection is safe only at idle. A message arriving while the recipient is mid-turn waits in the mailbox until the current turn ends. So an "urgent" message is delivered at the *next turn boundary* — it cannot preempt work already in progress. This is inherent and the spec accepts deferred-until-turn-boundary delivery as the contract. (The continue-in-turn mode narrows the window to "end of current turn," which is the best achievable.)

### Optional complement: agent-side `/loop`

For long-running autonomous agents that keep working across many turns, an opt-in `/loop` calling `/inbox` per iteration drains mail between steps. A complement to push, never the default — Triggers A/B handle the common case.

---

## 7. Transport & architecture

The MCP server runs **in-process with claudectl and starts automatically on launch** (§1) — there is no separate start command. claudectl is one process exposing two frontends over a shared core: the TUI and the bus MCP server. The server listens on a local transport (Unix socket / stdio bridge as appropriate); agents connect to it via the MCP registration that `claudectl init` installs (§8).

Lifecycle:

- `claudectl` (normal) → TUI **and** bus server both up.
- `claudectl --bus-only` → headless server, no TUI (daemon / CI / long-running supervisor host).
- `claudectl --no-bus` → TUI only, bus disabled (pure monitoring, the legacy behavior).

Modules:

- **`bus.rs`** (new): tool handlers, subject routing, claim protocol, mailbox state. Started by the main app init, not a subcommand.
- **`policy.rs`** (new): content + flow validation (§10).
- **`supervisor.rs`** (new): durable role directory, team-burst orchestration, dead-lead restart (§13).
- **`discovery.rs`** / **`monitor.rs`** (existing): roster, status, `Waiting`-edge detection. Reused.
- **`terminals/`** (existing): Trigger-B injection fallback. Reused.
- **`orchestrator.rs`** (existing): optionally grows to host the `workflow.toml` centralized mode alongside the bus.

A single shared in-process server serves all sessions (one shared mailbox store), rather than one server per session. Because the server's lifetime is claudectl's lifetime, the durable state (SQLite, §4) — not the process — is what guarantees messages and role addresses survive across claudectl restarts.

---

## 8. Agent-side provisioning & lifecycle

For the bus to work, three things must exist in each participating agent's Claude Code config: the **MCP server registration**, the **`Stop` hook** (Trigger A — now the primary path), and the **`/inbox` slash command** (Trigger B fallback + optional `/loop` use). Today claudectl provisions none of this — every user would hand-wire three things per worktree and get them subtly wrong. claudectl MUST own the full lifecycle of these artifacts: install, update, verify, and uninstall.

### Provisioning lives inside `claudectl init`, not a separate command

There is **no `claudectl setup` verb.** The single canonical entry point for getting an environment ready is [**`claudectl init`**](https://github.com/mercurialsolo/claudectl/issues/257) — the same opinionated 60-second onboarding flow that handles the budget cap, brain auto-pilot detection, supervisor plugin install, and curated skill suggestions. Bus-readiness is one of its phases, not a parallel competing wizard. This matters because the worst onboarding bug we could ship is two separate "first-run" wizards a new user has to discover (the [dashingsauce r/ClaudeCode comment](https://github.com/mercurialsolo/claudectl/issues/257) names that pattern explicitly: *"you end up connecting the same thing five times in five places"*).

`claudectl init` (no args) launches the interactive path; `--non-interactive` runs the same flow with prompted defaults for CI / dotfile automation. The bus-specific phases of that flow:

1. **Discover worktrees / sessions** — scan for git worktrees and running sessions, propose roles (defaulting role name from directory/branch).
2. **Assign roles** — confirm or edit the role for each, resolving the §5 shared-cwd ambiguity by prompting for explicit `--role` where needed.
3. **Detect hook capability & choose trigger per role** — probe whether `Stop` hooks are installable (not blocked by `allowManagedHooksOnly`); select Trigger A (`Stop` hook → MCP tool, default) where possible, else Trigger B (`Waiting`-edge injection) (§6).
4. **Install the claudectl-bus plugin (or fallback writes)** — MCP server registration, `Stop` hook, `/inbox` command.
5. **Subscriptions & policy** — optionally seed subject subscriptions (§3) and confirm the `[bus.policy]` defaults (§10).

Non-interactive equivalent for a single role (the path that ships today):

```
claudectl bus role bind implementer ~/proj-wt-impl
```

Future shape, once `init` lands:

```
claudectl init --non-interactive \
  --bus-role implementer --bus-cwd ~/proj-wt-impl \
  --bus-trigger stop-hook --bus-subscribe "task.*"
```

### Package as a Claude Code plugin (preferred mechanism)

Claude Code plugins bundle hooks (`hooks/hooks.json`), slash commands, and MCP server config in a single installable unit. claudectl ships **one "claudectl-bus" plugin** carrying all three artifacts together, rather than editing `.claude/settings.json`, the commands dir, and the MCP config as three separate writes. Benefits: atomic install/uninstall, a single version to migrate, and the plugin's `${CLAUDE_PLUGIN_ROOT}` / `${CLAUDE_PLUGIN_DATA}` paths give claudectl a clean home for the hook scripts and any persistent state. `claudectl init` enables/configures this plugin; uninstall removes the plugin cleanly with no residue in user config.

### Fallback: direct settings write

Where plugins are unavailable or an enterprise `allowManagedHooksOnly` policy blocks user/project/plugin hooks, `init` falls back to writing fenced entries into `.claude/settings.json` (or detecting that hooks are blocked entirely, in which case it provisions only the MCP server + `/inbox` command and selects **Trigger B** for that role).

### Managed lifecycle (claudectl owns these, not the user)

claudectl tracks every artifact it installs in its own state (`~/.claudectl/`) and manages the full lifecycle:

- **install** — enable the plugin (or write fenced settings) idempotently; never clobber unmanaged user content (detect & merge, or prompt).
- **verify / status** — `claudectl init --check` reports whether each artifact is present, current-version, drifted, or missing. Drift = artifact edited out-of-band or stale after a claudectl upgrade.
- **update / migrate** — on claudectl upgrade, re-render artifacts to the new version; `init` offers to migrate detected stale installs.
- **uninstall** — `claudectl init --remove [--role X]` cleanly removes only claudectl-managed artifacts, leaving unmanaged config intact.

`init --reset` re-runs the full flow from scratch and is the recommended recovery path when an onboarding gets wedged.

### Ownership & idempotency rules

- Managed artifacts are **tagged/fenced** (e.g. a `# >>> claudectl managed >>>` marker block, or a dedicated managed file the user's config imports) so claudectl can update or remove exactly its own content without touching hand-written config.
- All operations are **idempotent** — re-running `init` converges to the desired state, never duplicates.
- claudectl **never silently overwrites** unmanaged content; conflicts surface in the interactive flow or fail loudly in `--non-interactive` mode.
- A marker file `~/.claudectl/onboarding.json` records the last completed init version so the flow doesn't re-prompt unnecessarily.

This makes "is my environment bus-ready and current?" a single `claudectl init --check`, and makes onboarding a new worktree a single `init` pass rather than three manual edits.

---

## 9. Slash commands: risk and capability

Claude Code sessions interpret slash commands (`/loop`, custom commands). This cuts both ways and shapes two requirements.

### Risk — command injection at the delivery boundary

If a delivered message body begins with `/`, the receiving session executes it as a **command** rather than reading it as content. A message could thereby smuggle `/loop` or a destructive custom command into a peer (accidentally, or via a poisoned upstream). 

**Requirement (non-optional):** the bus MUST neutralize command semantics in message bodies at the injection boundary — escape/wrap a leading `/`, or reject the message. A *message* must never be able to become a *command* in the recipient. Sanitization lives at the injection point, since that is exactly where the message channel meets the command channel.

### Capability — slash commands as the clean delivery nudge

Conversely, a custom slash command is a *better* nudge than raw text. Define `/inbox` (or similar) in each agent's config; claudectl's nudge is simply "run `/inbox`." The agent pulls its mailbox through a path it explicitly controls, sidestepping barge-in — a slash command is unambiguous and self-contained in a way a pasted paragraph is not.

### Resolution

Commands flow **one direction only**: claudectl → session, as the controlled wake signal (`/inbox`). Message bodies flowing the other direction are stripped of all command syntax. This separates the control channel from the content channel cleanly.

---

## 10. Guardrails

claudectl is the single process every message passes through, making it the correct, unbypassable enforcement point. Guardrails are **policy, not hardcoded** — a `[bus.policy]` config block with strict defaults, loosened opt-in.

### Content validation

- **Type allowlist.** Every message declares a `type` (`task`, `result`, `question`, `status`, `handoff`). Untyped/malformed messages are rejected. Prevents agents dumping whole reasoning traces into a send.
- **Command sanitization.** Leading-`/` and known control sequences stripped/escaped/rejected at the injection boundary (§9). Non-optional.
- **Size limits.** Max body length per message.

### Flow control

- **Rate limits.** Max messages per thread and per role per minute. Stops a looping agent flooding a peer before hop caps trigger.
- **Hop cap.** Per-thread maximum number of hops; thread halts when exceeded.
- **Stop sentinel.** A configured sentinel string in a body halts routing for that thread.
- **Loop / echo detection.** Hash recent bodies per thread; near-identical repeated sends (semantic ping-pong) are flagged and halted early — catching the failure mode hop-counts catch only late.
- **Cost ceiling.** Wire existing per-session cost/budget tracking into "stop delivering on budget exhaustion."
- **Global kill-switch.** Operator can halt the entire bus.

### Authorization

- **Recipient / subject ACLs.** Per-role rules: which roles may publish/subscribe to which subjects, who may broadcast, who may direct-send to whom. Makes fan-out topologies safe. Optional but recommended for >2 agents.

### Example policy

```toml
[bus.policy]
allowed_types     = ["task", "result", "question", "status", "handoff"]
max_body_bytes    = 8192
max_msgs_per_min  = 30
max_hops          = 20
stop_sentinel     = "<<<HALT>>>"
sanitize_commands = true        # non-optional in effect; strips leading "/"
claim_timeout_s   = 120

[bus.policy.acl]
planner     = { publish = ["task.*", "review.requested"], subscribe = ["result.*", "status.*"] }
implementer = { publish = ["task.completed", "status.update"], subscribe = ["task.*"] }
reviewer    = { publish = ["review.completed"], subscribe = ["review.requested"] }
fallback    = "human"           # unclaimed tasks escalate here
```

---

## 11. Open questions

- **Mailbox persistence format** — SQLite (WAL, concurrent-safe) is the current lean. Confirm no objection to the dependency.
- **Shared cwd** — `whoami` cwd-inference must require explicit `--role` when multiple sessions share a worktree.
- **Single vs. per-session server** — single shared server assumed (shared mailbox); confirm.
- **Claim escalation target** — human via TUI vs. a designated `fallback` role vs. priority re-publish. Possibly configurable per subject.
- **Subject taxonomy** — ship a default set, or leave entirely to user config?
- **Managed-artifact fencing mechanism** — claudectl-bus plugin (preferred, atomic) vs. fenced marker-block in `.claude/settings.json` (fallback). Plugin assumed primary; confirm.
- **`Stop` hook availability** — *resolved:* `Stop` hooks exist, support `mcp_tool` handlers, and can continue-in-turn. Remaining risk is only the enterprise `allowManagedHooksOnly` case, handled by the Trigger B fallback (§6, §8).
- **Native agent-teams integration** — *resolved (see §1, §13):* agent teams are a bounded-burst execution unit, not a long-lived coordinator. claudectl does **not** reinvent intra-team primitives; it is the durable persistence/supervision layer *over* ephemeral teams. Remaining sub-question: should the supervisor spawn/drive native teams programmatically (via the Agent SDK env flag) or only observe and restart them?
- **Continue-in-turn vs. notify-and-idle default** — should Trigger A default to blocking the `Stop` to deliver in-turn (lower latency, but interacts with the 8-block cap) or to notify-and-idle (simpler, one extra turn)? Possibly per-role policy.
- **Supervisor restart fidelity** — native teams have no session resumption for in-process teammates; on a dead-lead restart the supervisor must re-spawn rather than resume. Confirm the durable task/message state in SQLite is sufficient to reconstruct a team's working context on respawn, or define what's lost.

---

## 12. Suggested build order

1. **Roles + `whoami` + `list_agents`** — durable names and a read-only directory. Lowest risk, immediately useful.
2. **In-process bus MCP server, autostarted with claudectl** — server skeleton over existing discovery; up by default on launch, `--no-bus` / `--bus-only` flags. No separate start command.
3. **Provisioning via `claudectl init` (§8, tracked in #257)** — bus-readiness phases (role assignment + managed install of MCP registration and `/inbox` command, with `--check`/`--remove`) fold into the umbrella `claudectl init` flow rather than a separate `setup` verb. Needed early: every later step assumes agents are wired up, and manual wiring is the main onboarding friction.
4. **Mailbox (SQLite) + `publish` / `read_inbox` (directed `addressed_to` only)** — point-to-point messaging working end to end.
5. **Delivery: `Stop` hook → `read_inbox` MCP tool (Trigger A), `/inbox` injection fallback (Trigger B)** — closes the loop; in-turn delivery, barge-in-safe by construction.
6. **Command sanitization + content validation** — guardrails before opening up routing.
7. **Subjects + `subscribe` + claim protocol** — full pub/sub.
8. **Flow guards (rate/hop/loop/cost) + ACLs.**
9. **Managed lifecycle: update/migrate on upgrade, drift detection** — hardens §8 once artifacts are in real use.
10. **Supervisor + long-horizon persistence (§13)** — durable role directory across team teardown, dead-lead restart, repeated team-burst orchestration. The differentiated capability; build once the messaging substrate is solid.
11. **TUI bus view** — live roster + message-flow graph + team-burst lifecycle; the human's window into a bus the agents drive themselves.

---

## 13. Longevity: persistence across team teardown (the differentiated layer)

The target use case — agents collaborating continuously over a project that runs for weeks, months, or longer — is **not** served by native agent teams, which are a bounded-burst tool. The verified limitations that make teams unfit for long-horizon operation, and what claudectl supplies instead:

| Agent-teams limitation (native) | Consequence for long-running work | claudectl supervisor provides |
| --- | --- | --- |
| No session resumption for in-process teammates; lead may message dead teammates after resume | A team cannot survive a restart | Durable role directory + mailbox in SQLite; on restart, re-spawn teammates and reattach by **role**, not session ID |
| Lead is fixed for the team's lifetime; no leadership transfer | Single point of failure over months | Supervisor restarts a dead lead and re-creates the team from persisted task/role state |
| Cleanup tears the team down on task completion | Lifecycle is create→synthesize→destroy, not continuous | Bursts are *episodes*; the durable layer persists between them and launches the next when work arrives |
| One team at a time, no nested teams | Can't run many concurrent long-lived workstreams | Bus coordinates *across* multiple sequential/parallel team episodes and standalone agents |
| Linear ~5x token cost per active teammate | Continuous always-on teams are economically implausible | Teams are spun up only for bursts of genuine parallel work, then torn down; the persistent layer between bursts is cheap (no idle teammates burning tokens) |

### The model: episodic bursts under a durable supervisor

The right architecture for months-to-years collaboration is **not** an always-on swarm (too costly, and teams can't persist anyway). It is:

1. A **durable supervisor** (claudectl `--bus-only` or the running TUI) holds the long-lived state: the role directory, the persistent mailboxes, the project-level task backlog, subscriptions, and policy. This survives indefinitely because it is process-independent SQLite state, not a set of live sessions.
2. When a unit of work warrants parallelism, the supervisor **spins up a native agent team as an episode** — a bounded burst — optionally seeding it from the persisted backlog.
3. The team does its parallel work using its *own* native task list and mailbox (the bus does not interfere intra-team, §2).
4. On completion the team **tears down normally**; the supervisor harvests results into durable state and the role addresses persist for the next episode.
5. Between episodes, standalone long-lived agents (not in any team) continue to coordinate over the bus, and the supervisor **restarts anything that dies**.

This gives the months/years horizon as a *supervised sequence of cheap-to-persist, expensive-only-when-active episodes*, rather than an impossible continuous team. Native teams are the muscle; claudectl is the skeleton and memory that outlive any single contraction.

### What must be durable (survives process death, claudectl restart, team teardown)

- Role directory: role → cwd-selector binding, last-known session, subscriptions.
- Mailboxes: undelivered and threaded messages, keyed by role.
- Project task backlog: work not yet dispatched to an episode, plus harvested results.
- Policy and ACLs.

### What is explicitly ephemeral (re-derived, never relied upon long-term)

- Live session IDs, PIDs, tmux pane IDs (re-discovered each launch).
- A native team's in-flight intra-team task list (owned by the team; harvested at teardown, not mirrored live).
- Any agent's context window (teams don't resume; the supervisor reconstructs working context from durable state on respawn — see open question on restart fidelity).

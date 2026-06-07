# Agent Bus

The agent bus is claudectl's coordination layer for multiple Claude Code sessions running on the same machine — and, with the [relay feature](relay.md), across machines too. Sessions discover each other through a **persistent directory** of roles and exchange work through a **persistent mailbox**. Mail survives session restarts; roles outlive the processes they're bound to.

This page covers how to turn it on, bind roles, send and receive messages, and uninstall. For the design rationale (why pub/sub, how the claim protocol works, the scope boundary with native agent teams), see the [Agent Bus Design Spec](AGENT_BUS.md).

## When to use it

- **Multiple specialist sessions on one project** — `frontend`, `backend`, `infra` Claude sessions in separate worktrees; each picks up work in its lane.
- **Plan / impl / review** — a planner session decomposes a task and addresses sub-work to implementer + reviewer roles.
- **Long-running supervisor** — a `supervisor` role drains an inbox of incidents reported by other sessions and triages.
- **Cross-machine handoff** — pair the bus with the [relay feature](relay.md) to push messages across hosts.

If you only ever run one Claude session at a time, the bus is overhead. It pays off the moment you have two cooperating sessions.

## Quick start

Three steps: build with the feature, register the MCP server with Claude Code, bind roles.

### 1. Build with the `bus` feature

The bus stack (rmcp + Tokio runtime) is opt-in to keep the default binary small. Pick the install path you want:

```bash
cargo install claudectl --features bus
# or
brew install mercurialsolo/tap/claudectl  # Homebrew bottle ships with default features —
                                          # bus support requires building from source until
                                          # the bottled build flips on
```

Verify:

```bash
claudectl bus --help                       # should show subcommands: stdio, role, send, inbox, whoami, stop-hook
```

### 2. Register the bus as an MCP server

The Claude Code plugin in `claude-plugin/` already includes the wiring:

```json
// claude-plugin/.mcp.json
{
  "mcpServers": {
    "claudectl-bus": {
      "command": "claudectl",
      "args": ["bus", "stdio"],
      "env": { "CLAUDECTL_BUS_ROLE": "${CLAUDECTL_BUS_ROLE}" }
    }
  }
}
```

If you're using the bundled plugin (recommended), running `claudectl init` will install it. Otherwise, point Claude Code at any `.mcp.json` containing the block above. The MCP server runs on stdio — Claude Code spawns one per session it talks to.

### 3. Bind a role

A role is a stable name other sessions address you by. Four ways to create the binding — pick the one that fits your context:

| Where you are | Command | What it binds |
|---|---|---|
| Outside any session (CI, scripts) | `claudectl bus role bind <name> <cwd>` | cwd-keyed |
| Outside any session, known pid | `claudectl bus role bind <name> <cwd> --pid <pid>` | cwd + pid pinned |
| TUI dashboard, session selected | `Ctrl+R` → type role name → `Enter` | selected session's pid + cwd |
| Inside a running Claude session | `/bind <role>` slash command | walks ancestor chain to find Claude's pid + uses current cwd |

PID-keyed bindings beat cwd-keyed ones during resolution — the disambiguator for "two sessions in the same worktree, different roles."

If you can't pick a role name, run `claudectl bus role suggest --pid <pid>` (or omit `--pid` from inside a Claude session) and the suggester scans the transcript + cwd for hints — explicit "you are the X" mentions, tool fan-out shape (writes-heavy → `impl`, reads-heavy → `reviewer`), path patterns the session touches.

## Day-to-day usage

### Inspect the directory

```bash
claudectl bus role list                    # all bound roles, their cwd, pid, last-seen
claudectl bus whoami                       # which role this cwd resolves to
claudectl bus whoami --json                # machine-readable form (used by the Stop hook)
```

### Send a directed message

From the CLI (debugging or scripting):

```bash
claudectl bus send <to-role> "<body>" \
  --subject task.created \
  --msg-type task \
  --from <your-role> \
  --priority normal
```

From inside a Claude session, just use natural language with the recipient role — the Claude side calls the bus's `publish` MCP tool and the message lands on disk before the turn ends. A leading `/` in any body is neutralized at the boundary so a queued message cannot smuggle a slash command into the recipient.

### Drain the inbox

The recipient picks up mail in two ways:

**Automatic (recommended).** The Stop hook installed by `claudectl init` drains the mailbox at the end of every Claude turn. When mail is present, the hook returns `decision: "block"` with the rendered messages as `additionalContext` so the agent picks the work up in the same turn — no user interaction, no polling. This is **Trigger A** in the [design spec](AGENT_BUS.md#trigger-a-claude-code-stop-hook-claudectl-mcp-tool-primary).

**Manual.** Use the `/inbox` slash command (provided by the bundled plugin) any time, or:

```bash
claudectl bus inbox                        # drains the cwd-inferred role
claudectl bus inbox --role <name>          # drain a specific role
claudectl bus inbox --json                 # machine-readable form
```

Messages are drained on read — once delivered, they're marked acked and won't appear again.

## Where state lives

| Path | What |
|---|---|
| `~/.claudectl/bus/bus.db` | SQLite (WAL) — roles, subscriptions, messages, status |
| `~/.claude/settings.json` | Claude Code hook config (the Stop hook lives here after `init`) |
| `~/.claudectl/onboarding.json` | What `init` provisioned, when, against which version |

WAL mode lets the TUI process and every `claudectl bus stdio` subprocess read/write concurrently without locking each other out.

### Inspecting state

```bash
sqlite3 ~/.claudectl/bus/bus.db ".tables"
sqlite3 ~/.claudectl/bus/bus.db "SELECT role, pid, last_seen FROM roles"
sqlite3 ~/.claudectl/bus/bus.db "SELECT subject, addressed_to, status FROM messages ORDER BY created_at DESC LIMIT 20"
```

## Worked example: planner → implementer handoff

Goal: a `planner` session decomposes a task, then addresses the implementation work to an `impl` session.

```bash
# Terminal 1 (cwd: /work/proj-plan)
claude                                     # start a Claude session
# inside the session, once at a prompt:
/bind planner                              # binds this session as `planner`

# Terminal 2 (cwd: /work/proj-impl)
claude
/bind impl

# Now from the planner session, send work to impl:
# inside the planner session:
> Use the claudectl-bus MCP tool to send `impl` a task that says
> "implement the date parsing for the report tool".

# At the impl session's next Stop boundary, the Stop hook drains the
# mailbox and the impl agent sees the task as additionalContext in the
# same turn — no user intervention needed.

# Verify from a third terminal:
claudectl bus role list
# planner   /work/proj-plan   pid=12345
# impl      /work/proj-impl   pid=23456

claudectl bus inbox --role impl --json     # peek without re-draining
```

## Uninstall

The bus DB and role table are user state — `claudectl init --remove` deliberately leaves them alone so re-running `init` reconnects to your existing roles. To wipe everything (DB, roles, mailbox), use:

```bash
claudectl init --remove                    # soft: removes hooks + onboarding marker, keeps state
claudectl init --purge --yes               # hard: above + nukes ~/.claudectl/ + config file
```

See [Quick Start § Uninstall](quickstart.md#uninstall) for the full lifecycle commands.

## What's implemented today

| Phase | Status |
|---|---|
| Roles + `whoami` + `list_agents` | Shipped |
| MCP server (stdio subprocess) | Shipped |
| Provisioning via `claudectl init` | Shipped |
| Mailbox + directed `publish` / `read_inbox` | Shipped |
| `Stop` hook continue-in-turn delivery | Shipped |
| Content sanitization (`/` neutralized, body cap, subject grammar, type allowlist) | Shipped |
| PID-keyed role bindings + ancestor-walk resolution | Shipped (0.55.0) |
| TUI Ctrl+R + `/bind` slash command + `bus role suggest` | Shipped (0.55.0) |
| Subjects + `subscribe` + claim protocol | Not started |
| Flow guards (rate/hop/loop/cost) + ACLs | Not started |
| Long-horizon supervisor | Not started |
| TUI bus panel | Not started |

For the full design and roadmap including unshipped phases, see the [Agent Bus Design Spec](AGENT_BUS.md).

## See also

- [Quick Start](quickstart.md) — install + `claudectl init`
- [Relay & Hive Mind](relay.md) — extend the bus across machines
- [Reference § Setup](reference.md#setup) — every `init` subcommand flag
- [Configuration](configuration.md) — TOML config, hooks, rules

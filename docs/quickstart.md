# Quick Start

Get claudectl running in under two minutes.

## 1. Install

```bash
brew install mercurialsolo/tap/claudectl     # Homebrew — ships with bus/coord/relay/hive built in
# or
cargo install claudectl                                          # Cargo — default features only (hive)
cargo install claudectl --features bus,coord,relay,hive          # Cargo with all features
```

Verify it works:

```bash
claudectl --version
```

## 2. Onboard with `claudectl init`

```bash
claudectl init
```

The wizard walks five phases — weekly budget cap, local-LLM brain auto-detection (probes ollama / llama.cpp / LM Studio / vLLM), Claude Code hook install, agent-bus role binding, and curated skill suggestions. Each phase is skippable (`s` at the prompt) and the result is recorded at `~/.claudectl/onboarding.json` so later runs of `claudectl init --check` can show drift.

For dotfile automation:

```bash
claudectl init --non-interactive --budget 25 --skip-brain --skip-bus --skip-skills
```

If you only want the hook install (the previous `--init` flag), that's the **Plugin** phase — accept it and skip the others.

Your existing Claude Code settings are preserved; the hook install only adds claudectl entries.

**What gets added:** the Plugin phase writes hooks in two places — into `~/.claude/settings.json` (the dashboard-observability hooks) and into the embedded plugin at `~/.claude/plugins/claudectl/hooks/hooks.json` (the bus + brain plugin hooks). Both sets coexist; Claude Code merges them.

**`~/.claude/settings.json` (dashboard observability):**

| Hook | Matcher | Command | What it does |
|------|---------|---------|--------------|
| `PreToolUse` | `Bash` | `claudectl --json 2>/dev/null \|\| true` | Lets claudectl see Bash commands before they run |
| `PostToolUse` | `*` | `claudectl --json 2>/dev/null \|\| true` | Notifies claudectl after every tool completion |
| `Stop` | (all) | `claudectl --json 2>/dev/null \|\| true` | Notifies claudectl when a turn ends |

These are fire-and-forget snapshot reads. `|| true` keeps Claude Code unblockable if claudectl isn't installed or fails.

**`~/.claude/plugins/claudectl/hooks/hooks.json` (bus + brain plugin):**

| Hook | Matcher | Script | What it does |
|------|---------|--------|--------------|
| `PreToolUse` | `Bash\|Write\|Edit\|NotebookEdit` | `brain-gate.sh` | Queries the local LLM for approve/deny on potentially destructive tool calls |
| `PostToolUse` | `Bash\|Write\|Edit\|NotebookEdit` | `outcome-record.sh` | Records the outcome so the brain learns from your corrections |
| `SessionStart` | (all) | `session-briefing.sh` | Surfaces queued mail and recent context at session start |
| `Stop` | (all) | `inbox-drain.sh` | Drains the agent's bus mailbox; can return `decision:"block"` with `additionalContext` to deliver mail in the same turn (Trigger A in [docs/AGENT_BUS.md](AGENT_BUS.md#6-notification--delivery-handshake)) |

Both sets are removed cleanly by `claudectl init --remove`.

## 3. Verify the install

```bash
claudectl doctor
```

This runs a top-down checklist: PATH, hooks, plugin files, brain endpoint, bus feature, bus DB, session discovery, terminal integration. Green means you're ready. If anything fails, the doctor names the exact command to fix it.

## 4. Start the dashboard

Open one or more Claude Code sessions in separate terminals, then:

```bash
claudectl
```

You'll see every session in a live table with status, cost, context usage, burn rate, and more. (Forgot step 2 + 3? On first run you'll see a banner pointing you back to `claudectl init`.)

## 5. Try demo mode (no Claude Code needed)

```bash
claudectl --demo
```

Runs with fake sessions so you can explore the dashboard, keybindings, and features without any live sessions. Press `R` on any session to record a highlight reel — demo mode drip-feeds a scripted coding session (reading files, writing code, fixing errors, running tests) so you can see the session recorder in action.

## Key actions from the dashboard

| Key | Action |
|-----|--------|
| `j`/`k` | Navigate sessions |
| `Enter` | Expand session detail |
| `Tab` | Jump to session's terminal |
| `y` | Approve a blocked prompt |
| `i` | Send input to a session |
| `n` | Launch a new session |
| `?` | Show all keybindings |

## Optional: submit a task to the supervisor

The supervisor turns the durable coord ledger into a task runner: submit work, declare verifiers, let the reconciler hand it to a role's mailbox (or spawn a fresh session). It survives daemon restarts.

```bash
# Inline submission — useful for one-shot scripts
claudectl supervisor submit \
  --name "rename-utils" \
  --cwd "$PWD" \
  --prompt "Rename utils.rs → helpers.rs and update every import" \
  --role backend

# Inspect what's running
claudectl supervisor status            # compact table
claudectl supervisor logs <task_id>    # transitions + verifier history
```

Batch from a `tasks.toml` file (RFC §4 shape) with `claudectl supervisor run tasks.toml --dry-run` to preview, then without `--dry-run` to commit. See the [README's Supervisor section](../README.md#supervisor) for the verifier syntax (`run` / `brain` / `agent`) and the full design overview.

`claudectl supervisor drain` halts new assignments without killing running tasks; the `supervisor drain` row in `claudectl doctor` surfaces the state.

## Optional: project-scoped hooks

If you only want claudectl hooks in specific projects (not globally), the `--init` legacy flag still works for hook-only installs:

```bash
claudectl --init -s project
```

This writes to `.claude/settings.local.json` (gitignored) instead of the global file. The `-s project` flag matches Claude Code's own `--scope` convention. `--init` is otherwise deprecated — prefer `claudectl init` for new setups.

## Optional: add the local LLM brain

The brain auto-approves safe operations and blocks dangerous ones using a local model:

```bash
ollama pull gemma4:e4b && ollama serve       # One-time setup
claudectl --brain                            # Start with brain enabled
```

### Toggle the brain mid-session

```bash
claudectl --mode off                         # Pause brain (manual approvals)
claudectl --mode on                          # Resume brain (default)
claudectl --mode auto                        # Brain handles everything
claudectl --mode status                      # Show current mode
```

If you use the Claude Code plugin, type `/brain off` or `/brain auto` directly in your session.

### Auto-insights

Enable the brain to automatically detect friction patterns and suggest workflow improvements:

```bash
claudectl --brain --insights on            # Enable auto-generation
claudectl --brain --insights               # View current insights
```

## Optional: install the Claude Code plugin

The `claude-plugin/` directory in the claudectl repo is a Claude Code plugin that integrates the brain directly into your sessions, no TUI required:

- `/sessions` — see all active sessions
- `/spend` — cost breakdown
- `/brain on|off|auto` — toggle brain mid-session
- `/auto-insights` — view or configure auto-generated workflow insights
- `/inbox` — drain pending agent-bus messages addressed to this session's role
- `/role <name>` — set this session's agent-bus role, e.g. `/role frontend` or `/role tester` (auto-detects Claude's pid)
- **Automatic brain gate** — the plugin hook queries the brain before every Bash/Write/Edit call

The plugin and `--init` hooks are complementary. Use `--init` for dashboard observability, the plugin for inline brain decisions.

## Upgrading

After `brew upgrade claudectl` (or `cargo install claudectl --force --locked`), the new binary is on disk but the hook entries, plugin files, and DB schema were written by the old binary. Refresh them with:

```bash
claudectl init --upgrade
```

The command walks four steps and reports each: (1) re-write Claude Code hook entries, (2) re-write embedded plugin files from the new binary, (3) run any pending bus / coord DB migrations, (4) bump the onboarding marker's recorded version. It's safe to run any time — files that haven't changed are reported "unchanged."

`claudectl doctor` has a `plugin version` row that flags this scenario: it compares the binary's version against the on-disk `.claude-plugin/plugin.json` and surfaces an advisory with the upgrade command when they differ.

## Uninstall

Roll back the onboarding wizard's installed artifacts:

```bash
claudectl init --remove                      # Soft uninstall: hooks + onboarding marker
claudectl init --purge --yes                 # Hard uninstall: --remove + nuke ~/.claudectl/ + config
```

`--remove` is the safe form — strips Claude Code hooks and the onboarding marker, but preserves user data (bus DB roles, brain decision logs, hive knowledge, relay identity, your budget config line). Use this when you want to stop the integration without losing what claudectl has learned.

`--purge` is the hard reset — `--remove` plus `~/.claudectl/` (all subdirs) plus `~/.config/claudectl/config.toml`. Use this when reinstalling fresh, recovering from corrupted state, or fully uninstalling. Pair with `--yes` to skip the confirmation prompt; without it you'll see a list of paths and have to confirm.

Or remove just the hooks (legacy flag, still supported):

```bash
claudectl --uninstall                        # Remove from user settings
claudectl --uninstall -s project             # Remove from project settings
```

Both `--remove` and `--uninstall` surgically remove only claudectl entries. All other settings and hooks are preserved.

To uninstall the binary:

```bash
brew uninstall claudectl                     # Homebrew
# or
cargo uninstall claudectl                    # Cargo
```

## Next steps

- [Reference](reference.md) -- dashboard features, keybindings, all CLI flags
- [Configuration](configuration.md) -- TOML config, hooks, rules, model pricing
- [Relay & Hive Mind](relay.md) -- hive knowledge is built-in; add `--features relay` for cross-machine networking
- [Terminal Support](terminal-support.md) -- compatibility and setup notes
- [Troubleshooting](troubleshooting.md) -- common issues and FAQ

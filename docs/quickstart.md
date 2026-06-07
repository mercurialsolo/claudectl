# Quick Start

Get claudectl running in under two minutes.

## 1. Install

```bash
brew install mercurialsolo/tap/claudectl     # Homebrew (macOS / Linux)
# or
cargo install claudectl                       # Cargo (any platform)
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

**What gets added:**

| Hook | Matcher | What it does |
|------|---------|--------------|
| `PreToolUse` | `Bash` | Lets claudectl see commands before they run |
| `PostToolUse` | `*` | Notifies claudectl after every tool completion |
| `Stop` | (all) | Notifies claudectl when a session ends |

The hooks call `claudectl --json 2>/dev/null || true` — if claudectl isn't running, Claude Code continues normally.

## 3. Start the dashboard

Open one or more Claude Code sessions in separate terminals, then:

```bash
claudectl
```

You'll see every session in a live table with status, cost, context usage, burn rate, and more.

## 4. Try demo mode (no Claude Code needed)

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
- `/bind <role>` — attach this session to an agent-bus role (auto-detects Claude's pid)
- **Automatic brain gate** — the plugin hook queries the brain before every Bash/Write/Edit call

The plugin and `--init` hooks are complementary. Use `--init` for dashboard observability, the plugin for inline brain decisions.

## Uninstall

Roll back the onboarding wizard's installed artifacts:

```bash
claudectl init --remove                      # Removes hooks + onboarding marker
```

Or remove just the hooks (legacy flag, still supported):

```bash
claudectl --uninstall                        # Remove from user settings
claudectl --uninstall -s project             # Remove from project settings
```

Both surgically remove only claudectl entries. All other settings and hooks are preserved. Phases that own user state (e.g. the bus database, your budget config line) deliberately decline to delete it.

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

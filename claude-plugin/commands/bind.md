---
name: bind
description: Bind this Claude session to an agent-bus role so other sessions can address it
args: "<role-name>"
---

Attach this session to an agent-bus role so peers can send it directed messages with `claudectl bus send <role> …`. The binding is **PID-keyed** — it follows this exact Claude Code process, so re-runs in the same project don't get confused by other sessions sharing the cwd.

## What to do

1. The user supplied a role name as `{{args}}`. If it's empty, ask them what to name the role and stop until they reply.

2. Run:

   ```bash
   claudectl bus role bind --self {{args}}
   ```

   The `--self` flag tells the CLI to walk the ancestor process chain, find this Claude Code process, and bind the role to that pid + the current working directory. No need to look up the pid yourself.

3. On success the command prints `bound role <name> -> <cwd> (pid=<pid>)`. Echo that confirmation back to the user.

4. If the command errors with "could not find a Claude Code process in the ancestor chain", the plugin was invoked outside the normal `claude` execution context. Tell the user to try again from inside a Claude session, or pass an explicit pid via `claudectl bus role bind <name> <cwd> --pid <pid>`.

5. If the user wants to verify the binding, suggest `claudectl bus role list` or `claudectl bus whoami --json`.

## Why pid-binding

Cwd-only bindings get ambiguous when two Claude sessions run in the same folder (e.g. worktrees, or one session in `/repo` and another in `/repo/sub`). The pid binding picks the specific session you ran `/bind` from, so directed messages land where you expect.

See issues #307 (pid binding) and #310 (this slash command).

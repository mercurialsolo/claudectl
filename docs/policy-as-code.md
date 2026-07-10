# Policy as code — team guardrails a dev can't soften

`.claudectl.toml` and `~/.config/claudectl/config.toml` are *personal* config: a
developer can add, edit, or delete any rule in them. That's the right model for
preferences, but it's the wrong model for a guardrail a team depends on — a dev
can delete a deny rule and never force-push protection is gone, silently.

A **team policy** is a `.claudectl/policy.toml` file **committed to the repo**.
Because it comes from version control rather than a developer's home directory,
and the brain gate evaluates it at the highest precedence, its denies can't be
removed by editing local config. You change the guardrails by changing the
checked-in file — a reviewed commit, not a silent local edit.

## Quick start

```bash
claudectl --policy init      # scaffold .claudectl/policy.toml
$EDITOR .claudectl/policy.toml
git add .claudectl/policy.toml && git commit -m "Add team guardrails"
```

Every developer who pulls the repo now inherits the guardrails — no per-machine
setup. Verify what's active:

```bash
claudectl --policy           # show the guardrails and where they're loaded from
claudectl doctor             # includes a "team policy" row
```

## The file

```toml
# .claudectl/policy.toml
[deny]
# Commands blocked anywhere in this repo (case-insensitive substring match).
commands = [
  "git push --force",
  "git push -f",
  "kubectl delete",
]
# Tools blocked entirely (exact, case-insensitive).
tools = ["WebFetch"]
```

- **`commands`** — substring match against the tool input, the same semantics as
  auto-rules. `"git push --force"` blocks `git push --force origin main`.
- **`tools`** — an exact (case-insensitive) tool name. Use it to forbid a whole
  capability in a sensitive repo.

Unknown sections and keys are ignored, so a newer policy file stays readable by
an older binary (and vice versa).

## How enforcement works

On every tool call, the Claude Code plugin gate (`claudectl --brain-query`):

1. Discovers the policy by walking up from the working directory to the repo
   root (so it works from any subdirectory).
2. Evaluates the policy's denies **first**, before user deny rules, before the
   brain, before the zero-LLM heuristic.
3. If a policy deny matches, the call is blocked with `source: "policy"` and the
   guardrail's reason — and nothing downstream can approve it.

```
tool call ─▶ TEAM POLICY ─▶ user deny rules ─▶ user approve rules ─▶ brain / heuristic
              (checked-in,        (local config — can't override a policy deny above)
               highest precedence)
```

Because policy denies are sourced from the repo and matched at the top of the
chain, a local `.claudectl.toml` edit can add rules but can't remove or shadow a
policy guardrail.

## What it does not cover (yet)

Slice 1 is forbidden commands and tools. On the roadmap under their own
`policy.toml` sections:

- `[limits]` — spend caps that floor the user's own caps (`min(user, policy)`).
- `[require] verifiers` — verifier kinds every supervisor task must include.
- `[brain] min_lite_mode` — a floor on the zero-LLM brain-lite policy.
- Enforcement even when the brain gate is turned off (today a policy applies
  while the gate is active; a developer who disables the gate entirely opts out
  of claudectl governance — a separate always-on hook is the planned fix).

## Notes

- The policy is per-repo. A machine working across several repos enforces each
  repo's own `.claudectl/policy.toml`.
- Denied calls are recorded as `policy_deny` observations in the local decision
  log, so a lead can audit what the guardrails caught without the block counting
  toward the brain's own accuracy.

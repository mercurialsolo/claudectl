---
name: Session Monitoring
description: Provides awareness of claudectl session state, health checks, and cost tracking. Activated when the user asks about session health, spending, brain decisions, or multi-session coordination.
version: 0.48.0
---

# claudectl Session Monitoring

You have access to claudectl, a local session supervisor for Claude Code. Use it when the user asks about:
- How many sessions are running, their status, or what they're doing
- Session costs, burn rates, or budget tracking
- Session health (cognitive rot, loops, stalls, context saturation)
- Brain decisions, learning progress, or accuracy
- File conflicts between sessions
- Multi-session orchestration

## Available CLI Flags

```bash
# Setup
claudectl --init                   # Wire up Claude Code hooks (one-time)
claudectl --init -s project        # Wire up hooks for this project only
claudectl --uninstall              # Remove claudectl hooks

# Session status
claudectl --list                   # Human-readable session list
claudectl --json                   # Machine-readable JSON with full data
claudectl --watch                  # Stream status changes

# Cost and history
claudectl --stats --since 24h     # Aggregated statistics
claudectl --history --since 24h   # Completed session history
claudectl --summary --since 8h    # Activity summary

# Brain
claudectl --mode status     # Show current brain gate mode
claudectl --mode on         # Brain evaluates tool calls (default)
claudectl --mode off        # Disable brain gate
claudectl --mode auto       # Brain auto-approves above threshold
claudectl --brain-stats accuracy  # Per-tool accuracy breakdown
claudectl --brain-stats learning-curve  # Correction rate over time
claudectl --brain-stats baseline  # Brain vs static rules comparison
claudectl --brain-stats false-approve   # Safety: false positive rate

# Diagnostics
claudectl --doctor                # Check terminal integration
claudectl --brain-prompts         # List prompt template sources
```

## Health Check Icons

When reporting session health from JSON output, these are the severity indicators:
- Critical: cognitive rot (score > 70), context > 90%, error loops
- Warning: context > 80%, cost spikes, stalls, early decay
- Info: proactive compaction suggestion, low cache

## Key Fields in JSON Output

Each session in `--json` output includes:
- `status`: NeedsInput, Processing, Waiting, Idle, Finished
- `cost_usd`: total session cost
- `burn_rate_per_hr`: current spend rate
- `context_tokens` / `context_max`: context window usage
- `decay_score`: 0-100 cognitive health score
- `has_file_conflict`: boolean for cross-session conflicts
- `pending_tool_name` / `pending_tool_input`: what's waiting for approval

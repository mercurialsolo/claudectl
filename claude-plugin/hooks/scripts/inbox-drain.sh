#!/usr/bin/env bash
# Stop hook for the claudectl agent bus (Trigger A, AGENT_BUS.md §6).
#
# Fires after each turn finishes. Delegates entirely to `claudectl bus
# stop-hook`, which:
#   - drains the caller's mailbox via cwd-inferred role resolution,
#   - if mail is present, emits Claude Code's Stop-hook output protocol so the
#     agent picks the work up in the same turn (decision: "block" +
#     hookSpecificOutput.additionalContext),
#   - is silent + exit 0 in every other case (no role bound, empty inbox, DB
#     missing) so the hook never blocks a session.
#
# This shell wrapper is intentionally thin: it only protects against the case
# where claudectl is not on PATH. All bus logic lives in Rust where it is
# tested. See `src/bus/stop_hook.rs`.
set +e
command -v claudectl >/dev/null 2>&1 || exit 0
exec claudectl bus stop-hook 2>/dev/null

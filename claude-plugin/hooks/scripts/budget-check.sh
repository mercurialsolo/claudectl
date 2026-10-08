#!/usr/bin/env bash
# claudectl budget-check: PreToolUse hook that denies tool calls when THIS
# session's spend exceeds the configured budget.
#
# The matching and the comparison live in `claudectl --budget-check`, because
# pairing a hook process to a session means walking the process tree, which is
# not something this script can do. It used to compare the first `cost_usd` in
# `claudectl --json` — whichever session that happened to be — against the
# budget, so an unrelated expensive session denied everyone.
#
# Exits 0 on every path: a budget check must never block Claude Code.

set -uo pipefail

if ! command -v claudectl &>/dev/null; then
    exit 0
fi

BUDGET="${CLAUDECTL_BUDGET:-}"
if [ -n "$BUDGET" ]; then
    claudectl --budget-check "$PPID" --budget "$BUDGET" 2>/dev/null || true
else
    # No env override: claudectl resolves the budget from its own layered
    # config, so .claudectl.toml and the user config are both honoured.
    claudectl --budget-check "$PPID" 2>/dev/null || true
fi
exit 0

#!/usr/bin/env bash
# claudectl session-briefing: SessionStart hook that injects a per-project
# briefing of accumulated brain knowledge (recent decisions, learned
# preferences, known anti-patterns) into the new session.
#
# Output is sent to Claude Code as additionalContext.
#
# Falls through silently when claudectl isn't installed, the brain has no
# data yet, or the briefing command fails — never blocks session start.

set -euo pipefail

# Bail if claudectl is missing.
if ! command -v claudectl &>/dev/null; then
    exit 0
fi

# Respect brain gate mode — if the user has turned the brain off, don't
# surface a briefing either.
GATE_MODE_FILE="$HOME/.claudectl/brain/gate-mode"
GATE_MODE="on"
if [ -f "$GATE_MODE_FILE" ]; then
    GATE_MODE=$(tr -d '[:space:]' <"$GATE_MODE_FILE" 2>/dev/null || echo "on")
fi
if [ "$GATE_MODE" = "off" ]; then
    exit 0
fi

PROJECT=$(basename "${PWD:-unknown}")

BRIEFING=$(claudectl --brain-briefing --project "$PROJECT" 2>/dev/null || true)

# Skip injection when the briefing is empty / placeholder. Claude Code accepts
# either no output (skip) or an additionalContext JSON envelope.
if [ -z "$BRIEFING" ] || echo "$BRIEFING" | grep -q "No accumulated brain data"; then
    exit 0
fi

# Emit the additionalContext envelope. We escape the briefing for JSON using a
# python-free approach: write to a temp file and let jq handle it if available;
# otherwise fall back to a minimal manual escape.
if command -v jq &>/dev/null; then
    printf '%s' "$BRIEFING" | jq -Rs '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:.}}'
else
    ESCAPED=$(printf '%s' "$BRIEFING" \
        | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e ':a' -e 'N' -e '$!ba' -e 's/\n/\\n/g')
    printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"%s"}}\n' "$ESCAPED"
fi

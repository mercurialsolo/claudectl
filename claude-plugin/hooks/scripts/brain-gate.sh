#!/usr/bin/env bash
# claudectl brain-gate: PreToolUse hook that queries the local LLM brain
# for approve/deny decisions on tool calls.
#
# Receives tool call JSON on stdin from Claude Code:
#   {"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}}
#
# Outputs a decision:
#   {"decision": "approve"} or {"decision": "deny", "reason": "..."}
#
# Falls through silently (no output) when the brain abstains or is unavailable,
# letting Claude Code's normal permission flow handle it.

set -euo pipefail

# Check if claudectl is installed
if ! command -v claudectl &>/dev/null; then
    exit 0  # Not installed — fall through to normal permission flow
fi

# Check brain gate mode (on/off/auto). Default is "on" (file absent = on).
GATE_MODE_FILE="$HOME/.claudectl/brain/gate-mode"
GATE_MODE="on"
if [ -f "$GATE_MODE_FILE" ]; then
    GATE_MODE=$(cat "$GATE_MODE_FILE" 2>/dev/null || echo "on")
    GATE_MODE=$(echo "$GATE_MODE" | tr -d '[:space:]')
fi

if [ "$GATE_MODE" = "off" ]; then
    exit 0  # Brain disabled — fall through
fi

# Read the tool call from stdin
INPUT=$(cat)

TOOL_NAME=$(echo "$INPUT" | sed -n 's/.*"tool_name" *: *"\([^"]*\)".*/\1/p')
if [ -z "$TOOL_NAME" ]; then
    exit 0
fi

# Extract command/input based on tool type
COMMAND=""
case "$TOOL_NAME" in
    Bash)
        COMMAND=$(echo "$INPUT" | sed -n 's/.*"command" *: *"\([^"]*\)".*/\1/p')
        ;;
    Write|Edit|NotebookEdit)
        COMMAND=$(echo "$INPUT" | sed -n 's/.*"file_path" *: *"\([^"]*\)".*/\1/p')
        ;;
    *)
        # For other tools, try to extract a reasonable input summary
        COMMAND=$(echo "$INPUT" | sed -n 's/.*"tool_input" *: *\({[^}]*}\).*/\1/p' | head -c 200)
        ;;
esac

# Resolve project name from cwd
PROJECT=$(basename "${PWD:-unknown}")

# Query the brain (--brain enables it even without config).
# We pipe the full hook payload on stdin so that claudectl can extract a
# structured diff digest (file_path, old/new_string, content, ...) for
# richer prompt context — see #237.
RESULT=$(printf '%s' "$INPUT" | claudectl --brain --brain-query --tool "$TOOL_NAME" --tool-input "$COMMAND" --project "$PROJECT" 2>/dev/null) || exit 0

# Parse the action from the JSON result
ACTION=$(echo "$RESULT" | sed -n 's/.*"action" *: *"\([^"]*\)".*/\1/p')
REASONING=$(echo "$RESULT" | sed -n 's/.*"reasoning" *: *"\([^"]*\)".*/\1/p')
SOURCE=$(echo "$RESULT" | sed -n 's/.*"source" *: *"\([^"]*\)".*/\1/p')
BELOW=$(echo "$RESULT" | sed -n 's/.*"below_threshold" *: *\(true\|false\).*/\1/p')

case "$ACTION" in
    approve)
        # In "on" mode: only auto-approve when confidence is above threshold
        # In "auto" mode: approve all brain approvals regardless of confidence
        if [ "$GATE_MODE" != "auto" ] && [ "$BELOW" = "true" ] && [ "$SOURCE" = "brain" ]; then
            exit 0  # Fall through — let user decide
        fi
        echo '{"decision":"approve"}'
        ;;
    deny)
        # Build reason string
        if [ -n "$REASONING" ]; then
            REASON="claudectl: $REASONING"
        else
            REASON="claudectl brain denied this action"
        fi
        printf '{"decision":"deny","reason":"%s"}\n' "$REASON"
        ;;
    *)
        # abstain, error, or unknown — fall through
        exit 0
        ;;
esac

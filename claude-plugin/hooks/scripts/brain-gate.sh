#!/usr/bin/env bash
# claudectl brain-gate: PreToolUse hook that queries the local LLM brain
# for approve/deny decisions on tool calls.
#
# Receives tool call JSON on stdin from Claude Code:
#   {"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}}
#
# Emits a hook response on stdout. For denies we emit both the legacy
# `{"decision":"deny","reason":...}` field (for older Claude Code) and a
# `hookSpecificOutput` envelope with `continueOnBlock:true` plus the brain's
# reasoning as `systemMessage` / `permissionDecisionReason`, so newer runtimes
# surface *why* the brain blocked into Claude's next turn (#249).
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

# Minimal JSON-string escaper for the no-jq fallback. Handles the chars that
# would otherwise break a JSON literal: backslash, double quote, CR/LF, tab.
json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    printf '%s' "$s"
}

if command -v jq &>/dev/null; then
    TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty')
    case "$TOOL_NAME" in
        Bash) COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty') ;;
        Write|Edit|NotebookEdit)
            COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.file_path // empty') ;;
        *) COMMAND=$(printf '%s' "$INPUT" | jq -c '.tool_input // {}' | head -c 200) ;;
    esac
else
    TOOL_NAME=$(echo "$INPUT" | sed -n 's/.*"tool_name" *: *"\([^"]*\)".*/\1/p')
    COMMAND=""
    case "$TOOL_NAME" in
        Bash)
            COMMAND=$(echo "$INPUT" | sed -n 's/.*"command" *: *"\([^"]*\)".*/\1/p')
            ;;
        Write|Edit|NotebookEdit)
            COMMAND=$(echo "$INPUT" | sed -n 's/.*"file_path" *: *"\([^"]*\)".*/\1/p')
            ;;
        *)
            COMMAND=$(echo "$INPUT" | sed -n 's/.*"tool_input" *: *\({[^}]*}\).*/\1/p' | head -c 200)
            ;;
    esac
fi

if [ -z "$TOOL_NAME" ]; then
    exit 0
fi

# Resolve project name from cwd
PROJECT=$(basename "${PWD:-unknown}")

# Query the brain (--brain enables it even without config).
# We pipe the full hook payload on stdin so that claudectl can extract a
# structured diff digest (file_path, old/new_string, content, ...) for
# richer prompt context — see #237.
RESULT=$(printf '%s' "$INPUT" | claudectl --brain --brain-query --tool "$TOOL_NAME" --tool-input "$COMMAND" --project "$PROJECT" 2>/dev/null) || exit 0

# Parse the action from the JSON result. Prefer jq so multi-line / escaped
# reasoning round-trips correctly; fall back to the existing sed pattern when
# jq is missing.
if command -v jq &>/dev/null; then
    ACTION=$(printf '%s' "$RESULT" | jq -r '.action // empty')
    REASONING=$(printf '%s' "$RESULT" | jq -r '.reasoning // empty')
    SOURCE=$(printf '%s' "$RESULT" | jq -r '.source // empty')
    BELOW=$(printf '%s' "$RESULT" | jq -r '.below_threshold // false')
else
    ACTION=$(echo "$RESULT" | sed -n 's/.*"action" *: *"\([^"]*\)".*/\1/p')
    REASONING=$(echo "$RESULT" | sed -n 's/.*"reasoning" *: *"\([^"]*\)".*/\1/p')
    SOURCE=$(echo "$RESULT" | sed -n 's/.*"source" *: *"\([^"]*\)".*/\1/p')
    BELOW=$(echo "$RESULT" | sed -n 's/.*"below_threshold" *: *\(true\|false\).*/\1/p')
fi

emit_advisory() {
    # Below-threshold approval — surface the brain's hesitation as
    # additionalContext so Claude sees the reasoning even though we let the
    # tool call proceed. No `decision` field → does not block.
    local msg="claudectl brain is uncertain: $1"
    if command -v jq &>/dev/null; then
        jq -n --arg msg "$msg" '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$msg}}'
    else
        local em
        em=$(json_escape "$msg")
        printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"%s"}}\n' "$em"
    fi
}

emit_deny() {
    # Emit both the legacy `decision`/`reason` (for older Claude Code) and the
    # new `hookSpecificOutput.continueOnBlock` envelope (#249) so the brain's
    # reasoning surfaces back into Claude's next turn instead of just stopping
    # the call.
    local reasoning="$1"
    local sysmsg="claudectl brain blocked this call: $reasoning"
    if command -v jq &>/dev/null; then
        jq -n \
            --arg r "claudectl: $reasoning" \
            --arg dr "$reasoning" \
            --arg sm "$sysmsg" \
            '{
                decision: "deny",
                reason: $r,
                hookSpecificOutput: {
                    hookEventName: "PreToolUse",
                    permissionDecision: "deny",
                    permissionDecisionReason: $dr,
                    continueOnBlock: true,
                    systemMessage: $sm
                }
            }'
    else
        local er em
        er=$(json_escape "$reasoning")
        em=$(json_escape "$sysmsg")
        printf '{"decision":"deny","reason":"claudectl: %s","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s","continueOnBlock":true,"systemMessage":"%s"}}\n' "$er" "$er" "$em"
    fi
}

case "$ACTION" in
    approve)
        # In "on" mode: only auto-approve when confidence is above threshold
        # In "auto" mode: approve all brain approvals regardless of confidence
        if [ "$GATE_MODE" != "auto" ] && [ "$BELOW" = "true" ] && [ "$SOURCE" = "brain" ]; then
            # #249: surface the brain's hesitation without blocking so Claude
            # picks up the same context a human reviewer would have used.
            if [ -n "$REASONING" ]; then
                emit_advisory "$REASONING"
            fi
            exit 0
        fi
        echo '{"decision":"approve"}'
        ;;
    deny)
        if [ -z "$REASONING" ]; then
            REASONING="claudectl brain denied this action"
        fi
        emit_deny "$REASONING"
        ;;
    *)
        # abstain, error, or unknown — fall through
        exit 0
        ;;
esac

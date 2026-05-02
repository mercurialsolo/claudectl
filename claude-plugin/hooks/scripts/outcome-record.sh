#!/usr/bin/env bash
# claudectl outcome-record: PostToolUse hook that captures tool-call outcomes
# (exit code, stderr tail, session metadata) and writes them to the pending
# outcomes spool. The brain reaper later attributes each pending outcome to
# the matching decision and uses it for #220 baselining.
#
# Receives the Claude Code PostToolUse payload on stdin, e.g.:
#   {
#     "session_id": "...",
#     "tool_use_id": "...",
#     "tool_name": "Bash",
#     "tool_input": {"command": "cargo test"},
#     "tool_response": {"interrupted": false, "stdout": "...", "stderr": "..."}
#   }
#
# Always exits 0 — never block Claude Code.

set -u  # do not set -e: we tolerate missing fields

if ! command -v claudectl >/dev/null 2>&1; then
    exit 0
fi

# Respect brain gate mode; "off" disables outcome capture too so the user gets
# a single switch.
GATE_MODE_FILE="$HOME/.claudectl/brain/gate-mode"
GATE_MODE="on"
if [ -f "$GATE_MODE_FILE" ]; then
    GATE_MODE=$(tr -d '[:space:]' < "$GATE_MODE_FILE" 2>/dev/null || echo "on")
fi
if [ "$GATE_MODE" = "off" ]; then
    exit 0
fi

INPUT=$(cat)
if [ -z "$INPUT" ]; then
    exit 0
fi

# ---- Extract fields with tolerant sed parsing ----------------------------
# Multi-byte / nested JSON values are best-effort only; the goal is a useful
# fingerprint, not a perfect parser. Failures fall through silently.

extract() {
    # $1 = key
    echo "$INPUT" | sed -n 's/.*"'"$1"'" *: *"\([^"]*\)".*/\1/p' | head -n 1
}

TOOL_NAME=$(extract "tool_name")
SESSION_ID=$(extract "session_id")
TOOL_USE_ID=$(extract "tool_use_id")

# Tool-specific input summary (mirrors brain-gate.sh).
COMMAND=""
case "$TOOL_NAME" in
    Bash)
        COMMAND=$(echo "$INPUT" | sed -n 's/.*"command" *: *"\([^"]*\)".*/\1/p' | head -n 1)
        ;;
    Write|Edit|NotebookEdit)
        COMMAND=$(echo "$INPUT" | sed -n 's/.*"file_path" *: *"\([^"]*\)".*/\1/p' | head -n 1)
        ;;
    *)
        # No reliable summary for other tools; leave blank so reaper falls
        # back to (tool, project) matching.
        COMMAND=""
        ;;
esac

# Inferred exit code: prefer explicit numeric exit_code, else infer from
# `interrupted` / presence of an error string.
EXIT_CODE=$(echo "$INPUT" | sed -n 's/.*"exit_code" *: *\([0-9-]\+\).*/\1/p' | head -n 1)
if [ -z "$EXIT_CODE" ]; then
    INTERRUPTED=$(echo "$INPUT" | sed -n 's/.*"interrupted" *: *\(true\|false\).*/\1/p' | head -n 1)
    HAS_ERROR=$(echo "$INPUT" | sed -n 's/.*"is_error" *: *\(true\|false\).*/\1/p' | head -n 1)
    if [ "$INTERRUPTED" = "true" ] || [ "$HAS_ERROR" = "true" ]; then
        EXIT_CODE="1"
    else
        EXIT_CODE="0"
    fi
fi

STDERR_TAIL=$(echo "$INPUT" | sed -n 's/.*"stderr" *: *"\([^"]*\)".*/\1/p' | head -n 1)
PROJECT=$(basename "${PWD:-unknown}")
TS=$(date +%s)

# ---- Build PendingOutcome JSON and pipe to claudectl ---------------------
# Shell-escape strings for JSON. We trust the upstream hook payload not to
# contain attacker-controlled binary data; any bad escapes fall through with
# claudectl --record-outcome rejecting the JSON and exiting non-zero, which
# we ignore.

json_escape() {
    # Escape backslashes and double quotes. Newlines collapsed to spaces.
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n\r\t' '   '
}

ESCAPED_TOOL=$(json_escape "${TOOL_NAME:-unknown}")
ESCAPED_PROJECT=$(json_escape "$PROJECT")
ESCAPED_COMMAND=$(json_escape "$COMMAND")
ESCAPED_SESSION=$(json_escape "$SESSION_ID")
ESCAPED_TUID=$(json_escape "$TOOL_USE_ID")
ESCAPED_STDERR=$(json_escape "$STDERR_TAIL")

PAYLOAD=$(cat <<EOF
{
  "tool": "$ESCAPED_TOOL",
  "command": "$ESCAPED_COMMAND",
  "project": "$ESCAPED_PROJECT",
  "session_id": "$ESCAPED_SESSION",
  "tool_use_id": "$ESCAPED_TUID",
  "exit_code": $EXIT_CODE,
  "stderr_tail": "$ESCAPED_STDERR",
  "ts": $TS
}
EOF
)

echo "$PAYLOAD" | claudectl --record-outcome --json >/dev/null 2>&1 || true
exit 0

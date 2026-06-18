#!/usr/bin/env bash
# Gemini CLI → Kaijutsu hook adapter
#
# Transforms Gemini CLI hook JSON to kaijutsu hook format, sends it to the
# kaijutsu-mcp hook socket, and maps the response back to Gemini's format.
#
# Fail-open: if kaijutsu-mcp is unreachable, exits 0 so the agent continues.
set -euo pipefail

# Resolve this script's directory so the jq field-map filter is found
# regardless of the cwd Gemini invokes us from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Read Gemini's hook payload from stdin
INPUT=$(cat)
EVENT_NAME=$(echo "$INPUT" | jq -r '.hook_event_name // empty')

if [ -z "$EVENT_NAME" ]; then
    exit 0
fi

# Map Gemini CLI event names → kaijutsu event names
case "$EVENT_NAME" in
    BeforeTool)       KJ_EVENT="tool.before" ;;
    AfterTool)        KJ_EVENT="tool.after" ;;
    BeforeAgent)      KJ_EVENT="prompt.submit" ;;
    AfterAgent)       KJ_EVENT="agent.stop" ;;
    SessionStart)     KJ_EVENT="session.start" ;;
    SessionEnd)       KJ_EVENT="session.end" ;;
    PreCompress)      KJ_EVENT="agent.compact" ;;
    *)                exit 0 ;;  # Unknown event, pass through
esac

# Build kaijutsu payload — field map lives in gemini-to-kaijutsu.jq (single
# source of truth, parallel to claude-to-kaijutsu.jq).
KJ_INPUT=$(echo "$INPUT" | jq --arg event "$KJ_EVENT" -f "$SCRIPT_DIR/gemini-to-kaijutsu.jq")

# Transform-only escape hatch: print the kaijutsu payload and exit without
# touching the socket.
if [ -n "${KJ_HOOK_DRYRUN:-}" ]; then
    echo "$KJ_INPUT"
    exit 0
fi

# Socket discovery
SOCK="${KJ_HOOK_SOCKET:-${XDG_RUNTIME_DIR:-/tmp}/kaijutsu/hook-${PPID}.sock}"

# Find kaijutsu-mcp binary
KJ_MCP="${KJ_MCP_BIN:-}"
if [ -z "$KJ_MCP" ]; then
    if command -v kaijutsu-mcp >/dev/null 2>&1; then
        KJ_MCP="kaijutsu-mcp"
    elif [ -x "$HOME/.cargo/bin/kaijutsu-mcp" ]; then
        KJ_MCP="$HOME/.cargo/bin/kaijutsu-mcp"
    else
        exit 0
    fi
fi

# Send to kaijutsu-mcp hook client — fail open on any error
KJ_RESPONSE=$(echo "$KJ_INPUT" | "$KJ_MCP" hook --socket "$SOCK" 2>/dev/null) || true
KJ_EXIT=${PIPESTATUS[1]:-0}

# If kaijutsu denied the action, relay to Gemini
if [ "$KJ_EXIT" -eq 2 ] 2>/dev/null; then
    REASON=$(echo "$KJ_RESPONSE" | jq -r '.reason // "blocked by kaijutsu"' 2>/dev/null || echo "blocked by kaijutsu")
    echo "$REASON" >&2
    exit 2
fi

# Inject drift context if present
if [ -n "$KJ_RESPONSE" ]; then
    CONTEXT=$(echo "$KJ_RESPONSE" | jq -r '.context // empty' 2>/dev/null || true)
    if [ -n "$CONTEXT" ]; then
        jq -n --arg ctx "$CONTEXT" '{
            hookSpecificOutput: {
                additionalContext: $ctx
            }
        }'
    fi
fi

exit 0

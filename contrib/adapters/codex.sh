#!/usr/bin/env bash
# Codex CLI -> Kaijutsu hook adapter
#
# Transforms Codex hook JSON to kaijutsu hook format, sends it to the
# kaijutsu-mcp hook socket, and maps the response back to Codex's format.
#
# Install: add the hooks section from contrib/codex-hooks.json to
# ~/.codex/hooks.json (or $CODEX_HOME/hooks.json).
#
# Fail-open: malformed/unknown input, an unavailable jq, kaijutsu-mcp, or
# socket all leave Codex's action untouched and return success.
set -euo pipefail

# Codex may launch command hooks with a minimal environment. Recover the
# conventional Linux runtime directory when it exists so socket discovery does
# not silently disappear. Non-Linux hosts simply retain the existing fail-open
# behavior unless XDG_RUNTIME_DIR was supplied explicitly.
if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
    RUNTIME_CANDIDATE="/run/user/$(id -u)"
    if [ -d "$RUNTIME_CANDIDATE" ]; then
        export XDG_RUNTIME_DIR="$RUNTIME_CANDIDATE"
    fi
fi

# Resolve this script's directory so the jq field-map filter is found
# regardless of the cwd Codex invokes us from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

INPUT=$(cat) || exit 0
EVENT_NAME=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null) || exit 0

if [ -z "$EVENT_NAME" ]; then
    exit 0
fi

# Map Codex event names -> kaijutsu event names. Both compact lifecycle
# events map to agent.compact; the listener uses trigger to retain whether
# the compaction was automatic or manual.
case "$EVENT_NAME" in
    PostToolUse)     KJ_EVENT="tool.after" ;;
    UserPromptSubmit) KJ_EVENT="prompt.submit" ;;
    Stop)            KJ_EVENT="agent.stop" ;;
    SessionStart)    KJ_EVENT="session.start" ;;
    SessionEnd)      KJ_EVENT="session.end" ;;
    SubagentStart)   KJ_EVENT="subagent.start" ;;
    SubagentStop)    KJ_EVENT="subagent.stop" ;;
    PreCompact|PostCompact) KJ_EVENT="agent.compact" ;;
    *)               exit 0 ;;
esac

# -c is load-bearing: the hook socket listener reads ONE line per event.
KJ_INPUT=$(echo "$INPUT" | jq -c --arg event "$KJ_EVENT" \
    -f "$SCRIPT_DIR/codex-to-kaijutsu.jq" 2>/dev/null) || exit 0

# Transform-only escape hatch: print the kaijutsu payload and exit without
# touching the socket. Used by tests and to validate a live hook payload.
if [ -n "${KJ_HOOK_DRYRUN:-}" ]; then
    echo "$KJ_INPUT"
    exit 0
fi

# Socket hint: PPID-based default, env override. No /tmp fallback — the
# server never binds there (it disables the socket without XDG_RUNTIME_DIR),
# and the hook client's ping-based discovery covers a wrong/missing hint.
SOCK="${KJ_HOOK_SOCKET:-${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/kaijutsu/hook-${PPID}.sock}}"

# Find kaijutsu-mcp binary — check PATH first, then common locations.
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

# Send to kaijutsu-mcp hook client — fail open on any error. The socket is a
# hint: the client pings candidates and falls back to discovery, so a wrong
# PPID here (intermediate shell layer) is survivable.
SOCK_ARGS=()
if [ -n "$SOCK" ]; then
    SOCK_ARGS=(--socket "$SOCK")
fi
KJ_RESPONSE=$(echo "$KJ_INPUT" | "$KJ_MCP" hook "${SOCK_ARGS[@]}" 2>/dev/null) || true
KJ_EXIT=${PIPESTATUS[1]:-0}

# A kaijutsu deny uses the conventional hook exit code 2. Codex treats this
# as a blocked hook; all transport and processing failures above remain open.
if [ "$KJ_EXIT" -eq 2 ] 2>/dev/null; then
    REASON=$(echo "$KJ_RESPONSE" | jq -r '.reason // "blocked by kaijutsu"' 2>/dev/null \
        || echo "blocked by kaijutsu")
    echo "$REASON" >&2
    exit 2
fi

# Codex consumes additional context in hookSpecificOutput. Keep the native
# hook event name so Codex can validate the response against the event.
if [ -n "$KJ_RESPONSE" ]; then
    CONTEXT=$(echo "$KJ_RESPONSE" | jq -r '.context // empty' 2>/dev/null || true)
    if [ -n "$CONTEXT" ]; then
        jq -cn --arg ctx "$CONTEXT" --arg event "$EVENT_NAME" '{
            hookSpecificOutput: {
                hookEventName: $event,
                additionalContext: $ctx
            }
        }'
    fi
fi

exit 0

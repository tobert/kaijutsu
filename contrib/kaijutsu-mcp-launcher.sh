#!/usr/bin/env bash
# Launcher for kaijutsu-mcp as a Claude Code stdio MCP server.
#
# MCP servers inherit the client's environment, whose SSH_AUTH_SOCK is
# whatever terminal the session happened to start in — after a reboot it
# can point at a dead socket and kernel SSH auth fails (-32000 on /mcp).
# Prefer a live socket from the environment; otherwise fall back to the
# repo's untracked .ssh-agent file, but only if the socket it names is
# actually alive. Failing loudly beats silently connecting nowhere.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! -S "${SSH_AUTH_SOCK:-}" && -f "$REPO/.ssh-agent" ]]; then
    # shellcheck disable=SC1091
    source "$REPO/.ssh-agent" >/dev/null
    if [[ ! -S "${SSH_AUTH_SOCK:-}" ]]; then
        echo "kaijutsu-mcp-launcher: no live ssh-agent socket (env stale, .ssh-agent stale)" >&2
    fi
fi

BIN="$REPO/target/release/kaijutsu-mcp"
[[ -x "$BIN" ]] || BIN="$REPO/target/debug/kaijutsu-mcp"

exec "$BIN" "$@"

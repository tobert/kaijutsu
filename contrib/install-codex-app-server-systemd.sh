#!/bin/bash
# Install and start the Codex app-server as a systemd user service.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$SCRIPT_DIR/codex-app-server.service.in"
SYSTEMD_USER_DIR="$HOME/.config/systemd/user"
UNIT="$SYSTEMD_USER_DIR/codex-app-server.service"
CODEX_BIN="$(command -v codex)"

if [[ -z "$CODEX_BIN" || ! -x "$CODEX_BIN" ]]; then
    echo "codex is not installed or is not executable" >&2
    exit 1
fi

mkdir -p "$SYSTEMD_USER_DIR"
sed "s|@CODEX_BIN@|$CODEX_BIN|g" "$TEMPLATE" > "$UNIT"

systemctl --user daemon-reload
systemctl --user enable --now codex-app-server.service

echo "Installed $UNIT"
echo "Endpoint: ws://127.0.0.1:4500"
echo "Status:   systemctl --user status codex-app-server"
echo "Logs:     journalctl --user -u codex-app-server -f"

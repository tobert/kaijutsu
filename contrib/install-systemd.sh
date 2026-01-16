#!/bin/bash
# Install kaijutsu-server as a systemd user service
#
# Usage: ./install-systemd.sh [--dev]
#   --dev   Use cargo run (rebuilds on start, good for development)
#   (none)  Use pre-built binary (faster startup)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
SYSTEMD_USER_DIR="$HOME/.config/systemd/user"

# Parse args
USE_DEV=false
if [[ "${1:-}" == "--dev" ]]; then
    USE_DEV=true
fi

# Build release binary if not using dev mode
if [[ "$USE_DEV" == "false" ]]; then
    echo "Building kaijutsu-server (release)..."
    cargo build --release -p kaijutsu-server --manifest-path "$REPO_DIR/Cargo.toml"
fi

# Create systemd user directory
mkdir -p "$SYSTEMD_USER_DIR"

# Generate appropriate unit file
if [[ "$USE_DEV" == "true" ]]; then
    echo "Installing dev service (uses cargo run)..."
    cp "$SCRIPT_DIR/kaijutsu-server.service" "$SYSTEMD_USER_DIR/"
else
    echo "Installing production service (uses binary)..."
    cat > "$SYSTEMD_USER_DIR/kaijutsu-server.service" << EOF
[Unit]
Description=Kaijutsu Server (kaish execution + collaboration)
Documentation=https://github.com/tobert/kaijutsu
After=network.target

[Service]
Type=simple
ExecStart=$REPO_DIR/target/release/kaijutsu-server
WorkingDirectory=$REPO_DIR
Restart=on-failure
RestartSec=5

# Environment
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
EOF
fi

# Reload and enable
systemctl --user daemon-reload
systemctl --user enable kaijutsu-server.service

echo ""
echo "Installed! Commands:"
echo "  systemctl --user start kaijutsu-server    # Start now"
echo "  systemctl --user status kaijutsu-server   # Check status"
echo "  journalctl --user -u kaijutsu-server -f   # Follow logs"
echo ""
echo "To start on login: systemctl --user enable kaijutsu-server"

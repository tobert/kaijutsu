#!/bin/bash
# Install ollama v0.20.0-rc1 ROCm binary alongside pacman-managed packages.
# Run as root, step through interactively.
set -euo pipefail

OLLAMA_VERSION="v0.20.0-rc1"
DOWNLOAD_URL="https://github.com/ollama/ollama/releases/download/${OLLAMA_VERSION}/ollama-linux-amd64-rocm.tar.zst"
INSTALL_DIR="/usr/local/lib/ollama-rc"
BACKUP_BIN="/usr/bin/ollama.pacman-backup"

echo "=== Ollama ${OLLAMA_VERSION} ROCm installer ==="
echo ""
echo "This will:"
echo "  1. Download ollama-linux-amd64-rocm.tar.zst (~990 MB)"
echo "  2. Extract to ${INSTALL_DIR}"
echo "  3. Symlink /usr/local/bin/ollama -> RC binary (takes precedence over /usr/bin)"
echo "  4. Restart ollama.service"
echo ""
echo "To revert: remove /usr/local/bin/ollama symlink and restart the service."
echo ""
read -rp "Continue? [y/N] " confirm
[[ "$confirm" =~ ^[Yy]$ ]] || exit 0

# Step 1: Download
echo ""
echo "--- Step 1: Downloading ${OLLAMA_VERSION} ROCm ---"
mkdir -p /tmp/ollama-rc
cd /tmp/ollama-rc
if [[ -f ollama-linux-amd64-rocm.tar.zst ]]; then
    echo "Archive already exists in /tmp/ollama-rc, skipping download."
    read -rp "Re-download anyway? [y/N] " redownload
    [[ "$redownload" =~ ^[Yy]$ ]] && curl -L -o ollama-linux-amd64-rocm.tar.zst "$DOWNLOAD_URL"
else
    curl -L -o ollama-linux-amd64-rocm.tar.zst "$DOWNLOAD_URL"
fi
echo "Download complete: $(du -h ollama-linux-amd64-rocm.tar.zst | cut -f1)"

# Step 2: Extract
echo ""
echo "--- Step 2: Extracting to ${INSTALL_DIR} ---"
read -rp "Continue? [y/N] " confirm
[[ "$confirm" =~ ^[Yy]$ ]] || exit 0

mkdir -p "${INSTALL_DIR}"
tar --zstd -xf ollama-linux-amd64-rocm.tar.zst -C "${INSTALL_DIR}"
echo "Extracted. Contents:"
ls -lh "${INSTALL_DIR}/"
# The archive may nest under bin/ or lib/ollama/
OLLAMA_BIN=$(find "${INSTALL_DIR}" -name ollama -type f -executable | head -1)
echo "Found binary: ${OLLAMA_BIN}"

if [[ -z "$OLLAMA_BIN" ]]; then
    echo "ERROR: Could not find ollama binary in extracted archive."
    echo "Contents of ${INSTALL_DIR}:"
    find "${INSTALL_DIR}" -type f | head -20
    exit 1
fi

# Verify it works
echo ""
echo "Version check:"
"${OLLAMA_BIN}" --version

# Step 3: Symlink
echo ""
echo "--- Step 3: Creating /usr/local/bin/ollama symlink ---"
echo "This takes precedence over /usr/bin/ollama (pacman) in PATH."
read -rp "Continue? [y/N] " confirm
[[ "$confirm" =~ ^[Yy]$ ]] || exit 0

ln -sfv "${OLLAMA_BIN}" /usr/local/bin/ollama
echo "Verify PATH resolution:"
which ollama
ollama --version

# Step 4: Restart service
echo ""
echo "--- Step 4: Restarting ollama.service ---"
read -rp "Continue? [y/N] " confirm
[[ "$confirm" =~ ^[Yy]$ ]] || exit 0

systemctl restart ollama.service
sleep 2
systemctl status ollama.service --no-pager | head -12

echo ""
echo "=== Done! ==="
echo "Try: ollama pull gemma4:31b"
echo "To revert: rm /usr/local/bin/ollama && systemctl restart ollama.service"

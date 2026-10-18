#!/usr/bin/env bash
# Build localhost/kaijutsu-agent-dev from the host's release binaries.
# The kernel and kaijutsu-mcp must come from the same commit: their wire versions must match.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
# kaibo also publishes release images on ghcr (Amy, 2026-10-18); a binary from one of those can replace the local build.
kaibo="${KAIBO_BIN:-$HOME/src/kaibo/target/release/kaibo}"
commit="$(git -C "$repo" rev-parse --short HEAD)"

for bin in kaijutsu-server kaijutsu-mcp kaijutsu-acp; do
  if [[ ! -x "$repo/target/release/$bin" ]]; then
    echo "missing $repo/target/release/$bin: run cargo build --release -p kaijutsu-server -p kaijutsu-mcp -p kaijutsu-acp" >&2
    exit 1
  fi
done
if [[ -n "$(git -C "$repo" status --porcelain -- crates assets Cargo.toml Cargo.lock)" ]]; then
  echo "the kaijutsu tree has uncommitted changes in crates/ or assets/; the image label would not name the binaries' source" >&2
  exit 1
fi

ctx="$(mktemp -d)"
trap 'rm -rf "$ctx"' EXIT
cp "$here/Containerfile" "$repo"/target/release/{kaijutsu-server,kaijutsu-mcp,kaijutsu-acp} "$kaibo" "$ctx/"

podman build --network=none \
  --build-arg "USER_NAME=$(id -un)" \
  --build-arg "KAIJUTSU_COMMIT=$commit" \
  --tag "localhost/kaijutsu-agent-dev:$commit" \
  --tag "localhost/kaijutsu-agent-dev:latest" \
  "$ctx"

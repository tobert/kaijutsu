#!/usr/bin/env bash
# Check a running agent development container for the paths its seats need to write.
# Usage: contrib/agent-dev/check.sh <project>
#
# The default rc (assets/defaults/rc/lib/create/S30-build-cache.kai) points CARGO_TARGET_DIR at
# ~/.cache/kaijutsu/cargo-target. The container's root file system is read-only, so that path must be a volume.
set -euo pipefail

project="${1:?usage: check.sh <project>}"
container="kaijutsu-dev-$project"
fail=0

probe() {
  local path="$1"
  if podman exec "$container" sh -c "mkdir -p \"$path\" && touch \"$path/.kj-probe\" && rm \"$path/.kj-probe\"" 2>/dev/null; then
    echo "ok    $path is writable"
  else
    echo "FAIL  $path is not writable in $container" >&2
    fail=1
  fi
}

probe "$HOME/.cache/kaijutsu/cargo-target"
probe "$HOME/.cargo/registry"
probe /workspace
probe /exchange/to-supervisor
probe "$HOME/src/$project"

exit "$fail"

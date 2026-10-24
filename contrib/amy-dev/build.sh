#!/usr/bin/env bash
# Build localhost/amy-dev from this directory. Run it on the machine the image will run on: the build uses the network,
# and the host user's name and UID become the container user (keep-id).
#
# The GPU userspace comes from machines/<host>.env, named by `uname -n`, or from --gpu. Pass --gpu to build for a host
# that has no profile yet.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
ref="$(git -C "$repo" rev-parse --short HEAD)"
host="$(uname -n)"

gpu=""
if [[ "${1:-}" == "--gpu" ]]; then
  gpu="${2:?--gpu needs a value: none, rocm, intel, or nvidia}"
elif [[ -f "$here/machines/$host.env" ]]; then
  # shellcheck source=/dev/null
  source "$here/machines/$host.env"
  gpu="$GPU"
else
  echo "no machines/$host.env: pass --gpu none|rocm|intel|nvidia" >&2
  exit 1
fi

podman build \
  --build-arg "USER_NAME=$(id -un)" \
  --build-arg "USER_UID=$(id -u)" \
  --build-arg "GPU=$gpu" \
  --label "amy-dev.kaijutsu=$ref" \
  --tag "localhost/amy-dev:$ref-$gpu" \
  --tag "localhost/amy-dev:$gpu" \
  --file "$here/Containerfile" \
  "$here"

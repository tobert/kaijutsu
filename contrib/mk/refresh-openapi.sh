#!/usr/bin/env bash
# Copy the megakernel's /mk/v1 OpenAPI file into kaijutsu-mk's test fixtures
# and name its source commit. Run the crate's tests afterward: the conformance
# test reports every type that no longer matches.
set -euo pipefail

mk_repo="${1:-$HOME/src/megakernel-qwen38-flashnext-strixhalo}"
here="$(cd "$(dirname "$0")/../.." && pwd)"
fixtures="$here/crates/kaijutsu-mk/tests/fixtures"

if [[ -n "$(git -C "$mk_repo" status --porcelain -- service/openapi.json)" ]]; then
    echo "refusing: $mk_repo/service/openapi.json has uncommitted changes" >&2
    exit 1
fi
rev="$(git -C "$mk_repo" rev-parse HEAD)"
cp "$mk_repo/service/openapi.json" "$fixtures/mk-openapi.json"
sed -i "1s|.*|mk-openapi.json: megakernel-qwen38-flashnext-strixhalo service/openapi.json at $rev|" "$fixtures/mk-openapi.source"
echo "mk-openapi.json now matches megakernel $rev"

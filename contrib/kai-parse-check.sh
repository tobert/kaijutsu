#!/usr/bin/env bash
# Parse-check every .kai in the repo with `kaish --plan-file`, which analyzes
# without running: no command executes and no filesystem is touched.
#
#     contrib/kai-parse-check.sh            # exits 0, or 1 listing failures
#     KAISH=/path/to/kaish contrib/kai-parse-check.sh
#
# Two jobs. For us it guards against committing an rc script the kernel
# cannot parse -- an rc script only fails when a context is created, which is
# far from the edit that broke it. For the kaish lane it is a real corpus to
# test a candidate lexer change against, which beats synthetic cases.
#
# A parse failure here is not always ours. kaish 0.16 rejects a bareword that
# ends in `=` or carries a second `=` (`ps -o etime=,pcpu=`), so a script can
# fail on a line that is correct POSIX. Read the message before editing the
# script -- see docs/issues.md, "A kaish parse failure degrades the gate".
set -uo pipefail

KAISH="${KAISH:-kaish}"
if ! command -v "$KAISH" >/dev/null 2>&1; then
  echo "kai-parse-check: '$KAISH' not found; set KAISH to its path" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 2

echo "kaish: $("$KAISH" --version 2>&1 | head -1)"
echo

checked=0
failed=0
while IFS= read -r f; do
  checked=$((checked + 1))
  if out="$("$KAISH" --plan-file "$f" 2>&1)"; then
    continue
  fi
  failed=$((failed + 1))
  echo "FAIL $f"
  # The error body is JSON; print each message with its offset when jq is
  # present, and the raw body when it is not.
  if command -v jq >/dev/null 2>&1; then
    echo "$out" | jq -r '.errors[]? | "       \(.message)  [offset \(.start)-\(.end)]"' 2>/dev/null \
      || echo "       $out"
  else
    echo "       $out"
  fi
done < <(find . -name '*.kai' -not -path './target/*' -print | sort)

echo
if [ "$failed" -ne 0 ]; then
  echo "$failed of $checked .kai file(s) failed to parse"
  exit 1
fi
echo "all $checked .kai file(s) parse"

#!/usr/bin/env bash
# Smoke test for the headless app harness: start it with agent control on,
# drive the app through kaijutsu/act, read kaijutsu/state, and take a
# screenshot. Exits non-zero on the first failed check; always tears the
# harness down.
#
# Usage: contrib/headless-smoke.sh [headless-app.sh start options...]
# Needs built kaijutsu-server, kaijutsu-mcp, and kaijutsu-app (start --build).
set -euo pipefail

HARNESS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/headless-app.sh"
SIZE="1280x720"
FAILED=0

check() { # description, command...
  local what="$1"; shift
  if "$@" >/dev/null 2>&1; then echo "ok    $what"; else echo "FAIL  $what"; FAILED=1; fi
}

# A JSON field of the last `state` reply equals a value.
state_is() { # field expected
  "$HARNESS" state | python3 -c '
import json, sys
field, want = sys.argv[1], sys.argv[2]
got = json.load(sys.stdin)["result"][field]
sys.exit(0 if str(got).lower() == want.lower() else 1)' "$1" "$2"
}

wait_state() { # field expected seconds
  local _
  for _ in $(seq 1 "$3"); do state_is "$1" "$2" && return 0; sleep 1; done
  return 1
}

act_refused_with() { # action code
  "$HARNESS" act "$1" | python3 -c '
import json, sys
sys.exit(0 if json.load(sys.stdin).get("error", {}).get("code") == int(sys.argv[1]) else 1)' "$2"
}

png_size_is() { # path WxH
  python3 -c '
import struct, sys
data = open(sys.argv[1], "rb").read(24)
w, h = struct.unpack(">II", data[16:24])
sys.exit(0 if f"{w}x{h}" == sys.argv[2] else 1)' "$1" "$2"
}

cleanup() { "$HARNESS" stop >/dev/null 2>&1 || true; }
trap cleanup EXIT
"$HARNESS" stop >/dev/null 2>&1 || true

"$HARNESS" start --agent-control --size "$SIZE" "$@" >/dev/null
check "agent control is on at startup" wait_state agent_control true 30
check "the app joined a context on the Conversation screen" wait_state screen Conversation 30

"$HARNESS" act GoToWell >/dev/null
check "act GoToWell reaches the Room screen" wait_state screen Room 15

shot="$("$HARNESS" shot smoke | tail -1)"
check "the screenshot is the requested size ($SIZE)" png_size_is "$shot" "$SIZE"

check "act refuses the player's latch" act_refused_with ToggleAgentControl -32602
check "act refuses an unknown action" act_refused_with NoSuchAction -32602

exit "$FAILED"

#!/usr/bin/env bash
# Drive one task through Harbor's standalone ACP runner against a throwaway
# kaijutsu kernel.
#
# Usage:
#   contrib/bench/run-host-task.sh <run-dir> <task-dir> <instruction> [label]
#
# The runner uses its own cwd as the ACP session cwd, so this runs it from the
# task directory. Logs land in "<run-dir>/tasks/<label>/": acp-events.jsonl and
# acp-summary.json from the runner, runner.log from this script.
set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: run-host-task.sh <run-dir> <task-dir> <instruction> [label]" >&2
  exit 2
fi

RUN_DIR="$1"
TASK_DIR="$2"
INSTRUCTION="$3"
LABEL="${4:-$(basename "$TASK_DIR")-$(date +%H%M%S)}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# The runner imports the `acp` package (agent-client-protocol) and nothing
# else. It must not run from inside harbor's own package directory: a sibling
# `acp.py` there shadows the package, which is why harbor copies this one file
# into a task container. Do the same — copy it out, then run it.
# contrib/bench/README.md, "Python environment" builds the interpreter.
BENCH_PYTHON="${BENCH_PYTHON:-/home/atobey/src/bench-work/acp-venv/bin/python}"
ACP_RUNNER_SRC="${HARBOR_ACP_RUNNER:-/home/atobey/src/harbor/src/harbor/agents/installed/acp_runner.py}"

for path in "$RUN_DIR/env.sh" "$ACP_RUNNER_SRC" "$BENCH_PYTHON"; do
  if [[ ! -f "$path" ]]; then
    echo "missing: $path" >&2
    exit 2
  fi
done
if [[ ! -d "$TASK_DIR" ]]; then
  echo "missing task directory: $TASK_DIR" >&2
  exit 2
fi

# shellcheck disable=SC1091
source "$RUN_DIR/env.sh"

LOGS_DIR="$RUN_DIR/tasks/$LABEL"
mkdir -p "$LOGS_DIR"
ACP_RUNNER="$RUN_DIR/acp_runner.py"
cp "$ACP_RUNNER_SRC" "$ACP_RUNNER"

# HARBOR_ACP_REQUESTED_MODEL stays unset on purpose: kaijutsu-acp advertises no
# model-selection mechanism, and the runner raises when a requested model has
# nowhere to go. The model is the kernel's, chosen at boot.
export HARBOR_ACP_PERMISSION_MODE="${HARBOR_ACP_PERMISSION_MODE:-allow}"
export HARBOR_ACP_AUTH_POLICY="${HARBOR_ACP_AUTH_POLICY:-auto}"
export HARBOR_ACP_MCP_SERVERS_JSON="${HARBOR_ACP_MCP_SERVERS_JSON:-[]}"
unset HARBOR_ACP_REQUESTED_MODEL

echo "== task $LABEL"
echo "   cwd         $TASK_DIR"
echo "   logs        $LOGS_DIR"
echo "   instruction $INSTRUCTION"

set +e
( cd "$TASK_DIR" && \
  "$BENCH_PYTHON" "$ACP_RUNNER" \
    --instruction "$INSTRUCTION" \
    --logs-dir "$LOGS_DIR" \
    --launcher "$REPO_ROOT/contrib/bench/acp-launch.sh" ) \
  >"$LOGS_DIR/runner.log" 2>&1
STATUS=$?
set -e

echo "   runner exit $STATUS"
if [[ -f "$LOGS_DIR/acp-summary.json" ]]; then
  python3 -c '
import json, sys
summary = json.load(open(sys.argv[1]))
print("   stop reason ", (summary.get("prompt_response") or {}).get("stopReason"))
print("   permissions ", summary.get("permissions_requested"))
print("   usage       ", summary.get("latest_usage_update"))
if summary.get("error"):
    print("   error       ", summary["error"])
' "$LOGS_DIR/acp-summary.json"
fi
exit "$STATUS"

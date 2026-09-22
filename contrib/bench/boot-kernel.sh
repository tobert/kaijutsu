#!/usr/bin/env bash
# Boot a throwaway kaijutsu kernel for bench runs.
#
# Every piece of state lives under one run directory: its own XDG trees, its
# own config root, its own keys, its own log. Nothing here touches the
# operator's kernel, and the port is chosen away from the default 2222.
#
# Usage:
#   contrib/bench/boot-kernel.sh [--run-id ID] [--port N] [--model mock|deepseek]
#
# On success it writes "$RUN_DIR/env.sh" and prints its path. Source that file
# to get KJ_BENCH_PORT, KJ_BENCH_KEY, KJ_BENCH_CHARACTER, and the XDG trees.
set -euo pipefail

BENCH_ROOT="${BENCH_ROOT:-/home/atobey/src/bench-work}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SERVER_BIN="${KJ_SERVER_BIN:-$REPO_ROOT/target/debug/kaijutsu-server}"
MCP_BIN="${KJ_MCP_BIN:-$REPO_ROOT/target/debug/kaijutsu-mcp}"

RUN_ID="bench-$(date +%Y%m%d-%H%M%S)"
PORT="22722"
MODEL_MODE="mock"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-id) RUN_ID="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --model) MODEL_MODE="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

case "$MODEL_MODE" in
  mock|deepseek) ;;
  *) echo "--model takes 'mock' or 'deepseek', not '$MODEL_MODE'" >&2; exit 2 ;;
esac

if [[ "$PORT" == "2222" ]]; then
  echo "refusing port 2222: that is the operator's kernel" >&2
  exit 2
fi
if ! command -v ss >/dev/null 2>&1; then
  # Without ss there is no port check, and "no output" would read as "free".
  # Refuse rather than boot a second kernel onto a busy port.
  echo "ss (iproute2) is required to check whether port $PORT is free" >&2
  exit 2
fi

# Exactly the local port, never a substring: `grep ":22722"` also matches
# :227220. `ss -H` drops the header so an empty result means an empty list.
port_is_listening() {
  ss -Hltn "sport = :$1" | grep -qE "[[:space:]][^[:space:]]*:$1[[:space:]]"
}

if port_is_listening "$PORT"; then
  echo "port $PORT is already listening; pick another with --port" >&2
  exit 2
fi
for binary in "$SERVER_BIN" "$MCP_BIN"; do
  if [[ ! -x "$binary" ]]; then
    echo "missing binary: $binary" >&2
    exit 2
  fi
done

RUN_DIR="$BENCH_ROOT/kernels/$RUN_ID"
if [[ -e "$RUN_DIR" ]]; then
  echo "run directory already exists: $RUN_DIR" >&2
  exit 2
fi

CONFIG_ROOT="$RUN_DIR/config"
mkdir -p \
  "$RUN_DIR/keys" \
  "$RUN_DIR/logs" \
  "$RUN_DIR/xdg/data" \
  "$RUN_DIR/xdg/config" \
  "$RUN_DIR/xdg/state" \
  "$RUN_DIR/xdg/cache" \
  "$RUN_DIR/xdg/run" \
  "$CONFIG_ROOT/kernel" \
  "$CONFIG_ROOT/rc" \
  "$CONFIG_ROOT/client" \
  "$CONFIG_ROOT/midi"
chmod 700 "$RUN_DIR/xdg/run"

export XDG_DATA_HOME="$RUN_DIR/xdg/data"
export XDG_CONFIG_HOME="$RUN_DIR/xdg/config"
export XDG_STATE_HOME="$RUN_DIR/xdg/state"
export XDG_CACHE_HOME="$RUN_DIR/xdg/cache"
export XDG_RUNTIME_DIR="$RUN_DIR/xdg/run"

# The connecting identity. kaijutsu-acp authenticates as this character and
# the context's performer must be someone else, so this one reviews and the
# performer below works (docs/approval-identity.md).
REVIEWER="bench-director"
PERFORMER="bench-coder"
KEY_FILE="$RUN_DIR/keys/$REVIEWER"
ssh-keygen -t ed25519 -N "" -C "$REVIEWER@bench" -f "$KEY_FILE" >/dev/null

echo "== init: root character $REVIEWER"
"$SERVER_BIN" init --as "$REVIEWER" --key "$KEY_FILE.pub"

# Name the rc tree explicitly. `rc reseed` also resolves `--config-root` now
# (crates/kaijutsu-server/src/main.rs, `cmd_rc_reseed`), but `--dir` says which tree
# was written without depending on that resolution, and this script has a tree
# in hand either way.
echo "== rc reseed into $CONFIG_ROOT/rc"
"$SERVER_BIN" rc reseed --dir "$CONFIG_ROOT/rc"

echo "== starting kaijutsu-server on port $PORT"
SERVER_ENV=()
if [[ "$MODEL_MODE" == "mock" ]]; then
  SERVER_ENV+=("KJ_MOCK_SCRIPT_DIR=$REPO_ROOT/contrib/bench/mock_scripts")
fi
env ${SERVER_ENV[@]+"${SERVER_ENV[@]}"} \
  XDG_DATA_HOME="$XDG_DATA_HOME" \
  XDG_CONFIG_HOME="$XDG_CONFIG_HOME" \
  XDG_STATE_HOME="$XDG_STATE_HOME" \
  XDG_CACHE_HOME="$XDG_CACHE_HOME" \
  XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
  RUST_LOG="${KJ_BENCH_RUST_LOG:-info}" \
  "$SERVER_BIN" --config-root "$CONFIG_ROOT" --port "$PORT" \
  >"$RUN_DIR/logs/kernel.log" 2>&1 &
SERVER_PID=$!
echo "$SERVER_PID" > "$RUN_DIR/kernel.pid"

for _ in $(seq 1 120); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "kernel died during startup; see $RUN_DIR/logs/kernel.log" >&2
    tail -20 "$RUN_DIR/logs/kernel.log" >&2
    exit 1
  fi
  if port_is_listening "$PORT"; then
    break
  fi
  sleep 0.5
done
if ! port_is_listening "$PORT"; then
  echo "kernel never listened on $PORT; see $RUN_DIR/logs/kernel.log" >&2
  exit 1
fi
echo "   pid $SERVER_PID, log $RUN_DIR/logs/kernel.log"

# Record what this pid IS, so stop-kernel.sh can refuse a reused pid.
# starttime is field 22 of /proc/<pid>/stat, but field 2 (comm) is
# parenthesized and may hold spaces, so count from after the closing paren.
SERVER_START_TICKS="$(sed 's/.*) //' "/proc/$SERVER_PID/stat" | awk '{print $20}')"
cat > "$RUN_DIR/kernel.ident" <<IDENT
pid=$SERVER_PID
start_ticks=$SERVER_START_TICKS
config_root=$CONFIG_ROOT
port=$PORT
IDENT

# $HOME for every client this script starts. A throwaway kernel mints a fresh
# host key each boot; without this the client learns it by trust on first use
# into the operator's real ~/.ssh/known_hosts, and the next boot on the same
# port is refused as a host-key mismatch. --insecure skips the check outright;
# the redirected HOME is the belt to its braces.
CLIENT_HOME="$RUN_DIR/home"
mkdir -p "$CLIENT_HOME/.ssh"
chmod 700 "$CLIENT_HOME/.ssh"

KJMCP=("python3" "$REPO_ROOT/contrib/bench/kjmcp.py"
       "--binary" "$MCP_BIN" "--port" "$PORT" "--key-file" "$KEY_FILE"
       "--home" "$CLIENT_HOME" "--insecure")

# Listening is not ready: the socket binds before the kernel finishes its
# genesis rc run. Ask it a read-only question until it answers.
echo "== waiting for the kernel to answer"
READY=""
for attempt in $(seq 1 30); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "kernel died before it answered; see $RUN_DIR/logs/kernel.log" >&2
    tail -20 "$RUN_DIR/logs/kernel.log" >&2
    exit 1
  fi
  if KJMCP_QUIET=1 "${KJMCP[@]}" "kj context list" >/dev/null 2>&1; then
    READY="yes"
    echo "   ready after $attempt attempt(s)"
    break
  fi
  sleep 1
done
if [[ -z "$READY" ]]; then
  echo "kernel listened on $PORT but never answered a read; see $RUN_DIR/logs/kernel.log" >&2
  exit 1
fi

# The gate, patched AFTER the first start. The kernel seeds a config tree only
# when its directory is empty (crates/kaijutsu-server/src/rpc.rs, the
# `dir_is_empty` arm), so writing gate.toml beforehand would cost this kernel
# theme.toml, mcp.toml, continuation.toml and the rest. gate.toml is re-read
# at every gated submission, so patching it now needs no restart.
#
# The admin tier: the bench driver runs `kj character create` and friends from
# an `mcp` context, and an uncovered kj write would raise an ask nobody is
# there to answer. The coder tier gets `sleep` alone — an ACP session's shell
# work stays uncovered on purpose, so it raises real asks and exercises the
# permission round trip.
GATE_FILE="$CONFIG_ROOT/kernel/gate.toml"
if [[ ! -f "$GATE_FILE" ]]; then
  echo "the kernel did not seed $GATE_FILE; refusing to invent one" >&2
  exit 1
fi
echo "== patching $GATE_FILE"
python3 - "$GATE_FILE" <<'GATE'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
text = path.read_text()
# A second [context_type.mcp] table makes the whole file unparseable, and an
# unparseable gate policy refuses every gated submission — including the
# character read behind register_session. So the extra keys go inside the
# tier the shipped default already carries.
if text.count("[context_type.mcp]") != 1:
    raise SystemExit(
        f"{path}: expected exactly one [context_type.mcp] tier, "
        f"found {text.count('[context_type.mcp]')}"
    )
extra = ["kj character", "kj backend", "kj cast", "kj context set", "kj model"]
out = []
inside = False
for line in text.splitlines():
    if line.strip() == "[context_type.mcp]":
        inside = True
    elif inside and line.strip() == "]":
        out.extend(f'  "{key}",   # bench: unattended kernel setup' for key in extra)
        inside = False
    out.append(line)
if "[context_type.coder]" in text:
    raise SystemExit(f"{path}: a [context_type.coder] tier is already present")
out.extend(["", "[context_type.coder]", "allow = [", '  "sleep",', "]"])
path.write_text("\n".join(out) + "\n")
GATE

echo "== creating the performer character $PERFORMER"
"${KJMCP[@]}" "kj character create $PERFORMER"

if [[ "$MODEL_MODE" == "mock" ]]; then
  echo "== pointing the kernel at the mock backend"
  "${KJMCP[@]}" \
    "kj backend set mock --kind mock --key-optional" \
    "kj backend default set --backend mock --model bench-mock"
else
  echo "== pointing the kernel at deepseek-v4-flash"
  "${KJMCP[@]}" \
    "kj backend default set --backend deepseek --model deepseek-v4-flash"
fi

cat > "$RUN_DIR/env.sh" <<ENV
# Source me. Written by contrib/bench/boot-kernel.sh.
export KJ_BENCH_RUN_DIR="$RUN_DIR"
export KJ_BENCH_PORT="$PORT"
export KJ_BENCH_KEY="$KEY_FILE"
export KJ_BENCH_CHARACTER="$PERFORMER"
export KJ_BENCH_REVIEWER="$REVIEWER"
export KJ_BENCH_MODEL_MODE="$MODEL_MODE"
export KJ_BENCH_CLIENT_HOME="$CLIENT_HOME"
export XDG_DATA_HOME="$XDG_DATA_HOME"
export XDG_CONFIG_HOME="$XDG_CONFIG_HOME"
export XDG_STATE_HOME="$XDG_STATE_HOME"
export XDG_CACHE_HOME="$XDG_CACHE_HOME"
export XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR"
ENV

echo
echo "kernel ready: $RUN_DIR/env.sh"
echo "  port      $PORT"
echo "  key       $KEY_FILE"
echo "  performer $PERFORMER"
echo "  reviewer  $REVIEWER"

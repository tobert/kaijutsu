#!/usr/bin/env bash
# Stop a throwaway kernel booted by boot-kernel.sh.
#
# Usage: contrib/bench/stop-kernel.sh <run-dir>
#
# A pidfile alone does not identify a process: the kernel may have exited and
# the number been handed to something else. Check that the pid really is this
# run's server — right binary, right config root, and the same start time the
# boot recorded — and refuse loudly rather than signal a stranger.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: stop-kernel.sh <run-dir>" >&2
  exit 2
fi
RUN_DIR="$1"
PID_FILE="$RUN_DIR/kernel.pid"
IDENT_FILE="$RUN_DIR/kernel.ident"

if [[ ! -f "$PID_FILE" ]]; then
  echo "no pidfile at $PID_FILE" >&2
  exit 1
fi
PID="$(cat "$PID_FILE")"
if [[ ! "$PID" =~ ^[0-9]+$ ]]; then
  echo "pidfile $PID_FILE does not hold a pid: $PID" >&2
  exit 1
fi

if [[ ! -d "/proc/$PID" ]]; then
  echo "pid $PID is not running; leaving $PID_FILE for the record"
  exit 0
fi

# /proc/<pid>/cmdline is NUL-separated; turn it into one greppable line.
CMDLINE="$(tr '\0' ' ' < "/proc/$PID/cmdline")"
if [[ "$CMDLINE" != *kaijutsu-server* ]]; then
  echo "refusing to signal pid $PID: it is not a kaijutsu-server" >&2
  echo "  cmdline: $CMDLINE" >&2
  exit 1
fi

EXPECTED_CONFIG_ROOT=""
EXPECTED_START_TICKS=""
if [[ -f "$IDENT_FILE" ]]; then
  EXPECTED_CONFIG_ROOT="$(sed -n 's/^config_root=//p' "$IDENT_FILE")"
  EXPECTED_START_TICKS="$(sed -n 's/^start_ticks=//p' "$IDENT_FILE")"
fi

if [[ -n "$EXPECTED_CONFIG_ROOT" && "$CMDLINE" != *"$EXPECTED_CONFIG_ROOT"* ]]; then
  echo "refusing to signal pid $PID: it is a kaijutsu-server, but not this run's" >&2
  echo "  expected --config-root $EXPECTED_CONFIG_ROOT" >&2
  echo "  cmdline: $CMDLINE" >&2
  exit 1
fi

# Field 22 of /proc/<pid>/stat is starttime in clock ticks since boot; a reused
# pid cannot carry the same one. Field 2 (comm) is parenthesized and may hold
# spaces, so count from after the closing paren rather than from the start.
ACTUAL_START_TICKS="$(sed 's/.*) //' "/proc/$PID/stat" | awk '{print $20}')"
if [[ -n "$EXPECTED_START_TICKS" && "$EXPECTED_START_TICKS" != "$ACTUAL_START_TICKS" ]]; then
  echo "refusing to signal pid $PID: the number was reused" >&2
  echo "  boot recorded start_ticks=$EXPECTED_START_TICKS, /proc says $ACTUAL_START_TICKS" >&2
  exit 1
fi
if [[ -z "$EXPECTED_START_TICKS" ]]; then
  echo "note: no start_ticks in $IDENT_FILE; identity rests on the cmdline alone" >&2
fi

kill "$PID"
for _ in $(seq 1 40); do
  if [[ ! -d "/proc/$PID" ]]; then
    echo "stopped $PID"
    exit 0
  fi
  sleep 0.25
done
kill -9 "$PID"
echo "killed $PID after it ignored SIGTERM"

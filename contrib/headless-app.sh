#!/usr/bin/env bash
# Headless runtime harness for kaijutsu-app.
#
# Runs the Bevy app on a headless Wayland compositor (labwc, wlroots headless
# backend) against a throwaway kernel, so app changes can be checked with BRP
# screenshots and keys without a person at a display. Everything this script
# starts lives under one state directory and `stop` tears down only that.
#
# Usage:
#   contrib/headless-app.sh start [--size WxH] [--kernel-port N] [--brp-port N]
#                                 [--wayland NAME] [--build]
#   contrib/headless-app.sh status
#   contrib/headless-app.sh shot NAME [DIR]    BRP screenshot -> DIR/NAME.png
#   contrib/headless-app.sh keys KEY...        BRP send_keys: all KEYs held at once (a chord)
#   contrib/headless-app.sh text STRING        BRP type_text: type STRING one char per frame
#   contrib/headless-app.sh logs               path of the app, kernel, labwc logs
#   contrib/headless-app.sh stop
#
# The kernel is a second kaijutsu-server on its own port with its own XDG trees,
# config root, and keys; it never touches the operator's kernel on 2222. Host
# execution is denied (the kernel default). The app authenticates with a
# generated key bound to the root character `headless-director` (--key-file),
# so no ssh-agent is involved. Seeding goes through kaijutsu-mcp (there is no
# standalone `kj`), via contrib/bench/kjmcp.py.
#
# The last seed step raises one pending ask (`git push` is in gate.toml's ask
# list). It runs last because a kaijutsu-mcp built before the ledger push
# callback cannot keep a connection once an ask is pending; the app can.
#
# Builds are low priority and never clean:
#   contrib/headless-app.sh start --build
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE="${KJ_HEADLESS_DIR:-/tmp/kj-headless}"
SERVER_BIN="${KJ_SERVER_BIN:-$REPO_ROOT/target/debug/kaijutsu-server}"
MCP_BIN="${KJ_MCP_BIN:-$REPO_ROOT/target/debug/kaijutsu-mcp}"
APP_BIN="${KJ_APP_BIN:-$REPO_ROOT/target/debug/kaijutsu-app}"
RTDIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
DIRECTOR="headless-director"

die() { echo "headless-app: $*" >&2; exit 1; }

port_is_listening() {
  ss -Hltn "sport = :$1" | grep -qE "[[:space:]][^[:space:]]*:$1[[:space:]]"
}

# A pid file names a process; check the cmdline still matches before signaling.
pid_alive_as() { # pidfile, cmdline-substring
  [[ -f "$1" ]] || return 1
  local pid
  pid="$(cat "$1")"
  [[ -d "/proc/$pid" ]] && tr '\0' ' ' < "/proc/$pid/cmdline" | grep -q -- "$2"
}

stop_pid() { # pidfile, cmdline-substring
  local pid
  if pid_alive_as "$1" "$2"; then
    pid="$(cat "$1")"
    kill "$pid"
    for _ in $(seq 1 40); do
      [[ -d "/proc/$pid" ]] || break
      sleep 0.25
    done
    [[ -d "/proc/$pid" ]] && kill -9 "$pid"
    echo "stopped $2 ($pid)"
  fi
  rm -f "$1"
}

kjmcp() {
  KJMCP_QUIET=1 python3 "$REPO_ROOT/contrib/bench/kjmcp.py" \
    --binary "$MCP_BIN" --port "$KERNEL_PORT" --key-file "$STATE/keys/$DIRECTOR" \
    --home "$STATE/home" --insecure --context headless-admin "$@" 2>>"$STATE/logs/seed.log"
}

load_env() { # shared by subcommands that talk to a running harness
  [[ -f "$STATE/env.sh" ]] || die "not running (no $STATE/env.sh)"
  # shellcheck disable=SC1091
  source "$STATE/env.sh"
}

brp() { # method, params-json
  curl -sS -m 30 "http://127.0.0.1:$BRP_PORT" -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}"
}

cmd_start() {
  local size="1920x1080" KERNEL_PORT=22724 BRP_PORT=15712 wayland="" build=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --size) size="$2"; shift 2 ;;
      --kernel-port) KERNEL_PORT="$2"; shift 2 ;;
      --brp-port) BRP_PORT="$2"; shift 2 ;;
      --wayland) wayland="$2"; shift 2 ;;
      --build) build=1; shift ;;
      *) die "unknown option: $1" ;;
    esac
  done
  [[ "$KERNEL_PORT" != 2222 ]] || die "refusing port 2222: that is the operator's kernel"
  [[ ! -e "$STATE/env.sh" ]] || die "already started ($STATE); run stop first"
  for tool in ss curl python3 labwc wlr-randr ssh-keygen; do
    command -v "$tool" >/dev/null || die "missing tool: $tool"
  done
  port_is_listening "$KERNEL_PORT" && die "kernel port $KERNEL_PORT is busy"
  port_is_listening "$BRP_PORT" && die "BRP port $BRP_PORT is busy"

  if [[ $build -eq 1 ]]; then
    echo "== build (low priority)"
    (cd "$REPO_ROOT" && systemd-run --user --scope -p CPUWeight=20 --nice=19 -- \
      cargo build -p kaijutsu-server -p kaijutsu-mcp -p kaijutsu-app)
  fi
  for binary in "$SERVER_BIN" "$MCP_BIN" "$APP_BIN"; do
    [[ -x "$binary" ]] || die "missing binary: $binary (try --build)"
  done

  mkdir -p "$STATE"/{keys,logs,config/kernel,config/rc,config/client,config/midi,home/.ssh,app}
  mkdir -p "$STATE"/xdg/{data,config,state,cache,run} "$STATE/shots"
  chmod 700 "$STATE/xdg/run" "$STATE/home/.ssh"

  # --- compositor: reuse a named socket, or start our own labwc -------------
  local labwc_started=0
  if [[ -n "$wayland" ]]; then
    [[ -S "$RTDIR/$wayland" ]] || die "no wayland socket $RTDIR/$wayland"
  else
    mkdir -p "$STATE/labwc-cfg"
    local before after
    before="$(cd "$RTDIR" && ls -d wayland-* 2>/dev/null | grep -v '\.lock$' || true)"
    WLR_BACKENDS=headless WLR_RENDERER=vulkan WLR_LIBINPUT_NO_DEVICES=1 \
      WLR_HEADLESS_OUTPUTS=1 labwc -C "$STATE/labwc-cfg" \
      >"$STATE/logs/labwc.log" 2>&1 &
    echo $! > "$STATE/labwc.pid"
    labwc_started=1
    for _ in $(seq 1 40); do
      after="$(cd "$RTDIR" && ls -d wayland-* 2>/dev/null | grep -v '\.lock$' || true)"
      wayland="$(comm -13 <(echo "$before" | sort) <(echo "$after" | sort) | head -1)"
      [[ -n "$wayland" ]] && break
      kill -0 "$(cat "$STATE/labwc.pid")" 2>/dev/null || die "labwc died; see $STATE/logs/labwc.log"
      sleep 0.25
    done
    [[ -n "$wayland" ]] || die "labwc made no wayland socket; see $STATE/logs/labwc.log"
  fi
  local output
  output="$(WAYLAND_DISPLAY="$wayland" wlr-randr | awk 'NR==1{print $1}')"
  [[ -n "$output" ]] || die "wlr-randr lists no output on $wayland"
  WAYLAND_DISPLAY="$wayland" wlr-randr --output "$output" --custom-mode "$size" \
    || die "could not set $size on $output"
  echo "== wayland $wayland, output $output at $size (labwc started by us: $labwc_started)"

  # --- ephemeral kernel -----------------------------------------------------
  local key="$STATE/keys/$DIRECTOR"
  ssh-keygen -t ed25519 -N "" -C "$DIRECTOR@headless" -f "$key" >/dev/null
  local kenv=(XDG_DATA_HOME="$STATE/xdg/data" XDG_CONFIG_HOME="$STATE/xdg/config"
              XDG_STATE_HOME="$STATE/xdg/state" XDG_CACHE_HOME="$STATE/xdg/cache"
              XDG_RUNTIME_DIR="$STATE/xdg/run")
  echo "== init root character $DIRECTOR"
  env "${kenv[@]}" "$SERVER_BIN" init --as "$DIRECTOR" --key "$key.pub" >/dev/null
  env "${kenv[@]}" "$SERVER_BIN" rc reseed --dir "$STATE/config/rc" >/dev/null
  env "${kenv[@]}" RUST_LOG=info "$SERVER_BIN" --config-root "$STATE/config" \
    --port "$KERNEL_PORT" >"$STATE/logs/kernel.log" 2>&1 &
  echo $! > "$STATE/kernel.pid"
  local ready=""
  for _ in $(seq 1 120); do
    kill -0 "$(cat "$STATE/kernel.pid")" 2>/dev/null \
      || { tail -20 "$STATE/logs/kernel.log" >&2; die "kernel died during startup"; }
    port_is_listening "$KERNEL_PORT" && break
    sleep 0.5
  done
  port_is_listening "$KERNEL_PORT" || die "kernel never listened on $KERNEL_PORT"
  for _ in $(seq 1 30); do
    if kjmcp "kj context list" >/dev/null; then ready=yes; break; fi
    sleep 1
  done
  [[ -n "$ready" ]] || die "kernel listened but never answered; see $STATE/logs/kernel.log"

  # --- seed -----------------------------------------------------------------
  echo "== seed contexts and blocks"
  # Multi-line content rides inside single quotes: kaish keeps the newlines.
  local abc=$'X:1\nT:Headless\nM:4/4\nL:1/8\nK:C\nCDEF GABc|c2 G2 E2 C2|'
  local md=$'## Harness\n\n- **Compositor**: labwc on the wlroots headless backend.\n- **Kernel**: a throwaway kaijutsu-server on its own port.\n- **App**: kaijutsu-app with BRP enabled.\n\n```rust\nfn main() { println!("hello"); }\n```'
  kjmcp \
    "kj kohai create alpha-notes" \
    "kj kohai create beta-review" \
    "kj kohai create gamma-scratch" \
    "kj block create -c alpha-notes --role user --kind text --content 'Summarize the headless harness in three bullets.'" \
    "kj block create -c alpha-notes --role model --kind thinking --content 'The harness has three parts: a compositor, a kernel, and the app.'" \
    "kj block create -c alpha-notes --role model --kind text --content-type text/markdown --content '$md'" \
    "kj block create -c alpha-notes --role model --kind text --content-type text/vnd.abc --content '$abc'" \
    "kj block create -c alpha-notes --role user --kind text --content 'Now list the contexts.'" \
    "kj block create -c alpha-notes --role model --kind text --content 'There are four: alpha-notes, beta-review, gamma-scratch, and the admin seat.'" \
    "kj block create -c beta-review --role user --kind text --content 'Review the diff for the shader port.'" \
    "kj block create -c beta-review --role model --kind text --content 'The WESL imports resolve; the materials bind group 2 matches.'" \
    "kj block create -c gamma-scratch --role system --kind notification --content 'scratch context created by the headless harness'" \
    >/dev/null || die "seeding failed"

  # The ask: `git push` is on gate.toml's ask list. The call parks until the
  # reviewer answers; we leave after it is raised and the ask stays pending.
  echo "== raise one pending ask"
  (timeout 8 python3 "$REPO_ROOT/contrib/bench/kjmcp.py" --binary "$MCP_BIN" \
    --port "$KERNEL_PORT" --key-file "$key" --home "$STATE/home" --insecure \
    --context headless-admin "git push origin headless-harness" \
    >"$STATE/logs/ask.log" 2>&1 || true)

  # --- app ------------------------------------------------------------------
  echo "== launch app"
  env WAYLAND_DISPLAY="$wayland" BRP_EXTRAS_PORT="$BRP_PORT" \
    XDG_DATA_HOME="$STATE/xdg/data" XDG_CONFIG_HOME="$STATE/xdg/config" \
    XDG_STATE_HOME="$STATE/xdg/state" \
    RUST_LOG="${KJ_HEADLESS_APP_LOG:-info,wgpu=warn,naga=warn,bevy_render=info}" \
    "$APP_BIN" --host 127.0.0.1 --port "$KERNEL_PORT" --insecure --key-file "$key" \
    --maximize >"$STATE/logs/app.log" 2>&1 &
  echo $! > "$STATE/app.pid"
  for _ in $(seq 1 60); do
    kill -0 "$(cat "$STATE/app.pid")" 2>/dev/null \
      || { tail -20 "$STATE/logs/app.log" >&2; die "app died during startup"; }
    port_is_listening "$BRP_PORT" && break
    sleep 1
  done
  port_is_listening "$BRP_PORT" || die "app never opened BRP on $BRP_PORT"

  cat > "$STATE/env.sh" <<ENV
# Written by contrib/headless-app.sh start
WAYLAND_NAME="$wayland"
LABWC_STARTED=$labwc_started
KERNEL_PORT=$KERNEL_PORT
BRP_PORT=$BRP_PORT
ENV
  cmd_status
}

cmd_status() {
  load_env
  echo "state dir     $STATE"
  echo "wayland       $WAYLAND_NAME (labwc ours: $LABWC_STARTED)"
  echo "kernel        port $KERNEL_PORT, pid $(cat "$STATE/kernel.pid" 2>/dev/null || echo '-')"
  echo "app           pid $(cat "$STATE/app.pid" 2>/dev/null || echo '-'), BRP http://127.0.0.1:$BRP_PORT"
  echo "key           $STATE/keys/$DIRECTOR"
  echo "logs          $STATE/logs/{app,kernel,labwc,ask}.log"
}

cmd_shot() {
  load_env
  local name="${1:?usage: shot NAME [DIR]}" dir="${2:-$STATE/shots}"
  mkdir -p "$dir"
  local out="$dir/$name.png"
  rm -f "$out"
  brp brp_extras/screenshot "{\"path\":\"$out\"}"
  echo
  for _ in $(seq 1 20); do
    [[ -s "$out" ]] && { echo "$out"; return 0; }
    sleep 0.25
  done
  die "no screenshot written to $out"
}

cmd_keys() {
  load_env
  [[ $# -gt 0 ]] || die "usage: keys KEY..."
  local list
  list="$(printf '"%s",' "$@")"
  brp brp_extras/send_keys "{\"keys\":[${list%,}]}"
  echo
}

cmd_text() {
  load_env
  [[ $# -eq 1 ]] || die "usage: text STRING"
  local json
  json="$(python3 -c 'import json,sys; print(json.dumps({"text": sys.argv[1]}))' "$1")"
  brp brp_extras/type_text "$json"
  echo
}

cmd_stop() {
  # Stop what we started; a missing state dir is not an error.
  if [[ -d "$STATE" ]]; then
    stop_pid "$STATE/app.pid" kaijutsu-app
    stop_pid "$STATE/kernel.pid" kaijutsu-server
    if [[ -f "$STATE/env.sh" ]] && grep -q '^LABWC_STARTED=1' "$STATE/env.sh" \
       || [[ -f "$STATE/labwc.pid" ]]; then
      stop_pid "$STATE/labwc.pid" labwc
    fi
    rm -rf "$STATE/xdg" "$STATE/home" "$STATE/config" "$STATE/keys" "$STATE/app" \
      "$STATE/labwc-cfg" "$STATE/env.sh"
    echo "stopped; logs and shots kept in $STATE/logs and $STATE/shots"
  else
    echo "nothing to stop"
  fi
}

case "${1:-}" in
  start) shift; cmd_start "$@" ;;
  status) cmd_status ;;
  shot) shift; cmd_shot "$@" ;;
  keys) shift; cmd_keys "$@" ;;
  text) shift; cmd_text "$@" ;;
  logs) echo "$STATE/logs" ;;
  stop) cmd_stop ;;
  *) sed -n '2,25p' "${BASH_SOURCE[0]}"; exit 2 ;;
esac

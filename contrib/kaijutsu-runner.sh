#!/bin/bash
# Kaijutsu Runner - run in the graphical session (Moonlight/Konsole)
# Wraps cargo watch with outer restart loop and control files
#
# ═══════════════════════════════════════════════════════════════════
# FOR CLAUDE: Autonomous Development Loop
# ═══════════════════════════════════════════════════════════════════
#
# This script runs in a SEPARATE graphical session (Moonlight/Wayland).
# You CANNOT launch it directly - the human starts it in Konsole.
#
# YOUR WORKFLOW:
#   1. Edit code normally with Edit/Write tools
#   2. cargo watch detects changes → auto rebuilds → auto restarts
#   3. Use BRP tools (mcp__bevy_brp__*) to inspect running app
#   4. Use ./contrib/kj commands to control the runner:
#
#      ./contrib/kj status   - check if runner is active
#      ./contrib/kj pause    - pause watch (app keeps running for BRP)
#      ./contrib/kj resume   - resume watching
#      ./contrib/kj rebuild  - force clean rebuild (cargo clean first)
#      ./contrib/kj restart  - restart cargo watch
#      ./contrib/kj tail     - follow runner output
#
# CHECKING BUILD RESULTS:
#   - ./contrib/kj tail     - see live output
#   - ./contrib/kj log      - see full typescript
#   - cat /tmp/kj.status    - quick state check
#
# IF APP CRASHES:
#   - cargo watch auto-restarts after 2s delay
#   - all output captured in /tmp/kaijutsu-runner.typescript
#
# IF USER QUITS (q):
#   - Creates /tmp/kj.noloop to prevent auto-restart
#   - Use ./contrib/kj restart to start again
#
# ═══════════════════════════════════════════════════════════════════
#
# Usage: ./contrib/kaijutsu-runner.sh [--release] [app args...]
#   --release is consumed by the runner; every other arg is passed through to
#   kaijutsu-app verbatim, e.g.:
#     ./contrib/kaijutsu-runner.sh --host 192.168.1.4 --port 2222
#
# Control files (touch to trigger):
#   /tmp/kj.pause    - pause watching (remove to resume)
#   /tmp/kj.rebuild  - force full rebuild (clean + build)
#   /tmp/kj.restart  - restart cargo watch loop
#   /tmp/kj.stop     - stop everything and exit
#   /tmp/kj.noloop   - auto-created on clean exit, blocks auto-restart
#
# Output captured to /tmp/kaijutsu-runner.typescript via script(1)

set -uo pipefail

CTRL_PAUSE="/tmp/kj.pause"
CTRL_REBUILD="/tmp/kj.rebuild"
CTRL_RESTART="/tmp/kj.restart"
CTRL_STOP="/tmp/kj.stop"
CTRL_NOLOOP="/tmp/kj.noloop"
STATUS_FILE="/tmp/kj.status"
TYPESCRIPT="/tmp/kaijutsu-runner.typescript"
PROJECT_DIR="/home/atobey/src/kaijutsu"

PROFILE="debug"
CARGO_PROFILE_FLAG=""
# dynamic_linking compiles Bevy as a shared lib for fast incremental relinks —
# debug-only, so release builds stay self-contained.
CARGO_FEATURES_FLAG="--features dynamic_linking"

# --release is ours; everything else passes through to kaijutsu-app.
APP_ARGS=()
for arg in "$@"; do
    if [[ "$arg" == "--release" ]]; then
        PROFILE="release" CARGO_PROFILE_FLAG="--release" CARGO_FEATURES_FLAG=""
    else
        APP_ARGS+=("$arg")
    fi
done
# Shell-quoted string form for the two places that need a single string:
# the script(1) re-exec and the `cargo watch -x` command.
APP_ARGS_STR=""
[[ ${#APP_ARGS[@]} -gt 0 ]] && APP_ARGS_STR=$(printf '%q ' "${APP_ARGS[@]}")

# If not already inside script(1), re-exec under it
if [[ -z "${SCRIPT_WRAPPER:-}" ]]; then
    export SCRIPT_WRAPPER=1
    echo "📜 Wrapping in script(1) → $TYPESCRIPT"
    exec script -f -q -c "$(printf '%q ' "$0" "$@")" "$TYPESCRIPT"
fi

WATCH_PID=""

log() {
    echo -e "\033[36m[$(date '+%H:%M:%S')]\033[0m $*"
}

status() {
    local state="$1"
    local msg="$2"
    echo "state=$state msg=\"$msg\" pid=${WATCH_PID:-none} ts=$(date -Iseconds)" > "$STATUS_FILE"
    log "📊 $state: $msg"
}

cleanup() {
    log "🧹 Cleaning up..."
    [[ -n "$WATCH_PID" ]] && kill "$WATCH_PID" 2>/dev/null
    pkill -f "target/$PROFILE/kaijutsu-app" 2>/dev/null || true
    rm -f "$CTRL_PAUSE" "$CTRL_REBUILD" "$CTRL_RESTART" "$CTRL_STOP" "$CTRL_NOLOOP"
    status "stopped" "Runner exited"
    exit 0
}

trap cleanup SIGINT SIGTERM

start_watch() {
    cd "$PROJECT_DIR"
    status "starting" "Launching cargo watch"

    # Kill any existing
    [[ -n "$WATCH_PID" ]] && kill "$WATCH_PID" 2>/dev/null || true
    pkill -f "target/$PROFILE/kaijutsu-app" 2>/dev/null || true
    sleep 0.5

    export RUST_LOG="${RUST_LOG:-debug,wgpu=warn}"
    # Always include http:// scheme — tonic gRPC requires it
    export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
    export OTEL_EXPORTER_OTLP_PROTOCOL="${OTEL_EXPORTER_OTLP_PROTOCOL:-grpc}"
    export OTEL_SERVICE_NAME="${OTEL_SERVICE_NAME:-kaijutsu-app}"

    local run_cmd="run -p kaijutsu-app $CARGO_PROFILE_FLAG $CARGO_FEATURES_FLAG"
    [[ -n "$APP_ARGS_STR" ]] && run_cmd="$run_cmd -- $APP_ARGS_STR"

    cargo watch \
        -x "$run_cmd" \
        -w crates/kaijutsu-app \
        -w crates/kaijutsu-client \
        --why \
        -c &

    WATCH_PID=$!
    log "🚀 cargo watch started (PID $WATCH_PID)"
    status "running" "Watching for changes"
}

full_rebuild() {
    log "🔨 Full rebuild requested"
    [[ -n "$WATCH_PID" ]] && kill "$WATCH_PID" 2>/dev/null || true
    pkill -f "target/$PROFILE/kaijutsu-app" 2>/dev/null || true

    cd "$PROJECT_DIR"
    status "building" "Clean rebuild in progress"
    cargo clean -p kaijutsu-app
    cargo build -p kaijutsu-app $CARGO_PROFILE_FLAG $CARGO_FEATURES_FLAG

    rm -f "$CTRL_REBUILD" "$CTRL_NOLOOP"
    start_watch
}

# ═══════════════════════════════════════════════════════════════════
log "═══════════════════════════════════════════════════════════════"
log "🎮 Kaijutsu Runner"
log "   Profile: $PROFILE"
log "   Project: $PROJECT_DIR"
log "   Output:  $TYPESCRIPT"
[[ -n "$APP_ARGS_STR" ]] && log "   App args: $APP_ARGS_STR"
log ""
log "   Control files:"
log "     touch $CTRL_PAUSE   → pause"
log "     touch $CTRL_REBUILD → clean rebuild"
log "     touch $CTRL_RESTART → restart watch"
log "     touch $CTRL_STOP    → stop & exit"
log "   On clean exit (q), creates $CTRL_NOLOOP to prevent auto-restart"
log "═══════════════════════════════════════════════════════════════"

# Clear old control files
rm -f "$CTRL_PAUSE" "$CTRL_REBUILD" "$CTRL_RESTART" "$CTRL_STOP" "$CTRL_NOLOOP"

start_watch

# Outer control loop
while true; do
    # Stop requested?
    if [[ -f "$CTRL_STOP" ]]; then
        log "🛑 Stop requested"
        cleanup
    fi

    # Full rebuild requested?
    if [[ -f "$CTRL_REBUILD" ]]; then
        full_rebuild
    fi

    # Restart watch requested?
    if [[ -f "$CTRL_RESTART" ]]; then
        log "🔄 Restart requested"
        rm -f "$CTRL_RESTART" "$CTRL_NOLOOP"
        start_watch
    fi

    # Pause/resume
    if [[ -f "$CTRL_PAUSE" ]]; then
        if [[ -n "$WATCH_PID" ]] && kill -0 "$WATCH_PID" 2>/dev/null; then
            log "⏸️  Pausing (kill watch, app keeps running)"
            kill "$WATCH_PID" 2>/dev/null || true
            WATCH_PID=""
            status "paused" "Remove $CTRL_PAUSE to resume"
        fi
    else
        # Resume if paused
        if [[ -z "$WATCH_PID" ]] || ! kill -0 "$WATCH_PID" 2>/dev/null; then
            if [[ ! -f "$CTRL_PAUSE" ]]; then
                log "▶️  Resuming watch"
                start_watch
            fi
        fi
    fi

    # Check if cargo watch exited
    if [[ -n "$WATCH_PID" ]] && ! kill -0 "$WATCH_PID" 2>/dev/null; then
        wait "$WATCH_PID" 2>/dev/null
        EXIT_CODE=$?
        WATCH_PID=""

        if [[ -f "$CTRL_NOLOOP" ]]; then
            log "🛑 Noloop set, staying stopped (rm $CTRL_NOLOOP to allow restart)"
            status "stopped" "Noloop active"
        elif [[ $EXIT_CODE -eq 0 ]]; then
            # Clean exit (user quit with 'q') - pause and wait
            log "✅ App exited cleanly (code 0)"
            log "   → touch $CTRL_RESTART to restart, or $CTRL_STOP to exit"
            touch "$CTRL_NOLOOP"
            status "stopped" "Clean exit, touch /tmp/kj.restart to restart"
        else
            # Crash or error - auto restart after delay
            log "⚠️  cargo watch exited with code $EXIT_CODE, restarting in 2s..."
            status "restarting" "Watch crashed (code $EXIT_CODE), restarting"
            sleep 2
            start_watch
        fi
    fi

    sleep 1
done

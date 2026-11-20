#!/bin/bash
# Install kaijutsu-server on macOS as a per-user LaunchAgent: the macOS
# counterpart of install-systemd.sh. Run as yourself, not root, from the
# shell whose PATH the kernel's shell should see. Safe to re-run: it rebuilds,
# copies the binary, rewrites the LaunchAgent, and restarts the service.
#
# The service runs a copy in ~/bin, so rebuilding the repo does not change
# the running kernel until you re-run this script.
#
# Set up the kernel first, with the service stopped (`kaijutsu-server
# bootstrap`, see README.md); this script does not touch kernel data. The
# service sees only PATH, RUST_LOG and any XDG_DATA_HOME or XDG_CONFIG_HOME
# from this shell, so a backend's key must come from a file
# (`kj backend set --api-key-file`), not an environment variable.
#
# launchd restarts a failing server every 5 seconds with no limit; if the log
# repeats one error, stop the agent and fix it.
#
# Overrides:
#   PORT=2222        SSH port
#   RUST_LOG=info    log filter
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
RUST_LOG="${RUST_LOG:-info}"
PORT="${PORT:-2222}"

LABEL="local.kaijutsu-server"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/kaijutsu-server.log"
BIN="$HOME/bin/kaijutsu-server"
DOMAIN="gui/$(id -u)"

die() { echo "error: $*" >&2; exit 1; }
xml() { local s="${1//&/&amp;}"; s="${s//</&lt;}"; printf '%s' "${s//>/&gt;}"; }
step() { echo; echo "--- $* ---"; }

[[ "$(uname -s)" == Darwin ]] || die "this script is for macOS; see install-systemd.sh"
[[ "$(id -u)" -ne 0 ]] || die "run as your own user; the LaunchAgent lives in your GUI session"

step "Building kaijutsu-server (release)"
cargo build --release -p kaijutsu-server --manifest-path "$REPO_DIR/Cargo.toml"

step "Writing $PLIST"
mkdir -p "$(dirname "$PLIST")" "$(dirname "$LOG")"
# launchd starts jobs with a bare PATH; the kernel's shell runs host commands
# from this one. The server reads its data and config under the XDG homes.
env_entries=""
for var in PATH RUST_LOG XDG_DATA_HOME XDG_CONFIG_HOME; do
    [[ -n "${!var:-}" ]] || continue
    env_entries+="        <key>$var</key>"$'\n'"        <string>$(xml "${!var}")</string>"$'\n'
done
# Written beside the old plist and checked before anything is stopped.
cat > "$PLIST.new" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$(xml "$BIN")</string>
        <string>--port</string>
        <string>$PORT</string>
    </array>
    <key>WorkingDirectory</key>
    <string>$(xml "$HOME")</string>
    <key>EnvironmentVariables</key>
    <dict>
$env_entries    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>StandardOutPath</key>
    <string>$(xml "$LOG")</string>
    <key>StandardErrorPath</key>
    <string>$(xml "$LOG")</string>
</dict>
</plist>
EOF
plutil -lint "$PLIST.new" >/dev/null || die "generated plist is invalid: $PLIST.new"

step "Stopping the existing agent"
if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    launchctl bootout "$DOMAIN/$LABEL"
    # bootout returns before launchd has removed the job, and bootstrapping
    # a job that is still registered fails with an I/O error.
    for _ in $(seq 1 40); do
        launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || break
        sleep 0.5
    done
    launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 \
        && die "$LABEL is still registered 20s after bootout; check 'launchctl print $DOMAIN/$LABEL'"
else
    echo "not running"
fi
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    lsof -nP -iTCP:"$PORT" -sTCP:LISTEN
    die "something else is listening on port $PORT (a kaijutsu-server started by hand?)"
fi

step "Installing $BIN"
mkdir -p "$(dirname "$BIN")"
# Copy then rename, so a running copy is never overwritten in place.
cp "$REPO_DIR/target/release/kaijutsu-server" "$BIN.new"
mv "$BIN.new" "$BIN"

mv "$PLIST.new" "$PLIST"

step "Starting the agent"
launchctl bootstrap "$DOMAIN" "$PLIST"
for _ in $(seq 1 60); do
    lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1 && break
    sleep 0.5
done
lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1 \
    || die "kaijutsu-server is not listening on port $PORT after 30s; see $LOG"
echo "listening on port $PORT"

cat <<EOF

=== Done ===
kaijutsu-server runs from $BIN (log: $LOG). It starts at login and
restarts if it exits with an error. Clients connect on port $PORT.

Stop:      launchctl bootout $DOMAIN/$LABEL
Start:     launchctl bootstrap $DOMAIN $PLIST
Restart:   launchctl kickstart -k $DOMAIN/$LABEL
Logs:      tail -f $LOG
Uninstall: launchctl bootout $DOMAIN/$LABEL; rm $PLIST $BIN
EOF

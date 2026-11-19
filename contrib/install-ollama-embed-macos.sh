#!/bin/bash
# Install Ollama on macOS as a per-user LaunchAgent and pull EmbeddingGemma 2
# for the kernel's semantic index. Run as yourself, not root. Safe to re-run:
# it rewrites the LaunchAgent, restarts the service, and re-verifies.
#
# The LaunchAgent is ours rather than `brew services`, because the Homebrew
# plist takes no environment and we pin the listen address and keep-alive.
#
# Overrides:
#   MODEL=embeddinggemma-2:740m-q8_0   another tag from ollama.com/library/embeddinggemma-2
#   PORT=11434                          listen port on 127.0.0.1
#   KEEP_ALIVE=24h                      how long an idle model stays loaded (-1 = forever)
set -euo pipefail

# The text-only build: the kernel embeds text, so the vision and audio
# encoders would only cost memory. Text vectors come from the same weights.
MODEL="${MODEL:-embeddinggemma-2:270m-q8_0-text}"
PORT="${PORT:-11434}"
KEEP_ALIVE="${KEEP_ALIVE:-24h}"
# EmbeddingGemma 2's native width; it also serves 512, 256 and 128 (MRL).
DIMENSIONS=768

LABEL="local.ollama"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/ollama.log"
BASE="http://127.0.0.1:$PORT"
DOMAIN="gui/$(id -u)"

die() { echo "error: $*" >&2; exit 1; }
step() { echo; echo "--- $* ---"; }

[[ "$(uname -s)" == Darwin ]] || die "this script is for macOS"
[[ "$(id -u)" -ne 0 ]] || die "run as your own user; the LaunchAgent lives in your GUI session"
command -v brew >/dev/null || die "Homebrew is not installed (https://brew.sh)"
command -v python3 >/dev/null || die "python3 is needed to check the embedding response"

step "Installing ollama"
if brew list --formula ollama >/dev/null 2>&1; then
    echo "ollama formula already installed; upgrading if outdated"
    brew upgrade ollama
else
    brew install ollama
fi
OLLAMA_BIN="$(brew --prefix)/bin/ollama"
[[ -x "$OLLAMA_BIN" ]] || die "expected $OLLAMA_BIN after brew install"
"$OLLAMA_BIN" --version

step "Checking for another ollama"
# Two servers on one port means one silently fails to bind, and the client
# talks to whichever won. Refuse rather than guess.
# Capture first: a `| grep -q` would close the pipe early, and under
# pipefail the writer's SIGPIPE would turn a match into a miss.
services="$(brew services list)" || die "brew services list failed"
if awk '$1 == "ollama" && $2 == "started" { found = 1 } END { exit !found }' <<<"$services"; then
    die "ollama is already running under brew services; run 'brew services stop ollama' and re-run"
fi
if pgrep -xq Ollama; then
    die "the Ollama desktop app is running; quit it (and remove it from Login Items) and re-run"
fi
if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    echo "stopping the existing $LABEL agent"
    launchctl bootout "$DOMAIN/$LABEL"
    # bootout returns before launchd has removed the job, and bootstrapping
    # a job that is still registered fails with an I/O error.
    for _ in $(seq 1 40); do
        launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || break
        sleep 0.5
    done
    launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 \
        && die "$LABEL is still registered 20s after bootout; check 'launchctl print $DOMAIN/$LABEL'"
fi
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    lsof -nP -iTCP:"$PORT" -sTCP:LISTEN
    die "something else is listening on port $PORT"
fi

step "Writing $PLIST"
mkdir -p "$(dirname "$PLIST")" "$(dirname "$LOG")"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$OLLAMA_BIN</string>
        <string>serve</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>OLLAMA_HOST</key>
        <string>127.0.0.1:$PORT</string>
        <key>OLLAMA_KEEP_ALIVE</key>
        <string>$KEEP_ALIVE</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>$LOG</string>
    <key>StandardErrorPath</key>
    <string>$LOG</string>
</dict>
</plist>
EOF
plutil -lint "$PLIST"

step "Starting the agent"
launchctl bootstrap "$DOMAIN" "$PLIST"
for _ in $(seq 1 60); do
    curl -fsS "$BASE/api/version" >/dev/null 2>&1 && break
    sleep 0.5
done
curl -fsS "$BASE/api/version" || die "ollama did not answer at $BASE within 30s; see $LOG"
echo

step "Pulling $MODEL"
OLLAMA_HOST="127.0.0.1:$PORT" "$OLLAMA_BIN" pull "$MODEL"

step "Verifying /v1/embeddings"
# The request the kernel's embedder sends: a batch carrying both profile
# prefixes (its boot probe is a document), `dimensions`, and float encoding.
# It refuses any vector of another width. Prints the widths, one per input.
check_width() {
    local want="$1"
    curl -fsS "$BASE/v1/embeddings" -H 'content-type: application/json' \
        -d "{\"model\":\"$MODEL\",\"input\":[\"task: search result | query: hello\",\"title: none | text: dimension probe\"],\"dimensions\":$want,\"encoding_format\":\"float\"}" \
        | python3 -c 'import json,sys; print(" ".join(str(len(d["embedding"])) for d in json.load(sys.stdin)["data"]))'
}
got="$(check_width "$DIMENSIONS")" || die "embedding request failed; see $LOG"
[[ "$got" == "$DIMENSIONS $DIMENSIONS" ]] || die "asked for two $DIMENSIONS-dimension vectors, got widths: $got"
echo "$DIMENSIONS dimensions: ok"
# Advisory only: a model may refuse a width it does not serve.
got="$(check_width 512)" || got="a failed request"
if [[ "$got" == "512 512" ]]; then
    echo "512 dimensions: ok (truncation honored; smaller widths are available)"
else
    echo "512 dimensions: got $got (truncation ignored; keep dimensions at $DIMENSIONS)"
fi

cat <<EOF

=== Done ===
ollama serves $BASE (log: $LOG). It starts at login and restarts if it exits.

Point the kernel at it, then restart kaijutsu-server (it reads the setting
when it starts):

  kj backend set local-embed --kind openai --base-url $BASE/v1 --key-optional
  kj backend model set local-embed $MODEL --embedding-profile embeddinggemma
  kj embedding set local-embed/$MODEL --dimensions $DIMENSIONS
  kj embedding show

The 'embeddinggemma' profile adds EmbeddingGemma's task prefixes, so Ollama
must receive text as-is.

Stop:      launchctl bootout $DOMAIN/$LABEL
Start:     launchctl bootstrap $DOMAIN $PLIST
Uninstall: launchctl bootout $DOMAIN/$LABEL; rm $PLIST; brew uninstall ollama
EOF

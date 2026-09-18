#!/usr/bin/env bash
# The launcher an ACP client execs. stdout is the ACP wire; nothing else may
# write to it, so every diagnostic here goes to stderr.
#
# The Harbor runner takes this path as --launcher and speaks JSON-RPC to
# whatever it execs. Read the kernel's coordinates from the environment so the
# same launcher serves every run:
#
#   KJ_BENCH_PORT       kernel SSH port
#   KJ_BENCH_KEY        private key of the connecting (reviewing) character
#   KJ_BENCH_CHARACTER  performer character for each new session
#   KJ_BENCH_ACP_BIN    kaijutsu-acp binary (default: the worktree's debug build)
#   KJ_BENCH_CLIENT_HOME  HOME for the bridge, so nothing it writes lands in
#                         the operator's own dotfiles
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ACP_BIN="${KJ_BENCH_ACP_BIN:-$REPO_ROOT/target/debug/kaijutsu-acp}"

: "${KJ_BENCH_PORT:?source the run directory env.sh first}"
: "${KJ_BENCH_KEY:?source the run directory env.sh first}"
: "${KJ_BENCH_CHARACTER:?source the run directory env.sh first}"

# `--insecure` already stops this bridge consulting or learning known_hosts
# (crates/kaijutsu-client/src/ssh.rs, `check_server_key` returns before the
# TOFU branch). Redirecting HOME as well means a future client path that does
# reach for a dotfile still cannot reach the operator's.
if [[ -n "${KJ_BENCH_CLIENT_HOME:-}" ]]; then
  export HOME="$KJ_BENCH_CLIENT_HOME"
fi

echo "acp-launch: kernel localhost:$KJ_BENCH_PORT as $KJ_BENCH_CHARACTER" >&2

exec "$ACP_BIN" \
  --connect \
  --host localhost \
  --port "$KJ_BENCH_PORT" \
  --key-file "$KJ_BENCH_KEY" \
  --insecure \
  --context-type "${KJ_BENCH_CONTEXT_TYPE:-coder}" \
  --character "$KJ_BENCH_CHARACTER"

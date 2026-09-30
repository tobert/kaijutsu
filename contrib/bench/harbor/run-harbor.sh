#!/usr/bin/env bash
# Run one Harbor job with kaijutsu-solo-acp as the agent, under rootless podman.
#
# The provider key is read from its file into this shell's environment and
# handed to Harbor as the template `${DEEPSEEK_API_KEY}`, which Harbor resolves
# from our environment at run time and writes back to config.json verbatim.
# The key itself never reaches a command line, a file this script writes, or a
# job-directory JSON. The run ends with a scan that says so, or aborts.
#
# Requires bash >= 4.4 (empty-array expansion under `set -u`) and the Python
# Harbor runs on (>= 3.12).
set -euo pipefail

# xtrace would print the key twice: the line that reads it, and the line that
# writes the scan pattern. Refuse rather than leak into a terminal or a log.
if [[ $- == *x* ]]; then
  echo "refusing to run under 'set -x': this script handles a provider key" >&2
  exit 2
fi

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

: "${HARBOR_ENV_SH:=/home/atobey/src/bench-work/harbor/env.sh}"
: "${KAIJUTSU_ACP_BINARY:=/home/atobey/src/bench-work/dist/out/kaijutsu-solo-acp}"
: "${KAIJUTSU_ACP_GATE:=${here}/../gate-sandbox.toml}"
# Model and backend defaults live in kaijutsu_solo_agent.py (DEFAULT_MODEL,
# DEFAULT_BACKEND_KIND). Leaving KAIJUTSU_ACP_MODEL / KAIJUTSU_ACP_BACKEND
# unset lets that one owner decide.
: "${KAIJUTSU_ACP_RUST_LOG:=info}"
: "${KAIJUTSU_ACP_KEY_FILE:=${HOME}/.deepseek-key}"
: "${KAIJUTSU_ACP_KEY_ENV:=DEEPSEEK_API_KEY}"
: "${HARBOR_JOBS_DIR:=/home/atobey/src/bench-work/harbor/jobs}"
: "${HARBOR_AGENT_TIMEOUT_MULTIPLIER:=5}"
: "${HARBOR_CONCURRENCY:=1}"

usage() {
  cat <<'USAGE'
usage: run-harbor.sh --job-name NAME (--task-path DIR | --dataset REF)
                     [--task NAME]... [--task-file FILE] [--] [harbor args...]

  --job-name NAME     Job directory name under $HARBOR_JOBS_DIR. Must not exist.
  --task-path DIR     A standalone task directory (harbor run -p).
  --dataset REF       A dataset reference, e.g. terminal-bench@2.0 (harbor run -d).
  --task NAME         Task name to include from the dataset. Repeatable.
  --task-file FILE    File of task names, one per line; '#' comments allowed.
  --                  Everything after this is passed to `harbor run` as-is.

Environment (all have defaults):
  KAIJUTSU_ACP_BINARY KAIJUTSU_ACP_GATE KAIJUTSU_ACP_RUST_LOG
  KAIJUTSU_ACP_MODEL KAIJUTSU_ACP_BACKEND (unset: kaijutsu_solo_agent.py decides)
  KAIJUTSU_ACP_BASE_URL (unset: the provider's own endpoint)
  KAIJUTSU_ACP_KEY_FILE KAIJUTSU_ACP_KEY_ENV (the variable the binary reads)
  KAIJUTSU_ACP_NO_KEY=1 (a local server with no key; needs KAIJUTSU_ACP_BASE_URL)
  KAIJUTSU_ACP_IDLE_TIMEOUT KAIJUTSU_ACP_REQUEST_TIMEOUT (seconds; unset: the kernel's)
  HARBOR_ENV_SH HARBOR_JOBS_DIR HARBOR_AGENT_TIMEOUT_MULTIPLIER
  HARBOR_CONCURRENCY (trials at once; default 1)
USAGE
}

job_name=""
task_path=""
dataset=""
task_names=()
passthrough=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --job-name) job_name="${2:?--job-name needs a value}"; shift 2 ;;
    --task-path) task_path="${2:?--task-path needs a value}"; shift 2 ;;
    --dataset) dataset="${2:?--dataset needs a value}"; shift 2 ;;
    --task) task_names+=("${2:?--task needs a value}"); shift 2 ;;
    --task-file)
      file="${2:?--task-file needs a value}"
      [[ -f "$file" ]] || { echo "no such task file: $file" >&2; exit 1; }
      while IFS= read -r line; do
        line="${line%%#*}"
        line="$(printf '%s' "$line" | tr -d '[:space:]')"
        [[ -n "$line" ]] && task_names+=("$line")
      done < "$file"
      shift 2
      ;;
    --) shift; passthrough=("$@"); break ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$job_name" ]] || { echo "--job-name is required" >&2; usage >&2; exit 2; }
if [[ -n "$task_path" && -n "$dataset" ]]; then
  echo "--task-path and --dataset are mutually exclusive" >&2
  exit 1
fi
if [[ -z "$task_path" && -z "$dataset" ]]; then
  echo "one of --task-path or --dataset is required" >&2
  usage >&2
  exit 2
fi
[[ -f "$HARBOR_ENV_SH" ]] || { echo "no harbor env: $HARBOR_ENV_SH" >&2; exit 1; }

# Sourced BEFORE the path checks below: env.sh may set or change any of these,
# and a check above it would validate a value that never takes effect.
# shellcheck source=/dev/null
source "$HARBOR_ENV_SH"

# `podman compose` prints ">>>> Executing external compose provider ... <<<<"
# on every invocation. Harbor folds a compose exec's stderr into its stdout, so
# that banner lands in the output of commands it parses -- `uname -s && uname
# -m` in AcpAgent._detect_platform is the first casualty. Silence it at the
# source rather than teaching every parser to skip a line.
export PODMAN_COMPOSE_WARNING_LOGS=false

[[ -f "$KAIJUTSU_ACP_BINARY" ]] || { echo "no agent binary: $KAIJUTSU_ACP_BINARY" >&2; exit 1; }
[[ -f "$KAIJUTSU_ACP_GATE" ]] || { echo "no gate policy: $KAIJUTSU_ACP_GATE" >&2; exit 1; }
# KAIJUTSU_ACP_NO_KEY=1 runs a local server that takes no key: no key is read,
# none is handed to Harbor, and there is nothing to scan for.
no_key=0
[[ "${KAIJUTSU_ACP_NO_KEY:-}" == 1 ]] && no_key=1
if (( no_key )); then
  [[ -n "${KAIJUTSU_ACP_BASE_URL:-}" ]] || { echo "KAIJUTSU_ACP_NO_KEY=1 needs KAIJUTSU_ACP_BASE_URL" >&2; exit 1; }
  unset KAIJUTSU_ACP_KEY_ENV
else
  [[ -f "$KAIJUTSU_ACP_KEY_FILE" ]] || { echo "no key file: $KAIJUTSU_ACP_KEY_FILE" >&2; exit 1; }
fi
# Provenance should name one path, not one path plus how this script spelled it.
KAIJUTSU_ACP_BINARY="$(realpath -- "$KAIJUTSU_ACP_BINARY")"
KAIJUTSU_ACP_GATE="$(realpath -- "$KAIJUTSU_ACP_GATE")"

job_dir="${HARBOR_JOBS_DIR}/${job_name}"
# The scan below covers exactly this directory. If it already exists, the scan
# would cover somebody else's output and this run's would be mixed into it.
if [[ -e "$job_dir" ]]; then
  echo "job directory already exists: $job_dir" >&2
  echo "pick another --job-name; this script scans that directory for the key" >&2
  exit 1
fi
mkdir -p "$HARBOR_JOBS_DIR"

key_args=()
if (( ! no_key )); then
  # The scan pattern is read from a mode-600 file of our own rather than
  # /dev/stdin, so "could not supply the pattern" is distinguishable from
  # "could not search".
  scan_dir="$(umask 077; mktemp -d "${HARBOR_JOBS_DIR}/.keyscan-${job_name}.XXXXXX")"
  pattern_file="${scan_dir}/pattern"
  cleanup() { rm -f -- "$pattern_file"; rmdir -- "$scan_dir" 2>/dev/null || true; }
  trap cleanup EXIT

  # Read the key into the environment and into the pattern file. Never echoed,
  # never on a command line: only its variable NAME travels, inside a Harbor env
  # template. xtrace is already refused above; belt and braces here.
  { set +x; } 2>/dev/null
  printf -v "$KAIJUTSU_ACP_KEY_ENV" '%s' "$(< "$KAIJUTSU_ACP_KEY_FILE")"
  export "${KAIJUTSU_ACP_KEY_ENV?}"
  [[ -n "${!KAIJUTSU_ACP_KEY_ENV}" ]] || { echo "key file is empty: $KAIJUTSU_ACP_KEY_FILE" >&2; exit 1; }
  ( umask 077; printf '%s\n' "${!KAIJUTSU_ACP_KEY_ENV}" > "$pattern_file" )

  # KAIJUTSU_ACP_KEY_ENV also reaches the adapter, which passes it to the
  # binary as --api-key-env: the variable Harbor puts in the container is the
  # one the kernel reads, whatever the provider calls its own.
  export KAIJUTSU_ACP_KEY_ENV
  key_args=(--ae "${KAIJUTSU_ACP_KEY_ENV}=\${${KAIJUTSU_ACP_KEY_ENV}}")
else
  export KAIJUTSU_ACP_NO_KEY
fi

export KAIJUTSU_ACP_BINARY KAIJUTSU_ACP_GATE KAIJUTSU_ACP_RUST_LOG
[[ -n "${KAIJUTSU_ACP_BASE_URL:-}" ]] && export KAIJUTSU_ACP_BASE_URL
[[ -n "${KAIJUTSU_ACP_IDLE_TIMEOUT:-}" ]] && export KAIJUTSU_ACP_IDLE_TIMEOUT
[[ -n "${KAIJUTSU_ACP_REQUEST_TIMEOUT:-}" ]] && export KAIJUTSU_ACP_REQUEST_TIMEOUT
[[ -n "${KAIJUTSU_ACP_MODEL:-}" ]] && export KAIJUTSU_ACP_MODEL
[[ -n "${KAIJUTSU_ACP_BACKEND:-}" ]] && export KAIJUTSU_ACP_BACKEND
export PYTHONPATH="${here}${PYTHONPATH:+:${PYTHONPATH}}"

args=(
  run
  -a kaijutsu_solo_agent:KaijutsuSoloAcp
  -e podman
  -k 1
  -n "$HARBOR_CONCURRENCY"
  -o "$HARBOR_JOBS_DIR"
  --job-name "$job_name"
  --agent-timeout-multiplier "$HARBOR_AGENT_TIMEOUT_MULTIPLIER"
  "${key_args[@]}"
  -y
)
if [[ -n "$task_path" ]]; then
  args+=(-p "$task_path")
else
  args+=(--dataset "$dataset")
fi
for name in "${task_names[@]}"; do
  args+=(-i "$name")
done
args+=("${passthrough[@]}")

echo "harbor run -> ${job_dir}" >&2
set +e
harbor "${args[@]}"
status=$?
set -e

# The job directory is where the key would land if Harbor ever wrote it. Its
# absence means the scan covered nothing, which is not the same as clean.
if [[ ! -d "$job_dir" ]]; then
  echo "expected job directory was never created: $job_dir" >&2
  echo "nothing was scanned for the key; treat this run as unverified" >&2
  exit 4
fi

# Job-level provenance the agent cannot see: what was asked for, and by what.
harbor_version="$(harbor --version 2>/dev/null | tr -d '\n' || true)"
# Joined in bash, not with `paste`: a PATH that shadows `paste` (a clipboard
# helper, say) hangs here after Harbor has finished and before the key scan.
tasks_json=""
for name in "${task_names[@]}"; do
  [[ -n "$name" ]] && tasks_json+="${tasks_json:+,}\"${name}\""
done
cat > "${job_dir}/kaijutsu-job-provenance.json" <<JSON
{
  "written_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "job_name": "${job_name}",
  "task_path": "${task_path}",
  "dataset": "${dataset}",
  "tasks": [${tasks_json}],
  "harbor_version": "${harbor_version}",
  "agent_timeout_multiplier": "${HARBOR_AGENT_TIMEOUT_MULTIPLIER}",
  "concurrency": "${HARBOR_CONCURRENCY}",
  "binary": "${KAIJUTSU_ACP_BINARY}",
  "gate": "${KAIJUTSU_ACP_GATE}",
  "key_env": "${KAIJUTSU_ACP_KEY_ENV:-}",
  "no_key": ${no_key},
  "idle_timeout": "${KAIJUTSU_ACP_IDLE_TIMEOUT:-}",
  "request_timeout": "${KAIJUTSU_ACP_REQUEST_TIMEOUT:-}",
  "backend": "${KAIJUTSU_ACP_BACKEND:-}",
  "model": "${KAIJUTSU_ACP_MODEL:-}",
  "base_url": "${KAIJUTSU_ACP_BASE_URL:-}",
  "harbor_exit": ${status}
}
JSON

if (( no_key )); then
  echo "key scan: skipped, no key was used (${job_dir})" >&2
  exit "$status"
fi

# grep exits 0 (match), 1 (no match), >1 (error). Only 1 is "clean": an error
# reported as clean is the worst outcome here, so it aborts and says why.
# -R follows symlinked directories; a trial root may be a symlink
# (harbor/models/trial/paths.py, TrialPaths._step_path supports one), and an
# unfollowed link would be silently unscanned.
set +e
scan_out="$(grep -RlFf "$pattern_file" -- "$job_dir" 2>&1)"
scan_status=$?
set -e
case "$scan_status" in
  0)
    echo "API KEY FOUND in job output; scrub these files:" >&2
    printf '%s\n' "$scan_out" >&2
    exit 3
    ;;
  1)
    echo "key scan: clean (${job_dir})" >&2
    ;;
  *)
    echo "key scan FAILED (grep exit ${scan_status}); this run is unverified:" >&2
    printf '%s\n' "$scan_out" >&2
    exit 5
    ;;
esac

exit "$status"

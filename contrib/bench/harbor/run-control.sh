#!/usr/bin/env bash
# Run one Harbor job with a pre-integrated agent (mini-swe-agent by default,
# terminus-2 as an override) on the DeepSeek model kaijutsu runs by default,
# as a control arm for run-harbor.sh's kaijutsu-solo-acp runs.
#
# Same interface as run-harbor.sh: --job-name, --task-path | --dataset,
# --task, --task-file, and `--` passthrough to `harbor run`. Same key
# hygiene: the provider key is read from its file into this shell's
# environment and handed to Harbor as the template `${DEEPSEEK_API_KEY}`,
# which Harbor resolves from our environment at run time and writes back to
# config.json verbatim. The key itself never reaches a command line, a file
# this script writes, or a job-directory JSON. The run ends with the same
# grep-based scan run-harbor.sh uses, or aborts. See README-control.md for
# the model string, the agent's own step/cost limits and how they compare to
# ours, and what the verification run observed.
#
# Requires bash >= 4.4 (empty-array expansion under `set -u`), Python >= 3.12
# (Harbor's own floor), and python3 on PATH (used to read observed agent_info
# back out of each trial's result.json for provenance).
set -euo pipefail

# xtrace would print the key twice: the line that reads it, and the line that
# writes the scan pattern. Refuse rather than leak into a terminal or a log.
if [[ $- == *x* ]]; then
  echo "refusing to run under 'set -x': this script handles a provider key" >&2
  exit 2
fi

: "${HARBOR_ENV_SH:=/home/atobey/src/bench-work/harbor/env.sh}"
: "${HARBOR_JOBS_DIR:=/home/atobey/src/bench-work/harbor/jobs}"
: "${HARBOR_CONTROL_AGENT:=mini-swe-agent}"
: "${HARBOR_CONTROL_MODEL:=deepseek/deepseek-v4-flash}"
: "${HARBOR_CONTROL_KEY_FILE:=${HOME}/.deepseek-key}"
: "${HARBOR_CONTROL_KEY_ENV:=DEEPSEEK_API_KEY}"
: "${HARBOR_CONTROL_AGENT_TIMEOUT_MULTIPLIER:=5}"
# HARBOR_CONTROL_COST_LIMIT, HARBOR_CONTROL_REASONING_EFFORT,
# HARBOR_CONTROL_MAX_TOKENS, HARBOR_CONTROL_STEP_LIMIT are left unset here on
# purpose: leaving one unset means the *agent's own* default stands, not a
# value this script picks silently. See README-control.md, "Limits and
# fairness" for what each agent's own default actually is (mini-swe-agent's
# is unlimited steps and unlimited cost, not a small number).

usage() {
  cat <<'USAGE'
usage: run-control.sh --job-name NAME (--task-path DIR | --dataset REF)
                      [--task NAME]... [--task-file FILE] [--] [harbor args...]

  --job-name NAME     Job directory name under $HARBOR_JOBS_DIR. Must not exist.
  --task-path DIR     A standalone task directory (harbor run -p).
  --dataset REF       A dataset reference, e.g. terminal-bench@2.0 (harbor run -d).
  --task NAME         Task name to include from the dataset. Repeatable.
  --task-file FILE    File of task names, one per line; '#' comments allowed.
  --                  Everything after this is passed to `harbor run` as-is.

Environment (all have defaults):
  HARBOR_CONTROL_AGENT HARBOR_CONTROL_MODEL
  HARBOR_CONTROL_KEY_FILE HARBOR_CONTROL_KEY_ENV
  HARBOR_CONTROL_AGENT_TIMEOUT_MULTIPLIER
  HARBOR_CONTROL_COST_LIMIT HARBOR_CONTROL_REASONING_EFFORT
  HARBOR_CONTROL_MAX_TOKENS HARBOR_CONTROL_STEP_LIMIT (unset: agent's own default)
  HARBOR_ENV_SH HARBOR_JOBS_DIR
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

case "$HARBOR_CONTROL_AGENT" in
  mini-swe-agent|terminus-2) ;;
  *)
    echo "unsupported HARBOR_CONTROL_AGENT: $HARBOR_CONTROL_AGENT (mini-swe-agent or terminus-2)" >&2
    exit 2
    ;;
esac
if [[ "$HARBOR_CONTROL_MODEL" != */* ]]; then
  echo "HARBOR_CONTROL_MODEL must be 'provider/model' (litellm form), got: $HARBOR_CONTROL_MODEL" >&2
  exit 2
fi
# terminus-2 has no cost-limit or max-tokens kwarg (see README-control.md,
# "terminus-2 support"). A set-but-ignored knob is a silent fallback; refuse
# instead of pretending it took effect.
if [[ "$HARBOR_CONTROL_AGENT" == "terminus-2" ]]; then
  if [[ -n "${HARBOR_CONTROL_COST_LIMIT:-}" ]]; then
    echo "HARBOR_CONTROL_COST_LIMIT has no terminus-2 equivalent; unset it or use mini-swe-agent" >&2
    exit 2
  fi
  if [[ -n "${HARBOR_CONTROL_MAX_TOKENS:-}" ]]; then
    echo "HARBOR_CONTROL_MAX_TOKENS has no terminus-2 equivalent; unset it or use mini-swe-agent" >&2
    exit 2
  fi
fi

# Sourced BEFORE the path checks below: env.sh may set or change any of these,
# and a check above it would validate a value that never takes effect.
# shellcheck source=/dev/null
source "$HARBOR_ENV_SH"

# `podman compose` prints ">>>> Executing external compose provider ... <<<<"
# on every invocation, and Harbor folds a compose exec's stderr into its
# stdout. See run-harbor.sh's README, "Podman: the compose banner".
export PODMAN_COMPOSE_WARNING_LOGS=false

[[ -f "$HARBOR_CONTROL_KEY_FILE" ]] || { echo "no key file: $HARBOR_CONTROL_KEY_FILE" >&2; exit 1; }

job_dir="${HARBOR_JOBS_DIR}/${job_name}"
# The scan below covers exactly this directory. If it already exists, the scan
# would cover somebody else's output and this run's would be mixed into it.
if [[ -e "$job_dir" ]]; then
  echo "job directory already exists: $job_dir" >&2
  echo "pick another --job-name; this script scans that directory for the key" >&2
  exit 1
fi
mkdir -p "$HARBOR_JOBS_DIR"

# The scan pattern is read from a mode-600 file of our own rather than
# /dev/stdin, so "could not supply the pattern" is distinguishable from
# "could not search".
scan_dir="$(umask 077; mktemp -d "${HARBOR_JOBS_DIR}/.keyscan-${job_name}.XXXXXX")"
pattern_file="${scan_dir}/pattern"
config_scratch_dir="${HARBOR_JOBS_DIR}/.control-config-${job_name}"
cleanup() {
  rm -f -- "$pattern_file"
  rmdir -- "$scan_dir" 2>/dev/null || true
  rm -rf -- "$config_scratch_dir"
}
trap cleanup EXIT

# Read the key into the environment and into the pattern file. Never echoed,
# never on a command line: only its variable NAME travels, inside a Harbor env
# template. xtrace is already refused above; belt and braces here.
{ set +x; } 2>/dev/null
printf -v "$HARBOR_CONTROL_KEY_ENV" '%s' "$(< "$HARBOR_CONTROL_KEY_FILE")"
export "${HARBOR_CONTROL_KEY_ENV?}"
[[ -n "${!HARBOR_CONTROL_KEY_ENV}" ]] || { echo "key file is empty: $HARBOR_CONTROL_KEY_FILE" >&2; exit 1; }
( umask 077; printf '%s\n' "${!HARBOR_CONTROL_KEY_ENV}" > "$pattern_file" )

# Agent-specific limit kwargs. Left off the command line entirely when the
# matching HARBOR_CONTROL_* variable is unset, so the agent's own default
# stands rather than a value this script invents.
ak_args=()
case "$HARBOR_CONTROL_AGENT" in
  mini-swe-agent)
    [[ -n "${HARBOR_CONTROL_COST_LIMIT:-}" ]] && ak_args+=(--ak "cost_limit=${HARBOR_CONTROL_COST_LIMIT}")
    [[ -n "${HARBOR_CONTROL_REASONING_EFFORT:-}" ]] && ak_args+=(--ak "reasoning_effort=${HARBOR_CONTROL_REASONING_EFFORT}")
    [[ -n "${HARBOR_CONTROL_MAX_TOKENS:-}" ]] && ak_args+=(--ak "max_tokens=${HARBOR_CONTROL_MAX_TOKENS}")
    if [[ -n "${HARBOR_CONTROL_STEP_LIMIT:-}" ]]; then
      # mini-swe-agent has no --step-limit CLI flag (see README-control.md,
      # "Limits and fairness"); the only exposed mechanism is a config file
      # merged on top of the packaged "mini" config via mini-swe-agent's own
      # -c, which mini_swe_agent.py always prepends.
      mkdir -p "$config_scratch_dir"
      step_limit_file="${config_scratch_dir}/step-limit.yaml"
      printf 'agent:\n  step_limit: %s\n' "$HARBOR_CONTROL_STEP_LIMIT" > "$step_limit_file"
      ak_args+=(--ak "config_file=${step_limit_file}")
    fi
    ;;
  terminus-2)
    [[ -n "${HARBOR_CONTROL_REASONING_EFFORT:-}" ]] && ak_args+=(--ak "reasoning_effort=${HARBOR_CONTROL_REASONING_EFFORT}")
    [[ -n "${HARBOR_CONTROL_STEP_LIMIT:-}" ]] && ak_args+=(--ak "max_turns=${HARBOR_CONTROL_STEP_LIMIT}")
    ;;
esac

args=(
  run
  -a "$HARBOR_CONTROL_AGENT"
  -m "$HARBOR_CONTROL_MODEL"
  -e podman
  -k 1
  -n 1
  -o "$HARBOR_JOBS_DIR"
  --job-name "$job_name"
  --agent-timeout-multiplier "$HARBOR_CONTROL_AGENT_TIMEOUT_MULTIPLIER"
  --ae "${HARBOR_CONTROL_KEY_ENV}=\${${HARBOR_CONTROL_KEY_ENV}}"
  "${ak_args[@]}"
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

# Job-level provenance the agent cannot see: what was asked for, by what, and
# what Harbor itself reports each trial's agent as being (name/version are
# read back from result.json, not assumed from HARBOR_CONTROL_AGENT, so a
# Harbor-side agent-resolution surprise is visible here rather than hidden).
harbor_version="$(harbor --version 2>/dev/null | tr -d '\n' || true)"
tasks_json="$(printf '%s\n' "${task_names[@]}" | sed '/^$/d' | sed 's/.*/"&"/' | paste -sd, -)"
observed_agent_info="$(python3 - "$job_dir" <<'PY'
import glob
import json
import os
import sys

job_dir = sys.argv[1]
rows = []
for result_path in sorted(glob.glob(os.path.join(job_dir, "*", "result.json"))):
    try:
        with open(result_path, encoding="utf-8") as fh:
            data = json.load(fh)
    except (OSError, json.JSONDecodeError):
        continue
    info = data.get("agent_info") or {}
    rows.append(
        {
            "trial": os.path.basename(os.path.dirname(result_path)),
            "name": info.get("name"),
            "version": info.get("version"),
            "model_info": info.get("model_info"),
        }
    )
print(json.dumps(rows))
PY
)"
cat > "${job_dir}/kaijutsu-control-job-provenance.json" <<JSON
{
  "written_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "job_name": "${job_name}",
  "requested_agent": "${HARBOR_CONTROL_AGENT}",
  "requested_model": "${HARBOR_CONTROL_MODEL}",
  "task_path": "${task_path}",
  "dataset": "${dataset}",
  "tasks": [${tasks_json}],
  "harbor_version": "${harbor_version}",
  "agent_timeout_multiplier": "${HARBOR_CONTROL_AGENT_TIMEOUT_MULTIPLIER}",
  "cost_limit": "${HARBOR_CONTROL_COST_LIMIT:-}",
  "reasoning_effort": "${HARBOR_CONTROL_REASONING_EFFORT:-}",
  "max_tokens": "${HARBOR_CONTROL_MAX_TOKENS:-}",
  "step_limit": "${HARBOR_CONTROL_STEP_LIMIT:-}",
  "key_env": "${HARBOR_CONTROL_KEY_ENV}",
  "harbor_exit": ${status},
  "observed_agent_info": ${observed_agent_info}
}
JSON

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

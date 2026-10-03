# Running kaijutsu under a benchmark

Amy: "we are *NOT* chasing high benchmark numbers, mostly using it to set our
own personal bests and really using it to see what we need to do make kaijutsu
make sense to others."

So this is a measuring instrument for kaijutsu, not a leaderboard entry. A run
answers what a coder context does when nobody is sitting with it: where a turn
stops, what an ask costs, how many tokens a solved task takes, and what a
change to the instructions does to all three. What it does not answer is
whether kaijutsu is better than another agent — the tasks are a fixed cheap
subset, the model is a cheap model, and every number here is one sample.

The story of how this was built is `docs/devlog.md`, "The benchmark that
measured the harness (September 18)". What is still broken is `docs/issues.md`,
"What running under a benchmark showed (2026-09-18)".

## The pieces

```
harbor run                     host: builds the task image, one trial per task
   │  podman compose
task container
   │  acp_runner.py — Harbor's ACP client, started in the task's working dir
kaijutsu-solo-acp              static x86_64 binary, uploaded by the adapter
   │  SSH + Cap'n Proto over 127.0.0.1, the real wire
kaijutsu kernel                inside the same process (docs/solo-acp.md)
   ├── gate policy             gate-sandbox.toml: [global] uncovered = "allow"
   ├── workspace mount         the launch directory, read-write
   └── rc overlay              optional: an instruction-set variant
```

`contrib/bench/harbor/kaijutsu_solo_agent.py` is the adapter that makes the
binary present in a container that has never heard of it, builds the ACP
registry entry, and writes provenance. `contrib/bench/harbor/run-harbor.sh`
wraps `harbor run` around it and scans the output for the provider key.

A model can write only where the kernel mounts a directory read-write
(`docs/mounts.md`, "What a kernel mounts today"). `kaijutsu-solo-acp` mounts
the directory it was launched in, which is the task's own working directory, so
no `--mount` is needed for an ordinary task.

## One-time setup

Linux x86_64, rootless podman, `uv`, bash 4.4 or later, Python 3.12 or later.

**Harbor** installs as a uv tool under a bench-work directory, with an `env.sh`
that keeps it there:

```bash
source /home/atobey/src/bench-work/harbor/env.sh
uv tool install --python 3.12 'harbor==0.23.0'
harbor --version
```

Pin 0.23.0: `kaijutsu_solo_agent.py` subclasses Harbor's `AcpAgent` and checks
its model-name split against Harbor's own, so a newer Harbor is a deliberate
upgrade, not a side effect of reinstalling.

`env.sh` exports `UV_TOOL_DIR`, `UV_TOOL_BIN_DIR`, `HARBOR_HOME`,
`XDG_CACHE_HOME`, `XDG_DATA_HOME`, `DOCKER_CONFIG`, `DOCKER_HOST`, and a `PATH`
that reaches the uv tool bin directory. `run-harbor.sh` sources it;
`HARBOR_ENV_SH` names it.

**The compose provider.** Harbor drives podman through `podman compose` and
passes `--project-directory`, which `podman-compose` does not accept — the
trial's teardown fails on it. Install Docker's own `docker-compose` v2 binary,
put it on `PATH` under that name, and point it at podman's socket:

```bash
systemctl --user enable --now podman.socket
# in env.sh:
# DOCKER_HOST=unix://$XDG_RUNTIME_DIR/podman/podman.sock
# PODMAN_COMPOSE_PROVIDER=<bench-work>/harbor/uv-tool-bin/docker-compose
```

Name the provider with `PODMAN_COMPOSE_PROVIDER`. A host that also has the
distribution's `podman-compose` installed (moltar does, 1.6.0) otherwise
depends on `PATH` order to pick the right one. `podman compose version` must
print `Docker Compose version v5.x`, not `podman-compose`.

Check the setup without spending tokens: the `oracle` agent runs the task's
own solution.

```bash
harbor run --dataset "terminal-bench@2.0" -i fix-git -a oracle -e podman \
  -o /home/atobey/src/bench-work/harbor/jobs --job-name tb2-fix-git-oracle -y
# 26 s on moltar, reward 1.0
```

`PODMAN_COMPOSE_WARNING_LOGS=false` is exported by `run-harbor.sh`. Without it
`podman compose` prints `>>>> Executing external compose provider … <<<<` on
every invocation, Harbor folds a compose exec's stderr into its stdout, and
the banner lands in the output of commands Harbor parses — the first casualty
is `uname -s && uname -m` in `AcpAgent._detect_platform`. This hits Harbor's
stock `acp` agent under podman too.

**The provider key** lives in one file, mode 600, named by
`KAIJUTSU_ACP_KEY_FILE` (default `~/.deepseek-key`). `run-harbor.sh` reads it
into its own environment and gives Harbor the template
`--ae DEEPSEEK_API_KEY=${DEEPSEEK_API_KEY}`, never the value. Harbor resolves a
template from the host environment at run time and persists it unchanged, so
the job's `config.json` holds the template. The script refuses to run under
`set -x`, which would print the key twice.

After the run it greps the job directory for the key. Exit 3 means the key was
found, naming the files; exit 4 means the job directory was never created, so
nothing was scanned; exit 5 means `grep` itself failed and the run is
unverified. Only `grep`'s "no match" is reported as clean.

The scan does not cover Harbor's console output if the caller redirects it to a
file, the environment of a running process (the key rides `podman compose exec
-e KEY=value`, visible in that process's argv), or podman's recorded container
environment until the container is removed.

Inside the container the model can also read the key from `/proc` when the
agent runs as root, which is usual for a task image. `kaijutsu-solo-acp` clears
its dumpable flag, which stops a same-uid reader but not root
(`docs/solo-acp.md`, "The key and /proc"). Use a key scoped to the benchmark
account, not a long-lived one.

## Building the binaries

Build from a clean detached worktree, not the shared checkout. Other players'
uncommitted edits would otherwise land in the binary, and `BUILD_INFO.json`
would record it as dirty.

```bash
git worktree add --detach ~/src/wt/kj-bench-build HEAD
WORKTREE=~/src/wt/kj-bench-build contrib/bench/build-static.sh
```

It needs rootless podman and nothing else: the worktree is mounted read-only,
cargo runs inside the container, and every artifact lands under `WORK_ROOT`
(default `/home/atobey/src/bench-work/dist`). `WORKTREE` names the source tree.
`CARGO_BUILD_JOBS` (default 4) caps cargo's parallelism; a cold build took
12.5 minutes at 4 jobs on moltar (2026-09-30).

Output is `$WORK_ROOT/out/kaijutsu-solo-acp`, `kaijutsu-server`, and
`kaijutsu-acp`, stripped, plus
`$WORK_ROOT/kaijutsu-agent-linux-x86_64.tar.gz`.

The static link flags go in
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS`, not `RUSTFLAGS`. A blanket
`RUSTFLAGS` applies to host builds too, and rustc refuses to build a
proc-macro crate as `+crt-static`. The explicit `--target` is what separates
host artifacts from target artifacts.

The script proves linkage itself: it prints `file` and `ldd` for each binary,
then runs each one's `--help` inside `debian:bookworm-slim`, `ubuntu:24.04`,
and `alpine:3.22`. A dynamic dependency shows up as an `ldd` line and as a
failing smoke test.

## The host loop, without containers

Use this to debug the bridge, the gate, and the ask path. It is the only loop
where asks actually fire, because it keeps the shipped gate policy instead of
the sandbox tier.

```bash
uv venv /home/atobey/src/bench-work/acp-venv --python 3.12
uv pip install --python /home/atobey/src/bench-work/acp-venv agent-client-protocol

./contrib/bench/boot-kernel.sh --run-id ds-01 --port 22724 --model deepseek
./contrib/bench/run-host-task.sh \
  /home/atobey/src/bench-work/kernels/ds-01 \
  /home/atobey/src/bench-work/tasks/py-fizz \
  "Run 'python3 -m unittest -v' in this directory. One test fails. Fix fizz.py so every test passes, then run the tests again to confirm." \
  ds-run-1
./contrib/bench/stop-kernel.sh /home/atobey/src/bench-work/kernels/ds-01
```

`boot-kernel.sh` takes `--run-id`, `--port`, and `--model mock|deepseek`, and
nothing else; it refuses port 2222. The mock model needs a server built with
`--features kaijutsu-kernel/test-mock` and replays
`contrib/bench/mock_scripts/`. `contrib/bench/README.md` covers the run
directory, the gate patch, and reading `acp-events.jsonl`.

There is no standalone `kj` binary. `contrib/bench/kjmcp.py` runs `kj` commands
against a running kernel over the MCP stdio bridge.

## Running a suite

```bash
# one standalone task
./contrib/bench/harbor/run-harbor.sh --job-name kj-hello-world \
  --task-path /home/atobey/src/harbor/examples/tasks/hello-world

# one task from a dataset
./contrib/bench/harbor/run-harbor.sh --job-name kj-fix-git \
  --dataset "terminal-bench@2.0" --task fix-git

# the pinned 20-task subset
./contrib/bench/harbor/run-harbor.sh --job-name kj-tb2-armA \
  --dataset "terminal-bench@2.0" \
  --task-file contrib/bench/analysis/tb2-subset.txt
```

`--job-name` must not already exist: the key scan covers exactly that
directory. Everything after `--` reaches `harbor run` unchanged.

The subset is 20 of Terminal-Bench 2.0's 89 tasks, pinned to commit
`69671fbaac6d67a7ef0dfec016cc38a64ef7a77c`. They were chosen to be runnable by
a cheap model on one workstation: 12 of the dataset's 16 categories, 3 easy /
12 medium / 5 hard, every base image `ubuntu:24.04` or a `python:3.1x-slim`
variant, every `agent_timeout_sec` at or under 1800 seconds, and every
candidate's Dockerfile and `setup.sh` read for a GPU, a heavy ML install, or a
large download before it was included.
`contrib/bench/analysis/tb2-subset.md` holds the table and the exclusions.

Settings ride environment variables, each of which has a matching
`--ak key=value`:

| Variable | Kwarg | What it sets |
|---|---|---|
| `KAIJUTSU_ACP_MAX_TOKENS` | `max_tokens` | `--max-tokens N`. Unset: 65536 on DeepSeek (V4's advertised output limit), else the factory ceiling, 16384. |
| `KAIJUTSU_ACP_WORKSPACE_MOUNTS` | `workspace_mounts` | Comma-separated directories mounted read-write and made if missing, written to the state directory's `config/mounts.toml`. Unset: `/app,/git,/srv`. Empty: none. |
| `KAIJUTSU_ACP_RC_OVERLAY` | `rc_overlay` | A local rc variant directory, uploaded and applied before any context is created. |
| `KAIJUTSU_ACP_EGRESS_ALLOW` | `egress_allow` | Comma-separated hosts the coder context's `curl` may reach (`docs/egress.md`). Unset: `*`, every host. Empty: none. The adapter adds `coder/create/S50-egress.kai` to the rc overlay; it runs `kj context set . --egress-allow HOST` as the root character `solo`, the context's lineage root. It refuses an overlay that already has that file, and a `--context-type` in `solo_args`. |
| `KAIJUTSU_ACP_MODEL` | `solo_model` | The model id. Unset: `deepseek-v4-flash`. |
| `KAIJUTSU_ACP_BACKEND` | `backend_kind` | The provider. Unset: `deepseek`. |
| `KAIJUTSU_ACP_BASE_URL` | `base_url` | `--base-url URL`, an OpenAI-compatible endpoint. Unset: the provider's own. |
| `KAIJUTSU_ACP_KEY_FILE`, `KAIJUTSU_ACP_KEY_ENV` | `api_key_env` (the variable name) | The key's file, and the variable it travels in. The adapter passes the name as `--api-key-env`. Default `~/.deepseek-key` in `DEEPSEEK_API_KEY`. |
| `KAIJUTSU_ACP_RUST_LOG` | `rust_log` | Default `info`. Below `info` the run keeps no token record. |
| `KAIJUTSU_ACP_BINARY`, `KAIJUTSU_ACP_GATE` | `binary_path`, `gate_config_path` | The uploaded binary and gate policy. |

`harbor agent schema kaijutsu_solo_agent:KaijutsuSoloAcp` prints them, with
`PYTHONPATH` on `contrib/bench/harbor`.

### Qwen on Alibaba, and tenchi

Neither is a factory backend. Both are OpenAI-compatible, so a run names the
`openai` provider, the endpoint, and the variable the key travels in. The
Alibaba workspace endpoint is the one crush uses; read it from there rather
than writing it into this repository:

```bash
# Qwen 3.8 flash on Alibaba Model Studio
KAIJUTSU_ACP_BACKEND=openai KAIJUTSU_ACP_MODEL=qwen3.8-flash \
KAIJUTSU_ACP_BASE_URL="$(jq -r .providers.alibaba.base_url ~/.config/crush/crush.json)" \
KAIJUTSU_ACP_KEY_FILE=~/.alibaba-inference-key.txt KAIJUTSU_ACP_KEY_ENV=ALIBABA_API_KEY \
HARBOR_AGENT_TIMEOUT_MULTIPLIER=1 \
  ./contrib/bench/harbor/run-harbor.sh --job-name kj-qwen-fixgit-1 \
  --dataset "terminal-bench@2.0" --task fix-git
```

tenchi (the DGX Spark, vLLM serving `qwen3.8-27b`) takes no key and is slow:
about 17 tokens a second, and 25 s to prefill the coder seat. Give it its
thinking room. `KAIJUTSU_ACP_NO_KEY=1` runs with no key and no key scan, and
the two timeouts land on the backend row:

```bash
KAIJUTSU_ACP_BACKEND=openai KAIJUTSU_ACP_MODEL=qwen3.8-27b \
KAIJUTSU_ACP_BASE_URL=http://tenchi-inference.taila4abc.ts.net:8000/v1 \
KAIJUTSU_ACP_NO_KEY=1 KAIJUTSU_ACP_IDLE_TIMEOUT=600 KAIJUTSU_ACP_REQUEST_TIMEOUT=1800 \
HARBOR_AGENT_TIMEOUT_MULTIPLIER=5 \
  ./contrib/bench/harbor/run-harbor.sh --job-name kj-tenchi-openssl-2 \
  --dataset "terminal-bench@2.0" --task openssl-selfsigned-cert
```

Rootless task containers on moltar reach both: the tailnet name resolves
inside the container. The kernel drops the factory `effort = max` on either
endpoint with a warning (`effort has no sink on a non-hosted, non-DeepSeek
OpenAI-compatible endpoint`), so these runs use each model's default
reasoning.

`HARBOR_AGENT_TIMEOUT_MULTIPLIER` (default 5) multiplies each task's own
`agent_timeout_sec`. Raise it when a run is being cut off mid-work; set it to 1
to hold the agent to the task's own published timeout.

The wrapper pins one attempt per trial (`-k 1`) and runs `HARBOR_CONCURRENCY`
trials at once (default 1). The job provenance records both.
Whether `harbor run -e podman` is well behaved above one concurrent trial under
rootless podman has not been tested here.

Harbor's own `--model` is not passed through: its runner raises when a model is
requested and the agent advertises no model-selection mechanism, and kaijutsu
advertises none. `contrib/bench/harbor/README.md` covers what happens to a
`--model` that is given anyway.

## Reading results

```bash
python3 contrib/bench/analysis/summarize_job.py <job-dir>
python3 contrib/bench/analysis/classify_run.py <job-dir>/<trial>/agent \
  --kernel-log <job-dir>/<trial>/agent/acp.txt
```

`summarize_job.py` prints one row per trial plus totals; `--format jsonl` gives
one JSON object per trial and one for the totals. `classify_run.py` reports one
run in detail; `--format text` shortens it.

**`turn_end_class` says how the turn ended, not whether the task passed.**
Harbor's verifier decides that, from `verifier/reward.txt` and
`verifier/ctrf.json`. A turn can end cleanly on a wrong answer, and a task can
pass after a turn that fell over.

| Class | What ended the turn |
|---|---|
| `completed_verified` | `end_turn`, and a command ran successfully after the last edit. A `shell_write` call counts as both an edit and a command, so a run whose last call is a completed `shell_write` lands here; events cannot tell a test run from a `sed -i` |
| `ended_unverified` | `end_turn`, with no edit or nothing run after it |
| `yielded_on_ask` | the last tool result was a gate waiting on its reviewer |
| `yielded_on_async` | the last tool result left an operation nobody awaited |
| `output_starved` | the last tool results spilled their output cap |
| `token_ceiling` | `max_tokens` |
| `iteration_cap` | ACP `max_turn_requests`; kaijutsu's agentic loop has no per-turn iteration cap today, so this class is unreachable from a kaijutsu trial |
| `cancelled` | `session/cancel` |
| `provider_failure` / `setup_failure` | the run errored, with or without a session |
| `unclassified_stop_reason` | a stop reason this scheme does not name |

`asks_orphaned` counts asks the turn raised and then outran, which is a stall
with no ask visible to the client.

The **verdict line** is the driven-worker convention: the worker's final
message ends with `RESULT: done`, `RESULT: blocked — …`, or
`RESULT: gave up — …`, alone on the last line
(`contrib/bench/rc-variants/coder-driven/README.md`, "The verdict line"). Both
tools read it. The totals compare it against Harbor's own reward:
`verdict_done_and_solved`, `verdict_done_but_failed` (the worker claimed done
and the verifier disagreed), `verdict_not_done_but_solved`.

Totals worth watching:

- `tokens_per_solved_task` — tokens over solved trials, divided by the solved
  trials that carried token data. The harness literature reports up to 40x
  spread here between harnesses at nearly equal pass rates, so it is the
  number that moves when the instrument is wrong.
- `ask_count`, `stall_count`, `turns_ended_early`. A trial stalls when it
  yielded on an ask or an async operation, or orphaned an ask. It ended early
  when its class is not `completed_verified` and its reward is below 1.0.
- `tokens_source` per row. Harbor's own token and cost columns stay empty:
  kaijutsu sends no `PromptResponse.usage`. The kernel log riding the agent's
  stderr into `agent/acp.txt` is the only token record, scoped to that trial's
  session id.

**Provenance.** `<job>/kaijutsu-job-provenance.json` records what was asked
for: job name, dataset or task path, task names, Harbor version, timeout
multiplier, binary and gate paths, the key's variable name, Harbor's exit
status. `<job>/<trial>/agent/kaijutsu-provenance.json` records what ran:
`binary.sha256` and size, `gate.sha256`, `worktree.head` and whether it was
dirty, backend and model, `max_tokens`, the rc overlay and its
hash, the egress hosts and their script, the CA bundle, and the state directory.

To map a binary back to a commit, match `binary.sha256` against
`BUILD_INFO.json`, which `build-static.sh` writes beside the binaries and into
the tarball with the commit it compiled and whether that tree was dirty.
`worktree.head` in the trial file is the adapter's worktree at upload time; it
identifies the adapter, the gate file and the rc variant, and it can be newer
than the binary when the branch moved after the build.

## Comparing instruction sets

An rc variant replaces some of a context type's rc files and changes nothing
else. `contrib/bench/rc-variants/coder-driven/` is the worked example: it gives
the coder type a driven-worker contract in place of the shared collaborative
base. Its README states what it replaces, why the loader accepts it, and the
confounds to name in a result.

Run the arms as one variable:

```bash
./contrib/bench/harbor/run-harbor.sh --job-name kj-tb2-armA-shipped \
  --dataset "terminal-bench@2.0" --task-file contrib/bench/analysis/tb2-subset.txt

KAIJUTSU_ACP_RC_OVERLAY="$PWD/contrib/bench/rc-variants/coder-driven" \
  ./contrib/bench/harbor/run-harbor.sh --job-name kj-tb2-armB-driven \
  --dataset "terminal-bench@2.0" --task-file contrib/bench/analysis/tb2-subset.txt
```

The overlay path resolves against the directory `harbor` runs in, so give it
absolutely.

Same tasks, same binary, same gate policy, same token
ceiling, same timeout multiplier. Each job's provenance records all of them, and
the overlay's sha256 ties a job to the exact variant that produced it. Compare
with `summarize_job.py`: pass rate, tokens per solved task, `turns_ended_early`,
and verdict agreement.

## Control arm

A benchmark number without a control arm measures the model, not the
instrument. The control is mini-swe-agent — the standard bash-only harness —
on the same tasks and the same model, so the difference between the two rows is
kaijutsu.

Run it with `contrib/bench/harbor/run-control.sh`, which takes the same
arguments as `run-harbor.sh` and handles the key and the post-run scan the same
way (`contrib/bench/harbor/README-control.md`):

```bash
HARBOR_CONTROL_AGENT_TIMEOUT_MULTIPLIER=1 HARBOR_CONTROL_STEP_LIMIT=100 \
HARBOR_CONTROL_COST_LIMIT="0.25" \
contrib/bench/harbor/run-control.sh --job-name ctl-tb2-miniswe \
  --dataset "terminal-bench@2.0" --task-file contrib/bench/analysis/tb2-subset.txt
```

The model string is `deepseek/deepseek-v4-flash`; litellm already prices it.
mini-swe-agent ships with no step limit, and Harbor overrides its $3 cost limit
to unlimited. kaijutsu's agentic loop has no per-turn iteration cap either
(`docs/issues.md`, "Per-cast turn token budget"), so `HARBOR_CONTROL_STEP_LIMIT`
has no kaijutsu value to match; pick a limit that bounds the control run's
cost instead. Its shell inherits the process environment: a model that
runs `env` puts the provider key in its transcript. That happened once in the
recorded control run; the post-run scan exits 3 and names the file to scrub.

`summarize_job.py` already reads a non-ACP trial: it reports
`turn_end_class: "n/a"` and takes tokens and cost from Harbor's own
`agent_result`, which litellm-backed agents do populate.

## Cost

A Terminal-Bench 2.0 task costs about $0.02 on `deepseek-v4-flash`. Measured by
DeepSeek account balance before and after `kj-calib-1`: three tasks, 2.65M input
tokens, $0.05. The account is shared with kaibo's DeepSeek cast, so a balance
delta is an upper bound on what a run spent, never an under-count.

A full day of this work, about 70 task runs across four arms plus reviews,
took the balance from $60.07 to $56.21.

Per-run token counts come from `summarize_job.py`, which reads the kernel log.
They are exact; only the dollar figure is a bound.

Qwen runs on Alibaba bill against Amy's plan, and no dollar figure was taken
for them; use the token counts. tenchi costs nothing but time.

## Recorded baselines

One row per recorded job. `Binary → commit` is the trial provenance's
`binary.sha256` and the commit `BUILD_INFO.json` gives for it.

| Date | Job | Binary → commit | Model | Settings | Tasks | Solved | Tokens/solved | Asks | Stalls | Ended early | Verdict agreement |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-09-18 | `kj-hw-1`, `kj-fixgit-1` | not recorded (predates provenance) | deepseek-v4-flash | shipped coder, collaborative, 16K ceiling, multiplier 5 and 2 | 2 (hello-world, fix-git) | 2 | 43K and 1.39M | 0 | 0 | 0 | no verdict line |
| 2026-09-18 | `kj-calib-1` | `e9802591…` → `c9ad92c4` (dirty) | deepseek-v4-flash | shipped coder, collaborative, 16K ceiling, multiplier 1 | 3 (openssl-selfsigned-cert, regex-log, sqlite-with-gcov) | 1 | 756K | 0 | 0 | 2 | no verdict line |
| 2026-09-18 | `kj-tb2-armA-shipped` | `05d77c21…` → built from `7cd1593d` | deepseek-v4-flash | shipped coder, autonomous, 32768 ceiling, multiplier 1 | 20 (tb2-subset) | 14 (0.70) | 3.29M | 0 | 0 | 6 | no verdict line in this arm |
| 2026-09-18 | `kj-tb2-armB-driven` | `05d77c21…` → built from `7cd1593d` | deepseek-v4-flash | `coder-driven` overlay, autonomous, 32768 ceiling, multiplier 1 | 20 (tb2-subset) | 15 (0.75) | 3.89M | 0 | 0 | 4 | line present 17/20; done and solved 14, done but failed 2, blocked and failed 1 |
| 2026-09-18 | `kj-tb2-armC-lost6-ceilingfix` | `d5d21d59…` → `413b9ce0` | deepseek-v4-flash | shipped coder, autonomous, 32768 ceiling, multiplier 1, ran beside arm B | 6 (the tasks arm A lost) | 5 | 4.64M | 0 | 0 | 1 | no verdict line in this arm |
| 2026-09-18 | `ctl-tb2-miniswe` (control) | mini-swe-agent as Harbor installs it | deepseek/deepseek-v4-flash | step limit 100, cost limit $0.25 per task, multiplier 1, ran beside arm A | 20 (tb2-subset) | 18 (0.90) | 1.79M | n/a | n/a | 2 | n/a |
| 2026-09-26 | `kj-tb2-render-envelope` | `bc27a61f…` → `855ace8a` | deepseek-v4-flash | shipped coder, autonomous, 32768 ceiling, multiplier 1, shell results as the JSON envelope, ran beside the plain arm | 20 (tb2-subset) | 16 (0.80) | 4.35M | 0 | 0 | 5 | no verdict line in this arm |
| 2026-09-26 | `kj-tb2-render-plain` | `01f5b424…` → `0e88658c` | deepseek-v4-flash | same, shell results as plain text (`model_text`) | 20 (tb2-subset) | 17 (0.85) | 3.40M | 0 | 0 | 4 | no verdict line in this arm |
| 2026-09-30 | `kj-qwen-fixgit-1`, `kj-qwen-pair-1`, `kj-qwen-sqlite-1` | `4bd76664…` → `12ed6d76` | qwen3.8-flash (Alibaba) | shipped coder, yolo sandbox gate, factory 16K ceiling, effort dropped, multiplier 1, moltar | 4 (fix-git, openssl-selfsigned-cert, fix-code-vulnerability, sqlite-with-gcov) | 4 | 0.96M | 0 | 0 | 0 | no verdict line |
| 2026-09-30 | `kj-tenchi-openssl-1` | `4bd76664…` → `12ed6d76` | qwen3.8-27b (tenchi vLLM) | same, multiplier 2 | 1 (openssl-selfsigned-cert) | 0 | n/a | 0 | 0 | 1 | first inference failed: `LLM stream idle for 120s` |
| 2026-10-01 | `kj-tenchi-tb2-20-2` | `8635a93b…` → `4071fc68` | qwen3.8-27b (tenchi vLLM) | shipped coder, yolo sandbox gate, factory 16K ceiling, no key, idle 600 s, request 1800 s, multiplier 4, 2 trials at once, moltar | 20 (`tb2-subset.txt`) | 7 | 0.52M | 0 | 0 | 11 | no verdict line |
| 2026-10-01 | `kj-ds4-tb2-20-1` | `1997713d…` → `6c8252c4` | deepseek-v4-flash | shipped coder with `done`, yolo sandbox gate, 32768 ceiling, multiplier 1, 2 trials at once, moltar | 20 (`tb2-subset.txt`) | 18 (0.90) | 3.57M | 0 | 0 | 2 | `done` on 15/15 turns that ended on their own; 15 done and solved |
| 2026-10-03 | `kj-ds4-tb2-20-20261003`, `…b` | `0f37d0ee…` → `9283226c` | deepseek-v4-flash | shipped coder, yolo sandbox gate, **16384 ceiling** (unset), multiplier 1, 1 trial at once, moltar; Harbor died at 10 tasks and `…b` ran the other 10 | 20 (`tb2-subset.txt`) | 14 (0.70) | 2.93M | 0 | 0 | 4 | `done` on 17/17 turns that ended on their own; 14 done and solved |

`kj-calib-1`'s two failures: `regex-log` ended `provider_failure` when the
model's `write` call arrived with its JSON arguments cut off and the whole turn
failed; `sqlite-with-gcov` ended `iteration_cap` at 50 collaborative
iterations, with nobody there to send the follow-up it asked for.

How arm A's six losses ended, from `summarize_job.py`: three turns stopped at
the output ceiling after 4, 9 and 16 inferences (`headless-terminal`,
`model-extraction-relu-logits`, `raman-fitting`), one reached the 100-iteration
cap (`dna-assembly`), one failed on a dropped provider stream
(`db-wal-recovery`), and one hit Harbor's agent timeout (`chess-best-move`).
The control solved all six of those tasks. Its own two losses,
`configure-git-webserver` and `query-optimize`, were both solved by arm A.
Median time for a solved task was 374 s for arm A and 291 s for the control. The control's transcript for `db-wal-recovery` contained the provider
key, because the model ran `env` and mini-swe-agent's shell inherits the
process environment; the post-run scan caught it and the file was scrubbed.

### What each change moved

One attempt per task is noisy, and arm C measured how noisy. It reran the six
tasks arm A lost with one change, the turn-loop fix, and solved five. Only one
of the five is the fix: `model-extraction-relu-logits` hit the output ceiling
again, continued, and finished at 42 inferences where arm A's turn ended at 4.
`headless-terminal` and `raman-fitting` never reached the ceiling this time,
and `chess-best-move` and `db-wal-recovery` failed in arm A for reasons the fix
does not touch. Four of six lost tasks flipped with nothing relevant changed.
Read a difference of a few tasks between two k=1 rows as noise, and repeat a
run before calling anything a personal best.

What is supported:

- **The harness costs pass rate, tokens and time against a bash-only control
  on the same model:** 14 and 15 of 20 against 18, about twice the tokens per
  solved task, and a slower median solve. A fresh coder seat measured
  about 14,400 input tokens on 2026-10-01 (the first inference's input
  count), 80% of it tool schemas; the 46,000 first recorded here was a
  trial's total. hello-world's whole trial cost 42,497 input tokens against
  the control's 3,670.
- **Every ceiling stop in arm A was fatal** (three of three), and the one
  ceiling stop in arm C was survived. That is the turn-loop change
  (`413b9ce0`): a turn now continues past the output ceiling, at most three
  times, and a tool call with cut-off arguments returns an error instead of
  failing the turn.
- **The `coder-driven` instructions changed what the model does, measurably.**
  Shell calls passing `foreground: true` went from 86% (666 of 772) to 95% (783
  of 827), `kj wait` calls from 8 to 4, agent timeouts from 2 to 0. The shipped
  instructions already reached 86%, so asynchronous-by-default shell cost less
  in practice than the code audit predicted. Pass rate moved from 14 to 15,
  which is inside the noise.
- **The verdict line works as a done signal.** It ended 17 of 20 final
  messages, which is every turn that ended on its own; the three without it
  stopped at the iteration cap, the output ceiling and a dropped stream.
  `RESULT: done` was right 14 times of 16, and the one `RESULT: blocked` was a
  real loss. A driver can read it today; a kernel-side completion command would
  make the three abnormal endings visible the same way.
- **Under the sandbox gate nothing asks**, so these runs say nothing about the
  approval path. The host-loop findings in `docs/issues.md` stand.

- **Plain shell results cost fewer tokens than the envelope** (2026-09-26,
  the two `render` rows). Same binary but the rendering, run side by side:
  total tokens 92.3M to 69.4M, tokens per solved task 5.77M to 4.08M,
  inferences 1012 to 909, tool calls 1313 to 1116, repeated identical
  commands 35 to 19. Plain was cheaper on 14 of 20 paired tasks (sign test
  p about 0.12), and the median per task moved less (2.79M to 2.55M), so a
  few long tasks carry most of it. Solves (16 and 17) are inside the noise.
  `~/src/bench-work/arms/compare_arms.py` produced the comparison. Plain
  rendering is the kernel default from here (Amy, 2026-09-27: freeze the
  plain, and "we'll dial in the plain outputs even more over time").

### Qwen on current main (2026-09-30)

The first runs since the turn started holding on its own ask and the
iteration cap went away. Under the yolo gate nothing asks, so these runs
exercise the turn loop, the shell and the tools, not the approval path.

| Task | Reward | Class | Inferences | Tool calls (shell) | Tokens in / out | Trial time |
|---|---|---|---|---|---|---|
| fix-git | 1.0 | `completed_verified` | 38 | 43 (37) | 791K / 20K | 427 s |
| fix-code-vulnerability | 1.0 | `completed_verified` | 16 | 22 (16) | 323K / 5K | 139 s |
| openssl-selfsigned-cert | 1.0 | `completed_verified` | 26 | 25 (21) | 539K / 13K | 273 s |
| sqlite-with-gcov | 1.0 | `agent_timeout` | 70 | 98 (87) | 2.12M / 35K | 941 s, agent cut off at the task's 900 s |
| openssl-selfsigned-cert on tenchi | 0.0 | `provider_failure` | 0 | 0 | — | 154 s |

Against `kj-tb2-render-plain` on DeepSeek flash, Qwen used fewer tokens on
fix-code-vulnerability (328K against 1.0M) and sqlite-with-gcov (2.16M against
5.5M), and more on fix-git (811K against 630K) and openssl-selfsigned-cert
(552K against 522K); fix-git took three times as long (427 s against 136 s).
One sample each; read it as "the harness works with Qwen", not as a
comparison.

What the runs showed, beyond the pass rate:

- **sqlite-with-gcov was solved and still timed out.** The build and the
  coverage run finished early enough for the verifier; the model kept
  polishing (probing which `PATH` directories were writable) until Harbor's
  900 s limit. Its last calls were a background `make` it polled with
  `read_shell_operation` and a `sleep 90`. No shell call hit the broker's
  120 s or 315 s timeout in any run: the one long build ran in the
  background.
- **tenchi cannot finish an inference.** vLLM streams Qwen's thinking as
  `delta.reasoning`; the kernel's OpenAI delta reads only
  `reasoning_content` (`crates/kaijutsu-kernel/src/llm/openai/types.rs`,
  `Delta`), so the thinking phase is invisible and the stream counts as idle.
  A prefill of the 14K-token coder seat takes about 25 s on tenchi, so the
  prompt is not the delay. `kaijutsu-solo-acp` also has no flag for the
  backend's idle timeout. Both need a kernel change.
- **A deleted working directory wedges the shell.** openssl-selfsigned-cert
  ran `cd /tmp/neg/ssl`, later `rm -rf /tmp/neg`, and every following
  `shell` and `shell_write` call failed with `context cwd '/tmp/neg/ssl' is
  unavailable; set a valid cwd before executing`, including `cd /app; pwd`
  (`runtime/context_shell.rs`, the initial-cwd check). The model got out by
  writing a file into the missing directory with the `write` tool.
- **The kaish parser refuses ordinary bash.** The shell-escape guard denied,
  as "no execution plan", `git diff master^ master`, `grep "<<<<<<<\|>>>>>>>"`,
  a `( make … )` subshell, `find … \( -name … \)`, and a brace group used
  as a report block. 6 of the 29 failed tool calls across the four Qwen runs were
  parse refusals.
- **Builtins differ from the programs a model expects.** `git` is kaish-git:
  `git status` refuses a repository holding a 17 MB blob (8 MiB
  `max_blob_bytes`), and `git diff` takes `--from/--to` instead of two
  revisions. `ls -d`, `stat -f` and `diff --help` fail on the builtins.
  Builtin `ln` could not write `/usr/local/bin` (read-only outside the
  mounted workspace) though `apt-get install` wrote `/usr/bin` a call
  earlier.
- **The read-only `shell` and `shell_write` split costs calls.** Models ran
  write commands in `shell` and read the refusal ("external commands are
  disabled on this shell"), and fix-git called a tool that does not exist,
  `shell_read_note`.
- **The ACP bridge labels `shell_write` an edit.** `acp_tool_kind` splits the
  name on `_` and matches `write` (`crates/kaijutsu-acp/src/update.rs`), so
  Harbor's trajectory records every `shell_write` as kind `edit`.
  `classify_run.py` now reads the title as well, and counts a completed
  `shell_write` as both an edit and a command; before that every run in the
  2026-09-26 arms and all three solved Qwen runs read `ended_unverified`.

Left unmeasured: the Rust polyglot slice (`contrib/bench/analysis/polyglot-rust.md`)
has not been run with a model; Anthropic models are untested end to end.

### tenchi on the 20-task subset (2026-10-01)

The first full subset run on a local model: qwen3.8-27b on tenchi's vLLM,
about 17 output tokens a second. 7 of 20 solved. The run took 7.5 hours
with two trials at once. tenchi costs nothing but time, so there is no
dollar column; vLLM reports no cost.

| Task | Result | Ended | Tokens in / out | Inferences | Trial time |
|---|---|---|---|---|---|
| constraints-scheduling | pass | `completed_verified` | 169K / 13K | 8 | 836 s |
| db-wal-recovery | pass | `completed_verified` | 311K / 10K | 14 | 654 s |
| extract-elf | pass | `completed_verified` | 587K / 29K | 29 | 1982 s |
| fix-code-vulnerability | pass | `completed_verified` | 874K / 16K | 30 | 1109 s |
| fix-git | pass | `completed_verified` | 863K / 15K | 32 | 1037 s |
| modernize-scientific-stack | pass | `completed_verified` | 336K / 15K | 16 | 984 s |
| openssl-selfsigned-cert | pass | `completed_verified` | 377K / 10K | 20 | 683 s |
| configure-git-webserver | fail | `completed_verified` | 1.03M / 40K | 41 | 2732 s |
| query-optimize | fail | `completed_verified` | 397K / 29K | 22 | 3612 s |
| dna-assembly | fail | `token_ceiling` | 128K / 66K | 7 | 3982 s |
| chess-best-move | fail | agent timeout | 1.08M / 46K | 34 | 3637 s |
| cobol-modernization | fail | agent timeout | 383K / 54K | 17 | 3635 s |
| headless-terminal | fail | agent timeout | 503K / 54K | 23 | 3635 s |
| largest-eigenval | fail | agent timeout | 132K / 49K | 8 | 3637 s |
| model-extraction-relu-logits | fail | agent timeout | 50K / 48K | 3 | 3634 s |
| overfull-hbox | fail | agent timeout | 250K / 47K | 13 | 3055 s |
| raman-fitting | fail | agent timeout | 533K / 57K | 17 | 3632 s |
| regex-log | fail | agent timeout | 113K / 51K | 7 | 3638 s |
| sparql-university | fail | agent timeout | 286K / 44K | 13 | 3637 s |
| sqlite-with-gcov | fail | agent timeout | 531K / 46K | 27 | 3635 s |

Totals: 8.93M tokens in, 0.74M out; 0.52M in per solved task. No turn
failed, nothing asked (yolo gate), nothing stalled.

- **Time is the limit, not the turn loop.** 10 of the 13 failures are
  Harbor's agent timeout at four times each task's own limit. Several made
  only 3 to 8 inferences in an hour (model-extraction-relu-logits 3,
  regex-log 7, largest-eigenval 8): each inference spent minutes thinking
  near the 16K output ceiling. Output tokens per inference, not tool calls,
  set the pace.
- Every solved task ended `completed_verified` in under 35 minutes; a
  long run did not turn into a solve.
- The changes this run needed: vLLM's `delta.reasoning`, solo-acp
  `--no-key` and the two backend timeouts, batch results placed after their
  calls, a removed cwd that refuses once and moves, `shell_write` as kind
  `execute` (`ce55dca4` through `4071fc68`).

### deepseek-v4-flash with `done` (2026-10-01)

18 of 20, against 17 for the frozen plain-rendering arm and 18 for the
bash-only control on the same model. One sample per task, so read it as
"the harness no longer costs pass rate here", not as a gain. Binary
6c8252c4: `done` and its nudge, cut shell output kept in CAS, `timeout_ms`,
the removed-cwd fix, batch results after their calls. It predates the
same-file edit ordering (ecc17787), the coder's own binding and one shell
(a22c2912/7a6a7b2f), and the orientation preload (c2133264).

| Task | Result | Ended | Tokens in / out | Inferences | Trial time |
|---|---|---|---|---|---|
| chess-best-move | pass | `done` | 2.17M / 58K | 44 | 379 s |
| cobol-modernization | pass | `done` | 10.5M / 140K | 94 | 805 s |
| configure-git-webserver | pass | `done` | 2.45M / 52K | 48 | 376 s |
| constraints-scheduling | pass | `done` | 607K / 20K | 21 | 145 s |
| db-wal-recovery | pass | `done` | 1.08M / 55K | 24 | 305 s |
| dna-assembly | pass | `done` | 13.8M / 186K | 101 | 1187 s |
| extract-elf | fail | agent process exited 1 | 345K / 9K | 14 | 108 s |
| fix-code-vulnerability | pass | `done` | 842K / 18K | 26 | 140 s |
| fix-git | pass | `done` | 709K / 22K | 23 | 157 s |
| headless-terminal | fail | agent process exited 1 | 335K / 22K | 15 | 176 s |
| largest-eigenval | pass | agent timeout | 6.79M / 153K | 62 | 938 s |
| model-extraction-relu-logits | pass | agent timeout | 2.41M / 168K | 30 | 971 s |
| modernize-scientific-stack | pass | `done` | 1.09M / 33K | 29 | 247 s |
| openssl-selfsigned-cert | pass | `done` | 509K / 23K | 18 | 342 s |
| overfull-hbox | pass | `done` | 1.80M / 47K | 34 | 306 s |
| query-optimize | pass | `done` | 2.61M / 65K | 49 | 1565 s |
| raman-fitting | pass | agent timeout | 11.2M / 177K | 81 | 937 s |
| regex-log | pass | `done` | 1.67M / 85K | 22 | 424 s |
| sparql-university | pass | `done` | 1.71M / 40K | 41 | 286 s |
| sqlite-with-gcov | pass | `done` | 904K / 18K | 33 | 272 s |

- **`done` held.** Every turn that ended on its own ended with `done`, and no
  nudge fired: the model never ended on text alone. All 15 verdicts were
  `done` on a solved task.
- **Both losses were the agent process dying on `grep -r PATTERN /`.**
  extract-elf and headless-terminal each ran a recursive builtin `grep` over
  the whole filesystem as its last command, and `kaijutsu-solo-acp` exited 1
  with no error logged. kaish builtins run in the kernel's process, so a walk
  that reads something unbounded can take the agent down with it
  (`docs/issues.md`).
- **Three solves ran past the agent timeout.** largest-eigenval reached a
  passing eval 37 s before its 900 s limit; the deliverable was in place when
  Harbor stopped it. raman-fitting and model-extraction-relu-logits also
  passed after timing out.
- **Same-file edits raced 9 times** ("edit applied, but ... another writer
  changed it"), the bug ecc17787 fixed after this binary was built.
- **13 results were cut and stored in CAS; the model read a `/v/cas` path
  once.** The 6 KiB head and tail were usually enough.
- **Input cost is reasoning replay.** The first inference is about 14.3K
  tokens; cobol-modernization and dna-assembly spent 10-14M input tokens over
  94-101 inferences.

### deepseek-v4-flash at a 16K ceiling (2026-10-03)

14 of 20, against 18 for `kj-ds4-tb2-20-1`. **The two runs differ in their
output ceiling:** that baseline passed `max_tokens` 32768, and this one left it
unset, so the factory 16384 applied at effort `max`. Binary 9283226c: the
`[stderr]` marker, the "subset of bash" shell description, a guard that lets a
shell run a script file, the coder's building rules, and wrapped `python`.

| Task | Result | Ended | Tokens in | Inferences | Trial time |
|---|---|---|---|---|---|
| db-wal-recovery | fail | `done` | 14.4M | 114 | 978 s |
| dna-assembly | fail | `token_ceiling` | 78K | 8 | 362 s |
| headless-terminal | fail | `token_ceiling` | 40K | 7 | 593 s |
| model-extraction-relu-logits | fail | `token_ceiling` | 28K | 5 | 362 s |
| query-optimize | fail | `done` | 449K | 21 | 1390 s |
| raman-fitting | fail | `done` | 7.13M | 66 | 1085 s |
| extract-elf | pass | `done` | 13.1M | 133 | 918 s |
| modernize-scientific-stack | pass | `done` | 104K | 8 | 84 s |

The other 12 passed.

- **Three losses were reasoning past the ceiling.** Each turn stopped on
  `length` four times with 166-186K characters of reasoning and no tool call,
  then ended. The adapter now passes 65536 on DeepSeek when `max_tokens` is
  unset. Compare runs only at the same ceiling, read from provenance.
- **db-wal-recovery destroyed its input.** The first `sqlite3` open deleted the
  encrypted WAL it had to recover; the model then reconstructed records by
  guessing and called `done`, disclosing the guess. The coder stance now says
  to back up important files first and that a guessed result is not checked.
- **query-optimize missed a timing check by 17%** while niced test builds ran
  on the host; treat it as confounded.
- **raman-fitting** fit the peaks in the wrong units.
- **extract-elf passed**: the 10-01 loss came from `grep -r /` reading `/proc`,
  which is now unlisted.
- **configure-git-webserver** passed after about 70 calls finding that `/git`
  was read-only to kaijutsu; refusals now list the mounts, and the adapter
  mounts `/app`, `/git`, and `/srv` read-write.
- 93 of 1056 tool calls failed. Three were guard parse denials; the rest were
  ordinary program errors, missing files, read-only writes, and builtin gaps
  (`docs/issues.md`, "From kj-ds4-tb2-20-20261003").

## Known limits

`docs/issues.md`, "What running under a benchmark showed (2026-09-18)" holds
what these runs exposed and what is still open, most costly first. A model's
turn now holds on its own ask and reads the approved command's output as its
tool result (`docs/gate-resume.md`, "The turn holds"), but no benchmark has run
the ask path since: the sandbox gate tier allows everything, which is why every
recorded run has an ask count of zero.

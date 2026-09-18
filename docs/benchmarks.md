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
uv tool install harbor          # 0.23.0
harbor --version
```

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
# DOCKER_HOST=unix://$XDG_RUNTIME_DIR/podman/podman.sock in env.sh
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

```bash
contrib/bench/build-static.sh
```

It needs rootless podman and nothing else: the worktree is mounted read-only,
cargo runs inside the container, and every artifact lands under `WORK_ROOT`
(default `/home/atobey/src/bench-work/dist`). `WORKTREE` names the source tree.

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
| `KAIJUTSU_ACP_CONSENT` | `consent` | `--consent collaborative\|autonomous`. Unset: the binary's default, collaborative, a 50-iteration per-turn cap. |
| `KAIJUTSU_ACP_MAX_TOKENS` | `max_tokens` | `--max-tokens N`. Unset: the factory ceiling, 16384. |
| `KAIJUTSU_ACP_RC_OVERLAY` | `rc_overlay` | A local rc variant directory, uploaded and applied before any context is created. |
| `KAIJUTSU_ACP_MODEL` | `solo_model` | The model id. Unset: `deepseek-v4-flash`. |
| `KAIJUTSU_ACP_BACKEND` | `backend_kind` | The provider. Unset: `deepseek`. |
| `KAIJUTSU_ACP_RUST_LOG` | `rust_log` | Default `info`. Below `info` the run keeps no token record. |
| `KAIJUTSU_ACP_BINARY`, `KAIJUTSU_ACP_GATE` | `binary_path`, `gate_config_path` | The uploaded binary and gate policy. |

`harbor agent schema kaijutsu_solo_agent:KaijutsuSoloAcp` prints them, with
`PYTHONPATH` on `contrib/bench/harbor`.

`HARBOR_AGENT_TIMEOUT_MULTIPLIER` (default 5) multiplies each task's own
`agent_timeout_sec`. Raise it when a run is being cut off mid-work; set it to 1
to hold the agent to the task's own published timeout.

The wrapper pins one attempt per trial and one trial at a time (`-k 1 -n 1`).
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
| `completed_verified` | `end_turn`, and a command ran successfully after the last edit |
| `ended_unverified` | `end_turn`, with no edit or nothing run after it |
| `yielded_on_ask` | the last tool result was a gate waiting on its reviewer |
| `yielded_on_async` | the last tool result left an operation nobody awaited |
| `output_starved` | the last tool results spilled their output cap |
| `token_ceiling` | `max_tokens` |
| `iteration_cap` | the per-turn agentic iteration cap |
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
dirty, backend and model, consent and `max_tokens`, the rc overlay and its
hash, the CA bundle, and the state directory.

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

Same tasks, same binary, same gate policy, same consent mode, same token
ceiling, same timeout multiplier. Each job's provenance records all of them, and
the overlay's sha256 ties a job to the exact variant that produced it. Compare
with `summarize_job.py`: pass rate, tokens per solved task, `turns_ended_early`,
and verdict agreement.

## Control arm

A benchmark number without a control arm measures the model, not the
instrument. The control is mini-swe-agent — the standard bash-only harness —
on the same tasks and the same model, so the difference between the two rows is
kaijutsu.

This section is a stub: `contrib/bench/harbor/run-control.sh` is being written,
and the invocation below is read from Harbor's source, not yet run. Harbor
routes mini-swe-agent through litellm, and DeepSeek is a first-class provider
there (`harbor/agents/model_connection.py`, `PROVIDERS`). The model name must
carry a `provider/` prefix or mini-swe-agent refuses it.

```bash
harbor run --agent mini-swe-agent --model deepseek/deepseek-v4-flash \
  --ae DEEPSEEK_API_KEY='${DEEPSEEK_API_KEY}' \
  --dataset "terminal-bench@2.0" -i fix-git -e podman -y
```

`summarize_job.py` already reads a non-ACP trial: it reports
`turn_end_class: "n/a"` and takes tokens and cost from Harbor's own
`agent_result`, which litellm-backed agents do populate.

## Cost

A Terminal-Bench 2.0 task costs about $0.02 on `deepseek-v4-flash`. Measured by
DeepSeek account balance before and after `kj-calib-1`: three tasks, 2.65M input
tokens, $0.05. The account is shared with kaibo's DeepSeek cast, so a balance
delta is an upper bound on what a run spent, never an under-count.

Per-run token counts come from `summarize_job.py`, which reads the kernel log.
They are exact; only the dollar figure is a bound.

## Recorded baselines

One row per recorded job. `Binary → commit` is the trial provenance's
`binary.sha256` and the commit `BUILD_INFO.json` gives for it.

| Date | Job | Binary → commit | Model | Settings | Tasks | Solved | Tokens/solved | Asks | Stalls | Ended early | Verdict agreement |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-09-18 | `kj-hw-1`, `kj-fixgit-1` | not recorded (predates provenance) | deepseek-v4-flash | shipped coder, collaborative, 16K ceiling, multiplier 5 and 2 | 2 (hello-world, fix-git) | 2 | 43K and 1.39M | 0 | 0 | 0 | no verdict line |
| 2026-09-18 | `kj-calib-1` | `e9802591…` → `c9ad92c4` (dirty) | deepseek-v4-flash | shipped coder, collaborative, 16K ceiling, multiplier 1 | 3 (openssl-selfsigned-cert, regex-log, sqlite-with-gcov) | 1 | 756K | 0 | 0 | 2 | no verdict line |
| TBD | `kj-tb2-armA-shipped` | `05d77c21…` → built from `7cd1593d` | deepseek-v4-flash | shipped coder, autonomous, 32768 ceiling, multiplier 1 | 20 (tb2-subset) | TBD | TBD | TBD | TBD | TBD | TBD |
| TBD | `kj-tb2-armB-driven` | TBD | deepseek-v4-flash | `coder-driven` overlay, autonomous, 32768 ceiling, multiplier 5 | 20 (tb2-subset) | TBD | TBD | TBD | TBD | TBD | TBD |

`kj-calib-1`'s two failures: `regex-log` ended `provider_failure` when the
model's `write` call arrived with its JSON arguments cut off and the whole turn
failed; `sqlite-with-gcov` ended `iteration_cap` at 50 collaborative
iterations, with nobody there to send the follow-up it asked for.

## Known limits

`docs/issues.md`, "What running under a benchmark showed (2026-09-18)" holds
what these runs exposed and what is still open, most costly first. The two that
shape every result above: an approved command's output never reaches the model,
and a turn can end before its ask is offered. The sandbox gate tier avoids both
by allowing everything, which is why every recorded run has an ask count of
zero.

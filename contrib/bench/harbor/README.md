# Harbor runs kaijutsu

Harbor launches an agent inside the task container and judges what it left
behind. `kaijutsu-solo-acp` is one command that is a whole kaijutsu
(`docs/solo-acp.md`), so the only thing missing is getting that command into a
container that has never heard of it. `kaijutsu_solo_agent.py` is that step.

## What it does

Harbor's shipped `acp` agent can launch a `local` distribution: a command
already present in the environment (`src/harbor/agents/installed/acp.py`,
`AcpLocalTarget`). `KaijutsuSoloAcp` subclasses it and makes the command
present. Its `install()` checks the machine, uploads the static binary and the
gate policy to `/installed-agent/kaijutsu/`, makes them readable and
executable, then calls `AcpAgent.install()`, which installs the runner's Python
dependencies. It checks for a CA bundle afterwards and writes
`kaijutsu-provenance.json` into the trial's `agent/` directory.

It also builds the ACP registry entry itself, so no `agent.json` is needed.
`agent.json` here is that entry written out, for the case where the binary is
already baked into an image: `-a acp --ak registry_entry_path=agent.json`.

`kaijutsu_solo_agent.py` owns the backend and model defaults
(`DEFAULT_BACKEND_KIND`, `DEFAULT_MODEL`, `DEFAULT_RUST_LOG`). `run-harbor.sh`,
`job.yaml` and `agent.json` restate them for readability and defer to them:
leaving `KAIJUTSU_ACP_MODEL` / `KAIJUTSU_ACP_BACKEND` unset lets the module
decide.

The runner starts the binary in the task's working directory and passes that
directory as the ACP session's cwd. Solo mounts its launch directory
read-write, so the workspace is the task's own directory with no `--mount`
needed. Observed on `fix-git`, whose `WORKDIR` is `/app/personal-site`:
`kaijutsu-solo-acp: workspace mount: /app/personal-site (read-write)`.

## Running it

```bash
./run-harbor.sh --job-name kj-hello-world \
  --task-path /home/atobey/src/harbor/examples/tasks/hello-world

./run-harbor.sh --job-name kj-fix-git \
  --dataset "terminal-bench@2.0" --task fix-git

# a list of task names, one per line
./run-harbor.sh --job-name kj-subset --dataset "terminal-bench@2.0" \
  --task-file ../analysis/tb2-subset.txt

# anything after -- reaches `harbor run`
./run-harbor.sh --job-name kj-probe --task-path ... -- --install-only
```

The wrapper sources Harbor's `env.sh`, reads the provider key from its file
into the environment, runs one trial at a time under podman (`-e podman -k 1
-n 1`), writes `kaijutsu-job-provenance.json`, and scans the job directory for
the key.

Environment variables with defaults: `KAIJUTSU_ACP_BINARY`, `KAIJUTSU_ACP_GATE`,
`KAIJUTSU_ACP_MODEL`, `KAIJUTSU_ACP_BACKEND`, `KAIJUTSU_ACP_RUST_LOG`,
`KAIJUTSU_ACP_KEY_FILE`, `KAIJUTSU_ACP_KEY_ENV`, `HARBOR_ENV_SH`,
`HARBOR_JOBS_DIR`, `HARBOR_AGENT_TIMEOUT_MULTIPLIER`. `--ak key=value` sets the
same things per run; `harbor agent schema kaijutsu_solo_agent:KaijutsuSoloAcp`
prints them. `job.yaml` is the same job as a config file, for `harbor run -c`.

`KAIJUTSU_ACP_CONSENT` (`--ak consent=`) and `KAIJUTSU_ACP_MAX_TOKENS`
(`--ak max_tokens=`) pass `--consent <collaborative|autonomous>` and
`--max-tokens <N>` through to the binary. Neither has a default here: left
unset, the flag is left off the command line entirely, so the binary's own
default stands — collaborative consent, the factory output-token ceiling.
`consent` is checked against the two known values and `max_tokens` against
being a positive integer before the command is built, so a typo is a refusal
here rather than a refusal from the binary after the container is already
up. Both are recorded in `kaijutsu-provenance.json`'s `model` object.

`KAIJUTSU_ACP_RC_OVERLAY` (`--ak rc_overlay=`) names a local rc overlay
directory (see `contrib/bench/rc-variants/*/README.md` for the shape, e.g.
`coder-driven`). `install()` uploads it into the container under
`/installed-agent/kaijutsu/rc-overlay/` (`environment.upload_dir`, the same
call `AcpAgent.install()` uses for the agent's own source) and passes
`--rc-overlay /installed-agent/kaijutsu/rc-overlay` to the binary, which
applies it after the kernel seeds `/config/rc` and before any context can be
created. Left unset, the seeded rc tree is unchanged. `kaijutsu-provenance.json`'s
`rc_overlay` object records the local host path and a sha256 over the
directory's sorted relative paths and file bytes, so a job's provenance ties
a run to the exact variant that produced it even though the variant's own
files never leave the host in the trial's output.

Requirements: bash >= 4.4 (the wrapper expands possibly-empty arrays under
`set -u`) and Python >= 3.12 (Harbor's own floor — `requires-python = ">=3.12"`;
the adapter imports Harbor and runs in its interpreter).

## The provider key

The key is read from `~/.deepseek-key` into this shell's environment and given
to Harbor as the template `${DEEPSEEK_API_KEY}`, not as a literal. Harbor
resolves a template from the host environment at run time
(`harbor/utils/env.py`, `resolve_env_vars`) and persists it unchanged
(`templatize_sensitive_env`), so `config.json` in the job directory holds the
template. Verified on every run so far: `"DEEPSEEK_API_KEY": "${DEEPSEEK_API_KEY}"`.

The wrapper refuses to run under `set -x`: xtrace would print the key on the
line that reads it and again on the line that writes the scan pattern.

### The model can read the key from `/proc`, by design of the sandbox

**Observed**, with a fake key and a sentinel variable, driving the test-mock
build through Harbor's own `acp_runner.py`:

- A command the model runs through the shell **does not** see the key in its
  own environment. kaish builds a hermetic child environment — `env_clear()`
  then only the shell's exported variables
  (`kaish-kernel/src/spawn.rs:208-215`, `hermetic_env` at `:154`), and kaijutsu
  seeds that scope with `HOME` and `PATH` only
  (`crates/kaijutsu-kernel/src/runtime/context_shell.rs:89-97`). The probe's
  `env` showed `HOME`, `PATH`, `PWD`, plus the context's durable exports, and
  neither the sentinel nor `DEEPSEEK_API_KEY`.
- The same command **can** read the key out of `/proc`. Its direct parent is
  the kernel process, same uid, and the kernel holds the key in its own
  environment because that is where it reads it from. The probe reported
  `ancestor pid=… comm=kaijutsu-solo-a SENTINEL_VISIBLE KEYVAR_VISIBLE`, i.e.
  `tr '\0' '\n' < /proc/$PPID/environ` hands the model the provider key.

Inside a disposable task container, with a key scoped to the run, this is an
accepted exposure. It is **not** acceptable for a solo kernel run on a host
with a long-lived key, and it means an untrusted task's content could exfiltrate
the key through the model. The smallest fix is `prctl(PR_SET_DUMPABLE, 0)` early
in `kaijutsu-solo-acp`'s `main`, which makes `/proc/<pid>/environ` unreadable to
same-uid processes; removing the variable from the process environment instead
is harder, because backend initialization reads it from the environment on the
kernel thread, after the safe single-threaded window has closed.

### What the scan does and does not cover

It greps the job directory for the key after the run (`grep -RlFf`, pattern
from a mode-600 file, `-R` so a symlinked trial root is followed). `grep`'s
exit status is read explicitly: 0 is a leak (exit 3, files named), 1 is clean,
anything else aborts as unverified (exit 5) instead of being reported as clean.
The wrapper refuses a `--job-name` whose directory already exists, and fails
with exit 4 if the expected directory was never created, so "clean" always
means a directory was really scanned.

Not covered: Harbor's console output if the caller redirects it to a file; the
environments of running processes (the key rides `podman compose exec
-e KEY=value`, visible in that process's argv while it runs); and podman's
recorded container environment until the container is removed. Harbor's
`delete` defaults to true and `job.yaml` sets it explicitly; the wrapper relies
on that default rather than passing a flag, because Harbor's CLI exposes the
setting through the job config rather than a `harbor run` flag.

## The gate

`--gate-config ../gate-sandbox.toml` installs `[global] uncovered = "allow"`,
so no command raises an approval ask. Both real runs recorded
`permissions_requested: 0`. Without it nearly every command asks, the model is
told nothing ran, and the run costs several times the tokens — so a missing or
empty `gate_config_path` is refused. Shipping no policy is available, but only
as the explicit `--ak gate_config_path=none`.

## Why `--model` is not passed through

Harbor's runner raises when a model is requested and the agent advertises no
model-selection mechanism (`acp_runner.py`, "ACP agent did not advertise a
model-selection mechanism"). kaijutsu advertises none: its ACP session carries
no `models` list and no model config option. So Harbor's `--model` is split the
way Harbor splits it and both halves become the binary's own flags: the
provider half sets `--backend-kind`, the model half sets `--model`. A provider
kaijutsu does not know (anything but `anthropic`, `deepseek`, `openai`) is
refused rather than sent to the default backend. `--ak backend_kind=` and
`--ak solo_model=` still win over both.

The split is duplicated from `BaseAgent._init_model_info`, and the constructor
compares its result against Harbor's own `_parsed_model_provider` /
`_parsed_model_name`, so an upstream change fails loudly here.

`result.json`'s `model_info` follows Harbor's rule, not ours:
`AcpAgent.to_agent_info` (`acp.py:527-541`) builds it only when **both** the
parsed name and the parsed provider are set. Observed:

| Harbor `--model` | `model_info` |
|---|---|
| not given | `null` |
| `deepseek-v4-pro` (no slash) | `null` — no provider to parse |
| `deepseek/deepseek-v4-pro` | `{"name": "deepseek-v4-pro", "provider": "deepseek"}` |

`agent_info.version` is no longer a hardcoded string: it is
`git-<short head>[-dirty]` for the worktree this adapter lives in, falling back
to `sha256-<12 hex>` of the binary when git cannot answer.

## Static-binary prerequisites

Both are checked in `install()`, each failing with the real reason:

- **x86_64.** The binary is a static x86_64 build. `uname -m` is read from the
  last line of the exec output, not the whole buffer, so a runtime banner
  cannot be mistaken for the answer.
- **A CA bundle on disk.** `rustls-platform-verifier` carries no roots and reads
  the host store, so without one every model call fails with a TLS error.
  Harbor's ACP setup installs `ca-certificates`
  (`acp.py`, `_build_dependencies_command`, lines 664-670 for apt/apk/dnf/yum),
  which is why the check runs *after* `super().install()`. Measured on
  hello-world's own base image: `ubuntu:24.04` ships no CA bundle at all until
  that step runs.

## State directory

Each constructed agent gets `/installed-agent/kaijutsu/state-<12 hex>`, and
`install()` refuses if that directory already exists. A second install in a
reused container therefore cannot silently continue the previous kernel's
contexts and transcript. The steps of one multi-step trial share the directory,
which is the continuity a trial is meant to have. The directory is made
world-writable (`chmod -R a+rwX`) so a non-root agent user can use it; that is
deliberate inside a disposable task container and belongs nowhere else.

## Provenance

- `<job>/kaijutsu-job-provenance.json` — written by the wrapper: job name,
  task path or dataset and task names, Harbor version, timeout multiplier,
  binary and gate paths, the key's variable name (never its value), Harbor's
  exit status.
- `<job>/<trial>/agent/kaijutsu-provenance.json` — written by `install()`:
  sha256, size and mtime of the uploaded binary, sha256 of the gate file,
  worktree HEAD and whether it was dirty, backend kind and model, `RUST_LOG`,
  the detected machine and CA bundle, and the state directory.

## Reading a run

`<job>/<trial>/agent/` holds `acp-events.jsonl`, `acp-summary.json`,
`trajectory.json`, `kaijutsu-provenance.json`, and `acp.txt`. The analysis
tools in `../analysis/` read the first two directly:

```bash
python3 ../analysis/classify_run.py <job>/<trial>/agent \
  --kernel-log <job>/<trial>/agent/acp.txt
python3 ../analysis/summarize_job.py <job>
```

`acp.txt` is the runner's stdout and stderr, and the agent process inherits
that stderr, so the kernel's own log is in it. The adapter always sets
`RUST_LOG` (default `info`), so the `LLM stream completed` lines carry
per-inference token counts and `classify_run.py --kernel-log` scopes them to
this run's session. Set `rust_log` below `info` and a run keeps no token record
at all.

Harbor's own token and cost columns stay empty: kaijutsu sends no
`PromptResponse.usage`, so `agent_result.n_input_tokens` and `cost_usd` are
`null`. The kernel log is the only token source today.

## Podman: the compose banner

`podman compose` prints `>>>> Executing external compose provider … <<<<` on
every invocation, and Harbor folds a compose exec's stderr into its stdout. The
banner therefore lands in the output of any command Harbor parses. The first
casualty is `uname -s && uname -m` in `AcpAgent._detect_platform`, which fails
with `Unsupported ACP platform '>>>> Executing external compose provider …
linux'`. `run-harbor.sh` exports `PODMAN_COMPOSE_WARNING_LOGS=false` to silence
it at the source. This hits Harbor's stock `acp` agent under podman too.

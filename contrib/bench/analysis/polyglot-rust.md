# Aider polyglot, Rust slice

All 30 Rust exercises from `Aider-AI/polyglot-benchmark`, materialized as
Harbor task directories under
`/home/atobey/src/bench-work/harbor/datasets/polyglot-rust/` via the
`aider_polyglot` adapter (`adapters/aider_polyglot` in the Harbor checkout,
console script `aider_polyglot`). Task list: `polyglot-rust-tasks.txt`.

## Generation

```bash
git clone --depth 1 https://github.com/Aider-AI/polyglot-benchmark.git \
  /home/atobey/src/bench-work/harbor/repos/polyglot-benchmark
# HEAD: 7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f (2024-12-22)

cd /home/atobey/src/harbor/adapters/aider_polyglot
UV_PROJECT_ENVIRONMENT=/home/atobey/src/bench-work/harbor/aider-polyglot-venv \
UV_CACHE_DIR=/home/atobey/src/bench-work/harbor/.cache/uv \
  uv run --frozen -- aider_polyglot \
    --polyglot-root /home/atobey/src/bench-work/harbor/repos/polyglot-benchmark \
    --languages rust \
    --output-dir /home/atobey/src/bench-work/harbor/datasets/polyglot-rust
```

`UV_PROJECT_ENVIRONMENT` keeps the adapter's own `.venv` out of the read-only
`/home/atobey/src/harbor` checkout. Output: "Found 30 exercises for rust" /
"Generated 30/30 tasks", 2.2 MB total (before any container is built).
Confirms all 30 upstream Rust exercises materialized, no filtering was
needed.

## One task, inspected closely (`polyglot_rust_accumulate`, representative)

- **Image**: `FROM buildpack-deps:jammy`, then `apt-get install
  build-essential pkg-config libssl-dev`, then Rust via `rustup` (`curl
  https://sh.rustup.rs | sh`) — network access at **build** time, every
  task, every rebuild. Not vendored; no cached toolchain layer is shipped.
- **Verifier**: `tests/test.sh` (a single template shared across all six
  polyglot languages — the non-Rust branches are dead code for this slice,
  gated on `"rust" = "java"` etc., harmless but a shared-adapter quirk worth
  knowing about) copies the exercise's test file into `src/`, then:
  `cargo add regex thiserror anyhow` (unconditionally, whether or not the
  exercise needs them), `cargo generate-lockfile`, `cargo test --verbose`.
  Every one of those steps hits crates.io. **Observed**, real run
  (`gigasecond`): `test-stdout.txt` shows `Downloaded <crate> vN.N.N` for 17
  crates (`regex`, `syn`, `thiserror`, `anyhow`, `time`, transitive deps,
  ...) — nothing is vendored, so a flaky or rate-limited crates.io mirror is
  a flaky task run, not a kaijutsu or model problem.
- **Timeouts** (`task.toml`, identical across all 30 generated tasks):
  `agent.timeout_sec = 1800`, `verifier.timeout_sec = 1800`,
  `environment.build_timeout_sec = 1800`. `memory_mb = 4096`, `cpus = 1`,
  `storage_mb = 10240`, `gpus = 0`.
- **Network**: `task.toml` sets `network_mode = "public"` under **both**
  `[agent]` and `[verifier]` for every task — the agent can reach the
  network while solving, and the verifier can reach it while grading. This
  is a real, documented, per-task setting, not an environment-level default
  we're inferring.

## Pipeline proof (real, no-LLM `oracle`, sequential)

Two smallest by test-file line count (`tests/tests/*.rs`, a proxy for
exercise scope): `gigasecond` (59 lines, 64 KB task dir) and `robot-name`
(59 lines, 60 KB task dir).

```bash
harbor run -p /home/atobey/src/bench-work/harbor/datasets/polyglot-rust/polyglot_rust_gigasecond \
  -a oracle -e podman -o /home/atobey/src/bench-work/harbor/jobs \
  --job-name polyglot-rust-oracle-gigasecond -y
# real: 1m 8s.  Reward 1.0, 1/1 trials, 0 exceptions.

harbor run -p /home/atobey/src/bench-work/harbor/datasets/polyglot-rust/polyglot_rust_robot-name \
  -a oracle -e podman -o /home/atobey/src/bench-work/harbor/jobs \
  --job-name polyglot-rust-oracle-robot-name -y
# real: 26s.  Reward 1.0, 1/1 trials, 0 exceptions.
```

Both pass. `robot-name` was faster than `gigasecond` (26s vs 68s wall,
likely podman layer-cache reuse of the Rust-toolchain image layer from the
first run) — a hint that a fresh cold run (or `--no-delete`-free image
churn across 30 tasks) has real per-task build-time variance even before any
agent runs.

## Running a directory of local tasks as one Harbor job

No new option is needed. **Observed**, `--dry-run` (free, no container
built):

```bash
harbor run -p /home/atobey/src/bench-work/harbor/datasets/polyglot-rust \
  -a oracle -e podman --job-name x -y --dry-run
# Dry run OK — 30 trial(s); nothing was run.
```

`-p`/`--task-path` on a directory that contains multiple `task.toml`
subdirectories auto-discovers every one of them as a trial — this is the
same directory-of-tasks mechanism a registry dataset gives you, just local.
`-i/--include-task-name` filters it exactly like it filters a `--dataset`
selection:

```bash
harbor run -p /home/atobey/src/bench-work/harbor/datasets/polyglot-rust \
  -i polyglot_rust_gigasecond -i polyglot_rust_robot-name \
  -a oracle -e podman --job-name x -y --dry-run
# Dry run OK — 2 trial(s); nothing was run.
```

`run-harbor.sh`'s and `run-control.sh`'s existing `--task-path DIR
--task-file FILE` (task names, one per line, turned into repeated `-i`)
already expresses this combination — **confirmed**, `--dry-run` against
`polyglot-rust-tasks.txt` resolves all 30 named tasks. No gap to report.

## Running the slice

```bash
# kaijutsu-solo-acp
./run-harbor.sh --job-name kj-polyglot-rust \
  --task-path /home/atobey/src/bench-work/harbor/datasets/polyglot-rust \
  --task-file ../analysis/polyglot-rust-tasks.txt

# control arm (mini-swe-agent, same DeepSeek model)
./run-control.sh --job-name control-polyglot-rust \
  --task-path /home/atobey/src/bench-work/harbor/datasets/polyglot-rust \
  --task-file ../analysis/polyglot-rust-tasks.txt
```

Both scripts hardcode `-e podman -k 1 -n 1` — one trial at a time,
sequential, not the CLI's own `-n 4` default. At 1800s agent + up to 1800s
verify + up to 1800s build ceilings per trial, 30 trials sequential is a
real multi-hour worst case even though the two oracle proof runs above
finished in well under two minutes each; an LLM agent that struggles and
iterates will run far closer to the ceiling than oracle did. Budget a job
window accordingly, or split `polyglot-rust-tasks.txt` into smaller
`--task-file`s for parallel/incremental jobs.

## Caveats

- **Network dependence = flakiness.** Both image build (rustup install) and
  test run (`cargo add`/`generate-lockfile`/`test`, live crates.io) need
  outbound network, every trial, every task. A failed trial's exit code
  alone can't distinguish "the model's fix was wrong" from "crates.io hiccup
  mid-build."
- **Contamination.** Exercism exercises (and their reference solutions) are
  public; a model has plausibly seen them in pretraining. This slice is
  useful as a **personal-best regression** (did this change make kaijutsu
  worse at the same public exercises it solved before?), not as an
  uncontaminated capability measure — do not report it as one.
- **Uniform difficulty label.** Every generated task carries
  `difficulty = "medium"` from the adapter; it does not distinguish
  `gigasecond` (trivial) from `xorcism` (1770 test lines, clearly harder).
  Test-file line count (used above to pick the two smallest) is a rough but
  real proxy if task selection by difficulty matters later.

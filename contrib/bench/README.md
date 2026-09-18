# Bench: an ACP client driving a throwaway kaijutsu kernel

`docs/benchmarks.md` is the rerun recipe end to end — setup, static build,
suites under Harbor, results, and the recorded baselines. This file is the
host loop.

These scripts stand up a disposable kaijutsu kernel, point Harbor's standalone
ACP runner at it through `kaijutsu-acp`, and run one task unattended. Nothing
here touches the operator's kernel: the run gets its own XDG trees, its own
config root, its own keys, and a port away from the default 2222.

Everything a run writes lives under `/home/atobey/src/bench-work/`. The
kernel's read-write VFS mounts are `$HOME/src` and `/tmp`
(`crates/kaijutsu-server/src/rpc.rs:1541-1555`), so a task directory has to sit
under `$HOME/src` for the agent to edit it.

Every client these scripts start runs with `$HOME` pointed inside the run
directory and connects with `--insecure`. A throwaway kernel mints a fresh SSH
host key at every boot: without both of those, the first connection learns that
key by trust on first use into the operator's real `~/.ssh/known_hosts`, and the
next boot on the same port is refused as a host-key mismatch.

## Files

| File | What it does |
|---|---|
| `boot-kernel.sh` | Makes a run directory, generates keys, inits the root character, reseeds rc, starts the server, waits for it to answer, patches the seeded gate policy, creates the performer character, points the kernel at a model. Writes `env.sh`. |
| `stop-kernel.sh` | Stops the server named by a run directory's pidfile, after checking the pid really is that server. |
| `acp-launch.sh` | The launcher an ACP client execs. Reads the kernel's coordinates from `env.sh`. |
| `run-host-task.sh` | Runs one task through Harbor's ACP runner and collects logs. |
| `kjmcp.py` | Runs `kj` commands against a kernel over the MCP stdio bridge. There is no standalone `kj` binary. |
| `mock_scripts/` | Scripted model turns for the mock backend, one file per model name. |

## Python environment

The Harbor runner needs Python 3.12 and the `agent-client-protocol` package:

```bash
uv venv /home/atobey/src/bench-work/acp-venv --python 3.12
uv pip install --python /home/atobey/src/bench-work/acp-venv agent-client-protocol
```

`run-host-task.sh` copies the runner out of harbor's package directory before
running it. It has to: a sibling `acp.py` there shadows the `acp` package and
the import fails.

## A real run

```bash
cd /home/atobey/src/wt/kaijutsu-bench
./contrib/bench/boot-kernel.sh --run-id ds-01 --port 22724 --model deepseek
./contrib/bench/run-host-task.sh \
  /home/atobey/src/bench-work/kernels/ds-01 \
  /home/atobey/src/bench-work/tasks/py-fizz \
  "Run 'python3 -m unittest -v' in this directory. One test fails. Fix fizz.py so every test passes, then run the tests again to confirm." \
  ds-run-1
./contrib/bench/stop-kernel.sh /home/atobey/src/bench-work/kernels/ds-01
```

The kernel reads the DeepSeek key the way it always does — the factory backend
row names `~/.deepseek-key` and `DEEPSEEK_API_KEY`
(`crates/kaijutsu-kernel/src/seed_backends.rs`). No key ever reaches these
scripts.

## A mock run

The mock backend is compiled out of a normal build. Build the server with it:

```bash
cargo build -p kaijutsu-server --features kaijutsu-kernel/test-mock
cp target/debug/kaijutsu-server /home/atobey/src/bench-work/kaijutsu-server-mock
KJ_SERVER_BIN=/home/atobey/src/bench-work/kaijutsu-server-mock \
  ./contrib/bench/boot-kernel.sh --run-id mock-01 --port 22723 --model mock
```

Keep the copy. `target/debug/kaijutsu-server` is shared, and the next ordinary
`cargo build -p kaijutsu-server` replaces it with one that has no mock backend
— at which point `--model mock` fails at `kj backend set mock --kind mock`,
because `mock` is not a kind a build without the feature can parse.

`--model mock` starts the server with `KJ_MOCK_SCRIPT_DIR` pointed at
`mock_scripts/` and sets the kernel default model to `bench-mock`, which reads
`mock_scripts/bench-mock.json`. Each element of that file is one model turn;
the loop panics if it asks for more turns than the script holds.

## Reading a run

`<run-dir>/tasks/<label>/` holds:

- `acp-events.jsonl` — every `session/update` and `session/request_permission`
  the runner saw, in order. This is the record that shows what the agent was
  told and when.
- `acp-summary.json` — stop reason, permission count, any error.
- `runner.log` — the runner's stdout plus `kaijutsu-acp`'s stderr. Set
  `RUST_LOG=kaijutsu_acp=debug,info` before `run-host-task.sh` for the
  permission pump's own tracing.

`<run-dir>/logs/kernel.log` holds the kernel side. Token counts per call are
on its `LLM stream completed` lines.

## The gate

`boot-kernel.sh` lets the kernel seed its own `/config/kernel` tree at first
start, then patches the seeded `gate.toml` and adds two things: kj verbs the
admin seat needs, inside the existing `[context_type.mcp]` tier, and a
`[context_type.coder]` tier allowing `sleep`.

The order matters. A config tree is seeded only when its directory is still
empty (`crates/kaijutsu-server/src/rpc.rs`, the `dir_is_empty` arm), so writing
`gate.toml` before the first start would cost the kernel `theme.toml`,
`mcp.toml`, `continuation.toml` and the rest of the tree. `gate.toml` is
re-read at every gated submission, so patching it after the kernel answers
needs no restart.

What that leaves uncovered for a coder is most of what a coder runs, so most
commands raise an approval ask. That is deliberate — it is what exercises the
`session/request_permission` round trip. It is not *everything*: the shipped
`[global]` tier still allows `rg`, `wc`, `kj block create`, `kj block append`,
`kj stage include`, `kj stage exclude` and `kj handoff note`, and any `kj` verb
whose declared effect is Read is allowed beneath every layer.

Widen the coder tier to watch an agent work without the gate in the way:

```toml
[context_type.coder]
allow = ["sleep", "cd", "python3", "ls", "cat", "head", "tail", "grep", "echo"]
```

An allow covers a command key, never its arguments, so a redirect, a
background flag, or a `$(…)` substitution still drops the statement back to
the gate.

Do not add a second `[context_type.mcp]` table. The file then fails to parse,
and an unparseable gate policy refuses every gated submission — including the
character read behind `register_session`, which makes the kernel look broken
in a way that names the wrong cause. `boot-kernel.sh` refuses rather than write
a file with two of them.

## Stopping a run

`stop-kernel.sh` reads `<run-dir>/kernel.pid`, then checks the process before
signalling it: the cmdline must name `kaijutsu-server` and this run's
`--config-root`, and `/proc/<pid>/stat`'s start time must match the one
`kernel.ident` recorded at boot. A pid the kernel no longer owns is refused
with the mismatch named, never signalled.

# ACP fleet

```bash
cargo build -j 4 -p kaijutsu-solo-acp --features test-mock
cargo run -j 4 -p kaijutsu-acp-fleet -- run            # every host scenario
cargo run -j 4 -p kaijutsu-acp-fleet -- run crates/kaijutsu-acp-fleet/fleet/chat.toml --trace

# contained scenarios need podman and the fleet image
podman build -t kaijutsu-fleet -f contrib/Containerfile.fleet contrib
cargo run -j 4 -p kaijutsu-acp-fleet -- run crates/kaijutsu-acp-fleet/fleet/contained

# the same scenarios as cargo tests; --ignored adds the contained ones
cargo test -j 4 -p kaijutsu-solo-acp --features test-mock --test acp_fleet
cargo test -j 4 -p kaijutsu-solo-acp --features test-mock --test acp_fleet -- --ignored
```

The fleet drives an ACP agent the way an ACP client does and checks what it
leaves behind. Each scenario is one TOML file: the prompts a client sends,
the replies a scripted model gives, the answers to permission requests, what
the ACP update stream must show, and what the workspace must hold afterward.
The shape follows Harbor's task: an instruction, an environment, and a
verifier. Every scenario uses the scripted mock model, so a run spends
nothing.

Scenarios run in one of two modes:

- **Host** (`crates/kaijutsu-acp-fleet/fleet/`) tests that the safety
  layers hold: gate tiers, the classifier hook against a mock classifier,
  and the read-only shell. Verifiers are declarative.
- **Contained** (`fleet/contained/`) runs the agent in a container with no
  network and no host home, under a gate that allows everything and with
  the classifier hook off. The model runs real host programs with no asks,
  and a verifier may run a script in another container.

`acp-fleet run` prints `PASS` or `FAIL` per scenario, with every expectation
that did not hold and the agent's stderr tail. It exits 0 when all pass, 1
when any fails, and 2 when it cannot start.

## A host scenario

```toml
description = "A severe verdict raises an ask naming the verdict; deny means the command never ran."

[classifier]
verdict = "destructive"
expect_scored = ["mkdir doomed"]

[[model]]
tool_calls = [{ name = "shell_write", input = { command = "mkdir doomed" } }]
[[model]]
text = "asked"
[[model]]
text = "left it alone"

[[prompt]]
text = "make the doomed directory"
permissions = ["deny"]
permission_titles = ["says destructive"]
text_contains = ["left it alone"]
tool_calls = [{ title = "shell_write", status = "failed" }, { title = "kj" }]

[[verify]]
path = "doomed"
exists = false
```

## A contained scenario

```toml
description = "Contained yolo: the model runs real git through shell_write with no asks, and a script verifier reads the commit back."

mode = "contained"

[[model]]
tool_calls = [{ name = "shell_write", input = { command = "git init -q" } }]
# ... write, git add, git commit ...
[[model]]
text = "committed"

[[prompt]]
text = "put hello.txt under git and commit it"
text_contains = ["committed"]

[[verify]]
script = """
test "$(git log --format=%s)" = "fleet: first commit"
"""
```

## Keys

| Key | Meaning |
|---|---|
| `description` | One sentence: what the scenario proves. Required. |
| `mode` | `host` (the default) or `contained`. |
| `gate` | The gate policy, as TOML text. Default in host mode: the shipped `gate.toml` without its `[classifier]` table. Default in contained mode: `[global] uncovered = "allow"`. |
| `[classifier]` | A mock classifier; host mode only. See below. |
| `rc` | A table of rc-tree-relative path to contents, installed with `--rc-overlay`. In contained mode it is added to the overlay that turns the classifier hook off, and wins on the same path. |
| `files` | A table of workspace-relative path to contents, written before the agent starts. |
| `[[model]]` | One scripted model reply, consumed in order across all prompts. `text` and `tool_calls` expand to stream events; `events` gives the mock backend's raw events instead. |
| `[[prompt]]` | One `session/prompt`. Required, at least one. |
| `prompt.permissions` | Answers to this prompt's permission requests, in order: `allow`, `deny`, or `cancel`. The prompt must raise exactly this many. |
| `prompt.permission_titles` | One substring per request, in order, that the request's title must contain. |
| `prompt.stop_reason` | The `stopReason` the prompt must end with. Default `end_turn`. |
| `prompt.text_contains` | Substrings of the agent's message text, including text from follow-up turns. |
| `prompt.tool_calls` | When present, the tool calls the prompt must show, exactly and in order, by `title`, and optionally last `status` and `output_contains`. |
| `[[verify]]` | After the agent exits: a `path` with `exists`, `equals`, or `contains`; or, contained only, a `script`. |

Unknown keys are refused, so a misspelled expectation fails the load instead
of checking nothing. Paths must stay inside the workspace or rc tree.

A prompt also fails when its agent text contains `stream error:`, which is
what the ACP bridge sends when a model turn fails outside a prompt. An
exhausted mock script shows up this way.

## The mock classifier

```toml
[classifier]
behavior = "answer"          # or "down" (nothing listens) or "malformed" (replies are not JSON)
labels = ["informative", "caution", "destructive"]   # the ladder, least severe first; this is the default
verdict = "destructive"      # the label every command gets; required for "answer"
expect_scored = ["mkdir doomed"]     # scored, in this order; others may come between
expect_unscored = ["mkdir allowed"]  # never scored
```

The runner serves it on `127.0.0.1` and writes its URL into the gate
policy's `[classifier] url`. It speaks the protocol the classifier hook
calls (`assets/defaults/rc/lib/hooks/lfm2d.kai`); that protocol lives in
`kaijutsu_acp_fleet::classifier` alone, so a new classifier changes that
module and not the scenarios.

The classifier can raise an ask and never lower one
(`docs/gate-policy-tuning.md`, "Verdicts"). A benign verdict adds no ask of
its own, but an uncovered statement still meets the gate's own ask, titled
`shell_write: 1 statement(s) — <command>`. `fleet/classifier-benign.toml`
shows that.

## How a run works

Each scenario gets a scratch directory under
`/home/atobey/src/bench-work/fleet/` holding the workspace and the fleet
files the agent reads: the mock script, the gate policy, and the rc
overlay. The runner starts a fresh `kaijutsu-solo-acp --backend-kind mock
--model fleet-mock --gate-config <gate>`, sends `initialize`, `session/new`
with the workspace as cwd, then each prompt. After each prompt's response
it waits until the update stream has been silent for 3 seconds: permission
requests, and the follow-up turns their answers start, arrive after the
response. It closes stdin at the end, so the agent removes its own
temporary state, then checks the workspace and removes the scratch
directory. `--keep` keeps it.

A gate ask does not hold the turn open. The gated call is refused as
pending, the turn ends, and `session/request_permission` arrives. An allowed
command runs and starts a follow-up model turn, so a scenario scripts a
reply for it; `fleet/permission-allow-deny.toml` shows the whole shape.

## Contained mode

The agent runs as `podman run -i --rm --init --network=none` in the
`kaijutsu-fleet` image (`contrib/Containerfile.fleet`, Arch with `git`).
It sees three mounts: the host's agent binary at
`/opt/kaijutsu/kaijutsu-solo-acp` (read-only), the fleet files at `/fleet`
(read-only), and the workspace at `/work`, its only writable path and the
session cwd. It has no host home.

The yolo posture is two parts: a gate with `uncovered = "allow"`, and an rc
overlay replacing `lib/create/S50-lfm2d.kai` with one that sets
`LFM2D_MODE=off` and installs no hook. `fleet/contained/hook-off.toml`
proves the overlay applies.

A `script` verifier runs with `bash -xeuo pipefail` in a fresh
`--network=none` container over the same workspace, and passes on exit 0.
Scripts never run on the host; the loader refuses a `script` in host mode.

With no podman or no image, a contained scenario fails and names the build
command. It is never skipped.

The client in `kaijutsu_acp_fleet::client` is independent of kaijutsu. It
spawns any ACP agent command and exposes the raw `session/update`
notifications, so other tests can drive an agent with it directly.

# ACP fleet

```bash
cargo build -j 4 -p kaijutsu-solo-acp --features test-mock
cargo run -j 4 -p kaijutsu-acp-fleet -- run            # every host scenario
cargo run -j 4 -p kaijutsu-acp-fleet -- run crates/kaijutsu-acp-fleet/fleet/chat.toml --trace
cargo run -j 4 -p kaijutsu-acp-fleet -- run crates/kaijutsu-acp-fleet/fleet/approval

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
  layers hold: gate tiers, pre_call hooks installed through rc, and the
  read-only shell. Verifiers are declarative.
- **Contained** (`fleet/contained/`) runs the agent in a container with no
  network and no host home, under a gate that allows everything. The model
  runs real host programs with no asks, and a verifier may run a script in
  another container.

`acp-fleet run` prints `PASS` or `FAIL` per scenario, with every expectation
that did not hold and the agent's stderr tail. It exits 0 when all pass, 1
when any fails, and 2 when it cannot start.

## A host scenario

```toml
description = "A pre_call hook that exits 3 raises an ask carrying its stderr; deny means the command never ran."

[rc]
"coder/create/S60-fleet-hook.kai" = """
kj hook add pre_call '{"match_tool":"shell_write","hook_id":"fleet-hook"}' '{"type":"kaish_path","path":"/config/rc/lib/hooks/fleet.kai"}'
"""
"lib/hooks/fleet.kai" = """
cmd="$(echo $KJ_TOOL_ARGS | jq -r '.command')"
echo "fleet hook asks about: $cmd" >&2
exit 3
"""

[[model]]
tool_calls = [{ name = "shell_write", input = { command = "mkdir doomed" } }]
[[model]]
text = "asked"
[[model]]
text = "left it alone"

[[prompt]]
text = "make the doomed directory"
permissions = ["deny"]
permission_titles = ["fleet hook asks about: mkdir doomed"]
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
| `gate` | The gate policy, as TOML text. Default in host mode: the shipped `gate.toml`. Default in contained mode: `[global] uncovered = "allow"`. |
| `rc` | A table of rc-tree-relative path to contents, installed with `--rc-overlay`. Each file's directory must already exist in the seeded tree. See "Hook scenarios". |
| `files` | A table of workspace-relative path to contents, written before the agent starts. |
| `[[model]]` | One scripted model reply, consumed in order across all prompts. `text` and `tool_calls` expand to stream events; `events` gives the mock backend's raw events instead. |
| `[[prompt]]` | One `session/prompt`. Required, at least one. |
| `prompt.permissions` | Answers to this prompt's permission requests, in order: `allow`, `deny`, `cancel`, or `hold`. The prompt must raise exactly this many. `hold` sends no response until a later prompt's `release`. |
| `prompt.release` | Answers for requests earlier prompts held, oldest first, sent once this prompt's turn ends: `allow`, `deny`, or `cancel`. |
| `prompt.wait_for_text` | After the turn ends, wait until the prompt's agent text contains this, before the quiet wait. For a message that comes later than the quiet wait, such as a permission timeout. |
| `prompt.permission_titles` | One substring per request, in order, that the request's title must contain. |
| `prompt.stop_reason` | The `stopReason` the prompt must end with. Default `end_turn`. |
| `prompt.text_contains` | Substrings of the agent's message text, including text from follow-up turns. |
| `prompt.tool_calls` | When present, the tool calls the prompt must show, exactly and in order, by `title`, and optionally last `status` and `output_contains`. |
| `prompt.cancel` | `{ after_tool_call = "<title>", release = "<file>" }`: send `session/cancel` once that tool call is `in_progress`, then write the workspace file `release`. See "Cancel scenarios". |
| `[[verify]]` | After the agent exits: a `path` with `exists`, `equals`, or `contains`; or, contained only, a `script`. |
| `known_gap` | `{ finding = "F7", fails = ["verify offered"] }`: the scenario reproduces a recorded finding. It must fail, and every failure must contain one of the `fails` substrings. See "The approval matrix". |

Unknown keys are refused, so a misspelled expectation fails the load instead
of checking nothing. Paths must stay inside the workspace or rc tree.

A prompt also fails when its agent text contains `stream error:`, which is
what the ACP bridge sends when a model turn fails outside a prompt. An
exhausted mock script shows up this way.

## Hook scenarios

A scenario installs a pre_call hook the way a shipped one would be: an rc
create script for the session's type (`coder`) runs `kj hook add` with a
`kaish_path` body under `lib/hooks/`. The broker reads the body's exit code:

| Exit | Outcome | Scenario |
|---|---|---|
| 0 | Proceed to the gate. An uncovered statement still meets the gate's own ask, titled `shell_write: 1 statement(s) — <command>`. | `fleet/hook-proceeds.toml` |
| 3 | Ask; the stderr tail becomes the ask's title. | `fleet/hook-asks.toml` |
| 124 | The body timed out: ask. | none |
| any other | Deny with no ask: a hook that fails fails closed. | `fleet/hook-fails.toml` |

A hook can raise an ask and never lower one (`docs/gate-policy-tuning.md`,
"Verdicts"). A fault running the body asks, and a `kaish_path` body that
cannot be read denies. A program the tiers allow outright, or refuse, never
reaches a hook; `fleet/gate-tiers.toml` has its hook record what it saw to show that.

## The approval matrix

`fleet/approval/` runs each approval property through ACP, one scenario
per property and entry path, against `docs/issues.md`, "Approval paths:
the burn-down". Path A is `shell_write`, path B is the read-only `shell`,
and the ACP prompt is `session/request_permission`.

| Property | Path A | Path B | ACP prompt |
|---|---|---|---|
| (a) A config deny refuses and leaves a row | gap F8 | gap F8 | |
| (b) A learned allow outranks a config deny | not reachable | not reachable | |
| (c) An uncovered statement asks a model | pass | read-only by design | |
| (d) An approval runs what was shown | pass | pass | |
| (e) A deny the ask stopped still wins | pass (hook) | | |
| (f) A model cannot forget a human's rule | not reachable | | |
| (g) An unanswered prompt is offered again | | | gap F7 |

```toml
known_gap = { finding = "F7", fails = ["verify offered"] }
```

A scenario with a `known_gap` reports `GAP` while the finding holds. It
fails when no failure matches `fails`, which means the finding no longer
reproduces: remove the marker and the finding's line. Any failure that no
`fails` substring matches, and any error that stops the run, still fails
it.

Not reachable through ACP: a standing rule comes only from
`kj ledger allow|deny --remember`, and the bridge offers only allow once and
reject once. No ACP client can create a human rule, so (b), (f), and the
rule form of (e) need another surface. The RPC shells (path C) and
kaijutsu-mcp (path D) have no ACP entry.

## Cancel scenarios

```toml
[[model]]
tool_calls = [{ name = "shell_write", input = { command = "while ! [[ -f release ]]; do sleep 0.05; done; echo finished > finished.txt" } }]
[[model]]
text = "a fresh turn after the cancel"

[[prompt]]
text = "wait for the release file"
cancel = { after_tool_call = "shell_write", release = "release" }
stop_reason = "cancelled"
tool_calls = [{ title = "shell_write", status = "completed" }]

[[prompt]]
text = "are you still there"
text_contains = ["a fresh turn after the cancel"]
```

The runner waits for the named tool call to be reported `in_progress`, sends
`session/cancel`, waits for the kernel to confirm it stopped a running turn,
and only then writes `release`. The scripted command waits for that file,
so the cancel always lands while the call runs, with no timing guess.

`session/cancel` is a soft interrupt: the running tool call finishes, and
the turn ends before its next model call, with `stopReason: cancelled`. The
second prompt gets the reply the cancelled turn did not take, so a turn that
kept going, or one still holding the context, fails the scenario
(`fleet/cancel.toml`).

ACP gives no acknowledgment of a `session/cancel`, so the runner reads the
confirmation from the agent's stderr: the kernel's `turn_interrupted=true`.
`turn_interrupted=false` fails the prompt at once: the cancel found no
running turn.

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

The agent runs as `podman run -i --rm --init --network=none
--pids-limit=512` in the `kaijutsu-fleet` image
(`contrib/Containerfile.fleet`, Arch with `git`). A contained agent uses
about 70 processes and threads on a 24-core host.
It sees three mounts: the host's agent binary at
`/opt/kaijutsu/kaijutsu-solo-acp` (read-only), the fleet files at `/fleet`
(read-only), and the workspace at `/work`, its only writable path and the
session cwd. It has no host home.

The yolo posture is a gate with `uncovered = "allow"`: no statement asks, and
no hook sees a program the tier allows.

A `script` verifier runs with `bash -xeuo pipefail` in a fresh
`--network=none --pids-limit=512` container over the same workspace, and
passes on exit 0.
Scripts never run on the host; the loader refuses a `script` in host mode.

With no podman, no image, or a stale image, a contained scenario fails and
names the build command. It is never skipped. The image keeps a copy of the
Containerfile it was built from at `/opt/kaijutsu/Containerfile.fleet`; an
image whose copy differs from `contrib/Containerfile.fleet` as the fleet was
compiled is stale. Rebuild after editing the Containerfile. A newer
`archlinux:latest` does not make the image stale.

The client in `kaijutsu_acp_fleet::client` is independent of kaijutsu. It
spawns any ACP agent command and exposes the raw `session/update`
notifications, so other tests can drive an agent with it directly.
`crates/kaijutsu-solo-acp/tests/solo_acp_stdio.rs` uses it for the binary's
own boot, mount, signal, and exit tests.

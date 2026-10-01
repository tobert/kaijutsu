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
text = "left it alone"
tool_calls = [{ name = "done", input = { status = "blocked", summary = "mkdir doomed was denied" } }]

[[prompt]]
text = "make the doomed directory"
permissions = ["deny"]
permission_titles = ["fleet hook asks about: mkdir doomed"]
text_contains = ["left it alone"]
tool_calls = [{ title = "shell_write", status = "failed", output_contains = "denied by solo (deny)" }, { title = "kj", status = "completed" }, { title = "done", status = "completed" }]

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
tool_calls = [{ name = "done", input = { status = "done", summary = "committed hello.txt" } }]

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
| `session_cwd` | A workspace-relative directory `session/new` names as the session cwd. The agent still launches in the workspace root. Default: the workspace root. |
| `[[model]]` | One scripted model reply, consumed in order across all prompts. `text` and `tool_calls` expand to stream events, text first; `events` gives the mock backend's raw events instead. See "Ending a task". |
| `[[prompt]]` | One `session/prompt`. Required, at least one. |
| `prompt.permissions` | Answers to this prompt's permission requests, in order: `allow`, `allow_always`, `deny`, `deny_always`, `cancel`, or `hold`. The prompt must raise exactly this many. Each selects the offered option of that ACP kind (`allow_once`, `allow_always`, `reject_once`, `reject_always`). `hold` sends no response until this prompt's `release`, or never. |
| `prompt.release` | Answers for the requests this prompt holds, oldest first, sent while the prompt is open: once every held request has arrived, and after `on_hold`. Each answer after the first waits until the bridge has recorded the one before it. Any answer but `hold`. See "Held requests". |
| `prompt.on_hold` | `{ write = "<file>", wait_for = "<path>" }`: once every held request has arrived, write the workspace file `write`, then wait for `wait_for` to exist, before sending `release`. See "Held requests". |
| `prompt.gate` | Replace the kernel's `gate.toml` with this before sending the prompt, as an operator editing it would. Host mode only; the agent then runs with a named `--state-dir` in the scratch directory. |
| `prompt.permission_titles` | One substring per request, in order, that the request's title must contain. |
| `prompt.permission_tool_calls` | One title per request, in order: the request's `toolCall.toolCallId` must name a `tool_call` this prompt announced with exactly that title. |
| `prompt.stop_reason` | The `stopReason` the prompt must end with. Default `end_turn`. |
| `prompt.text_contains` | Substrings of the agent's message text, including any that arrive during the quiet wait. |
| `prompt.tool_calls` | When present, the tool calls the prompt must show, exactly and in order, by `title`, and optionally last `status`, ACP `kind` (an omitted kind reads as `other`), and `output_contains` (one substring, or a list that must all appear). |
| `prompt.reports_cost` | `true`: the last `usage_update` before the response must carry `cost` in USD. See "Harbor shape". |
| `prompt.cancel` | `{ after_tool_call = "<title>", release = "<file>" }`: send `session/cancel` once that tool call is `in_progress`, then write the workspace file `release`. See "Cancel scenarios". |
| `[[verify]]` | After the agent exits: a `path` with `exists`, `equals`, or `contains`; or, contained only, a `script`. |
| `known_gap` | `{ finding = "F8", fails = ["prompt 2: expected tool calls"] }`: the scenario reproduces a recorded finding. It must fail, and every failure must contain one of the `fails` substrings. See "The approval matrix". |

Unknown keys are refused, so a misspelled expectation fails the load instead
of checking nothing. Paths must stay inside the workspace or rc tree.

Every scenario is also checked for the Harbor shape; see "Harbor shape".

A prompt also fails when its agent text contains `stream error:`, which is
what the ACP bridge sends when a model turn fails outside a prompt. An
exhausted mock script shows up this way.

## Ending a task

```toml
[[model]]
text = "made it"
tool_calls = [{ name = "done", input = { status = "done", summary = "made the directory" } }]

[[prompt]]
text = "make the directory"
tool_calls = [{ title = "shell_write", status = "completed" }, { title = "done", status = "completed" }]
```

An ACP session runs a coder context, which is offered `done`, so a scripted
model ends each task the way a coder must: its last reply calls `done`, and
that prompt's `tool_calls` lists it. A reply with no tool call does not end
the turn. The kernel answers it with a notice and takes another reply, at
most twice (`docs/conversation-session.md`, "Ending a task with done"); a
script with no reply left then fails the prompt, and the agent's stderr
says the mock script was exhausted. The runner adds no `done` of its own.

The notice is a `(System, Notification)` block, which the bridge sends as
`agent_message_chunk` text. `fleet/done-nudge.toml` shows the whole shape: a
text-only reply, the notice, then a reply that calls `done`, and
`stopReason: end_turn`. The `done` call arrives with no `kind`, which ACP
reads as `other`, so Harbor names the step by its title, `done`.

A turn that ends another way needs no `done`. A cancelled turn ends at the
cancel (`fleet/cancel.toml`). A background completion starts a turn of its
own after the response, which calls `done` again
(`fleet/harbor-background-after-response.toml`).

## Harbor shape

Harbor, the Terminal-Bench harness, runs an ACP agent through one
`session/prompt` and builds its trajectory from the `session/update`
stream. After every scenario, host and contained, the runner checks the
whole transcript for what Harbor reads (`crates/kaijutsu-acp-fleet/src/shape.rs`).
A failure starts `harbor shape <invariant>:` and ends with the offending
event. The line numbers are Harbor at `9b16836`, cloned in
`~/src/research/harbor`: `acp_runner.py` is
`src/harbor/agents/installed/acp_runner.py`, and `acp.py` is its sibling.

| Invariant | What must hold | Harbor source |
|---|---|---|
| `agent-info` | `initialize` returns `agentInfo` with a name and version. | `acp_runner.py` 699–700 records it in the run summary. |
| `tool-call-first` | Every `toolCallId` is first seen in a `tool_call`, never in a `tool_call_update`. | `acp.py` 1384–1389 names the call from its first event (`_resolve_tool_name`, 321–330). |
| `tool-call-unique` | No two `tool_call`s announce the same id. | `acp.py` 1386 and 196–209 group every event by id. |
| `tool-call-title` | The announcing `tool_call` has a title, and no update clears it. | `acp.py` 321–330: with kind `other`, the title is the function name. |
| `tool-call-input` | The `tool_call` or a later update carries `rawInput`, a JSON object. | `acp.py` 1398–1400 and 313–318: `rawInput` becomes the call's arguments. |
| `tool-call-settles` | Every call announced during a prompt is `completed` or `failed` when its response arrives. | `acp.py` 1408–1409 closes a step on `completed`; the run ends at the response. |
| `permission-allow-option` | Every permission request offers `allow_once` or `allow_always`. | `acp_runner.py` 345–356 picks the first; with none it answers `cancelled`. |
| `permission-tool-call` | A permission request's `toolCall.toolCallId` names a call a `tool_call` announced before it. | `acp.py` 1314–1325 and 1391–1396 attach the request to that call's step. |
| `stop-reason` | The prompt response's `stopReason` is one ACP v1 defines. | `acp_runner.py` 804–808 records the response. |
| `run-ends-at-response` | No model work (message, thought, `tool_call`, `tool_call_update`) or permission request arrives after a prompt's response. | `acp_runner.py` 804–808: the prompt's return ends the run and closes the agent. |
| `usage-cost` | A `usage_update.cost`, when present, is `{currency: "USD", amount: <number>}`. | `acp.py` 1431–1440 and 1642–1647. |

`prompt.reports_cost` goes further: a cost must arrive before the response
(`acp_runner.py` 361–362 keeps the last `usage_update`; `acp.py` 1642–1647
reads its cost). Harbor also reads `usage` from the prompt response
(`acp.py` 1649–1659); ACP v1 puts that field behind an unstable feature,
and the bridge does not send it.

An invariant the agent breaks wherever it applies is listed in `SHAPE_GAPS`
in `crates/kaijutsu-acp-fleet/src/run.rs`, with its finding. Its failures
are excused, and the scenario reports `GAP`. A scenario that exercised it
with no failure fails, and names the line to remove. A gap particular to a
scenario uses `known_gap` instead:

```toml
known_gap = { finding = "H2", fails = ["harbor shape run-ends-at-response", "harbor shape tool-call-settles"] }
```

`SHAPE_GAPS` is empty. The open findings, H2 and H3, are in
`docs/issues.md`, "ACP fleet: what stays open". A permission request names
the model's tool call that raised its ask (`docs/acp.md`, "Permission asks,
ledger-driven"), so Harbor attaches it to that call's step. Scenarios for
the rest of what Harbor needs:

| Scenario | What it shows |
|---|---|
| `harbor-session-cwd.toml` | A relative path resolves in the `session/new` cwd, not the launch directory (`acp_runner.py` 716–717). |
| `harbor-shell-timeout.toml` | A command past its `timeout_ms` is killed; the call fails with what it printed and `killed after timeout_ms 500`. |
| `harbor-truncated-tool-call.toml` | A call whose arguments were cut off at `max_tokens` fails with the reason, and the turn goes on. |
| `harbor-permission-cancelled.toml` | A request answered `cancelled`, as Harbor's deny mode answers every one (`acp_runner.py` 342–343), denies the ask; the held turn reads the refusal and goes on. |
| `harbor-permission-background.toml` | A gated background call's request names the model's `shell_write` call, not the operation that runs the command. Gap H2. |
| `harbor-usage-cost.toml` | Gap H3: no cost is reported. |
| `harbor-background-after-response.toml` | Gap H2: a background completion starts a turn after the response. |

Model selection is not checked. Harbor sets a model only when it is run
with one (`acp_runner.py` 721–760), through `session/new`'s `models` or a
`configOptions` entry of category `model`. The bridge advertises neither,
so a Harbor run given a model fails with "ACP agent did not advertise a
model-selection mechanism". Name the model through the agent's launch
arguments instead, as `contrib/bench/harbor/` does.

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
| (b) A learned allow outranks a config deny | pass | | |
| (c) An uncovered statement asks a model | pass | read-only by design | |
| (d) An approval runs what was shown | pass, "always allow" too | pass | |
| (e) A later deny refuses before anyone is asked | pass (hook) | | |
| (e′) A rule added after the ask leaves it alone | pass | | |
| (f) A model cannot forget a human's rule | pass | | |
| (g) An unanswered prompt is offered again | | | pass |

```toml
known_gap = { finding = "F8", fails = ["prompt 2: expected tool calls"] }
```

A scenario with a `known_gap` reports `GAP` while the finding holds. It
fails when no failure matches `fails`, which means the finding no longer
reproduces: remove the marker and the finding's line. Any failure that no
`fails` substring matches, and any error that stops the run, still fails
it.

The answers `allow_always` and `deny_always` select ACP's "always" options,
which remember an exact-text rule whose creator is the ACP connection's
principal. (e′) follows Amy's ruling: "the policy at the time the command
was first evaluated should cover its lifetime". A prompt's `gate` stands in
for an operator editing `gate.toml`, which (b) needs: a config deny keeps
a command from ever asking, so the rule has to be learned first. (b) runs
its commands only under `shell_write`, so path B has no cell for it.

A model's turn holds on its own ask, so it cannot change anything between
an ask and its answer; only a sibling call in the same reply can. (d) and
(e′) use one. In (d) a sibling changes the cwd and a variable while the
ask is held. In (e′) two identical calls ask together, the first is
answered "always deny", and the second is allowed after the rule exists.

Path B has no rule scenarios, and the RPC shells (path C) and
kaijutsu-mcp (path D) have no ACP entry.

## Cancel scenarios

```toml
[[model]]
tool_calls = [{ name = "shell_write", input = { command = "while ! [[ -f release ]]; do sleep 0.05; done; echo finished > finished.txt" } }]
[[model]]
text = "a fresh turn after the cancel"
tool_calls = [{ name = "done", input = { status = "done", summary = "still here" } }]

[[prompt]]
text = "wait for the release file"
cancel = { after_tool_call = "shell_write", release = "release" }
stop_reason = "cancelled"
tool_calls = [{ title = "shell_write", status = "completed" }]

[[prompt]]
text = "are you still there"
text_contains = ["a fresh turn after the cancel"]
tool_calls = [{ title = "done", status = "completed" }]
```

The runner waits for the named tool call to be reported `in_progress`, sends
`session/cancel`, waits for the kernel to confirm it stopped a running turn,
and only then writes `release`. The scripted command waits for that file,
so the cancel always lands while the call runs, with no timing guess.

`session/cancel` is a soft interrupt: the running tool call finishes, and
the turn ends before its next model call, with `stopReason: cancelled` and
no `done`. The
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
it waits until the update stream has been silent for 3 seconds, which
catches a message a prompt did not expect, such as a permission request
or a follow-up turn from an ask that does not hold a turn. It closes stdin
at the end, so the agent removes its own temporary state, then checks the
workspace and removes the scratch directory. `--keep` keeps it.

A model's gated call holds its turn (`docs/gate-resume.md`, "The turn
holds"). The call is reported `in_progress`, then `failed` with the waiting
text, then `pending`; `session/request_permission` arrives while
`session/prompt` is still open. The bridge records the answer as a `kj`
tool call, the approved command runs, the call settles `completed` or
`failed`, and the same turn goes on with the result. A scenario scripts
one reply after the call and no follow-up turn;
`fleet/permission-allow-deny.toml` shows the whole shape. That reply calls
`done`, so the prompt's `tool_calls` ends with it. Since every
permission request a model's turn raises now arrives before the response,
the quiet wait could shrink or end at the response for such prompts; it
stays at 3 seconds until the bridge reports turn state (`docs/issues.md`,
"ACP fleet: what stays open").

## Held requests

```toml
permissions = ["hold"]
release = ["allow"]
on_hold = { write = "asked", wait_for = "d-drifted" }
```

A held request gets no response until the runner sends its `release`
answer, while the prompt is still open. The runner waits for every
request the prompt holds, runs `on_hold`, then answers. Between two
`release` answers it waits for the bridge's `kj` call for the first to
complete, so the second is decided under whatever the first changed.
`fleet/approval/d-shell-write-runs-what-was-shown.toml` uses `on_hold` to
let a sibling call change the cwd and env after the ask exists and before
the answer. A held request with no `release` answer is never answered:
`fleet/approval/g-unanswered-prompt-offered-again.toml` lets the bridge
time it out after 30 s and answers the second offer of the same ask.

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

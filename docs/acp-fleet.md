# ACP fleet

```bash
cargo build -j 4 -p kaijutsu-solo-acp --features test-mock
cargo run -j 4 -p kaijutsu-acp-fleet -- run            # every shipped scenario
cargo run -j 4 -p kaijutsu-acp-fleet -- run crates/kaijutsu-acp-fleet/fleet/chat.toml --trace

# the same scenarios as one cargo test
cargo test -j 4 -p kaijutsu-solo-acp --features test-mock --test acp_fleet
```

The fleet drives an ACP agent the way an ACP client does and checks what it
leaves behind. Each scenario is one TOML file in
`crates/kaijutsu-acp-fleet/fleet/`: the prompts a client sends, the replies
a scripted model gives, the answers to permission requests, what the ACP
update stream must show, and what the workspace must hold afterward. The
shape follows Harbor's task: an instruction, an environment, and a verifier.
Every scenario uses the scripted mock model, so a run spends nothing.

`acp-fleet run` prints `PASS` or `FAIL` per scenario, with every expectation
that did not hold and the agent's stderr tail. It exits 0 when all pass, 1
when any fails, and 2 when it cannot start.

## A scenario

```toml
description = "A model turn writes a file by a relative path, so the file lands in the session cwd."

[[model]]
tool_calls = [
  { name = "write", input = { path = "notes/fleet.txt", content = "written by the fleet\n" } },
]

[[model]]
text = "wrote it"

[[prompt]]
text = "write the file"
text_contains = ["wrote it"]
tool_calls = [{ title = "write", status = "completed" }]

[[verify]]
path = "notes/fleet.txt"
equals = "written by the fleet\n"
```

| Key | Meaning |
|---|---|
| `description` | One sentence: what the scenario proves. Required. |
| `gate` | The gate policy, as TOML text. Default: the shipped `gate.toml` without its `[classifier]` table, so a run never reaches a network classifier. |
| `files` | A table of workspace-relative path to contents, written before the agent starts. |
| `[[model]]` | One scripted model reply, consumed in order across all prompts. `text` and `tool_calls` expand to stream events; `events` gives the mock backend's raw events instead. |
| `[[prompt]]` | One `session/prompt`. Required, at least one. |
| `prompt.permissions` | Answers to this prompt's permission requests, in order: `allow`, `deny`, or `cancel`. The prompt must raise exactly this many. |
| `prompt.stop_reason` | The `stopReason` the prompt must end with. Default `end_turn`. |
| `prompt.text_contains` | Substrings of the agent's message text. The runner waits for them, so text from a follow-up turn counts. |
| `prompt.tool_calls` | When present, the tool calls the prompt must show, exactly and in order, by `title` and optionally last `status`. |
| `[[verify]]` | A check on one workspace path after the agent exits: `exists`, `equals`, or `contains`. |

Unknown keys are refused, so a misspelled expectation fails the load instead
of checking nothing. Paths must stay inside the workspace.

A prompt also fails when its agent text contains `stream error:`, which is
what the ACP bridge sends when a model turn fails outside a prompt. An
exhausted mock script shows up this way.

## How a run works

Each scenario gets a scratch directory under
`/home/atobey/src/bench-work/fleet/` holding the workspace, the mock script,
the gate policy, and the agent's `TMPDIR`. The runner starts a fresh
`kaijutsu-solo-acp --backend-kind mock --model fleet-mock --gate-config
<gate>` with the workspace as its launch directory, sends `initialize`,
`session/new` with the workspace as cwd, then each prompt. It closes stdin
at the end, so the agent removes its own temporary state, then checks the
workspace and removes the scratch directory. `--keep` keeps it.

A gate ask does not hold the turn open. The gated call is refused as
pending, the turn ends, and `session/request_permission` arrives after the
prompt's response. The runner waits for the prompt's permission requests,
answers them, and then waits for `text_contains`. An allowed command runs
and starts a follow-up model turn, so a scenario scripts a reply for it;
`fleet/permission-allow-deny.toml` shows the whole shape.

The client in `kaijutsu_acp_fleet::client` is independent of kaijutsu. It
spawns any ACP agent command and exposes the raw `session/update`
notifications, so other tests can drive an agent with it directly.

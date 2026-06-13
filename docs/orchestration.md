# Orchestration in Kaijutsu

How to drive multi-model, multi-context work — whether you're at the kj prompt
or driving via MCP. This guide is written for both audiences: the **User** who
wants to coordinate a team of bindings, and the **Agent** that needs to read,
plan, and direct from inside an MCP session.

## Mental Model

Five concepts compose. Most confusion comes from mixing them up.

| Term        | What it is                                                                                          |
| ----------- | --------------------------------------------------------------------------------------------------- |
| **User**    | The human owner. One per kernel session.                                                            |
| **Agent**   | An AI driving via MCP. Multiple Agents can share a User.                                            |
| **Peer**    | An addressable endpoint registered with the kernel — `amy@kaijutsu-app@<uuid>`, `amy@claude-code-mcp@<uuid>`. Today: kinds `app` and `mcp`. |
| **Context** | A CRDT node: document, drift mailbox, model binding, current state. The atom of conversation.       |
| **Binding** | The model wired to a context. A context bound to Haiku will run Haiku turns regardless of who's driving. |

### Two graphs

Two things flow through kaijutsu, and they use different mechanisms:

- **Content** flows along the **context DAG**. Edges are **drifts**.
  `kj drift push|pull|merge`.
- **Action** flows across the **peer set**. Mechanism is **invoke_peer** —
  ask another peer to do something on its end (focus a block in the app,
  push an update, prompt another mcp peer).

Drift carries knowledge between contexts. Invoke carries imperative across
peers. Don't conflate them.

### Driver vs. binding

The most useful distinction for orchestration: **the Agent driving a context
is not the same as the model bound to it.**

An Agent (Claude, over MCP) can be wired to a context whose binding is Haiku.
When the Agent calls `submit_input`, the kernel runs a turn for *Haiku*, not
the Agent. The Agent is the operator at the controls; Haiku is the model in
the binding.

This is the key to multi-model orchestration: an Agent reads, plans, drifts,
and invokes; the per-context binding does the per-context thinking. Different
bindings mean different viewpoints, costs, and capabilities.

## Walking the Topology

### Identity

Two `whoami`s answer different questions:

- The **MCP-server** `whoami` returns User, peer info, registered context.
- The **kernel-side** `whoami` (via `kaish_exec` or the broker) returns
  context id, label, model, provider, and `forked_from`.

Keep both in mind. The MCP layer's notion of "who am I" is about your
connection; the kernel's is about which context you're acting in.

### The DAG

```bash
kj context list --tree
```

```
  019dd95c  59197411                    anthropic/claude-haiku-4-5-20251001
  019dda1c  claude-orchestration-probe  anthropic/claude-haiku-4-5-20251001
* 019dda2d  probe-kid                   anthropic/claude-haiku-4-5-20251001
```

`*` marks your current context. Each row is `id_short  label  binding`.

### Reading another context's history

```
block_list status=done role=model      # all model output across the DAG
kernel_search query="..."              # regex across blocks
block_read block_id=...                # one block
```

These are read-only and safe — useful when you walk into a long-running
session and want to find your team.

### Handles

- **Labels** are the durable handle. Use them.
- UUID prefixes are documented but unreachable from MCP today (see Friction).
- `.parent` is documented but unreachable from MCP today (see Friction).

When forking or registering, give every context a meaningful label.

## Patterns

### 1. Drive a turn (submit-and-pump)

To make a context's binding take a turn:

```
write_input text="..."
submit_input
```

The kernel pumps the binding for as long as the consent budget allows
(see [Turn Pump](#the-turn-pump)), then pauses. New blocks accumulate in the
document. Read them with `block_list`.

The input document is CRDT-backed and shared — the User in `kaijutsu-app`,
an Agent over MCP, and other peers all see the same buffer. `submit_input`
snapshots it into a user block and clears it.

Use this when an Agent wants the binding's perspective. Let the model think;
don't think for it.

### 2. Fork to explore

```bash
kj fork --name probe-kid                                             # inherits parent's binding
kj fork --name fast-check --model anthropic/claude-haiku-4-5-20251001
kj fork --name compact-summary --compact --distill-model haiku       # cheap-then-expensive
kj fork --name approach-a --prompt "investigate the auth module"     # injects a starting drift
```

Notes:
- `kj fork` **stays on the parent** (POSIX `fork()` semantics) and returns
  the child id in `.data`. Pass `--switch` to move your session into the
  child.
- `--compact` runs an LLM distillation of the parent's history into the
  child's first block.
- `--prompt "..."` injects a drift block into the child as it's created
  AND drives the child's first autonomous turn — the child starts working
  while you keep going.
- `--exclude <block>` (repeatable) forks everything *except* named blocks —
  the repair path for a context poisoned by a giant tool output.
- Fork selectivity is growing into range filters + factory presets
  (`full`/`window`/`spawn`) — design locked in `docs/fork-filters.md`.

### 3. Drift between contexts

Drift is the content edge. Stage and flush — drifts batch:

```bash
kj drift push <label> "literal content"        # stage
kj drift push <label> --summarize              # stage an LLM-distilled summary of the source
kj drift queue                                 # see what's staged
kj drift flush                                 # deliver
kj drift pull <label> [prompt]                 # LLM digest from another context into this one
kj drift merge                                 # summarize this fork back into parent
```

A drift block lands in the target context's document. The next time anyone
submits in that context, the binding sees the drift in its history.

**Drift is passive** — receiving a drift doesn't wake the binding. Whoever
wants the binding to react has to follow the drift with a submit.

### 4. Invoke between peers

For *action* (not content), use `invoke_peer`:

```
invoke_peer nick=<peer-id> action=<verb> params=<json>
```

Used to ask another peer to do something on its end — tell the
`kaijutsu-app` peer to switch context, focus a block, surface a notification.
Or coordinate between two Agents on different MCP peers.

An orchestrating Agent uses `invoke_peer` to direct the User's attention or
coordinate across the peer set. Drift moves content between contexts;
invoke moves action between peers.

> Peer enumeration as a first-class broker tool is on the roadmap. For now,
> peers are visible through context registration and the kaijutsu-app's
> connection state.

### 5. Cheap-then-expensive

A common shape: ask Haiku to compress, hand the result to Opus.

```bash
kj fork --name big-task --compact \
        --distill-model claude-haiku-4-5-20251001 \
        --model claude-opus-4
```

The parent's history compresses through Haiku into a short summary; the
child starts with that summary plus an Opus binding. Tokens saved, lineage
preserved. Pattern works whenever the upstream context is heavy and the
forward work needs a bigger model.

### 6. Parallel exploration

```bash
kj fork --name approach-a --prompt "try X"   # stays on the orchestrator;
kj fork --name approach-b --prompt "try Y"   # each child starts its own turn

# (children run autonomously; --prompt drove their first turn)

kj drift pull approach-a "summarize what you tried"
kj drift pull approach-b "summarize what you tried"
```

Fan out, let bindings think in parallel, pull the digests back. Children
share the block store but have isolated histories.

### 7. Personas (tool surface, not prompt)

Personas narrow the tool surface a binding can reach during a turn:

```
personas_list
personas_apply name=explorer
```

Today personas are an instance allowlist (e.g. `explorer` = `builtin.file`,
`builtin.block`, `builtin.resources`, `builtin.kernel_info` — read-mostly).
They do **not** include a system prompt yet. Use them when an orchestrator
wants a child binding to be tool-bounded by role.

## The Turn Pump

When does a binding actually take a turn?

- **`submit_input` triggers a turn.** That's the event.
- The pump runs the binding for up to the **consent budget**. In
  collaborative mode the kernel pauses after a few agentic iterations with
  a `Paused after N agentic iteration(s)` message. The User or an Agent has
  to submit again to resume. In autonomous mode the budget is broader.
- **Drift arrival does NOT trigger a turn.** Drifts land as blocks; the
  next submit incorporates them.
- **`invoke_peer` does NOT trigger a turn** in the receiving context (it
  triggers an action on the receiving peer — e.g. "focus this block").

Implication: an Agent that wants a binding to react to a drift must follow
the drift with a submit — its own `submit_input` to that context, or an
`invoke_peer` to a peer that will submit there.

## Known Friction

Workarounds for today, what would unbreak each.

### `shell` returns structured JSON

The `shell` tool returns a JSON envelope, not bare stdout. Parse it; don't
text-match the body.

```json
{
  "stdout": "...",
  "exit_code": 0,
  "status": "done",
  "block_id": "<key>",
  "content_type": "text/plain",
  "ephemeral": false,
  "data": null,
  "elapsed_ms": 42
}
```

- `exit_code` is the real kaish/tool exit code. Detect failure with
  `exit_code != 0` rather than scanning stdout for "error".
- `data` is the structured payload from kj's `KjResult::data` when present
  — JSON arrays for `kj … list`, objects for `kj … inspect`, `null`
  otherwise.
- `status` is `done` / `error` / `timeout` / `stream_closed`. The latter
  two come with `exit_code: -1` and an `error` field; `block_id` still
  points at a command block you can `block_read` later.

Completion is event-driven (subscribes to `BlockStatusChanged` with a
500ms store-check fallback) — the 300s `timeout_secs` is a safety cap,
not the polling cadence.

### kaish argv lexer eats kj context refs

**Symptom:**

```
kj drift push 019dda1c "..."
# lexer error: identifier cannot start with digit: 019dda1c

kj drift push .parent "..."
# source: parent: io error: No such file or directory
```

Two of three documented context-ref forms (UUID prefix and `.parent`) are
eaten by kaish before kj parses them.

**Workaround:** always reference contexts by **label**.

```bash
kj drift push claude-orchestration-probe "fact"
```

Pick labels at fork/register time so this is feasible. See
`gotcha_kaish_kj_args.md` for details.

### `kj fork` auto-switches

**Symptom:** after `kj fork --name child`, your current context is `child`,
not the original.

**Workaround:** switch back explicitly.

```bash
kj fork --name child
kj context switch <parent-label>
```

### Drift / fork / context live on `kj`, not as broker tools

**Shape:** `tool_search "drift"`, `"fork"`, `"context"` return empty matches
— by design. These verbs live in `kj` (via `shell`) rather than as
individual broker tools. The tool-surface footprint of lifting 30+ verbs
into broker namespace was judged worse than keeping `kj` as the rich
entry point. See `docs/kj-cleanup.md` for the locked direction.

**How to use them:** drive `shell "kj …"`. The new structured return
gives you `exit_code` for failure detection and `data` for structured
results — no text-matching needed. Completion (bash/zsh/fish + in-process
hooks) is a follow-up once kaish migrates to clap_derive.

## A Worked Example

A short recipe that exercises the basics: register, observe, fork, drift.

```
# 1. Register and look around
register_session label="orchestrator"
kaish_exec whoami                              # confirm registered context
shell "kj context list --tree"         # see the DAG (read result block)

# 2. Fork an explorer with a different binding
shell "kj fork --name explorer --model anthropic/claude-haiku-4-5-20251001"
# fork auto-switches — switch back:
shell "kj context switch orchestrator"

# 3. Send the explorer a starting prompt via drift
shell 'kj drift push explorer "investigate the auth module"'
shell "kj drift flush"

# 4. Move to the explorer and let it think
shell "kj context switch explorer"
write_input text="investigate as instructed; report findings"
submit_input
# wait, then block_list to see the binding's response

# 5. Pull the explorer's findings back
shell "kj context switch orchestrator"
shell 'kj drift pull explorer "summarize what you found"'
# new drift block in orchestrator's history; submit again to use it
```

Each `shell` call returns the structured JSON envelope. Read
`exit_code` to detect failures; read `data` when a `kj … list` /
`kj … inspect` call provides one. The verbs themselves stay in `kj` —
that namespace is the design center, not a transitional surface.

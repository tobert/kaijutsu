# Council API v2 (draft)

Council v2 is a small HTTP contract for a held-context model server: hold
chat contexts with snapshots at the boundaries the client names, then read
them three ways. A server that implements it serves kaijutsu's chat seats and
its council from one store, so the two share KV and one eviction policy.

This draft replaces `docs/council-api.md` (v1) and the three servers' own
context routes: the HIP megakernel's `/mk/v1`, lfm2d's `/v1/opinion`, and
mkviolet, which serves both. "Converting" below says what each one changes.
Nothing here is built yet.

## What changes from v1

- **Contexts are native chat.** A context is `messages` in the model's own
  chat shape, tool calls and tool results included, not v1's system and
  `turns`. The council bench measured why (mkviolet,
  `web/council.html`): Gemma 4 keeps its own thinking in view only inside
  one agent turn. Seats built that way put more than 0.9999 of the
  probability on the answer letters on every case tried, where v1's lettered
  menu put a median of 0.94 on `verdict`.
- **One store, three reads.** `decisions` (typed, pooled), `reads` (raw
  next-token letters), and `generations` (sampled replies) all start from the
  same held snapshots. v1 had only decisions; generation lived in each
  server's private routes.
- **A fork is a new context that starts at another context's snapshot.** A
  client never patches a held context's middle; it forks. The server reports
  which ancestor snapshot it reused.
- **The case can arrive as a tool result.** A context that ends with a call
  to its queue tool receives the case as that call's result, and the model
  answers by calling the tool again.

## Terms

| Term | Meaning |
|---|---|
| context | A held chat under an id the client chose: messages, tools, and whether the model thinks. The client owns the id; the server owns the snapshots. |
| snapshot | Held model state at one boundary of one build. Its id is opaque, derived by the server from the tokens, the boundaries along the way, and the server's identity. |
| boundary | The end of a message the client marked `snap`. A snapshot exists only at a boundary. |
| spec | A held question set, content-addressed by its canonical JSON. Unchanged from v1. |
| case | The `state` one decision reads. |
| read | One context's answers to one case. |
| identity | The model, weights, tokenizer, template, and engine that produced a number. |

## Endpoints

| Method and path | Does |
|---|---|
| `GET /council/v2/identity` | Identity, limits, and capabilities. |
| `PUT /council/v2/contexts/{id}` | Holds or updates a context. The body is the whole context. |
| `GET /council/v2/contexts/{id}` | Its head, snapshots, and their tiers. |
| `DELETE /council/v2/contexts/{id}` | Drops the record and its pins. |
| `POST /council/v2/specs`, `GET`, `GET /{spec_id}`, `DELETE /{spec_id}` | As v1. |
| `POST /council/v2/decisions` | Typed questions from a spec, pooled over contexts. |
| `POST /council/v2/reads` | Raw next-token letters after a suffix. Capability `reads`. |
| `POST /council/v2/generations` | A sampled reply from a context. Capability `generate`. |

## Contexts

```json
PUT /council/v2/contexts/0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b12
{"thinking": true,
 "tools": [{"type": "function", "function": {"name": "next_command", "description": "…", "parameters": {…}}}],
 "queue": "next_command",
 "messages": [
   {"role": "system", "content": "You are a policy expert for shell commands. …"},
   {"role": "user", "content": "Commands from a coding agent wait in a queue. …", "snap": true},
   {"role": "assistant", "reasoning": "I should get the first command.",
    "tool_calls": [{"id": "c0", "name": "next_command", "arguments": {}}]},
   {"role": "tool", "tool_call_id": "c0", "content": "{\"command\": \"ls -la src\"}"},
   {"role": "assistant", "reasoning": "ls only lists. …",
    "tool_calls": [{"id": "c1", "name": "next_command", "arguments": {"answers": {"verdict": "A"}}}],
    "snap": true}]}
-> {"id": "0199b3c4-…7b12", "head": "snap:51d0…", "tokens": 1840, "kept": 0, "fed": 1840,
    "from": null, "dry_run": false,
    "snapshots": [{"id": "snap:9a1e…", "message": 1, "tokens": 412, "tier": "device"},
                  {"id": "snap:51d0…", "message": 4, "tokens": 1840, "tier": "device"}]}
```

- **Messages are the model's chat shape.** Roles are `system`, `user`,
  `assistant`, and `tool`. An assistant message may carry `content`,
  `reasoning` (its thinking), and `tool_calls`; a `tool` message names the
  `tool_call_id` it answers. The server renders them with its model's own
  template, so a client never writes template tokens.
- **The server renders thinking where the model was trained to see it.**
  For a model whose template drops earlier thinking, such as Gemma 4 and
  Qwen 3, `reasoning` before the last user message is not rendered; inside
  one agent turn it is. The rule is the template's, so it is part of
  `template` in identity.
- **`snap: true` marks a boundary** at the end of that message. A `PUT`
  compares the body with what the server holds, message by message and flag
  by flag, and feeds from the first difference. It extends only from a
  boundary.
- **`from`** in the reply names the ancestor snapshot the build started
  from, held by this context or another, or `null`. `kept` counts its
  tokens.
- **`fork: {"context": <id>, "at": <snapshot>}`** in a `PUT` asserts the new
  context's messages extend that snapshot. When they do not, the answer is a
  `409`; when the snapshot is gone, a `409` naming the context's current
  head. A client forks after a context fills, after an exclude, or after an
  edit; the parent is never changed.
- **`queue`** names a tool in `tools`. When the messages end with an
  assistant call to it, a decision delivers the case as that call's result,
  and the model answers by calling it again (see Decisions).
- **`pin`, `persist`, `warm`, `dry_run`, and `If-Match`** work as in v1.
  `pin` holds the snapshot's state, not just its record. A pinned snapshot
  may move to a slower tier; it is never dropped.
- **`tier`** says where a snapshot's state lives: `device`, `host`, or
  `disk`. A read from a `host` or `disk` snapshot loads it first and reports
  the time in `ms.load`.

## Decisions

Unchanged from v1 except where noted: `spec_id` or inline `questions`,
`contexts` with optional `at`, `state`, `pool`, `timeout_ms`, the answers and
their exact relations, `leave_one_out`, and the errors.

- **The case goes where the context expects it.** For a context with no
  `queue`, as in v1: in its own user turn after the spec. For a context that
  ends with a call to its `queue` tool: as that call's result. The server
  builds the queue tool's `answers` schema from the spec, one property per
  question with its options as an enum, so the model reads the questions in
  the tool definition. Either way, how a spec is compiled is `template`.
- **`think`** sets the thought before the answer: `{"tokens": 0}` closes it,
  as v1 always did; `{"tokens": N}` lets the model think greedily, up to N
  tokens, and each read reports its `thought`. Greedy keeps rule 1: the same
  snapshot and case give the same thought and the same numbers. Capability
  `think`.

## Reads

The raw System 1 read that decisions are built on, for clients that compile
their own questions.

```json
POST /council/v2/reads
{"contexts": [{"id": "0199b3c4-…7b12"}],
 "suffixes": ["<channel|><|tool_call>call:next_command{answers:{verdict:<|\"|>"],
 "letters": ["A", "B", "C"], "top": 5}
```

- Each suffix is read after the context's head, which is never changed.
- **A suffix is the client's own structure**, so its special-token text
  tokenizes as single ids. This is the one place content may be control;
  `signals.control_text` still reports each hit.
- Each read returns the letters' log probabilities, their `mass`, and the
  `top` tokens, so a client sees where the rest of the probability went.
  With several contexts, the pooled distribution follows the decision rules.

## Generations

```json
POST /council/v2/generations
{"context": "0199b3c4-…7b12", "messages": [{"role": "user", "content": "how did it go?", "snap": true}],
 "max_tokens": 2048, "sample": {"temperature": 1.0}, "stream": true}
```

- **A generation appends to a context.** The messages it adds and the reply
  it writes become the context's new head, with boundaries where they are
  marked; the reply reports the snapshots, as a `PUT` does.
- **`stream: true`** sends server-sent events: `token`, then `done` with the
  reply's `content`, `reasoning`, `tool_calls`, `finish`, and `usage`
  (`kept`, `fed`, `completion`). Closing the stream cancels the reply.
- **A prompt that leaves no room is a `413`** with `error.type`
  `context_full` and the counts, before anything runs. The client forks.

## Rules every server keeps

v1's six rules hold: raw numbers replay bit for bit; content is never
control, except a `reads` suffix; each context describes for itself; no
winner is decided; failures are explicit; the rendering is the server's.
One more:

7. **A held context never changes in the middle.** A `PUT` that differs
   before the last boundary rebuilds from the first difference, and the
   reply's `kept` says so. Nothing is spliced out of held state, so a number
   never comes from history the context no longer has.

## Identity, limits, capabilities

As v1, plus:

| Limit | Means |
|---|---|
| `context_tokens` | The longest context, its spec layer, and a case, or a generation's prompt and reply. |
| `snapshot_bytes` | What one boundary snapshot costs on the device. |

| Capability | Means |
|---|---|
| `reads` | `POST /council/v2/reads`. |
| `generate` | `POST /council/v2/generations`. |
| `think` | Decisions accept `think` with `tokens` above 0. |
| `fork` | `PUT` accepts `fork`. |
| `tiers` | Snapshots report `tier`, and pinned snapshots move between tiers. |

## Extensions

A server serves the model-specific rest under its own prefix and lists it in
`identity.extensions`. mkviolet keeps `/mk/v1` for these:

| mkviolet route | Why it is not in the council API |
|---|---|
| `render`, `tokenize`, `detokenize` | The template and tokenizer, for debugging and bench tools. |
| `embeddings` | Gemma 4's last hidden state; quality unmeasured. |
| `status` (event stream), `metrics` | Engine passes per card. |
| `traces` | Residual and attention traces. |
| `jlens`, `steer`, `routing` | Jacobian Lens, residual steering, and MoE routing introspection (`docs/06-council-jlens-extensions.md` in mkviolet). |

## Converting

| From | Changes |
|---|---|
| council v1 | `system` and `turns` become `messages`, with the system message first. `snap` moves onto messages. Contexts gain `tools`, `thinking`, `queue`, and `fork`. Decisions gain `think`. Paths move to `/council/v2`. |
| HIP megakernel `/mk/v1` | Context ids become client-chosen; the content-hash id becomes the snapshot id. `POST /contexts` becomes `PUT /contexts/{id}`. `reads` takes `contexts` and is otherwise the same. `generate`'s `from.context` becomes `generations` on a context, and its reply becomes the head. |
| lfm2d `/v1/opinion` | Held specs and describe-first carry over as `specs` and `text` questions. Contexts move from a domain-tagged token hash to client ids with `snap` boundaries. |
| mkviolet | One snapshot store for council and generation slots. `/mk/v1/choices` goes, replaced by `reads`. Snapshots record their boundaries, which a sliding-layer ring requires (`docs/07-long-context.md` in mkviolet). |
| kaijutsu | The mk provider sends each chat as a held context with `snap` on user turns and tool results, generates on it, and forks with `fork.at` after a context fills or an exclude. The council's seats become agent-loop contexts with a `queue` tool. |

## Open questions

- **Snapshot ids across servers.** They stay opaque and per server. A
  client that moves a context between servers sends the messages again.
- **The queue tool on models without tool training.** A server without a
  tool-call template answers `queue` with a `400`; whether a plain-turn
  delivery should fall back silently is open.
- **Thinking at decision time.** `think` costs about 21 ms a token on
  mkviolet. Whether it pays, and at what length, is the bench's next
  measurement.
- **Tool-result size.** A tool result is written whole into a context, even
  when the context then has no room for another turn, so a fork can carry
  it. The client's preview and storage rules for large results are its own.

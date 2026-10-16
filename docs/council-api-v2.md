# Council API v2 (draft)

Council v2 is a small HTTP contract for a held-context model server: hold
chat contexts with snapshots at the boundaries the client names, then read
them three ways. A server that implements it serves kaijutsu's chat seats and
its council from one store, so the two share KV and one eviction policy.

This draft replaces `docs/council-api.md` (v1) and the three servers' own
context routes: the HIP megakernel's `/mk/v1`, lfm2d's `/v1/opinion`, and
mkviolet, which serves both. "Converting" below says what each one changes.
mkviolet serves this draft (`src/council_v2.rs`, `src/held.rs`), and
kaijutsu's mk provider and council speak it on the `council-v2` branch.

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
- **A boundary exists only where the rendering keeps it.** A marked message
  whose rendering is not a prefix of the whole context is not a boundary,
  and the reply lists no snapshot for it. In Gemma 4 this is a message
  inside an agent turn once a later user message follows, because the
  template then drops that turn's thinking. The end of a user message is
  always a boundary, and so is the end of a run of tool results until the
  next user message.
- **Every call carries an `id`**, printable text the server never renders, and a
  `tool` message names the call it answers in `tool_call_id`, a call of the
  assistant message before its run of tool messages.
- **The head is held** after every `PUT`, marked or not, so a decision or
  a generation starts from it.
- **`thinking`** absent is the server's default; mkviolet's is on.
- **`from`** in the reply names the ancestor snapshot the build started
  from, held by this context or another, or `null`. `kept` counts its
  tokens.
- **`fork: {"context": <id>, "at": <snapshot>}`** in a `PUT` asserts the new
  context's messages extend that snapshot. When they do not, the answer is a
  `409` of type `fork_mismatch`; when the snapshot is gone, a `409` of type
  `snapshot_gone` naming the context's current head. A client forks after a context fills, after an exclude, or after an
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
  ends with a call to its `queue` tool: as that call's result. Either way,
  how a spec is compiled is `template`.
- **The client writes the queue tool's `answers` schema.** The tools render
  at the top of the context, before any spec, so the server cannot add them
  at decision time. Each asked question is an `answers` property whose
  `enum` is the letters of its options in order, `A`, `B`, and so on, with
  the options described in the property. The server checks this and answers
  a `400` naming `queue` otherwise.
- **A queue context's questions read the way the model writes one call:**
  in the order the template writes the keys, each after the most likely
  letters of the ones before it. Unlike v1's reads, these answers condition
  each other. A decision's contexts all end with a call to their queue tool,
  or none do.
- **A decision's contexts fit the decision limit,** `decision_context_tokens`
  in identity, which can be shorter than `context_tokens`. mkviolet reads
  decisions in a 4,096-position slot.
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

- Each suffix is read after the context's head, which is never changed, or
  after `at`, a boundary the context holds.
- **A suffix is the client's own structure**, so its special-token text
  tokenizes as single ids. This is the one place content may be control;
  `signals.control_text` still reports each hit.
- Each read returns its `context`, `snapshot`, and `suffix` index, the
  letters' log probabilities as `logprobs`, their `mass`, and the `top`
  tokens as `{token, id, logprob}`, so a client sees where the rest of the
  probability went. Reads are not pooled; a client that pools follows the
  decision rules.

## Generations

```json
POST /council/v2/generations
{"context": "0199b3c4-…7b12", "messages": [{"role": "user", "content": "how did it go?", "snap": true}],
 "max_tokens": 2048, "sample": {"temperature": 1.0}, "stream": true}
```

- **A generation appends to a context.** The messages it adds and the reply
  it writes become the context's new head, with boundaries where they are
  marked; the reply reports the snapshots, as a `PUT` does.
- **`messages` may be empty:** the reply then follows the context as it is.
- **`stream: true`** sends server-sent events: `token`, as `/mk/v1`'s, then
  `done` with the reply's `content`, `reasoning`, `tool_calls`, `finish`,
  `usage` (`prompt`, `kept`, `fed`, `completion`), and the new `head` and
  `snapshots`. Closing the stream cancels the reply, and the context stays
  as it was. Without `stream`, `done` is the JSON response.
- **The server names each call of the reply** with the `id` a later `tool`
  message answers. A call that does not parse is kept as written,
  `{id, name, raw, error}`, and left out of the context's record.
- **The reply is not a boundary.** The server holds the head after it when
  that costs at most a few tokens of feeding; a client marks the next user
  message or tool result `snap`.
- **A prompt that leaves no room is a `413`** with `error.type`
  `context_full`, `prompt_tokens`, and `context_tokens`, before anything
  runs. The client forks.

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
| `context_tokens` | The longest context, or a generation's prompt and reply. |
| `decision_context_tokens` | The longest context a decision reads, before its spec layer and case. |
| `snapshot_bytes` | What one token of a snapshot costs. |

`extensions` lists the server's own route prefixes, such as `["/mk/v1"]`.

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
| mkviolet | Done: one snapshot store for council v2 contexts, generations, and decisions; each snapshot holds the rows after its parent boundary. `/mk/v1/choices` is gone, replaced by `reads`. v1 contexts and `/mk/v1/generate` keep their own stores. |
| kaijutsu | The mk provider sends each chat as a held context with `snap` on user turns and tool results, generates on it, and forks with `fork.at` after a context fills or an exclude. The council's seats become agent-loop contexts with a `queue` tool. |

## Open questions

- **Snapshot ids across servers.** They stay opaque and per server. A
  client that moves a context between servers sends the messages again.
- **The queue tool on models without tool training.** A server without a
  tool-call template answers `queue` with a `400`; whether a plain-turn
  delivery should fall back silently is open.
- **Thinking at decision time.** `think` costs about 21 ms a token on
  mkviolet, which does not serve it yet. Whether it pays, and at what
  length, is the bench's next measurement.
- **Durable context records.** Snapshots outlive a restart on disk;
  context records do not, so a client `PUT`s each context again.
- **Tool-result size.** A tool result is written whole into a context, even
  when the context then has no room for another turn, so a fork can carry
  it. The client's preview and storage rules for large results are its own.

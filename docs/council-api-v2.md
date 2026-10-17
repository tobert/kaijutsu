# Council API v2 (draft)

Council v2 is a small HTTP contract for a held-context model server. The
client sends a whole chat context with every request; the server keeps
snapshots of model state at the boundaries the client names and starts each
request from the longest one it holds. Contexts are read three ways. A server
that implements it serves kaijutsu's chat seats and its council from one
store, so the two share KV and one eviction policy.

This draft replaces `docs/council-api.md` (v1) and the three servers' own
context routes: the HIP megakernel's `/mk/v1`, lfm2d's `/v1/opinion`, and
mkviolet, which serves both. "Converting" below says what each one changes.
mkviolet serves this draft (`src/council_v2.rs`, `src/held.rs`), and
kaijutsu's mk provider and council speak it on the `council-v2` branch.

## What changes from v1

- **The server keeps no context records.** A request carries its context
  whole, and the server's only state is snapshots, which are named by their
  tokens, and specs, which are named by their content. Neither can disagree
  with the client: a client that sends a context gets that context, and a
  restart, a second client, or a lost record costs feeding, never a wrong
  answer. v1's context ids, `If-Match`, and "PUT it again" are gone.
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
- **The case can arrive as a tool result.** A context that ends with a call
  to its queue tool receives the case as that call's result, and the model
  answers by calling the tool again.
- **Pins have owners.** A client pins the snapshots it needs under its own
  name, for a time it renews.

## Terms

| Term | Meaning |
|---|---|
| context | A chat the client sends whole: messages, tools, and whether the model thinks. It is a value, not a record; the client keeps it. |
| snapshot | Held model state at one boundary of a context. Its id is derived by the server from the tokens, the boundaries along them, and the server's identity, so a snapshot never changes. |
| boundary | The end of a message the client marked `snap`. A snapshot exists only at a boundary or at a context's end. |
| head | The snapshot at a context's end. |
| pin | A claim that keeps a snapshot from eviction, held by an owner until its time runs out. |
| spec | A held question set, content-addressed by its canonical JSON. Unchanged from v1. |
| case | The `state` one decision reads. |
| read | One context's answers to one case. |
| identity | The model, weights, tokenizer, template, and engine that produced a number. |

## Endpoints

| Method and path | Does |
|---|---|
| `GET /council/v2/identity` | Identity, limits, and capabilities. |
| `POST /council/v2/contexts` | Builds a context and holds its snapshots. |
| `GET /council/v2/snapshots/{id}` | A snapshot's tokens, tier, size, and pins. |
| `PUT /council/v2/pins/{owner}`, `GET`, `DELETE` | An owner's pins. |
| `POST /council/v2/specs`, `GET`, `GET /{spec_id}`, `DELETE /{spec_id}` | As v1. Specs survive a restart. |
| `POST /council/v2/decisions` | Typed questions from a spec, pooled over contexts. |
| `POST /council/v2/reads` | Raw next-token letters after a suffix. Capability `reads`. |
| `POST /council/v2/generations` | A sampled reply after a context. Capability `generate`. |

## Contexts

Every request that reads a context carries it as this object:

```json
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
```

`POST /council/v2/contexts` builds one ahead of its first read, such as a
council seat at startup:

```json
POST /council/v2/contexts
{"context": {"thinking": true, "tools": […], "queue": "next_command", "messages": […]}}
-> {"head": "snap:51d0…", "tokens": 1840, "kept": 0, "fed": 1840, "from": null, "dry_run": false,
    "snapshots": [{"id": "snap:9a1e…", "message": 1, "tokens": 412, "tier": "host"},
                  {"id": "snap:51d0…", "message": 4, "tokens": 1840, "tier": "host"}]}
```

- **Messages are the model's chat shape.** Roles are `system`, `user`,
  `assistant`, and `tool`. An assistant message may carry `content`,
  `reasoning` (its thinking), and `tool_calls`; a `tool` message names the
  `tool_call_id` it answers. The server renders them with its model's own
  template, so a client never writes template tokens.
- **An assistant's text renders before its tool calls,** as the model
  writes them.
- **A system message may come later.** The first message is usually
  `system`. A later `system` message is system text at that point in the
  conversation, such as an instruction from the client's runtime. It is not
  a user message, so it does not end an agent turn: in Gemma 4 it renders as
  its own system turn and the turn's thinking before it stays. Like a user
  message, it carries no `reasoning` or `tool_calls`.
- **The server renders thinking where the model was trained to see it.**
  The rule is the template's, so it is part of `template` in identity.
  mkviolet renders Gemma 4 with the template's `preserve_thinking` on: an
  assistant message with tool calls keeps its `reasoning` for the rest of
  the context, so an agent turn renders the same after the next prompt and
  a held prefix stays a prefix; a text answer's `reasoning` is not rendered
  once a later user message follows.
- **`snap: true` marks a boundary** at the end of that message. The server
  renders the context, starts from the longest boundary it holds, feeds
  the rest, and holds a snapshot at each boundary it passes and at the end.
  The reply says what it reused: `from` names the snapshot the build started
  from, held from this context or any other, or `null`; `kept` counts the
  tokens it did not feed.
- **A boundary exists only where the rendering keeps it.** A marked message
  whose rendering is not a prefix of the whole context is not a boundary,
  and the reply lists no snapshot for it. In Gemma 4 this is a text answer
  with thinking once a later user message follows, because the template
  then drops that thinking. The end of a user message and the end of a run
  of tool results are always boundaries.
- **Every call carries an `id`**, printable text the server never renders, and a
  `tool` message names the call it answers in `tool_call_id`, a call of the
  assistant message before its run of tool messages.
- **`thinking`** absent is the server's default; mkviolet's is on.
- **`queue`** names a tool in `tools`. When the messages end with an
  assistant call to it, a decision delivers the case as that call's result,
  and the model answers by calling it again (see Decisions).
- **A changed context is a new context.** A client that edits or drops an
  earlier message sends the new messages; the server starts from the last
  boundary they share with anything it holds. There is no fork request:
  sharing a prefix is the fork.
- **`dry_run: true`** renders and reports `kept`, `fed`, and `from`
  without feeding.
- **`tier`** says where a snapshot's state lives: `device`, `host`, or
  `disk`. A read from a `host` or `disk` snapshot loads it first and reports
  the time in `ms.load`.

## Snapshots and pins

```json
GET /council/v2/snapshots/snap:51d0…
-> {"id": "snap:51d0…", "tokens": 1840, "tier": "host", "bytes": 829030400,
    "pins": [{"owner": "kernel-7f3a", "expires_in_s": 3412}]}

PUT /council/v2/pins/kernel-7f3a
{"snapshots": ["snap:51d0…", "snap:c4e2…"], "ttl_s": 3600}
-> {"owner": "kernel-7f3a", "snapshots": [{"id": "snap:51d0…", "held": true},
                                          {"id": "snap:c4e2…", "held": false}],
    "expires_in_s": 3600}
```

- **An owner is any printable name a client chooses,** up to 128
  characters and without a slash, such as a kernel id. It names pins only.
- **`PUT` replaces the owner's whole set** and renews its time, so a client
  that restarts sends its set again and drops what it no longer needs.
  `DELETE` drops the set; `GET` lists it. An owner with no pins is a `404`.
- **A pin keeps a snapshot and the snapshots it is built on** from
  eviction, in memory and on disk. A pinned snapshot may move to a slower
  tier; it is never dropped. A pin on a snapshot the server does not hold
  reports `held: false` and holds nothing: the client builds the context.
- **Pins expire** after `ttl_s` seconds, at most a day, without a `PUT`, so
  a client that goes away leaves nothing held. Pins survive a restart until
  they expire. A snapshot names the owners that pin it, not the ones whose
  pins keep it as part of a chain.
- **The limit is `pin_bytes`** in identity. A `PUT` past it is a `413` that
  pins nothing.

## Decisions

Unchanged from v1 except where noted: `spec_id` or inline `questions`,
`contexts`, `state`, `pool`, `timeout_ms`, the answers and their exact
relations, `leave_one_out`, and the errors.

```json
POST /council/v2/decisions
{"spec_id": "sha256:9c1f…", "state": {"command": "rm -rf build", …},
 "contexts": [{"thinking": true, "tools": […], "queue": "next_command", "messages": […]}]}
```

- **`contexts` are context objects,** each sent whole, as above. Each read
  names its context by its index in `contexts`, as do the keys of
  `leave_one_out`, and reports the `snapshot` it read after.
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
  decisions in a 4,096-position slot, of which a context may take 3,072.
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
{"contexts": [{"thinking": true, "tools": […], "messages": […]}],
 "suffixes": ["<channel|><|tool_call>call:next_command{answers:{verdict:<|\"|>"],
 "letters": ["A", "B", "C"], "top": 5}
```

- Each suffix is read after the context's end.
- **A suffix is the client's own structure**, so its special-token text
  tokenizes as single ids. This is the one place content may be control;
  `signals.control_text` still reports each hit.
- Each read returns its `context` index, `snapshot`, and `suffix` index,
  the letters' log probabilities as `logprobs`, their `mass`, and the `top`
  tokens as `{token, id, logprob}`, so a client sees where the rest of the
  probability went. Reads are not pooled; a client that pools follows the
  decision rules.

## Generations

```json
POST /council/v2/generations
{"context": {"thinking": true, "tools": […],
             "messages": [… , {"role": "user", "content": "how did it go?", "snap": true}]},
 "max_tokens": 2048, "sample": {"temperature": 1.0}, "stream": true}
```

- **A generation replies after its context.** The server builds the
  context as `POST /contexts` does, then samples the reply. The client
  appends the reply to its own messages for the next request.
- **`stream: true`** sends server-sent events: `token`, as `/mk/v1`'s, then
  `done` with the reply's `content`, `reasoning`, `tool_calls`, `finish`,
  `usage` (`prompt`, `kept`, `fed`, `completion`), `head`, and the
  context's `snapshots`. Closing the stream cancels the reply. Without
  `stream`, `done` is the JSON response.
- **The server names each call of the reply** with the `id` a later `tool`
  message answers. A call that does not parse is kept as written,
  `{id, name, raw, error}`; a client that sends it back sends it as text.
- **`head` is the snapshot after the reply,** as the reply's message
  renders, when the server holds it. The server holds it when that costs at
  most a few tokens of feeding, so the next request, which sends the reply
  and what follows, starts there. Otherwise `head` is `null` and the next
  request starts from the last boundary.
- **A prompt that leaves no room is a `413`** with `error.type`
  `context_full`, `prompt_tokens`, and `context_tokens`, before anything
  runs. The client shortens the context.

## Rules every server keeps

v1's six rules hold: raw numbers replay bit for bit; content is never
control, except a `reads` suffix; each context describes for itself; no
winner is decided; failures are explicit; the rendering is the server's.
One more:

7. **A snapshot never changes.** Its id names its tokens and the boundaries
   along them, so a read from it reads exactly the context that made it,
   and a number never comes from history the request did not send.

## Identity, limits, capabilities

As v1, plus:

| Limit | Means |
|---|---|
| `context_tokens` | The longest context, or a generation's prompt and reply. |
| `decision_context_tokens` | The longest context a decision reads, before its spec layer and case. |
| `snapshot_bytes` | What one token of a snapshot costs. |
| `pin_bytes` | The most snapshot state all pins together hold. |

`extensions` lists the server's own route prefixes, such as `["/mk/v1"]`.

| Capability | Means |
|---|---|
| `reads` | `POST /council/v2/reads`. |
| `generate` | `POST /council/v2/generations`. |
| `think` | Decisions accept `think` with `tokens` above 0. |
| `pins` | `/council/v2/pins`. |
| `tiers` | Snapshots report `tier`, and pinned snapshots move between tiers. |

## Extensions

A server serves the model-specific rest under its own prefix and lists it in
`identity.extensions`. mkviolet keeps `/mk/v1` for these:

| mkviolet route | Why it is not in the council API |
|---|---|
| `render`, `tokenize`, `detokenize` | The template and tokenizer, for debugging and bench tools. |
| `cases` | The last 32 decisions' cases and answers, for labeling. |
| `embeddings` | Gemma 4's last hidden state; quality unmeasured. |
| `status` (event stream), `metrics` | Engine passes per card. |
| `traces` | Residual and attention traces. |
| `jlens`, `steer`, `routing` | Jacobian Lens, residual steering, and MoE routing introspection (`docs/06-council-jlens-extensions.md` in mkviolet). |

## Converting

| From | Changes |
|---|---|
| council v1 | `system` and `turns` become `messages`, with the system message first, and a context travels with each request instead of living under an id. `snap` moves onto messages. Contexts gain `tools`, `thinking`, and `queue`. `pin` and `persist` become owned pins. Decisions gain `think`. Paths move to `/council/v2`. |
| HIP megakernel `/mk/v1` | Context ids and `POST /contexts` give way to sending the context; the content-hash id becomes the snapshot id. `reads` takes `contexts` and is otherwise the same. `generate`'s `from.context` becomes `generations` with the context. |
| lfm2d `/v1/opinion` | Held specs and describe-first carry over as `specs` and `text` questions. Contexts move from a domain-tagged token hash to messages with `snap` boundaries. |
| mkviolet | One snapshot store for contexts, generations, and decisions; each snapshot holds the rows after its parent boundary, in memory and on NVMe. `/mk/v1/choices` is gone, replaced by `reads`. v1 contexts and `/mk/v1/generate` keep their own stores. |
| kaijutsu | The mk provider sends each chat whole, with `snap` on user turns and tool results. Each council context is an agent-loop seat per spec, with a `queue` tool whose `answers` schema is the spec's, built at startup and pinned under the kernel's name. |

## Open questions

- **The cost of sending everything.** Each request renders and tokenizes
  its whole context. On mkviolet, a 32,480-token context renders, checks,
  and answers a `dry_run` in 79 ms; a server could cache renderings by
  message prefix if that grows.
- **Snapshot ids across servers.** They stay opaque and per server. A
  client that moves to another server sends the same messages; nothing
  else moves.
- **The queue tool on models without tool training.** A server without a
  tool-call template answers `queue` with a `400`; whether a plain-turn
  delivery should fall back silently is open.
- **Thinking at decision time.** `think` costs about 21 ms a token on
  mkviolet, which does not serve it yet. Whether it pays, and at what
  length, is the bench's next measurement.
- **Tool-result size.** A tool result is written whole into a context,
  even when the context then has no room for another turn. The client's
  preview and storage rules for large results are its own.

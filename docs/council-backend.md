# The council backend

A backend of kind `council` is a council v2 server: gogi's
`docs/council-api.md` is the contract, `gogi-proto` owns every wire type,
spec id, and pooling rule, and the nagaya is the server we run today. One
backend serves three things:

- **Chat turns** for any context whose `model` slot names one of its served
  models (this document).
- **Gate decisions** for a context whose `gate` slot names it, and the
  judge's reads (`docs/council.md`).
- **Embeddings** through `/council/v2/embeddings`
  (`docs/synthesis.md`).

Amy: "mk should be entirely replaced by gogi & modern council api, of which
nagaya is our current implementation", and "we're definitely doing chat
turns in the nagaya web app".

## Setup

```
kj backend set spaceghost --kind council --base-url http://100.65.47.5:8091 --key-optional
kj backend model set spaceghost lfm3 --context-window 32768
kj character create lfm3 --model spaceghost/lfm3 --max-tokens 4096 --rc assistant
```

- The backend has no key; `base_url` is the server address.
- A council model runs chat turns only with `max_tokens`. A council slot
  without one, such as a judge's, needs none for the gate and runs no chat
  turn; `kj context info` says so and names the fix
  (`docs/character.md`, "Slot roles").
- `GET /council/v2/identity` lists each served model's `capabilities` and
  `limits`. `kj backend show` lists the model rows the kernel holds, which
  may be fewer.

## Crate: `kaijutsu-council`

`crates/kaijutsu-council` is the one Rust client for the council v2 routes.
Only `kaijutsu-kernel` and `kaijutsu-index` depend on it. The kernel's own
`council/` module is the gate's policy, not the client.

- `CouncilClient` holds the transport: base URL, timeout, `traceparent`,
  request send, and status-to-error mapping. Each route family adds its
  calls in its own module: identity, context builds, snapshots, pins,
  specs, decisions, reads, embeddings, and generations.
- **One error type.** A transport failure, a non-2xx status, a body that
  does not match its schema, a mid-stream `error` event, and a stream that
  ends without `done` are different variants. None of them becomes an
  answer or an empty reply.
- **Tolerant decoding.** Unknown fields in a response are ignored at run
  time; a missing required field or a wrong type is still a decode error.
  The conformance tests run against `gogi_fake::Server` (gogi's
  `docs/gogi-fake.md`), and the same bodies run against a real server when
  `COUNCIL_URL` names one.
- The client pins nothing about the server. The caller reads the identity
  and decides what it accepts; a snapshot id is valid only under the
  identity that produced it.

## Chat turns

`llm/council/` sits beside `llm/openai/` and `llm/deepseek/`, an adapter
over `kaijutsu-council` with no HTTP of its own. Each streamed turn is one
`POST /council/v2/generations` (gogi's `docs/council-api.md`,
"Generations").

- **The served model must generate.** The turn's model names a served model
  by its `model` or an alias. A model the server does not serve, or one it
  serves without the `generate` capability, is an invalid request naming the
  backend, the model, and what the server serves. No other route stands in.
- **The whole conversation goes with every turn.** The context is the
  system prompt, the history as council messages (`llm/council/build.rs`),
  the tools, and `thinking`. `snap` marks the system message, the end of
  each user turn, and the last of each run of tool results. The server keeps
  no record of the chat; it starts from the longest snapshot it holds along
  the messages, so a turn that extends the last feeds only its new messages.
- **Nothing is held by the client.** There is no context id, no `PUT`, and
  no fork request. A fork, an exclude, or an edit sends the messages as they
  now stand, and the server reuses the prefix they share with what it holds.
  A server restart costs feeding, never a wrong answer.
- **A kernel instruction later in the history** (`llm::Role::System`) is a
  `system` message at its point (`docs/conversation-session.md`, "Kernel
  instructions").
- **`thinking` follows the served model.** A turn sends `true` when the
  served model declares `thinking` and the turn's effort is not `none`, and
  `false` otherwise. An effort other than `none` on a model without
  `thinking` is an invalid request, not a dropped setting.
- **The identity is read again** when none is cached, when the cached one is
  older than ten minutes, and after a generation refused with a 400 whose
  `param` names `thinking`. The 400 covers a model that lost `thinking`; the
  age covers one that gained it, which no error reports.
- **Tools and calls take Anthropic's shape.** A tool is `{name,
  description, input_schema}` and a call is `{id, name, input}`. The reply's
  calls carry the `id` the server gave them, and the next turn's results
  name it. A failed result is a `tool` message with `is_error: true`.
- **A call that did not parse** is stored as an ordinary `ToolUse` whose
  input is `{"truncated_arguments": <raw text>}`, answered by an error
  `ToolResult` (`runtime/llm_stream.rs`), and goes back on later turns as a
  normal call and result.
- **Each token names its phase.** A `token` event's `phase` is `reasoning`,
  `content`, or `call`; the provider sends each piece to the thinking or
  text block its phase names, and a call's pieces add no text. At `done`,
  each block must equal `done`'s `reasoning` and `content`, or the turn is
  an error. A `ping` and an event the contract does not name are skipped; an
  `error` event ends the turn with its `request_id`.
- **`finish` is the stop reason.** `end_turn`, `tool_use`, and
  `max_tokens` pass through, and `context_full`, a reply cut at the
  context's end, reads as `max_tokens`. `done`'s `seed` goes to the turn's
  usage.
- **A full context is a `413` `context_full`**, reported as
  `LlmError::ContextFull`. `max_tokens` is capped at the served model's
  `context_tokens`, and the server checks the room.
- **A `429` `busy` never ran** (gogi's `docs/council-api.md`, "Errors"), so
  the endpoint cools down and the same request may be sent again
  (`docs/retries-and-ratelimits.md`).

### Liveness

A call is written in the `call` phase, and a long call or a long prefill can
send few content events. The runtime cancels a stream that sends no event
for its idle timeout (120 seconds by default), so a slow served model wants a
longer `--idle-timeout` on its backend. Whether the in-flight strip shows
call-phase progress is open.

### Sharing the server

The same server serves the gate, the judge, and chat turns, under the
endpoint's concurrency limit (`kj backend set --max-concurrent`). Reads take
priority over generation on the server, so a chat turn yields to gate
decisions. That is the intended order.

## Tests

- The crate's council tests run against `gogi_fake::Server`; a live run
  points `COUNCIL_URL` at a server.
- The provider tests drive `llm/council/` against the crate's scripted
  server (its `test-util` feature): message and `snap` placement, thinking
  by capability, identity refresh, call ids, phase routing, the `done`
  comparison, `context_full`, and the refusal of a model the server does not
  serve or serves without `generate`.

## Open questions

- Which liveness signal does the in-flight strip get during a long `call`
  phase?
- How does a small served model's window (32768 tokens for lfm3) interact
  with `kj fork` filters and rc instruction size?
- `lfm3-omni` reads images, audio and video and writes audio; kaijutsu sends
  text only so far.

Related: `docs/council.md`, gogi's `docs/council-api.md`, `docs/character.md`,
`docs/retries-and-ratelimits.md`.

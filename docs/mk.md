# mk: the megakernel suite

Design. Built: the `kaijutsu-mk` crate with `MkClient` and the council
calls (step 1 of "Order of work"). The rest is not built. The megakernel is the Qwen
inference service in `~/src/megakernel-qwen38-flashnext-strixhalo`; its wire is
`/mk/v1` (`service/openapi.json`) plus `/council/v1`
(`service/council-openapi.json`). Facts below were read at megakernel
`3a4c342`.

## Purpose

`kaijutsu-mk` is the one Rust client for every megakernel route, and `mk` is a
provider kind that lets a context talk to it as a model.

Amy: "I'm ok with our inference being slow in this case, it's more a backup &
occasional testing thing we want to keep possible. I also want to have a
fairly unified mk suite of apis, and go to the trouble of having a custom
client for it."

So the goals are, in order:

1. A seat that runs entirely on zorak: chat, tool calls, no hosted provider.
   A small maintenance agent is the first user.
2. One client, one set of types, one error vocabulary for generation, council
   decisions, held contexts, reads, and status.
3. A seam for other decision servers (lfm2d today, others later) behind the
   council module's call shape.

Speed is not a goal. Decode runs at about 16 to 19 tokens per second, so a
turn with thinking on costs seconds. Correctness and observability come first.

## What the service already does

Observed in the megakernel source (`service/generate.py`, `gpu.py`, `http.py`,
`v1.py`, `openapi.json`):

- `POST /mk/v1/generate` takes `from` (exactly one of `messages` with
  optional `tools`, `context`, `prompt`, `tokens`), `thinking` (default
  true), `reasoning_effort` (`xhigh`, `medium`, `low`; absent is the
  template's `xhigh`), `prefill`, `sample` (`temperature`, `top_p`, `top_k`,
  `min_p`, `seed`), `max_tokens` (1 to 32768, default 4096), and `stream`.
- The service renders `messages` with the model's own chat template from the
  GGUF. The client never formats a prompt.
- Content in `messages` and `tools` is escaped, so text such as `<|im_end|>`
  cannot become a control token. Raw `prompt` and `tokens` keep their meaning.
- A message is `{role, content?, reasoning_content?, tool_calls?}` with role
  `system`, `user`, `assistant`, or `tool`. A tool call sent back is
  `{name, arguments}`, with `name` non-empty and `arguments` an object.
- The reply is split into `think`, `answer`, and `tool` phases. Tool calls are
  Qwen3-Coder XML, parsed against the request's tool schemas.
- A call that does not parse comes back as `{name|null, raw, error}`. It is
  never repaired and never dropped.
- Streaming is SSE: `token` events (`id`, `piece`, `phase`, `p`, `h`, `n`,
  `tps`), then one `done` event (`context`, `reasoning_content`, `content`,
  `tool_calls`, `finish` of `stop|length|cancelled`, `seed`, `usage` of
  `prompt|kept|fed|completion`, `ms` of `prefill|decode`, `model` of
  `id|weight_hash`). A server failure mid-stream sends one `error` event
  (`{error: {code, message, type}}`) and stops.
- Error statuses: 400 bad request, 404 unknown held context, 503 unavailable
  (including the state after an unrecovered GPU spin-timeout), 507 pin
  budget. Each carries the same `Error` body.
- `done.context` is a content-addressed id of prompt plus reply. The next
  request may extend it. A client that hangs up cancels the generation.
- `GET /mk/v1/model` reports `id`, `weight_hash`, `max_context`, and the
  build. `POST /mk/v1/render` renders `messages` and `tools` the way
  `generate` would and returns `n_tokens`, without generating.
- Generations run concurrently. Every GPU job takes one priority lock: a
  generation takes it once per decode step, a prefill once per 256-token
  chunk. A waiting read goes before any other job; within a class, arrival
  order. Two generations interleave step by step and each slows down.
- `GET /mk/v1/status` streams queue depth (`reads`, `other`), generations in
  flight, and held-context bytes. No response carries its own queue time.
- There is no authentication; the service is for the tailnet.

Not in the service, and so not assumed: stop sequences, grammar-constrained
decoding, logit bias, tool-call ids, and a `tool_call_id` on tool messages.

Unknown, to measure before relying on it:

- How often a Qwen3.8 tool call fails to parse in a real agent loop.
- Whether resending the full `messages` each turn keeps the held prefix, or
  re-prefills from the first turn that the template re-renders differently
  (the template trims earlier turns' reasoning).
- How many of the 32768 tokens the seat's system prompt and tool schemas
  take before the first user word.

## Crate: `kaijutsu-mk`

`crates/kaijutsu-mk` was `kaijutsu-council`. Only `kaijutsu-kernel` depends on
it. The kernel's own `council/` module keeps its name: it is the gate's
policy, not the client.

There is one client type. `MkClient` holds the transport, and each route
family adds its calls in its own `impl MkClient` block in its module.

Layout, smallest first:

| Module | Holds | State |
|---|---|---|
| `council` | `wire`, `canon`, `math`, the `/council/v1` calls | exists |
| `client` | `MkClient`: base URL, timeout, `traceparent`, request send, status-to-error mapping; `MkError` | exists |
| `generate` | `GenerateRequest`, `Message`, `Tool`, `Sample`, `TokenEvent`, `Done`, an SSE decoder, `generate` and `generate_stream` calls | new |
| `model` | `GET /mk/v1/model` and `POST /mk/v1/render` | new |
| `contexts` | `POST /mk/v1/contexts`, `GET`, `DELETE` | new, only when a caller needs it |
| `reads` | `POST /mk/v1/reads` | new, only when a caller needs it |
| `status` | `GET /mk/v1/status` SSE | new, only when a caller needs it |

We add a module when a caller exists, not to mirror the OpenAPI file.

Rules the crate keeps:

- One error type. A transport failure, a non-2xx status, a body that does not
  match its schema, a mid-stream `error` event, and a stream that ends without
  `done` are different variants. None of them becomes an answer or an empty
  reply.
- Unknown fields in a response are ignored at run time, as the council
  client already does. Amy: "tolerant." The service changes often, and a new
  field must not stop a seat. Drift is caught by the conformance test against
  the vendored OpenAPI file, not by the running decoder. A missing required
  field or a wrong type is still a decode error.
- `MkClient` pins nothing about the server. The caller reads `model` and
  decides what identity it accepts. Held-context ids are valid only under the
  identity that produced them.
- Types follow `service/openapi.json`. A conformance test validates every
  request we build and every recorded response against that file. See
  "Tests".

## Provider: `BackendKind::Mk`

`llm/mk/` sits beside `llm/openai/` and `llm/deepseek/`. It is an adapter over
`kaijutsu-mk::generate`; it does no HTTP of its own.

Backend row: kind `mk`, `base_url` required (the service address), no key.
`kj backend set` and `SUPPORTED_BACKEND_KINDS` gain `mk`. `Provider::from_backend`
builds the client. `Provider::stream` wraps the result in a new
`ProviderStream::Mk` that yields the kernel's `StreamEvent`.

### Request mapping

| Kernel | `/mk/v1/generate` |
|---|---|
| `BuildOpts.system` | one `system` message, first |
| user text | `user` message |
| assistant text | `assistant` message `content` |
| `Reasoning` block | `assistant` message `reasoning_content` |
| `ToolUse` | `assistant` message `tool_calls[{name, arguments}]` |
| `ToolResult` | `tool` message `content`, in the order of the calls |
| `BuildOpts.tools` | `tools[{type: "function", function: {name, description, parameters}}]` |
| `max_tokens` | `max_tokens`, capped at the room left in the window |
| `temperature`, `top_p` | `sample` |
| `effort` `xhigh`, `medium`, `low` | `reasoning_effort`, with `thinking: true` |
| `effort` `none` | `thinking: false` |
| no `effort` | neither field; the template's default (thinking on, `xhigh`) |

Rules:

- The kernel's `Role` has only `User` and `Assistant`; the system prompt
  arrives as one string in `BuildOpts.system`, so it is always first.
- An `effort` outside the four tokens above is an error, not a pass-through.
  The tunables cascade (cast slot, model row, `llm_defaults`) chooses it, so
  `thinking: false` for routine turns is a model-row or cast setting.
- The service has no tool-call ids. The adapter makes an id for each call
  (`mk-<turn>-<index>`), matches results to calls by position, and refuses a
  history whose result count differs from its call count.
- The adapter sends `reasoning_content` back and lets the template decide
  what to keep. It does not trim history itself.
- The first version resends `messages` every turn. Using `done.context` with
  `POST /mk/v1/contexts` is an optimization to add only after we measure that
  resending re-prefills.

### Calls that did not parse

The runtime already stores a call that did not parse as an ordinary call
(`runtime/llm_stream.rs`, the `ToolUseInvalid` arm): a `ToolUse` block whose
input is `{"truncated_arguments": <raw text>}`, answered by an error
`ToolResult`. Hydration returns it as a plain `ToolUse` with an object input,
so it goes back to the service as a normal `tool_calls` entry followed by a
`tool` message with the error. The adapter needs no special replay path.

It goes back on every request after the call: first on the very next one,
which carries the error result, then on every later turn, fork, and cold
start, because the first version resends the whole history. With held
contexts, the held prefix already has the model's exact tokens and nothing is
re-rendered.

Two changes this needs:

- The runtime's error text says the arguments "were not valid JSON" and were
  "most likely cut off at the output limit." That is wrong for a Qwen XML
  parse failure. The text must name the parser's own `error` and give the
  output-limit advice only when the turn stopped at `length`.
- The service may return `name: null`. The runtime and the replay need a
  non-empty name, so the adapter uses a fixed placeholder and the error text
  says the tool name did not parse.

### Response mapping

| Service | `StreamEvent` |
|---|---|
| `phase: think` tokens | `ThinkingStart`, `ThinkingDelta`, `ThinkingEnd` |
| `phase: answer` tokens | `TextStart`, `TextDelta`, `TextEnd` |
| `phase: tool` tokens | no content event; see "Liveness" |
| `done.tool_calls` with `arguments` | `ToolUse { id, name, input }` |
| `done.tool_calls` with `raw` and `error` | `ToolUseInvalid { id, name, arguments: raw, error }` |
| `done.finish` | `Done { stop_reason, input_tokens, output_tokens, extra }` |
| `error` event | `Error`, from the crate's mid-stream error variant |

`stop_reason` is `tool_calls` when any call came back, `stop` for a plain end,
and `length` for a length end. `input_tokens` is `usage.prompt`,
`output_tokens` is `usage.completion`, and `extra` carries `usage.kept`,
`usage.fed`, `ms`, `seed`, and the model identity, so the telemetry layer sees
prefix reuse and the seed that replays the turn.

A `cancelled` finish is not a reply. If the client did not cancel, the adapter
raises an error. A stream that ends with no `done` is an error, not a short
reply.

### Liveness

A tool call is written in the `tool` phase and arrives only in `done`. At 16
to 19 tokens per second, a call that writes 1500 tokens of a file is about 90
seconds with no event. The in-flight strip must show that the turn is alive.
`StreamEvent` has no event for this today. The design choice (a progress event
carrying the token count, or the tool-phase text as it arrives) is open; it
must not invent a `ToolUse` before `done`.

### Window and admission

- The window is the service's `max_context` from `GET /mk/v1/model`, read
  when the provider is built. A model row's `context_window` larger than that
  is a construction error. No kernel path enforces `context_window` today; it
  is shown by `kj backend` and context info, so the provider enforces the
  window itself.
- Before each call, the adapter renders the request with `POST /mk/v1/render`
  and checks `n_tokens + max_tokens` against the window. When the prompt
  leaves no room, the call is an error that says so, not a truncation. When
  it leaves some room, `max_tokens` is lowered to fit and the telemetry says
  so.
- The megakernel serves the gate and the musicians too, and reads preempt
  generation. A seat on `mk` therefore yields to them. This is the intended
  order. The provider records its own wall time and first-token delay as
  telemetry; queue depth is available from `/status` when a caller needs it.
  Admission policy belongs with `docs/issues.md`, "System 1 musicians share
  the megakernel with the gate".

## The council module and other decision servers

The council client keeps its current calls and types. Two changes:

- It uses `MkClient` for transport, so generation and decisions share timeout,
  tracing, and error mapping.
- Its calls sit behind a small trait only when a second decision server
  exists. lfm2d already has an opinion API with a different shape
  (`docs/council-api.md`, "Converging the two servers"); until it adopts
  `/council/v1`, no trait is written. We do not guess the second shape.

A hosted decision API (the "or" and "oai" cases Amy named) would be a new
module that implements that trait, not a change to the gate.

## The seat

A maintenance seat on `mk` needs:

- A backend row and a cast row naming the `mk` backend and its model, with
  the model row's tunables (`effort none` for routine turns, a `max_tokens`
  sized to the window).
- A context type and rc bundle for the seat, written as other seats are
  (`docs/prompts.md`).
- A tool loadout narrowed to maintenance work, small enough that its schemas
  leave room in the window. Capabilities are ergonomic here, not a security
  boundary.
- The council gate in bumper mode, so the seat can act without a human and the
  gate can still say "think again" (`docs/council.md`, "Bumper mode").

Model turns need a performer and a distinct reviewer
(`docs/approval-identity.md`). That rule applies unchanged.

## Tests

Each test below can fail for a named reason.

- **Schema conformance.** Build requests with the crate's types and validate
  them against `service/openapi.json`. Validate recorded responses the same
  way. Mutation check: rename a field in a fixture and watch validation fail.
  The OpenAPI files are copied into the crate's fixtures with their source
  commit named in a sidecar file, and a script refreshes them. This is the
  sensor for server drift, since the running decoder tolerates new fields.
- **Stream decoder.** Scripted SSE for a thinking-then-answer reply, a
  tool-call reply, an invalid tool call, a length stop, a cancel, an `error`
  event, a stream cut short, and an unknown event. The last three must error.
- **Provider mapping.** A scripted fake server drives `llm/mk/` the way
  `with_scripted_stream` drives the kernel. Cases: system placement, effort
  mapping (including a refused token), tool id assignment, result-count
  mismatch, reasoning round trip, replay of a stored unparsed call,
  stop-reason mapping, window refusal, and a lowered `max_tokens`.
- **Replay.** Send the same `seed` and greedy `sample` twice against the
  fake; assert the same request body bytes. Against the real service (a live
  test, run on zorak by hand), assert the same tokens.
- **Live probes on zorak.** With `thinking: false` and a cheap seed: render
  the seat's first turn and record `n_tokens`; a ten-turn tool loop that
  records `usage.kept` and `usage.fed` per turn for the resend path; and
  twenty small tool tasks that count parse failures. The service shares
  zorak with the gate, so run these under the heavy lock that `~/exomemory`
  describes.

Kernel tests deny host subprocess execution; none of these needs it.

## Order of work

1. Done: rename the crate and the kernel's paths; `CouncilClient` became
   `MkClient` and `CouncilError` became `MkError`. The crate's 70 tests and
   the kernel's council tests pass unchanged.
2. Add `generate` and `model` types, the SSE decoder, and the conformance
   test, with no provider yet. `MkError::Status` decodes only the council
   error body today; the `/mk/v1` body (`code`, `message`, and a free-text
   `type`) needs its own decoded form.
3. Add `llm/mk/` and `BackendKind::Mk` against the scripted fake, with
   render-based admission and the unparsed-call text fix.
4. Run the live probes. Decide the resend-versus-context question and the
   tool-call reliability question from the numbers.
5. Add the cast, rc, and seat, and decide liveness.

## Open questions

- Should the provider keep `done.context` ids in the conversation, so a
  restart or a fork reuses a held prefix? Held contexts survive a service
  restart, but only for the same weights and engine identity.
- How does the seat's window interact with `kj fork` filters when 32768 tokens
  is the whole budget?
- Does a Qwen `tool` role with several results at once match what the
  kernel's grouped `ToolResult` blocks mean? The service renders them as the
  template defines; a live probe should confirm the call order.
- Which liveness signal does the in-flight strip get during the `tool` phase?

Related: `docs/council.md`, `docs/council-api.md`, `docs/prompts.md`,
`docs/approval-identity.md`, `docs/issues.md`.

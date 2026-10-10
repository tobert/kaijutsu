# Retries and rate limits

Status: "What we build first" is built (2026-10-10): `llm/endpoint.rs`,
with the call sites below. "Later, if needed" is not built. Amy,
2026-10-10: "we'll do after the current work ties up. let's focus on the
simple big wins and see if nuance is needed later. I suspect a simple
concurrency limit + cooldowns will do the trick."

The kernel limits how hard it presses each model endpoint. Each endpoint
gets a concurrency limit and a cooldown, and a caller waits for a slot
rather than piling on. A busy endpoint slows every caller of that endpoint,
not only the one that got the busy answer.

## Why

On 2026-10-10 one coder seat sent about ten shell calls in two minutes. The
gate's council decisions, the judge shadow's priming, and the judge's reads
all went to one megakernel at once. Gate time rose from about 1 s to 13 s,
and four judge reads missed their 30 s deadline on the cleanup steps the
judge exists for (`docs/council.md`, "Shadow build order", step 4). Nothing
in the kernel limited the burst. Every provider already maps HTTP 429 to
`LlmError::RateLimited`, and the council client keeps `Retry-After` on 429
and 503, but nothing acts on either.

The megakernel now takes at most 16 POST or PUT requests at once and
answers the next one 429 (`docs/mk-admission.md`). The kernel's part is to
answer that by backing off, and to not send more than an endpoint can take
in the first place.

## What we build first

- **One limiter per endpoint.** An endpoint is a base URL's origin
  (scheme, host, and port), written `http://zorak:8090` or
  `https://api.anthropic.com:443`, with the default port written out. Two
  backends that name the same server share one limiter, and so do the gate's
  `[council] server` and an `mk` backend at the same address. The key is the
  host as written, not its address: `http://zorak:8090` and
  `http://100.83.138.103:8090` are two endpoints, so name a server one way. A backend with
  no base URL is its own endpoint, `hosted backend <name>`: two hosted
  Anthropic backends with different keys do not cool each other down. The
  kernel owns the limiters (`Kernel::endpoints`), and they outlive registry
  rebuilds, so a slot taken before a `kj backend set` is released to the
  same endpoint after it.
- **A concurrency limit.** At most N requests to the endpoint are in flight.
  A streaming turn holds its slot until the stream ends. The default is
  unlimited until a backend sets one; the megakernel and local inference
  are the first to set it.
- **A cooldown.** A 429 or 503 starts a cooldown for the whole endpoint:
  `Retry-After` when the answer carries one, else 5 s, doubled on each
  cooldown round in a row up to 60 s, and reset by the next completed
  answer. A cooldown lasts at least 1 s, `Retry-After: 0` included. During
  a cooldown no new request starts; waiting callers keep waiting.
- **One step per round.** Busy answers that arrive while the endpoint is
  already cooling down do not step the doubling again, so 16 requests in
  flight that all answer 503 start a 5 s cooldown, not a 60 s one.
- **A completed answer resets the doubling; an open stream does not.** A
  non-streamed 2xx is a completed answer. A stream that opens with 200 is
  not one yet: it resets the doubling when it reaches its end event (mk
  `done`, Anthropic `message_stop`, OpenAI `[DONE]`). A stream that fails
  with an error event carrying 429 or 503, such as the megakernel's
  mid-stream 503 `pass_timeout_error`, is a busy answer and starts a
  cooldown. A server that answers 200 and then fails every stream would
  otherwise hold the doubling at 5 s.
- **Waiting is bounded by the caller's own deadline.** A council decision
  waits within its `deadline_ms`, a judge read within its 30 s, shadow
  priming within 30 s, and a model turn within its backend's request
  timeout (600 s when unset). A caller that finds a free slot when it asks
  takes it; a caller that has waited gets no slot at or after its deadline,
  even when one is free at that instant. A caller that cannot get a slot in time fails
  the way a timeout fails today: the gate records a miss whose cause names
  the wait (`no answer within the 700 ms deadline: the deadline passed
  waiting 700 ms for a slot at http://zorak:8090 (2 of 2 slots in
  flight)`), the judge records a miss, and a turn fails with `RateLimited`.
  No new fallback.
- **A megakernel 429 is sent again; nothing else is.** The megakernel
  refuses a POST or PUT past its own limit with 429, `Retry-After: 1`, and
  body type `busy`, before any work starts (`docs/mk-admission.md`). A 429
  from an mk endpoint therefore means "did not get a slot": the cooldown
  starts, and the same caller sends the same call again once it ends, still
  within its own deadline. Each cooldown lasts at least 1 s, so the resends
  are at most one a second and end at the deadline. The `mk` provider gives
  each resent call only the time left before its turn's deadline, as the
  gate does. A streamed generate gets the time left to open, and then its
  full request timeout for each read. This covers the `mk` provider and
  every council call. A 503, a 429 from another provider, and any failure after a
  request started are not sent again by the limiter.
- **Retries stay where they are.** A bump-only seat already sends again
  after a miss, a judge read is optional, and a turn that fails is visible
  to its player. A turn's existing startup retry (`runtime/llm_stream.rs`,
  two retries for a transient error) still applies to `RateLimited`, so a
  turn that got no slot tries twice more, each waiting for a slot again.

## Where it applies

Every outbound model request takes a slot first:

1. Model turns and one-shot prompts, through each provider in
   `LlmRegistry` (anthropic, deepseek, openai-compatible, mk).
   `Provider::from_backend` gives each client its endpoint. Anthropic and
   the OpenAI-compatible clients take one slot per request; a streamed
   reply holds it until the stream finishes. The Anthropic client's
   context-window lookup, `GET /v1/models/{id}`, takes a slot too; its
   answer starts no cooldown. The mk client takes a slot
   for each call it makes (model, render, generate), and a streamed
   generate holds its slot until the stream ends. `codex-app` takes no
   slot: it reaches a local daemon, not a model endpoint, and `kj backend
   set` refuses `--max-concurrent` on it.
2. Council calls (`council/sync.rs`, `council/gate.rs`): identity, spec
   POST, and context PUT while preparing, and the decision itself, for the
   gate's decisions, observations, shadow priming, and the judge's reads.
   Each call takes its own slot and releases it when it answers, so no
   caller holds one slot while it waits for another.

MCP servers keep their own `InstancePolicy.max_concurrency` (`kj policy`).
Host programs a seat runs, such as `curl`, are outside this design.

The beat path gets no special case. Hyoushigi resolvers keep their own
admission and their timeouts; a request that cannot get a slot fails or
times out, the song goes on, and when the endpoint recovers the next
request goes through.

## Configuration

The limit belongs to the backend, set with `kj backend`:

```sh
kj backend set mk-zorak --kind mk --base-url http://zorak:8090 --max-concurrent 2
kj backend set tenchi --kind openai --base-url http://zorak:8090/v1 --key-optional --max-concurrent 4
kj backend show mk-zorak
```

`kj backend set` declares the whole row, so the other flags are stated
again. `--max-concurrent` must be at least 1; omitting it is unlimited, the
default. The limit is the `backends.max_concurrent` column. Two backends at
one origin with different limits use the smaller one, and `kj backend
show` says so. Only backends that built set a limit: a backend the registry
skips (no key, say) does not cap the backends beside it, and `kj backend
show` reports its limit as `not applied: the backend is not registered`.

```text
Max concurrent: 2
Endpoint: http://zorak:8090
  Limit: 2, the smallest of the backends here (mk-zorak 2, tenchi 4)
  In flight: 1
  Cooldown: none
```

An endpoint no backend names, such as a `[council] server` with no
matching backend row, is unlimited and has a cooldown.

## What we can see

- A council decision's `queue_ms` is the time it waited for slots, its
  waits before a 429 resend included, plus the queue time the server
  reports when it reports one. A miss records the wait it had when the
  deadline passed. Observations and judge reads fill their own `queue_ms`
  the same way, and the `council.decide` span carries it as
  `council.queue_ms`.
- A provider request's span records its wait as `llm.slot_wait_ms`.
- A cooldown that starts or ends is one `info` log line naming the
  endpoint, the status, and the duration.
- `kj backend show` lists the backend's endpoint, its limit, its in-flight
  count, and the cooldown time left.

## Later, if needed

Each of these waits for a reading that says the simple form is not enough:

- Requests or tokens per minute, for hosted providers with published
  quotas.
- Priority between callers of one endpoint: a gate decision before a judge
  read before shadow priming, so a burst cannot starve the gate.
- Sending only a shadow's newest tail when priming falls behind, instead of
  each step.
- A per-context share, so one seat cannot hold every slot.

## Tests

- A limit of 1 holds a second request until the first ends
  (`llm/endpoint.rs`).
- A 429 with `Retry-After: 2` delays the next request to that endpoint
  by 2 s, and does not delay another endpoint (`llm/endpoint.rs`).
- Busy answers in a row double the cooldown from 5 s, and a success
  resets it; busy answers during one cooldown step it once; every
  cooldown lasts at least 1 s (`llm/endpoint.rs`).
- A caller that waited until its deadline gets no slot though one is free;
  a megakernel 429 with no wait is sent again at most once a second until
  the deadline; a 429 at the deadline is not sent again
  (`llm/endpoint.rs`).
- Opening a stream keeps the doubling, and a stream that ends whole resets
  it; a mid-stream 503 cools the endpoint (`llm/endpoint.rs`,
  `llm/mk/mod.rs`, `llm/claude/mod.rs`, `llm/openai/mod.rs`).
- A backend that does not build sets no limit (`llm/db_config.rs`), `kj
  backend show` says so, and `kj backend set` refuses `--max-concurrent`
  on codex-app (`kj/backend.rs`).
- A council decision that cannot get a slot within its deadline is a miss
  whose cause names the wait, and its `queue_ms` is the time it waited
  (`council/gate_e2e.rs`).
- A council 429 is sent again after `Retry-After`, and the decision allows
  (`council/gate_e2e.rs`).
- Two backends at one origin share one limiter, and `kj backend show`
  names the smaller limit (`llm/endpoint.rs`, `kj/backend.rs`).
- An mk turn holds its slot until its stream ends, sends a 429 again after
  `Retry-After`, does not send a 503 again, and fails `RateLimited` when it
  gets no slot in its timeout, and gives a resent call only the time left
  before its deadline (`llm/mk/mod.rs`). The Anthropic client cools its
  endpoint on 503 and 429 (`llm/claude/mod.rs`), and its context-window
  lookup takes a slot; the OpenAI-compatible client cools on 429
  (`llm/openai/mod.rs`).
- The megakernel client opens a new connection after an answer that says
  `Connection: close` (`kaijutsu-mk/src/client.rs`).

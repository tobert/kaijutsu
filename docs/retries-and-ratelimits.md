# Retries and rate limits

Status: planned, not built. Amy, 2026-10-10: "we'll do after the current
work ties up. let's focus on the simple big wins and see if nuance is
needed later. I suspect a simple concurrency limit + cooldowns will do the
trick."

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

The megakernel is adding queue depth and will answer 429 when it is full.
The kernel's part is to answer that by backing off, and to not send more
than an endpoint can take in the first place.

## What we build first

- **One limiter per endpoint.** An endpoint is a base URL's origin
  (scheme, host, and port). Two backends that name the same server share one
  limiter, and so do the gate's `[council] server` and an `mk` backend at the
  same address. A hosted provider with no base URL is its own endpoint.
- **A concurrency limit.** At most N requests to the endpoint are in flight.
  A streaming turn holds its slot until the stream ends. The default is
  unlimited until a backend sets one; the megakernel and local inference
  are the first to set it.
- **A cooldown.** A 429 or 503 starts a cooldown for the whole endpoint:
  `Retry-After` when the answer carries one, else 5 s, doubled on each
  busy answer in a row up to 60 s, and reset by the next success. During a
  cooldown no new request starts; waiting callers keep waiting.
- **Waiting is bounded by the caller's own deadline.** A council decision
  waits within its `deadline_ms`, a judge read within its 30 s, and a model
  turn within its request timeout. A caller that cannot get a slot in time
  fails the way a timeout fails today: the gate records a miss, the judge
  records a miss, and a turn fails with `RateLimited`. No new fallback.
- **Retries stay where they are.** The limiter does not retry a failed
  request. A bump-only seat already sends again after a miss, a judge read
  is optional, and a turn that fails is visible to its player.

## Where it applies

Every outbound model request takes a slot first:

1. Model turns, through each provider in `LlmRegistry` (anthropic,
   deepseek, openai-compatible, mk).
2. Council calls built in `council/sync.rs`: the gate's decisions, shadow
   priming, and the judge's reads.

MCP servers keep their own `InstancePolicy.max_concurrency` (`kj policy`).
Host programs a seat runs, such as `curl`, are outside this design.

The beat path gets no special case. Hyoushigi resolvers keep their own
admission and their timeouts; a request that cannot get a slot fails or
times out, the song goes on, and when the endpoint recovers the next
request goes through.

## Configuration

The limit belongs to the backend, set with `kj backend`:

```sh
kj backend set mk-zorak --max-concurrent 2
kj backend set tenchi --max-concurrent 4
kj backend show mk-zorak
```

Two backends at one origin with different limits use the smaller one, and
`kj backend show` says so. An endpoint no backend names, such as a
`[council] server` with no matching backend row, is unlimited and has a
cooldown.

## What we can see

- Council decisions already have a `queue_ms` column, today always 0. It
  becomes the time the decision waited for a slot, so `kj ledger bumps`
  and the decision record show a busy endpoint.
- A turn's span and a judge observation record their wait the same way.
- A cooldown that starts or ends is one `info` log line naming the
  endpoint, the status, and the duration.
- `kj backend show` lists each endpoint's limit, its in-flight count, and
  whether it is cooling down.

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

- A limit of 1 holds a second request until the first ends.
- A 429 with `Retry-After: 2` delays the next request to that endpoint
  by 2 s, and does not delay another endpoint.
- A council decision that cannot get a slot within its deadline is a miss
  whose cause names the wait, and its `queue_ms` is the time it waited.
- Two backends at one origin share one limiter.

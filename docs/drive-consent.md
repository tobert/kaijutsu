# Drive consent — who may cause a turn, and when it would be wasteful

A **drive request** asks a context to take a turn it did not ask for. Drift's
`--drive` is the first caller; the beat scheduler and any future
orchestration are the same shape. This doc is the general policy, because the
question is not drift's — it is "who may spend this context's tokens, and is
now a sensible moment".

Two independent gates, and a request must pass **both**:

1. **Consent** — does this context accept drive requests at all? Policy,
   set per context, defaulted per `context_type`.
2. **Warmth** — is this context's prompt cache likely still alive? Runtime,
   computed. A drive into a cold cache pays a full reprocess.

Amy, 2026-08-12, ruling both into existence:

> "the default should be a gentle mailbox drop that gets picked up on the next
> turn but we have the `--drive` option too to ensure a turn happens. I think
> we will also want a way for a context to be able to disable drive requests,
> perhaps by default. […] we can have it on by default for musicians, maybe
> coders. But also when we detect a KV might be gone, we can disable it, so we
> don't resurrect it, when our pattern is usually to always be ready to revive
> from saved state (like sleeping)."

## Gate 1: consent is the receiver's, not the sender's

The default remains a **gentle mailbox drop**: the block lands durably, and
the receiver folds it in on its next natural turn (`llm/mailbox.rs` `catch_up`).
Nothing new happens. `--drive` is an *escalation*, and it is a **request** —
never an instruction.

This is the distinction that makes a sender-side flag safe. An earlier design
pass rejected "sender declares the wake" outright, because a flag that lets a
sender spend a receiver's tokens is the wrong direction for consent. The veto
inverts it: the sender may *ask*, the receiver decides whether asking works.

Consent is **per-context, defaulting off**, with defaults set per
`context_type` through the rc lifecycle — the same mechanism that already
carries stances and bindings:

| `context_type` | Default | Why |
|---|---|---|
| `musician` | **on** | Beat-driven by construction. A player that cannot be woken cannot take a hand-off mid-piece — the whole point. |
| `coder` | probably on | Amy: "maybe coders". Wants a real look; a coder woken mid-task is useful, but it is also the seat most likely to be expensive. |
| everything else | **off** | Deny-by-default, consistent with the capability allow-set stance. |

Natural home is the **context binding / loadout**, not a new concept: this is
exactly an ergonomic nudge in the CLAUDE.md sense — narrowing what can happen
to a context by construction, not a security boundary. Every player is inside
the trust boundary; a context that declines drive requests is *focused*, not
*distrusted*.

## Gate 2: don't resurrect what has gone cold

This is the subtler half, and it comes from a property of the system that
otherwise hides the cost.

**Kaijutsu contexts are designed to always be revivable from durable state.**
That is the whole CRDT-and-block-log bet: a context is never really gone, only
sleeping, and can be rehydrated whenever. The failure mode is that this makes
revival look *free* to anything that can request it. It is not. When a
context's provider-side prompt cache has expired, the next call reprocesses
the whole conversation as a cache miss — the exact cost the cache-breakpoint
policy exists to avoid.

So a drive request into a context whose cache has aged out is the worst case:
it is *possible* (the state is all there), it looks routine, and it silently
pays full freight. **Detect it and decline.**

> **Disambiguation:** "KV" here is the *model's* attention KV / prompt cache,
> not the kernel key-value store, which was built and demolished 2026-07-04
> and is not coming back. Different noun, same two letters.

### The predicate is computable today — no new schema

Everything needed already exists:

- `context_usage.updated_at` (`kernel_db.rs:624-635`) is the wallclock of the
  **last completed LLM call** for a context. This is the right clock —
  distinct from `contexts.last_activity_at`, which any block write touches
  (a shell command or a drift arrival would falsely look like warmth).
- `cache_breakpoints` (`kernel_db.rs:795`) carries the context's breakpoints
  and their TTLs, set via `kj cache add --ttl ephemeral|extended`
  (`kj/cache.rs:138-141`) — **ephemeral ≈ 5m, extended ≈ 1h**.
- `context_usage.cache_read_tokens` records whether the last call actually
  *hit* cache — direct evidence, better than inference.

A first cut:

```
warm(ctx) =
    has breakpoints
    AND (now - context_usage.updated_at) < shortest breakpoint TTL
```

with `cache_read_tokens > 0` on the last call as corroboration that caching is
working at all for this context.

Deliberately conservative readings:

- **No breakpoints** → there is no cache to lose. A drive is an ordinary cold
  call; do not suppress on warmth grounds. Consent still applies.
- **No usage row** → never called. Same as above: nothing to resurrect.
- **Shortest, not longest, TTL** → if the system-prompt breakpoint has expired
  the prefix is already invalidated, so the longer-lived ones downstream do
  not save us.

The TTLs are *provider* promises, not guarantees — a cache can be evicted
early. So this predicate is a cheap conservative estimate, and it should be
described as one everywhere it appears. It answers "is this obviously
wasteful?", not "is the cache definitely present?".

## Refusals must be loud

A suppressed drive is exactly the silent-fallback shape CLAUDE.md rejects: the
sender asked for a turn, no turn happened, and by default nobody would know.
Both gates must report.

- **Consent refusal:** the drift still delivers (that is the default path and
  it is not in question) — but the response says the turn was not requested
  because the target does not accept drive requests. The content is never at
  risk; only the escalation was declined.
- **Warmth refusal:** likewise delivered, and the response should say the
  cache is presumed cold and roughly how stale it is, so the caller can decide
  whether to insist.

Which implies a third thing worth designing rather than bolting on: some way
to **insist** — an override for "yes, I know it is cold, wake it anyway" —
because "the cache is cold" is a cost signal, not a correctness one, and there
will be moments (a live piece; a human asking) where the cost is worth paying.

## Prerequisite before any of this ships

The **rc lifecycle identity smear** (filed in `docs/issues.md`). The rc kaish
is materialized with the *sender's* principal (`kj/lifecycle.rs:376,388`)
while bound to the target's context. Capabilities resolve correctly against
the target (`kj/mod.rs:563-576`), but block authorship and `privileged` ride
in from the sender. A driven turn must not be attributed to whoever asked for
it — fix this first, or `--drive` writes the receiver's work under the
sender's name.

## Open questions

1. **Coders on or off by default?** Amy said "maybe". The argument for is that
   a coder woken by a sibling's finding is the cybernetic loop working; the
   argument against is that it is the seat most likely to burn budget
   unattended.
2. **Where does consent live, exactly?** Context binding/loadout is the
   proposed home. Confirm it composes with the rc `create` lifecycle so a
   `context_type` default is set once and can be overridden per context.
3. **What does "insist" look like?** A second flag, a confirmation round-trip,
   or a capability only some players hold.
4. **Should the beat scheduler go through these gates too?** It drives
   musicians today by a different path (`rc/musician/tick/S10-drive.kai`). If
   consent is real policy it should be one gate, not two — but a musician
   that vetoes its own beat is a broken instrument, so the tick path may
   legitimately bypass consent while still respecting warmth.

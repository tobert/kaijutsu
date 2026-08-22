# Asks and forms

A brief, not a spec. It answers one question — *when an agent needs something
from a human, what does it use?* — and stops before choosing an
implementation, because the choice depends on a decision Amy has not made yet.

## The question

A delegated coder hits something it cannot decide. Two shapes:

- **"May I?"** — it wants to run `cargo update -p kaish`, or `rm -rf` a tree.
  There is a *statement*, and the answer is allow or deny.
- **"Which of these two?"** — it wants a judgment about approach. There is no
  statement, and the answer is prose or a choice among options someone writes
  at ask time.

Amy, 2026-08-22:

> *"I'm curious if we should have a unique feature for, well, basically
> abstract forms, vs the approval gates, which have different needs. They block
> in similar ways but the data seems pretty different? Maybe a form tool /
> tools? Is there some prior art we should riff on?"*

The data is different. The blocking is the same. That split is the whole brief.

## "May I?" is already built, and is not the problem

The coder runs the command; the gate stops it; an ask row lands in the ledger;
an orchestrator answers with `kj ledger allow <id>`. No new surface is needed,
and `ask` already means exactly this in kaijutsu's vocabulary — *a durable row a
gate leaves behind, waiting for a decision* (`CLAUDE.md`, Terms).

What is broken is the **wait**, not the vocabulary: `gate_wait_timeout` is one
process-wide 300 s (`kaijutsu-types/src/timeout.rs`), so an unattended coder
dies five minutes into a gate and a later answer re-drives nothing. That is
tracked in `docs/issues.md` ("An expired approval should become a tool error"),
and it is the load-bearing fix for delegation — not a new concept.

**Do not add a `kj ask` verb for this.** It would give `ask` a second meaning,
which the writing guide forbids for exactly the reason it would bite here: two
mechanisms named the same thing.

## Drift is the wrong edge for either shape

Drift is an overlay that carries *findings* between contexts. A request for a
decision is not a finding, and routing one through drift would make drift mean
two things. `fork` and `drift` are already kept distinct on purpose
(`CLAUDE.md`, Terms); this would blur a third thing into one of them.

## The ledger is three layers, and only the bottom is shell-shaped

Read `crates/approval-ledger/src/schema.rs` and the seam is visible:

| Layer | What it is | Reusable for a form? |
|---|---|---|
| `approvals` (`:269`) | request_id, context_id, principal_id, origin, description, `pending`/`claimed`/`allowed`/`denied`/`expired`/`abandoned`, expiry, claim fields, decided fields — plus a one-way ratchet trigger, `approval_events` (audit) and `approval_signals` (advisory) | **Yes, entirely.** Nothing here is about shells. |
| `approval_options` (`:346`) | `(request_id, seq, option_id, label, kind)` in presentation order | **Partly.** This *is* a single-select field. It cannot express free text, multi-select, typed values, validation, or more than one field per ask. |
| `approval_statements` + `_commands` / `_args` / `_redirects` / `_vars`, and `approval_rules` (`:421`) | content-addressed kaish plan trees, digest-keyed rules, free-variable refusal | **No, and it should not try.** |

Layer 3 exists so a decision *generalizes* — approve this statement shape, and
future identical statements decide themselves. That is the entire reason the
statement, not the ask, is the content-addressed unit. A question has no
statement, no digest, and nothing to generalize, so layer 3 is dead weight for
it rather than a foundation.

So the ledger is roughly **80% of a form system already**, and the 20% that is
missing is a richer field type — not durability, not claiming, not audit.

## We already have the other half, split the wrong way

MCP elicitation is **already on our wire** (`kaijutsu.capnp:1509`):

```capnp
struct McpElicitationRequest {
  requestId @0 :Text;  server @1 :Text;  message @2 :Text;
  schema @3 :Text;     hasSchema @4 :Bool;      # JSON Schema
}
struct McpElicitationResponse {
  action @0 :Text;     # "accept", "decline", or "cancel"
  content @1 :Text;    # JSON response data
}
```

That is the right *payload*. Three outcomes beat allow/deny for a question —
"decline" (I won't answer) and "cancel" (I'm abandoning this) are genuinely
different, and a coder's question needs the distinction.

But `interface ElicitationEvents` is `onRequest @0 (...) -> (response)` —
**synchronous, connection-bound, and gone when the connection drops.**

That is the finding worth keeping:

> **The ledger is durable with the wrong payload. Elicitation has the right
> payload and no durability at all.**

An unattended delegated coder needs both, and needs the durability *more* than
an interactive one does — there may be no one connected when it asks.

## Prior art worth riffing on

- **debconf** — the closest structural match. It separates the **template**
  (question text, type: string/boolean/select/multiselect/note/password,
  choices, default) from the **answer database**, and adds a **priority** so
  low-priority questions take their default without ever being asked. That
  priority knob is the same lever `lfm2d` is meant to pull on approval fatigue,
  and it generalizes past approvals for free.
- **systemd-ask-password** — generalizes our claim semantics. An ask file
  (`Message=`, `NotAfter=`, `Socket=`) can be presented by *several* agents at
  once (console, plymouth, gnome) and the first answer wins. We will have the
  same multi-presenter problem: the app, an MCP orchestrator, and ACP can all
  see one ask.
- **MCP elicitation** — its schema subset is deliberately flat and
  primitive-only, with no nesting. That restraint is worth copying rather than
  inventing something richer we then have to render everywhere.
- **ACP permission requests** — we ship a bridge, so its shape constrains ours
  whether or not we design for it.

## What we would do, if we do anything

**Stop treating layers 1–2 as private to approvals.** Concretely:

1. Widen `approvals.origin` past its `CHECK (origin IN ('hook', 'shell_gate',
   'kj_verb'))` — that check is precisely where a form origin slots in.
2. Let an ask carry a schema-shaped answer alongside `decided_option`, so free
   text and multi-select become expressible without touching layer 3.
3. Project it onto the elicitation wire that already exists, so a connected
   client renders a real form and a disconnected one still finds the ask
   waiting in `kj ledger`.

The per-ask wait budget then serves both kinds of ask instead of just gates,
which is why that work should land first either way.

## What this brief does not decide

- Whether to build any of it. The `kj wait` + turn-tail path ("end the turn
  with the question, the orchestrator answers with `kj drive --prompt`") works
  today with zero machinery, and we have not yet watched enough delegated turns
  to know what an orchestrator actually wants to branch on.
- Whether a form ask should be answerable by a **model**, or only by a human.
  The gate's whole point is that a human decides; a form's is not obviously the
  same, and getting that wrong would quietly turn the ledger into something
  that no longer means "a human was asked".
- Whether `approval_options.kind` is already the extension point, making this
  much smaller than it looks.

The honest next step is not code. It is to run several delegated turns, collect
the questions coders actually ask, and see whether they are allow/deny in
disguise.

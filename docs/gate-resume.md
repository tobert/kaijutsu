# The gate stops blocking, and the kernel resumes the action

**Amy's ruling, 2026-08-22.** A gated tool call must not hold an RPC open
while a human thinks. The kernel tells the client it is waiting; the client
may block locally; nothing blocks on the wire. When the answer lands — an
hour later, at 3am, after a kernel restart — **the kernel performs the
action itself** and authors the result into the originating context.

> *"perhaps we should consider a state machine and not actually having
> anything block on the wire? so kernel would instruct the client it is
> working, client can block, but they don't block on rpc."*

This supersedes the blocking wait that shipped in Slice 4.6
(`gate-and-shell-split.md`), which was verified live holding 81.2s. Deleting
it is the point, not a cost: see "What this deletes".

## Why blocking could never reach where Amy wants it

Amy also asked for waits that outlast a human errand: *"some of those blocks
should be eternal; block on the user indefinitely."* The blocking shape
cannot get there, and the reason is structural rather than a number to tune.

`timeout::gate::CLIENT_CALL` is 330s and is a **compile-time constant in the
client process**. Its own doc says why it cannot be config: the client
"cannot read the kernel's `TimeoutPolicy` — they communicate only over the
wire this deadline bounds." The client fixes its deadline at dispatch, before
the gate exists and before anyone knows what is being asked. Every hop below
it is derived from it by subtracting a 15s margin. So no per-ask budget can
exceed about five minutes without a wire renegotiation mid-call.

The deeper defect is a conflation, and it survives any choice of numbers:
`run_gate`'s deadline calls `approval_ledger::decide::expire`, so **the
caller's wait elapsing kills the ask**. The call's lifetime and the ask's
lifetime are different things wearing one number. Once nothing blocks, they
separate on their own.

## What already exists — verify before building on it

Checked against the code on 2026-08-22, not recalled:

- **The ledger is already the state machine.** Six durable states
  (`Pending`/`Claimed`/`Allowed`/`Denied`/`Expired`/`Abandoned`), durable
  event rows, and a claim race (guarantee 5) that makes concurrent answers
  safe — exactly one answerer wins, losers read a loud failure.
- **Durable before asked** (guarantee 1): `create_ask` commits before
  anything waits. The row survives everything the caller does not.
- **The kernel already announces.** `ledger.changed` on the FlowBus,
  bridged to clients in `kaijutsu-server/src/rpc.rs`. The "kernel instructs
  the client" half is built.
- **The model already has an inbox.** `llm/mailbox.rs` is fed by *blocks*,
  and `BlockKind::Notification` is LLM-visible. Telling a model its ask
  resolved means authoring a block — no new channel.
- **The error vocabulary is already right.** `McpError::GateUnavailable` is
  distinct from `Denied` by Amy's 2026-08-17 ruling, and carries a reason
  the model reads in full. A third state (`Pending`) joins them rather than
  replacing either.

**Verified absent:** `create_ask` does no deduplication — every call makes a
fresh row. And no mechanism redeems an *answered ask*; rules key on
`(statement_digest, authorized_label, …)` and guarantee 3 forbids an ALLOW
rule on a statement with a free variable, which the gate's statement
deliberately has. That is why everything escalates today, and it is why an
answered ask cannot currently authorize anything.

## The shape

```text
call    →  no rule covers it  →  create ask + persist the action  →  RETURNS NOW
                                        │
                                        │   "pending approval, ask 7f3a,
                                        │    nothing was run"
                                        ▼
                              ledger.changed → client (may block locally)
                                        │
                        [minutes, hours, a kernel restart]
                                        │
                              kj ledger allow 7f3a
                                        ▼
                   kernel claims the job, runs the action, authors
                   its result blocks into the originating context
                                        ▼
                        model sees the result whenever it next runs
```

Three rules this has to keep, and the third is what makes it safe:

1. **The action is persisted with the ask, not held in a stack frame.** A
   suspended call cannot survive a restart; a row can.
2. **Approval is single-use.** An allowed ask authorizes exactly one
   execution. It is not a rule and must not become one — a standing
   permission is a different decision with a different UX (see
   `gate-and-shell-split.md`, "Digest-keyed allow-always").
3. **Absence of an answer is never permission.** Unchanged from today, and
   now easier to hold: nothing times out into a verdict, because nothing
   times out at all. An ask sits `Pending` until a human moves it.

## What this deletes

Prefer deleting a mechanism to generalizing it. With nothing blocking:

- `timeout::gate` — `CLIENT_CALL`, `BROKER_CALL`, `MAX_KERNEL_WAIT`,
  `MARGIN`, and the test that keeps the four hops ordered.
- `TimeoutPolicy::effective_gate_wait()` and `gate_wait_timeout`.
- The `shell.rs` clamp (`min(gate_wait, mcp_call_timeout_default - 5s)`).
- The patient hold in `runtime/kj_builtin.rs` for gated verbs — it exists
  only so kaish's watchdog cannot kill a wait that no longer happens. The
  distill-verb hold stays; it is a different problem.
- `run_gate`'s poll loop and its `expire` call.

`Slice 4.8`'s "a killed caller abandons its ask" needs rethinking rather
than deleting: with no blocking caller, there is no death to detect.
Abandonment becomes a property of the *context* (archived, concluded), not
of a caller's liveness.

## Open questions, with recommendations

**Where does the persisted action live?** Recommend a new table in
`KernelDb`, keyed by `request_id`, **not** in `approval-ledger`. The ledger
crate is deliberately free of MCP and tool vocabulary; teaching it what an
instance and a tool call are would put kaijutsu's domain inside a crate that
does not have it. The ledger owns the decision; the kernel owns what to do
about it. Normalized columns, not a JSON blob.

**Exactly-once.** A resumed job must not run twice if the kernel dies
between running and recording. Claim the job the way the ledger claims an
ask — a status column plus `BEGIN IMMEDIATE` — and record the terminal state
in the same transaction that records the result block id. At boot, a job
found `claimed` with no result is the one genuinely ambiguous case: it must
fail closed and author an error, not re-run.

**Staleness.** A persisted action carries the environment it needs (context,
principal, cwd, env). If any of that no longer resolves — the context is
archived, the cwd is gone — the resume must refuse loudly and author an
error block, never approximate. Amy's rule: *"the operation would not go
through without approval"*; the converse is that an approval authorizes
*that* operation, not a similar one.

**Per-ask TTL.** Nullable, `NULL` = eternal. Expiry becomes a janitor's job
over the ledger, decoupled from every caller. A first pass may ship with no
TTL at all — eternal is the default Amy asked for, and a sweep can come
later.

**What the model is told, and when.** Recommend authoring a `Notification`
block on every terminal transition (allowed-and-run, denied, expired), so a
model that is running learns without polling and a model that is not finds
it on rehydrate.

## Slices

1. **The call stops blocking, and an answered ask is redeemable.** Two
   halves, and they cannot be separated — see below. `GateVerdict::Pending
   { request_id }`, a third `PermissionAskOutcome`, a tool result that says
   "pending, ask `<id>`, nothing was run"; the ask stops being expired by
   the caller; and `run_gate` looks for an existing allowed-and-unredeemed
   ask matching this statement *before* creating a new one, redeeming it
   single-use when it finds one. Deletes the poll loop; keeps the ladder.
2. **The persisted action.** The `KernelDb` table, written in the same
   transaction as the ask's creation, plus the claim protocol. No executor
   yet; a test proves the row round-trips and survives a restart.
3. **The executor.** A `ledger.changed` subscriber that claims allowed jobs,
   runs them, and authors result blocks. Boot-time recovery for jobs claimed
   without a result. This is where exactly-once earns its tests.
4. **Notifications and the ladder deletion.** Terminal transitions author
   `Notification` blocks; `timeout::gate` and the patient hold come out.
   **After the executor, never before** — between "stops blocking" and "the
   kernel resumes," the ladder is what still makes a retry work.
5. **TTL and the janitor**, if evidence says it is wanted.

**Why slice 1 has two halves.** An earlier draft of this doc said slice 1
could ship alone because "the model can retry after answering, since nothing
has run." That is wrong, and the reason is the verified-absent finding above:
`create_ask` does no deduplication. A retry would build the same
free-variable statement, find no rule covering it (guarantee 3 forbids one),
create a *second* ask, and return `Pending` again — a loop that never
terminates however many times a human says yes. The redemption check is not
an optimization on top of the state machine; it is the edge that closes it.

With both halves, slice 1 is worth landing alone: an approved action runs on
the model's next attempt, which is the whole behavior change from a model's
point of view, and slices 2–3 upgrade "next attempt" to "immediately, even
with nothing running."

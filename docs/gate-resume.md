# The gate stops blocking

**Read "Rescoped" first if you are here to build something.** This document
is written oldest-first: the 2026-08-22 ruling below is what shipped, and the
2026-08-23 ruling deleted its durable half. Everything about *not blocking the
wire* still holds. Anything promising that the kernel resumes an action across
a restart does not.

**Amy's ruling, 2026-08-22.** A gated tool call must not hold an RPC open
while a human thinks. The kernel tells the client it is waiting; the client
may block locally; nothing blocks on the wire. The original ruling went
further — when the answer lands, *the kernel performs the action itself* —
and that half was reversed a day later.

> *"perhaps we should consider a state machine and not actually having
> anything block on the wire? so kernel would instruct the client it is
> working, client can block, but they don't block on rpc."*

This supersedes the blocking wait that shipped in Slice 4.6
(`gate-and-shell-split.md`), which was verified live holding 81.2s. Deleting
it is the point, not a cost: see "What this deletes".

## Captured result review (September 16)

PostCall and OnError approval reviews work that already ran. Interactive and
approved commands, including authored structured kj calls, retain execution and
the current ask in a durable checkpoint,
publish Waiting blocks, and consume the answer inside their ordered hook
snapshot. Approval continues remaining hooks; it never executes the command or
earlier hooks again. These asks use `hook_result` origin, carry no executable
source, and are excluded from generic retry redemption and the execution/resume
queue. Only the retained execution owner consumes the answer. Inspect them
with `kj ledger list --origin hook_result`.

Cancellation or dropping the wait abandons an unanswered ask. Restart preserves
the captured execution but reports interrupted review; it cannot recreate the
in-memory hook snapshot. Authored structured calls return a typed Pending
refusal while the kernel worker retains review; the RPC does not wait for a
human. Disconnect leaves accepted structured work running. Kernel shutdown
cancels retained review and joins settlement without repeating execution.
Quiet structured calls use the same review owner without authoring transcript
blocks. `kj ledger show <request-id>` includes the captured execution and final
result; its structured data exposes `result_review.captured` and
`result_review.settled`. Every ask in a sequence retains the same invocation link.
Streaming commands retain review on the kernel worker too; their connection
keeps the execution ID active until settlement and output delivery. Interrupt
or disconnect cancels the caller token. Kernel shutdown cancels and joins review
settlement, preserving captured execution even after the RPC adapter departs.
MCP shell commands use the same review owner. Async calls keep their operation
receipt; foreground calls return typed Pending while execution remains retained.
Their kernel worker survives caller disconnect. Other MCP tools still lack a
review owner and fail escalation before creating an ask. See
`docs/kaish-integration.md` for the caller inventory.

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
- **The error vocabulary is already right.** `GateUnavailable` is distinct
  from `Denied` by Amy's 2026-08-17 ruling, and carries a reason the model
  reads in full — shipped 2026-09-01 as `RefusalKind::GateUnavailable` vs
  `RefusalKind::Denied`, both cases of one `McpError::Refused(Refusal)`
  rather than the separate `McpError` variants this was written against
  (`docs/gate-and-shell-split.md`, Ruling 2's shipped-shape note). A third
  state (`Pending`) joins them rather than replacing either.

**Verified absent:** `create_ask` does no deduplication — every call makes a
fresh row. And no mechanism redeems an *answered ask*; rules key on
`(statement_digest, authorized_label, …)` and guarantee 3 forbids an ALLOW
rule on a statement with a free variable, which the gate's statement
deliberately has. That is why everything escalates today, and it is why an
answered ask cannot currently authorize anything.

## Where the pieces live

Two layers, and the split is the thing to hold onto: **`approval-ledger` knows
nothing about kaijutsu.** It stores asks, decisions and rules, and enforces its
own guarantees. It has never heard of a tool, a context, or a shell. The
kaijutsu meaning lives one layer up, in `kj/gate.rs`.

```text
  WHO ASKS                          crates/kaijutsu-kernel/src/kj/
  ─────────                         ──────────────────────────────
  shell_write ──┐                   shell_gate.rs ┐
  a hook's Ask ─┼── builds a ──────  hook_gate.rs  ├─→ GateSpec
  kj cc send  ──┘    GateSpec        cc.rs         ┘   { origin, instance, tool,
                                                        authorized_label,
                                                        statements[] }
                                          │
                                          ▼
                                   gate.rs :: run_gate        ← ALL the policy
                                     1. rules cover it?        (deny wins)
                                     2. answer already given?  (single-use)
                                     3. else record + Pending  (never waits)
                                          │
  ════════ crate boundary ═══════════════ │ ══════════════════════════════════
                                          ▼
  crates/approval-ledger/  — storage + guarantees, no kaijutsu vocabulary
    ask.rs      create_ask, find_redeemable      decide.rs  decide/expire/
    claim.rs    claim (one winner)                          abandon/redeem_ask
    rules.rs    learn_from_approval, redeem      events.rs  append
```

### The tables, and which way the arrows point

```text
                    approval_statements          ← CONTENT-ADDRESSED, SHARED
                    (statement_digest PK,           the same statement body is
                     rendered, kind)                ONE row however many asks
                        ▲        ▲                  reference it
      ┌─────────────────┘        └──────────────┐
      │ digest                            digest │
  approval_ask_statements                  approval_rules
  (request_id, stmt_seq, digest)           (rule_id PK, digest,
      │  the ORDERED list; stmt_seq         authorized_label, scope,
      │  is ask-relative, so the same       allow, learned_from ──┐
      │  statement can be #0 here and       … matched against     │
      ▼  #2 there                           FUTURE asks           │
  ╔══════════════════════════════╗                                │
  ║  approvals   (request_id PK) ║ ◄──────────────────────────────┘
  ║                              ║   learned_from: the ask a rule grew from
  ║  status: pending → claimed   ║
  ║        → allowed | denied    ║   auto_reason set  ⇒ a RULE decided it.
  ║        | expired | abandoned ║   That row is an audit record, never an
  ║  authorized_label            ║   offer — find_redeemable excludes it.
  ║  auto_reason, decided_by     ║
  ╚══════════════════════════════╝
      │            │            │
      │            │            └────────────► approval_options   (the choices
      │            │                                               offered)
      │            └───────────────────────► approval_signals  (lfm2d /
      │                                                         classifier reads)
      ├──────────────────────────────────► approval_events   (append-only audit:
      │                                     created/claimed/decided/expired/
      │                                     abandoned/redeemed)
      │                                          │
      │                                          ▼  trigger on insert
      │                                     ledger_generation ──→ LedgerFlow::
      │                                     (one counter)         Changed → clients
      │
      └──────────────────────────────────► approval_redemptions  (request_id PK)
                                            "this answer has been delivered".
                                            The PK is the single-use guarantee;
                                            the row, not any return value.
```

**One file, one connection.** `KernelDb::conn_for_ledger` returns `&self.conn`
— every table above lives in the kernel's own SQLite database, so an ask and
anything kaijutsu wants to commit alongside it share a transaction.

**Who a human talks to.** `kj/ledger.rs` — `kj ledger list` / `show` / `allow`
/ `deny` / `rules` / `forget`. That is the only surface that decides an ask,
and it writes through the same `decide`/`claim` functions the gate reads.

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
                   the caller's NEXT attempt redeems the answer
                   and runs the action  (2026-08-22 had the kernel
                   run it here; see "Rescoped")
                                        ▼
                        model sees the result whenever it next runs
```

A restart anywhere in that gap sweeps the ask to `Abandoned`, and the human
is told to ask again rather than answering into a void.

Three rules this has to keep, and the third is what makes it safe:

1. ~~**The action is persisted with the ask, not held in a stack frame.**~~
   Reversed 2026-08-23. Nothing is persisted; an ask that cannot outlive the
   process is abandoned at boot instead.
2. **An answer is single-use, and a denial is an answer.** A decided ask —
   allowed *or* denied — is delivered to exactly one retry and is then
   spent. Allowed authorizes one execution and never becomes a standing
   permission; that is what rules are for (see `gate-and-shell-split.md`,
   "Digest-keyed allow-always").

   Denied has to be redeemable for the same reason, found while writing
   slice 1: if only allowed asks were consumable, a model whose request was
   denied would retry, find nothing to redeem, create a *second* identical
   ask, and be told `Pending` again — forever, with a human watching
   duplicate rows pile up in `kj ledger list` and no way to make it stop by
   answering. Today denial reaches the model as `Denied`; losing that would
   be a regression, and the loop would be worse than the regression. One
   question, one answer, delivered once.
   **Only a human's undelivered answer is redeemable**, and both halves of
   that were learned the hard way while building slice 1 — each found by a
   test that refused to be weakened into passing.

   A rule-decided ask (`auto_reason` set) is an *audit record of a call that
   already completed*, not an offer. Every rule-covered call mints one. If
   they were redeemable, `kj ledger forget` would not take effect: the next
   identical request would find a stale authorization and silently use it
   instead of asking anybody. Enforced in `find_redeemable`'s query — a
   property of the row, not something each caller must remember after
   deciding.

   And **learning a rule spends the answer it was learned from.** `kj ledger
   allow --remember always` decides an ask *and* mints a rule; the human said
   yes once. While the rule stands, every covered call is answered by the
   rule and never reaches the redemption step, so without this the source ask
   would sit decided-and-unredeemed indefinitely — and would be the first
   thing found the moment the rule was forgotten. One decision, one use,
   whether the use is running the action or minting the rule.

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

`Slice 4.8`'s `AbandonOnDrop` goes too. It took the abandon signal from the
wait being dropped, and there is no wait to drop.

**Amy's ruling on what replaces it, 2026-08-22: nothing automatic.**

> *"I think abandoned asks are kinda difficult to determine consistently so
> we let them go stale and maybe have a janitor pick it up someday. So maybe
> asks should have an abandoned state it's just not automatic, it's like
> archive, we set it manually and when we have good evidence… that way an ask
> from a context I abandoned hangs around, mostly just annoys me, I can mark
> it abandoned, or when we archive we can sweep and mark asks abandoned.
> that'll be like 90% of the solution."*

So `Abandoned` stays in the ledger and loses its automatic caller. It becomes
a state a human sets, the way archive is — plus a sweep at context-archive
time later. A stale ask sitting in `kj ledger list` is an annoyance with an
obvious manual fix, which is a better failure than a heuristic that marks the
wrong asks abandoned and is hard to notice. Nothing here needs the guess, and
guessing consistently is the part we cannot do.

## Rescoped: the kernel does not resume across a restart

**Amy's ruling, 2026-08-23.** Everything above about *not blocking the wire*
stands and is shipped. What is gone is the durable half — the `gate_actions`
table, the claim protocol, boot recovery, and exactly-once across a crash.

> *"Why put all this effort into resume? I almost feel we should just fail
> tool calls across restarts. the kernel is really reliable and the only
> reason it restarts a lot right now is because we're actively advancing it."*

**The accounting.** Three things were riding on one design, and they separate
cleanly:

| What | Where it lives now | Cost |
|---|---|---|
| Not blocking the wire while a human thinks | shipped, slice 1 | the real driver |
| Redeeming the answer when the caller tries again | shipped, slice 1 | one query |
| Surviving a kernel restart between ask and answer | **deleted** | ~964 lines, and every hard problem in the design |

The first two are most of the value. The third dragged in exactly-once across
a crash, the `claimed`-at-boot row nobody can resolve, and staleness rules for
a context that may be archived by the time an answer lands — and the worst
outcome this design can produce, **an approved destructive action running
twice**, is reachable only on that path. Failing closed is both safer and far
smaller.

### What replaces it

**Pending asks are abandoned at boot.** An ask that silently stops being
answerable is worse than one honestly buried: a human would answer it, be told
nothing, and nothing would happen. At cold start no live waiter can exist by
construction, so every unresolved ask is swept to `Abandoned` with a reason
saying the kernel restarted, nothing ran, ask again. This is the use Amy
described when she designed the state: *"it's like archive, we set it manually
and when we have good evidence, but it can also be used for sweeps."* A cold
start is the best evidence there is.

**The sweep covers `claimed`, not just `pending`.** `ask::list_pending` hides
a claimed row on purpose — an answerer is working it, and a second answerer
would step on the claim. That is true while the process holding the claim is
alive, and only then. At cold start a `claimed` row is an answerer that died
mid-decision, and it is reachable from neither `list_pending` nor
`list_history`: invisible and unanswerable at once. `ask::list_unresolved` is
the read that sees it.

**A decided ask is never swept.** An `allowed` or `denied` answer nobody has
redeemed yet survives the restart untouched — it is still redeemable, and
abandoning it would silently destroy a human's answer.

**cwd is pinned in memory, at ask time.** The gate captures the context's
persisted cwd when it records the ask, keyed by `request_id`, and hands it
back when the answer is redeemed; `shell_write` passes it through
`ExecuteOptions.cwd`.

Without this, a command runs wherever the context has wandered to by the time
a human answers. Picture a recursive delete aimed at a relative path: the
human reads it in `kj ledger show` with one directory in mind, the context
runs `cd` somewhere else before the answer lands, and the approved command
deletes a different tree with the same name. That breaks the rule the whole
gate exists to keep: **an approval authorizes that operation, not a similar
one.**

A pinned directory that no longer resolves is a loud refusal, never a fallback
to the current one. **`ExecuteOptions.cwd` alone does not fail closed** —
proven by falsification: with the explicit `try_set_cwd` validation removed,
the command ran anyway and printed its output. The check is load-bearing.

The pin was in memory on the ruling above: an ask cannot outlive the
process, so neither should its pin.

**Reversed 2026-09-01** (`docs/gate-shape-b.md`, "The cwd moves onto the
ask"). That ruling collided with the one two paragraphs up — a decided ask
**is** never swept at boot — and the collision is reachable: a human answers
`allow`, the kernel restarts before the caller retries, `take_pinned_cwd`
returns `None` on the retry because `cwd_pins` did not survive the restart,
and the outcome comes back `Allowed` with `cwd: None`.
`mcp/servers/shell.rs`'s own `if let Some(cwd)` guard then falls straight
through on that `None` and the approved command runs wherever the context
now sits — its own comment names this "the exact bug this pin exists to
close."

**Ruled and SHIPPED 2026-09-01: the cwd is recorded on the `approvals` row,
and `cwd_pins`, `pin_cwd` and `take_pinned_cwd` are deleted.** The column
arrived through an `ALTER TABLE ... ADD COLUMN` guarded by `PRAGMA
table_info`, the pattern `add_rc_runs_script_count_column_if_missing`
already uses — an added column rebuilds nothing, so none of the
`ON DELETE CASCADE` hazard a rebuild carries applies, which matters because
`approvals` has six cascading children. `run_gate` reads the cwd back off
the row on redemption; an ask that recorded none runs unpinned, the way a
caller with no context does. This is a small, deliberate step toward the
restart survival this document deletes above — one column, not a claim
protocol — because the ask has nothing to verify a cwd against unless it
carries what it was asked under.

**Quiesce covers what can be drained.** A tool call and a model turn are
bounded by machine time, so a graceful shutdown can finish them. An ask is
bounded by human time and cannot be drained at all. The two are complements,
not alternatives — see `docs/issues.md`, "A quiesce flag for graceful restart".

### Two findings that outlived the design they were found in

**Do not reconstruct the action from the ask.** An ask's statements carry
`render_for_review(ps)` — a rendering built for a human to read in
`kj ledger show`, not re-executable source. This stays true and now has a
sharper edge: whatever is *not* in the rendered statement is not covered by
the approval either, because the ledger digests exactly that text.

**The digest is the authorization key, so it must cover everything that runs.**
Found 2026-08-23: `kj cc send` rendered the literal template
`kj cc send ${TARGET} ${MESSAGE}`, so every send to one target hashed
identically and an approval read for one message could be redeemed by another.
`hook_gate` and `shell_gate` both put the real content in the digest; `cc.rs`
was the outlier. A related gap is disclosed rather than fixed:
`shell_write`'s gate covers `parsed.command` and deliberately not
`parsed.stdin` — see `kj::shell_gate`'s module docs for the full honest list.

## Slices

1. **The call stops blocking, and an answered ask is redeemable.** SHIPPED
   (`d8d45d39`). Two halves that cannot be separated — see below.
2. ~~The persisted action.~~ **Deleted 2026-08-23**, the day after it landed.
3. ~~The executor.~~ **Deleted with it** — there is nothing durable to execute.
4. **Abandon on boot, and pin the cwd.** SHIPPED (`8123a873`, `f880285a`),
   with the digest fix (`ee8b6749`).
5. **Delete `timeout::gate` and the patient hold.** The ladder is what still
   makes a retry work, so it comes out only once the rest is deployed and
   living.

**Why slice 1 has two halves.** An earlier draft said slice 1 could ship alone
because "the model can retry after answering, since nothing has run." That is
wrong: `create_ask` does no deduplication, so a retry would build the same
free-variable statement, find no rule covering it (guarantee 3 forbids one),
create a *second* ask, and return `Pending` again — a loop that never
terminates however many times a human says yes. The redemption check is not an
optimization on top of the state machine; it is the edge that closes it.

**The invariant the whole lane turns on:** *only a human's undelivered answer
is redeemable.*

- a **denial** must be redeemable, or a denied caller loops forever minting
  duplicate asks and never learns it was denied;
- a **rule-decided** ask (`auto_reason` set) is an audit record, not an offer;
- **learning a rule spends the answer it was learned from.**

## Still open

**Approval delivery runs on the kernel worker.**
`Kernel::start_approval_delivery` installs one `ledger.changed` subscription and
snapshots the outstanding backlog before returning. It refuses unreadable
startup state. `runtime/approval_resume.rs` re-reads uncollected answers on each
event and each second, handles only Live contexts, and limits delivery work to
four items per scan. Ready completion notices share that budget; it is not a
provider-request budget.

Allowed executable asks are claimed before execution, then fill their existing
command/output pair or author one. Admission reads current pair linkage and
context state under the same database guard as redemption. A link completed
after the delivery scan therefore determines pair ownership and whether the
model performer has changed. Missing approval rows, read errors and malformed
pair identifiers leave the answer unconsumed. Denied and cancelled linked pairs settle without
execution. Other answers write a seed and request automatic continuation
only within the original window; the caller retries and redeems the answer.
Result reviews belong to their retained command owner and are excluded here.

Callers declare whether they publish a pair before the gate creates an ask.
That expectation commits with the ask in `approval_pair_handoffs`. Model calls,
interactive pre-call hooks, authored structured calls and asynchronous shell
operations declare it; quiet, streaming and direct foreground calls have no
pair to publish. A paired caller releases delivery in the same transaction as
its Waiting result, complete ask link and receipt. A bare link or a terminal
caller failure does not release execution. Publication emits a ledger-change
hint so an answer received earlier is scanned again.

The driver checks release before admission, under the claim's database guard.
Matching gate retries cannot redeem a paired executable ask: its original
operation owns the answer. They report that ownership without an AskRef that
a new caller might attach to another pair. Non-executable asks remain eligible
for retry after publication. Terminal publication abandons an unreleased
invocation in the result transaction. Pending asks become Abandoned; Allowed
and Denied decisions remain intact and their answers are spent without execution.
`kj ledger show` reports the publication abandonment reason separately from the
decision, including `publication_abandoned` in structured data. The typed client
preserves that reason; the TUI's ask detail and the app's recent ledger display
it alongside the original decision. Redemption alone never proves execution.

At startup, retained result projections recover first. Any remaining unreleased
invocation has lost its caller and is abandoned; linked pairs settle to Error,
and unlinked asks create no execution. Interrupted operations then settle their
original pairs and receipts atomically. After retiring unresolved asks, server
startup closes receiptless Running/Waiting blocks: statuses, appended stderr
and one explanation per context commit together, preserving recorded output.
A failed write rolls back retirement, and startup fails visibly so
recovery can retry. An abrupt live failure without a terminal result still holds
its ask until restart.

Allowed execution reserves completion delivery in the claim transaction. The
claim stays spent if notification persistence fails. Asynchronous shell admission
reserves the same kind of delivery record with its receipt. A prepared message
and its delivered block are distinct from execution; the block and delivery
marker commit together. Periodic scans retry notices without another ledger
answer and cannot execute source. Startup recovers pending messages from settled
receipts, or reports an unavailable outcome when no pair was recorded. Recovered
notices never replay automatic provider wakes. Archived or reassigned recipients
retain an explicit suppression reason. `kj ledger show` exposes
`completion_notification`; `kj wait --operation` exposes `state.notifications`.
Both report pending, ready, delivered, or suppressed, with a block or reason when
available. These dispositions do not claim that another model request ran.

Shutdown stops delivery, cancels preparation and commands, and waits for command
settlement. A spent claim never authorizes replay, including after a preparation
failure. Restart does not resume approved source. See `docs/gate-shape-b.md`,
"Slice 5: approval executes", and `docs/kaish-integration.md`.

**Why this is available now, and not a return to the durable resume
machinery this document deleted above.** That machinery — the
`gate_actions` table, the claim protocol, boot recovery — existed to survive
a *kernel* restart between ask and answer, and its worst reachable outcome
was an approved destructive action running twice. The subscriber ruled here
adds nothing durable: it is in-memory, the same shape as the cwd pin before
its own reversal above, so a kernel restart between the ask and the
subscriber's run loses the subscription outright — and the pending-ask boot
sweep still means nothing survives a restart to be run twice. This is a
narrower mechanism solving a narrower problem (a caller that will never
retry, on a kernel that stayed up), not the design that was deleted.

Two supporting rulings make it possible:

- **The stability of the environment under an execution is the caller's
  contract.** `cargo build` pulls in whatever it pulls in at link and run
  time; the ledger does not try to reproduce a world, so it does not
  snapshot one. This answers the staleness question this section used to
  leave open.
- **An archived context is completely inert.** Checked twice: once when an
  ask is answered (`kj ledger allow`/`deny` fail on an archived context's
  ask), and again at execution time, because a context can be archived in
  the gap between the two.

**Not yet built.** `docs/gate-shape-b.md` carries the build record and the
two things it needs first: the ledger must store the submission's
executable source beside its human-readable review rendering (this
document's own finding above — `render_for_review` is not re-executable —
still holds and is exactly the gap to close), and whether to refuse
executing a stored statement that carries a free `${VAR}` is an open
question, not yet ruled.

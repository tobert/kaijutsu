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

**Status, 2026-09-20:** most of what "Still open" below narrates is live
behavior, not pending work — read it as a description, not a TODO list. Two
things are genuinely still open: Slice 5, deleting `timeout::gate` and its
patient hold (`kaijutsu-types/src/timeout.rs`; still read by
`mcp/servers/shell.rs` and `runtime/kj_builtin.rs`); and the live-failure and
continuation-admission audit, which `docs/issues.md` still points back at
this section by name.

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

**Reversed 2026-09-01** (below, "The cwd moves onto the
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

## The refusal contract and approval execution

This part is the refusal and wire contract, and the record of how approval
came to execute. Code comments cite its subsections by name, so keep the
headings below as they are.

### What this contract gives a refused caller

`kj ledger allow <id>` — and the command runs. The kernel executes the source
it stored, into the blocks already waiting on that ask.

This contract gives a refused caller three things a bare transport exception
does not: a **structured refusal** instead of a transport exception, an **ask
id** it can hold rather than regex out of prose, and an **answer that
executes** rather than a retry it has to reconstruct byte-for-byte.

- What is wrong and why: `docs/issues.md`, "Gate wiring: one defect, three
  symptoms".
- Which idiom this family gets, and where the family stops:
  `docs/error-chain.md`, "The one shared shape, and where it stops".
- The gate's own doctrine: `docs/gate-and-shell-split.md`.

**This grew into most of the durable-resume design "Rescoped" above deleted.**
`docs/issues.md` split the work in two: this contract was structured refusals
plus a retry keyed on the ask id, and the durable-resume design was approval
alone being enough to run the work, detached from the original caller. The
durable-resume design was held back because it "needs the environment
captured at ask time and an answer for staleness." Both were settled on
2026-09-01 — the cwd moves onto the ask, and environment stability is the
caller's contract — so the retry half of this contract was dropped in favor
of executing on approval.

What is still not the durable-resume design: output lands in the blocks the
original call already authored, not in a work item detached from any call,
and nothing survives a restart. The subscriber is in memory.

### The wire shape

A verdict cannot ride the capnp error channel: `capnp::ErrorKind` is
`Failed`, `Overloaded`, `Disconnected`, `Unimplemented`, and none of them
means "denied". So a refusal rides the result.

capnp rejects a union declared inline in a method's result list — both the
named form (`-> (outcome :union { … })`) and the bare form are parse errors
under capnp 1.5.0. Each of the seven methods therefore names a result struct:

```capnp
struct ShellExecuteOutcome {
  union {
    ok      @0 :BlockId;
    refused @1 :Refusal;
  }
}
```

A union, not a sibling field. A caller that forgets to check a
`hasRefusal()` flag reads a zeroed `BlockId` and believes it succeeded; a
union makes the Rust match exhaustive, so the refusal cannot be skipped by
inattention. This is the "keep it loud" constraint applied to the wire —
never a success with a status field nobody reads.

**Ordinals do not move.** Only the return *types* change, so the seven keep
`@2 @7 @43 @45 @58 @87 @99` and no `retiredNN` stub is needed. Permitted
under the flag-day rule: wire only, never storage.

`Refusal`, `RefusalKind`, `AskRef` and `AskStatus` are defined once beside
the other error types in `kaijutsu.capnp`, and mirrored in
`kaijutsu-types/src/refusal.rs` for the Rust halves.

**`AskStatus` is a second enum on purpose.** `approval-ledger` and
`kaijutsu-types` are independent leaves — neither depends on the other — and
making one depend on the other to share six variants is the more expensive
mistake. A kernel test pins the correspondence, the way `Status` and its
capnp ordinals are already pinned.

### The type must survive every boundary, and there are five

The VFS family proved the failure mode: fixing capnp bought nothing until the
client's actor stopped re-flattening the result one hop later.

```text
McpError → capnp → RpcError → CallError → the app
```

**Every boundary that stringifies is a place the type dies.** A fix that
stops at the wire is not a fix. `RpcError::Refused` and `CallError::Refused`
are both required.

### Three findings that change the build

#### 1. A mandatory `HookId` forced the model's main shell path off the type

`McpError::Denied`, `GatePending` and `GateUnavailable` each require a
`by_hook: HookId`. The direct `shell_write` gate has no hook behind it, so it
cannot construct any of them, and
`crates/kaijutsu-kernel/src/mcp/servers/shell.rs:505` reports every
non-allowed outcome as `McpError::Protocol` — a **fault** variant carrying a
**verdict**, with the distinction left in the words:

```rust
return Err(McpError::Protocol(format!(
    "{headline} [{}]: {} — nothing was run",
    outcome.ask_description(),
    outcome.reason
)));
```

This is the path a model actually takes. The three-way split exists and this
path cannot reach it.

**The fix is to carry a `Refusal`, not a `HookId`.** `Refusal.subject` is
empty when nothing has a name, so a hookless gate produces a true
`GatePending` instead of a `Protocol` fault.

#### 2. The ask id is born as prose

`GateOutcome::ask_description()` (`kj/gate.rs:162`) is
`format!("ask {} ({})", a.request_id, a.status)`, and every consumer carries
that string forward. The typed `AskRef` already exists one line above it and
is discarded at the first layer. Nothing downstream can recover the id
without parsing — which is exactly what a test does today
(`mcp/servers/shell.rs:1167`, taking the first whitespace-separated token of
a `kj ledger list` line).

#### 3. A decided ask outlives its cwd pin

The collision between "the cwd pin lives in memory" and "a decided ask is
never swept at boot" is narrated above, under "Rescoped: the kernel does not
resume across a restart" ("**Reversed 2026-09-01**"). Two facts from the
original finding are not told there and stay here: `mcp/servers/shell.rs:517`
assigns the returned `None` straight to `pinned_cwd`, so the `if let
Some(cwd)` validation below it never runs; and the guard at `shell.rs:552`
only fires when a pin *exists* and fails to resolve — a *missing* pin falls
through silently. The gap is narrow — the restored cwd usually equals the
pinned one, since both are read from `context_shell.cwd` — but reachable
whenever the context `cd`s between escalation and restart, which is the
scenario the pin was built for.

This is the finding that put the cwd fix in this contract rather than
leaving it to the pin: "verify that context still matches the ask" cannot be
done against a pin that only lives in memory. See "The cwd moves onto the
ask", below.

### What changes in redemption

Today (`kj/gate.rs:526`, the one production call site): the caller re-sends
the statement, the gate re-renders it, re-derives a digest per statement, and
`find_redeemable` requires an exact **set** match of those digests against
the ask's stored statements — plus label, context, principal, `status IN
('allowed','denied')`, `auto_reason IS NULL`, and not already in
`approval_redemptions`.

Under Shape B the caller presents `request_id`. Everything in that predicate
**except the digest set match** still has to hold. The digest comparison is
replaced by a direct lookup, and the statement text to execute is read back
from the ledger (`ask::load_ask_statements`), so there is one copy instead of
two and nothing to reconcile.

**Allow rules are a different mechanism and do not change.**
`approval_rules` is keyed on `statement_digest` and served by
`rules::redeem`; `find_redeemable` never touches it. Removing statement
matching from single-use redemption leaves standing rules exactly as they
are.

**Exactly-once is already structural.** `approval_redemptions.request_id` is
a `PRIMARY KEY` and `redeem_ask` decides on the `INSERT`'s row count rather
than reading first. Presenting an id does not weaken it.

### The cwd moves onto the ask

**Settled and shipped:** the cwd is recorded on the `approvals` row instead
of pinned in memory; see "Rescoped: the kernel does not resume across a
restart", above, for the column and its `ALTER TABLE` mechanics. Redemption
then verifies instead of shrugging:

| ask's recorded cwd vs the context's live cwd | what happens |
|---|---|
| match | run there |
| diverged | refuse, naming both directories |
| the ask recorded none | run unpinned, as a caller with no context does today |

The two alternatives were refusing on a lost pin — which leaves a human's
approval permanently uncollectable — and sweeping decided-unredeemed asks at
boot, which discards that approval outright. Both trade a silent wrong
directory for a dead ask.

This records a piece of the caller's environment durably: this contract is
"the kernel executes the stored statement after verifying the context still
matches the ask", and there is nothing to verify against if the ask does not
carry what it was asked under. It is one column, not a claim protocol.

### Slices: the wire and execution build

1. **The shared types.** SHIPPED. `kaijutsu-types::refusal` and the capnp
   declarations. No behavior change.
2. **`McpError` carries a `Refusal`.** SHIPPED. The three gate variants
   collapsed to `McpError::Refused(Refusal)` — the kind carries what the
   variants did — and the hookless `shell_write` gate produces a real
   verdict for the first time (finding 1). `settled_block_status()` reads
   the kind and keeps its one mapping.

   It also closed something not in the plan: `PhaseOutcome::Deny` carried a
   reason the LLM-visible path discarded, which is why `docs/issues.md`'s
   first hard receipt showed a broken hook as a bare "denied by hook
   shell-escape-guard". Denials keep their reason now — a D-28 change, and
   the one `docs/gate-and-shell-split.md` already argued for.
3. **The wire.** Seven result structs, the server side, the client side,
   `RpcError::Refused` → `CallError::Refused`. Two helpers carry a refusal
   across: `set_refusal` on the server, `refusal_from_capnp` on the client,
   both total matches so a new kind is a build error.
4. **The consumers.** SHIPPED, and larger than expected. The client wrappers
   kept their signatures, so app/mcp/acp needed no changes and their existing
   error rendering picked up the structured `Display` — reason, ask id and
   remedy — for free.

   What did need work was **the model's own tool path**, a seventh
   block-settling site that never reached `settled_block_status()`.
   `llm_stream.rs` derived `final_status` from `is_error`, so a pending ask
   settled its ToolCall/ToolResult pair `Error` and reached the model as
   `"Execution error: …"`. That is the collapse this lane exists to remove,
   on the surface where it costs the most: a model reads a crash, retries,
   and mints another ask. `map_tool_dispatch_result` now returns the settled
   status beside the error flag, because the two answer different questions —
   `is_error` is the D-28 channel and is always true for a refusal; `status`
   is what the blocks settle to and is `Waiting` for a pending one.

   Each kind also carries a stable `ErrorPayload.code` (`gate.pending`,
   `gate.denied`, `gate.unavailable`, `capability.*`), following
   `tool.timeout`'s precedent, so a consumer branches on a code rather than
   on prose.
5. **Approval executes.** SHIPPED. The cwd and `exec_source` columns, the
   block link, the free-variable snapshot, the executor branch in the
   `ledger.changed` driver, and the split of `PENDING_REASON` into an
   executes text and a retry text. The digest set match STAYS (below).

   **Live for every shell origin.** The first live probe after deploy
   showed the gap: with hooks installed, every production shell ask comes
   through `hook_gate.rs`, which carried no source, no plan and no
   variables, so approval executed for nothing and, worse, an ALLOW rule
   remembered on `dd of=${DEV}` would have redeemed every future value of
   `DEV`. The hook gate now plans a shell-shaped call the way the shell
   gate plans a submission: the command rides as `exec_source`, the
   planned statements feed the snapshot, and the free and bound names go
   on the statement so the rule refusal fires. The RPC shell box's own
   pair fills; the MCP `shell` path gets a pair authored and a wake. A
   command that does not parse keeps the retry shape. The wire tests still
   synthesize the link, because the harness installs no hook.

### Slice 5: approval executes

`kj ledger allow <id>` runs it. The caller checks its own blocks.

**Approval triggers execution (Amy, 2026-09-01).** The answer runs the
stored source and fills the command and output blocks already sitting
`Waiting` on that ask. There is nothing for a caller to present, so the `ask`
tool parameter and `kj ledger redeem <id>` from the earlier sketch are both
gone.

**Approval delivery owns execution as well as wakes.**
`runtime/approval_resume.rs` subscribes to `ledger.changed` on the kernel worker.
It claims executable answers before running source; other answers wake their
caller to retry. The redemption primary key prevents a second execution, and a
spent claim never permits restart replay. Preparation failures after the claim
consume the approval without running its source.

**What changed to make this available.** This document originally gave two
reasons this was not on the table: the environment had to be captured at ask
time, and staleness had no answer. Both are now settled — the cwd moves onto
the ask (above), and the stability of the environment under an execution is
the caller's contract ("What the ask must carry, and what it must not try
to", below). What remains for the ask to carry is small and entirely
durable: executable source, principal, context, cwd.

**This is not the durable resume machinery deleted in August** — "Still
open", below, tells the fuller version of that comparison.

**It also closes the stranded pair.** A refused `submitInput` has already
authored its command and output blocks, and they settle `Waiting` on the ask.
Under a caller-presents-the-id shape, a retry authors a second pair and leaves
the first waiting forever. Executing on approval fills in the pair that is
already there, which is the behavior the `Waiting` status was introduced to
describe.

#### What must be built first

- **An `exec_source` column on `approvals`, because no existing field is it.**
  `approval_statements.rendered` is `render_for_review(ps)` — the plan's
  rendering plus appended human-readable `NOTE:` lines about unquoted heredoc
  delimiters (`kj/shell_gate.rs:125`, `:173`). Handing that to kaish would run
  the notes. This document's own earlier finding says it outright: *"an
  ask's statements carry `render_for_review(ps)` … not re-executable
  source."*

  There is a field that happens to hold executable text, and reaching for it
  would be a mistake. `authorized_label` is the submitted source for a
  `ShellGate` ask (`kj/shell_gate.rs:160`) and the **target session name**
  for a `KjVerb` one (`kj/cc.rs:193`). Executing "whichever field is
  executable for this origin" is an implicit per-origin rule that a fourth
  origin would silently get wrong. Store the executable text in a column
  whose only job is that.

  `GateSpec` gains `exec_source: Option<String>`. `None` means this ask
  cannot run on approval and the caller must retry — today's behavior,
  preserved for any origin not wired to execute. It is not a silent fallback:
  the refusal's `remedy` says which one the caller is getting.

  The gate is all-or-nothing per submission, so the executable unit is the
  whole submission, not the per-statement renderings.
- **A free `${VAR}` means the approved text and the executed bytes can
  differ.** Guarantee 3 already refuses to learn a RULE for such a statement.
  The proposal was to refuse to execute one. **Instead (Amy): snapshot
  the values onto the ask** — "The ask carries its free variables", below.

#### The rest, settled

- `approvals` gains `cwd` and `exec_source` through `ALTER TABLE ... ADD COLUMN`
  guarded by `PRAGMA table_info`, following
  `add_rc_runs_script_count_column_if_missing`. An added column rebuilds
  nothing, so the FK-cascade hazard that table rebuilds carry does not apply.
  This is the crate's second ALTER-TABLE step; the third is the point to
  build the ladder its doc comment already names.
- `cwd_pins`, `pin_cwd` and `take_pinned_cwd` are deleted with it.
- `approval_redemptions.request_id` is a `PRIMARY KEY` and `redeem_ask`
  decides on the `INSERT`'s row count, so exactly-once is already structural
  and does not change.
- **`find_redeemable`'s digest set match STAYS, and deleting it would have
  been wrong.** It is only removable for an ask that redeems by id, and the
  subscriber does not call `find_redeemable` at all — it looks a row up
  directly. What still uses the matcher is the RETRY path, which is every
  origin with `exec_source: None`.

  Deleting it there would reopen a bug closed on 2026-08-23. `kj cc send`
  renders the concrete message into its statement precisely so the digest
  varies with it (`kj/cc.rs:138`); without the digest in the predicate,
  `find_redeemable` matches on label + context + principal, and an approval
  read for one message would redeem a send of any other message to the same
  target. The matcher is not the duplication Shape B set out to remove — it
  is the authorization key for callers that still retry.

  It becomes deletable when every origin executes on approval, not before.
- `PENDING_REASON` stops saying "run the same command again". It should say
  what actually happens now: answer it, and the command runs.

### The ask has to name its blocks, and the gate cannot

An execution on approval fills the command and output blocks the original
call already authored. That needs the ask to name them, and nothing in the
gate path can: `shellExecute` creates the pair BEFORE gating, but reaches the
gate through `broker().shell_pre_call_hooks`, which knows nothing about
blocks.

**The caller records the link after escalation.** `run_gate` returns
the ask id, and `execute_shell_command` already holds both block ids, so it
is the one scope where the three are together. `approvals` gains
`command_block_id` and `output_block_id` (`BlockId::to_key()` form), written
through `KernelDb::link_ask_blocks` so the ledger connection stays behind the
kernel rather than being reached from the server.

Best-effort and logged: the refusal is already correct and already returned,
so a failed link degrades to the subscriber authoring fresh blocks, never to
a failed call.

**The MCP `shell_write` path links nothing** — it has no pair at gate time,
and its ToolCall/ToolResult blocks are authored by the layer above it. An ask
from that path carries `NULL`, and the subscriber authors into the ask's
context instead.

**Coverage.** `link_ask_blocks` is unit tested both ways, including that an
unknown ask is an error rather than a silent no-op. The executor is covered
by `kaijutsu-server/tests/gate_executes_wire.rs` over the real surfaces —
`kj ledger allow|deny` answers from a second context — from both origins.
The `shell_write` cases mint the ask over MCP and synthesize the block link,
which isolates the subscriber from the gate. The `shell_box_*` cases drive
the shipped path whole: `shellExecute` authors the pair, a PreCall `Ask`
hook on `shell_write` refuses it with the command as `exec_source` (the
hook gate plans a shell-shaped call the way `shell_gate` does) and the
pair linked as `PairOwner::Session` — `execute_shell_command`'s own link
call — and the allow fills that same pair with no second pair and no seed
block, because a session-owned pair tells nobody; the deny settles it
`Error`. Dropping the link call in `execute_shell_command` fails the allow
case.

### What the ask must carry, and what it must not try to

**The stability of the environment under an execution is the caller's
contract (Amy).** This is restated below, under "Still open" —
`cargo build` pulls in whatever it pulls in at link and run time, and the
ledger does not try to reproduce a world, so it does not snapshot one.

This is what makes an approval executable at all. A CAS snapshot was
considered and declined — the store exists, but a snapshot of a tree is not
the environment (toolchain, network, link-time inputs), so it would buy
confidence it cannot honor.

**Not now, but the shape it would take:** a hook that takes a btrfs or
container snapshot and steps it forward. Deliberately deferred — do not
design around it.

So the ask carries only what the *kernel* must know to run the thing at all:
the executable source, the principal, the context, and the cwd. Everything
else is the caller's.

### The ask carries its free variables

**Snapshot, do not refuse (Amy, 2026-09-02).** Approval captures the
initial environment a contextual shell would read. `ContextShellInputs` supplies
both construction and capture: selected cwd, durable exports, and host execution
policy. HOME comes from the interpreter defaults, PWD from the selected initial
cwd, and PATH from the kernel startup capture when Exec is granted. Durable
exports override those values. Read-only shells never gain host execution from
an environment value.

`kj::env_snapshot` takes the union of statement and non-literal heredoc free
variables, deduplicated in first-seen order. `approval_env` stores each value or
explicit unset (`request_id, seq, name, value NULL-for-unset`). Before execution,
the shared restore helper exports captured values and unsets captured absences.
It validates all names and refuses duplicates before mutation; temporary overlay
names cannot collide with any target. A restore failure prevents execution.

Cwd and durable exports are read under one database lock. Storage faults refuse
before recording an ask, including dry-run audit asks. The hook plan reader also
reports a capture failure before running the classifier. Construction initializes
kaish at the selected cwd, then validates that directory in its VFS namespace;
there is no second database restore that can select a newer cwd.

**Both consumers use the same input rules** (Amy: the classifier "should see
the same data"). The broker's `KJ_TOOL_PLAN` includes `env: [{name, value|null}]`
beside `statements`. Hook classification and ask creation are independent
snapshots; intervening durable changes can affect the later one. The lfm2d scorer
still does not substitute the environment into its clauses; see `docs/issues.md`.

**The human sees the values on the ask's `description`, not on the
statement rendering.** `approval_statements` is content-addressed and
inserted once per digest, so a value baked into that row would show a later
ask with different values the first ask's stale ones. The description is
per-ask and already the line `kj ledger show` prints.

**What it does not cover.** A command substitution runs a program and a
clock reads the wall; neither is a variable, and neither appears as a free
name. Those stay under the position that environment stability is the
caller's contract.

### The subscriber, in order

Approval delivery runs on the kernel worker. `start_approval_delivery` installs
one subscription and snapshots old answers before returning; unreadable backlog
refuses host startup. Shutdown stops new delivery, cancels preparation and
execution, and joins command settlement. A claimed action is never replayed.
Cancelled preparation reports that no source ran; commands already running use
the shared command cancellation and settlement path. Preparation unwinding
settles only the claimed pair, or records a no-run error if no pair exists, then
propagates the original panic. Shutdown retains delivery seeds for claimed work
before joining, without starting another model turn.

On each `ledger.changed` the driver re-reads the undelivered answers and,
for each it has not acted on: resolves the context and refuses anything not
Live; reads the whole approval row, because `exec_source` decides the branch
and the answer summary does not carry it. No `exec_source`: the old wake,
unchanged. A denial or cancellation with a linked pair: settle the pair
`Error` with the reason on stderr, then redeem. A `Session` pair's blocks
are its delivery. A `Turn` pair also gets a new seed saying that the action
did not run, because its cached mailbox cannot observe the in-place edit. A
terminal answer with no pair falls back to the wake. An allow: read liveness
and the turn performer's assignment, then claim under the same database lock.
Read faults leave the answer unclaimed for retry. Only the claim winner may
settle a changed performer's pair or execute source. A repeated delivery cannot
replace previously accepted output after reassignment. Then materialize a shell for the ask's principal and context under a synthetic
session id, resolve or author the pair, move to the ask's cwd, restore the
ask's env, run.

**`link_ask_blocks` records who owns the pair, not just its ids.** A run
into a `PairOwner::Session` pair — a connected session's own blocks, which
it watches directly — tells nobody. A run into a pair whose turn ended at
the gate — `PairOwner::Turn` (a model's own tool call) or one the driver
authored fresh because the ask named none — gets a seed block naming the
output; the seed also carries a turn request when no turn is in flight, and
stands alone when one already is, because the fill is an in-place edit a
running turn's cached mailbox will not re-read on its own `catch_up`. The
same distinction applies when an allowed action cannot materialize a shell,
restore its environment, or enter its recorded directory: a `Turn` receives
an explicit no-run seed; a `Session` pair stays settled-only.

Once a seed is durable, rejected turn admission does not repeat it on later
ledger changes. The next manual drive can read the seed. An ordinary answer
remains unredeemed until its caller retries; delivery does not grant a second
execution claim.

**What a crash costs.** The redemption row is claimed before the run, so a
crash between the two loses the action: the ask reads redeemed, nothing
ran. That is the chosen side; the other ordering runs an approved
destructive action twice. Context liveness is checked before the claim under
the same database lock; missing or archived contexts leave the answer unclaimed.
A shell that will not materialize or a cwd that no longer resolves land in the same
place by design: the approval is spent, the pair says why, the human asks
again if they still want it.

**Not gated on `turn_in_flight`.** A running turn is a reason not to spend
a turn waking someone, not a reason not to run an approved action or to
withhold a turn-owned pair's seed; only the turn request is skipped while
one is already running.

### Archived contexts are inert

An archived context runs nothing and answers nothing. Two checks, because one
is not enough:

1. **On request.** `kj ledger allow`/`deny` and the other ask actions fail
   when the ask's context is archived. A human cannot answer a question on
   behalf of a dead context.
2. **At execution time.** Whatever triggers the run checks again before
   running. The gap between an answer and its execution is exactly where a
   context can be archived, so the first check cannot stand alone.

`ContextState::Archived` and `archived_at` both exist and are already read
together (`kj/context.rs:1304`); the checks match that precedent rather than
inventing a third reading. **The precedent is load-bearing:** `archive_context`
stamps `archived_at` only and leaves `context_state` at `live`, so a check
that read the state column alone never saw an archived context — which is
what the gate-resume driver's original Live check did, found by the wire
test for check 2. Both checks now read both halves.

**Archiving should also sweep the ledger.** A context going archived leaves
its unresolved asks answerable-in-principle and dead-in-fact. The existing
`ApprovalStatus::Abandoned` and the boot sweep are the machinery to reuse —
same shape as `abandon_unresolved_on_restart`, a different trigger and a
reason naming the archive.

### The receipts

Moved to `docs/devlog.md`, "The answer that travelled as an error". The
five asks from one probe, the broken control presented as a verdict, and
the live one are the story this design was argued from.

### Adjacent, not in scope

Three string classifiers of the same family, none about the gate:

- `is_disconnect_error` (`kaijutsu-client/src/actor.rs:1902`) — tears down a
  connection on `msg.contains("Disconnected")`. Named as open in
  `docs/error-chain.md`.
- `is_retryable_label_conflict` (`kaijutsu-mcp/src/lib.rs:1904`) —
  `CallError::Rpc(msg).contains("label conflict")`.
- `is_session_lost_error` (`kaijutsu-app/src/view/editor/mod.rs:135`) —
  `"no such session"`.

Each is a typed error flattened to prose and grepped back. They belong to
their own families and must not be folded into the refusal shape.

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
failure. Restart does not resume approved source. See above,
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

**Built above.** "Slice 5: approval executes" and "The ask carries its free
variables" cover the two things this needed first: the ledger stores the
submission's executable source in its own `exec_source` column, separate
from `render_for_review`'s human-readable rendering (this document's own
finding above — `render_for_review` is not re-executable — still holds and
is exactly the gap that column closes), and whether to refuse executing a
stored statement that carries a free `${VAR}` is now settled: snapshot the
value onto the ask rather than refuse.

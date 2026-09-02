# Gate Shape B — the answer executes

`kj ledger allow <id>` — and the command runs. The kernel executes the source
it stored, into the blocks already waiting on that ask.

Shape B gives a refused caller three things it does not have today: a
**structured refusal** instead of a transport exception, an **ask id** it can
hold rather than regex out of prose, and an **answer that executes** rather
than a retry it has to reconstruct byte-for-byte.

- What is wrong and why: `docs/issues.md`, "Gate wiring: one defect, three
  symptoms".
- Which idiom this family gets, and where the family stops:
  `docs/error-chain.md`, "The one shared shape, and where it stops".
- The gate's own doctrine: `docs/gate-and-shell-split.md`.
- What redemption does today: `docs/gate-resume.md`.

**B has grown into most of A.** `docs/issues.md` split the work in two: B
was structured refusals plus a retry keyed on the ask id, and A was approval
alone being enough to run the work, detached from the original caller. A was
held back because it "needs the environment captured at ask time and an
answer for staleness". Both were settled on 2026-09-01 — the cwd moves onto
the ask, and environment stability is the caller's contract — so the retry
half of B was dropped in favour of A's trigger.

What is still not A: output lands in the blocks the original call already
authored, not in a work item detached from any call, and nothing survives a
restart. The subscriber is in memory.

## The wire shape

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

## The type must survive every boundary, and there are five

The VFS family proved the failure mode: fixing capnp bought nothing until the
client's actor stopped re-flattening the result one hop later.

```text
McpError → capnp → RpcError → CallError → the app
```

**Every boundary that stringifies is a place the type dies.** A fix that
stops at the wire is not a fix. `RpcError::Refused` and `CallError::Refused`
are both required.

## Three findings that change the build

### 1. A mandatory `HookId` forced the model's main shell path off the type

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

### 2. The ask id is born as prose

`GateOutcome::ask_description()` (`kj/gate.rs:162`) is
`format!("ask {} ({})", a.request_id, a.status)`, and every consumer carries
that string forward. The typed `AskRef` already exists one line above it and
is discarded at the first layer. Nothing downstream can recover the id
without parsing — which is exactly what a test does today
(`mcp/servers/shell.rs:1167`, taking the first whitespace-separated token of
a `kj ledger list` line).

### 3. A decided ask outlives its cwd pin

`docs/gate-resume.md` rules the cwd pin in-memory: *"an ask cannot outlive
the process, so neither should its pin."* The same document rules that a
decided ask **is** never swept at boot, because abandoning it would destroy a
human's answer.

Both rules are right on their own. Together they leave a hole:

1. a human answers `allow`; the caller has not retried yet;
2. the kernel restarts — the pending sweep does not touch a decided ask, so
   the answer survives; `cwd_pins` is a process-lifetime `HashMap`, so the
   pin does not;
3. the caller retries, `take_pinned_cwd` returns `None`, and the outcome is
   `Allowed` with `cwd: None`;
4. `mcp/servers/shell.rs:517` assigns that `None` to `pinned_cwd`, the
   `if let Some(cwd)` validation below it does not run, and the approved
   command executes in whatever directory the context is at now.

The guard at `shell.rs:552` fires only when a pin **exists** and fails to
resolve. A **missing** pin falls through silently into the outcome its own
comment calls "the exact bug this pin exists to close (approve in directory
A, run in directory B)."

Narrow — the restored cwd usually equals the pinned one, since both are read
from `context_shell.cwd` — but reachable whenever the context `cd`s between
escalation and restart, which is the scenario the pin was built for.

**This is Shape B's business**, because "verify that context still matches
the ask" cannot be done against a pin that only lives in memory. Ruled
below.

## What changes in redemption

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

## The cwd moves onto the ask

**Ruled: record it durably; delete the pin.** The `approvals` row gains the
cwd its ask escalated in, and `cwd_pins`, `pin_cwd` and `take_pinned_cwd` come
out with it.

Redemption then verifies instead of shrugging:

| ask's recorded cwd vs the context's live cwd | what happens |
|---|---|
| match | run there |
| diverged | refuse, naming both directories |
| the ask recorded none | run unpinned, as a caller with no context does today |

The two alternatives were refusing on a lost pin — which leaves a human's
approval permanently uncollectable — and sweeping decided-unredeemed asks at
boot, which discards that approval outright. Both trade a silent wrong
directory for a dead ask.

This does record a piece of the caller's environment durably, which is a step
toward the restart survival `docs/gate-resume.md` deleted. It is a small and
deliberate one: Shape B's contract is "the kernel executes the stored
statement after verifying the context still matches the ask", and there is
nothing to verify against if the ask does not carry what it was asked under.
It is one column, not a claim protocol.

## Slices

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
5. **Redeem by id.** The ledger read, the divergence check, the cwd column
   replacing the pin map, and the deletion of the digest set match.
   `PENDING_REASON` stops saying "run the same command again".

## Slice 5: approval executes

`kj ledger allow <id>` runs it. The caller checks its own blocks.

**Ruled (Amy, 2026-09-01): approval triggers execution.** The answer runs the
stored source and fills the command and output blocks already sitting
`Waiting` on that ask. There is nothing for a caller to present, so the `ask`
tool parameter and `kj ledger redeem <id>` from the earlier sketch are both
gone.

**This modifies a driver that already exists; it does not add one.**
`spawn_gate_resume_driver` (`kaijutsu-server/src/rpc.rs`) already subscribes
to `ledger.changed` and already finds the answers nobody has collected. What
it does with them is WAKE the context — a seed block plus a turn request —
so the woken turn retries the call and that retry redeems. The change is to
run the source instead of driving a turn.

**Its doc comment carries the warning to read first.** It notes that it does
not need exactly-once and deliberately does not implement it, because "every
hard problem it carried — exactly-once across a crash, a `claimed` row
nobody can resolve — belonged to executing the action, and this does not
execute anything." Executing is exactly what the ruling asks for, so that
sentence stops being a reason the driver is simple and becomes the list of
things to get right. Staying in memory with nothing surviving a restart is
what keeps it to the within-process exactly-once `approval_redemptions`
already provides.

**What changed to make this available.** `docs/gate-resume.md` gave two
reasons this was not on the table: the environment had to be captured at ask
time, and staleness had no answer. Both are now settled — the cwd moves onto
the ask, and the stability of the environment under an execution is the
caller's contract (below). What remains for the ask to carry is small and
entirely durable: executable source, principal, context, cwd.

**This is not the durable resume machinery deleted in August.** That design
carried a `gate_actions` table, a claim protocol and boot recovery, and its
worst reachable outcome was an approved destructive action running twice. The
subscriber is in memory and adds nothing durable. A pending ask is still
abandoned at boot, so nothing survives a restart to be run twice.

**It also closes the stranded pair.** A refused `submitInput` has already
authored its command and output blocks, and they settle `Waiting` on the ask.
Under a caller-presents-the-id shape, a retry authors a second pair and leaves
the first waiting forever. Executing on approval fills in the pair that is
already there, which is the behavior the `Waiting` status was introduced to
describe.

### What must be built first

- **An `exec_source` column on `approvals`, because no existing field is it.**
  `approval_statements.rendered` is `render_for_review(ps)` — the plan's
  rendering plus appended human-readable `NOTE:` lines about unquoted heredoc
  delimiters (`kj/shell_gate.rs:125`, `:173`). Handing that to kaish would run
  the notes. `docs/gate-resume.md` says it outright: *"an ask's statements
  carry `render_for_review(ps)` … not re-executable source."*

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
  differ.** Guarantee 3 already refuses to learn a RULE for such a statement;
  a single-use ask has no equivalent guard, and executing stored source makes
  the gap reachable rather than theoretical. The proposal is to refuse to
  execute a stored statement with free variables and say so — not yet ruled.

### The rest, settled

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
- **`find_redeemable`'s digest set match STAYS, and the plan to delete it was
  wrong.** It is only removable for an ask that redeems by id, and the
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

## The ask has to name its blocks, and the gate cannot

An execution on approval fills the command and output blocks the original
call already authored. That needs the ask to name them, and nothing in the
gate path can: `shellExecute` creates the pair BEFORE gating, but reaches the
gate through `broker().shell_pre_call_hooks`, which knows nothing about
blocks.

**Ruled: the caller records the link after escalation.** `run_gate` returns
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

**Coverage gap, named rather than papered over.** `link_ask_blocks` is unit
tested both ways, including that an unknown ask is an error rather than a
silent no-op. The CALL SITE in `execute_shell_command` is not covered — it
needs an RPC-level harness, and the subscriber work needs one anyway.

## What the ask must carry, and what it must not try to

**The stability of the environment under an execution is the caller's
contract** (Amy, ruling). `cargo build` pulls in whatever it pulls in at link
and run time; we trust it is there and we let the caller manage its target.
The ledger does not try to reproduce a world.

This is the answer to the staleness question `docs/gate-resume.md` left open,
and it is what makes an approval executable at all. A CAS snapshot was
considered and declined — the store exists, but a snapshot of a tree is not
the environment (toolchain, network, link-time inputs), so it would buy
confidence it cannot honor.

**Not now, but the shape it would take:** a hook that takes a btrfs or
container snapshot and steps it forward. Deliberately deferred — do not
design around it.

So the ask carries only what the *kernel* must know to run the thing at all:
the executable source, the principal, the context, and the cwd. Everything
else is the caller's.

## Archived contexts are inert

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
inventing a third reading.

**Archiving should also sweep the ledger.** A context going archived leaves
its unresolved asks answerable-in-principle and dead-in-fact. The existing
`ApprovalStatus::Abandoned` and the boot sweep are the machinery to reuse —
same shape as `abandon_unresolved_on_restart`, a different trigger and a
reason naming the archive.

## The receipts

Kept here because they are what the design was argued from, and
`docs/issues.md` is a backlog of what is *not* done. They belong in a devlog
chapter once this arc closes.

**Five asks from one probe, 2026-08-30.** Probing a gated surface minted five
durable rows, because each refusal arrived as a transport error, every
transport error invited a retry, and every retry with different text minted a
new ask. That is the cost of a verdict on the fault channel, measured.

**A broken control presented as a verdict, 2026-08-30.** `shell-escape-guard`
was installed pointing at a hook body path the `/config` melt had moved out
from under it. It could not read its own body and failed closed, denying
every `shell_write` call in the kernel — including the `kj` verbs needed to
diagnose it. What reached the caller was:

```
remote exception: denied by hook shell-escape-guard
```

No reason. The reason existed the whole time, one layer away in the journal:

```
hook.deny hook_id=hook:shell-escape-guard phase=PreCall
  reason=kaish hook body at "/etc/rc/lib/hooks/shell-guard.kai" could not be
         read: not found: No such file or directory (os error 2)
```

`emit_deny_attribution` called that deliberate — the model was to see only
the hook id. `docs/gate-and-shell-split.md` had already argued the collapse
was wrong on doctrine: the usual reason to hide gate state is an adversary,
and there is none inside the trust boundary. This was that argument with a
cost attached, and it is why a denial now carries its reason.

**A live one, 2026-09-01.** A `shell` call came back as `Cap'n Proto error:
remote exception: gate ... is waiting on a human: ask 01a05d19-...` — an open
question, delivered on the channel that means "I could not tell you what
happened", with the id a caller needs recoverable only by regex.

## Adjacent, not in scope

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

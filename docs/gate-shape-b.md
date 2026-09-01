# Gate Shape B — the claim ticket

`kj ledger allow <id>`, then present `<id>`. The kernel runs the statement it
stored, in the caller's live context, after checking that context still
matches the ask.

Shape B gives a refused caller three things it does not have today: a
**structured refusal** instead of a transport exception, an **ask id** it can
hold rather than regex out of prose, and a **retry keyed on that id** rather
than on resubmitting byte-identical text.

- What is wrong and why: `docs/issues.md`, "Gate wiring: one defect, three
  symptoms".
- Which idiom this family gets, and where the family stops:
  `docs/error-chain.md`, "The one shared shape, and where it stops".
- The gate's own doctrine: `docs/gate-and-shell-split.md`.
- What redemption does today: `docs/gate-resume.md`.

Shape A — approval alone is enough to run the work, detached from the
original caller — is the destination. B is a strict subset and forecloses
nothing.

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

1. **The shared types.** `kaijutsu-types::refusal` and the capnp
   declarations. No behavior change.
2. **`McpError` carries a `Refusal`.** Replace `by_hook: HookId` on the three
   gate variants; give the hookless `shell_write` gate a real verdict
   (finding 1). `settled_block_status()` keeps its one mapping.
3. **The wire.** Seven result structs, the server side, the client side,
   `RpcError::Refused` → `CallError::Refused`.
4. **The consumers.** The MCP tool result keeps `is_error: true` and gains
   the id; the ACP path already rides `Status` and is correct.
5. **Redeem by id.** The ledger read, the divergence check, and the deletion
   of the digest set match. `PENDING_REASON` stops saying "run the same
   command again".

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

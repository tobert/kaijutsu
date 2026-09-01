# The error chain

**A verdict is a result; a fault is an error.** A call that reached an answer
succeeded, even when the answer is no. A call that could not reach one failed.
These travel different channels, and confusing them is the defect this
document exists to name.

- **Verdict** — the machinery worked and produced a considered answer:
  gated, pending, denied on the merits, refused by a capability. The caller
  learns something true and must not retry.
- **Fault** — the machinery broke: transport, a dead instance, a serialization
  failure, a mount gone. The caller learns nothing about the request and a
  retry is reasonable.

The cost of collapsing them is not aesthetic. A transport error means *"I
could not tell you what happened"*, so every client and model treats it as
maybe-transient and retries. A gate escalation means the opposite. Probing a
gated surface on 2026-08-30 minted five asks, because each error invited a
retry and each retry with different text minted a new durable row.

## The chain has four layers, and the type survives only three

```text
McpError  ──►  settled_block_status()  ──►  Status on the block   ✓ distinction kept
   │
   └────────►  capnp::Error::failed()  ──►  a string at the client ✗ distinction lost
```

**1. Origin.** `McpError` (`crates/kaijutsu-kernel/src/mcp/error.rs`, 23
variants) already draws the line, and its doc comments say so. `Denied`,
`GatePending`, `GateUnavailable`, `CapabilityDenied`, `FacadeDenied` and
`LoadoutDenied` are verdicts. `Io`, `Protocol`, `InstanceDown`, `Timeout`,
`ConcurrencyCap` and `Cancelled` are faults.

**2. The durable projection is correct.** `settled_block_status()`
(`error.rs:233`) maps `GatePending` to `Status::Waiting` and everything else
to `Status::Error`. The wire enum carries the doctrine in its own comment
(`kaijutsu.capnp`, `Status`): a call that recorded a durable ask and ran
nothing settles `waiting` rather than `error`, *because an unanswered
question is not a refusal*.

**3. The call return collapses it.** `crates/kaijutsu-server/src/rpc.rs:8922`
is the clearest instance, and the comment above it shows the code knows
exactly what it is losing:

```rust
// No "denied" prefix: this arm carries three different verdicts
// (`Denied`, `GateUnavailable`, `GatePending`) and only one of them is a no.
let settled = err.settled_block_status();   // pending → Waiting, not Error
...
return Err(capnp::Error::failed(reason));   // all three become one exception
```

**4. The client receives a string.** `RPC error: Cap'n Proto error: Failed:
remote exception: …`.

**The same error object goes two directions and only one keeps its type.**
Into the block store it arrives as `Waiting`, correctly distinguished. Out to
the caller it arrives as a transport failure. This is not a confused
implementation — it is a careful one with nowhere to put the distinction on
the return path.

## Four idioms for "no" already exist

None was invented for the gate. Each was solved locally, correctly, and
separately, which is the strongest evidence that the concept is real and
that the RPC surface is the one place it was never given a home.

| idiom | where | what it says |
|---|---|---|
| `Status::waiting @5` | block status | not started, not refused — awaiting a decision |
| `denied @9 :Bool` | `SnapshotNode`, `Vfs.snapshot` | this node refused; the walk still succeeded |
| `isError :Bool` | `ToolResult` | the tool ran and reported failure (D-28) |
| `(success :Bool, error :Text)` | 14 methods | ordinary application failure |

`SnapshotNode.denied`'s own comment states the general rule better than the
gate's does: *a cut is the walker's budget, a denial is the filesystem saying
no, rendered as a seam rather than failing the walk.*

## Proportions

| measure | count |
|---|---|
| RPC methods across all interfaces | 152 |
| methods naming any `error`/`denied`/`success` field in their return | 18 |
| `capnp::Error::failed` sites, `rpc.rs` | 201 |
| `capnp::Error::failed` sites, all crates | 231 |
| `McpError` variants | 23 |

**About one method in eight has a second channel.** The other seven have
exactly one way to say anything went wrong, which is to throw. So this is not
a gate bug that leaked: throwing is the *default* way to fail an RPC here, and
the gate is merely where it hurts most, because a retryable-looking error
there mints a new durable ask each time.

## Direction

**Do not add a fifth idiom.** Pick one of the four as the general answer for
"the call succeeded, the answer is no", and give the throw-only methods access
to it.

`ErrorPayload` (`kaijutsu.capnp`) is most of the vocabulary already —
`category` (tool, stream, rpc, render, parse, validation, kernel), `severity`,
a stable machine-readable `code`, and `detail`. It is currently reachable only
on Error *blocks*, never on a call return. Generalizing it is a smaller change
than it looks, because the shape is designed and shipped.

**Do it once, not per method.** Per-method is 152 conversations; a shared
result struct is one conversation and a mechanical sweep.

This reframes the gate work. `docs/issues.md`, "Gate wiring: one defect, three
symptoms" asks for structured `{ask_id, status}` on escalation. Under this
view that is not a gate feature — it is the gate being the first caller of a
general facility. Build the general shape first; a gate-shaped special case
would be the fifth idiom.

## What still needs deciding

- **Which idiom generalizes.** `ErrorPayload` is the candidate; `denied :Bool`
  is the simplest thing that could work for the binary cases.
- **Whether a verdict rides the success path or a typed error.** The
  constraint from `docs/issues.md` is *keep it loud*: `is_error: true`, never
  a success with a status field nobody reads. Trading a wrong channel for a
  silent one is the trade this repo refuses.
- **Flag-day scope.** Wire changes are permitted under the flag-day rule (wire
  only, never storage), but capnp interface ordinals stay sequential —
  retiring a method leaves a `retiredNN @NN ()` stub, not a hole.

## Method inventory

Every one of the 152 methods was read against its implementation, not its
signature. **A** fault-only, **B** needs a verdict channel, **C** already has
one.

| interface | methods | A | B | C |
|---|---|---|---|---|
| `Kernel` | 103 | 66 | 18 | 19 |
| `Vfs` | 17 | 2 | **14** | 1 |
| `World` + `PeerCommands` | 4 | 3 | 1 | 0 |
| event/callback interfaces | 28 | 28 | 0 | 0 |
| **total** | **152** | **99** | **33** | **20** |

**33 methods need a channel; 99 are genuinely fault-only.** The event
interfaces are server-to-client push with void returns — there is no result
slot for a verdict and a failed call can only mean delivery broke.
`ElicitationEvents.onRequest` looks like a counterexample and is not: a human
declining is already carried as ordinary data in
`McpElicitationResponse.action` (`accept`/`decline`/`cancel`).

### The 33 are six families, not 33 conversations

| family | count | what it is |
|---|---|---|
| VFS permission / read-only | 14 | `Vfs` methods, all through one function |
| caller-input validation | 4 | `getBlocks`, `createContext`, `renameContext`, `reportMidiPresence` |
| gate / hook verdicts | 4 | `execute`, `shellExecute`, `executeKj`, `callMcpTool` |
| policy and administrative | 4 | `mount` (frozen), `prompt` (quiesced), `setBlockExcluded` (state), `bindKernel` (wire version) |
| editor writes reaching the VFS | 3 | `editorOpen`, `editorKeys`, `editorSave` — family 1 by another door |
| capability / facade denial | 3 | `editInput`, `submitInput`, `commitCapture` |
| a foreign verdict | 1 | `invokePeer` — the *peer's* handler said no |

**Start with the VFS: 14 of 33 collapse in three lines.** Every `Vfs` method
ends in `.map_err(vfs_err_to_capnp)`, and that function
(`crates/kaijutsu-server/src/rpc.rs`, `fn vfs_err_to_capnp`) is:

```rust
fn vfs_err_to_capnp(e: kaijutsu_kernel::VfsError) -> capnp::Error {
    capnp::Error::failed(format!("{}", e))
}
```

`VfsError` already carries `ReadOnly`, `PermissionDenied` and
`PathEscapesRoot` as distinct variants; this discards the variant and keeps
the `Display` string. A read-only mount refusing a write is the textbook
verdict, and `Vfs.snapshot` — the one `Vfs` method classed C — already models
it correctly with `denied :Bool`. So the fix has a worked example *inside the
same interface*.

### Two findings outside the error question

- **Eight `Kernel` MCP methods have no implementation.** `setMcpRoots`,
  `listMcpPrompts`, `getMcpPrompt`, `subscribeMcpProgress`, `completeMcp`,
  `setMcpLogLevel`, `subscribeMcpLogs` and `cancelMcpRequest` have no override
  in `KernelImpl` and fall through to capnp-rpc's generated default, which
  always returns `unimplemented`. They are class A trivially, but a quarter of
  that slice's MCP surface is dead on the wire.
- **`listPresets` swallows database errors** — `db.list_presets()` and the
  cast read that follows it both end in `unwrap_or_default()`, so a caller
  cannot distinguish a failed read from an empty list. Not a verdict-channel
  gap (nothing is thrown at all) but a silent fallback of the kind this
  repo's stance argues against.

### Sharp edges worth naming

- **`renameContext` has no result struct at all** — it is void-returning
  (`kaijutsu.capnp:1755`) — yet its expected failure is a caller-supplied
  duplicate label. A client cannot tell "you tried to take a taken label"
  from "the database fell over".
- **`unmount` has `success :Bool` and no `error :Text`**, so it can say no
  without saying why. Classed C, but a half-measure.
- **`invokePeer` conflates the same two things one level out**: `PeerError`
  holds `InvocationFailed` (the peer's handler answered no) alongside
  `NotFound`, `Disconnected` and `Timeout` (connectivity faults).
- **`prompt`'s quiesce refusal is administrative**, with a documented remedy
  (`kj system resume`), and it travels the same channel as a socket failure.

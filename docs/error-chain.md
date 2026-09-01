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

**There is no general verdict facility, and we are not building one** (Amy,
2026-09-01). **The unit of work is the family, not the method and not the
surface.** Six families, four changes; the inventory below is the evidence.

A general type was the obvious move and it is wrong here for a reason the
inventory makes concrete: the families do not want the same thing. Forcing
them into one struct means every site translating into a lowest common
denominator and back out, and the information that actually helps a caller
differs per family — an ask you can answer, a capability you could grant, a
path and a mount, a field you got wrong.

| family | n | what it gets | new machinery |
|---|---|---|---|
| VFS permission / read-only | 14 | `errno` on the result | none — a POSIX convention |
| validation + policy | 8 | `(success :Bool, error :Text)` | **none — 14 methods already use it** |
| gate + capability | 7 | one shared refusal shape (below) | one, scoped |
| `invokePeer` | 1 | split `InvocationFailed` out of `PeerError` | none |

**The VFS wants an errno, not our vocabulary.** It is a filesystem, POSIX
settled this, and the SFTP and FUSE consumers on the far end already speak it.
A kaijutsu verdict type would fit worse than the obvious domain answer. Fix
`vfs_err_to_capnp`, which is where all 14 collapse.

**Validation and policy need nothing invented.** `(success, error)` is already
the repo's answer for "no, and here is why", on 14 methods. These 8 are
finishing that, not starting something.

### The one shared shape, and where it stops

Gate and capability share a refusal genuinely: *this principal, in this
context, may not do this right now — and here is the thing to present or
change so it can.* For a gate that is an ask id you answer; for a capability
it is the name you would grant. Splitting them would be inventing twice for
one idea, so they get one shape.

**It stops there.** Before adding a family to it, apply the test: is the
refusal *about the caller's standing*? If the answer is no, it does not
belong, however much the struct would fit.

- A duplicate label is not about standing — the caller may rename, they
  named it badly. Validation.
- A read-only mount is a property of the **mount**, not of who asked.
  Everyone gets the same answer. VFS.
- `invokePeer` carries a **foreign** system's verdict. We are relaying, not
  deciding, and we should not dress another program's error as our own.

Stretching this shape to cover those is how a scoped thing becomes the
general facility we just declined.

### The doctrine generalizes; the mechanism does not

What is worth writing down is which idiom to reach for, so a seventh family
does not invent a fifth:

- A refusal about the caller's standing, carrying something to act on → the
  gate/capability shape.
- A domain with its own settled vocabulary → **use that vocabulary**
  (errno for filesystems), not ours.
- A plain no with a reason → `(success :Bool, error :Text)`.
- A per-item refusal inside a bulk result → a `denied` flag on the item, as
  `SnapshotNode` does; the call still succeeds.

### Constraints on any of it

- **A verdict cannot ride the capnp error channel.** `capnp::ErrorKind` is
  `Failed`, `Overloaded`, `Disconnected`, `Unimplemented` — every variant
  describes a fault and none means "denied". Verdicts ride results. This is
  a property of the transport, not a choice.
- **Keep it loud.** `docs/issues.md` sets the constraint: never a success
  with a status field nobody reads. Trading a wrong channel for a silent one
  is the trade this repo refuses.
- **Flag-day scope.** Wire changes are permitted (wire only, never storage),
  but capnp interface ordinals stay sequential — retiring a method leaves a
  `retiredNN @NN ()` stub, not a hole.

### What this means for the gate lane

`docs/issues.md`, "Gate wiring: one defect, three symptoms" asks for
structured `{ask_id, status}` on escalation. That is the gate+capability
shape, and it is **independently shippable** — it waits on no general design.
Build it for the 7 methods in its family and stop at the boundary above.

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

**Since measured, the VFS row has been resolved** (see below): 11 of its 14
Bs were retired unused, `read` was fixed, and `write`/`create` remain. The
open count is **21**, not 33 — and the largest remaining family is the 7 the
gate lane already owns.

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

### The VFS family — done, and smaller than the count suggested

**Counting methods that *can* produce a verdict is not the same as counting
work.** Of 17 `Vfs` wire methods, **four had a caller**: `read` and `snapshot`
in production, `create` and `write` for one e2e test. The other 13 had none —
general filesystem access over this interface was superseded by SFTP, which is
how every remote consumer reaches the VFS now.

So the 14 became: **13 retired** to `retiredNN @NN ()` stubs, and **one fixed**.
`Vfs.read` now returns `error :VfsErrorKind` beside its data, and `snapshot`
already modelled it with `denied :Bool`. `write` and `create` still throw;
they are test-only and can follow whenever.

**A raw errno could not cross this wire.** The doc above says "errno", and the
domain vocabulary is right, but the *encoding* cannot be a platform number:
`ENOTEMPTY` is 39 on Linux and 66 on macOS, and a macOS client talks to a
Linux kernel. `VfsErrorKind` carries POSIX semantics with a stable wire
encoding, and each side maps to its own numbers.

### The chain had a fifth layer nobody had named

Fixing capnp was not enough. The client's own actor repeated the same collapse
one hop later: `CallError::Rpc(String)` flattened the freshly-typed
`RpcError::Vfs` back into prose, immediately after we had rescued it. The full
path is five layers, not four:

```text
VfsError → capnp → RpcError → CallError → the app
```

**Every boundary that stringifies is a place the type dies**, and a fix that
stops at the wire buys nothing. `CallError` now carries `Vfs { kind, path }`
too.

The receipt for why this matters was already in the tree, written by its own
victim. `roster.rs` had:

```rust
/// Matched on the error text because that is all the wire carries —
/// `VfsError`'s variants collapse to a message string at the capnp boundary.
fn reads_as_absent(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    d.contains("not found") || d.contains("no mount point")
}
```

A client lowercasing kernel prose to recover a distinction the kernel had as
a typed enum and threw away — with a test pinning the exact wording, including
the `"RPC error: remote exception: "` prefix. It is now a match on
`VfsErrorKind::is_absent()`.

**There is a second string-matching classifier still in there.**
`is_disconnect_error(msg)` decides whether to tear down the connection by
searching the error text for `"Disconnected"`. Same defect, different
consequence, not yet fixed.

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

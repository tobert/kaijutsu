# The client

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-client` — the
RPC client library all apps/CLIs use to reach the kernel. Code is truth: every
pointer below names a symbol — `grep` it.*

`kaijutsu-client` provides four things: an SSH transport (one channel bound to
the `kaijutsu-rpc` subsystem, `docs/architecture/README.md#process--transport-model`),
a typed Cap'n Proto facade over `World`/`Kernel`, a `Send+Sync` `ActorHandle`
that bridges multithreaded callers into the `!Send` Cap'n Proto `LocalSet`, and
per-context mirrors that apply server-pushed events incrementally.

---

## The actor bridge

### `ActorHandle` (`src/actor.rs:897`)

`Clone + Send + Sync`. Holds an `mpsc::Sender<ChannelCmd>` (the only path in),
plus `broadcast` senders for `ServerEvent` and `ConnectionStatus`. Every public
method builds a `oneshot` reply channel, sends a `ChannelCmd` (command + caller
tracing span) over the bounded mpsc (cap 32), and awaits the reply. The mpsc is
the only shared mutable state, so the handle is trivially `Send+Sync` while the
real Cap'n Proto work runs inside `RpcActor` on a `LocalSet`. `spawn_actor`
(`:4112`) wires the channels, builds the actor, and `spawn_local`s `actor.run()`.

### `RpcActor` (internal, `!Send`, `src/actor.rs:2151`)

Runs the connection FSM: `Idle → Connecting → Connected → Closing → Cooldown →
(retry) | Terminal`. Owns the live `RpcClient` + `KernelHandle`, the ping task,
and the handshake task. Cap'n Proto calls dispatch as `spawn_local` children so
the main loop stays reactive; a `biased` select prioritizes close over command
intake.

### `RpcClient` / `SshClient`

`RpcClient` (`src/rpc.rs:200`, `!Send`) wraps a `world::Client` bootstrapped from
the `RpcSystem`. An `RpcSystemGuard` (`Rc<AbortHandle>`) aborts the
`spawn_local(rpc_system)` task on last-drop, closing the SSH stream so the server
detects disconnect. `bind_kernel` (`:295`) returns a `(KernelHandle, KernelId)`;
`KernelHandle` (`:675`) exposes the per-context RPC methods. `SshClient`
(`src/ssh.rs:190`) wraps `russh`, opens one session channel bound to the
`kaijutsu-rpc` subsystem (`SSH_RPC_SUBSYSTEM`; SFTP and client-share sessions
use their own named subsystems over separate connections), supports
agent/file/in-memory keys, and does TOFU host-key checking via `known_hosts`
(mismatch is a non-retryable error).

---

## Client-side mirror

`ContextMirror` (`src/context_feed.rs`) is the client's view of one context. It
is fed by `ActorHandle::subscribe_context` plus `get_blocks_versioned`, and
`document_store.rs` holds one per context. The wire contract is
`docs/change-feed.md`.

### Compose input

The compose draft is an ordinary block — `Role::User`, `Status::Draft`,
`ephemeral`, one per `(context, principal)`. It is sent via
`editInput`/`submitInput` and read back like any other block off the per-context
change feed: it lands in `mirror.blocks()` and is exposed as
[`DocumentEntry::draft_text`] (`src/document_store.rs:80`). There is no separate
document type for it.

### `subscriptions.rs`

Defines `ServerEvent` (`:52`, the typed enum of all server-push callbacks) and
`ConnectionStatus` (`:266`). `BlockEventsForwarder` (`:302`) implements the
`block_events::Server` capnp trait, deserializes each callback, and emits onto the
`broadcast` channel. `ResourceEventsForwarder` (`:1462`) does the same for MCP
resource events.

---

## Data flow

**Outbound:** caller (any thread) → `ActorHandle` method → bounded mpsc → actor
loop → `spawn_local(run_rpc_call(...))` → `KernelHandle` method → capnp
`request.send()` → the one `kaijutsu-rpc` subsystem channel → server → reply
via oneshot. Per-call timeout 30 s (`RPC_CALL_TIMEOUT`); disconnect-class
errors trigger the `Closing` transition.

**Inbound:** server pushes an event over the same channel → capnp callback (in
the `LocalSet`) → `BlockEventsForwarder` → `broadcast` (cap 256) → consumer →
`ContextMirror` updated → consumer reads `mirror.blocks()`. The wire contract is
`docs/change-feed.md`.

**Handshake** (`connect_handshake`, `:3205`): every phase budget below is a
field of `kaijutsu_types::timeout::TransportTimeouts` (`DEFAULT`), not a bare
constant — SSH dial+auth (5 s) → `bind_kernel` (5 s) → `join_context` if set
(5 s) → `attach_peer` if remembered (best-effort, non-fatal — this is also
where a peer registration re-attaches after a kernel restart, see Smells) →
`subscribe_blocks_filtered` + `subscribe_mcp_resources` in parallel
(5 s). Total budget 25 s.

---

## Smells (not fixed — see [issues](../issues.md))

- **Peer re-attach residual gap** — `connect_handshake` (`actor.rs:3288`)
  re-sends `attach_peer` on every reconnect once a registration is remembered,
  but the *initial* registration isn't remembered until the app's first
  successful `attach_peer`; a kernel restart before that leaves the peer
  un-reattached until the caller retries one explicitly.
- **String-matched disconnect classification** — `is_disconnect_error`
  (`actor.rs:2050`) matches on the capnp error `Display` text; fragile if capnp
  changes formatting (would stop triggering reconnect).

`ContextMirror` tracks a per-context `version` and refuses — rather than
silently buffering or guessing at — a delivery that doesn't advance it: a
straggler the snapshot already covers is discarded as a no-op, and an
inversion above the snapshot is a hard error, never a dropped change
(`context_feed.rs`, test `an_inversion_above_the_snapshot_is_still_refused`).
`finish_closing` (`actor.rs:2532`) captures its attempt count from the
`mem::replace` result before `self.state` becomes the `Idle` placeholder, so
backoff carries correctly from `Closing` through `Cooldown` rather than
resetting to 1 s on every reconnect attempt.

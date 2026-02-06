# Phase 2: Replace ConnectionBridge with ActorPlugin

**Goal:** Delete `ConnectionBridge` (1,557 lines) and replace it with a thin Bevy plugin that wraps `ActorHandle` — the app's sole RPC interface.

## Key Files

| File | Action |
|------|--------|
| `crates/kaijutsu-app/src/connection/bridge.rs` | **Delete** |
| `crates/kaijutsu-app/src/connection/actor_plugin.rs` | **New** (~300 lines) |
| `crates/kaijutsu-app/src/connection/mod.rs` | Update exports |
| `crates/kaijutsu-app/src/cell/systems.rs` | Migrate from ConnectionEvent to ServerEvent |
| `crates/kaijutsu-app/src/dashboard/mod.rs` | Migrate command sends to ActorHandle |
| `crates/kaijutsu-app/src/dashboard/seat_selector.rs` | Migrate command sends |
| `crates/kaijutsu-app/src/ui/timeline/systems.rs` | Migrate fork/cherry-pick |
| `crates/kaijutsu-app/src/ui/constellation/mod.rs` | Migrate event reads |
| `crates/kaijutsu-app/src/ui/constellation/create_dialog.rs` | Migrate command sends |
| `crates/kaijutsu-app/src/ui/widget/mod.rs` | Migrate connection status |

## Architecture Decisions

### ActorPlugin Design

```rust
// connection/actor_plugin.rs

/// Bevy resource wrapping the ActorHandle
#[derive(Resource)]
pub struct RpcActor {
    pub handle: ActorHandle,
}

/// Bevy resource for connection state (replaces ConnectionState)
#[derive(Resource, Default)]
pub struct RpcConnectionState {
    pub connected: bool,
    pub identity: Option<Identity>,
    pub current_kernel: Option<KernelInfo>,
    pub current_seat: Option<SeatInfo>,
}

/// Bevy message for server-pushed events (bridge from broadcast → Bevy ECS)
#[derive(Message)]
pub struct ServerEventMessage(pub ServerEvent);

/// Bevy message for connection status changes
#[derive(Message)]
pub struct ConnectionStatusMessage(pub ConnectionStatus);

pub struct ActorPlugin;

impl Plugin for ActorPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ServerEventMessage>()
           .add_message::<ConnectionStatusMessage>()
           .init_resource::<RpcConnectionState>()
           .add_systems(Update, (
               poll_server_events,
               poll_connection_status,
               update_connection_state,
           ));
    }
}
```

**Event bridging:** A Bevy system polls the `broadcast::Receiver` each frame and writes Bevy messages. This is the only place that touches tokio from the Bevy side.

```rust
fn poll_server_events(
    actor: Option<Res<RpcActor>>,
    mut events: MessageWriter<ServerEventMessage>,
) {
    // Store receiver as a Local<> or in the resource
    // Try recv in a loop (non-blocking) until Empty
    // On Lagged: emit DocumentsStale so cell/systems.rs triggers resync
    loop {
        match receiver.try_recv() {
            Ok(event) => events.write(ServerEventMessage(event)),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Lagged(n)) => {
                log::warn!("ServerEvent broadcast lagged by {n}, bumping generation");
                generation.0 += 1; // SyncGeneration resource
                break;
            }
            Err(TryRecvError::Closed) => break,
        }
    }
}
```

**Lag recovery:** On `Lagged`, the system bumps the `SyncGeneration` counter (see Phase 1).
`cell/systems.rs` detects the generation mismatch on the active document and triggers
`get_document_state()` to resync. CRDT ops cannot be skipped.

**Async command dispatch:** Consumer systems that need to call ActorHandle methods use `IoTaskPool::get().spawn()`.

Two patterns depending on whether other systems need to observe the result:

**Pattern A: Fire-and-forget** (results arrive via block events)
```rust
fn submit_prompt(actor: Res<RpcActor>, /* ... */) {
    let handle = actor.handle.clone();
    IoTaskPool::get().spawn(async move {
        if let Err(e) = handle.prompt(&content, None, &cell_id).await {
            log::error!("Prompt failed: {e}");
        }
        // Results come via ServerEvent::BlockInserted, not the return value
    }).detach();
}
```

**Pattern B: RpcResultMessage** (state-changing ops other systems observe)
```rust
/// Bevy message for RPC results that multiple systems need to observe
#[derive(Message, Clone)]
pub enum RpcResultMessage {
    KernelAttached(KernelInfo),
    ContextJoined { seat: SeatInfo, context_name: String },
    ContextLeft,
    Forked { context: Context, source_document: String },
    CherryPicked { new_block_id: BlockId },
    RpcError { operation: String, error: String },
}

fn attach_kernel(
    actor: Res<RpcActor>,
    mut results: MessageWriter<RpcResultMessage>,
    // ...
) {
    let handle = actor.handle.clone();
    let tx = results.clone_sender(); // Bevy 0.18 MessageWriter supports this
    IoTaskPool::get().spawn(async move {
        match handle.attach_kernel(&id).await {
            Ok(info) => tx.write(RpcResultMessage::KernelAttached(info)),
            Err(e) => tx.write(RpcResultMessage::RpcError {
                operation: "attach_kernel".into(),
                error: e.to_string(),
            }),
        }
    }).detach();
}
```

**Which pattern for which operations:**

| Operation | Pattern | Why |
|-----------|---------|-----|
| `prompt`, `shell_execute` | Fire-and-forget | Results arrive as block events |
| `call_mcp_tool` | Fire-and-forget | Result arrives as `ServerEvent::McpToolResult` |
| `attach_kernel` | RpcResultMessage | Dashboard, constellation, widgets all need to know |
| `join_context` | RpcResultMessage | Constellation, tabs, cell systems all react |
| `leave_seat` | RpcResultMessage | Tabs, constellation need to update |
| `fork_from_version` | RpcResultMessage | Constellation adds node, user auto-joins |
| `cherry_pick_block` | RpcResultMessage | Timeline UI needs confirmation |
| `list_kernels`, `list_contexts` | Direct (non-detached) | Caller awaits result in dashboard |

This eliminates the silent-failure problem: state-changing operations broadcast their
results as Bevy messages, so any system can observe success or failure. Errors surface
in the UI (widget flash, status bar message) rather than silently logging.

### Command → ActorHandle Method Mapping

| ConnectionCommand | ActorHandle Method | Notes |
|---|---|---|
| `ConnectSsh { config }` | `spawn_actor(config, ...)` | Creates the resource, not a method call |
| `Disconnect` | Drop the `RpcActor` resource | Actor shuts down on drop |
| `Whoami` | `handle.whoami()` | World-level (Phase 1) |
| `ListKernels` | `handle.list_kernels()` | World-level (Phase 1) |
| `AttachKernel { id }` | `handle.attach_kernel(&id)` | World-level, sets kernel cap |
| `CreateKernel { config }` | `handle.create_kernel(config)` | World-level |
| `DetachKernel` | `handle.detach()` | Releases kernel cap |
| `ListContexts` | `handle.list_contexts()` | |
| `ListMySeats` | `handle.list_my_seats()` | World-level |
| `JoinContext { context, instance }` | `handle.join_context(&ctx, &inst)` | |
| `LeaveSeat` | `handle.leave_seat()` | |
| `TakeSeat { ... }` | `handle.take_seat(...)` | World-level |
| `Prompt { content, model, cell_id }` | `handle.prompt(&content, model, &cell_id)` | |
| `ShellExecute { command, cell_id }` | `handle.shell_execute(&command, &cell_id)` | |
| `CallMcpTool { server, tool, args }` | `handle.call_mcp_tool(&srv, &tool, &args)` | |
| `ForkDocument { ... }` | `handle.fork_from_version(...)` | |
| `CherryPickBlock { ... }` | `handle.cherry_pick_block(...)` | |

### Event → ServerEvent/ConnectionStatus Mapping

| ConnectionEvent | Replacement | Source |
|---|---|---|
| `Connected` | `ConnectionStatus::Connected` | status broadcast |
| `Disconnected` | `ConnectionStatus::Disconnected` | status broadcast |
| `ConnectionFailed(msg)` | `ConnectionStatus::Error(msg)` | status broadcast |
| `Reconnecting { attempt, delay }` | `ConnectionStatus::Reconnecting { attempt }` | status broadcast |
| `Identity(id)` | Stored on connect in `RpcConnectionState` | Direct from whoami() |
| `KernelList(list)` | Direct return from `list_kernels()` | async result |
| `AttachedKernel(info)` | `RpcResultMessage::KernelAttached` | async → message |
| `DetachedKernel` | State update on `detach()` completion | async result |
| `Error(msg)` | `ActorError` from method calls | async result |
| `ContextsList(list)` | Direct return from `list_contexts()` | async result |
| `MySeatsList(list)` | Direct return from `list_my_seats()` | async result |
| `SeatTaken { seat }` | `RpcResultMessage::ContextJoined` | async → message |
| `SeatLeft` | `RpcResultMessage::ContextLeft` | async → message |
| `BlockCellInitialState { ... }` | `ServerEvent::BlockInserted` (initial batch) | event broadcast |
| `PromptSent { prompt_id, cell_id }` | Direct return from `prompt()` | async result |
| `BlockInserted { ... }` | `ServerEvent::BlockInserted` | event broadcast |
| `BlockTextOps { ... }` | `ServerEvent::BlockTextOps` | event broadcast |
| `BlockStatusChanged { ... }` | `ServerEvent::BlockStatusChanged` | event broadcast |
| `BlockDeleted { ... }` | `ServerEvent::BlockDeleted` | event broadcast |
| `BlockCollapsedChanged { ... }` | `ServerEvent::BlockCollapsedChanged` | event broadcast |
| `BlockMoved { ... }` | `ServerEvent::BlockMoved` | event broadcast |
| `McpToolResult { ... }` | `ServerEvent::McpToolResult` | event broadcast |
| `ResourceUpdated { ... }` | `ServerEvent::ResourceUpdated` | event broadcast |
| `ResourceListChanged { ... }` | `ServerEvent::ResourceListChanged` | event broadcast |
| `ForkComplete { ... }` | `RpcResultMessage::Forked` | async → message |
| `CherryPickComplete { ... }` | `RpcResultMessage::CherryPicked` | async → message |

**Key insight:** Half the ConnectionEvent variants exist only because the bridge is fire-and-forget. With async methods, *some* become direct return values (list operations) while *state-changing* operations emit `RpcResultMessage` so multiple systems can observe the result. Only server-pushed subscription events need the broadcast pattern.

## Implementation Steps

### Step 1: Write ActorPlugin
- Create `connection/actor_plugin.rs`
- Define `RpcActor` resource, `RpcConnectionState`, `SyncGeneration`, Bevy messages
- Define `RpcResultMessage` enum for state-changing operation results
- Implement `poll_server_events` (with generation bump on lag) and `poll_connection_status` systems
- Register in `connection/mod.rs`

### Step 2: Migrate dashboard (easiest consumer)
- `dashboard/mod.rs`: Replace `ConnectionCommands.send()` with `IoTaskPool` async calls to `ActorHandle`
- Replace `ConnectionEvent` reads with `ServerEventMessage` reads + async results
- `dashboard/seat_selector.rs`: Same pattern
- This is the best test — dashboard exercises connect, list, attach, join

### Step 3: Migrate cell/systems.rs (most complex consumer)
- Replace `handle_block_events()` to read `ServerEventMessage` instead of `ConnectionEvent`
- Replace prompt/shell submission to use `ActorHandle` async
- The block event processing logic stays identical — only the event source changes

### Step 4: Migrate remaining consumers
- `ui/timeline/systems.rs`: fork/cherry-pick → async ActorHandle calls
- `ui/constellation/mod.rs`: SeatTaken/SeatLeft → from async join results or broadcast
- `ui/constellation/create_dialog.rs`: JoinContext → async
- `ui/widget/mod.rs`: connection status → `ConnectionStatusMessage`

### Step 5: Delete bridge.rs
- Remove `ConnectionBridgePlugin` from app
- Add `ActorPlugin` to app
- Delete `connection/bridge.rs`
- Clean up `connection/mod.rs` exports
- Verify no remaining references to old types

### Step 6: Auto-reconnect and resync
- `RpcActor` handles reconnection internally (Phase 1 adds subscription re-registration)
- `ActorPlugin` forwards `ConnectionStatus::Reconnecting` / `Connected` to UI
- On reconnect, `RpcActor` bumps `SyncGeneration` (Phase 1) — no separate event needed
- `cell/systems.rs` detects stale generation on active document and resyncs
- Much simpler than bridge's manual reconnect logic — verify that Phase 1's
  post-connect hook re-subscribes and bumps generation

## What Gets Deleted

| What | Lines | Why |
|------|-------|-----|
| `ConnectionCommand` enum | ~70 | Replaced by direct ActorHandle method calls |
| `ConnectionEvent` enum | ~130 | Split into ServerEvent broadcast + async returns |
| `ConnectionCommands` resource | ~15 | Replaced by `RpcActor` resource |
| `ConnectionEvents` resource | ~15 | Replaced by `ServerEventMessage` |
| `ConnectionState` resource | ~40 | Replaced by `RpcConnectionState` |
| `ConnectionBridgePlugin` | ~100 | Replaced by `ActorPlugin` |
| `spawn_connection_thread()` | ~200 | Replaced by `spawn_actor()` from kaijutsu-client |
| `run_connection_loop()` | ~400 | Lives in RpcActor now |
| `BlockEventsCallback` | ~150 | Moved to kaijutsu-client subscriptions |
| `ResourceEventsCallback` | ~80 | Moved to kaijutsu-client subscriptions |
| Reconnect logic | ~100 | Built into RpcActor |
| **Total** | **~1,300** | Net savings after adding ~300-line ActorPlugin |

## Verification

- [ ] `cargo check -p kaijutsu-app` passes
- [ ] App connects to server and shows dashboard
- [ ] Kernel attach + context join works
- [ ] Prompt submission and block rendering works
- [ ] Shell execution works
- [ ] Connection loss → auto-reconnect works
- [ ] After reconnect, block subscriptions are live (new blocks arrive)
- [ ] After reconnect or broadcast lag, SyncGeneration bumps and active doc resyncs
- [ ] RpcResultMessage emitted for attach/join/leave/fork/cherry-pick
- [ ] Multiple systems (dashboard, constellation) observe RpcResultMessage correctly
- [ ] RpcError surfaces in UI (widget flash or status message)
- [ ] Constellation updates on seat taken/left
- [ ] Timeline fork/cherry-pick works
- [ ] No references to `ConnectionCommand`, `ConnectionEvent`, or `ConnectionBridge` remain

## Dependencies

- **Phase 1** must be complete (ActorHandle has all Tier 1 methods + subscriptions)

## Status Log

| Date | Status | Notes |
|------|--------|-------|
| | | |

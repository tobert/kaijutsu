# The server

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-server` — SSH
transport, the Cap'n Proto RPC surface, LLM streaming, the beat scheduler, auth.
Code is truth; verified 2026-06-16.*

`kaijutsu-server` is the only process that holds a live `Kernel`. It authenticates
SSH clients, multiplexes many RPC sessions onto the one shared kernel, streams LLM
tokens into CRDT blocks, drives the musician beat loop, and persists SSH identity.

---

## Transport — SSH + Cap'n Proto (`src/ssh.rs`)

Startup (`SshServer::run_on_listener`, `:223`): load/generate the Ed25519 host
key, open `AuthDb`, build a `russh` config with 30 s keepalive × 3 (≈90 s
dead-peer window), call `create_shared_kernel`, spawn the **turn-driver** and
**beat-scheduler** threads, then run the russh server.

Per connection (`ConnectionHandler`, `:353`): `auth_publickey` (`:753`) looks up
the fingerprint in `AuthDb` via `spawn_blocking`; in anonymous mode unknown keys
auto-register. On the **second** channel (index 1, the RPC channel) it spawns a
named OS thread.

RPC thread model (`:711`): each session runs on a dedicated OS thread with a
`current_thread` Tokio runtime + `LocalSet` — required because `capnp-rpc`
capabilities are `!Send`. `catch_unwind` contains per-connection panics. `run_rpc`
(`:421`) wraps the channel in an `ActivityStream` (stamps `last_activity` on every
byte), builds `ConnectionState`, registers `WorldImpl` as the bootstrap
capability, and runs the `RpcSystem` over a `twoparty::VatNetwork`. A watchdog
(`:528`) warns only when idle past 120 s (above the keepalive reap window).
Connection count is capped (default 100).

---

## RPC surface (`src/rpc.rs`)

~7,400 lines, ~162 functions. A two-level capability tree:

- **`WorldImpl`** (`:1407`) — `world::Server`: `whoami`, `list_kernels`,
  `bind_kernel`. `bind_kernel` returns a `KernelImpl` capability; there is one
  shared kernel, not one per user.
- **`KernelImpl`** (`:1473`) — `kernel::Server`: the monolith, ~80 methods.
- **`VfsImpl`** (`:6887`) — `vfs::Server`: 17 filesystem methods.

`KernelImpl` methods group by domain (see the report for the full table): lifecycle
(`get_info`, `ping`), shell exec (`execute`, `interrupt`, `complete`,
`subscribe_output`), VFS, tools (`execute_tool`, `get_tool_schemas`), **block
CRDT** (`subscribe_blocks[_filtered]`, `push_ops`, `get_blocks`, `move_block`,
`set_block_excluded`, `cherry_pick_block`), **LLM** (`prompt`, `configure_llm`,
`drift_queue`/`cancel`), **context ops** (`get_context_state`/`sync`,
`create`/`join`/`leave`/`conclude`/`compact`/`interrupt_context`), MCP, peers,
kaish (`shell_execute`, cwd/vars), **KV** (`kv_get`/`set`/`delete`/`keys`/`watch`),
**input doc** (`edit_input`/`submit_input`/`clear_input`), semantic index, config,
and dead letters.

**The facade gate:** humans (app) and agents (MCP) reach capabilities through the
same `KernelImpl`. The guard is `broker().check_facade(&context_id, "shell")` —
keyed on the **context binding**, not on which client called (`:3004`). The
deny-by-default allow-set is evaluated inside the broker.

The monolith is **deliberate**: a capnp `impl kernel::Server` must be one `impl`
block (file doc at `:9`). `// ===` banners are the navigation aid. (Splitting it is
tracked in [issues](../issues.md).)

`create_shared_kernel` (`:974`) is the whole-stack constructor: FlowBus → KernelDb
→ Kernel → mounts (RO `/`, RW `~/src`,`/tmp`,`/etc/rc`, then freeze) → block store
→ config backend → LLM registry → optional ONNX semantic index → `KjDispatcher` →
context recovery from KernelDb.

---

## LLM streaming (`src/llm_stream.rs`)

`spawn_llm_for_prompt` (`:184`): resolve provider/model (explicit param >
per-context > kernel default), trigger auto-compaction, build tool defs via the
broker, assemble the system prompt (static base + rc sections + situational
addendum), create a fresh `ContextInterruptState`, and `spawn_local`
`process_llm_stream`.

`process_llm_stream` (`:575`) is the agentic loop: acquire the per-context
conversation lock, read hydration policy (full vs windowed), hydrate the mailbox
(`catch_up` or `rehydrate_windowed`), resolve image blocks from CAS, then loop
(consent-capped: 50 collaborative / 100 autonomous iterations). Each iteration
builds `BuildOpts` with cache breakpoints, calls `provider.stream` with
exponential backoff, and processes `StreamEvent`s under a two-layer timeout
(per-chunk idle + total wall-clock). Tokens write directly to the CRDT block
store; clients observe via `BlockFlow`. Tool calls run concurrently via
`dispatch_tool_via_broker_with_cancel` (120 s per-tool). On completion it
publishes `TurnFlow::Completed { output_block_id }` for autonomous turns.

---

## Beat scheduler (`src/beat.rs`)

`BeatScheduler` (`:261`) is a server-lifetime task on its own OS thread, driving
musician contexts' hyoushigi timelines. A min-heap of `(Instant, ContextId)`, a
`BeatCommand` ingress channel, and a `TurnFlow::Completed` subscription. Commands:
Arm, Play, Pause, Stop, SetTempo, SetOoda, SetRotate, Disarm. `STEP = TickDelta(1)`
— the playhead is **event-counted, never wall-clock-scaled** (freeze = pause,
resume = +1, no rewind). Each wake: `fire_due → process_one` advances the
playhead, materializes committed cells (ABC→MIDI), and drains failures to error
blocks. The OODA boundary fires the `tick` rc verb then publishes
`TurnFlow::Requested`. On `TurnFlow::Completed`, `on_turn_completed` crystallizes
the output as an ABC cell one phrase ahead — guarded by three checks (ephemeral,
track-bearing, `beat()`-authored). Poison cells get a 3-failure retry budget.

---

## Interrupt + auth

`ContextInterruptState` (`src/interrupt.rs:26`): per-context `stop_after_turn`
(soft, checked before each iteration), `cancel: CancellationToken` (hard, selects
against the stream loop), and a `generation` counter so stream-A cleanup can't
clobber stream-B. Created fresh per prompt.

`AuthDb` (`src/auth_db.rs`): SQLite (`auth.db`) with `principals` (UUIDv7 id,
unique username) and `credentials` (SSH fingerprint → principal, CASCADE).
`authenticate` (`:94`) is a single join on the hot path via `spawn_blocking`.
Authorization is binary (key in DB = allowed); anonymous mode auto-registers with
a sanitized username. Identity flows into every CRDT block insert as the author.
Management CLI in `main.rs`: add-key, remove-user, list-users/keys, import,
set-nick.

---

## Smells (not fixed — see [issues](../issues.md))

- **`rpc.rs` monolith** (~7,400 lines) — known/documented; navigation is
  grep/LSP-dependent.
- **External MCP offline** — `list_mcp_servers` returns empty; admin deferred to
  Phase 2. Clients silently get nothing.
- **No graceful SIGTERM** — the WAL checkpoint only fires on clean `Arc` drop; a
  `systemd stop` leaves the WAL for next open.
- **`unwrap()` on workspace insert** in `create_shared_kernel` (`:1092`) panics
  rather than `?`-propagating, unlike its neighbors.
- **Implicit channel-index convention** — only channel 1 gets an RPC thread; the
  "open exactly 3 channels in order" contract is a comment, not a constant.
- **`AuthDb` behind one mutex** — concurrent auth attempts serialize; no pooling.
- **Tool-result visibility gap** — when `insert_tool_result_as` fails the model
  still gets the result but the user never sees the block (`llm_stream.rs:1339`).
- **`$HEARD` ergonomics** — pushed as a JSON string (not a kaish array), window
  hardcoded to 8 phrases (Chameleon batch 2).

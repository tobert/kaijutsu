# The server

*Deep-dive companion to [README.md](README.md). Covers `kaijutsu-server` — SSH
transport, the Cap'n Proto RPC surface, LLM streaming, the beat scheduler, auth.
Code is truth: every pointer below names a symbol — `grep` it.*

`kaijutsu-server` is the only process that holds a live `Kernel`. It authenticates
SSH clients, multiplexes many RPC sessions onto the one shared kernel, streams LLM
tokens into blocks, drives the musician beat loop, and persists SSH identity.

---

## Transport — SSH + Cap'n Proto (`src/ssh.rs`)

Startup (`SshServer::run_on_listener`, `:281`): load/generate the Ed25519 host
key, open `AuthDb`, build a `russh` config with 30 s keepalive × 3 (≈90 s
dead-peer window), call `create_shared_kernel`, spawn the **turn-driver** and
**beat-scheduler** threads, then run the russh server.

Per connection (`ConnectionHandler`, `:480`): `channel_open_session` (`:839`)
stashes every channel a client opens in a per-connection map; `subsystem_request`
(`:892`) dispatches by the requested subsystem **name**, not by channel ordinal
— `"kaijutsu-rpc"` (`SSH_RPC_SUBSYSTEM`) spawns the RPC thread, `"kaijutsu-sftp"`
and `"kaijutsu-share"` spawn theirs, and an unrecognized name gets
`channel_failure` + close. An earlier scheme opened three channels
(control/rpc/events) in a fixed order and keyed the handler by ordinal alone;
control and events carried no traffic of their own; the client now opens one
channel and names what it wants. `auth_publickey` (`:1009`) looks up the
fingerprint in `AuthDb` via `spawn_blocking`; in anonymous mode unknown keys
auto-register.

RPC thread model (`run_rpc`, `:622`): each session runs on a dedicated OS thread
with a `current_thread` Tokio runtime + `LocalSet` — required because
`capnp-rpc` capabilities are `!Send`. `catch_unwind` contains per-connection
panics. `run_rpc` wraps the channel in an `ActivityStream` (stamps
`last_activity` on every byte), builds `ConnectionState`, registers `WorldImpl`
as the bootstrap capability, and runs the `RpcSystem` over a
`twoparty::VatNetwork`. A watchdog (`run_rpc_watchdog`, `:747`) warns only when
idle past `RPC_IDLE_WARN_THRESHOLD` = 120 s (above the keepalive reap window).
Connection count is capped (default 100).

---

## RPC surface (`src/rpc.rs`)

~13,000 lines. A two-level capability tree:

- **`WorldImpl`** (`:3150`) — `world::Server`: `whoami`, `list_kernels`,
  `bind_kernel`. `bind_kernel` returns a `KernelImpl` capability; there is one
  shared kernel, not one per user.
- **`KernelImpl`** (`:3242`, trait impl `:3559`–`:12049`) — `kernel::Server`: the
  monolith.
- **`VfsImpl`** (`:12050`, trait impl `:12179`) — `vfs::Server`: file operations
  over the mounted VFS; the family shrank from seventeen wire methods to four
  with real callers once SFTP superseded the rest (`docs/devlog.md`, "The
  answer that travelled as an error").

`KernelImpl` methods group by domain: lifecycle (`get_info`, `ping`), shell exec
(`execute`, `interrupt`, `complete`, `subscribe_output`), VFS, tools
(`execute_tool`, `get_tool_schemas`), **blocks** (`subscribe_context`,
`subscribe_blocks[_filtered]`, `get_blocks`, `move_block`, `set_block_excluded`,
`cherry_pick_block`), **LLM** (`prompt`, `configure_llm`, `drift_queue`/
`drift_cancel`), **context ops** (`create_context`/`join_context`/
`rename_context`/`promote_context`/`demote_context`/`archive_context`/
`compact_context`/`interrupt_context` — there is no `fork`/`drift` RPC method;
those stay `kj` verbs per the "administration is a `kj` verb, chatty paths stay
on RPC" rule), MCP, peers, kaish (`shell_execute`, cwd/vars), **per-client view
state** (`set_last_context`/`get_client_view`), **input doc**
(`edit_input`/`submit_input`/`clear_input`), semantic index, config, and dead
letters.

**The facade gate:** humans (app) and agents (MCP) reach capabilities through the
same `KernelImpl`. The guard is `Broker::check_facade(&context_id, "shell")`
(`kaijutsu-kernel/src/mcp/broker.rs:1441`) — keyed on the **context binding**,
not on which client called. The deny-by-default allow-set is evaluated inside
the broker.

The monolith is **deliberate**, not unsplit debt: a capnp `impl kernel::Server`
must be contiguous, so a mechanical split would produce delegating trait
methods in a thin file plus per-subject inherent `impl` blocks elsewhere —
more surface area, no real modularity (file doc, `:9`). `// ===` banners are
the navigation aid.

`create_shared_kernel` (`:2428`) is the whole-stack constructor: FlowBus →
KernelDb → Kernel → mounts (RO `/`, an opaque `/dev`, RW `~/src`/`/tmp`, the
`/config` trees, the ephemeral `/run/midi`+`/run/audio`+`/run/roster` views,
`/v/cas`, `/r`, then freeze) → block store → config backend → LLM registry →
optional lfm2d semantic index → `KjDispatcher` → context recovery from
KernelDb.

---

## LLM streaming (`src/llm_stream.rs`)

`spawn_llm_for_prompt` (`:274`): resolve provider/model (explicit param >
per-context > kernel default), build tool defs via the broker, assemble the
system prompt (static base + rc sections + situational addendum), create a
fresh `ContextInterruptState`, and `spawn_local` `process_llm_stream`. There is
no automatic compaction — a block-count-triggered summarize-and-mark-compacted
pass would silently melt history; instead `process_llm_stream` records usage
from every `StreamEvent::Done` into a token-usage gauge the user reads
(`kj context info`).

`process_llm_stream` (`:1360`) is the agentic loop: acquire the per-context
conversation lock, read hydration policy (full vs windowed), hydrate the mailbox
(`catch_up` or `rehydrate_windowed`), resolve image blocks from CAS, then loop
(consent-capped: `COLLABORATIVE_MAX_ITERATIONS` = 50 / `AUTONOMOUS_MAX_ITERATIONS`
= 100). Each iteration builds `BuildOpts` with cache breakpoints, calls
`provider.stream` with exponential backoff, and processes `StreamEvent`s under
a two-layer timeout (per-chunk idle + total wall-clock). Tokens write directly
to the block store; clients observe via `BlockFlow`. Tool calls run
concurrently via `dispatch_tool_via_broker_with_cancel`, racing the broker's
own per-instance `call_timeout` (live-configurable, `kj policy set`) against
the interrupt token — there is no second, hardcoded timeout ceiling; one used
to clamp every policy timeout to 120s and was removed as redundant with the
cancel-token race. On completion it publishes
`TurnFlow::Completed { output_block_id }` for autonomous turns.

---

## Beat scheduler (`src/beat.rs`)

`BeatScheduler` (`:353`) is a server-lifetime task on its own OS thread, driving
the per-track model's timelines (tracks, not contexts, are what get scheduled —
a musician context attaches to a track). A min-heap of `(Instant, TrackId,
generation)` — the generation guards against a stale heap entry from a track
that was re-armed since it was scheduled — a `BeatCommand` ingress channel, and
a `TurnFlow::Completed` subscription. Commands: Attach, Detach, Play, Pause,
Stop, SetTempo, SetOoda, SetRotate, SetClock, Delete. `STEP = TickDelta::new(1)`
— the playhead is **event-counted, never wall-clock-scaled** (freeze = pause,
resume = +1, no rewind; a wakeup more than `GRID_RESEED_AFTER_PERIODS` late
re-seeds the grid at the actual wakeup rather than catching up — missed beats
are missed). Each wake, `fire_due` advances the playhead, materializes
committed cells (ABC→MIDI), and drains failures to error blocks. The OODA
boundary fires the `tick` rc verb then publishes `TurnFlow::Requested`. On
`TurnFlow::Completed`, `on_turn_completed` (`:2155`) crystallizes the output as
an ABC cell one phrase ahead — guarded by three checks (ephemeral/excluded, a
track-bearing block already off the timeline, a `beat()`-authored legacy
transport row) that refuse to re-crystallize something that is not a fresh
player Act. Poison cells get a `MATERIALIZE_RETRY_BUDGET` = 3 retry budget
before the cell is skipped and the failure surfaces as an error block.

---

## Interrupt + auth

`ContextInterruptState` (`src/interrupt.rs:26`): per-context `stop_after_turn`
(soft, checked before each iteration), `cancel: CancellationToken` (hard, selects
against the stream loop), and a `generation` counter so stream-A cleanup can't
clobber stream-B. Created fresh per prompt.

`AuthDb` (`src/auth_db.rs`): a keyring, not an identity registry — SQLite
(`auth.db`) with `principals` (UUIDv7 id, bare bookkeeping row) and
`credentials` (SSH fingerprint → principal id, no name). `authenticate` is a
single-table lookup on the hot path via `spawn_blocking`, returning a bare
`PrincipalId`. Authorization is binary (key in DB = allowed); anonymous mode
binds an unknown key to the seeded `hajime` character rather than minting.
The given name a player reads is `characters.name` in `kernel.db`, resolved
through `KernelDb::name_for` (`docs/character.md`, "`auth.db` is a
keyring"). Management CLI in `main.rs`: `add-key --as <character>
[--rebind]`, `list-keys`, `list-characters`.

---

## Smells (not fixed — see [issues](../issues.md))

- **External MCP offline** — `list_mcp_servers` returns empty; admin deferred to
  Phase 2. Clients silently get nothing.
- **No graceful SIGTERM** — no signal handler in `main.rs`; the WAL checkpoint
  only fires on clean `Arc` drop, so a `systemd stop` leaves the WAL for next
  open.
- **`AuthDb` behind one mutex** — concurrent auth attempts serialize; no pooling.
- **Tool-result visibility gap** — when `insert_tool_result_as` fails the model
  still gets the result but the user never sees the block (`llm_stream.rs:1226`,
  `:2386`).
- **`$HEARD` ergonomics** — pushed as a JSON string (not a kaish array of
  hashes a script could `for phrase in $HEARD`), window hardcoded to
  `HEARD_WINDOW_PHRASES` = 8 (`beat.rs:272`).

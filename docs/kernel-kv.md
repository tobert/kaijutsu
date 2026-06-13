# Kernel KV

*A first-class, CRDT-backed key–value store in the kernel — persistent shell
variables that sync. (Direction locked 2026-06-12.)*

The kernel grows a small, durable, collaborative key–value store: flat string
keys, string values, multi-writer, real-time-synced, SQLite-backed. Think of
it as the kernel's `env` — but persistent across restarts and shared across
every peer and rc script attached to the kernel.

Its first customer is app state (which context an app instance is looking at),
but it is deliberately general: it is the missing primitive for cross-RC
scratch state, peer coordination, and any "remember this small fact" need that
doesn't deserve a context, a config file, or a block.

---

## Why this, and why now

The trigger is the app reset: we're deleting the 3D card stack, the
constellation, and the forms system (~5,600 LOC of UI experiments whose time
has passed), and having the kernel **bootstrap a privileged context on an empty
DB** so the app is immediately usable. After that, the app shows a single
context and needs somewhere durable to remember *which* one — surviving kernel
restarts and app reconnects. Today that state is ephemeral app memory, which is
exactly why `tech_debt_peer_reattach_on_reconnect` exists: the app doesn't
re-attach to the right context after a kernel restart.

A kernel-hosted KV fixes that by construction (`active_context` lives in the
kernel, not the client) and pays for itself many times over: rc scripts already
want a place to stash cross-turn state, and a synced KV is the natural home.

We considered extending the existing `ConfigCrdtBackend` (theme/models/mcp as
CRDT documents keyed by deterministic IDs). We chose a **first-class** surface
instead — the config backend is file-shaped (CRDT ↔ disk `.toml`), whereas the
KV is variable-shaped (`get`/`set`/`keys`/`watch`). Underneath they share the
same machinery; on top they read very differently.

---

## Persistence model — live CRDT, flush to SQLite

The KV is **not** a relational `kv(key, value, expires_at)` table. It follows
the same pattern as the BlockStore and the config documents:

- The live source of truth is a **diamond-types-extended `Map` CRDT** (LWW per
  key). Writes mutate the Map in memory.
- Ops **journal to the KernelDb oplog in real time** and broadcast for `watch`
  subscribers and peer sync — the same flow blocks take.
- The store **flushes/compacts to a SQLite snapshot** on the existing
  compaction cadence, with `PRAGMA wal_checkpoint(TRUNCATE)` so the main file
  doesn't lag (the 2026-06-11 journaling hardening applies here unchanged).
- On **cold start** the Map is rebuilt via `load_from_db` (snapshot + oplog
  replay), like every other persisted CRDT.
- Persistence is **fail-loud**: a store that declares persistence with no DB
  handle returns `NoDatabaseConfigured` rather than silently dropping a write
  (the `journaling_db()` guard).

The win of reusing this path is that the KV inherits durability guarantees we
already built and tested, instead of introducing a second persistence story.

It is a `Map` document where the BlockStore is a block-log document — same
oplog/snapshot/checkpoint machinery beneath, a different shape on top. (This
*is* the proven in-repo pattern: block metadata is already stored as per-field
DTE `Map` cells — `document.rs` `block_map.get(KEY_KIND)` etc. — so a Map-backed
KV is consistent with how the kernel already does mutable LWW state, not a new
risk.)

**Churn caveat.** Unlike the append-only block log, KV keys are
*overwrite-heavy* — a client rewrites `current_context` on every switch. A
single shared root `Map` means a churny key re-snapshots the whole store on
compaction. At ~100 small keys (bounded by the 64 KB value cap) a snapshot is a
few KB of CBOR, so this is fine — but the compaction trigger should be validated
against the churn pattern during build (the block-log trigger is tuned for
append, not overwrite). If churn ever bites, the escape hatch is
**document-per-key** (each key its own document → isolated per-key compaction,
reusing the existing `DocumentEntry` machinery wholesale). We start with the
single root `Map` for simplicity and keep document-per-key in reserve.

---

## Keys, values, namespaces

- **Keys are flat UTF-8 strings.** There is no hierarchy in the type — a key is
  a string and the store is one flat map.
- **Namespaces are a convention, not a mechanism.** We use `.`-separated
  prefixes, dotted like a shell would (`foo.bar.baz`). Nothing enforces it; it's
  how humans and tools keep out of each other's way.
- **Clients self-namespace by id.** An app instance writes under
  `<client-id>.<name>` — e.g. `019e….current_context`. This is what keeps the
  store "simple and collaborative": there is no per-peer typing in the schema,
  but because each client prefixes with its own id, two instances never clobber
  each other's view state. (The flip side, written down so a future caller
  doesn't trip on it: shared mutable state under a *bare* key IS last-writer-wins
  across all writers — that's a feature for genuinely-global settings and a
  footgun for per-client state. Prefix per-client state.)
- **Client identity** needs a stable id, which we don't have yet. The app seeds
  one at `~/.local/share/kaijutsu/client-id` (a UUID) on first run and uses it
  as its namespace prefix. Failure mode to accept: if that seed file is lost,
  the next run gets a new UUID and the client's prior keys are orphaned under
  the old prefix (and can't be enumerated as "mine" — namespaces are just
  strings). At env scale that's tolerable; noted so it isn't a surprise.
- **No access control, deliberately.** Any writer can write any key — this is a
  single-user, shared-trust kernel (see the capability model: caps are
  ergonomic nudges, not security). We are choosing *no* per-key ACLs ever, not
  deferring them. The dotted convention is hygiene, not a boundary.
- **Values are strings.** Structured data is the caller's JSON. Shell-`env`
  ergonomics: inspectable, diffable, `kj kv get`-able.

### The value envelope (versioned from day one)

Every value is stored as a small versioned envelope, not a bare string. This is
the one piece that, shipped unversioned, would force a per-key migration to add
any future capability — so it's future-proof from the first write even though
v1 uses almost none of it:

```
{
  v: 1,                      // envelope schema version
  value: String,            // UTF-8; structured data is the caller's JSON
  written_at: i64,          // writer's wall clock at set (ms epoch), informational
  expires_at: Option<i64>,  // advisory TTL, absolute writer-clock ms (see below)
  cas_token: u64,           // 0 = unconditional; non-zero enables compare-and-swap
}
```

- **`v`** is the evolution hatch. If we ever want structured values (a child
  CRDT `Map` for per-field merge) or typed scalars, we bump `v` and readers
  dispatch on it — old keys stay `String`, new keys get the new shape, **no
  data migration**. (We deliberately did *not* add a separate `value_type` tag:
  it's redundant with `v`, which already gives reader-side dispatch and
  coexistence.)
- **`cas_token`** rides along unused in v1 (`set` always writes 0), so
  compare-and-swap lands later as `cas(key, expected, new)` with zero envelope
  migration — the field is already in every blob. CAS is the first coordination
  primitive every KV grows (locks, init guards, leader election); the field is
  ~free now and painful to retrofit.
- **Per-value size cap (~64 KB), enforced at `set`** (not a warning). Without it
  the KV becomes a backdoor blob store that bypasses the content-addressed DAG
  and bloats the oplog. Large/structured blobs belong in the CAS or a document,
  not here.

### TTL — advisory in v1, real expiry deferred

Naive embedded TTL is a CRDT trap: wall clocks don't converge, so a key
evaluated against the *reader's* clock can be alive on one peer and expired on
another, and concurrent `set`s with different TTLs tie-break arbitrarily. So in
v1 `expires_at` is **advisory**: an absolute timestamp on the *writer's* clock
that readers MAY treat as "gone" on a best-effort basis. There is **no sweeper**
and no guarantee — a never-read expired key simply lingers. Real expiry, if it's
ever needed, comes as a **lease-style primitive** (server-clock liveness, à la
etcd leases), not embedded data TTL. The envelope carries `expires_at` from day
one so that upgrade isn't a migration.

---

## Surface

Kernel API (first-class, not the document/block model):

```
kernel.kv().get(key) -> Option<String>
kernel.kv().set(key, value)              // set(key, value, expires_at?) advisory
kernel.kv().delete(key)
kernel.kv().keys(prefix?, limit?, cursor?) -> { keys: Vec<String>, next_cursor: Option<String> }
kernel.kv().watch(prefix?) -> stream of changes
// later, no migration: kernel.kv().cas(key, expected_token, value)
```

- The API stays deliberately small and shell-`env`-simple. The forward-proofing
  lives in the *envelope*, not the surface.
- **`keys` is cursor-shaped from day one** even though v1 returns everything
  with `next_cursor: None`. DTE `Map` iteration is unordered, so real pagination
  later needs a sorted side-index — but baking `limit`/`cursor` into the
  signature now means that's an internal change, not a breaking API change.
- **`watch`** is how the app reacts to changes (and how peers/rc see each
  other's writes) — backed by the same broadcast the oplog already drives. A
  watcher that misses ops across a compaction/reconnect boundary falls back to a
  full resync (standard for the oplog path); the app must handle that.
- **capnp**: a small `get`/`set`/`delete`/`keys`/`watch` surface on the wire.
- **`kj kv get|set|keys`** (eventually): the human/agent CLI over the same
  store. The natural fit for the dotted-namespace convention.

---

## Scale & shape expectations

- **~100 keys steady state** — shell-`env`-ish (`env | wc -l`), with headroom as
  it becomes the cross-RC scratchpad.
- **Mutable, overwrite-heavy.** Unlike the append-only block log, KV keys get
  rewritten (a client updates `current_context` on every switch). The oplog
  grows with writes, not keys; compaction is what keeps it bounded. This is the
  main thing to watch operationally — see open questions.

---

## Relationship to other kernel state

- **Not a context.** A KV entry is too small to deserve a `ContextId`, an rc
  lifecycle, or a conversation. The KV is the home for facts below that
  threshold.
- **Not a block.** Blocks are append-only DAG rows with provenance; KV keys are
  mutable LWW cells. Different shape, same persistence plumbing.
- **Adjacent to config.** `ConfigCrdtBackend` is the CRDT↔disk-file story; the
  KV is the CRDT↔variable story. They could converge later, but the surfaces
  serve different mental models and stay separate for now.

---

## Sequencing

This doc is one piece of the app-reset effort:

1. **[DONE — `2a26af9`] Kernel bootstrap** — `if 0 contexts at cold start,
   create a privileged `coder` context` (label `genesis`; broad loadout —
   drive/fork/drift/transport/operator + the coder stance). The hook landed in
   `create_shared_kernel` *after* the `SharedKernelState` is built — the
   `kj_dispatcher` the rc `create` lifecycle needs doesn't exist at the
   post-recovery `~1210` point originally eyeballed. Strictly zero-contexts
   trigger; fail-loud if the seed fails. Folded-in cleanup: extracted the
   `create_context` RPC's ~180-line recipe into a shared `create_context_inner`
   that both the RPC and the bootstrap call. Test: `tests/genesis_bootstrap.rs`.
   Unblocks the demolition by removing the constellation's "create context"
   entry point.
2. **Demolition [DONE — `24c73b3`]** — deleted `ui/card_stack/`,
   `ui/constellation/`, `ui/form/` (~5,617 LOC, 20 files); collapsed `Screen`
   to a single `Conversation` (default), keeping the state machine so
   `OnEnter` still drives initial visibility/focus and future screens can
   reattach. App boots straight into the conversation (`ConversationRoot`
   Visible). Orphaned input Actions + contexts + bindings + handler systems
   removed; the constellation-driven dock activity widget and
   `sync_model_info_to_constellation` dropped. Also retired ~half the
   bevy_vello phase-4 `UiVello*` sites (everything in `form/` + `constellation/`)
   by deletion — see `docs/bevy_vello-escape.md`. `cargo check` clean, 237 app
   tests pass.
3. **Kernel KV** (this doc):
   - **3a — primitive [DONE]** — the `Kv` primitive: `KvDocument` (a flat
     `key→String` LWW Map over a DTE `Document`, in `kaijutsu-crdt`) plus the
     kernel `Kv` store (`kaijutsu-kernel/src/kv.rs`) with KernelDb oplog
     journaling, snapshot/compaction, the versioned envelope, the 64 KB cap, and
     fail-loud `NoDatabaseConfigured`. Reserved well-known document id
     (`uuid5("kaijutsu:kv:root")`), `DocKind::Kv`, handle-implies-row honored.
     18 tests across both crates (convergence, persistence reopen, compaction
     reopen, advisory TTL, corrupt-envelope-surfaces, value cap, watch).
     **Scope corrections vs the design above:**
     - *Envelope is JSON, not raw CBOR.* DTE registers are typed
       (`Nil/Bool/Int/Float/Str`) and hold no bytes, so the envelope rides as a
       JSON string in the register. `v` still gives reader-side version
       dispatch; JSON keeps values inspectable/diffable, which the design wanted
       anyway. (The oplog *ops* are still versioned CBOR via `codec`.)
     - *Compaction trigger resolved.* Op-count since snapshot, threshold **200**
       — tuned below the block log's append-oriented 500 because KV is
       overwrite-heavy. Validated by `survives_compaction_and_reopen` (churns
       one key past the threshold, then reopens). The compaction **rebuilds the
       in-memory doc from the snapshot** so the live history matches what cold
       start deterministically reproduces — without that, post-snapshot ops
       replay as `DataMissing` (same reason the block store rebuilds on
       compact). The snapshot is key-sorted to make that rebuild deterministic.
     - *Delete-under-LWW resolved.* Delete is a `Nil` tombstone: `get`/`keys`
       treat it absent, a later `set` resurrects the key (last-writer-wins, the
       intended model) — confirmed by `deleted_key_can_be_resurrected`.
     - *Watch granularity.* v1 is a whole-store `tokio::broadcast`; prefix
       filtering is the watcher's job (deferred per the open question below).
   - **3a — capnp surface [DONE — `1a638bc`]** — `kvGet/kvSet/kvDelete/kvKeys/
     kvWatch` on the Kernel interface (@78–@82) + a `KvEvents` callback;
     `Kernel::init_kv` late-wires the store at server startup (fail-loud); RPC
     impls (CRUD synchronous; `kvWatch` bridges the whole-store broadcast to the
     callback, lifetime tied to the connection cancel token, drop-on-lag).
   - **3b [DONE — `99ab3c6`]** — `kj kv get|set|delete|keys` (no cap gate; the
     store is shared-trust env). Follows the `.data` convention.
   - **3c — app adoption** (in progress):
     - **client surface [DONE — `dee5b13`]** — `RpcClient` + `ActorHandle`
       `kv_get/kv_set/kv_delete/kv_keys`. KV is reachable end-to-end.
     - **client-id [DONE — `243ee14`]** — seed/read a UUID at
       `~/.local/share/kaijutsu/client-id`; corrupt → replaced, unwritable →
       ephemeral.
     - **reconnect wiring [DONE — needs live verification]** — `ClientId`
       resource seeded from the client-id file; `persist_current_context`
       observes `DocumentCache::active_id` and writes
       `<client-id>.current_context` on every change (one chokepoint → captures
       app UI, MCP-peer `switch_context`, and the restore itself); on connect
       with no joined context, bootstrap reads the saved context and, if it
       still exists, emits `RpcResultMessage::RestoreContext`, drained into a
       `ContextSwitchRequested` that travels the normal join path. Best-effort:
       a KV hiccup or deleted saved context degrades to prior behavior.
       Addresses the context-restore half of
       `tech_debt_peer_reattach_on_reconnect`. Compiles + 239 app tests pass;
       the reconnect/restore *behavior* still wants verification in the running
       app on the GPU box (can't be exercised by `cargo test`).
     - **watch [deferred]** — `kvWatch` exists on the wire + a client-side
       subscription is the next refinement for live cross-instance sync; the
       reattach fix doesn't need it (read-on-connect suffices).

---

## Open questions

Resolved into the design above: value-size discipline (hard 64 KB cap at
`set`), envelope evolution (`v` + `cas_token`), TTL (advisory-only in v1, leases
later), keys pagination (cursor-shaped signature). Still genuinely open:

- **~~Compaction trigger for churn~~ [RESOLVED in 3a].** Op-count-since-snapshot,
  threshold 200, with in-memory rebuild on compact. See the 3a note above.
- **Watch granularity.** Per-key, per-prefix, or whole-store change streams?
  v1 ships **whole-store** (`tokio::broadcast`, watcher filters by prefix).
  Per-prefix matches the namespace convention but costs more bookkeeping (the
  block-log flow bus is per-document, not per-prefix) — revisit if a hot watcher
  on a busy store proves the broadcast+filter wasteful.
- **~~Delete semantics under LWW~~ [RESOLVED in 3a].** `Nil` tombstone; a late
  concurrent `set` resurrects the key (intended last-writer-wins). Confirmed by
  `deleted_key_can_be_resurrected`.

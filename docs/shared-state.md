# Shared state — the VFS *is* the namespace

> **Status:** high-level sketch, captured 2026-06-28 from a framing conversation
> (the same session that sketched the now-retired myaku pulse facility).
> Directions, not commitments. Companion to `docs/slash-v.md` (the `/v` sysfs/CRDT
> surfaces); the pulse/sampling design that lived in `docs/myaku.md` is **retired —
> recover its detail from git history** and migrate it here (see *Open* below).
> Code is truth; this is where we're aiming.

## The thesis

There is no bespoke "shared state store." **The shared state space is the VFS
namespace**, and the only real choice is *which mount* a thing lives on. One name
per thing, `cat` is the universal read, and every surface — the Bevy app, kaish,
the file tools, MCP, SFTP — sees the same trees because they're ordinary
`VfsBackend`s. This is the "instrument you play" stance made literal, and it's the
same move `slash-v.md` already makes for context/session introspection.

The corollary: **the CRDT KV store (`KvDocument`/`Kv`) is being deleted.** It has
no production callers (test-only), and everything it was meant to hold is better
served by files on a mount. Two stores for "durable shared" is the silent-fallback
smell. See *Retiring KV* below for the one real tie to migrate first.

## Tiers are mounts, not abstractions

| Need | Mount | Backend | Semantics |
|------|-------|---------|-----------|
| **Ephemeral, shared-within-kernel, read-write** | `/run` | `MemoryBackend` (exists, `vfs/backends/memory.rs`) | tmpfs/XDG `RUNTIME_DIR` vibe. `/run/pulse/` = metric/probe data (design in git history, removed `docs/myaku.md`); `/run/<…>/` = agent/user scratchpad + OODA working trees. KV-replacement. |
| **Durable, peer-synced, introspectable** | `/v/...` | CRDT + synthesized backends (`/v/docs`, `/v/ctx`, `/v/session`) | the durable + sysfs namespace `slash-v.md` designs |

`/run` is its **own** mount (the existing `/scratch` `MemoryBackend` is likely
retired — nothing uses it; `/run` is the better-named home for all ephemeral
read-write state). One backend, two top-level uses: `/run/pulse/` written by probes,
the rest a general scratchpad.

Two clean lines fall out:

- **Synthesized `/v` is for non-file kernel state; file-born data lives in
  MemoryFS.** `slash-v.md`'s read-only synthesized backends (`/v/ctx`, `/v/session`)
  earn their keep because the block store and peer registry **aren't files** — they
  have to be *rendered*. Metrics are the opposite: a probe **writes them as files**,
  so there is nothing to synthesize. They go on a plain `MemoryBackend` mount
  (`/run`), not a `/v/ctx`-style backend. (This reverses an earlier draft that put
  metrics under a read-only `/v` backend — it was ceremony; see below.)
- **Read-only is a nudge, not a boundary — and metrics don't need it.** In the
  shared-trust model read-only isn't security, and metric data **self-heals**: clobber
  `/run/pulse/cpu/history` and the probe rewrites it within a tick. Paying for a Rust
  `VfsBackend` to protect continuously-regenerated data is a bad trade. Writable
  MemoryFS is fine.
- **Persistence is a copy, not a store type.** Ephemeral → durable is
  `cp /run/foo /v/docs/foo` (or out to the host FS). The tiers compose through plain
  file ops; no write-through, no dual ownership. (XDG framing: a probe's volatile
  data is `RUNTIME_DIR` → `/run`; if one ever wants persistence, `STATE_DIR` → a `/v`
  CRDT mount.)

"Shared" needs no CRDT *within one kernel*: every app/agent/MCP session talks to the
one kernel, so they all see the one `MemoryBackend`. CRDT (the `/v` tier) earns its
keep only for **durability and cross-kernel peer sync** — exactly what `slash-v.md`
already builds.

## The file-layout convention (a kaish helper, not a backend)

Because metrics are just files a probe writes, uniformity comes from a **shared
kaish helper** (`pulse_emit`, designed in the removed `docs/myaku.md` — git history) that every probe calls — *convention,
not enforcement*. It still honors `slash-v.md`'s hard-won sysfs principles, just in
userspace:

- **a snapshot read** — `now` is the current sample in one read (the headline
  "what is it right now"), **plus** `history`, a bounded per-probe TSV (the sample
  window, one read for a sparkline). (Per-field scalar files were dropped as
  redundant with `now`; easy to re-add if a single-value `cat` need appears.)
- **text, line-oriented, greppable** — `awk` a `history` column for a sparkline,
  `cat now` for an OODA glance;
- **hot vs cold reads** — `MemoryBackend` bumps `FileAttr.generation` on every write,
  so a re-`stat` shows a fresh sample and a dead probe stops bumping (the coherence
  signal, free, without a synthesized backend);
- **bounded, never unbounded** — `pulse_emit` trims `history` to its cap each tick
  (rewrite, not append), which also sidesteps the O(n)-append gap noted below.

## OODA's Observe stage reads this space

This is the load-bearing reason the space must be reachable by agents, not just the
HUD. A future OODA context that watches system state ("how's the GPU doing?") uses
the shared space as its **Observe surface**, built entirely from existing primitives:

- **Observe = an `rc` verb.** `/etc/rc/<ooda-type>/observe/SXX-*.kai` runs kaish that
  `cat`s `/run/pulse/...` (and `/v/ctx`, `/v/session`) and **assembles blocks**: `.kai`
  stdout already routes to `Trace` blocks, `.md` to the system-prompt slot. No new
  machinery — rc + kaish + blocks composed. (myaku design in git history; rc lifecycle in
  `crates/kaijutsu-kernel` `/etc/rc`.)
- **Pull and push.** The agent *pulls* by reading a file when it decides to look; the
  space can *push* by drifting a threshold crossing (`temp_c > 85`) into the
  context's mailbox to flush next turn — the async-event path that already exists.
- **OODA working tree = a `/run` subtree** the context reads/writes; persistence
  is opt-in `cp` out to `/v` or host FS.
- **Reductions** — an OODA context wants *"GPU 95% for 5 min, VRAM 22/24, trending
  up"*, not 120 raw `history` rows. Open who computes it: a probe-written summary file
  (kernel-side, shared, thin agent) vs the agent reducing `history` itself
  (thin-client tension). Deferred to the myaku session.

## Retiring KV (delete — it has one real caller, and it splits in two)

> Confirmed with Amy 2026-07-04: KV retirement is the plan. The original KV
> design record (`docs/kernel-kv.md`) is deleted — git history has it. The code
> deletion below is still to do (tracked in issues.md, "Delete KvDocument/Kv").

`KvDocument`/`Kv` + the capnp surface (`kvGet`/`kvSet`/`kvDelete`/`kvKeys`/`kvWatch`,
@79–83) and `kj kv` go away. The audit's "test-only" read was **too optimistic**: the
app is a real, production caller. It persists exactly one key pattern —
`<client-id>.current_context` (`kaijutsu-app/.../actor_plugin.rs:319` writes it on
every switch; the reconnect bootstrap `kv_get`s it back into a `RestoreContext` at
`:534`). That is the *entire* production use of a 64 KB-envelope, journaling,
compacting store: **one `ContextId` per app installation — "reopen the context this
window was last looking at."**

That one job is actually **two** needs the old design blurred, and naming the split is
what unblocks the deletion:

- **Live acting context** — what `slash-v.md` renders at `/v/session/<id>/context`. It
  is *per-session and ephemeral*: it dies with the connection, which is correct (a
  disconnected session has no live context to show). It already lives in
  `SessionContextMap` (`runtime/context_engine.rs:31`, an `Arc<DashMap<SessionId,
  ContextId>>` the shell resolves per-op). **No KV.**
- **Durable view restoration** — the app's actual KV use. It is *per-installation and
  must survive the connection ending* — that is the whole point of writing it durably.
  The ephemeral registries **cannot** hold it: `SessionContextMap` and `PeerRegistry`
  (a `HashMap` with `detach()` on disconnect, `peers.rs:148`) both vanish exactly when
  the restore value still has to exist. So "move it to the session registry" — the
  obvious first answer — would *silently break reattach-restore*.

The right home is what Amy called a "lil typed state the kernel manages on the client's
behalf": a **small, normalized, per-client store** — a `KernelDb` row keyed by the
stable client-id (`feedback_sql_schema`: relational, not a stringly JSON/KV blob), with
a typed RPC (`setLastContext` / `getClientView`) replacing `kvGet("<uuid>.current_
context")` and the hand-formatted key namespace (`client_id.rs`). This is strictly
*more* type-safe and *less* machinery than KV — no envelope versioning, no
overwrite-tuned journal/compaction, no stringly keys. It can be **projected read-only
under `/v`** for introspection (the slash-v pattern: render kernel state as files, write
via the typed path), so it honours "the VFS is the namespace" without resurrecting a
general KV. Everything else KV might have held becomes a `/run` file.

## Open questions (deferred)

- **`/run` shape** — settled: its **own** `MemoryBackend` mount (not `/scratch/run`;
  `/scratch` likely retired), metrics as plain files (no synthesized `/v` backend),
  death certificate as a `status` file in `/run/pulse/<probe>/`. Remaining: the
  scratchpad/OODA-working-tree layout under `/run` outside `/run/pulse/`. → myaku
  session.
- **Reduction ownership** — which summaries the kernel projects vs the agent derives
  (the thin-client tension). → myaku session.
- **`current_context` split — settled in shape, open in surface.** The *live* render
  reads `SessionContextMap`; the *durable* restore moves to a typed per-client
  `KernelDb` store (see *Retiring KV*). The store's `/v` projection is sketched as
  **`/v/clients`** (`docs/slash-v.md`, *Future*) — and it's more than read-only
  introspection: `/v/clients/<id>/context` is *writable*, so the same field is the
  client's own setter **and** a remote steering surface (drive a wall of tablets onto
  different contexts; players at them can also drive). Open: the typed RPC shape, how a
  steered client observes the change (poll `generation` vs. a `kvWatch` successor — KV's
  is being deleted), and which other fields (theme, layout, spotlight) join `context`.
- **Append gap** — `VfsOps` has no `append()`; `write_all`/`>>` are O(n). myaku
  sidesteps it (bounded rewrite) and OODA writes are turn-cadence, but a real
  `append()` (O(1) on `MemoryBackend` via `write(offset=size)`) is worth it someday.
  Tracked in `docs/issues.md`.

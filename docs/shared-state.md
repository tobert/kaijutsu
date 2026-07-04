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

The corollary: **the CRDT KV store (`KvDocument`/`Kv`) was deleted** (2026-07-04).
It had no production callers beyond the one real tie — the app's durable
context-restore — which moved to a typed per-client store first. Two stores for
"durable shared" was the silent-fallback smell. See *KV retired* below.

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

## KV retired (deleted 2026-07-04)

`KvDocument`/`Kv`, the capnp surface (`kvGet`/`kvSet`/`kvDelete`/`kvKeys`/`kvWatch`,
@79–83, ordinals retired not reused), and `kj kv` are gone. The one real production
use — the app's `<client-id>.current_context` restore-on-reconnect — split in two
first: the *live* acting context stayed in the already-ephemeral `SessionContextMap`
(no KV needed), and the *durable* restore moved to a small typed per-client store
(`client_views` `KernelDb` row + `setLastContext`/`getClientView` RPC, cb69a81a).
Everything else KV might have held becomes a `/run` file.

Open: `kvWatch`'s successor for the `/v/clients` steering surface (see *Open
questions* below) — does a steered client poll `generation` or get a new push
primitive? Full design history and the split rationale are in git
(this section, pre-2026-07-04, and `docs/issues.md`).

## Open questions (deferred)

- **`/run` shape** — settled: its **own** `MemoryBackend` mount (not `/scratch/run`;
  `/scratch` likely retired), metrics as plain files (no synthesized `/v` backend),
  death certificate as a `status` file in `/run/pulse/<probe>/`. Remaining: the
  scratchpad/OODA-working-tree layout under `/run` outside `/run/pulse/`. → myaku
  session.
- **Reduction ownership** — which summaries the kernel projects vs the agent derives
  (the thin-client tension). → myaku session.
- **`current_context` split — settled in shape, open in surface.** The *live* render
  reads `SessionContextMap`; the *durable* restore moved to a typed per-client
  `KernelDb` store (see *KV retired*). The store's `/v` projection is sketched as
  **`/v/clients`** (`docs/slash-v.md`, *Future*) — and it's more than read-only
  introspection: `/v/clients/<id>/context` is *writable*, so the same field is the
  client's own setter **and** a remote steering surface (drive a wall of tablets onto
  different contexts; players at them can also drive). Open: the typed RPC shape, how a
  steered client observes the change (poll `generation` vs. a `kvWatch` successor, now
  that KV itself is gone), and which other fields (theme, layout, spotlight) join `context`.
- **Append gap** — `VfsOps` has no `append()`; `write_all`/`>>` are O(n). myaku
  sidesteps it (bounded rewrite) and OODA writes are turn-cadence, but a real
  `append()` (O(1) on `MemoryBackend` via `write(offset=size)`) is worth it someday.
  Tracked in `docs/issues.md`.

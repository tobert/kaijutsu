# Shared state — the VFS *is* the namespace

> **Status:** high-level sketch, directions not commitments. Companion to
> `docs/slash-v.md` (the `/v` sysfs and document surfaces). `docs/myaku.md`
> — the probe/metrics facility this doc's OODA section builds on — was
> retired when the MIDI clock-drift design absorbed its pulse-clock content
> (`docs/midi.md`); the metrics/probe half was never migrated anywhere and
> stays unbuilt design here. Code is truth; this is where we're aiming.

## The thesis

There is no bespoke "shared state store." **The shared state space is the VFS
namespace**, and the only real choice is *which mount* a thing lives on. One name
per thing, `cat` is the universal read, and every surface — the Bevy app, kaish,
the file tools, MCP, SFTP — sees the same trees because they're ordinary
`VfsBackend`s. This is the "instrument you play" stance made literal, and it's the
same move `slash-v.md` already makes for context/session introspection.

The corollary: **there is no KV store.** `KvDocument`/`Kv` was deleted; two
stores for "durable shared" was the silent-fallback smell. See *KV retired*
below.

## Tiers are mounts, not abstractions

| Need | Mount | Backend today | Semantics |
|------|-------|---------|-----------|
| **Ephemeral, sink-fed, read-only** | `/run/midi`, `/run/audio`, `/run/roster` | three separate synthesized backends, each its own leaf mount — not a bare `/run` mount routing internally | tmpfs/XDG `RUNTIME_DIR` vibe: liveness/inventory facts written only by their one sink (a presence report, an audio node, roster refresh), never trusted as a stored fact across a restart. |
| **Ephemeral, shared-within-kernel, general read-write** | none yet | unbuilt — a generic `/run` `MemoryBackend` scratchpad (probe/metrics data, agent/OODA working trees) is still just direction | KV-replacement, if built. |
| **Durable, peer-synced, introspectable** | `/v/cas` | Kernel documents plus the shipped `CasFs`; `/v/ctx` is designed but unbuilt (`docs/slash-v.md`). | The durable and sysfs namespace `slash-v.md` designs. |

The generic scratchpad idea below (`/run/pulse/` for probe metrics,
`/run/<…>/` for agent/OODA working trees) has no mount behind it today —
only the three purpose-built read-only trees above exist. Where this section
says "goes on `/run`," read it as direction: the metrics story is what
`docs/myaku.md` sketched before it was retired, never built, and reconstructable
from git history if picked back up.

Two clean lines, if it gets built:

- **Synthesized state is for non-file kernel state; file-born data would live in
  MemoryFS.** The kernel's read-only synthesized backends earn their keep because
  what they render — the block store, the roster, a device's presence — **isn't
  files**; it has to be rendered. Metrics are the opposite: a probe would **write
  them as files**, so there would be nothing to synthesize. They'd go on a plain
  `MemoryBackend` mount, not a synthesized-backend tree.
- **Read-only is a nudge, not a boundary — and metrics wouldn't need it.** In the
  shared-trust model read-only isn't security, and metric data **self-heals**: clobber
  `/run/pulse/cpu/history` and the probe rewrites it within a tick. Paying for a Rust
  `VfsBackend` to protect continuously-regenerated data is a bad trade. Writable
  MemoryFS would be fine.
- **Persistence is a copy, not a store type.** Ephemeral → durable would be
  `cp /run/foo /v/docs/foo` (or out to the host FS). The tiers would compose through
  plain file ops; no write-through, no dual ownership.

Sharing needs no merge machinery *within one kernel*: every app, agent, and MCP
session talks to the one kernel, so they'd all see the one `MemoryBackend`. The
`/v` tier earns its keep for **durability and cross-kernel peer sync** — exactly
what `slash-v.md` already builds.

## The file-layout convention (a kaish helper, not a backend)

Because metrics are just files a probe writes, uniformity comes from a **shared
kaish helper** (`pulse_emit`, sketched in the retired `docs/myaku.md`,
recoverable from git history) that every probe would call — *convention,
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

- **Observe = an `rc` verb.** `/config/rc/<ooda-type>/observe/SXX-*.kai` runs kaish that
  `cat`s `/run/pulse/...` (and, once built, `/v/ctx`; `/run/roster` today) and
  **assembles blocks**: `.kai` stdout already routes to `Trace` blocks, `.md`
  to the system-prompt slot. No new machinery needed — rc + kaish + blocks
  compose.
- **Pull and push.** The agent *pulls* by reading a file when it decides to look; the
  space can *push* by drifting a threshold crossing (`temp_c > 85`) into the
  context's mailbox to flush next turn — the async-event path that already exists.
- **OODA working tree = a `/run` subtree** the context reads/writes; persistence
  is opt-in `cp` out to `/v` or host FS.
- **Reductions** — an OODA context wants *"GPU 95% for 5 min, VRAM 22/24, trending
  up"*, not 120 raw `history` rows. Open who computes it: a probe-written summary file
  (kernel-side, shared, thin agent) vs the agent reducing `history` itself
  (thin-client tension) — open if the metrics facility gets built.

## KV is retired

`KvDocument`/`Kv`, the capnp surface (`kvGet`/`kvSet`/`kvDelete`/`kvKeys`/`kvWatch`),
and `kj kv` are gone — no callers anywhere in the tree, and their old ordinals
(79–83) are reassigned to unrelated methods (`subscribeEditor`,
`setLastContext`, `getClientView`, `listTracks`, `promoteContext`), not left
as `retiredNN` stubs. The one real production use — the app's
`<client-id>.current_context` restore-on-reconnect — split in two: the *live*
acting context stays in the already-ephemeral `SessionContextMap` (no KV
needed), and the *durable* restore is a small typed per-client store
(`client_views` `KernelDb` row + `setLastContext`/`getClientView` RPC).
Everything else KV might have held becomes a `/run` file, if `/run` is ever
built as a general scratchpad.

Open: `kvWatch`'s successor for the `/v/clients` steering surface (see *Open
questions* below) — does a steered client poll `generation` or get a new push
primitive?

## Open questions (deferred)

- **`/run` shape — still open, nothing built.** A generic writable
  `MemoryBackend` mount for probe metrics and agent/OODA scratch has no
  mount behind it today; only the three read-only sink-fed trees
  (`/run/midi`, `/run/audio`, `/run/roster`) exist. If built: one mount, a
  death certificate as a `status` file per probe, and the
  scratchpad/OODA-working-tree layout still to design.
- **Reduction ownership** — which summaries the kernel projects vs. the agent
  derives (the thin-client tension) — open if the metrics facility gets built.
- **`current_context` split — settled in shape, open in surface.** The *live* render
  reads `SessionContextMap`; the *durable* restore moved to a typed per-client
  `KernelDb` store (see *KV retired*). The store's `/v` projection is sketched as
  **`/v/clients`** (`docs/slash-v.md`, *Future*) — and it's more than read-only
  introspection: `/v/clients/<id>/context` is *writable*, so the same field is the
  client's own setter **and** a remote steering surface (drive a wall of tablets onto
  different contexts; players at them can also drive). Open: the typed RPC shape, how a
  steered client observes the change (poll `generation` vs. a `kvWatch` successor, now
  that KV itself is gone), and which other fields (theme, layout, spotlight) join `context`.
- **Append gap** — `VfsOps` still has no `append()`; `write_all`/`>>` are O(n). A
  rewrite-bounded metrics facility would sidestep it and OODA writes are
  turn-cadence, but a real `append()` (O(1) on `MemoryBackend` via
  `write(offset=size)`) would be worth it someday.

# Shared state — the VFS *is* the namespace

> **Status:** high-level stance, not a full design. Companion to
> `docs/slash-v.md` (the `/v` sysfs and document surfaces). Code is truth;
> this is where we're aiming.

## The thesis

There is no bespoke "shared state store." **The shared state space is the VFS
namespace**, and the only real choice is *which mount* a thing lives on. One name
per thing, `cat` is the universal read, and every surface — the Bevy app, kaish,
the file tools, MCP, SFTP — sees the same trees because they're ordinary
`VfsBackend`s. This is the "instrument you play" stance made literal, and it's the
same move `slash-v.md` already makes for context/session introspection.

The corollary: **there is no KV store.** `KvDocument`/`Kv` was deleted; two
stores for "durable shared" was the silent-fallback smell. See *Retiring KV*
below.

## Retiring KV

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

- **`current_context` split — settled in shape, open in surface.** The *live* render
  reads `SessionContextMap`; the *durable* restore moved to a typed per-client
  `KernelDb` store (see *Retiring KV*). The store's `/v` projection is sketched as
  **`/v/clients`** (`docs/slash-v.md`, *Future*) — and it's more than read-only
  introspection: `/v/clients/<id>/context` is *writable*, so the same field is the
  client's own setter **and** a remote steering surface (drive a wall of tablets onto
  different contexts; players at them can also drive). Open: the typed RPC shape, how a
  steered client observes the change (poll `generation` vs. a `kvWatch` successor, now
  that KV itself is gone), and which other fields (theme, layout, spotlight) join `context`.

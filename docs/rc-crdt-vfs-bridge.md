# RC Scripts on the CRDT-VFS — Design Sketch

Status: **design only, not implemented.** Tracks the "CRDT-VFS bridge for
collaborative script editing" follow-up in `docs/issues.md`.

## Goal

Let multiple peers co-edit rc lifecycle scripts (`/etc/rc/<context_type>/<verb>/SXX-name.{kai,md}`)
the same way they co-edit any other file — live, conflict-free, via the CRDT —
instead of each `kj rc edit` being a lone last-writer-wins `UPDATE` against a
SQLite row.

Non-goal: changing *what* rc scripts do or *when* they fire. This is purely a
storage/edit-surface change underneath the existing dispatch.

## Current state (code is truth)

**Storage — plain SQLite.** `rc_scripts` table (`kernel_db.rs:546`), PK
`(context_type, verb, sort_key, name)`, `UNIQUE(path)`. Every edit path is a
direct row op:

- `kj rc add`  → `insert_rc_script` (`kernel_db.rs:1997`)
- `kj rc edit` → `update_rc_script` (`kernel_db.rs:2085`) — last write wins, no merge
- `kj rc rm`   → `delete_rc_script` (`kernel_db.rs:2073`)
- `kj rc show` → `get_rc_script` (`kernel_db.rs:2023`)
- `kj rc reseed` → re-applies `seed_scripts.rs` defaults

**Dispatch — reads rows.** `run_rc_lifecycle` (`lifecycle.rs:76`) calls
`db.list_rc_scripts(ctx_type, verb)` (`kernel_db.rs:2042`), ordered by
`(sort_key, name)`, then runs each: `.md` → block into the system-prompt slot,
`.kai` → `kaish` execution with `KJ_*` overlay vars.

**Seeding — floor, not state.** `ensure_seeded_rc_scripts` (`seed_scripts.rs`,
called from `KernelDb::open`) does `INSERT OR IGNORE` per canonical path on every
DB open. Built-in defaults (e.g. `S00-stance.md` for `coder`) are a *floor*: a
user `rm` reappears on next open; `kj rc edit` overrides in place; `kj rc reseed`
force-overwrites.

**The CRDT side already exists.** `FileDocumentCache` (`file_tools/cache.rs`)
maps any VFS path → a CRDT document: deterministic `ContextId` via
`uuid_v5(NAMESPACE_URL, "kaijutsu:file:{path}")`, one `BlockKind::Text` block per
file, full oplog + snapshot persistence (survives restart), mtime-staleness
reload, write-through `flush_dirty`/`flush_one`. It is owned by `Kernel`
(`file_cache: OnceLock<Arc<FileDocumentCache>>`) and reachable from rc/lifecycle
code via `KjDispatcher::kernel()` — just not wired there yet.

So the infrastructure is present and proven (the file tools use it). The work is
**wiring + a consistency decision + migration**, not new machinery.

## The core tension: who is authority?

Two durable representations want to hold the same bytes:

1. The `rc_scripts` **row** — queried by dispatch, keyed by the canonical tuple,
   carries `timeout_secs`, `created_by`, ordering.
2. The CRDT **document** — the collaborative edit surface, but a bag of text
   with no rc-specific columns.

Dispatch must stay cheap and synchronous-ish (`list_rc_scripts` is on the hot
fork path). Editing must become CRDT-mergeable. The design is mostly about how
these two stay coherent without a silent-fallback divergence (a poisoned rc
script is a footgun — it runs on every fork).

## Options

### Option A — CRDT is the edit surface, row is a projection (recommended)

The `rc_scripts` row stays the dispatch source of truth and the metadata home
(`timeout_secs`, ordering, `created_by`). The **content** column becomes a
write-through projection of a CRDT document at a canonical VFS path,
`/etc/rc/<type>/<verb>/SXX-name.<ext>`.

- `kj rc add` → create the CRDT doc (`create_or_replace`) **and** insert the row
  (content = current CRDT text) in one transaction-ish unit.
- `kj rc edit` → edit the CRDT doc; a flush hook projects the new text into
  `update_rc_script`. Concurrent peer edits merge in the CRDT; the row is
  overwritten with the merged result, not a racing raw string.
- `kj rc rm` → delete row + invalidate/evict the CRDT doc.
- Dispatch → unchanged: still reads the row. It never blocks on the CRDT.
- Reseed → writes both layers; CRDT doc replaced with the default body.

**Why recommended:** dispatch stays a plain indexed row read (no async CRDT
hydrate on the fork hot path); metadata keeps its typed columns; collaboration
lands exactly where editing happens. The row is a *cache* of the CRDT content,
refreshed on flush — and a cache that's always rewritten from the merged CRDT
state, never the other way, so there's a single direction of truth for content.

**Risks:** the row-content projection can lag a not-yet-flushed CRDT edit. Two
mitigations: (a) flush-on-edit synchronously inside `kj rc edit` before
returning; (b) dispatch could, behind a flag, read live CRDT content for any doc
that's currently dirty. Start with (a); it keeps dispatch untouched.

### Option B — CRDT is the only store; drop the content column

Make the rc script content live *solely* in the CRDT doc. `rc_scripts` keeps
only metadata (`path`, `timeout_secs`, ordering, `created_by`) — no `content`.
Dispatch loads content via `FileDocumentCache::read_content(path)` per script.

- Cleanest "single source of truth" for content.
- But: puts an `async` CRDT read (hydrate-from-snapshot+oplog, possible disk) on
  the fork hot path, N times per lifecycle. Fork latency is already load-bearing
  (cache breakpoints, rc, parent block count). Measure before committing.
- Migration must move every existing `content` into a CRDT doc with no window
  where dispatch sees an empty script.
- A CRDT read failure on the fork path is now a *fork* failure — needs an
  explicit, observable error (no silent empty-script fallback; per the
  crash-over-corruption stance an empty stance script is corruption).

### Option C — status quo + advisory CRDT mirror (rejected)

Keep rows authoritative; maintain a *best-effort* CRDT mirror for a future
collaborative editor, with no guarantee they agree. Rejected: a mirror that can
silently disagree with the executed script is exactly the silent-fallback /
data-corruption footgun the project stance forbids. If the CRDT and the row can
diverge, peers "collaborating" in the editor may not be editing what actually
runs.

## Recommendation

**Option A.** It reuses the existing cache, keeps the fork hot path a row read,
preserves the typed metadata, and gives collaboration the merge semantics at the
edit surface. Revisit Option B only if a use case needs dispatch to honor
unflushed live edits, and only after measuring the per-script async read cost on
the fork path.

## Sketch of the wiring (Option A)

1. **Canonical path = VFS path.** rc canonical paths are already
   `/etc/rc/...` — feed them straight to `FileDocumentCache`. The UUIDv5
   derivation makes the doc id stable across peers and restarts for free.
2. **`kj rc add`:** `cache.create_or_replace(path, body)` → read back merged
   content → `insert_rc_script(row{ content })`. Fail the whole op if either
   half fails (mirror the fork-atomicity transaction discipline:
   `insert_forked_context` is the template for "both writes or neither").
3. **`kj rc edit`:** apply the edit to the CRDT doc, `flush_one(path)`, then
   `update_rc_script(path, content = merged)`. Synchronous flush before return.
4. **`kj rc rm`:** `delete_rc_script` + `cache.invalidate(path)`.
5. **`kj rc reseed`:** rewrite both layers from `seed_scripts.rs`.
6. **Dispatch:** untouched.
7. **Access:** rc.rs / lifecycle.rs reach the cache via
   `self.kernel().file_cache(&self.blocks)`; thread it through `KjDispatcher`
   like the other kernel services.

## Migration

- On first open after the feature lands, for every existing `rc_scripts` row
  with no backing CRDT doc: create the doc from the row's `content`
  (`create_or_replace`). Idempotent (deterministic id + `INSERT OR IGNORE`-style
  guard). Reuses the seeding "floor on every open" pattern.
- Seed defaults (`seed_scripts.rs`) get the same treatment: when a seed inserts a
  row, ensure its CRDT doc exists.
- No down-migration needed — the `content` column stays populated, so an older
  binary reading the table still works (it just won't see live collaborative
  edits).

## Open questions

- **Edit-while-fork.** A peer editing `S00-stance.md` while another forks a
  `coder` context: Option A runs the last-flushed content. Is that acceptable, or
  do we need fork to see the live doc? (Leans acceptable; flag for Option B if
  not.)
- **`.kai` validation.** Should the CRDT edit surface pre-validate kaish syntax
  (kaish already pre-validates) before the row projection accepts a body, so a
  broken script can't reach dispatch? Probably yes — reject the projection, keep
  the prior row.
- **DTE / cross-peer replication.** rc docs are CRDT docs; do they replicate over
  the same drift/DTE path as conversation docs, or a dedicated channel? Ties into
  the "Config CRDT ops" issue.
- **Eviction.** rc docs are tiny and hot at fork time; consider pinning them in
  the cache (exempt from the 64-entry LRU) so a fork never pays a cold reload.

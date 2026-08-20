# File buffers — how kaijutsu holds file content

**Disk is the source of truth.** A file buffer is a cache of disk, never a
replacement for it. Every rule below follows from that one sentence, and the
2026-08-18 incident happened because the code believed the opposite.

Canonical for the file tools (`file_tools/cache.rs`), the tool surface they
expose, and the editor's relationship to both. The editor's own design is
`docs/vi.md`; this doc owns what sits underneath it.

## The incident this doc exists for

On 2026-08-18 a context edited `docs/issues.md` and wrote back content from
**2026-06-29**, dropping 115 entries and resurrecting 8 that had been deleted
as shipped. It was not a bad edit. The kernel *served* the stale content.

Measured the same day against the running kernel:

| path | kernel served | disk truth |
|---|---|---|
| `crates/kaijutsu-kernel/src/mcp/servers/file.rs` | 1250 lines | 1798 |
| `crates/kaijutsu-kernel/src/kj/ledger.rs` | 1221 | 2101 |
| `AGENTS.md` | 102 | 422 |

1250 lines matches commit `3321e597` exactly — **2026-06-26**.

The chain: `file_context_id(path)` is a deterministic `UUIDv5` of the path, so
a path maps to one document id forever. File documents are durable in the block
store, so they outlive a restart. On a cache miss the loader reads disk into
`text`, then calls `create_document`, and the "already exists" arm **discards
that fresh text** and serves the original block — while stamping the entry with
the *current* disk generation, so the staleness check can never fire again. The
path poisons its own freshness stamp, then a flush writes the result back.

This is a CRDT-era leftover. When the document was the source of truth,
persisting it was correct. After the CRDT was removed, disk became the source
and the durable document became a stale mirror keyed to collide with itself.

## The model: vim, faithfully

We had already built most of vim's *write* side and none of its *read* side.

| vim | kaijutsu today |
|---|---|
| buffer dirty flag | `CachedFileDoc.dirty`, `EditorSessionInfo.dirty` |
| `:w` / `ZZ` | `editor_save`, `flush_one`, `flush_dirty` |
| last-written state | `EditorSession.saved_content` (the checkpoint) |
| swap file | durable file document — but **silent**, which is the bug |
| `W12` changed-under-us | designed, **not implemented** (`vi.md`: "`:w!` == `:w` today") |

Vim reads the file from disk into the buffer every time you open it, keeps
unsaved work in a swap file, and *announces* a recovered swap rather than
serving it. We do the first not at all and the third silently.

### The four rules

1. **A clean buffer is not authoritative.** Re-read from disk. Never persist a
   clean buffer and trust it later. This alone removes the whole bug class.
2. **A dirty buffer is a swap file.** Persist it — unsaved work should survive a
   kernel restart, which is a real property we have today and want to keep. On
   reopen, *announce* it. Never serve it silently as the file.
3. **`:w` refuses when disk moved under us.** Compare disk generation against
   the buffer's load generation at flush time. `:w!` overrides. This is vim's
   W12 and it is the guard that would have stopped the incident.
4. **The kernel enforces, the UI explains.** A recovered swap is not served as
   authoritative until acknowledged. A `Bool` on a wire struct is ignorable by
   a renderer that does not know to look; the safety must not live there.
   `kj swap list` shows every recovered swap the kernel knows about; `kj swap
   ack <path>` keeps the buffer (acknowledges + flushes it to disk in one
   move) and `kj swap discard <path>` drops it (disk wins on the next read).
   Both refuse loudly on a path with no swap. Read the buffer first at
   `/v/swap/<kernel_id>/<path>` before choosing.

**An open editor session pins its cache entry.** Rules 2-3 protect a *dirty*
buffer — `evict_if_needed` already skips dirty entries — but a session sits
*clean* between `editor_open` and its first edit, and again after every
successful `:w`. In that window an unrelated read of some other path (any MCP
read, any kaish `cat` — each one inserts a cache entry) could evict the
session's entry, and `mark_dirty`/`flush_one` used to treat "no entry" as
"nothing to do" and return `Ok(())`: `:w` reported success while nothing
reached disk (P1, `docs/issues.md` "Tech-debt audits, 2026-08-20"). The fix is
two-sided: `mark_dirty`/`flush_one` now error (`FlushError::NotCached`) rather
than silently no-op on an uncached path, and `Kernel::editor_open_as` pins the
target's cache entry (`FileDocumentCache::pin`/`unpin`, ref-counted so two
sessions on one path don't fight over the pin) for the session's whole open
lifetime, released on close (`editor_keys`'s `Closed` arm, `editor_quit`). A
pinned entry is never evicted, dirty or not — the loud error is the backstop
for every *other* caller, not the editor's steady-state path.

**The pin protects eviction; `invalidate`/`invalidate_document` protect it
too, by refusing.** The pin above only ever stopped `evict_if_needed` — a
concurrent `invalidate`/`invalidate_document` (`kj swap discard`, a flush-
failure rollback in `mount_backend.rs`, a binary write dropping a stale text
shadow) still dropped a pinned entry outright, pulling the buffer out from
under an open editor session. Both functions now refuse on a pinned entry
instead of removing it, naming the path and the number of sessions holding it
open. A caller in a rollback or best-effort position (mount_backend's
flush-failure and binary-write paths) treats the refusal as informational —
`tracing::warn!` and move on, since the entry surviving under a live session
is the correct outcome, not a failure of the caller's own operation.
`kj swap discard` on a path with an open editor session is the one
production-reachable case today, and it now reports the refusal instead of
silently orphaning the session's buffer.

**The rule-4 swap check must run before every mutation, cold cache or warm,
and on every mutation path — not just `create_or_replace`'s.** A kaibo
review of the fixes above found the guard was still incomplete in three
places:

- `create_or_replace` only checked `entry.swap_recovered` on an
  already-cached (warm) entry. On a cold cache — no entry at all, e.g. right
  after a restart — it fell through to the "doc already exists" fallback and
  spliced new content straight into the swap's block with no check
  whatsoever. `FileDocumentCache::is_unacknowledged_swap`/`refuse_if_swap_recovered`
  is the one check now: warm, it reads `entry.swap_recovered`; cold, it
  consults the durable `dirty_file_buffers` row directly — the same row
  `try_get_or_load`'s recovery arm reads to set that flag in the first
  place.
- The MCP `edit` tool's `apply_edit_plan` recovered a swap via `get_or_load`
  and then mutated the block directly (`store.edit_text`, bypassing
  `create_or_replace` entirely) before `flush_one`'s refusal ever ran — so
  the recovered content was already clobbered by the time the caller saw an
  error, and the failed-flush rollback's `invalidate_document` deleted the
  document outright, orphaning the `dirty_file_buffers` row. It now calls
  `refuse_if_swap_recovered` before its first `edit_text`, the same check
  `create_or_replace` runs.
- A discard rollback (`ZQ`/`Kernel::editor_quit`) rewrites the block via a
  raw `blocks.edit_text` call that bypasses the file cache's own
  dirty/flush bookkeeping entirely. If the rollback actually changed the
  block's content — reachable via `kj swap ack` flushing a *later* edit than
  the checkpoint being rolled back to — the entry could be left claiming
  `dirty: false` while the block no longer matched disk: unflushable
  (`flush_one` on a clean entry silently no-ops) and, on a cold restart,
  missing the `dirty_file_buffers` row that content would need to be
  recovered as a swap rather than quietly lost. `editor_quit` now marks the
  entry dirty whenever `EditorSessions::quit` reports a content-changing
  rollback.

## Tool surface: three removals

Decided by Amy 2026-08-18. Each removal deletes a hazard rather than guarding
it — the "permission to get simpler" stance in `AGENTS.md`.

### `write` is removed

`write` is `create_or_replace` with **no precondition at all**
(`file_tools/cache.rs`, `mcp/servers/file.rs:477`). It is the flush-back that
destroys a file, and it is the only call that can lose a whole file in one
shot. Meanwhile `edit`'s hashline mode reverifies a hash before writing, so
"a stale edit fails loud instead of corrupting". The careful path was guarded
and the blunt one was not.

`shell_write` can already touch files, and it is **gated by the approval
ledger** while the file tools are not — so removing `write` also closes a
standing gate bypass (a model stopped from `echo x > file` could call `write`
and go straight through).

A **`create_file`** may replace it: a create has no prior state to clobber, so
it carries none of the risk. It must fail loudly if the path exists rather than
falling back to replace.

### `edit` becomes hashline-only

Two modes exist today: string mode (`old_string` → `new_string`, whitespace
exact) and hashline mode (`anchor` as `N:hash` or `N:hash..M:hash`, reverified
before writing).

Hashline is kept because **the anchor hash is a per-line changed-under-us
check** — the same guard as rule 3, at line granularity. String mode has no
such check. Ranges (`N:hash..M:hash`) already cover multi-line replacement, and
an empty `new_string` deletes, so nothing is lost but the unguarded path.

The cost is a required `read` before an edit. That is the correct discipline,
and it is what makes rule 3 enforceable.

### `grep` is removed

The MCP `grep` tool is **broken**: it returned "No matches found" for a pattern
that exists in the file, because it was searching a two-month-old copy. Its one
distinguishing property was being document-aware ("sees unflushed edits"), which
is worth exactly nothing when the document is stale.

kaish's shell `grep` works. Remove the tool; rewrite it later if a real need
appears. If it comes back, it should emit hashline anchors so that
`grep` → `edit` composes without a separate `read` — that is the ergonomic
answer to hashline-only editing.

**Open, from the same conversation:** whether shell `cat`/`grep` should learn
hashline output. Attractive, with one caveat worth stating before anyone builds
it — hashline prefixes are metadata, so making them the default would corrupt
every existing pipeline. It needs a flag, not a new default.

## Wire: no new RPC

The swap announcement is a **fact about a call that already exists**, so it
belongs in that call's return value.

All four editor methods already return `EditorState`, as does the
`subscribeEditor` push channel:

```capnp
editorOpen  @74 -> (state :EditorState)
editorKeys  @75 -> (state :EditorState)
editorState @76 -> (state :EditorState)
editorSave  @77 -> (state :EditorState)
```

`EditorState` already carries `dirty @4` and `message @6` — "transient
status/error line (vim E492)", which is exactly the channel vim announces a
swap file on. Two additive fields finish it:

- `swapRecovered @7 :Bool` — this buffer came back with unsaved changes rather
  than from disk
- `diskChangedSinceLoad @8 :Bool` — the W12 condition; what `:w` refuses on and
  `:w!` overrides

A struct field is additive and backward-compatible. A new interface method
spends an ordinal permanently ("Ordinals are dense and permanent... retiring a
method leaves a `retiredNN @NN ();` stub") and a wire change now requires
rebuilding **five** kernel-binding binaries, one of which fails silently when
it is stale. The rule in `AGENTS.md` decides it independently: administration is
a `kj` verb, chatty paths are RPC, and this is neither — it is a return value.

Model-facing callers need no wire at all. An agent opening via `vi` or reading
via a file tool gets a tool *result*; the announcement rides that text.

## Slices

1. **Load path.** On miss, always reconcile against disk: when the document
   exists, replace its block content with the text just read instead of
   discarding it. Match the specific `DocumentAlreadyExists` variant — `Err(_)`
   currently swallows the kind, so a real store failure reads as "already
   exists". Also stop letting `dirty` short-circuit the staleness check before
   it is reached (`cache.rs:255`).
2. **Swap semantics.** Persist only dirty buffers as recoverable. A clean entry
   is a cache, discarded and re-read. Announce a recovery; do not serve it as
   authoritative until acknowledged.

   The cold path cannot tell a swap from a stale cache without a durable
   marker, and slice 1 reconciles both — so until this lands, unsaved work is
   discarded on restart rather than silently served. **A KernelDb row carries
   the marker** (Amy, 2026-08-19): one normalized row per path holding the
   dirty flag and `loaded_generation`. A dirty row means announce; no row
   means reconcile. Slice 3 needs a restart-surviving generation anyway.

   The eventual model is lazy document creation — a clean buffer never becomes
   a document, so a document existing *means* unsaved work, with no marker at
   all. Deferred for cost (`block_id` becomes optional across every caller) and
   filed in `docs/issues.md`; the row goes away if it lands.
3. **`:w` guard.** Implement W12 — `:w` refuses when the disk generation moved
   past the load generation, `:w!` overrides. Retires "`:w!` == `:w` today".
4. **Tool surface.** Remove `write` and `grep`; make `edit` hashline-only; add
   `create_file` if wanted. Update `docs/kj-help/` and every published `///`.
5. **Wire fields.** `swapRecovered` / `diskChangedSinceLoad` on `EditorState`,
   and the renderer work to show them.

Slice 1 is the containment and should land alone, first. Slices 2–3 make it
correct; 4 removes the remaining ways to trip it; 5 is UI.

## Testing

The unit suite could not have caught this — it exercised the cache without a
durable document surviving across a load. Two tests are non-negotiable:

- A document that already exists in the block store with **stale** content, plus
  a newer file on disk, must serve the disk content. This is the incident,
  reduced.
- A flush after the disk moved under the buffer must **refuse**, and `:w!` must
  override. Assert on typed outcomes, not on message substrings.

Falsify both against the current code first. Both should fail today.

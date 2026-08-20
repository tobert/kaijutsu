# Editor + file-I/O tech-debt audit (2026-08-20)

Read-only audit of the lane that churned on 2026-08-18/19. No edits, no git
mutations. `cargo check -p kaijutsu-kernel --all-targets` is clean at
`7f3ab694`; every claim below is from reading the current source.

Known-open items from the brief (`edit`'s hashline-while-dirty hole, E325
announce-on-open, slices 4+5, kaish 0.15.1's `edit` builtin) are **not**
re-reported. Where the code makes one of them worse, it is named as such.

---

## BUGS

### B1. `:w` on `/etc/client/*` or `/etc/midi/*` reverts the edit it just saved

`crates/kaijutsu-kernel/src/editor.rs:64-66` — `config_owned()` is
`is_rc_path || is_config_path`, i.e. `/etc/rc` and `/etc/config` **only**.
But four trees are mounted with `ConfigDocFs`
(`kaijutsu-server/src/rpc.rs:1702,1720`; `kj/mod.rs:1046,1055`): rc, config,
**client, midi**. `ConfigDocFs::owns_config_docs() == true` for all four
(`runtime/config_doc_fs.rs:731`), so `resolve_editor_target`
(`editor.rs:83-107`) binds a `/etc/client/x.toml` session to the **config
block**, exactly like an rc script.

Then `EditorSessions::file_backed_path` (`editor.rs:699-702`) asks
`config_owned` instead, gets `false`, and hands the path to
`Kernel::editor_keys` (`kernel.rs:1424`) as a *file-backed* session:

1. `mark_dirty("/etc/client/x.toml")` + `flush_one_guarded(...)` run against
   the `FileDocumentCache` **shadow** doc (`file_context_id(path)`), which is
   a different document from the block the editor is writing.
2. `Kernel::invalidate_config_file_cache` (`kernel.rs:1634`) is gated on the
   same `config_owned`, so the shadow is **never invalidated** for these two
   trees — it still holds pre-edit text.
3. `flush_one` reads that pre-edit text and `vfs.write_all`s it →
   `ConfigDocFs::write_all` → **overwrites the just-edited config block with
   the stale shadow content.** The reconciler then pushes the reverted text
   back into the open session.

`ConfigDocFs` is writable (`read_only() == false`), so `MountBackend::read`
(`mount_backend.rs:268`) mints that shadow on any `cat /etc/client/x.toml` —
step 3's precondition is one shell read away. With no shadow resident, `:w`
instead silently no-ops (see B2) and reports success.

This is precisely the hazard `file_backed_path`'s own doc warns about
("flushing that shadow would write the wrong content to the wrong place"); the
guard just uses the wrong predicate. `/etc/client` and `/etc/midi` are declared
ordinary write surfaces (CLAUDE.md; `file_tools/path.rs:103-123` lets them
through `deny_etc_write`), so editing them is an expected action.

**Fix:** make the file-backed decision ask the mount table
(`owns_config_docs`), the same authority `resolve_editor_target` uses — or, if
the sync constraint on `invalidate_config_file_cache` must hold, at minimum add
`is_client_path || is_midi_path` to `config_owned` and rename it. Size M.
Risk: `config_owned` is also the rc-write-adjacent predicate; changing it
touches `invalidate_config_file_cache` and `run_commands`' checkpoint branch,
so all three call sites need re-reading together (see D1.5).

### B2. A `:w` whose buffer was evicted reports success and writes nothing

`FileDocumentCache::mark_dirty` (`cache.rs:644-664`) is a **silent no-op** when
the path has no cache entry (`if let Some(...)` — no else, `Ok(())`), and
`flush_one` (`cache.rs:757-771`) returns `Ok(())` for `None => // not cached`.
`flush_one_guarded` (`cache.rs:846-849`) forwards a missing entry to
`flush_one`, so it too returns `Ok`.

An editor session holds no reference in the cache. `evict_if_needed`
(`cache.rs:977-999`) drops the oldest **clean** entry once 64 entries are
resident — and every MCP `read`/`grep` and every kaish `cat` inserts one. So:
open `vi foo.rs` (clean entry), let 64 other files get read, type, `:w`:

- `mark_dirty` → no entry → no swap row, no error.
- `flush_one_guarded` → no entry → `Ok(())`.
- `kernel.rs:1439` → `sessions.save(id)` → checkpoint advances, `dirty` clears.

The player is told the file was saved. Disk never changed and there is no swap
row. Worse, the next `get_or_load` on that path sees no dirty row, reconciles
the block against disk (`cache.rs:521`), and the block flow pushes disk content
into the live session — the unsaved work is destroyed while the editor is open.

**Fix:** `mark_dirty` and `flush_one` must distinguish "nothing to flush" from
"no buffer here" — load-or-fail rather than `Ok(())`. This is the "silent
fallbacks are a mistake" rule; a `FlushError::NotBuffered` (or a `mark_dirty`
that loads the entry) makes it loud. Size S–M. Risk: `flush_one`'s
`Ok(())`-on-miss is relied on by `mount_backend`'s write path only *after* a
`create_or_replace` that guarantees an entry, so tightening it is safe there.

`docs/issues.md` "The file cache's size limit is a suggestion" covers eviction
from the opposite direction (unbounded growth); this is the other edge of the
same knife and is not in that entry.

### B3. A write onto a recovered swap destroys the swap, then refuses

`create_or_replace` (`cache.rs:555-598`) replaces a cached entry's block text
with **no `swap_recovered` check**. `MountBackend::write`
(`mount_backend.rs:334-347`) and the MCP `write` tool (`file.rs:508-537`) both
do `create_or_replace` → `mark_dirty` → `flush_one`. On a path recovered as a
swap, that sequence overwrites the unsaved work in the block store *first* and
only then hits `flush_one`'s `UnacknowledgedSwap` refusal
(`cache.rs:762-766`). The rollback (`invalidate` / `invalidate_document`)
drops the entry or the doc but **never clears the `dirty_file_buffers` row** —
`clear_dirty_file_buffer` has exactly two callers, `flush_dirty` and
`flush_one` (`cache.rs:703,797`). Net result: the recovered work is gone, the
row survives, and `/v/swap/<kernel>/<path>` keeps advertising a swap whose
content is now either the failed write's text or (after the doc is re-created
from disk) plain disk bytes labelled as unsaved work.

**Fix:** refuse in `create_or_replace` when `swap_recovered` (fail before
mutating), and clear the row on the rollback paths. Size S. Risk: the
`editor_keys` rollback deliberately *keeps* the row (`kernel.rs:1427-1436`) —
that reasoning is sound and must not be swept into this change.

### B4. Nothing acknowledges a swap, so the rule-4 refusal is terminal

`acknowledge_swap` (`cache.rs:880-886`) has **no production caller** — only
`cache.rs` tests. `swap_recovered()` likewise. So once a path comes back as a
recovered swap, every `:w`, every shell write and every MCP write on it fails
forever with:

> `E212: Can't open file for writing: <path>: <path>: recovered from a swap
> after a cold cache and not yet acknowledged — call acknowledge_swap before
> flushing (docs/file-buffers.md)`

Two problems beyond "the announcement is missing" (which `docs/issues.md`
already tracks): there is no acknowledgment *surface at all*, so enforcement is
a dead end rather than a speed bump; and the published status line names an
internal method and a repo path, which the writing-style rule forbids ("must
not leak internals... an internal module path is unresolvable to its reader").
`flush_error_message` (`kernel.rs:223-230`) also files this under **E212 "Can't
open file for writing"**, which is the wrong vim code — nothing failed to open.

**Fix:** either give rule 4 an acknowledgment verb (`kj editor` / a `:w!!`-ish
override / the slice-4 tool) in the same change, or hold the refusal until
slice 5 ships the announcement. Add a third arm to `flush_error_message` with
player-facing wording. Size S (message) + M (surface). Risk: removing the
refusal re-opens the silent-overwrite hole rule 4 exists for — pair it with the
announcement, don't just delete it.

---

## 1. Two mechanisms for one thing

**1.1 Two `PatchOp` implementations, one hardened.** `M`
`runtime/mount_backend.rs:423-546` applies patch ops inline; `runtime/kaish_backend.rs:841-954`
(`compute_patch_op`) does the same job with `check_byte_boundary`
(`kaish_backend.rs:814`) in front of every byte index. `mount_backend`'s copy
indexes `&text[*offset..end]` and `text.replace_range(*offset..end, ...)` with
no boundary check — a mid-char offset **panics** instead of failing loud, the
exact defect `3cb3ed4f` fixed on the other side. Its line ops also differ
(`split('\n')` + `join` vs `line_range` byte spans). Not reachable today:
kaish 0.15's `patch` and `sed` builtins both send a single whole-file
`Replace{offset:0, len: content.len()}` (verified in the cargo registry
sources), which is always on a boundary. It becomes reachable the moment any
caller sends a real offset — e.g. kaish 0.15.1's `edit` builtin.
**Action:** hoist `compute_patch_op` + `check_byte_boundary` into one shared
helper and have both backends fold through it. Risk: the two differ in more
than hardening (line-op semantics); reconcile deliberately, with tests, rather
than deleting one.

**1.2 `flush_dirty` is a second flush path with no callers and no rule-4 guard.** `S`
`cache.rs:667-743`. Zero callers outside its own warn string (`cache.rs:994`).
It also lacks `flush_one`'s `swap_recovered` refusal, so wiring it up later
would bypass rule 4 silently. `docs/issues.md` notes the no-callers half;
the missing guard is new. **Action:** delete it (the "prefer deleting a
mechanism" stance), or if a shutdown flush is wanted, implement it as a loop
over `flush_one` so there is one flush body.

**1.3 Legacy `String`-error wrappers alongside the typed API.** `S`
`cache.rs:163-165` (`get_or_load`) and `cache.rs:297-301` (`read_content`) are
documented as "legacy wrapper" / "New call sites should prefer `try_*`" — but
production call sites still use them: `editor.rs:109`, `file.rs:584,589`,
`kernel.rs:1507`, `swap_filesystem.rs:148,204,284`. A doc comment that says
"don't use this" while eight callers do is not a deprecation, it is two APIs.
**Action:** convert the callers (each already has an error context to map
into) and delete the wrappers. Risk: `swap_filesystem` deliberately maps every
error to `io::Error::other`; keep that behavior explicit at the call site.

**1.4 The MCP permission ladder is written twice.** `S`
`mcp/servers/file.rs:225-243` (`edit`) and `:249-267` (`write`) are the same
four-branch rc-write / `deny_etc_write` / `check_write` ladder with a different
tail call. **Action:** one `authorize_write(&path, &tool_ctx) -> Option<ExecResult>`
helper. Risk: none; the branches are byte-identical apart from the final call.

**1.5 Config-ownership is decided by two mechanisms in three places.** `M`
The resolver asks the mount table (`editor.rs:83-85`, `owns_config_docs`); the
prefix predicate `config_owned` decides `file_backed_path` (`editor.rs:701`),
`run_commands`' checkpoint-deferral branch (`editor.rs:566-573`) **and**
`invalidate_config_file_cache` (`kernel.rs:1635`). `config_owned`'s own doc
(`editor.rs:55-63`) claims it "survives only as the **synchronous** guard for
`Kernel::invalidate_config_file_cache`" — that is no longer true, and the drift
it warns about has already happened (B1). `docs/issues.md` tracks the residual
sync-prefix item, but not that it grew two more consumers.
**Action:** fold into B1's fix; at minimum correct the doc to list all three.

**1.6 Two tests re-derive `file_context_id` by hand.** `S`
`mount_backend.rs:1116-1124` and `:1185-1190` inline the UUIDv5 formula.
`file_context_id` is `pub(crate)` (`cache.rs:1011`) and reachable. A test that
computes the key itself keeps passing if the real derivation changes.
**Action:** call `crate::file_tools::cache::file_context_id`.

## 2. Guards and branches for states that cannot occur

**2.1 `try_read_content`'s identity `map_err`.** `S` `cache.rs:315-318` maps
`NotCached => NotCached, Backend(m) => Backend(m)`. Pure noise; delete the
`map_err`. Zero risk.

**2.2 `reload_block_from_disk` clears a flag it can never see set.** `S`
`cache.rs:244-250`, with a comment saying so ("Unreachable with
`swap_recovered: true` today"). Deliberate belt-and-braces; the comment is
better than the code. **Action:** leave with the comment, or convert to a
`debug_assert!(!entry.swap_recovered)` so a future violation is loud rather
than silently corrected.

**2.3 `get_or_load_with_content`'s `Err(_)` catch-all.** `M` `cache.rs:929`
treats *every* `create_document` failure as "already exists" and falls into the
replace path. This is the exact pattern slice 1 fixed in the sibling function
(`docs/file-buffers.md`: "Match the specific `DocumentAlreadyExists` variant —
`Err(_)` currently swallows the kind"). The fix landed on `try_get_or_load`
(`cache.rs:448`) and not here. **Action:** match
`BlockStoreError::DocumentAlreadyExists` and propagate everything else.

**2.4 `SwapFilesystem::entries_with_sizes` returns an `io::Result` that is
never `Err`.** `S` `swap_filesystem.rs:133-157`. Also `check_kernel_segment`
(`:108-117`) guards a multi-kernel data dir that does not exist yet — cheap and
self-documented; leave it.

## 3. Stale names, comments, and docs

**3.1 `CommandRequest::Write`'s doc still says `:w!` is a no-op.** `S`
`kaijutsu-editor/src/lib.rs:72-77`: "`force` (`!`) is recorded but presently a
no-op distinction: `:w!` == `:w`. The intended future use is..." — W12 shipped
in `997bcc1a` and this is the bit that carries it. This is the most misleading
comment in the lane: it tells a reader the guard does not exist.

**3.2 `EditorCore::text()`'s "Tech debt (sweep before done)" is done.** `S`
`kaijutsu-editor/src/lib.rs:246-249` says terminator fidelity "must live in the
kernel binding (remember the loaded terminator, re-apply on save)" — which
`EditorSession.terminator` (`editor.rs:291-294`) does. Rewrite as a statement
of the split, not a TODO.

**3.3 `insert_at_cursor`'s doc names a caller that deliberately does not use
it.** `S` `kaijutsu-editor/src/lib.rs:192-200`: "Used by the kernel to complete
a `:r`". The kernel uses `insert_at` with the cursor captured at submit time,
on purpose (the wandering-cursor race). See 4.5.

**3.4 `ConfigCrdtFs` survives in `docs/vi.md`.** `S` lines 281 and 283 — the
type is `ConfigDocFs` since `76b9c23d`, and `65818b3f` claimed to eradicate the
CRDT vocabulary from this file. Two survivors.

**3.5 `docs/vi.md:137` lists `dirty()` as an `EditorCore` accessor.** `S` No
such method; dirtiness is computed kernel-side against the checkpoint
(`editor.rs:799-815`).

**3.6 `docs/vi.md:159` describes the old save order.** `S` "`editor_save` — flush
the document to its owner; advance the checkpoint" predates flush-then-
checkpoint-only-on-success and the W12 refusal (`kernel.rs:1587-1602`).

**3.7 `docs/file-buffers.md:49` contradicts its own slice list.** `S` The vim
table still says "`W12` changed-under-us | designed, **not implemented**" while
line 180 and the shipped code say otherwise.

**3.8 `docs/file-buffers.md:46` names `flush_dirty` as a `:w` mechanism.** `S`
It has no callers (see 1.2).

**3.9 `file_tools/mod.rs:1-31` documents a layer that does not exist.** `S`
The ASCII diagram's "File Tool Engines (read, edit, write, glob, grep)" box has
no counterpart in the module (`cache`, `guard`, `hashline`, `path`,
`vfs_walker`); the tools live in `mcp/servers/file.rs`. `path.rs:1` says the
same thing ("the MCP file engines").

**3.10 `path.rs:44` documents the cwd fallback that was deleted.** `S` "`cwd`
... defaults to `/` everywhere a context lacks one" is exactly what
`refuse_missing_cwd` (`file.rs:987-997`) exists to prevent, and the test at
`file.rs:1150` pins the opposite.

**3.11 `kj editor save`'s published help says `ZZ`.** `S` `kj/editor.rs:45`:
"Checkpoint the buffer as saved (`ZZ`); for a file, also flush to disk". `ZZ`
writes *and quits*; `kj editor save` leaves the session open — it is `:w`. Same
wording in `kernel.rs:1561`. Published text a model reads.

**3.12 `kj editor save` prints "saved" on a failed flush.** `S`
`kj/editor.rs:148-152` emits `session N: saved — W12: ...`. Lead with the
consequence: it was not saved.

**3.13 Two `kernel_db` doc comments name callers that do not exist.** `S`
`kernel_db.rs:5632` ("or the edit is discarded" — no discard path calls
`clear_dirty_file_buffer`) and `:5655` ("for the cold-start sweep that must
announce each one" — the only consumer is `SwapFilesystem`).

**3.14 `docs/issues.md:31` says `list_dirty_file_buffers` has no consumer.** `S`
`swap_filesystem.rs:84` calls it — three lines after the same entry announces
`/v/swap` shipped.

**3.15 Rotting line references.** `S` `docs/issues.md:85`
(`file_tools/cache.rs:726-748` → now 977-999) and `docs/file-buffers.md:164`
(`cache.rs:255` → now ~355). `docs/vi.md:425` already states the rule ("Line
numbers drift — grep the symbol"); apply it in the other two files.

**3.16 `SwapFilesystem::real_path` names two different things.** `S`
`swap_filesystem.rs:121` (associated fn: components → the mirrored real path)
and `:329` (trait method: always `None`). One term, two meanings, in one file.
Rename the associated one `mirrored_path`.

## 4. Threaded-but-unused / vestigial

**4.1 `apply_edit_plan`'s `_tool_ctx` is unused.** `S` `file.rs:549` takes
`_tool_ctx: &ExecContext` and never reads it; both call sites pass it. Delete
the parameter.

**4.2 `dirty_file_buffers.context_id` is written and never read.** `S`
`kernel_db.rs:1249-1256`, `:5610-5628`. No consumer reads
`DirtyFileBuffer.context_id` (`cache.rs` uses only `loaded_generation`;
`swap_filesystem` uses `path` and `dirtied_at`). It is derivable from `path`
via `file_context_id`, so it is a second source of truth for the same mapping
that can disagree with it. **Action:** drop the column, or document it as a
debugging breadcrumb and note that `file_context_id(path)` is authoritative.

**4.3 `FileDocumentCache::disk_changed_since_load()` has no non-test caller.** `S`
`cache.rs:258-265`. `flush_one_guarded` reads the field directly
(`cache.rs:844`). It is presumably a slice-5 seam — say so in the doc, or
delete until slice 5 needs it.

**4.4 `swap_recovered()` / `acknowledge_swap()` likewise.** `S` See B4. Keep,
but the doc should name the surface that will call them.

**4.5 `EditorCore::insert_at_cursor` has no callers at all.** `S`
`kaijutsu-editor/src/lib.rs:197-200`. Delete it; `insert_at` is the one the
kernel needs and the race note lives there.

**4.6 `max_cached` is a field with no setter.** `S` `cache.rs:20,152`.
`DEFAULT_MAX_CACHED` is named for an override that does not exist. Either make
it configurable (it is a real operational knob — see B2) or rename to
`MAX_CACHED` and drop the field.

**4.7 Dead statement in a test.** `S` `file.rs:1417`:
`let _ = (&store, DocumentKind::File);` — an import-keeper left behind.

**4.8 `state_json` is a one-line pass-through.** `S` `kj/editor.rs:62-64`
wraps `st.to_json(id)`; four call sites. Inline it — the "one shape" comment
belongs on `EditorState::to_json`, where it already is.

## 5. Test quality

**5.1 The E212 assertion cannot distinguish the two failures it must.** `M`
`kernel.rs:2737` asserts `st.message.starts_with("E212")`. `flush_error_message`
(`kernel.rs:228`) funnels **both** `FlushError::Backend` and
`FlushError::UnacknowledgedSwap` into that prefix, so the test passes if a
read-only-mount failure is silently replaced by an unacknowledged-swap refusal.
That is the substring-assert failure mode `FlushError`'s own doc calls out
("a caller matching on a message substring is a test that passes for the wrong
reason", `cache.rs:50-52`) — typed at the cache layer, untyped again at the
assertion. Fixing B4's message (a distinct third arm) also fixes this.

**5.2 `EditorSessions`' one path-kind branch is never exercised in its own
tests.** `M` Every test in `editor.rs`'s `session_tests` uses
`RC_PATH = "/etc/rc/coder/create/S00.kai"`, so `run_commands`' `if is_config`
(`editor.rs:565-574`) always takes the config arm. `colon_w_saves_and_stays_open`
(`:1336`) asserts `!dirty` after `:w`, which is *only* true for config — the
file-backed contract is the opposite (`state` stays dirty until the kernel
flushes). Coverage exists at the kernel layer (`kernel.rs:2497+`), but the pure
registry's own branch has no test that would fail if it inverted.
**Action:** add one `session_tests` case on a non-config path asserting `saved
== true && state.dirty == true`.

**5.3 Hand-rolled `file_context_id` in tests.** `S` See 1.6 — a test that
cannot fail when the derivation changes.

**5.4 Leftover manual cleanup after a RAII temp dir.** `S`
`mount_backend.rs:989`: `std::fs::remove_dir_all(dir).ok();` after
`tempfile::tempdir()`, whose comment two lines up explains why RAII replaced
exactly this. Also `:983` asserts only `w.is_err()` where the sibling MCP test
(`file.rs:1742`) checks the refusal names the read-only mount.

**5.5 `test_tool_dispatch_not_found` builds two backends.** `S`
`mount_backend.rs:867-876` — one is only used to construct the `ExecContext`,
the other answers the call ("Re-create for the call"). Harmless, but it reads
as a workaround for a borrow that no longer exists.

**5.6 The unreadable-swap listing path is untested.** `S`
`swap_filesystem.rs:146-153` maps a failed read to `size: 0`. Nothing exercises
it, so the "visible but sized 0" contract is a comment only.

**5.7 Vim-dialect substring asserts.** `S` `editor.rs:1263,1307,1374,1443` and
several in `file.rs` match on message substrings. Unlike 5.1 these assert on
text that *is* the product (E37/E492 status lines) and there is no typed value
at that layer — acceptable. Listed so the next auditor does not "fix" them.

---

## Verified NOT debt (do not re-litigate)

- **`flush_one_guarded` as a separate method rather than a `force` param on
  `flush_one`** (`cache.rs:818-833`). The rationale is real and specific: a
  whole-file VFS overwrite means "I do not care what is there", and
  `create_or_replace` does not re-stamp `loaded_generation`, so guarding that
  path would refuse every `echo x > file` onto an externally-edited file.
- **The sticky `already_flagged ||` in the W12 predicate** (`cache.rs:859-860`).
  Not redundant with the live check: `FileAttr::generation` can step *backward*
  on `LocalBackend`, so a live-only comparison can lose a change the read path
  already observed.
- **Both defenses on the read-only write path** (`file.rs:494` `is_writable`
  gate + `file.rs:526` `invalidate_document` rollback). Mutation-verified
  2026-08-10 (`file.rs:1722-1727`): disabling either alone still passes,
  disabling both fails. Keep both.
- **`line_to_byte_offset` kept local instead of shared with
  `block_tools/translate`** (`kaish_backend.rs:956-967`). 1-indexed and
  clamping vs 0-indexed and erroring — sharing would silently change the kaish
  patch contract.
- **`MountBackend::raw_write` / `raw_append`** (`mount_backend.rs:121-165`).
  Not a duplicate write path: the deliberate binary / read-only-mount bypass
  that keeps un-flushable content out of the cache.
- **`config_owned` being a lexical prefix check** (`editor.rs:64`). The sync
  requirement on `editor_quit`'s invalidation path is real; it is the *doc* and
  the *coverage* (rc+config only) that are wrong, not the technique — see B1.
- **`flush_error_message` as a single source for three call sites**
  (`kernel.rs:215-230`). Exactly right; it just needs a third arm.
- **`check_byte_boundary`** (`kaish_backend.rs:814`). Load-bearing; the reason
  it exists is written down and it is the model for 1.1.
- **Skipping eviction when every entry is dirty** (`cache.rs:988-997`). A
  deliberate soft cap; already tracked in `docs/issues.md`.
- **`WorkspaceGuard`'s fail-open on a DB error** (`guard.rs:142-145,165-167`).
  Defensible under "capabilities are ergonomic nudges, not security" — but note
  it sits against the `feedback_db_errors_p1` posture. Worth one sentence in
  the doc comment saying the fail-open is a stance choice, not an oversight.

## Suggested order of work

1. **B1** — reachable data loss on two declared write surfaces.
2. **B2** — silent "saved" that saves nothing; likely in any long session.
3. **B4 message + B3 refusal ordering** — small, and B4's message fix also
   fixes 5.1.
4. **1.1** — before kaish 0.15.1's `edit` builtin can reach the unhardened copy.
5. **1.2, 1.3, 2.1, 2.3, 4.1, 4.5, 4.7** — deletions, all small, no behavior
   change.
6. **Group 3** — a single doc/comment sweep; 3.1 first (it actively misleads).
7. **5.2** — the one missing test with a real contract behind it.

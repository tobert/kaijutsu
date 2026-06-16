# CRDT-Owned Config — Design

The CRDT becomes the **sole owner** of config and rc scripts. Embedded Rust
source (compiled into the binary, visible in-repo) is the **seed**; after that,
the CRDT owns the content. No host-disk write-through, no reload-from-host, no
mtime staleness. Project repo source files are **not** in scope — there the host
disk is the truth (cargo/git/editor read real files) and write-through stays.

Status: **direction LOCKED 2026-06-16** (decided with Amy). No code yet. This doc
is the durable record; slices below are the build order.

---

## Why — dual ownership is the disease

Today an rc script lives *twice*: as a host file under `~/.config/kaijutsu/rc/`
**and** as a CRDT doc, with `FileDocumentCache` running a bidirectional sync
between them (write-through flush on edit + `reload_block_from_disk` on mtime
advance). That sync is the **single contributing factor** behind a whole cluster
of silent-fallback bugs we kept patching as symptoms:

- `mount_backend::read` serving **stale disk bytes** on a CRDT error (issues.md).
- `mount_backend::append` **wiping a file** to just the appended suffix via
  `unwrap_or_default()` on a CRDT error (the F1 finding, commit `04ce36e`).
- The same wipe surviving through the **stale-reload** path (`reload_block_from_disk`
  → `NotCached`) — deepseek F1, HIGH.
- `LocalBackend::setattr` mtime being a **no-op** while mtime is load-bearing for
  staleness detection (issues.md arch sweep).
- The **stale rc seed** class — a pre-existing host file never auto-upgrades and
  silently drifts behind the embedded default (issues.md, recurred for
  coder/genesis/mcp).

Every one of these is a symptom of "is the host file or the CRDT the truth?"
**Delete the dual ownership and the bug class is gone by construction — not
patched.** `reload_block_from_disk` (where F1 lives) exists *only* to pick up
external host edits; for CRDT-owned content there are no external host edits, so
the function — and the flush, and the mtime races — simply don't exist to fail.

This is contributing-factors thinking, not root-cause: we remove the structure
that *necessitates* the fragile sync, rather than hardening the sync.

---

## The seam already exists — and so does the target backend

The cache classifies files by **mount point → backend**, not by any file-class
logic. There are already three pipelines, and one is exactly the target:

| Pipeline | Backs | Ownership | Host disk |
|---|---|---|---|
| `FileDocumentCache` + `LocalBackend` | `/etc/rc` **and** `~/src`, `/tmp` | host-truth, write-through + reload | yes — the fragile sync |
| `ConfigCrdtBackend` | theme/models/mcp `.toml` | CRDT-truth, **debounced flush to host** | yes (flush only) |
| `KaijutsuFilesystem` (`/v/docs`) | CRDT docs | **CRDT-truth, no flush, no mtime** | **no** (`real_path()→None`) |

`KaijutsuFilesystem` over `KaijutsuBackend`
(`crates/kaijutsu-kernel/src/runtime/docs_filesystem.rs`,
`runtime/kaish_backend.rs`) is the pattern to extend: file ops → `BlockStore`
(CRDT, persisted in `kernel.db`), `read`/`write`/`list` all supported
(`kaish_backend.rs:458` lists docs), `read_only()=false`, `real_path()→None`. So
this refactor is **not new machinery** — it is **changing which backend a mount
uses**, plus seeding into the CRDT instead of onto host disk.

The classification lives in the mount table (`vfs/mount.rs`, frozen at boot,
`rpc.rs:1019–1084`): `/etc/rc` currently mounts `LocalBackend::new(rc_dir)`.
Point it at a CRDT-native backend and rc stops being a host mount.

---

## Scope

**In scope (this round):**
- **rc scripts** (`/etc/rc/*`) — slice 1, the core fail-loud win.
- **Config TOMLs** (theme/models/mcp via `ConfigCrdtBackend`) — slice 2. Already
  CRDT-truth, but drop the debounced host flush so the rule is uniform.

**Deferred:**
- **CRDT scratch mount** for agent working files (a writable CRDT-native mount,
  no host backing — the `/v/docs` pattern). Design once the rc generalization is
  proven.
- **Project-source residual** — `~/src`, `/tmp` keep `LocalBackend` write-through
  + reload (external tools read real files). The F1 fail-loud fix still applies
  *there*, much smaller and clearly justified, because that mount genuinely needs
  reload-on-external-edit. Track separately.

**Out of scope, permanently:** making the CRDT authoritative for project repo
source. Host disk is the truth there; an export-on-write sync would reintroduce
the very dual ownership we are deleting.

---

## The doc model — DECIDED: one unified CRDT-config store

Both existing CRDT-config pipelines already converge on the same model, so this is
settled by precedent, not invented:

- `ConfigCrdtBackend`: `config_context_id(path)` = `UUIDv5("kaijutsu:config:{path}")`
  → one `DocKind::Config` doc per path, seeded from `include_str!`
  (`config_backend.rs:28-55`).
- `FileDocumentCache`: `file_context_id(path)` = `UUIDv5("kaijutsu:file:{path}")`
  → one doc, one block.

**Decision:** rc and config TOMLs now have *identical* ownership, so they share
**one unified CRDT-config backend** (generalize `ConfigCrdtBackend`) owning both
the `/etc/rc/*` tree and the TOMLs. Mapping: **`UUIDv5(path) → single-block
`DocKind::Config` doc**, hierarchical paths, one seed mechanism, one edit surface.
(Avoids two near-identical CRDT-config stores — the duplication smell we just
fought in `get_or_load`, one level up.)

**Enumeration (the one thing host-backing gave free):** an opaque-UUID-per-path
scheme can't reconstruct a directory listing, and `load_rc_scripts` needs
`readdir /etc/rc/<type>/<verb>`. So the store carries a **path manifest** — seeded
from the embedded `include_dir!` tree, mutated by `kj rc add`/`rm` — that backs
prefix-readdir. Manifest as a CRDT doc or a `kernel.db` index (implementer's call;
both persist and both give SQL/scan prefix queries).

---

## Slice 1 — build the unified store, migrate rc

1. **Generalize `ConfigCrdtBackend`** into the unified CRDT-config backend:
   `UUIDv5(path) → DocKind::Config` doc over hierarchical paths, plus the path
   manifest for readdir. (Config TOMLs keep working through it unchanged for now;
   slice 2 drops their host flush.)
2. **Seed rc from embedded into the CRDT.** `assets/defaults/rc/` is already
   compiled in via `include_dir!` (`seed_scripts.rs:54`). Replace
   `ensure_rc_seed_files` (embedded → host disk, `seed_scripts.rs:122`) with
   embedded → CRDT docs + manifest. The "fresh" gate becomes "CRDT rc namespace
   empty," not "host dir empty."
3. **Remount `/etc/rc`** on the unified backend instead of `LocalBackend`
   (`rpc.rs:1083`). The host `~/.config/kaijutsu/rc/` dir is abandoned.
4. **Retarget `kj rc`** (`kj/rc.rs`): `show`/`list` read the CRDT; `reset`/`reseed`
   re-seed the CRDT doc from embedded; **`add`/`edit` write via a new
   `kj rc set <path> <content>`** (direct CRDT write). `write_rc_file`'s
   `flush_one` host write is removed.
5. **Lifecycle read** (`lifecycle.rs:261` `load_rc_scripts`): `readdir` +
   `try_read_content` route to the unified backend (manifest readdir + per-path
   doc read). Stays fatal-on-error — but now there is no host-vs-CRDT ambiguity to
   misclassify.

What this **deletes** for rc: write-through flush, `reload_block_from_disk`, mtime
staleness, and therefore F1/F3/stale-bytes/wipe/stale-seed — for the rc mount, by
construction.

### Cutover — hard reset
Fresh boot seeds the CRDT rc namespace from embedded defaults and ignores any
existing host `~/.config/kaijutsu/rc/`. Hand-edits in that host dir are **not**
migrated (decided: hard reset — the embedded defaults are canonical and in-repo;
real customization lands in `assets/defaults/rc/`).

### rc/config editing — DECIDED: `kj rc set` only for now
- **`kj rc set <path> <content>`** (and the config equivalent) is the write path:
  a direct CRDT write, scriptable.
- The `$EDITOR`-on-host-file affordance **retires** — there is no host file to
  `vim`. **CLAUDE.md must be updated** (it currently says "Edit a live script with
  `kj rc edit …` (or just `vim` the file)").
- Dev workflow for content changes: edit `assets/defaults/rc/` (in-repo, the
  canonical seed) + `kj rc reseed`, or `kj rc set` for a live one-off.
- **Deferred:** interactive app-editor-on-doc (open the rc/config CRDT doc in the
  app's editor surface) — a later slice, real UI work, not needed to ship the
  ownership change.

---

## Slice 2 — config TOMLs drop the host flush

The TOMLs already live in the (now-unified) backend; this slice removes their
host-disk coupling to match rc:
- Drop the debounced host flush (`config_backend.rs:316–320` write path + the
  debounce timer).
- Embedded seeds the CRDT once; CRDT owns thereafter; no host read-back on startup.
- Editing via `kj` set / reseed (parallel to rc); same hard-reset cutover.

(One rule for all config; loses the vim-the-toml affordance, parallel to retiring
vim-the-rc-file.)

---

## Open implementation questions (remaining)

1. **Durability + reseed semantics.** CRDT rc/config lives in `kernel.db`; confirm
   `kj rc reseed` (CRDT ← embedded) and the staleness-vs-embedded story (the
   issues.md "staleness indicator" want partly dissolves — drift is now
   CRDT-vs-embedded, surfaced by an explicit reseed, not silent host drift).
2. **Manifest storage** — CRDT doc vs `kernel.db` index (implementer's call; note
   the choice when it lands).
3. **Project-source F1 residual.** ~~Still real for `~/src`/`/tmp`~~ — RESOLVED:
   `reload_block_from_disk` now returns typed `CacheReadError` (Backend on store
   failure, NotCached only for removed/binary), `get_or_load` delegates to
   `try_get_or_load` (F3 duplication gone), and a stale-reload regression test
   guards it (F5). The fail-loud chain is now end-to-end for the host-truth mounts.

---

## What this resolves in issues.md

On landing, delete/rewrite: the `MountBackend::read` stale-bytes fallback, the
`append` wipe, the stale-rc-seed entries, and the `LocalBackend::setattr` mtime
no-op (no longer load-bearing for the CRDT mounts). The deepseek F1/F3/F5/F6
findings against commit `04ce36e` are superseded for rc (code path deleted) and
reduced to the project-source residual.

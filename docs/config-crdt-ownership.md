# CRDT-Owned Config — Design

The CRDT becomes the **sole owner** of config and rc scripts. Embedded Rust
source (compiled into the binary, visible in-repo) is the **seed**; after that,
the CRDT owns the content. No host-disk write-through, no reload-from-host, no
mtime staleness. Project repo source files are **not** in scope — there the host
disk is the truth (cargo/git/editor read real files) and write-through stays.

Status: **SHIPPED** — slice 1 (rc) landed 2026-06-16, slice 2 (config TOMLs)
2026-06-17. This doc is the durable design record; the slices below were the
build order. It supersedes the earlier `FileDocumentCache`-era design
(`docs/rc-crdt-vfs-bridge.md`, deleted 2026-07-04 — git history has it).

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
prefix-readdir.

**Manifest = the existing `documents` table (no new table).** DECIDED 2026-06-16
after reading the schema: `documents(document_id, workspace_id, doc_kind, path,
…)` already has a `path` column, a `UNIQUE(workspace_id, path)` index, and
`list_documents_by_kind`. `BlockStore::create_document` already writes a
`documents` row — today with `path: None`. So config/rc docs just need to carry
their path (a `create_document_with_path` variant), and `readdir` is
`list_documents_by_kind(Config)` filtered by the `/etc/rc/…` prefix, deriving
immediate children in Rust. The doc and its manifest row are written together by
the same `create_document` call → no separate-table dual-write to drift (the
manifest *is* the document registry).

**Read-path routing & cache coherence.** rc reads/writes route through the VFS
(`MountTable → ConfigCrdtFs`) **directly**, bypassing `FileDocumentCache` for the
kernel-internal callers (`kj rc`, `load_rc_scripts`) — mirroring a CRDT into
another CRDT is the exact sync we're deleting. The one remaining cache consumer is
an agent `builtin.file:read /etc/rc/…`; to keep that coherent without a host file,
`ConfigCrdtFs` returns an **in-memory advancing mtime** from `getattr` (bumped on
write), so the cache's existing staleness check reloads after a `kj rc set`. This
mtime is a *version stamp on the single source of truth*, not a host-vs-CRDT sync
— the "which is truth?" bug class stays gone by construction. (Future cleanup,
issues.md: teach `FileDocumentCache` to pass through CRDT-native mounts entirely.)

---

## Slice 1 — build the unified store, migrate rc — ✅ SHIPPED 2026-06-16

Landed across `debfb33` (foundation: `ConfigCrdtFs` + `config_doc` shared model +
`documents`-table manifest), `2b763c6` (seeding), `49c819a` (kj rc + load_rc_scripts
VFS-direct), `a2c1045` (production cutover at `rpc.rs` + CLAUDE.md). The CRDT is now
the sole owner of `/etc/rc`. **Live verification on the GPU runner (restart the
server, create a coder context, confirm the stance loads) is still pending — needs
a `systemctl --user restart` an agent shell can't do.** Build steps below are the
as-built record.



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

## Slice 2 — config TOMLs converge onto `ConfigCrdtFs` — ✅ SHIPPED 2026-06-17

Landed across `93c72a7` (config_seed module + generalized seeding), `fdd1c18`
(the /etc/config mount + reader re-point + ConfigCrdtBackend deletion + RPC
reshape), `9e581aa` (kj config + config-write capability), `a30b266` (app theme
over RPC), and `6f2ce9f` (config_dir revived as a one-time seed source for the
test harnesses). The CRDT is now the sole owner of all config (theme/models/
mcp.toml + system.md). **Live verification on the GPU runner (restart the
server, confirm the app themes over RPC and models.toml drives a turn) is still
pending — needs a `systemctl --user restart` an agent shell can't do**, same as
slice 1. The build order below is the as-built record.

**Decided 2026-06-17 (with Amy):** *converge* — delete `ConfigCrdtBackend`
entirely and mount a second `ConfigCrdtFs` (the slice-1 VfsOps backend, already
written generic for "the `/etc/rc` tree **and, later, the config TOMLs**") at
`/etc/config`. One backend *type*, one rule for rc AND config. Scope: **all four
files** (`theme.toml`, `models.toml`, `mcp.toml`, `system.md`), including
migrating the app to read theme over RPC.

### Why the old one-paragraph sketch was wrong
Slice-1 reconnaissance found config consumers are **heterogeneous** — they do
*not* all flow through the CRDT today:
- `models.toml` → kernel reads via `config_backend` (CRDT) — clean to converge.
- `system.md` → kernel reads via `config_backend` (CRDT) — clean to converge.
- `theme.toml` → **the app** reads the host file directly (`theme_loader.rs`,
  separate process, no RPC). The kernel never reads its *content*; `ensure_config`
  only writes the host file *so the app can read it*. So for theme, the host file
  is load-bearing for the app — dropping the host write breaks app theming unless
  the app fetches theme over RPC.
- `mcp.toml` → effectively inert (a `DEFAULT_MCP_CONFIG` const, no live loader
  found); converge the doc, no reader to re-point.
- The capnp config RPCs (`listConfigs`/`reloadConfig`/`resetConfig`/`getConfig`)
  are effectively unused client-side (no client wrappers exist) — low-risk to
  reshape, but the app theme migration *adds* a `getConfig` client wrapper.

### Build order (kernel-first, TDD, committable stages)
1. **Relocate the embedded `DEFAULT_*` consts** (`DEFAULT_THEME`,
   `DEFAULT_MODELS_CONFIG`, `DEFAULT_MCP_CONFIG`, `DEFAULT_SYSTEM_PROMPT`) out of
   `config_backend.rs` (into `config_doc.rs` / a small seed module) so deleting
   the backend keeps `kaijutsu_kernel::DEFAULT_SYSTEM_PROMPT` et al. exported.
2. **Config seed source + generalized seeding.** `config_seed_files() →
   [("/etc/config/theme.toml", DEFAULT_THEME), …]`; extract `ConfigCrdtFs`'s
   absent-only, fail-loud seed core (`seed_entries`) so both rc and config seed
   through it.
3. **Mount** a `ConfigCrdtFs` at `/etc/config` in `rpc.rs` setup; seed embedded
   once (fresh-namespace gate via `is_empty`).
4. **Re-point kernel readers** to the VFS: `initialize_kernel_models`
   (`/etc/config/models.toml`) and `llm_stream` (`/etc/config/system.md`). The
   parse-fail "reload from disk" safety valve becomes "reset that doc to embedded"
   (no disk to fall back to) — loud, not silent.
5. **Delete `ConfigCrdtBackend`** (`config_backend.rs`), the watcher, the
   `config_backend`/`config_watcher` fields on `SharedKernelState`, and
   `create_config_backend`.
6. **Reshape the capnp config RPC handlers** to route through the VFS:
   `getConfig`/`listConfigs`/`resetConfig` over `/etc/config`; `reloadConfig`
   (disk reload is meaningless now) → reseed-from-embedded.
7. **`kj config`** command (`kj/config.rs`) mirroring `kj rc`: `show`/`list`/
   `set`/`reset`, gated on a new `Capability::ConfigWrite`, driven through
   `self.kernel().vfs()`. TDD.
8. **App theme over RPC**: add a `getConfig` client wrapper + actor method; the
   app fetches `theme.toml` after connect and applies it (startup loads default
   synchronously, theme arrives post-connect). Drop the host theme read + the
   theme half of `write_default_configs_if_missing` (`bindings.toml` stays
   app-side host config — it is not a kernel config). Live hot-reload-on-edit may
   land as a follow-up if a CRDT subscription is needed.

(One rule for all config; loses the vim-the-toml affordance, parallel to retiring
vim-the-rc-file.)

---

## Open implementation questions (remaining)

1. **Durability + reseed semantics.** CRDT rc/config lives in `kernel.db`; confirm
   `kj rc reseed` (CRDT ← embedded) and the staleness-vs-embedded story (the
   issues.md "staleness indicator" want partly dissolves — drift is now
   CRDT-vs-embedded, surfaced by an explicit reseed, not silent host drift).
2. **Manifest storage** — DECIDED 2026-06-16: **`kernel.db` index** (a `path →
   doc id` table). Persists with everything else, clean SQL prefix scan for
   `readdir`, no CRDT-doc enumeration/serialize gymnastics. Matches the
   handle-implies-row persistence backbone.
3. **Backend shape** — DECIDED 2026-06-16: `ConfigCrdtBackend` is *not* a `VfsOps`
   backend (direct-access API + TOML watcher/flush, never mounted), so slice 1
   builds a **new `VfsOps` backend (`ConfigCrdtFs`)** that reuses
   `config_context_id` + the `DocKind::Config` single-block doc helpers (extracted
   to a shared module) and mounts at `/etc/rc`. TOMLs stay on `ConfigCrdtBackend`'s
   direct API for slice 1; they converge in slice 2 when the host flush dies. This
   keeps the mount-readdir lifetime from tangling with the soon-to-die TOML flush.
4. **Project-source F1 residual.** ~~Still real for `~/src`/`/tmp`~~ — RESOLVED:
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

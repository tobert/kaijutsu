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

> **2026-08-03 update — `models.toml` left the CRDT for SQL.** Model config
> (backends, casts, aliases, tunables, embedding) now lives in normalized
> kernel-db tables, edited via `kj backend`/`kj cast`/`kj alias` with live
> registry reload — no TOML in that path at all (devlog: "Contexts join a
> band"). Mentions of `models.toml` below are accurate history for slice 2 as
> built. The doctrine itself is unchanged and `theme.toml`, `mcp.toml`,
> `system.md`, and all of `/etc/rc` remain CRDT-owned exactly as described.

> **Superseded in part (2026-08-15/16):** `/etc/config`, `/etc/client`, and
> `/etc/midi` are still `ConfigCrdtFs` mounts — CRDT documents in
> `kernel.db`, exactly as this doc describes. What changed is the *gate*,
> not the storage: `deny_etc_write` narrowed to the host's real `/etc`, so
> the file tools and the editor now reach these mounts directly, and `kj
> config set`/`edit` were deleted as a redundant second door rather than
> because the storage moved (`988122f9`). `/etc/rc` is unchanged — still
> `rc-write`-gated, still `ConfigCrdtFs`. The storage half of Lane B —
> these documents becoming real files in a kernel-owned git worktree — has
> **not** shipped: `kaijutsu-configgit`, the git write seam, exists and is
> tested but is not called from anywhere in the kernel, and Amy has been
> leaning toward a simpler shape that may not use it at all. See "Lane B"
> below. The invariant that survives regardless of which shape wins is
> single kernel ownership: one source of truth, no host write-through. See
> CLAUDE.md ("Config is different from rc").

---

## Lane B — the git-worktree seam, shipped and deliberately unwired (2026-08-15/16)

`crates/kaijutsu-configgit` is the write half of a `<data_dir>/config` git
worktree meant to eventually replace `ConfigCrdtFs` as the storage for
`rc/`, `config/`, `client/`, and `midi/`. It landed 2026-08-15 (`9a8cd9f0`),
tested — a lossless round trip in `config_export.rs`, a spawn-free tripwire
in `tests/spawn_free.rs` — and is **not called from anywhere in the
kernel**. Nothing reads or writes `<data_dir>/config` today; the mounts
above are still `ConfigCrdtFs`, unchanged.

Amy has been ambivalent about whether this shape ships at all, in the same
conversation that produced the rulings below:

> "I'm leaning towards simplifying more, maybe just config files, keep the
> reseed tool, the edit config verbs go away. and the git process is just a
> skill we have handy via rc or help system"

Read literally, that replaces the git worktree with plain files on disk plus
a reset-to-embedded-default tool, and moves git out of the kernel entirely —
a skill reached through rc or the help system, not a mechanism the kernel
runs. The permission behind it, from the same conversation and recorded in
CLAUDE.md as "Permission to get simpler": *"if the agent can see the files
and edit them, that's fine, we don't need to complicate it just because it's
config."* The write-gate change described above (`988122f9`) already takes
that permission literally for the CRDT-backed mounts in place today. Whether
`kaijutsu-configgit`'s git worktree is still the intended storage, or gets
replaced outright by the simpler shape, is unresolved. Read the rulings
below as what was decided *if* the git-worktree shape is the one that ships
— not as a settled design.

### Rulings (Amy, 2026-08-15)

1. One git worktree at `<data_dir>/config`, holding all four config-like
   roots — `rc/`, `config/`, `client/`, `midi/` — under one `.git`. One
   history joins an rc edit to a config edit; there is no per-root
   repository.
2. Auto-commit per accepted mutation. The git log *is* the config oplog —
   that is the reason to melt these documents into files at all, so the 1:1
   join between a kernel operation and a commit is worth the noisy log it
   produces. Commits carry the operation id from the start, as a
   `kaijutsu-operation-id` extra header on the commit object — invisible to
   `git log --format=%(trailers)`, visible to `git cat-file -p`. The writer
   never stages through `.git/index`; it walks the live worktree and commits
   its full current state on every call, because the kernel's VFS is already
   the index and a second index would be a second copy of the truth.
3. No watcher, no implicit import on the worktree. An unexpectedly dirty
   worktree is a fail-loud condition, not something the kernel resolves by
   guessing. An explicit `kj … import` verb may be added later; nothing
   reads the directory back into the kernel today.
4. Seeding stays namespace-bootstrap-only through the migration: seed once
   on a fresh kernel, never again. A file the operator deletes stays
   deleted; a shipped default added after a kernel already exists does not
   retroactively appear on it. No tombstones — that limitation is unchanged
   by this migration and gets its own decision later, separately.
5. Commits are service-authored (`kaijutsu-kernel <kernel@kaijutsu.internal>`)
   for now. Principal plumbing — giving a config mutation its real actor —
   does **not** gate this work: *"I don't think the principal plumbing
   should gate the git work."* The gap is filed as a holistic sweep across
   the codebase, not a Lane B task (`docs/issues.md`, "Principal plumbing").
6. Empty virtual config directories keep their current limitation: they are
   synthesized from descendant paths today and have never round-tripped to a
   real filesystem, and a git tree cannot represent an empty directory
   either — a tree with zero entries can be written, but nothing lets a
   parent tree name an empty child the way `git ls-tree` would show one.
   Preserve the inability rather than build around a limit git itself has.

Known not to round-trip, from the export work (`config_export.rs`) — each is
an accepted limitation, not a blocking gap: file permissions and modes (the
CRDT backend has none; `getattr` reports a fixed mode), empty directories
(above), and the `documents.language` column (never set for config today).
Duplicate paths are structurally impossible under the UUIDv5-per-path
scheme.

**Git write mechanism, if this shape ships:** build on gitoxide's
**plumbing** crates only (`gix-object`, `gix-odb`, `gix-ref`, …), never the
`gix` facade — it links `gix-command` unconditionally and reintroduces
subprocess-spawn machinery — and never invoke the `git` executable (the
host-exec-has-one-owner rule: a `git` subprocess here is a second exec site
kaish already owns). Pin at the same versions as `~/src/kaish-extras`'s
`kaish-tools-git` (inspected 2026-08-15 at `96e9825`, read-only today), so
the write half can move there as a `Profile::Write` later instead of being
reimplemented.

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

**2026-07-12 addendum — theme.toml grew a `[scene]` table** (docs/color.md):
the one theme file now carries BOTH color lanes — the flat keys + `[ansi]`
feed the UI `Theme`, `[scene.hues/tiers/gains/post]` feed the 3D
`ScenePalette`. Same ownership mechanics as before (CRDT-owned, seeded once,
fetched over RPC on connect); `[scene.post]` additionally hot-applies to the
camera when a theme lands. The "live hot-reload-on-edit" follow-up above is
still open — a `kj config set` mid-session is NOT pushed to connected apps;
they pick it up on the next connect.

---

## Per-client config — the `/etc/client/` namespace (design direction, 2026-07-05)

> Captured with Amy 2026-07-05 while designing the metronome UX / patch bay.
> **Namespace + first consumer SHIPPED 2026-07-05** (`feat/metronome-config`):
> the `/etc/client` mount, the cascade-aware canonicalizers, and the metronome
> click config. The patch-bay routing table is the intended second consumer, but
> its storage shape is still open (likely *not* TOML — it's a relation the
> reconciler maintains, not hand-edited config). Both consumers are
> *machine-local* (each client faces a different ALSA graph), which is what
> forces this namespace.

### Why a second namespace

Everything in `/etc/config/*` today is a **kernel-wide singleton** — one
`models.toml`, one `system.md`, read by the kernel to drive turns no matter who
is connected. Correct for those. But client-facing config is not a singleton:
the metronome click and the patch-bay wiring differ *per client*, because each
client — the Bevy app on zorak, another on a laptop, an MCP producer seat, a
future headless edge node — faces a different local audio graph. A global
`patchbay.toml` cannot say "on **this** box, wire render → **this** synth."

So config splits into namespaces (a third left for later):

| Namespace | Scope | Examples | Reader |
|---|---|---|---|
| `/etc/config/*` | kernel-wide singleton | `models.toml`, `system.md` | kernel, no client-id |
| `/etc/client/*` | per-client (this design) | `metronome.toml`, `patchbay.toml` | client, presents its id |
| `/etc/principal/*` | per-player (deferred) | personal prefs someday | — |

`/etc/principal/` is a **door left open**, not designed now (config-hierarchy
thinking deferred by decision) — named so the `/etc/client/` shape doesn't paint
it out.

### The cascade

A client-facing read resolves in order (gitconfig/CSS style):

```
/etc/client/<id>/metronome.toml   →  this client's override (usually absent)
/etc/client/metronome.toml        →  the shared client default (seeded from embedded)
<embedded seed>                   →  last-resort fallback
```

Most clients ride the shared default; a client overrides only what it cares
about. The cascade is **opt-in by the reader**: kernel-global readers keep
reading `/etc/config/<name>` with no id and get no cascade; a client-facing read
passes its client-id and gets the two-level resolution. It reuses the existing
hierarchical config store unchanged — `/etc/client/<id>/x` is just another
`DocKind::Config` doc at a deeper path, readdir via the existing manifest prefix
scan. **No new backend machinery** — a mount at `/etc/client`, a resolver, and a
write-target policy.

### Client identity — must generalize past the Bevy app

The key is the **stable client-id the client presents** — the same string
`client_views` already takes (`get_client_view(client_id: &str)`), so the kernel
is already agnostic. What each client type must source:

- **Bevy app** — has it: `ClientId`, a per-installation UUID at
  `~/.local/share/kaijutsu/client-id` (survives restarts).
- **MCP producer seat / headless clients** — do **not** have a stable per-install
  id yet. The MCP `session_id` is per-Claude-Code-session (captured from hook
  events), not a durable installation id. Giving MCP + headless clients a stable
  client-id (own XDG file / config value) is a **prerequisite to-do** for their
  per-client config to persist. Until then they ride the shared
  `/etc/client/<name>` default — a fine degraded mode.

### Write-target policy

`kj config set` **defaults to the caller's own** `/etc/client/<id>/<name>` — a
client tweaking its metronome never touches a neighbor's. `--global` writes the
shared client default `/etc/client/<name>` (affects every client that hasn't
overridden). Kernel-global config (`/etc/config/*`, e.g. `models.toml`) is a
separate, always-explicit target — so `--global` here means "the shared *client*
default," not "the kernel singleton."

**Shipped so far:** the canonicalizer accepts the hierarchical `/etc/client`
namespace, so `kj config set /etc/client/metronome.toml` (shared default) and
`kj config set /etc/client/<id>/metronome.toml` (an explicit override) both work.
**Deferred:** the *ergonomics* — the caller-scoped default (no `<id>` needed) and
the `--global` flag — which need `kj` to resolve the caller's client-id. That
resolution is the same MCP/headless-client-id prerequisite noted above.

### Seeding & orphans

Per-client docs (`/etc/client/<id>/…`) are **not** compile-seeded — there is no
client-id at build time. They are created lazily on first per-client write;
until then the client rides the shared/embedded default (nothing to seed). The
shared `/etc/client/<name>` defaults *are* embedded-seeded like the rest. A lost
client-id orphans that client's subtree — the same accepted failure mode
`client_views` documents at single-user scale; a `kj config clients` list + a
prune verb can GC orphans later.

### First consumers

- `/etc/client/<id>/metronome.toml` → `{ enabled, note, channel, velocity,
  gate_ms }` — **SHIPPED**. Seeded shared default at `/etc/client/metronome.toml`
  (`assets/defaults/metronome.toml`); the app fetches its own via the cascade
  after connect (`actor_plugin` bootstrap) and applies to the `Metronome`
  resource, replacing the hardcoded `CLICK_NOTE`/`CH`/velocity/gate. Config-change
  *push* (re-apply on a live `kj config set` without reconnect) is the noted
  follow-up; today it applies once per (re)connect. Later: downbeat accent (needs
  meter the `BeatRef` doesn't carry).
- `/etc/client/<id>/patchbay.toml` → the declared **symbolic** wires for this
  client's audio graph ("render out → gm-synth"); the app-side reconciler reads
  its own and drives the local ALSA seq graph toward it (resolving symbols →
  live client numbers, which are dynamic). See the patch-bay design
  (forthcoming).

### Open (this design)

- **Which existing configs migrate to `/etc/client/`?** `theme.toml` is
  client-facing and a natural candidate; `models.toml`/`system.md` are not.
  Decide per-file when touched — no big-bang migration.
- **Config-changed push** so a live `kj config set` reaches the client without a
  reconnect (the app fetches theme once on connect today). A CRDT subscription on
  the client's config docs; scope it with the patch bay.

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

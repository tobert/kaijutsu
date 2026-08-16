# Kernel-Owned Config — Design

The kernel is the **sole owner** of config and rc scripts. Embedded Rust source
(compiled into the binary, visible in-repo) is the **seed**; after that, the
kernel owns the content. There is no host-disk write-through, no
reload-from-host, and no mtime staleness — one source of truth cannot disagree
with itself.

**Project repo source files are permanently out of scope.** There the host disk
is the truth (cargo, git, and editors read real files) and write-through stays.
Making the kernel authoritative for repo source would reintroduce the dual
ownership this design deletes.

Model config is not here. Backends, casts, aliases, tunables, and embedding live
in normalized `kernel.db` tables, edited via `kj backend`/`kj cast`/`kj alias`
with live registry reload — no TOML in that path at all.

## Write gates

`/etc/config`, `/etc/client`, and `/etc/midi` are ordinary write surfaces. The
file tools and the editor reach them directly; `deny_etc_write` covers only the
host's real `/etc`. `kj config` keeps `list`, `show`, and `reset` — the three
verbs with no file-tool equivalent.

`/etc/rc` is gated by the `rc-write` capability — rc is executable rather than
data.

---

## Storage model

One path maps to one document: `UUIDv5("kaijutsu:config:{path}")` →
a single-block `DocKind::Config` document, over hierarchical paths, with one seed
mechanism.

**The `documents` table is the manifest.** `documents(document_id, workspace_id,
doc_kind, path, …)` carries a `path` column and a `UNIQUE(workspace_id, path)`
index, so `readdir` is `list_documents_by_kind(Config)` filtered by prefix, with
immediate children derived in Rust. The document and its manifest row are written
by the same `create_document` call — a separate manifest table would be a second
copy of the truth, free to drift.

**Read-path routing.** rc reads and writes route through the VFS (`MountTable →
ConfigCrdtFs`) directly, bypassing `FileDocumentCache` for the kernel-internal
callers (`kj rc`, `load_rc_scripts`). The one remaining cache consumer is an
agent's `builtin.file:read /etc/rc/…`; `ConfigCrdtFs` returns an in-memory
advancing mtime from `getattr`, bumped on write, so the cache's staleness check
reloads after a write. That mtime is a version stamp on the single source of
truth, not a sync between two.

**Seeding is namespace-bootstrap-only.** A fresh kernel seeds once and never
again. A file the operator deletes stays deleted, and a shipped default added
after a kernel already exists does not retroactively appear on it. There are no
tombstones.

**There is no host file to edit.** Change a live script with `kj rc edit <path>
--content <body>`; restore one to its embedded default with `kj rc reset <path>`.
Change the shipped default by editing `assets/defaults/rc/`, the in-repo seed,
then reseeding.

`theme.toml` carries both color lanes: the flat keys and `[ansi]` feed the UI
`Theme`, and `[scene.hues/tiers/gains/post]` feed the 3D `ScenePalette`
(`docs/color.md`). The app fetches it over RPC on connect; `[scene.post]`
additionally hot-applies to the camera when a theme lands. **A `kj config set`
mid-session is not pushed to connected apps** — they pick it up on the next
connect.

---

## Lane B — the git-worktree seam, shipped and deliberately unwired

`crates/kaijutsu-configgit` is the write half of a `<data_dir>/config` git
worktree meant to eventually replace `ConfigCrdtFs` as the storage for `rc/`,
`config/`, `client/`, and `midi/`. It is tested — a lossless round trip in
`config_export.rs`, a spawn-free tripwire in `tests/spawn_free.rs` — and is
**not called from anywhere in the kernel**. Nothing reads or writes
`<data_dir>/config` today.

Amy has been ambivalent about whether this shape ships at all, in the same
conversation that produced the rulings below:

> "I'm leaning towards simplifying more, maybe just config files, keep the
> reseed tool, the edit config verbs go away. and the git process is just a
> skill we have handy via rc or help system"

Read literally, that replaces the git worktree with plain files on disk plus a
reset-to-embedded-default tool, and moves git out of the kernel entirely — a
skill reached through rc or the help system, not a mechanism the kernel runs.
The permission behind it, from the same conversation and recorded in CLAUDE.md
as "Permission to get simpler": *"if the agent can see the files and edit them,
that's fine, we don't need to complicate it just because it's config."*

Whether `kaijutsu-configgit`'s git worktree is still the intended storage, or
gets replaced outright by the simpler shape, is unresolved. **Read the rulings
below as what was decided *if* the git-worktree shape ships — not as a settled
design.** The invariant that survives either way is single kernel ownership: one
source of truth, no host write-through.

### Rulings (Amy, 2026-08-15)

1. One git worktree at `<data_dir>/config`, holding all four config-like roots —
   `rc/`, `config/`, `client/`, `midi/` — under one `.git`. One history joins an
   rc edit to a config edit; there is no per-root repository.
2. Auto-commit per accepted mutation. The git log *is* the config oplog — that
   is the reason to melt these documents into files at all, so the 1:1 join
   between a kernel operation and a commit is worth the noisy log it produces.
   Commits carry the operation id from the start, as a `kaijutsu-operation-id`
   extra header on the commit object — invisible to `git log
   --format=%(trailers)`, visible to `git cat-file -p`. The writer never stages
   through `.git/index`; it walks the live worktree and commits its full current
   state on every call, because the kernel's VFS is already the index and a
   second index would be a second copy of the truth.
3. No watcher, no implicit import on the worktree. An unexpectedly dirty
   worktree is a fail-loud condition, not something the kernel resolves by
   guessing. An explicit `kj … import` verb may be added later; nothing reads
   the directory back into the kernel today.
4. Seeding stays namespace-bootstrap-only through the migration: seed once on a
   fresh kernel, never again. No tombstones — that limitation gets its own
   decision later, separately.
5. Commits are service-authored (`kaijutsu-kernel <kernel@kaijutsu.internal>`)
   for now. Principal plumbing — giving a config mutation its real actor — does
   **not** gate this work: *"I don't think the principal plumbing should gate
   the git work."* The gap is filed as a holistic sweep across the codebase, not
   a Lane B task (`docs/issues.md`, "Principal plumbing").
6. Empty virtual config directories keep their current limitation: they are
   synthesized from descendant paths today and have never round-tripped to a
   real filesystem, and a git tree cannot represent an empty directory either —
   a tree with zero entries can be written, but nothing lets a parent tree name
   an empty child the way `git ls-tree` would show one. Preserve the inability
   rather than build around a limit git itself has.

Known not to round-trip, from the export work (`config_export.rs`) — each is an
accepted limitation, not a blocking gap: file permissions and modes (the backend
has none; `getattr` reports a fixed mode), empty directories (above), and the
`documents.language` column (never set for config today). Duplicate paths are
structurally impossible under the UUIDv5-per-path scheme.

**Git write mechanism, if this shape ships:** build on gitoxide's **plumbing**
crates only (`gix-object`, `gix-odb`, `gix-ref`, …), never the `gix` facade — it
links `gix-command` unconditionally and reintroduces subprocess-spawn machinery
— and never invoke the `git` executable (the host-exec-has-one-owner rule: a
`git` subprocess here is a second exec site kaish already owns). Pin at the same
versions as `~/src/kaish-extras`'s `kaish-tools-git` (inspected 2026-08-15 at
`96e9825`, read-only today), so the write half can move there as a
`Profile::Write` later instead of being reimplemented.

---

## Per-client config — the `/etc/client/` namespace

### Why a second namespace

Everything in `/etc/config/*` is a **kernel-wide singleton** — one `system.md`,
read by the kernel to drive turns no matter who is connected. Client-facing
config is not a singleton: the metronome click and the patch-bay wiring differ
per client, because each client — the Bevy app on zorak, another on a laptop, an
MCP producer seat, a future headless edge node — faces a different local audio
graph. A global `patchbay.toml` cannot say "on **this** box, wire render → **this**
synth."

| Namespace | Scope | Examples | Reader |
|---|---|---|---|
| `/etc/config/*` | Kernel-wide singleton. | `system.md` | The kernel, with no client-id. |
| `/etc/client/*` | Per-client. | `metronome.toml`, `patchbay.toml` | A client, presenting its id. |
| `/etc/principal/*` | Per-player. Deferred — named only so the `/etc/client/` shape does not paint it out. | Personal prefs someday. | — |

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
passes its client-id and gets the two-level resolution. It reuses the
hierarchical config store unchanged — `/etc/client/<id>/x` is just another
`DocKind::Config` document at a deeper path, readdir via the existing manifest
prefix scan. **No new backend machinery** — a mount at `/etc/client`, a
resolver, and a write-target policy.

### Client identity

The key is the **stable client-id the client presents** — the same string
`client_views` already takes (`get_client_view(client_id: &str)`), so the kernel
is already agnostic. What each client type must source:

- **Bevy app** — has it: `ClientId`, a per-installation UUID at
  `~/.local/share/kaijutsu/client-id` (survives restarts).
- **MCP producer seat and headless clients** — have no stable per-install id
  yet. The MCP `session_id` is per-Claude-Code-session (captured from hook
  events), not a durable installation id. Giving these clients a stable
  client-id (own XDG file or config value) is a **prerequisite** for their
  per-client config to persist. Until then they ride the shared
  `/etc/client/<name>` default — a fine degraded mode.

### Write-target policy

`kj config set` **defaults to the caller's own** `/etc/client/<id>/<name>` — a
client tweaking its metronome never touches a neighbor's. `--global` writes the
shared client default `/etc/client/<name>`, which affects every client that has
not overridden. Kernel-global config (`/etc/config/*`) is a separate,
always-explicit target — so `--global` here means "the shared *client* default,"
not "the kernel singleton."

The canonicalizer accepts the hierarchical `/etc/client` namespace, so `kj
config set /etc/client/metronome.toml` (shared default) and `kj config set
/etc/client/<id>/metronome.toml` (an explicit override) both work. **Deferred:**
the ergonomics — the caller-scoped default (no `<id>` needed) and the `--global`
flag — which need `kj` to resolve the caller's client-id. That resolution is the
same client-id prerequisite noted above.

### Seeding and orphans

Per-client documents (`/etc/client/<id>/…`) are **not** compile-seeded — there
is no client-id at build time. They are created lazily on first per-client
write; until then the client rides the shared or embedded default. The shared
`/etc/client/<name>` defaults *are* embedded-seeded like the rest. A lost
client-id orphans that client's subtree — the same accepted failure mode
`client_views` documents at single-user scale; a `kj config clients` list plus a
prune verb can collect orphans later.

### Consumers

- `/etc/client/<id>/metronome.toml` → `{ enabled, note, channel, velocity,
  gate_ms }`. Seeded shared default at `/etc/client/metronome.toml`
  (`assets/defaults/metronome.toml`); the app fetches its own via the cascade
  after connect (`actor_plugin` bootstrap) and applies it to the `Metronome`
  resource. Later: downbeat accent, which needs meter the `BeatRef` does not
  carry.
- `/etc/client/<id>/patchbay.toml` → the declared **symbolic** wires for this
  client's audio graph ("render out → gm-synth"); the app-side reconciler reads
  its own and drives the local ALSA seq graph toward it, resolving symbols to
  live client numbers, which are dynamic. See the patch-bay design
  (forthcoming).

---

## Open

1. **Which existing configs migrate to `/etc/client/`?** `theme.toml` is
   client-facing and a natural candidate. Decide per-file when touched — no
   big-bang migration.
2. **Config-changed push** so a live `kj config set` reaches the client without
   a reconnect. A document subscription on the client's config documents; scope
   it with the patch bay.
3. **Reseed semantics.** Confirm `kj rc reseed` and the staleness-vs-embedded
   story: drift is now document-vs-embedded, surfaced by an explicit reseed, not
   silent host drift.

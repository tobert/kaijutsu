# The config namespace — `/config` as a bind-mount registry

**Status: built 2026-08-29.** The registry, the namespace move and the melt
of all four trees have landed; `ConfigDocFs` deletion and the `DocKind::Symlink`
retirement follow. This supersedes
`docs/config-ownership.md`, whose premise — the kernel is the sole owner and
there is no host file — stops being true when this lands. It continues
`docs/rc-on-disk.md`, which melted rc and left the other three roots.

## The rule

**A `/config` path is a well-known name. Where it comes from is a mount
declaration, not a compiled-in constant.**

```
/config/            # no backend of its own — listed from its mount points
    rc/             # lifecycle scripts
    kernel/         # theme.toml, mcp.toml, system.md
    client/         # default/<name>, <client-id>/<name>
    midi/           # devices/<name>
```

**The four are siblings, never a base plus subtrees.** The kernel-global tree
is `/config/kernel` rather than `/config` itself because the path predicates
are component-correct but not sibling-aware: a root containing the others
would make `is_config_path("/config/rc/x")` true. `paths.rs` carries a test
asserting no tree sits under another.

Everything under `/config` is a real host directory reached through
`LocalBackend`. The kernel looks up `/config/rc`; it does not know or care
where that lands.

## Why not `/etc`, and why not `/v`

**Not `/etc`, because it is the host's.** Squatting it costs a guard that
exists for no other reason: `deny_etc_write`
(`crates/kaijutsu-kernel/src/file_tools/path.rs:77`) says in its own doc that
`/etc` is shared ground, kaijutsu's mounts sit alongside the host root where
`/etc/passwd` lives, and the function is "the line between them." Move off and
there is no line to draw. The guard deletes, and `/etc` goes back to being the
host's read-only `/etc`.

**Not `/v`, because these are not virtual.** `/v` holds things the kernel
synthesizes — `/v/cas`, `/v/ctx`, `/v/session`, `/v/swap`, `/v/docs`,
`/v/input`. A config directory is a directory: no synthesis, no special
semantics, nothing the kernel generates. Filing it under `/v` would say it is
one of those, and it is not. This keeps `docs/slash-v.md` principle 7's
junk-drawer rule intact rather than bending it.

**kaijutsu is not Linux**, and neither namespace is owed a Unix meaning. `/v`
means virtual, `/config` means configuration, `/run` means ephemeral liveness,
`/r` means remote client shares, `/` is the read-only host anchor. Each name
says what it is.

## The registry

Precedence, highest first:

```
kaijutsu-server --mount /config/rc=/home/amy/src/kaijutsu/assets/defaults/rc
<config-root>/mounts.toml
<config-root>/<name>            # the default: an ordinary subdirectory
```

`--config-root <dir>` sets the base, defaulting to `~/.config/kaijutsu/config`.
`mounts.toml` lives inside it:

```toml
[mounts]
"/config/rc"   = "~/src/kaijutsu/assets/defaults/rc"
"/config/midi" = "~/sync/kaijutsu-midi"
```

An unknown tree name is refused and the error lists the valid ones; a
malformed `mounts.toml` fails the boot rather than being skipped, because it
was written on purpose and mounting something else instead is the silent
fallback this repo refuses. Mounts resolve **before** the "Starting kaijutsu
server" line, so a bad declaration reports a server that never began rather
than one that came up and died.

There is no bootstrap cycle: the root comes from a flag or the default, and the
file inside it only ever names its own children.

**When nothing is declared, nothing looks special.** Every root is a
subdirectory of one host directory, and the registry is invisible. It earns its
keep only when a root diverges — which is the point of "don't make rooting on a
real system too hard."

## This needs no new VFS machinery

`MountTable` already does the two things the registry requires.

**Longest-prefix routing** (`crates/kaijutsu-kernel/src/vfs/mount.rs:1`). A
base mount at `/config` plus an override at `/config/rc` resolves the way a
bind mount does — `/config/theme.toml` through the base, `/config/rc/...`
through the override. No precedence logic to write.

**Synthetic mount children in a parent listing**
(`crates/kaijutsu-kernel/src/vfs/mount.rs:854-886`). `readdir` merges a
backend's own entries with the mount points beneath it, and tolerates a backend
miss when synthetic children exist. So `ls /config` lists `rc`, `midi` and
`client` even where no backend serves `/config` itself.

**Freeze after bootstrap** (`mount.rs:119`) is already the right lifetime:
declare once at startup, immutable thereafter.

And the pattern already exists for rc. `default_rc_dir()`
(`crates/kaijutsu-server/src/ssh.rs:148`) is documented as "the one place the rc
tree's default location is decided. Everything else takes the path it is
handed," and `rc_dir: &Path` is threaded through bootstrap
(`crates/kaijutsu-server/src/rpc.rs:1779-1782`). This generalizes that to every
root and makes it declarative.

## What it buys beyond tidiness

**Point rc at the repo.** `--mount /config/rc=./assets/defaults/rc` and the
running kernel reads the seed you are editing. No rebuild, no reseed, no
restart to see a stance change.

**The hazard this raises is already handled, and not with a wall.**
`kaijutsu-server rc reseed --force` writes the binary's embedded copy over the
rc tree, so a root pointed at a checkout would be overwritten with whatever the
last build contained. The answer is legibility, not a refusal: reseed compares
every entry it leaves alone and **names** the ones that differ, so a root
pointed somewhere unexpected reports every file in it.

```
./rc: 0 written, 0 replaced, 75 unchanged

2 file(s) differ from their defaults and were left alone:
  coder/create/S00-stance.kai
  lib/create/S20-cache.kai

Pass --force to overwrite them with the embedded defaults.
```

An untouched tree says nothing, so the warning stays worth reading. `--force`
is then an ordinary deliberate act by a player who has been told exactly what
it will take — which is the house stance (`docs/instrument-design.md`, "Many
hands, one trust boundary"): capabilities are ergonomic nudges, and the nudge
is naming the consequence, never denying the act.

This landed ahead of the mount registry because it stands on its own: presence
and agreement are different facts, and reporting only a count conflated them
whatever the root was.

**Test and fixture roots stop being a special case.** A test points a root at a
tempdir through the same mechanism production uses, instead of a bespoke
parameter.

**Sync policy becomes per-root.** theme and rc are shareable across machines;
`mcp.toml` and `client/` are machine-local snowflakes. Today they are forced
into one tree.

## `client/default/` — removing a live ambiguity

`kaijutsu_types::paths::client_config_path` (`paths.rs:126`) maps `None` to
`<root>/<name>` and `Some(id)` to `<root>/<id>/<name>`. A segment after the root
is a *filename* or a *client id* depending on which happens to be there.

On kernel documents that was survivable. On a real filesystem it breaks the
first time a client id collides with a config filename — one becomes a directory
where the other expects a file.

```
/config/client/default/metronome.toml     # the shared default
/config/client/<client-id>/metronome.toml # one client's override
```

One-line change; the app's two-step resolution
(`crates/kaijutsu-app/src/connection/actor_plugin.rs:564`) is unaffected.

## What this deletes

| Thing | Lines | Why it goes |
|---|---|---|
| `runtime/config_doc_fs.rs` | 1339 | The backend itself. Nothing left to back. |
| `config_export.rs` | 623 | Built for the cancelled git-worktree flip; zero production callers. |
| `config_doc.rs` | 60 | The shared config-document model. |
| `deny_etc_write` + tests | — | Only exists to draw a line inside `/etc`. |
| `VfsOps::owns_config_docs` | — | Only ever `true` on `ConfigDocFs`. |
| `EditorTarget::config_owned` + its branch | — | Binds to a block because there was no file. There is a file. |
| `DocKind::Symlink` | — | Nothing creates one once rc composition is real symlinks. |

Above 2000 lines. `ConfigDocFs` goes away entirely rather than shrinking again.

**What replaces the machinery is nothing.** Absent-only seeding already exists
as `ensure_rc_seed_files`; reset-to-embedded already exists as
`config_seed_body`; and `config_seed_override`
(`crates/kaijutsu-server/src/rpc.rs:1267`) already reads a config file from a
host directory at bootstrap. That last one is this design in miniature, written
months ago and used once per install.

## Migration — one flag day

`config_context_id` is a UUIDv5 of the canonical path (`config_doc.rs:28`), so
**moving a path changes a document's identity.** Move and melt must be the same
change; doing them in sequence would mint a second set of documents nobody
reads.

1. `kj config export <dir>`, **run against a kernel built BEFORE this melt.**

   This is the one ordering that cannot be recovered from. `export_config_tree`
   walks *documents*; after the melt the trees are `LocalBackend` and hold none,
   so the verb correctly reports nothing to export — and the documents holding
   your real theme, `mcp.toml`, and the MIDI profiles pulled from actual
   hardware are still in `kernel.db`, unreachable, with no verb left that reads
   them.

   So per machine: deploy a binary at `122fbef4` (or any commit after it and
   before this melt), run `kj config export <dir>`, and only then deploy the
   melt with `--config-root <dir>`.
2. Move `~/.config/kaijutsu/etc/rc/` to the new config root.
3. Declare the mounts; drop the four `/etc` mounts.
4. Delete the table above.
5. Repoint the `paths.rs` constants, the seeds, and every consumer.

**`config_export.rs` earns its keep on the way out.** Dead code today, written
for a flip that was cancelled, and its one legitimate job is step 1. Run it once
per machine, then delete it in step 4.

## `docs/slash-v.md` principle 7

Principle 7 reads: *"Resist the `/proc` junk-drawer. `/v/ctx` is only the
context/block model; `/v/session` is only live participants. Config stays at
`/etc/config`."*

The junk-drawer rule stands and this design obeys it — config does not move into
`/v`. Only the last sentence changes: config lives at `/config`, a sibling
top-level tree, the same way `/r` is a sibling because it names remote clients
rather than kernel-local virtual filesystems (`paths.rs:95-100`).

## To measure, not assume

- **Does `Kernel::invalidate_config_file_cache` survive?** Two readings
  disagree. `file_tools/cache.rs:706` argues the explicit call is required
  because the editor writes the block directly and never advances the backend's
  generation counter — an argument about `ConfigDocFs`, which dies with it.
  Against that, a mutation on 2026-08-29 showed the rc shadow self-healing
  without the hook, because a host file's stat moved. Settle it with a mutation
  on each surviving write path.
- **`DocKind::Symlink` is a persisted enum.** Removing a variant touches the
  schema and possibly the wire; `migrate()` has no ALTER-TABLE path and
  interface ordinals must stay sequential. Likely its own commit, after the rest.
- **Does the registry stay config-only?** The same mechanism could bind a
  workspace or a samples pool. Do not build for that; note it and see whether a
  second caller appears.

## The one thing that gets worse

Config leaves `kernel.db`. Today a config edit is a kernel-sequenced block
mutation inside the durable store, so it travels with a database backup.
Afterward it is a file in a directory, and backing it up is a separate act.

Same trade rc already took, and no production reader reaches config content
through the block store — every consumer goes through the VFS, including
`getConfig`, which a client calls at bootstrap before it has a context
(`crates/kaijutsu-server/src/rpc.rs:5589` reads via `vfs().read_all`). Nothing
breaks. It is named because "config is in the kernel's durable store" stops
being true, and someone will assume it still is.

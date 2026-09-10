# The config namespace — `/config` as a bind-mount registry

All four config trees — `/config/rc`, `/config/kernel`, `/config/client`,
`/config/midi` — are ordinary host directories reached through `LocalBackend`.
None of them has a config-owning backend or a document behind it; the kernel
once owned config as documents in its durable store, then gave that back
(`docs/devlog.md`, "Config: the kernel owned it, then gave it back"). This
continues `docs/rc-on-disk.md`, which melted rc first; here the other three
roots follow it under one mount registry.

## The rule

**A `/config` path is a well-known name. Where it comes from is a mount
declaration, not a compiled-in constant.**

```
/config/            # no backend of its own — listed from its mount points
    rc/             # lifecycle scripts
    kernel/         # theme.toml, mcp.toml, gate.toml, system.md
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

**Not `/etc`, because it is the host's.** The host's `/etc` is refused like
any other read-only host path, with no guard of its own — `/usr` and `/boot`
are equally the host's and equally covered by the read-only `/` mount
(`crates/kaijutsu-server/tests/config_mount_boot.rs`,
`the_hosts_etc_is_refused_by_the_read_only_root_like_any_host_path`).
Squatting `/etc` would have cost a guard drawing a line inside it that no
other host path needs.

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
(`crates/kaijutsu-kernel/src/vfs/mount.rs:844`). `readdir` merges a
backend's own entries with the mount points beneath it, and tolerates a backend
miss when synthetic children exist. So `ls /config` lists `rc`, `midi` and
`client` even where no backend serves `/config` itself.

**Freeze after bootstrap** (`mount.rs:119`) is already the right lifetime:
declare once at startup, immutable thereafter.

`create_shared_kernel` takes a `config_mounts: &ConfigMounts`
(`crates/kaijutsu-server/src/rpc.rs:2439`), the registry threaded declaratively
through bootstrap; `default_rc_dir()` (`crates/kaijutsu-server/src/ssh.rs:153`)
is a thin wrapper over it for the one caller that only wants rc's default.

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

## `client/default/`

`kaijutsu_types::paths::client_config_path` (`paths.rs:180`) maps `None` to
`<root>/default/<name>` and `Some(id)` to `<root>/<id>/<name>` — every
segment after the root is a client id, never a bare filename, so a client id
can never collide with a config filename the way it could on the earlier
`<root>/<name>` shape.

```
/config/client/default/metronome.toml     # the shared default
/config/client/<client-id>/metronome.toml # one client's override
```

The app's two-step resolution (`fetch_layered_config`,
`crates/kaijutsu-app/src/connection/actor_plugin.rs:591`) tries the client-id
path first and falls back to `default/`.

## Seeding needs no new machinery

Absent-only seeding is `ensure_rc_seed_files`
(`crates/kaijutsu-kernel/src/seed_scripts.rs:132`); reset-to-embedded is
`config_seed_body` (`crates/kaijutsu-kernel/src/config_seed.rs:129`); and
`config_seed_override` (`crates/kaijutsu-server/src/rpc.rs:1900`) reads a
config file from a host directory at bootstrap. Every root seeds from its
embedded default (`assets/defaults/`) while it is empty; a fresh
`--config-root` picks up the shipped defaults with no export, import, or
flag day. There is no other migration path: config's earlier life as
documents in `kernel.db` is not carried forward, on Amy's decision that
those documents were not worth one — see `docs/devlog.md`, "Config: the
kernel owned it, then gave it back."

- **`Kernel::invalidate_config_file_cache` (`kernel.rs:1875`) has nothing to
  do with `ConfigDocFs`.** It exists because a composition symlink's write
  can defeat the disk-generation staleness check `try_get_or_load` relies on
  for everything else, so a cache entry still resident in memory has no
  coherence signal telling it the file changed underneath it. `kj rc
  add`/`rm` and `kj config reset` call it explicitly for that reason.
- **`DocKind::Symlink` was not retired.** It survives as a persisted enum
  variant with no non-test callers. Removing it stays possible later (it
  still touches the schema, `migrate()` has no ALTER-TABLE path, and
  interface ordinals must stay sequential) but it is not part of this arc.

## Still open

- **Does the registry stay config-only?** The same mechanism could bind a
  workspace or a samples pool. Do not build for that; note it and see whether a
  second caller appears.

## The one thing that gets worse

Config lives outside `kernel.db`. A config edit is a file write in a host
directory, so a database backup does not carry it; backing up the config
root is a separate act (`docs/operating.md`). The documents config left
behind in `kernel.db` have no reader and are not migrated.

Same trade rc already took, and no production reader reaches config content
through the block store — every consumer goes through the VFS, including
`getConfig`, which a client calls at bootstrap before it has a context
(`crates/kaijutsu-server/src/rpc.rs:6337` reads via `vfs().read_all`). Nothing
breaks. It is named because "config is in the kernel's durable store" stops
being true, and someone will assume it still is.

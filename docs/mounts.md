# Mounts & subprocess — the opaque host

> **Status:** design direction; slice 1 (subprocess enablement) is built —
> see "Slice 1" below. The rest is direction, not commitment; code is truth.
> Companions: `docs/instrument-design.md` (the shared-trust doctrine this
> leans on), `docs/config-namespace.md` (the config mount registry, since
> melted onto host directories under `/config`). `/dev` already has a
> read-only, opaque-to-sweeps kernel mount (fixing an empty FSN view); a
> kaish-side rework making `/dev` writable enough for `> /dev/null` under
> our read-only root is still open, tracked as "Later slices" below.

## What a kernel mounts today

`create_shared_kernel` (`crates/kaijutsu-server/src/rpc.rs`) builds the whole
namespace and then calls `freeze_mounts()`. After that call the set is fixed:
the `mount` RPC refuses, and nothing a client says can widen it. The perimeter
is decided at boot, from the server's configuration.

| Path | Backend | Access |
|---|---|---|
| `/` | host root; `/proc` and `/sys` unlisted ("Unlisted paths" below) | read-only |
| `/dev` | host `/dev`, opaque to sweeps | read-only |
| `$HOME/src` | host directory | read-write |
| `/tmp` | host directory | read-write |
| `/config/<tree>` | the declared host directory (`docs/config-namespace.md`) | read-write |
| `/run/*`, `/v/cas`, `/r` | kernel-owned | as each backend says |

A recursive walk from `/` enters the host root and the read-write
directories and stops at `/config`, `/run`, `/v`, `/r`, and `/dev`; see
"Where a walk stops" below.

A context's cwd is where its relative paths resolve, and nothing more. It
may be read-only: a seat started in a directory it should only read is
told where to write. `kj context create --cwd` does not check that the
directory is writable. Amy, 2026-10-10: "cwd is just cwd, it's where the
model's paths return to."

`--rw-mount <DIR>` adds a host directory to that set, read-write, at the same
path inside the kernel: host `/app` is kernel `/app`. It is repeatable and
position-independent, and it is how a model gets write access to a workspace
outside `$HOME/src` and `/tmp`. `kaijutsu-solo-acp --mount <DIR>` is the same
seam (`SshServerConfig::rw_mounts`), and that binary also mounts the directory
it was launched in unless told not to (`docs/solo-acp.md`).

Each directory is checked before the freeze (`ssh::validate_rw_mounts`), and
one that fails refuses the boot, naming itself and the reason. It must be
absolute, exist, and be a directory; it may not be `/`, which would replace
the read-only root, and it may not be at or under `/config`, `/run`, `/v`,
`/r`, or `/dev`, which the kernel serves itself. The check is
component-correct: `/configuration` is an ordinary directory, not `/config`.

The check reads the path as written. It does not canonicalize, and the host
follows symlinks and `..` when the mount is used, so what a directory
resolves to is the operator's to know: `--rw-mount /home/u/link` where `link`
points at `/` does make the host root writable under that name. The reverse
is worth knowing too: a mount that contains a configuration tree's host
directory makes those files reachable read-write at a second kernel path,
while `/config/*` keeps winning for its own path by longest prefix. Both
follow from "reach, not a sandbox", and from the flag being something an
operator types.

Resolution is longest-prefix (`MountTable::owner_of`), so a read-write `/app`
wins over the read-only `/` for everything beneath it, and a nested pair
behaves the way it reads: with both `/app` and `/app/sub` mounted, `/app/sub/x`
goes to `/app/sub`.

A mount is reach, not a sandbox — the same doctrine the rest of this document
rests on. It decides what a model can see and resolve, not what a spawned
child process can do.

## When a write is refused

A write under a read-only mount names that mount and lists the mounts,
marking the writable ones `rw`, so one refusal shows where kaijutsu can
write. With the host root read-only and `/app` and `/tmp` writable:

```
touch: /git/x: invalid operation: kaijutsu mounts / read-only. Writable mounts are marked rw:
  /     ro  host
  /app  rw  host
  /tmp  rw  memory
```

The `write` and `edit` tools give the same text after the path:
`/git/server/hooks/post-receive: kaijutsu mounts / read-only. …`. The third
column says what serves the mount: `host` is a host directory, `memory` is
held in the kernel's memory, and `kernel` is a tree the kernel serves itself
(`/v/cas`, `/run/*`, `/r`). A directory that only the mount table knows,
such as `/v` or `/config`, belongs to the mount above it, so a write to
`/v/x` names `/`.

The list leaves out the `/config` trees, the kernel's own configuration,
unless one of them refused (Amy, 2026-10-03). It holds every other mount
when there are at most 10. A longer table keeps
the writable mounts and the mount that refused, and says how many read-only
mounts it left out: `12 read-only mounts not shown; run kaish-mounts to list
every mount`. A mount that is writable but refuses one path says
`the mount /config/rc is read-only at this path`.

The refusal is `MountTable`'s: a backend refuses with a bare
`VfsError::ReadOnly`, and the table replaces it with
`VfsError::ReadOnlyMount`, whose `kind()` is still `ReadOnly`. kaish's
`BackendError::ReadOnly` carries no text, so `MountBackend` hands the
refusal to kaish as `BackendError::InvalidOperation`, which is where the
`invalid operation:` prefix comes from. A redirect prints the text after
`redirect: PATH:` with no prefix. In a shell, `/dev`, `/v/docs`, `/v/jobs`,
and `/v/bin` are kaish's own mounts and refuse with kaish's text. Open
follow-ups: `docs/issues.md`, "Read-only refusals that name the mounts".

## Unlisted paths

The host's `/proc` and `/sys` are left out of the root listing
(`MountTable::unlist`, set at boot before the freeze, like the mounts). A
recursive walk that starts above an unlisted path never reaches it, because
walks reach a tree only through listings: `grep -r PATTERN /`, `find /`,
`ls -R /`, a `/**` glob, and the file tools' walks all skip it. Naming the
path still works: `ls /proc`, `cat /proc/self/status`, and
`grep -r PATTERN /proc/self` reach it as before. Unlisting hides; it does not
deny.

The reason is that several files there never end in practice. A whole read of
`/proc/<pid>/pagemap` returns 8 bytes for every page of a 47-bit address
space, about 256 GiB, and kaish's builtins read whole files into the kernel's
own process: a builtin `grep -r PATTERN /` was killed for memory with nothing
logged. A walk that names `/proc` can still do that; see `docs/issues.md`,
"A builtin `grep -r PATTERN /` killed the agent process".

An operator can unlist more paths at boot: `SshServerConfig::unlisted`,
which `kaijutsu-solo-acp --unlist <path>` fills. The Harbor adapter uses it
to keep its own log directory out of the model's walks
(`contrib/bench/harbor/README.md`, "Reading a run"). Each must be absolute
and not `/`, or the boot is refused.

The listing is the mount table's, so the FSN view and SFTP listings of `/`
leave the same paths out. In a kaish shell, `/dev` is kaish's own device
mount (`null`, `zero`, `random`, `urandom`; a whole read of an endless device
fails with an error), and `/v` lists kaish's mounts beside the kernel's:
unlisting either in kaijutsu does not hide it from a shell walk, because the
shell's view adds kaish's mounts back.

## Where a walk stops

A recursive walk passes through the host root and the workspace mounts and
stops at the kernel's own trees. `grep -r PATTERN /` searches `/usr`, `/tmp`,
`$HOME/src`, and every `--rw-mount` directory such as `/app`. It lists
`/config`, `/run`, `/v`, `/r`, and `/dev` but does not enter them, and says
so once on stderr, keeping its exit status:

```
grep: skipped mounts /config /dev /r /run /v (use --cross-mounts to enter)
```

Name a kernel tree to walk it: `grep -r PATTERN /v` walks `/v/cas`,
`/v/docs`, and kaish's `/v/jobs` alike. `--cross-mounts` on `grep`, `find`,
`ls -R`, `tree`, and `glob` crosses for one command, and `set -o crossmounts`
for the session. `find -xdev` stays in place even then. The rule is kaish's;
see kaish `help vfs`, "Walks stay in one mount".

The kernel trees are `KERNEL_ROOTS` (`kaijutsu_types::paths`), the same list
a `--rw-mount` may not land in. `MountBackend::walk_boundaries` reports them
to kaish, and the file tools' walks use the same list
(`MountTable::walk_boundaries`). A host mount is walked through wherever it
is mounted, so the host root and the workspace read as one tree, `/tmp`
included: it is the gateway between the host and the kernel. The server
refuses to boot when a backend with no host directory is mounted outside the
kernel trees, because a walk from `/` would enter it. A boundary is a rule
about kernel paths, not host content: the host directory behind
`/config/rc` stays reachable through the walked `/` wherever it lives on the
host.

Unlisting and walk boundaries do different jobs. An unlisted path (`/proc`,
`/sys`) is left out of its parent's listing, so nothing that lists `/` sees
it. A kernel tree stays listed, `ls /` shows it, and only a walk that did not
name it stops there.

## The inversion

Today the kernel mounts the **whole host** read-only (`kernel.mount("/",
LocalBackend::read_only("/"))` in `rpc.rs`) so that "ls /usr/bin, cargo,
etc." are visible — full visibility, and (until slice 1) zero capability.
The direction inverts the default:

- **Nothing visible by default.** Drop the host-root mount. The VFS
  namespace is the curated set: `/mnt/project` (or `~/src`), `/tmp`,
  `/config/rc` + `/config/kernel` (kernel-owned), `/v/*`, `/dev` (already a
  read-only kernel mount; writable enough for `> /dev/null` is still kaish
  work), plus the bin mounts below.
- **PATH dirs mounted deliberately.** At kernel startup, read the host
  `PATH`, canonicalize + dedupe its directories, and hold them as the
  kernel's *bin-mount catalog*. Contexts get them **surgically**: which
  bin mounts land in a given shell is a per-`context_type` decision made
  at materialization.
- **The namespace is the exec surface.** kaish's external resolution
  should walk `$PATH` *through the VFS* (stat candidates via the backend,
  then `resolve_real_path`) instead of searching host dirs directly — so
  "executable" becomes exactly "mounted". This is upstream kaish work
  (`try_execute_external`) and should ride the kaish mounts release train.

## What opacity means here (and what it doesn't)

VFS curation governs the **agent's view and what resolves** — a spawned
child still makes real syscalls against the real host (`aconnect` opens
`/usr/lib/*.so`, `/dev/snd`, regardless of our mounts). Under shared trust
(`docs/instrument-design.md`) that is fine and stated plainly: mounts are
an **ergonomic nudge, not a sandbox**. A musician that can't *see* or
*resolve* `rm` won't reach for it; that's the footgun-prevention we want,
and all we claim.

If child-side opacity is ever wanted, the same curated mount set is the
natural *generator* for a bwrap/landlock spec (mount table → sandbox
profile). Named here so nobody reinvents it; deliberately not now.

## The per-context seam

The kernel `MountTable` is **shared** (one kernel-owned table for every
context), but each materialized shell gets its own kaish `VfsRouter`
(where `/v/docs` already mounts per-shell,
`embedded_kaish.rs`). That router is the curation seam: per-context bin
mounts ride the shell's router, and the shared table keeps the
project/config/kernel-owned mounts. Sketch of the per-type exposure:

| context_type | bin mounts | rationale |
|---|---|---|
| coder | full PATH catalog | builds, git, toolchain — the working seat |
| mcp / default | full PATH catalog | the producer seat drives real work |
| director | curated utility set | operational reach (aconnect, pw-cli), not builds |
| musician | none | musical time only; a subprocess is never its move |
| toolie | none (read-only shell) | structural, already enforced |

Which set a type gets should be rc/loadout-driven (the same place the
`exec` authority is granted), so a live director can be widened without a
rebuild.

## Slice 1 — subprocess enablement (built)

The minimal cut, independent of the kaish release; everything in it
survives the inversion (later slices only narrow what's visible and what
`$PATH` contains):

- **`subprocess` cargo feature on** (`Cargo.toml` workspace dep). Without
  it, kaish's `try_execute_external` was a stub — every external command,
  absolute paths included, fell through to `command not found`.
- **`ExternalExec` policy at materialization**
  (`runtime/embedded_kaish.rs`): every shell states `Deny` or
  `Allow { path }` explicitly — deny-by-default, never inherited from
  kaish's feature-driven default. Read-only shells pin `Deny`
  structurally (the sandbox's fourth lever).
- **`exec` loadout authority** (`Capability::Exec`, token `"exec"`) —
  like every authority, **not implied by `*`**. Granted in the rc seeds
  to the broad roles (`lib/create/S10-binding.kai` → coder/mcp/default)
  and to `director`; musician/toolie never carry it. The gate is applied
  in `EmbeddedKaish::for_context`: no binding or no grant → `Deny`.
- **`MountBackend::resolve_real_path`** now resolves (it returned `None`
  unconditionally): sync mount-table walk
  (`MountTable::resolve_real_path_sync`, longest-prefix owner + the new
  sync `VfsOps::real_root`). Virtual cwds (`/v/*`, kernel-owned mounts) still
  yield `None` → external exec is skipped there, correctly.
- **`$PATH` seeded from the kernel process env** (`Kernel::host_path`,
  captured once) into exec-granted shells only. kaish never reads OS env
  itself.

Deploy note: rc seeds are **once-only** on a fresh kernel — the live
kernel's copies of `lib/create/S10-binding.kai` and
`director/create/S10-binding.kai` need `kaijutsu-server rc reseed --force`
(or a plain file edit — rc is a host file now, `docs/rc-on-disk.md`) to pick
up the `exec` grant, and only *newly created* contexts run
create-rc; existing coder/director contexts need a one-time
`kj binding allow "exec"` from a binding-admin context.

## Later slices (direction)

1. **Bin-mount catalog** — kernel startup reads `PATH`, mounts each dir
   read-only into the shells that get them (per-type, via the
   materialization seam). Namespace TBD (`/host/bin/NN-<name>` vs a
   union view; note kaish reserves `/v/bin/` for builtin dispatch —
   don't collide).
2. **VFS-mediated resolution in kaish** — `try_execute_external` walks
   `$PATH` through the backend (stat + `resolve_real_path` per
   candidate). Lands upstream with the kaish mounts release; until then
   slice 1's host-PATH resolution is the interim.
3. **Drop the host-root mount** — the actual inversion, once 1+2 make
   the curated namespace sufficient. `kj mount`-style surgical exposure
   (add a host tree to one context's view) probably arrives here.
4. *(optional, far)* **Generated sandbox profiles** — bwrap/landlock spec
   derived from the mount table, for child-side opacity where a
   deployment wants it.

## Open questions

- Does `mcp`/`default` keep the full catalog, or narrow once the catalog
  exists? (Slice 1 grants them exec via the shared lib seed.)
- `kj audio` / `kj midi` verbs: still worth having so a *musician-adjacent*
  flow never needs raw `aconnect`, even with exec working — the ALSA wiring
  is kernel-owned state, not a shell errand.
- The `aconnect 128:0 129:0` wire itself: nothing owns it; it dies on every
  app restart. The app auto-connecting its render port to TiMidity (when
  present) is likely the right home.

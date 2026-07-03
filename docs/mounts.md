# Mounts & subprocess — the opaque host

> **Status:** design direction, captured 2026-07-03 in a co-design session
> (Amy + Claude), after the Music Demo #1 post-mortem found external exec
> compiled out three layers deep. **Slice 1 (subprocess enablement) shipped
> the same day** — see "Slice 1" below. The rest is direction, not
> commitment; code is truth. Companions: `docs/instrument-design.md` (the
> shared-trust doctrine this leans on), `docs/config-crdt-ownership.md`
> (the CRDT mounts that stay untouched by all of this), and the kaish-side
> mounts rework in flight for the next kaish release (`/dev` kernel-owned in
> the `with_backend` path — fixes `> /dev/null` under our read-only root).

## The inversion

Today the kernel mounts the **whole host** read-only (`kernel.mount("/",
LocalBackend::read_only("/"))` in `rpc.rs`) so that "ls /usr/bin, cargo,
etc." are visible — full visibility, and (until slice 1) zero capability.
The direction inverts the default:

- **Nothing visible by default.** Drop the host-root mount. The VFS
  namespace is the curated set: `/mnt/project` (or `~/src`), `/tmp`,
  `/etc/rc` + `/etc/config` (CRDT), `/v/*`, `/dev` (kaish-owned, next
  release), plus the bin mounts below.
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

The kernel `MountTable` is **shared** (one CRDT-backed table for every
context), but each materialized shell gets its own kaish `VfsRouter`
(where `/v/docs` / `/v/input` already mount per-shell,
`embedded_kaish.rs`). That router is the curation seam: per-context bin
mounts ride the shell's router, and the shared table keeps the
project/config/CRDT mounts. Sketch of the per-type exposure:

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

## Slice 1 — subprocess enablement (SHIPPED 2026-07-03)

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
  in `materialize_context_kaish_inner`: no binding or no grant → `Deny`.
- **`MountBackend::resolve_real_path`** now resolves (it returned `None`
  unconditionally): sync mount-table walk
  (`MountTable::resolve_real_path_sync`, longest-prefix owner + the new
  sync `VfsOps::real_root`). Virtual cwds (`/v/*`, CRDT mounts) still
  yield `None` → external exec is skipped there, correctly.
- **`$PATH` seeded from the kernel process env** (`Kernel::host_path`,
  captured once) into exec-granted shells only. kaish never reads OS env
  itself.

Deploy note: rc seeds are **once-only** on a fresh kernel — the live
kernel's CRDT copies of `lib/create/S10-binding.kai` and
`director/create/S10-binding.kai` need a `kj rc reset <path>` (or live
edit) to pick up the `exec` grant, and only *newly created* contexts run
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
- `kj audio` / `kj midi` verbs (issues.md, from Music Demo #1): still
  worth having so a *musician-adjacent* flow never needs raw `aconnect`,
  even with exec working — the ALSA wiring is kernel-owned state, not a
  shell errand.
- The `aconnect 128:0 129:0` wire itself: nothing owns it; it dies on
  every app restart. The app auto-connecting its render port to TiMidity
  (when present) is likely the right home — tracked in issues.md.

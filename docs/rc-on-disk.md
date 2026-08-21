# Config on disk — melting the kernel-owned trees to real files

**Status: ruled, unbuilt.** Amy ruled the shape on 2026-08-21; no code has
moved. `docs/config-ownership.md` still describes the live system.

    ~/.config/kaijutsu/etc/rc/coder/create/S00-stance.kai

rc scripts become ordinary host files under `~/.config/kaijutsu/etc/rc/`,
mounted at `/etc/rc` through `LocalBackend`. The kernel reads the latest bodies
from disk at the start of every lifecycle run and records what it ran in the
approval ledger's content-addressed store. Git is optional and unmanaged by us.

## The four decisions

1. **Location: `~/.config/kaijutsu/etc/rc/`**, configurable later. *"Users can
   always use git there, or they can remap. defaults should just work with or
   without git."* The kernel never runs git and never requires a repo — ours is
   shared from `~/.config` through a local gitea, which is a user's choice about
   a directory, not a kernel feature.
2. **Bodies are stored on use.** `rc_run_scripts.body_sha256 →
   script_bodies(sha256)` already exists and already dedupes by content. It
   stays. A hash nothing can resolve is a hash of nothing: git answers only for
   committed edits, and the record earns its place on the uncommitted one.
3. **`rc-write` is dropped.** Its justification was that rc is executable
   rather than data. Once rc is a directory the file tools reach, there is no
   chokepoint left for the capability to sit on, and keeping it would be a gate
   that gates nothing.
4. **Hooks follow rc onto disk.** A hook body becomes a path read at call time,
   not a body snapshotted at install time, with the same digest recorded per
   call.

## Why this is mostly deletion

The execution path is already backend-agnostic and the disk path is already the
one most tests run on.

- **`load_rc_scripts` never learns what backs the mount.** `kj/lifecycle.rs:345`
  is `vfs.readdir` + `vfs.read_all`. It also reads every body into a
  `Vec<RcScript>` before the first script executes, so a run is snapshotted as a
  set and a script editing its neighbor mid-run cannot tear it. That is the
  "load the latest from disk before running" semantics already, against a
  different backend.
- **The broadly-used test dispatcher already mounts `/etc/rc` from
  `LocalBackend`** over a real host directory (`kj/mod.rs:988`), seeded by
  `seed_scripts::ensure_rc_seed_files` (`seed_scripts.rs:125`) — an
  install-if-absent disk seeder that already skips paths that exist. The
  document-backed `ConfigDocFs` is the path needing its own special tests.
- **The ledger record is built.** `approval-ledger/src/schema.rs:602` — one row
  per script per run, in order, pointing at a deduped body, `exit_code` and
  timings alongside.

## Slices

1. **Point production at disk.** Mount `LocalBackend` on `~/.config/kaijutsu/
   etc/rc` (path from config, with the default above), seed with
   `ensure_rc_seed_files`, delete the rc half of `ConfigDocFs`. Symlinks become
   real symlinks, which retires `DocKind::Symlink` for rc and the `read_all`
   symlink-sizing override.
2. **Drop `rc-write`.** 58 hits across 14 files, mechanical: `kaijutsu-types/
   src/paths.rs`, `file_tools/{path,guard}.rs`, `kj/{rc,config,binding,editor,
   mod}.rs`, `mcp/binding.rs`, `runtime/config_doc_fs.rs`, `kernel_db.rs`,
   `kaijutsu-server/src/{rpc,sftp}.rs`, `tests/rc_role_bindings.rs`.
3. **Shrink `kj rc`.** `reseed --overwrite` and its unified-diff machinery go —
   `git diff` is the diff, and reseed returns to install-if-absent. `kj rc
   edit`/`reset` lose their reason to exist once the file tools and the editor
   reach the files directly; keep `list` while it still reports something the
   filesystem does not.
4. **Record git provenance next to the digest.** Commit sha plus a dirty flag on
   the run row. Clean tree, the sha is the answer and git owns the history;
   dirty tree, `script_bodies` keeps the record honest. Optional by
   construction — no repo means no sha, never an error.
5. **Hook bodies become path references.** Read at call time, digest recorded
   per call. This deletes a live gotcha: today a hook body is snapshotted into
   its row at context create, so updating the script does not update the running
   hook.

   The invariant this reverses is written down and defended in code, so it is
   not enough to change the read: `mcp/broker.rs:416` deliberately does **not**
   re-resolve `action_kaish_script_id` against `hook_scripts` on restart,
   precisely so "script edits leak into existing hooks" cannot happen. That
   comment states the old rule. Under this slice, leaking edits into existing
   hooks is the *point*, and `hooks.action_kaish_body` stops being the source of
   truth. Retire the snapshot rule and its comment in the same change.

## All four roots, and `ConfigDocFs` goes away

**Ruled 2026-08-21.** `ConfigDocFs` serves `/etc/rc`, `/etc/config`,
`/etc/client` and `/etc/midi`. All four become host directories under
`~/.config/kaijutsu/etc/`, and `ConfigDocFs` is deleted rather than left
serving a shrinking set of roots.

rc is the one with executable semantics and a lifecycle, so it is the harder
melt and the one this document details. The other three are plain data on a
write surface that already has no `config-write` capability and no `kj config
set`/`edit` — for them the melt is close to just changing what backs the
mount.

Sequence rc first anyway: it is the one that can go wrong in an interesting
way, and getting it right teaches the other three.

## Open

Nothing is waiting on a ruling. The work is unbuilt.

**Multi-machine.** kaijutsu runs on moltar, zorak and a MacBook Pro, each with
its own kernel documents today. A shared `~/.config` checkout is the first
design where an rc change can travel between them. Nothing here depends on
that; it is what the location makes possible.

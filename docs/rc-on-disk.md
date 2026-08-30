# Config on disk — melting the kernel-owned trees to real files

**Status: slices 1, 2, 3 and 5 shipped; slice 4 open.** Amy ruled the shape
on 2026-08-21. Production mounts `/config/rc` from a host directory,
`rc-write` is deleted, `kj rc` is down to `add`/`list`/`rm`/`show`, and hook
bodies are path references read at fire time. `docs/config-namespace.md`
is canonical for `/config/kernel`, `/config/client` and `/config/midi`, and
for the mount registry all four roots live under.

**Reseeding is routine, not a rescue** (Amy, 2026-08-29): *"my rc is in the
code ... for the foreseeable future, we will reseed regularly."* So the host
tree is a materialization of the in-repo seed, and `kaijutsu-server rc reseed
--force` is an ordinary operation to design around. The snowflake TOML under
`/etc/config` is the opposite case — local, secret-bearing, never reseeded
over — which is one more reason the four roots do not all melt the same way.

**Amy ruled the git question on 2026-08-28: plain files, the kernel never
runs git.** `crates/kaijutsu-configgit` — the git-worktree write seam, built
and never wired — is retired rather than left on the shelf. Git stays a
choice about a directory (ours is shared from `~/.config` through a local
gitea), never a mechanism the kernel performs. That closes the question the
earlier kernel-owned design had left open.

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
3. **`rc-write` is dropped**, and not because it stopped working. The guard
   is path-based — `is_rc_path` plus a loadout check (`file_tools/guard.rs`) —
   so it sits above `VfsOps` and works identically over a `LocalBackend`
   mount. An earlier draft of this decision claimed the melt left "no
   chokepoint for the capability to sit on"; that was wrong, and it is
   corrected here rather than quietly deleted, because the ruling was made on
   other grounds and should not appear to rest on a false one.

   The real reason is that the capability stopped naming a real distinction.
   `rc-write` existed to say rc is executable and config is data, so the two
   deserve different write surfaces. Once both are host files under
   `~/.config/kaijutsu/etc/`, a player edits rc the way it edits anything —
   the file tools, the editor, `vim` on the host, an sftp client, git. The
   loadout check covers exactly one of those paths, so what it delivers is
   not "rc is protected" but "rc is protected from the one player who
   announced itself." An ergonomic nudge that a `vim` in the next terminal
   walks past is not a nudge; it is a false reading of the system that costs
   a capability to maintain.

   What replaces it is what the melt made available: rc is a directory, so a
   mistaken edit is visible in `git diff` and undone by `git checkout` — a
   better answer than a denial, because it is recoverable *and* legible after
   the fact, and the ledger already records what actually ran
   (`rc_run_scripts.body_sha256`). Prefer deleting a mechanism to
   generalizing it.
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
2. **Drop `rc-write`.** Mechanical, across: `kaijutsu-types/
   src/paths.rs`, `file_tools/{path,guard}.rs`, `kj/{rc,config,binding,editor,
   mod}.rs`, `mcp/binding.rs`, `runtime/config_doc_fs.rs`, `kernel_db.rs`,
   `kaijutsu-server/src/{rpc,sftp}.rs`, `tests/rc_role_bindings.rs`.
3. **Shrink `kj rc`. Shipped.** `edit`, `reset` and `reseed` are deleted —
   with `reseed` went its unified-diff machinery, because on disk `git diff` is
   the diff, and reseeding is `kaijutsu-server rc reseed [--force]` off the
   kernel. `add`, `rm` and `show` stay; `list` earns its keep because it marks
   each entry against its embedded seed (in-sync / differs / no-seed /
   not-installed / dangling), which the filesystem cannot report.

   Two things fell out that were not planned for. `kj rc`'s call into
   `config_doc_fs::seed_link_target` went with `reset`, leaving the disk seeder
   as its only remaining caller — the `ConfigDocFs` blocker is that much
   smaller. And the lexical deny on SFTP writes to `/etc/rc` and `/etc/config`
   (`privileged_write_denied`) was deleted in the same pass: it existed to stop
   an SFTP write bypassing `RcWrite`/`ConfigWrite`, and neither gates a file
   write any more, so it was denying writes every other path already allowed.
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

**Superseded 2026-08-29 by `docs/config-namespace.md`, which is canonical for
the remaining three roots and for where all four end up.** What changed: the
trees do not stay under `/etc`, and their host location stops being a
compiled-in constant. They become `/config/rc`, `/config/midi`,
`/config/client` and the base `/config`, each a well-known name whose host
directory is a mount declaration. Squatting `/etc` cost a guard
(`deny_etc_write`) that exists for no other reason.

**Ruled 2026-08-21.** `ConfigDocFs` serves `/etc/rc`, `/etc/config`,
`/etc/client` and `/etc/midi`. All four become host directories, and
`ConfigDocFs` is deleted rather than left serving a shrinking set of roots.

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

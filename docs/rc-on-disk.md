# Config on disk — melting the kernel-owned trees to real files

Production mounts `/config/rc` from a host directory through `LocalBackend`.
`rc-write` is deleted; `kj rc` is down to `add`/`list`/`rm`/`show`; a hook
body can be a path reference read at fire time
(`HookBody::KaishPath`) alongside the older snapshot-at-install variant.
Git provenance next to the run-row digest is the one piece still open (see
"Open" below). `docs/config-namespace.md` is canonical for `/config/kernel`,
`/config/client` and `/config/midi`, and for the mount registry all four
roots live under; this doc stays canonical for `/config/rc`, the one root
with executable semantics and a lifecycle. `docs/devlog.md`, "Config: the
kernel owned it, then gave it back" has the story of the design this melt
replaced.

**Reseeding is routine, not a rescue** (Amy, 2026-08-29): *"my rc is in the
code ... for the foreseeable future, we will reseed regularly."* So the host
tree is a materialization of the in-repo seed, and `kaijutsu-server rc reseed
--force` is an ordinary operation to design around. The snowflake TOML under
`/config/kernel` is the opposite case — local, secret-bearing, never reseeded
over — which is one more reason the four roots do not all melt the same way.

**Amy's guidance on the git question (2026-08-28): plain files, the kernel
never runs git.** `crates/kaijutsu-configgit` — the git-worktree write seam, built
and never wired — is retired rather than left on the shelf. Git stays a
choice about a directory (ours is shared from `~/.config` through a local
gitea), never a mechanism the kernel performs. That closes the question the
earlier kernel-owned design had left open.

    ~/.config/kaijutsu/config/rc/coder/create/S00-stance.kai

rc scripts are ordinary host files under `~/.config/kaijutsu/config/rc/` (the
`<config-root>/<name>` default; `docs/config-namespace.md`'s mount registry
can point `/config/rc` anywhere), mounted at `/config/rc` through
`LocalBackend`. The kernel reads the latest bodies from disk at the start of
every lifecycle run and records what it ran in the approval ledger's
content-addressed store. Git is optional and unmanaged by us.

## The four decisions

1. **Location: `~/.config/kaijutsu/config/rc/`** (`~/.config/kaijutsu/etc/rc/`
   at the time of this decision — the tree renamed off `/etc` in the melt below),
   configurable later. *"Users can always use git there, or they can remap.
   defaults should just work with or without git."* The kernel never runs git
   and never requires a repo — ours is shared from `~/.config` through a local
   gitea, which is a user's choice about a directory, not a kernel feature.
   "Configurable later" landed as the mount registry — `--mount` and
   `mounts.toml` remap the default (`docs/config-namespace.md`).
2. **Bodies are stored on use.** `rc_run_scripts.body_sha256 →
   script_bodies(sha256)` already exists and already dedupes by content. It
   stays. A hash nothing can resolve is a hash of nothing: git answers only for
   committed edits, and the record earns its place on the uncommitted one.
3. **`rc-write` is dropped**, and not because it stopped working. The guard
   is path-based — `is_rc_path` plus a loadout check (`file_tools/guard.rs`) —
   so it sits above `VfsOps` and works identically over a `LocalBackend`
   mount. An earlier draft of this decision claimed the melt left "no
   chokepoint for the capability to sit on"; that was wrong, and it is
   corrected here rather than quietly deleted, because the decision was made on
   other grounds and should not appear to rest on a false one.

   The real reason is that the capability stopped naming a real distinction.
   `rc-write` existed to say rc is executable and config is data, so the two
   deserve different write surfaces. Once both are host files under
   `~/.config/kaijutsu/config/`, a player edits rc the way it edits anything —
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

- **`load_rc_scripts` never learns what backs the mount.** `kj/lifecycle.rs:353`
  is `vfs.readdir` + `vfs.read_all`. It also reads every body into a
  `Vec<RcScript>` before the first script executes, so a run is snapshotted as a
  set and a script editing its neighbor mid-run cannot tear it. That is the
  "load the latest from disk before running" semantics already, against a
  different backend.
- **The broadly-used test dispatcher already mounts `/config/rc` from
  `LocalBackend`** over a real host directory (`kj/mod.rs:1116`), seeded by
  `seed_scripts::ensure_rc_seed_files` (`seed_scripts.rs:132`) — an
  install-if-absent disk seeder that already skips paths that exist. The
  document-backed `ConfigDocFs` was the path that had needed its own special
  tests; those fixtures were migrated onto real host directories and real
  symlinks when `ConfigDocFs` was deleted, which also surfaced a bug the
  document-backed fixtures had been masking: `rc_seed_status`
  compared a seed body's absolute path against a live symlink's host-relative
  target, so every composed rc script reported "differs from seed" until real
  symlinks made the mismatch visible.
- **The ledger record is built.** `approval-ledger/src/schema.rs:725` — one row
  per script per run, in order, pointing at a deduped body, `exit_code` and
  timings alongside.

## Slices

1. **Point production at disk — done.** `LocalBackend` mounts
   `~/.config/kaijutsu/config/rc` (path from config, with the default
   above), seeded with `ensure_rc_seed_files`. Symlinks are real symlinks,
   which retired `DocKind::Symlink` for rc and the `read_all`
   symlink-sizing override.
2. **Drop `rc-write` — done.** `RcWrite` and `context_allows_rc_write` no
   longer exist anywhere in the tree.
3. **Shrink `kj rc` — done.** `edit`, `reset` and `reseed` are deleted —
   with `reseed` went its unified-diff machinery, because on disk `git diff` is
   the diff, and reseeding is `kaijutsu-server rc reseed [--force]` off the
   kernel. `add`, `rm` and `show` stay; `list` earns its keep because it marks
   each entry against its embedded seed (in-sync / differs / no-seed /
   not-installed / dangling), which the filesystem cannot report. The lexical
   deny on SFTP writes to the config trees (`privileged_write_denied`) is
   deleted too: it existed to stop an SFTP write bypassing `RcWrite`/
   `ConfigWrite`, and neither gates a file write any more, so it was denying
   writes every other path already allowed.
4. **Record git provenance next to the digest — open.** Commit sha plus a
   dirty flag on the run row. Clean tree, the sha is the answer and git owns
   the history; dirty tree, `script_bodies` keeps the record honest.
   Optional by construction — no repo means no sha, never an error.
5. **Hook bodies become path references — done.** `HookBody::KaishPath`
   reads the body fresh from the VFS at every fire
   (`Broker::read_kaish_hook_body`) instead of snapshotting it into the row
   at install time, so editing the script updates the running hook. The
   older `HookBody::Kaish` snapshot variant still exists alongside it
   (`mcp/broker.rs`) — deliberately: re-resolving *that* variant's body on
   restart would let script edits leak into an install that asked for a
   snapshot, which is the invariant `hydrate_hooks_from_db`'s comment
   still defends for it. `hooks.action_kaish_body` is not the source of
   truth for a `KaishPath` hook — the path is.

## All four roots, and `ConfigDocFs`

All four roots — `/config/rc`, `/config/kernel`, `/config/client`,
`/config/midi` — mount `LocalBackend` over an ordinary host directory. None
of them sit under `/etc`, and none of their host locations is a compiled-in
constant; each is a well-known name whose host directory is a mount
declaration (`docs/config-namespace.md`). `ConfigDocFs` — the
document-backed backend all four trees mounted before this melt, along with
the `owns_config_docs`/`config_owned` predicates that answered for it and
its `config_doc.rs` kernel-document model — no longer exists in the tree,
and no migration ran against the documents it used to serve
(`docs/config-namespace.md`, "There is no migration").

rc is the one with executable semantics and a lifecycle, so it is the harder
melt and the one this document details. The other three are plain data on a
write surface that already has no `config-write` capability and no `kj config
set`/`edit` — for them the melt is close to just changing what backs the
mount. rc came first anyway: it is the one that can go wrong in an
interesting way, and getting it right taught the other three.

## Open

Nothing is waiting on a decision. The work is unbuilt.

**Multi-machine.** kaijutsu runs on moltar, zorak and a MacBook Pro, each with
its own local `~/.config/kaijutsu/config/` directory today. A shared
`~/.config` checkout is the first design where an rc change can travel
between them. Nothing here depends on that; it is what the location makes
possible.

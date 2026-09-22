# Offline administration: `kaijutsu-server kj`

Status: chosen by Amy 2026-09-22 ("Let's do option a for kj on the cli,
seems safer over time"), and shipped.

The server binary runs any `kj` verb against a stopped kernel:

```sh
systemctl --user stop kaijutsu-server
kaijutsu-server kj -- character create bob --root
kaijutsu-server kj -- backend set deepseek --api-key-file ~/.deepseek-key
kaijutsu-server kj --as amy -- context create banto --type director --as banto
systemctl --user start kaijutsu-server
```

It boots the same kernel the service boots, in process, dispatches the
argv through `KjDispatcher::dispatch` as the named root character in that
character's root context, prints the result, settles the kernel, and exits.
Every verb, every capability check, and every rc lifecycle behave exactly as
they do for that person typing in their root console over SSH. Nothing is
mirrored by hand, so a verb change reaches the offline path with no edit
here.

`init`, `add-key`, `list-keys`, and `list-characters` stay as they are: they
write or read the databases directly and work when the kernel refuses to
boot, which is the lockout path (`docs/character.md`, "Bootstrap").

## Rules

- **One kernel per data directory.** The running server and `kj` both take
  an exclusive advisory lock on `<data_dir>/kernel.lock` for the life of the
  process. The second holder refuses at once, naming the lock path and the
  fix (stop the service, or wait for the other `kj`). Two kernels over one
  `kernel.db` would both run rc and both own the worker pool.
- **The caller is a root character.** `--as <name>` names it; with one live
  root character the flag may be omitted, and with several the command
  refuses and lists them. The caller's actor is that principal, its
  reviewer is unset (a root confirms its own statements), and its context
  is the character's root context. `--context <label|id>` runs the verb
  from another context instead, the way `kj` resolves `.` and labels.
- **Boot is the server's boot, minus serving.** `create_shared_kernel` with
  the same arguments `run_server` passes, on a runtime whose worker threads
  have `KAISH_RC_THREAD_STACK`. Root contexts that do not exist yet are
  created, so `init` followed by `kj` gives the root its console. External
  MCP servers do not start (`start_external_mcp_servers` is a serving-path
  step), the SSH listener does not open, and the beat scheduler does not
  run. An unreachable embedding service warns, as it does at boot.
- **Exit settles the kernel.** After the verb returns, stop admission and
  await `shutdown_runtime_worker`, the same steps `spawn_signal_shutdown`
  takes on SIGTERM. A verb that admits a model turn, such as `kj drive`,
  therefore holds the command until the turn ends; say so in the help.
- **Output.** The result's message goes to stdout, `--json` prints the
  result's `.data` as JSON instead, errors go to stderr, and the exit code
  is 0 for ok, 1 for a `KjResult::Err`, and 2 for a Destroy verb's
  confirmation required — the same code `kj` already uses through kaish
  (`runtime/kj_builtin.rs::latch_result`). Verbs go straight to the
  dispatcher, not through a shell, so the gate and its asks are not on this
  path; a Destroy verb still needs `--confirm`.
- **The server CLI is clap.** The hand-rolled `match` in `main.rs` becomes
  a clap `Parser` with subcommands `init`, `add-key`, `list-keys`,
  `list-characters`, `migrate-keyring`, `rc reseed`, `kj`, and the serve
  default with `--port`. `///` on fields is the published help; keep the
  existing names, flags, and the `--config-root`/`--mount` globals. Read
  the emitted `--help` after.

## Implementation

`crates/kaijutsu-server/src/offline.rs` holds `KernelLock` (an exclusive
`flock` via `libc`, already a direct dependency of several other crates in
this workspace — no new crate in the tree) and `run_kj`, the boot-dispatch-
settle runner. `ssh.rs`'s `run_on_listener_inner` takes the same lock right
before its own `create_shared_kernel` call, so the serving path and `kj`
agree on one guard per data directory. `main.rs` is a clap `Parser`; real-
binary coverage lives in `tests/offline_kj.rs`.

Out of scope: running `kj` against a live server (that is SSH), a
daemonized mode, and moving `init` onto the dispatcher.

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
`migrate-keyring` also writes `kernel.db` directly, but only ever once
against a live root, so it is not a lockout tool — it takes the same
`KernelLock` `kj` and the service take, and needs the service stopped.

## Rules

- **One kernel per data directory.** The running server and `kj` both take
  an exclusive advisory lock on `<data_dir>/kernel.lock` for the life of the
  process. The second holder refuses at once, naming the lock path and the
  fix (stop the service, or wait for the other `kj`) when the lock is held —
  `flock`'s `EWOULDBLOCK`. Any other errno is reported as the OS error it is,
  naming the lock path, rather than claimed as contention. Two kernels over
  one `kernel.db` would both run rc and both own the worker pool.
- **The caller is a root character.** `--as <name>` names it; with one live
  root character the flag may be omitted, and with several the command
  refuses and lists them. `--as` refuses a retired root by name — retiring
  ends a character's own turn, not just its context work. The caller's actor
  is that principal, its reviewer is unset (a root confirms its own
  statements), and its context is the character's root context.
  `--context <label|id>` runs the verb from another context instead, the way
  `kj` resolves `.` and labels.
- **Boot is the server's boot, minus serving.** `create_shared_kernel` with
  the same arguments `run_server` passes, on a runtime whose worker threads
  have `KAISH_RC_THREAD_STACK`. Root contexts that do not exist yet are
  created, so `init` followed by `kj` gives the root its console. External
  MCP servers do not start (`start_external_mcp_servers` is a serving-path
  step), the SSH listener does not open, and the beat scheduler does not
  run. An unreachable embedding service warns, as it does at boot. A
  beat-bearing `context_type` (musician and similar) is therefore created
  unarmed: its create-lifecycle `kj transport attach` needs the live
  scheduler and leaves an Error block instead. It arms at the next serving
  boot, or by hand with `kj transport attach` once the service is running.
- **`kj ledger allow|deny` needs a live kernel.** Both are refused before
  boot, exit 1: answering an ask offline would record a durable answer with
  no approval-delivery worker running to act on it
  (`Kernel::start_approval_delivery` is a serving-path step), and the next
  serving boot retires the ask unexecuted (`approval_resume.rs`,
  `recover_unpublished_pairs`). Asks are answered on the running kernel over
  SSH.
- **Exit settles the kernel.** After the verb returns, stop admission, wait
  for every admitted turn to finish, and await `shutdown_runtime_worker` —
  the same join `spawn_signal_shutdown` performs on SIGTERM, plus the wait.
  `shutdown_runtime_worker` cancels the runtime pool's token, which an
  admitted turn treats as a hard interrupt, so settling before every turn has
  finished would cut it short instead of letting it conclude. A verb that
  admits a model turn, such as `kj drive`, therefore really does hold the
  command until the turn ends. A failed settlement exits 1 even when the
  verb itself succeeded, the same as the SIGTERM path.
- **Output.** The result's message goes to stdout, `--json` prints the
  result's `.data` as JSON instead, errors go to stderr, and the exit code
  is 0 for ok, 1 for a `KjResult::Err` or a failed settlement, and 2 for a
  Destroy verb's confirmation required — the same code `kj` already uses
  through kaish (`runtime/kj_builtin.rs::latch_result`). Verbs go straight to
  the dispatcher, not through a shell, so the gate and its asks are not on
  this path; a Destroy verb still needs `--confirm`.
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

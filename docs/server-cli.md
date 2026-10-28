# Offline administration: `kaijutsu-server kj` and `bootstrap`

Status: chosen by Amy 2026-09-22 ("Let's do option a for kj on the cli,
seems safer over time"), and shipped.

The server binary runs any `kj` verb against a stopped kernel:

```sh
systemctl --user stop kaijutsu-server
kaijutsu-server kj -- character create bob --root
kaijutsu-server kj -- backend set deepseek --api-key-file ~/.deepseek-key
kaijutsu-server kj --as amy -- kohai create banto --type director --as banto
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

## `bootstrap`

`bootstrap` sets up a kernel so its `ROOT` console can answer. A new person
runs it once and then talks to `ROOT`; the model there helps with the rest
of the setup. `kj` is still there for direct changes.

```sh
kaijutsu-server bootstrap
kaijutsu-server bootstrap --as amy --key ~/.ssh/id_ed25519.pub --backend deepseek --model deepseek-v4-pro
kaijutsu-server bootstrap --no-model
```

It writes the root character and its key directly, as `init` does. Then it
boots the kernel the way `kj` does and runs, as that root, each verb whose
result is missing:

1. `kj backend set` for each provider whose key it finds: `ANTHROPIC_API_KEY`
   or `~/.anthropic-key.txt`, and `DEEPSEEK_API_KEY` or `~/.deepseek-key`.
   `--backend` names one instead. The row records both key sources, so the
   kernel finds the key wherever it runs.
2. `kj backend model set` with the context window of a model it knows. For
   a model it does not know, the report names the model and this verb.
3. `kj backend default set`, when the kernel has no default, to the first
   provider found.
4. `kj character create root`, the model character that plays `ROOT`
   (`--console-as` names another).
5. `kj context create ROOT --type console --as root`, under the root context.

It prints each line it ran. A second run runs nothing, and a kernel set up by
hand is completed rather than overwritten. With no key found, it still
creates `ROOT` and prints the two `kj backend` lines that give it a model.

Run it with the service stopped: it takes the same lock as the service and
`kj`. The defaults are `--as $USER` and `--key ~/.ssh/id_ed25519.pub`. It
reads `--config-root`, `--mount`, and `--rw-mount` like `kj`. Tests call the
same code, `kaijutsu_server::bootstrap::bootstrap`, on a test server's
paths.

## `blocks repair-order`

Repairs blocks stored out of order before `855ace8a`, when `order_midpoint`
could return a key below its lower bound. It reads every conversation
document, archived ones included, and finds tool results that sort before
their call. Without `--apply` it opens `kernel.db` read-only and reports;
it exits nonzero while any result stays before its call.

```sh
systemctl --user stop kaijutsu-server
sqlite3 ~/.local/share/kaijutsu/kernel/kernel.db ".backup /path/to/kernel.db.bak"
kaijutsu-server blocks repair-order            # dry run
kaijutsu-server blocks repair-order --apply
kaijutsu-server blocks repair-order            # expect 0
systemctl --user start kaijutsu-server
```

`--apply` takes the `KernelLock`, so it refuses while the service runs. It
works on runs: maximal stretches of model, tool, and system error blocks
that carry a tick. User input, notifications, drift, and blocks without a
tick never move, so input held during a turn stays after it. Only a run
holding a misordered result is touched: its blocks get fresh keys in tick
order between the run's neighbors, in one journaled mutation
(`BlockStore::rekey_blocks`). Every block of the run is re-keyed because
stored keys can tie. The logic is `kaijutsu_kernel::order_repair`.

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

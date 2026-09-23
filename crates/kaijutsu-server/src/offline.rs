//! Offline administration: the data-directory lock, and `kaijutsu-server kj`
//! — running one `kj` verb against a stopped kernel.
//!
//! `docs/server-cli.md` is canonical.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kaijutsu_kernel::{KjCaller, KjResult};
use kaijutsu_types::SessionId;

use crate::config_mounts::ConfigMounts;
use crate::rpc::SharedKernel;

/// The data directory [`crate::rpc::create_shared_kernel`] resolves: `data_dir`
/// when given, else the XDG default. The lock and the kernel boot must agree
/// on this path, or the lock would guard nothing.
pub fn resolve_data_dir(data_dir: Option<&Path>) -> PathBuf {
    data_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::rpc::kernel_data_dir)
}

/// An exclusive advisory lock on `<data_dir>/kernel.lock`, held for the life
/// of whichever process acquires it. The serving path (`ssh.rs::run_on_listener_inner`)
/// and `kj` both take it before opening `kernel.db`, so two kernels never run
/// rc and own the worker pool over one data directory at once. Released when
/// this value drops — closing the last open file description on the lock
/// file releases the `flock`.
pub struct KernelLock {
    _file: File,
    path: PathBuf,
}

impl KernelLock {
    /// Take the lock, refusing at once — never blocking — when another
    /// process already holds it.
    pub fn acquire(data_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("{}: {e}", data_dir.display()))?;
        let path = data_dir.join("kernel.lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        // SAFETY: `file` owns a valid, open file descriptor for the duration
        // of this call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            // EWOULDBLOCK (== EAGAIN on Linux) is `flock`'s documented signal
            // for "already held, and LOCK_NB refused to wait" — every other
            // errno is a real failure (permissions, no space, a dead mount)
            // that contention's fixed text would misreport.
            return Err(if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                format!(
                    "{} is held by another kaijutsu-server or kj — stop the service, or wait \
                     for the other kj, then try again",
                    path.display()
                )
            } else {
                format!("flock {}: {error}", path.display())
            });
        }
        Ok(Self { _file: file, path })
    }
}

impl std::fmt::Debug for KernelLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelLock").field("path", &self.path).finish()
    }
}

/// What `kj` needs to boot the kernel and pick a caller, beyond the verb
/// itself.
pub struct KjRunArgs {
    pub config_dir: Option<PathBuf>,
    pub config_mounts: ConfigMounts,
    pub data_dir: Option<PathBuf>,
    pub rw_mounts: Vec<PathBuf>,
    /// `--as <name>`: the root character to act as. Omit when exactly one
    /// live root character exists.
    pub as_character: Option<String>,
    /// `--context <label|id>`: run the verb from another context instead of
    /// the caller's root context.
    pub context: Option<String>,
    /// `--json`: print the result's structured data instead of its message.
    pub json: bool,
    /// The verb and its arguments, e.g. `["character", "create", "bob"]`.
    pub argv: Vec<String>,
}

/// `kj ledger allow|deny` offline would record a durable answer that nothing
/// delivers: `run_kj` boots no approval-delivery worker
/// (`Kernel::start_approval_delivery` is a serving-path step), so the answer
/// sits undelivered until the next serving boot, where
/// `approval_resume::recover_unpublished_pairs` retires the ask without ever
/// running the approved statement. Refuse before boot, the way the running
/// kernel refuses nothing — asks are answered on the running kernel over
/// SSH, never offline.
const LEDGER_ANSWER_REFUSAL: &str =
    "kj ledger allow/deny needs a running kernel — asks are answered on the running kernel \
     over SSH; start the service and answer there.";

fn refuses_ledger_answer(argv: &[String]) -> bool {
    argv.first().map(String::as_str) == Some("ledger")
        && matches!(argv.get(1).map(String::as_str), Some("allow") | Some("deny"))
}

/// Run one `kj` verb against a stopped kernel: take the data-directory lock,
/// boot the same kernel the service boots (minus serving), dispatch, print,
/// settle, and report an exit code — `docs/server-cli.md`.
pub async fn run_kj(args: KjRunArgs) -> ExitCode {
    let resolved_data_dir = resolve_data_dir(args.data_dir.as_deref());
    let _lock = match KernelLock::acquire(&resolved_data_dir) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("kj: {e}");
            return ExitCode::FAILURE;
        }
    };

    // `--confirm` is extracted from the verb's own argv, the same way the
    // kaish `kj` builtin does it (`runtime/kj_builtin.rs`): presence of the
    // flag IS the confirmation, and the dispatcher never sees it as a
    // subcommand argument.
    let confirmed = kaijutsu_kernel::kj::parse::has_flag(&args.argv, &["--confirm"]);
    let mut argv = args.argv;
    kaijutsu_kernel::kj::parse::strip_flag(&mut argv, &["--confirm"]);

    if refuses_ledger_answer(&argv) {
        eprintln!("kj: {LEDGER_ANSWER_REFUSAL}");
        return ExitCode::FAILURE;
    }

    let shared = match crate::rpc::create_shared_kernel(
        args.config_dir.as_deref(),
        &args.config_mounts,
        args.data_dir.as_deref(),
        &args.rw_mounts,
    )
    .await
    {
        Ok(shared) => shared,
        Err(e) => {
            eprintln!("kj: {e}");
            return ExitCode::FAILURE;
        }
    };

    let exit = match resolve_caller(&shared, args.as_character.as_deref(), args.context.as_deref()) {
        Ok(mut caller) => {
            caller.confirmed = confirmed;
            dispatch_and_report(&shared, &caller, args.json, &argv).await
        }
        Err(e) => {
            eprintln!("kj: {e}");
            ExitCode::FAILURE
        }
    };

    match settle(&shared).await {
        Ok(()) => exit,
        Err(e) => {
            eprintln!("kj: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve `--as`/`--context` to a [`KjCaller`]: a live root character, its
/// reviewer unset (a root confirms its own statements), joined to its root
/// context or the one named by `--context`.
fn resolve_caller(
    shared: &SharedKernel,
    as_character: Option<&str>,
    context: Option<&str>,
) -> Result<KjCaller, String> {
    let db = shared.kernel_db.lock();

    let character = match as_character {
        Some(name) => {
            let row = db
                .get_character_by_name(name)
                .map_err(|e| format!("could not read character '{name}': {e}"))?
                .ok_or_else(|| {
                    format!("no character named '{name}' — `kaijutsu-server list-characters` to see who exists")
                })?;
            if !row.root {
                return Err(format!("'{name}' is not a root character"));
            }
            if row.retired_at.is_some() {
                return Err(format!("'{name}' is retired"));
            }
            row
        }
        None => {
            let mut roots: Vec<_> = db
                .list_characters(false)
                .map_err(|e| format!("could not read characters: {e}"))?
                .into_iter()
                .filter(|row| row.root)
                .collect();
            match roots.len() {
                0 => {
                    return Err(
                        "no live root character — run `kaijutsu-server init --as <name> --key \
                         <pubkey-file>`"
                            .to_string(),
                    );
                }
                1 => roots.remove(0),
                _ => {
                    let names: Vec<&str> = roots.iter().map(|row| row.name.as_str()).collect();
                    return Err(format!(
                        "more than one live root character ({}) — pass --as <name>",
                        names.join(", ")
                    ));
                }
            }
        }
    };

    let root_ctx = character.root_ctx.ok_or_else(|| {
        format!(
            "{} has no root context yet — this kernel boot should have created one",
            character.name
        )
    })?;

    let mut caller = KjCaller {
        principal_id: character.principal_id,
        actor_id: character.principal_id,
        reviewer_id: None,
        context_id: Some(root_ctx),
        session_id: SessionId::new(),
        confirmed: false,
        rc_depth: 0,
        privileged: false,
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    if let Some(context_ref) = context {
        let resolved = kaijutsu_kernel::kj::refs::resolve_context_arg(Some(context_ref), &caller, &db)
            .map_err(|e| format!("--context {context_ref}: {e}"))?;
        caller.context_id = Some(resolved);
    }

    Ok(caller)
}

/// Print `value` as compact JSON. `serde_json::Value`'s own `Display`
/// happens to serialize too, but going through `to_string` explicitly is
/// what keeps a bare `Value::String` printing as a quoted JSON string
/// (`"bob"`) rather than depending on that coincidence.
fn print_json(value: serde_json::Value) {
    match serde_json::to_string(&value) {
        Ok(encoded) => println!("{encoded}"),
        Err(e) => eprintln!("kj: could not encode --json output: {e}"),
    }
}

/// Dispatch one verb and print its result: the message to stdout, or with
/// `--json`, the structured data. Errors go to stderr.
async fn dispatch_and_report(
    shared: &SharedKernel,
    caller: &KjCaller,
    json: bool,
    argv: &[String],
) -> ExitCode {
    match shared.kj_dispatcher.dispatch(argv, caller).await {
        KjResult::Ok { message, data, .. } => {
            if json {
                print_json(data.unwrap_or(serde_json::Value::Null));
            } else {
                println!("{message}");
            }
            ExitCode::SUCCESS
        }
        KjResult::Switch(context_id, message) => {
            if json {
                print_json(serde_json::json!({ "switched_to": context_id.to_hex() }));
            } else {
                println!("{message}");
            }
            ExitCode::SUCCESS
        }
        KjResult::Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
        // Verbs go straight to the dispatcher, not through a shell, so kj's
        // own confirmation gate (`kj/mod.rs::KjResult::Latch`) is the only
        // gate on this path — the approval ledger's asks never fire here.
        // Exit 2 matches the kaish `kj` builtin's own convention for the
        // same case (`runtime/kj_builtin.rs::latch_result`).
        KjResult::Latch { command, target, message } => {
            let target_line = if target.is_empty() { String::new() } else { format!("\nTarget: {target}") };
            let hint = format!("kj {} --confirm", argv.join(" "));
            eprintln!("{command}: confirmation required ({message}){target_line}\nTo confirm, run: {hint}");
            ExitCode::from(2)
        }
    }
}

/// Wait until no turn is admitted anywhere on this kernel.
///
/// `shutdown_runtime_worker` cancels the runtime pool's token
/// (`Kernel::stop_runtime_worker`), and an admitted turn treats that
/// cancellation as a hard interrupt (`runtime/turn_request.rs`). Calling it
/// while a turn is still in flight would cut that turn short instead of
/// letting it conclude — so a verb that admits one, such as `kj drive`,
/// must not reach `shutdown_runtime_worker` until every admitted turn on
/// this kernel has finished on its own.
async fn await_turns_drained(shared: &SharedKernel) {
    if shared.kernel.turns_in_flight().is_empty() {
        return;
    }
    log::info!("kj: waiting for the admitted turn to finish before settling the kernel");
    while !shared.kernel.turns_in_flight().is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Stop admission, wait for every admitted turn to finish, and settle: the
/// same join `ssh.rs::spawn_signal_shutdown` performs on SIGTERM, plus the
/// wait above. A verb that admits a model turn (`kj drive`) holds this until
/// the turn ends — that is the offline command's whole "holds until the turn
/// ends" promise, not just the wire admission the verb itself returns after.
async fn settle(shared: &SharedKernel) -> Result<(), String> {
    shared.shutdown.cancel();
    await_turns_drained(shared).await;
    let result = shared
        .kernel
        .shutdown_runtime_worker()
        .await
        .map_err(|e| format!("settling the kernel after the verb: {e}"));
    match shared.kernel_db.lock().checkpoint() {
        Ok((busy, _, _)) if busy != 0 => {
            log::warn!("kj: wal_checkpoint(TRUNCATE) busy; WAL left for next open");
        }
        Ok(_) => {}
        Err(e) => log::warn!("kj: wal_checkpoint failed: {e}"),
    }
    result
}

/// The `RUST_LOG` directive `kaijutsu-server` falls back to when the
/// operator did not set one. `kj` boots the same kernel the serving default
/// boots, but its whole point is one verb's answer on stdout — the serving
/// default's `info` boot narration (every backend, the drift queue, every
/// recovered context) buries that answer in noise. Every other subcommand
/// keeps `info`. `kj`'s own errors always print (`eprintln!`, not
/// `tracing`), so a quieter default never hides a real failure.
pub fn default_log_directive(is_kj_subcommand: bool) -> &'static str {
    if is_kj_subcommand { "warn" } else { "info" }
}

#[cfg(test)]
mod default_log_directive_tests {
    use super::default_log_directive;

    #[test]
    fn kj_defaults_to_warn_everything_else_defaults_to_info() {
        assert_eq!(default_log_directive(true), "warn");
        assert_eq!(default_log_directive(false), "info");
    }
}

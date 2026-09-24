//! The kernel this binary carries: a real kaijutsu-server, in process, on a
//! loopback port the operating system chooses.
//!
//! The kernel runs on its own thread with its own multi-thread runtime. The
//! stack size matches the server's own (`KAISH_RC_THREAD_STACK`): boot runs
//! the root context's rc create chain on these threads, and the default 2 MiB
//! overflows it.
//!
//! The ACP bridge then dials `127.0.0.1:<port>` over real SSH and Cap'n
//! Proto. There is no in-process shortcut around the wire — a solo kernel is
//! the same kernel, reached the same way, so anything that works here works
//! against a shared one.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;

use kaijutsu_server::{SharedKernel, SshServer, SshServerConfig, ssh::KeySource};

use crate::state::SoloState;

/// How long boot may take before we give up and say so.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the drain at exit may take.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A fault injected on the kernel thread, so the paths taken when a kernel
/// dies under a live client can be driven from outside the process.
///
/// A `test-mock` build is the only one with anything but `None`, and the
/// flags that ask for one are hidden. The alternative — testing these paths
/// by breaking a real kernel — has no honest trigger: the failure they handle
/// is a bug, and this is exactly the code that runs after one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fault {
    #[default]
    None,
    /// Return an error from the kernel's own future once it is serving.
    #[cfg(feature = "test-mock")]
    FailAfterServing,
    /// Panic on the kernel thread once it is serving.
    #[cfg(feature = "test-mock")]
    PanicAfterServing,
}

/// A kernel running on its own thread.
pub struct SoloKernel {
    addr: SocketAddr,
    /// Set when an injected fault may fire. Armed by the caller at the
    /// moment it starts serving, so a test's fault lands on a live session
    /// rather than racing the bridge's connect.
    fault_armed: Arc<AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    stopped: std::sync::mpsc::Receiver<()>,
    /// Set before a deliberate stop, so the kernel thread knows the server
    /// ending is expected rather than a crash.
    stopping: Arc<AtomicBool>,
}

impl SoloKernel {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Let an injected fault fire from here on. A no-op without one.
    pub fn arm_fault(&self) {
        self.fault_armed.store(true, Ordering::SeqCst);
    }

    /// Stop the kernel: settle accepted work, checkpoint the database, and
    /// wait for the thread to finish. A timeout is reported, not ignored.
    pub fn stop(&mut self) -> Result<()> {
        let Some(stop) = self.stop.take() else {
            return Ok(());
        };
        self.stopping.store(true, Ordering::SeqCst);
        if stop.send(()).is_err() {
            bail!("the kernel thread ended before it could be stopped");
        }
        match self.stopped.recv_timeout(DRAIN_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(RecvTimeoutError::Timeout) => {
                bail!("the kernel did not settle within {DRAIN_TIMEOUT:?}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                bail!("the kernel thread ended without settling")
            }
        }
    }
}

/// Build the server configuration for a solo kernel. Every path is named:
/// nothing here resolves to an XDG default.
pub fn solo_server_config(state: &SoloState) -> SshServerConfig {
    let mut config = SshServerConfig::production(0);
    config.bind_addr = SocketAddr::from(([127, 0, 0, 1], 0));
    config.key_source = KeySource::Persistent(state.host_key_path());
    config.auth_db_path = Some(state.auth_db_path());
    config.config_dir = Some(state.root().to_path_buf());
    config.config_mounts =
        kaijutsu_server::config_mounts::ConfigMounts::new(state.config_root());
    config.data_dir = Some(state.root().to_path_buf());
    config
}

/// Start the kernel and wait until it is serving.
///
/// The listener is bound before the kernel is built, so the ACP bridge's
/// first connection queues in the backlog instead of being refused.
pub fn start(
    config: SshServerConfig,
    log_target: Option<PathBuf>,
    fault: Fault,
) -> Result<SoloKernel> {
    let (ready_tx, ready_rx) = channel::<Result<SocketAddr, String>>();
    let (stopped_tx, stopped_rx) = channel::<()>();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stopping = Arc::new(AtomicBool::new(false));
    let thread_stopping = Arc::clone(&stopping);
    // Set once the kernel is built and this thread has told `start` so. It
    // decides who reports a failure: before it, `start`; after it, the thread.
    let served = Arc::new(AtomicBool::new(false));
    let thread_served = Arc::clone(&served);
    let fault_armed = Arc::new(AtomicBool::new(false));
    let thread_fault_armed = Arc::clone(&fault_armed);

    std::thread::Builder::new()
        .name("kaijutsu-solo-kernel".to_string())
        .stack_size(kaijutsu_kernel::KAISH_RC_THREAD_STACK)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .thread_name("kaijutsu-solo-worker")
                .thread_stack_size(kaijutsu_kernel::KAISH_RC_THREAD_STACK)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("build the kernel runtime: {e}")));
                    return;
                }
            };

            // A panic here is the same event as a failure, and it must not
            // be allowed to unwind out of this thread: the main thread would
            // stay in `serve_stdio` while the ACP actor retried a handshake
            // against a kernel that no longer exists, which is a hang. Same
            // boundary the server puts around each RPC thread.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.block_on(run_kernel(
                    config,
                    ready_tx.clone(),
                    Arc::clone(&thread_served),
                    stop_rx,
                    fault,
                    Arc::clone(&thread_fault_armed),
                ))
            }));
            let _ = stopped_tx.send(());

            if thread_stopping.load(Ordering::SeqCst) {
                return;
            }
            let reason = match outcome {
                Ok(Ok(())) => "the kernel stopped serving".to_string(),
                Ok(Err(e)) => format!("the kernel failed: {e:#}"),
                Err(panic) => format!("the kernel panicked: {}", panic_message(&panic)),
            };
            // A kernel that never came up is [`start`]'s failure to report,
            // so hand it the reason instead of printing a second copy. A
            // refused mount arrives here.
            if !thread_served.load(Ordering::SeqCst) {
                let _ = ready_tx.send(Err(reason));
                return;
            }
            // A kernel that ends while we are serving is fatal: the ACP
            // client is talking to something that no longer exists, and
            // hanging would be worse than saying so. Exiting from here runs
            // no destructors, which is why the temporary state directory is
            // removed by an `atexit` hook rather than a guard
            // (`crate::state`).
            eprintln!("kaijutsu-solo-acp: {reason}");
            if let Some(path) = &log_target {
                eprintln!("kaijutsu-solo-acp: kernel state is at {}", path.display());
            }
            std::process::exit(1);
        })
        .context("spawn the kernel thread")?;

    match ready_rx.recv_timeout(READY_TIMEOUT) {
        Ok(Ok(addr)) => Ok(SoloKernel {
            addr,
            fault_armed,
            stop: Some(stop_tx),
            stopped: stopped_rx,
            stopping,
        }),
        Ok(Err(e)) => Err(anyhow!("{e}")),
        Err(RecvTimeoutError::Timeout) => {
            Err(anyhow!("the kernel did not start within {READY_TIMEOUT:?}"))
        }
        Err(RecvTimeoutError::Disconnected) => {
            Err(anyhow!("the kernel thread ended during startup"))
        }
    }
}

/// What a caught panic said, for the line this process exits on.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

/// Bind, boot, serve, and — on a stop signal — settle.
async fn run_kernel(
    config: SshServerConfig,
    ready_tx: std::sync::mpsc::Sender<Result<SocketAddr, String>>,
    served: Arc<AtomicBool>,
    stop_rx: tokio::sync::oneshot::Receiver<()>,
    fault: Fault,
    fault_armed: Arc<AtomicBool>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("bind {}", config.bind_addr))?;
    let addr = listener.local_addr().context("read the bound address")?;

    let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel::<SharedKernel>();
    let live: Arc<Mutex<Option<SharedKernel>>> = Arc::new(Mutex::new(None));
    let live_for_task = Arc::clone(&live);
    tokio::spawn(async move {
        // Only success is reported here. A kernel that was never built
        // leaves this quiet, and the thread sends the reason it failed.
        if let Ok(kernel) = kernel_rx.await {
            *live_for_task.lock() = Some(kernel);
            served.store(true, Ordering::SeqCst);
            let _ = ready_tx.send(Ok(addr));
        }
    });

    let server = SshServer::new(config);
    tokio::select! {
        result = server.run_on_listener_with_kernel_sink(listener, kernel_tx) => {
            result.context("serve SSH")?;
            Ok(())
        }
        _ = wait_to_fault(fault, fault_armed) => {
            // Unreachable without an injected fault: `wait_to_fault` pends
            // forever for `Fault::None`, which is the only variant a build
            // without `test-mock` has.
            #[cfg(feature = "test-mock")]
            if fault == Fault::PanicAfterServing {
                panic!("injected kernel panic");
            }
            bail!("injected kernel failure")
        }
        _ = stop_rx => {
            let kernel = live.lock().clone();
            if let Some(kernel) = kernel {
                settle(&kernel).await;
            }
            Ok(())
        }
    }
}

/// Wait until an injected fault should fire. Pends forever without one, so
/// the `select!` arm holding it never completes in an ordinary run.
async fn wait_to_fault(fault: Fault, armed: Arc<AtomicBool>) {
    if fault == Fault::None {
        std::future::pending::<()>().await;
    }
    while !armed.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A breath after arming, so the fault lands on a session that is serving
    // rather than on one still being set up.
    tokio::time::sleep(Duration::from_millis(250)).await;
}

/// The server's own teardown, run for a client disconnect instead of a
/// signal: stop admitting work, let accepted work finish, checkpoint.
async fn settle(kernel: &SharedKernel) {
    kernel.shutdown.cancel();
    if let Err(e) = kernel.kernel.shutdown_runtime_worker().await {
        tracing::warn!(error = %e, "the runtime worker did not shut down cleanly");
    }
    match kernel.kernel_db.lock().checkpoint() {
        Ok((busy, _, _)) if busy != 0 => {
            tracing::warn!("wal_checkpoint(TRUNCATE) busy; the WAL is left for the next open");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "wal_checkpoint failed"),
    }
}

//! `kaijutsu-solo-acp` — one command that is a whole kaijutsu: a private
//! kernel in this process, and ACP v1 on stdio.
//!
//! ```bash
//! # with DEEPSEEK_API_KEY (or ANTHROPIC_API_KEY) in the environment
//! kaijutsu-solo-acp
//!
//! # naming the provider and the model
//! kaijutsu-solo-acp --backend-kind deepseek --model deepseek-v4-flash
//! ```
//!
//! An ACP client launches this as a subprocess and speaks JSON-RPC 2.0 on its
//! stdin/stdout. **stdout is the wire** — every diagnostic goes to stderr.
//!
//! `kaijutsu-acp` connects to a kernel somebody else started. This one starts
//! its own: state directory, keys, root identity, config trees, model
//! defaults, performer character, kernel, and then the same ACP bridge over
//! the same SSH + Cap'n Proto wire. See `docs/solo-acp.md`.

mod kernel;
mod provider;
mod state;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use kaijutsu_acp::bridge::KernelBridge;
use kaijutsu_acp::{AcpBridge, serve_stdio};
use kaijutsu_client::{KeySource, SshConfig};

use provider::{BackendKind, ModelFlags, RealHost};
use state::SoloState;

/// The root character this kernel is born with. It owns the connection the
/// ACP bridge makes, and it is the reviewer every approval resolves to, so
/// it is never the performer.
const ROOT_CHARACTER: &str = "solo";

/// Seconds to wait for the bridge's own connection to the kernel.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser, Debug)]
#[command(name = "kaijutsu-solo-acp")]
#[command(about = "An ACP v1 agent with its own kaijutsu kernel inside it")]
struct Cli {
    /// Directory holding this kernel's state: databases, keys, and `/config`.
    /// Default: a temporary directory, removed however this process exits.
    /// Name one to keep contexts, transcripts, and settings across runs; it
    /// is used as given and never removed.
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Which provider to talk to. Left out, the provider is the one whose
    /// key is in the environment, and exactly one of DEEPSEEK_API_KEY,
    /// ANTHROPIC_API_KEY, or OPENAI_API_KEY must be set.
    #[arg(long, value_enum)]
    backend_kind: Option<BackendKind>,

    /// Provider endpoint, for an OpenAI-compatible server of your own.
    /// Default: the provider's own endpoint.
    #[arg(long)]
    base_url: Option<String>,

    /// Model id for every turn. Default: the provider's shipped model —
    /// `deepseek-v4-flash` or `claude-sonnet-5`. The `openai` provider ships
    /// none, so it needs this flag.
    #[arg(long)]
    model: Option<String>,

    /// Environment variable holding the provider key. Default: the
    /// provider's own variable. The key is read by the kernel from the
    /// environment; it is never accepted on this command line.
    #[arg(long, value_name = "VAR")]
    api_key_env: Option<String>,

    /// Character that performs the work in each ACP session. Created if it
    /// does not exist. A model turn needs a performer distinct from its
    /// reviewer, and the reviewer here is the root character `solo`.
    #[arg(long, default_value = "solo-coder")]
    character: String,

    /// rc bundle each ACP session's context runs.
    #[arg(long, default_value = "coder")]
    context_type: String,

    /// Host directory to mount read-write, at the same path inside the
    /// kernel: `--mount /app` makes host `/app` the kernel's `/app`. The
    /// model's file tools and shell may write anywhere under it. Repeatable.
    /// The start is refused unless the path is absolute, exists, and is a
    /// directory, and it may not be `/` or a reserved kernel path.
    #[arg(long, value_name = "DIR")]
    mount: Vec<PathBuf>,

    /// Do not mount the directory this agent was launched in. Without this,
    /// that directory is mounted read-write: it is the workspace a client
    /// that launched us there is asking about.
    #[arg(long)]
    no_cwd_mount: bool,

    /// Gate policy to install, replacing the shipped default. The file is
    /// copied verbatim into this kernel's `/config/kernel/gate.toml`.
    #[arg(long, value_name = "FILE")]
    gate_config: Option<PathBuf>,

    /// Fail the kernel once it is serving. Drives the recovery path in
    /// tests; hidden, and absent from a build without `test-mock`.
    #[cfg(feature = "test-mock")]
    #[arg(long, hide = true)]
    fail_kernel_after_serving: bool,

    /// Panic the kernel thread once it is serving. Drives the recovery path
    /// in tests; hidden, and absent from a build without `test-mock`.
    #[cfg(feature = "test-mock")]
    #[arg(long, hide = true)]
    panic_kernel_after_serving: bool,
}

impl Cli {
    /// The fault this run injects, if any. Never anything but `None` in a
    /// build without `test-mock`, where the flags do not exist.
    fn fault(&self) -> kernel::Fault {
        #[cfg(feature = "test-mock")]
        {
            if self.panic_kernel_after_serving {
                return kernel::Fault::PanicAfterServing;
            }
            if self.fail_kernel_after_serving {
                return kernel::Fault::FailAfterServing;
            }
        }
        kernel::Fault::None
    }
}

/// Host directories the kernel already mounts read-write on its own
/// (`docs/mounts.md`). Home comes from the same helper the kernel resolves
/// its own `$HOME/src` mount with, so the two cannot disagree about which
/// directory is already writable.
fn fixed_rw_mounts() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/tmp"),
        kaish_kernel::home_dir().join("src"),
    ]
}

/// Why the launch directory is not being mounted, or `None` when it should
/// be.
///
/// The automatic mount is a convenience, so an unmountable launch directory
/// says so on stderr and the kernel still starts. A directory named with
/// `--mount` is a request, and that one refuses the start.
fn skip_cwd_reason(cwd: &Path) -> Option<String> {
    if cwd == Path::new("/") {
        return Some("the launch directory is `/`, which stays read-only".to_string());
    }
    if let Some(fixed) = fixed_rw_mounts()
        .into_iter()
        .find(|fixed| cwd.starts_with(fixed))
    {
        return Some(format!(
            "{} is already read-write under {}",
            cwd.display(),
            fixed.display()
        ));
    }
    kaijutsu_server::ssh::validate_rw_mounts(std::slice::from_ref(&cwd.to_path_buf()))
        .err()
        .map(|reason| format!("the launch directory cannot be mounted: {reason}"))
}

fn main() -> ExitCode {
    // stderr only — stdout carries the ACP protocol.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false))
        .init();

    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kaijutsu-solo-acp: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    // The model comes first: a kernel with nowhere to send a prompt should
    // refuse before it writes anything to disk.
    let choice = provider::resolve(
        &ModelFlags {
            backend_kind: cli.backend_kind,
            base_url: cli.base_url.clone(),
            model: cli.model.clone(),
            api_key_env: cli.api_key_env.clone(),
        },
        &RealHost,
    )?;
    tracing::info!(
        backend = %choice.backend,
        model = %choice.model,
        "solo provider resolved"
    );

    let mut solo = SoloState::prepare(cli.state_dir.clone())?;
    // Named on stderr because a person looking for this kernel's database,
    // its log, or the contexts it kept has no other way to find them.
    eprintln!("kaijutsu-solo-acp: solo state directory: {}", solo.root().display());

    let key = state::ensure_client_key(&solo.client_key_path())?;
    state::prepare_rows(&solo, ROOT_CHARACTER, &cli.character, &key, &choice)?;

    let mut config = kernel::solo_server_config(&solo);
    config.rw_mounts = cli.mount.clone();
    if !cli.no_cwd_mount {
        match std::env::current_dir() {
            Ok(cwd) => match skip_cwd_reason(&cwd) {
                None => {
                    eprintln!(
                        "kaijutsu-solo-acp: workspace mount: {} (read-write)",
                        cwd.display()
                    );
                    config.rw_mounts.push(cwd);
                }
                Some(reason) => {
                    eprintln!("kaijutsu-solo-acp: no workspace mount added: {reason}");
                }
            },
            Err(e) => {
                eprintln!(
                    "kaijutsu-solo-acp: no workspace mount added: the launch directory is \
                     unreadable: {e}"
                );
            }
        }
    }
    let mut running = kernel::start(config, Some(solo.root().to_path_buf()), cli.fault())?;
    tracing::info!(addr = %running.addr(), "solo kernel serving");

    if let Some(gate) = &cli.gate_config {
        state::install_gate_config(&solo, gate)?;
        tracing::info!(source = %gate.display(), "installed the gate policy");
    }

    let served = serve(&cli, running.addr().port(), solo.client_key_path(), || {
        running.arm_fault()
    });

    // Settle the kernel before the state disappears, so a named state
    // directory is left consistent and a temporary one is removed only
    // after the database is closed.
    let settled = running.stop();
    let cleaned = solo.clean_up();

    served?;
    settled?;
    cleaned
}

/// Connect the ACP bridge to our own kernel and serve until the client goes
/// away.
fn serve(cli: &Cli, port: u16, key_path: PathBuf, serving: impl Fn()) -> Result<()> {
    let config = SshConfig {
        host: "127.0.0.1".to_string(),
        port,
        username: whoami::username(),
        key_source: KeySource::from_file(key_path),
        // The host key is this kernel's own, generated in its own state
        // directory. There is no known_hosts entry to check it against and
        // nothing between us but the loopback interface.
        insecure: true,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the ACP runtime")?;

    // Cap'n Proto RPC types are !Send, so the actor spawn must happen inside
    // a LocalSet, and the LocalSet must outlive the session. Dropping it
    // inside the block_on keeps russh's teardown on a thread that still has
    // a reactor — the same shape as `kaijutsu-acp`'s own main.
    let local = tokio::task::LocalSet::new();
    let context_type = cli.context_type.clone();
    let character = cli.character.clone();
    runtime.block_on(async move {
        let result = local
            .run_until(async move {
                let kernel = KernelBridge::connect(
                    config,
                    context_type,
                    Some(character),
                    None,
                    CONNECT_TIMEOUT,
                )
                .await
                .context("connect the ACP bridge to the solo kernel")?;
                let bridge = Arc::new(AcpBridge::new(kernel));
                tracing::info!("serving ACP v1 on stdio");
                serving();
                serve_stdio(bridge)
                    .await
                    .map_err(|e| anyhow::anyhow!("acp connection ended: {e:?}"))?;
                tracing::info!("client disconnected");
                Ok::<(), anyhow::Error>(())
            })
            .await;
        drop(local);
        result
    })
}

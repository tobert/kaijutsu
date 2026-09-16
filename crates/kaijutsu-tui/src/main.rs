//! `kaijutsu-tui` binary — a ratatui client over the kernel.
//!
//! ```bash
//! # against a kaijutsu-server on localhost:2222
//! kaijutsu-tui
//!
//! # a named context on zorak
//! kaijutsu-tui --host zorak --context kaijutsu
//! ```
//!
//! **stdout is the screen.** Diagnostics go to a log file under the
//! state directory when stderr is the terminal, since a line landing
//! mid-frame is a corrupted screen; a redirected stderr (`2>tui.log`) is
//! used as given, and `--log` names the file outright. The level is `warn`
//! unless `RUST_LOG` says otherwise.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use kaijutsu_client::{KeySource, SshConfig};
use kaijutsu_tui::bridge::KernelBridge;

#[derive(Parser, Debug)]
#[command(name = "kaijutsu-tui")]
#[command(about = "Terminal client for the kaijutsu kernel")]
struct Cli {
    /// SSH host.
    #[arg(long, default_value = "localhost")]
    host: String,

    /// SSH port.
    #[arg(long, default_value_t = 2222)]
    port: u16,

    /// SSH username (defaults to the local user).
    #[arg(long)]
    user: Option<String>,

    /// Skip known_hosts verification (testing only).
    #[arg(long)]
    insecure: bool,

    /// SSH private key file. Without this, keys come from the SSH agent.
    #[arg(long)]
    key: Option<std::path::PathBuf>,

    /// Context to attach to, by id or label. Creates it when the label names
    /// nothing live. Without this, the highest-ranked live context is used.
    #[arg(long)]
    context: Option<String>,

    /// rc bundle for a context this client creates.
    #[arg(long, default_value = "coder")]
    context_type: String,

    /// Parent, by label or id, for a context `--context` creates. Default:
    /// the kernel's only live root context.
    #[arg(long)]
    parent: Option<String>,

    /// Seconds to wait for the kernel connection before giving up.
    ///
    /// The actor retries a failed handshake forever with backoff, so without
    /// a bound the client would hang instead of saying why.
    #[arg(long, default_value_t = 30)]
    connect_timeout: u64,

    /// Where diagnostics go. Without this, a stderr that is not the
    /// terminal (a redirect or a pipe) is used as given, and a stderr that
    /// is the terminal sends them to `kaijutsu-tui/tui.log` under the state
    /// directory ($XDG_STATE_HOME or ~/.local/state), because stdout is the
    /// screen and a log line landing on it corrupts the frame.
    #[arg(long, value_name = "PATH")]
    log: Option<PathBuf>,

    /// Open the diff viewer on `kj diff <A> [B]` instead of the conversation.
    /// One path diffs disk against the kernel document that owns its text;
    /// two paths diff the two documents.
    #[arg(long, num_args = 1..=2, value_names = ["A", "B"])]
    diff: Option<Vec<String>>,

    /// Push the kitty keyboard protocol (`DISAMBIGUATE_ESCAPE_CODES`) with
    /// the alternate screen: a lone `Esc` arrives unambiguous, and `Ctrl+I`
    /// no longer reads as `Tab`. Off by default — this
    /// client never queries the terminal, so support is never probed for,
    /// only requested. A terminal without the protocol ignores the request.
    #[arg(long)]
    kitty_keyboard: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(log_file(cli.log.clone()))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Cap'n Proto RPC types are `!Send`, so the actor spawn must happen
    // inside a LocalSet, and the LocalSet must outlive the whole session.
    let local = tokio::task::LocalSet::new();
    runtime.block_on(async move {
        let result = local.run_until(async move { run(cli).await }).await;
        // `run_until` returns as soon as the inner future resolves — it does
        // not wait for the tasks the LocalSet still holds. Dropping `local`
        // here keeps russh's teardown on a thread that still has a reactor.
        drop(local);
        result
    })
}

/// The log file to write, or `None` for stderr: the `--log` path when
/// given, else the state-directory file when stderr is the terminal.
fn log_file(chosen: Option<PathBuf>) -> Option<PathBuf> {
    if chosen.is_some() {
        return chosen;
    }
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let state = dirs::state_dir().or_else(dirs::cache_dir)?;
    Some(state.join("kaijutsu-tui").join("tui.log"))
}

/// Quiet by default: `warn` unless `RUST_LOG` says otherwise. A log file
/// that cannot be opened is an error before the screen is taken, not a silent
/// fall back onto the screen.
fn init_tracing(file: Option<PathBuf>) -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(tracing::Level::WARN.to_string()));
    let registry = tracing_subscriber::registry().with(filter);
    match file {
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("create the log directory {}", dir.display()))?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open the log file {}", path.display()))?;
            registry.with(fmt::layer().with_writer(file).with_ansi(false)).init();
        }
        None => registry.with(fmt::layer().with_writer(std::io::stderr).with_ansi(false)).init(),
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let config = SshConfig {
        host: cli.host.clone(),
        port: cli.port,
        username: cli.user.clone().unwrap_or_else(whoami::username),
        key_source: cli.key.clone().map(KeySource::from_file).unwrap_or(KeySource::Agent),
        insecure: cli.insecure,
    };
    let identity = config.username.clone();

    let bridge = KernelBridge::connect(
        config,
        cli.context_type,
        cli.parent,
        std::time::Duration::from_secs(cli.connect_timeout),
    )
    .await?;

    let start = match &cli.context {
        Some(target) => bridge.open(target).await?,
        None => bridge.open_ranked().await?,
    };
    tracing::info!(context = %start.id.short(), label = %start.label, "attached");

    kaijutsu_tui::run::run(bridge, start, identity, cli.diff, cli.kitty_keyboard).await
}

//! `kaijutsu-tui` binary — an inline ratatui viewport over the kernel.
//!
//! ```bash
//! # against a kaijutsu-server on localhost:2222
//! kaijutsu-tui
//!
//! # a named context on zorak
//! kaijutsu-tui --host zorak --context kaijutsu
//! ```
//!
//! **stdout is the viewport** — every diagnostic goes to stderr, and the log
//! level is `warn` unless `RUST_LOG` says otherwise, because an INFO line
//! landing mid-frame is a corrupted screen.

use anyhow::Result;
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

    /// Seconds to wait for the kernel connection before giving up.
    ///
    /// The actor retries a failed handshake forever with backoff, so without
    /// a bound the client would hang instead of saying why.
    #[arg(long, default_value_t = 30)]
    connect_timeout: u64,

    /// Open the diff viewer on `kj diff <A> [B]` instead of the conversation.
    /// One path diffs disk against the kernel document that owns its text;
    /// two paths diff the two documents.
    #[arg(long, num_args = 1..=2, value_names = ["A", "B"])]
    diff: Option<Vec<String>>,
}

fn main() -> Result<()> {
    // stderr only, and quiet: stdout is the viewport.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(tracing::Level::WARN.to_string()));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false))
        .init();

    let cli = Cli::parse();

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
        std::time::Duration::from_secs(cli.connect_timeout),
    )
    .await?;

    let start = match &cli.context {
        Some(target) => bridge.open(target).await?,
        None => bridge.open_ranked().await?,
    };
    tracing::info!(context = %start.id.short(), label = %start.label, "attached");

    kaijutsu_tui::run::run(bridge, start, identity, cli.diff).await
}

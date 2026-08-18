//! `kaijutsu-acp` binary — ACP v1 over stdio, kernel over SSH.
//!
//! ```bash
//! # against a kaijutsu-server on localhost:2222
//! kaijutsu-acp --connect
//!
//! # against zorak
//! kaijutsu-acp --connect --host zorak --port 2222
//! ```
//!
//! An ACP client launches this as a subprocess and speaks JSON-RPC 2.0 on its
//! stdin/stdout. **stdout is the wire** — every diagnostic goes to stderr.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use kaijutsu_acp::bridge::KernelBridge;
use kaijutsu_acp::{AcpBridge, serve_stdio};
use kaijutsu_client::{KeySource, SshConfig};

#[derive(Parser, Debug)]
#[command(name = "kaijutsu-acp")]
#[command(about = "Agent Client Protocol (v1) bridge for the kaijutsu kernel")]
struct Cli {
    /// Connect to kaijutsu-server via SSH (uses ssh-agent for auth).
    ///
    /// Kept as an explicit flag to mirror kaijutsu-mcp's surface. There is no
    /// embedded mode: an ACP session without a kernel has nothing to say.
    #[arg(short, long)]
    connect: bool,

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

    /// rc bundle for contexts this bridge creates.
    ///
    /// `coder` by default: over ACP, kaijutsu *is* the agent and the client is
    /// the human's seat, so a new session should get the model-facing stance —
    /// unlike kaijutsu-mcp, where kaijutsu is the tool and `mcp` is right.
    #[arg(long, default_value = "coder")]
    context_type: String,

    /// Deprecated compatibility flag. ACP lifecycle requests supply the
    /// authoritative cwd; this value is accepted but never used as fallback.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Seconds to wait for the kernel connection before giving up.
    ///
    /// The actor retries a failed handshake forever with backoff, so without a
    /// bound the bridge would hang instead of telling anyone why.
    #[arg(long, default_value_t = 30)]
    connect_timeout: u64,
}

fn main() -> Result<()> {
    // stderr only — stdout carries the ACP protocol.
    let filter = EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into());
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false));

    let _otel_guard = if kaijutsu_telemetry::otel_enabled() {
        let (otel_layer, guard) = kaijutsu_telemetry::otel_layer("kaijutsu-acp");
        registry.with(otel_layer).init();
        Some(guard)
    } else {
        registry.init();
        None
    };

    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Cap'n Proto RPC types are !Send, so the actor spawn must happen inside a
    // LocalSet — and the LocalSet must outlive the whole session, not just the
    // connect. Same shape as kaijutsu-mcp's `run_serve`.
    let local = tokio::task::LocalSet::new();
    runtime.block_on(async move {
        let result = local.run_until(async move { run(cli).await }).await;
        // `run_until` returns as soon as the inner future resolves — it does
        // NOT wait for other tasks the LocalSet is still holding. A failed
        // bind_kernel (e.g. a wire-version mismatch) drops the RpcClient,
        // whose Drop only calls `AbortHandle::abort()` on the spawned capnp
        // RpcSystem task: that requests cancellation but doesn't run the
        // task's drop glue synchronously, so the task (and the SSH
        // ChannelStream it owns) can still be alive right here. If that drop
        // is deferred until `local` itself drops at the end of `main()` —
        // by then outside any entered Tokio runtime — russh's ChannelStream
        // teardown needs `Handle::current()` and panics with "there is no
        // reactor running" instead of the clean error this handshake exists
        // to produce. Dropping `local` here, still inside this block_on-
        // driven future, keeps that teardown on a thread with a reactor.
        drop(local);
        result
    })
}

async fn run(cli: Cli) -> Result<()> {
    if !cli.connect {
        anyhow::bail!(
            "kaijutsu-acp requires --connect: there is no embedded kernel mode. \
             Start kaijutsu-server and pass --connect [--host H] [--port P]."
        );
    }
    if let Some(cwd) = &cli.cwd {
        tracing::warn!(
            cwd = %cwd.display(),
            "--cwd is deprecated and ignored; ACP session cwd is authoritative"
        );
    }

    let config = SshConfig {
        host: cli.host.clone(),
        port: cli.port,
        username: cli.user.clone().unwrap_or_else(whoami::username),
        key_source: KeySource::Agent,
        insecure: cli.insecure,
    };

    tracing::info!(
        host = %config.host,
        port = config.port,
        user = %config.username,
        context_type = %cli.context_type,
        "kaijutsu-acp starting"
    );

    let kernel = KernelBridge::connect(
        config,
        cli.context_type,
        std::time::Duration::from_secs(cli.connect_timeout),
    )
    .await?;
    let bridge = Arc::new(AcpBridge::new(kernel));

    tracing::info!("serving ACP v1 on stdio");
    serve_stdio(bridge)
        .await
        .map_err(|e| anyhow::anyhow!("acp connection ended: {e:?}"))?;
    tracing::info!("client disconnected");
    Ok(())
}

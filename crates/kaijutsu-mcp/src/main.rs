//! Kaijutsu MCP server binary.
//!
//! Exposes the CRDT kernel to MCP clients (Claude Code, Gemini CLI, opencode).
//!
//! ## Subcommands
//!
//!   # MCP stdio server (default when no subcommand given)
//!   cargo run -p kaijutsu-mcp
//!   cargo run -p kaijutsu-mcp -- serve --connect
//!
//!   # One-shot hook client — reads stdin, sends to daemon socket
//!   cargo run -p kaijutsu-mcp -- hook
//!   cargo run -p kaijutsu-mcp -- hook --socket /tmp/kj-hook.sock
//!
//! ## Backward Compatibility
//!
//! The old flags (`--connect`, `--host`, `--port`, etc.) still work when no
//! subcommand is specified — they default to `serve`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use kaijutsu_mcp::KaijutsuMcp;
use kaijutsu_mcp::hook_listener::{HookListener, default_socket_path, send_hook_event, discover_sockets};

/// MCP server exposing kaijutsu CRDT kernel.
#[derive(Parser, Debug)]
#[command(name = "kaijutsu-mcp")]
#[command(about = "MCP server for kaijutsu CRDT kernel")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    // Top-level flags for backward compatibility (same as ServeArgs).
    // When no subcommand is given, these are used directly.
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// MCP stdio server with optional hook socket.
    Serve(ServeArgs),
    /// One-shot hook client: reads stdin JSON, sends to daemon socket, prints response.
    Hook(HookArgs),
}

/// Connection and server arguments — shared shape for top-level + serve subcommand.
#[derive(Args, Debug, Clone)]
struct ServeArgs {
    /// Connect to kaijutsu-server via SSH (uses ssh-agent for auth)
    #[arg(short, long)]
    connect: bool,

    /// SSH host for --connect mode
    #[arg(long, default_value = "localhost")]
    host: String,

    /// SSH port for --connect mode
    #[arg(long, default_value_t = 2222)]
    port: u16,

    /// Kernel ID to attach to
    #[arg(long, default_value = "lobby")]
    kernel: String,

    /// Context name to join within the kernel
    #[arg(long, default_value = "default")]
    context_name: String,

    /// Unix socket path for the hook listener.
    /// Default: $XDG_RUNTIME_DIR/kaijutsu/hook-{ppid}.sock
    #[arg(long)]
    hook_socket: Option<PathBuf>,
}

/// Hook client arguments.
#[derive(Args, Debug)]
struct HookArgs {
    /// Socket path to connect to.
    /// Default: $XDG_RUNTIME_DIR/kaijutsu/hook-{ppid}.sock
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing to stderr (MCP uses stdio for protocol)
    let filter = EnvFilter::from_default_env()
        .add_directive(tracing::Level::INFO.into());
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false));

    #[cfg(feature = "telemetry")]
    let _otel_guard = if kaijutsu_telemetry::otel_enabled() {
        let (otel_layer, guard) = kaijutsu_telemetry::otel_layer("kaijutsu-mcp");
        registry.with(otel_layer).init();
        Some(guard)
    } else {
        registry.init();
        None
    };

    #[cfg(not(feature = "telemetry"))]
    registry.init();

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Hook(args)) => run_hook_client(args).await,
        Some(Command::Serve(args)) => run_serve(args).await,
        None => run_serve(cli.serve).await,
    }
}

/// MCP stdio server + hook socket listener.
async fn run_serve(args: ServeArgs) -> Result<()> {
    // Cap'n Proto RPC requires LocalSet for !Send types
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(async {
        let mcp = if args.connect {
            tracing::info!(
                host = %args.host,
                port = %args.port,
                kernel = %args.kernel,
                "Connecting via SSH"
            );
            KaijutsuMcp::connect(&args.host, args.port, &args.kernel, &args.context_name).await?
        } else {
            tracing::info!("Starting with in-memory store");
            KaijutsuMcp::new()
        };

        // Start hook socket listener as a background task
        let socket_path = args.hook_socket.or_else(default_socket_path);
        let Some(socket_path) = socket_path else {
            tracing::warn!("$XDG_RUNTIME_DIR not set — hook socket disabled. Set --hook-socket explicitly to enable.");
            // Continue without hook socket — MCP server still works
            let service = mcp
                .serve(stdio())
                .await
                .inspect_err(|e| {
                    tracing::error!("MCP server error: {:?}", e);
                })?;
            tracing::info!("kaijutsu-mcp server ready (no hook socket)");
            service.waiting().await?;
            tracing::info!("kaijutsu-mcp server shutting down");
            return Ok(());
        };
        let listener = match mcp.backend() {
            kaijutsu_mcp::Backend::Local(store) => {
                // Local mode: hooks write to the same in-memory store
                let doc_ids = store.list_ids();
                let doc_id = doc_ids.first()
                    .cloned()
                    .unwrap_or_else(|| "hook-local".to_string());
                Arc::new(HookListener::local(store.clone(), doc_id))
            }
            kaijutsu_mcp::Backend::Remote(remote) => {
                Arc::new(HookListener::remote(remote.clone(), remote.context_id))
            }
        };

        let socket_path_bg = socket_path.clone();
        tokio::spawn(async move {
            if let Err(e) = listener.start(socket_path_bg).await {
                tracing::error!("Hook listener error: {e}");
            }
        });

        tracing::info!(socket = %socket_path.display(), "Hook socket started");

        // Create and serve the MCP server
        let service = mcp
            .serve(stdio())
            .await
            .inspect_err(|e| {
                tracing::error!("MCP server error: {:?}", e);
            })?;

        tracing::info!("kaijutsu-mcp server ready");

        // Wait for the service to complete
        service.waiting().await?;

        // Cleanup socket on exit
        let _ = tokio::fs::remove_file(&socket_path).await;

        tracing::info!("kaijutsu-mcp server shutting down");
        Ok(())
    }).await
}

/// One-shot hook client: reads stdin, sends to socket, prints response.
/// Fail-open: exits 0 if socket is unreachable.
async fn run_hook_client(args: HookArgs) -> Result<()> {
    use tokio::io::AsyncReadExt;

    // Read event JSON from stdin
    let mut input = String::new();
    tokio::io::stdin().read_to_string(&mut input).await?;
    let input = input.trim().to_string();

    if input.is_empty() {
        // Nothing to do — fail open
        return Ok(());
    }

    // Determine socket path — try explicit flag, then PPID default, then scan
    let socket_path = if let Some(path) = args.socket {
        path
    } else if let Some(default) = default_socket_path() {
        if default.exists() {
            default
        } else {
            // PPID default doesn't exist — try discovery
            match discover_sockets().into_iter().next() {
                Some(found) => found,
                None => {
                    tracing::debug!("No hook socket found, failing open");
                    return Ok(());
                }
            }
        }
    } else {
        // No XDG_RUNTIME_DIR — fail open
        tracing::debug!("$XDG_RUNTIME_DIR not set, no hook socket available");
        return Ok(());
    };

    // Send to socket — fail open on any error
    match send_hook_event(&socket_path, &input).await {
        Ok(Some(response)) => {
            let response = response.trim();
            if !response.is_empty() {
                // Check if the response indicates deny
                if let Ok(parsed) = serde_json::from_str::<kaijutsu_mcp::hook_types::HookResponse>(response) {
                    print!("{response}");
                    if parsed.is_deny() {
                        std::process::exit(2);
                    }
                } else {
                    print!("{response}");
                }
            }
        }
        Ok(None) => {
            // Socket doesn't exist — fail open
            tracing::debug!(path = %socket_path.display(), "Hook socket not found, failing open");
        }
        Err(e) => {
            // Connection error — fail open
            tracing::debug!("Hook socket error: {e}, failing open");
        }
    }

    Ok(())
}

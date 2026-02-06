//! Kaijutsu MCP server binary.
//!
//! Exposes the CRDT kernel to MCP clients (Claude Code, Gemini CLI, opencode).
//!
//! Usage:
//!   # In-memory mode (ephemeral)
//!   cargo run -p kaijutsu-mcp
//!
//!   # Connect to kaijutsu-server via SSH (shared state)
//!   cargo run -p kaijutsu-mcp -- --connect
//!   cargo run -p kaijutsu-mcp -- --connect --host localhost --port 2222
//!
//! Test with MCP inspector:
//!   npx @modelcontextprotocol/inspector cargo run -p kaijutsu-mcp

use anyhow::Result;
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{EnvFilter, fmt};

use kaijutsu_mcp::KaijutsuMcp;

/// MCP server exposing kaijutsu CRDT kernel.
#[derive(Parser, Debug)]
#[command(name = "kaijutsu-mcp")]
#[command(about = "MCP server for kaijutsu CRDT kernel")]
struct Args {
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
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing to stderr (MCP uses stdio for protocol)
    fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into())
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args = Args::parse();

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

        tracing::info!("kaijutsu-mcp server shutting down");
        Ok(())
    }).await
}

//! Kaijutsu MCP server binary.
//!
//! Exposes the CRDT kernel to MCP clients (Claude Code, opencode).
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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use kaijutsu_mcp::KaijutsuMcp;
use kaijutsu_mcp::hook_listener::{
    HookListener, PING_TIMEOUT, candidate_sockets, default_socket_path, resolve_hook_socket,
    send_hook_event, sweep_stale_sockets,
};

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
    let filter = EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into());
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false));

    let _otel_guard = if kaijutsu_telemetry::otel_enabled() {
        let (otel_layer, guard) = kaijutsu_telemetry::otel_layer("kaijutsu-mcp");
        registry.with(otel_layer).init();
        Some(guard)
    } else {
        registry.init();
        None
    };

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Hook(args)) => run_hook_client(args).await,
        Some(Command::Serve(args)) => run_serve(args).await,
        None => run_serve(cli.serve).await,
    }
}

/// MCP stdio server + hook socket listener.
async fn run_serve(args: ServeArgs) -> Result<()> {
    // Detect hosting agent (Claude Code, etc.)
    let agent = kaijutsu_agent_tools::detect();
    if let Some(ref a) = agent {
        tracing::info!(
            agent = a.agent_name(),
            session_id = a.session_id(),
            slug = a.slug(),
            version = a.version(),
            "Detected hosting agent"
        );
    }

    // Extract session ID from agent detection (if any)
    let detected_session_id = agent
        .as_ref()
        .and_then(|a| a.session_id().map(String::from));

    // Cap'n Proto RPC requires LocalSet for !Send types
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(async {
        // `Some(label)` only when register_session_auto below succeeds
        // *and* the session id wasn't known yet — that's the one case where
        // the label needs a rename once a hook event tells us the session
        // id (HookListener::remote, session.start handling).
        let mut pending_label_rename: Option<String> = None;

        let mcp = if args.connect {
            tracing::info!(
                host = %args.host,
                port = %args.port,
                kernel = %args.kernel,
                "Connecting via SSH"
            );
            let mcp = KaijutsuMcp::connect(
                &args.host,
                args.port,
                &args.context_name,
                detected_session_id.as_deref(),
            ).await?;

            // Auto-register a session context so hook events land somewhere
            // without requiring a model to call register_session first.
            // Best-effort: on failure we log and keep serving — the tool
            // can still be called manually.
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let unix_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // No session-id suffix here even when detection reported one:
            // detection scrapes the newest transcript file, and at MCP spawn
            // time the CURRENT session's transcript may not exist yet — the
            // detected id can belong to a previous session (observed live).
            // The first session.start hook event carries the true id; its
            // handler does the rename.
            let label = auto_register_label(&cwd, unix_secs);
            // The actor connects in the background, so the first attempt can
            // race it ("not ready: connecting"). Retry briefly with backoff;
            // exhaustion stays fail-open (the tool can be called manually).
            let mut result = String::new();
            let mut success = false;
            for delay_ms in [0u64, 250, 500, 1000, 2000, 4000] {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                result = mcp.register_session_auto(Some(label.clone()), None).await;
                success = serde_json::from_str::<serde_json::Value>(&result)
                    .ok()
                    .map(|v| {
                        v.get("success").and_then(|b| b.as_bool()).unwrap_or(false)
                            || v.get("already_registered")
                                .and_then(|b| b.as_bool())
                                .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if success {
                    break;
                }
            }
            if success {
                pending_label_rename = Some(label.clone());
                tracing::info!(label = %label, "Auto-registered MCP session");
            } else {
                tracing::warn!(
                    response = %result,
                    "Auto-register failed — continuing without a joined context; \
                     register_session can still be called manually",
                );
            }

            mcp
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

        // Sweep other processes' abandoned sockets before binding ours —
        // an unclean exit leaves the socket special file behind forever
        // (nothing unlinks it), and they accumulate in the runtime dir.
        if let Some(dir) = socket_path.parent() {
            let removed = sweep_stale_sockets(dir, &socket_path).await;
            tracing::info!(removed, dir = %dir.display(), "Stale hook socket sweep complete");
        }

        let listener = match mcp.backend() {
            kaijutsu_mcp::Backend::Local(store) => {
                // Local mode: hooks write to the same in-memory store
                let doc_ids = store.list_ids();
                let ctx_id = doc_ids.first()
                    .copied()
                    .unwrap_or_else(kaijutsu_crdt::ContextId::new);
                Arc::new(HookListener::local(store.clone(), ctx_id))
            }
            kaijutsu_mcp::Backend::Remote(remote) => {
                // shared_context_id is updated by register_session when a context is joined
                Arc::new(HookListener::remote(
                    remote.clone(),
                    Arc::clone(&remote.shared_context_id),
                    Arc::clone(mcp.session_id_arc()),
                    pending_label_rename.clone(),
                ))
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
/// Fail-open: exits 0 if socket is unreachable, or if anything about the
/// input/resolution is ambiguous.
async fn run_hook_client(args: HookArgs) -> Result<()> {
    use tokio::io::AsyncReadExt;

    // Read event JSON from stdin
    let mut input = String::new();
    tokio::io::stdin().read_to_string(&mut input).await?;
    let input = input.trim();

    if input.is_empty() {
        // Nothing to do — fail open
        return Ok(());
    }

    // Adapters may pipe pretty-printed JSON, but the socket listener reads
    // exactly one line. Parse-and-recompact; never forward something that
    // didn't parse as JSON.
    let Some((compact, event_session_id)) = normalize_hook_input(input) else {
        tracing::debug!("Hook stdin is not valid JSON, failing open");
        return Ok(());
    };

    // Resolve which of the (possibly many stale) sockets in the runtime dir
    // is actually ours: ping every live candidate and match on session_id,
    // with the adapter's explicit --socket (PPID-derived, same process tree)
    // as the tiebreaker when no session matches.
    let explicit = args.socket.clone();
    let candidates = candidate_sockets(args.socket);
    let Some(socket_path) = resolve_hook_socket(
        candidates,
        explicit.as_deref(),
        event_session_id.as_deref(),
        PING_TIMEOUT,
    )
    .await
    else {
        tracing::debug!("No hook socket resolved, failing open");
        return Ok(());
    };

    // Send to socket — fail open on any error
    match send_hook_event(&socket_path, &compact).await {
        Ok(Some(response)) => {
            let response = response.trim();
            if !response.is_empty() {
                // Check if the response indicates deny
                if let Ok(parsed) =
                    serde_json::from_str::<kaijutsu_mcp::hook_types::HookResponse>(response)
                {
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

/// Parse hook stdin as JSON and re-serialize compact (single line), also
/// extracting `session_id` (if present) for socket resolution. `None` if
/// `input` isn't valid JSON — the caller must never forward garbage.
fn normalize_hook_input(input: &str) -> Option<(String, Option<String>)> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    let session_id = value
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let compact = serde_json::to_string(&value).ok()?;
    Some((compact, session_id))
}

/// Generate the auto-register label: `cc-{cwd basename}-{MMDD-HHMM}` (UTC).
///
/// Deliberately no session-id suffix — startup agent detection can report a
/// PREVIOUS session's id (it scrapes the newest transcript file, which may
/// predate this session). `HookListener`'s `session.start` handling appends
/// `-{first 8 chars}` once a hook event reveals the true id.
fn auto_register_label(cwd: &Path, unix_secs: u64) -> String {
    let dirname = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("kaijutsu");
    format!("cc-{dirname}-{}", format_stamp(unix_secs))
}

/// Format a Unix timestamp (seconds) as `MMDD-HHMM`, UTC.
///
/// UTC rather than local time — a once-at-startup label stamp doesn't
/// justify pulling in a timezone-database dependency.
fn format_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86400) as i64;
    let secs_of_day = unix_secs % 86400;
    let (_year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    format!("{month:02}{day:02}-{hour:02}{minute:02}")
}

/// Civil (year, month, day) date from days-since-Unix-epoch. Howard
/// Hinnant's `civil_from_days` algorithm (proleptic Gregorian) — avoids
/// pulling in a full date/time crate for a once-at-startup label stamp.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- normalize_hook_input (item 1) --

    #[test]
    fn normalize_hook_input_reformats_pretty_json() {
        let pretty = "{\n  \"event\": \"tool.after\",\n  \"source\": \"claude-code\"\n}";
        let (compact, session_id) = normalize_hook_input(pretty).unwrap();
        assert_eq!(compact.lines().count(), 1, "must be a single line: {compact}");
        assert!(compact.contains("\"event\":\"tool.after\""));
        assert_eq!(session_id, None);
    }

    #[test]
    fn normalize_hook_input_extracts_session_id() {
        let json = r#"{"event":"session.start","source":"claude-code","session_id":"abc-123"}"#;
        let (_, session_id) = normalize_hook_input(json).unwrap();
        assert_eq!(session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn normalize_hook_input_rejects_invalid_json() {
        assert!(normalize_hook_input("not json at all").is_none());
        assert!(normalize_hook_input("{\"event\": \"tool.after\", }").is_none());
    }

    // -- format_stamp / civil_from_days (item 4) --

    #[test]
    fn format_stamp_matches_known_epochs() {
        // Ground truth via `date -u -d @<secs> +%m%d-%H%M`.
        assert_eq!(format_stamp(0), "0101-0000");
        assert_eq!(format_stamp(1_700_000_000), "1114-2213");
        assert_eq!(format_stamp(1_234_567_890), "0213-2331");
    }

    // -- auto_register_label (item 4) --

    #[test]
    fn auto_register_label_has_no_session_suffix() {
        // Even when startup detection reports a session id it is NOT baked
        // into the label — it can be a previous session's (stale transcript
        // scrape). session.start's rename appends the true id later.
        let label = auto_register_label(Path::new("/home/amy/src/kaijutsu"), 0);
        assert_eq!(label, "cc-kaijutsu-0101-0000");
    }

    #[test]
    fn auto_register_label_falls_back_when_cwd_has_no_basename() {
        // "/" has no file_name() component — must not panic.
        let label = auto_register_label(Path::new("/"), 0);
        assert_eq!(label, "cc-kaijutsu-0101-0000");
    }
}

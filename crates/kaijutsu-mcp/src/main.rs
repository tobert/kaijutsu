//! Kaijutsu MCP server binary.
//!
//! Exposes the kaijutsu kernel to MCP clients (Claude Code, opencode).
//!
//! ## Usage
//!
//!   # MCP stdio server — the default, and the only way to run it
//!   cargo run -p kaijutsu-mcp
//!   cargo run -p kaijutsu-mcp -- --connect
//!
//!   # One-shot hook client — reads stdin, sends to daemon socket
//!   cargo run -p kaijutsu-mcp -- hook
//!   cargo run -p kaijutsu-mcp -- hook --socket /tmp/kj-hook.sock
//!
//! There is no `serve` subcommand. It used to exist alongside these
//! top-level flags, declaring the same six options a second time, so
//! `--connect serve` set the top-level copy while the subcommand read its
//! own — and ran local while the caller believed it had connected. One
//! declaration means that cannot happen; `serve` is now a loud parse error
//! rather than a quiet wrong mode.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use kaijutsu_client::KeySource;
use kaijutsu_mcp::KaijutsuMcp;
use kaijutsu_mcp::hook_listener::{
    HookListener, PING_TIMEOUT, candidate_sockets, default_socket_path, resolve_hook_socket,
    send_hook_event, sweep_stale_sockets,
};

/// MCP server exposing the kaijutsu kernel.
#[derive(Parser, Debug)]
#[command(name = "kaijutsu-mcp")]
#[command(about = "MCP server for the kaijutsu kernel")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    // The server's options live here and ONLY here. Declaring them a second
    // time on a subcommand gives clap two fields for one flag, and the one
    // the caller sets is not necessarily the one the code reads.
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// One-shot hook client: reads stdin JSON, sends to daemon socket, prints response.
    Hook(HookArgs),
}

/// Connection and server arguments. Declared once, at the top level.
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

    /// SSH agent identity to connect as, given as its OpenSSH
    /// `SHA256:<base64>` fingerprint (the string `ssh-add -l` prints).
    /// Falls back to `KAIJUTSU_KEY_FINGERPRINT`; a flag wins over its
    /// variable. Default, with `--key-file` also unset: try every key the
    /// agent holds. A fingerprint the agent does not offer fails the
    /// connection rather than falling back to another key.
    #[arg(long)]
    key_fingerprint: Option<String>,

    /// Private key file to connect with, read directly instead of through
    /// the SSH agent. Falls back to `KAIJUTSU_KEY_FILE`; a flag wins over
    /// its variable. Default, with `--key-fingerprint` also unset: try
    /// every key the agent holds. The file must be unencrypted — an
    /// encrypted key fails the connection instead of prompting for a
    /// passphrase, and giving both `--key-fingerprint` and `--key-file`
    /// (after resolving their variables) is also an error.
    #[arg(long)]
    key_file: Option<PathBuf>,
}

/// Hook client arguments.
#[derive(Args, Debug)]
struct HookArgs {
    /// Native hook protocol. Omit for an already-normalized HookEvent.
    #[arg(value_enum)]
    source: Option<NativeHookSource>,

    /// Socket path to connect to.
    /// Default: $XDG_RUNTIME_DIR/kaijutsu/hook-{ppid}.sock
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Print the normalized HookEvent without contacting the listener.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum NativeHookSource { Claude, Codex }

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
        None => run_serve(cli.serve).await,
    }
}

/// MCP stdio server + hook socket listener.
async fn run_serve(args: ServeArgs) -> Result<()> {
    let key_source = resolve_key_source(
        args.key_fingerprint.clone(),
        args.key_file.clone(),
        std::env::var("KAIJUTSU_KEY_FINGERPRINT").ok(),
        std::env::var("KAIJUTSU_KEY_FILE").ok(),
    )?;

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
    let detected_agent_name = agent.as_ref().map(|a| a.agent_name());
    let agent_label = agent_label_prefix(detected_agent_name);

    // Cap'n Proto RPC requires LocalSet for !Send types
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(async {
        // `Some(base)` only when register_session_auto below succeeds *and*
        // the session id wasn't known yet — that's the one case where the
        // label needs stabilizing once a hook event tells us the session id
        // (HookListener::remote, `stabilize_context_label`). `base` is the
        // repo-only prefix (`auto_register_base`), NOT the timestamped
        // label used for the initial join — the stable label is
        // `{base}-{sid8}`, deterministic across MCP relaunches within the
        // same hosting-agent session (docs/issues.md "cc-* hook
        // re-registration mints a new context per MCP relaunch").
        let mut pending_label_base: Option<String> = None;

        let mcp = if args.connect {
            let ssh_dir = dirs::home_dir().map(|home| home.join(".ssh"));
            if let Some(warning) = personal_key_warning(&key_source, ssh_dir.as_deref()) {
                tracing::warn!("{warning}");
            }

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
                detected_agent_name,
                key_source.clone(),
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
            // No session-id suffix here even when detection reported one.
            // Claude detection scrapes the newest transcript file, and at MCP
            // spawn time the CURRENT session's transcript may not exist yet —
            // that id can belong to a previous session (observed live). Codex
            // supplies its thread id directly, but uses the same stabilization
            // path. The first hook event that carries a session_id (any event
            // type — not just session.start, which never fires again on a
            // same-session MCP relaunch) carries the true id;
            // `stabilize_context_label` does the fixup.
            let label = auto_register_label(&cwd, unix_secs, agent_label);
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
                pending_label_base = Some(auto_register_base(&cwd, agent_label));
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

        // Bind the socket SYNCHRONOUSLY, before spawning the accept loop —
        // `bind_socket` refuses (rather than silently steals) a path a live
        // listener still owns, and this process must know whether that
        // refusal happened before it decides, on exit, whether the path is
        // its own to unlink. A bound-then-spawned accept loop would let
        // main.rs find out about a bind failure only via a log line, with
        // no way to gate the exit-time cleanup on it.
        let bound_socket = match HookListener::bind_socket(&socket_path).await {
            Ok(unix_listener) => Some(unix_listener),
            Err(e) => {
                tracing::error!(
                    path = %socket_path.display(),
                    "Failed to bind hook socket: {e} — continuing without a hook socket \
                     rather than share another listener's endpoint"
                );
                None
            }
        };
        // Only this branch means WE bound the path — the one condition
        // under which cleanup on exit may unlink it.
        let owns_socket = bound_socket.is_some();

        if let Some(unix_listener) = bound_socket {
            let listener = match mcp.backend() {
                kaijutsu_mcp::Backend::Local(store) => {
                    // Local mode: hooks write to the same in-memory store
                    let doc_ids = store.list_ids();
                    let ctx_id = doc_ids.first()
                        .copied()
                        .unwrap_or_else(kaijutsu_types::ContextId::new);
                    Arc::new(HookListener::local(store.clone(), ctx_id))
                }
                kaijutsu_mcp::Backend::Remote(remote) => {
                    // shared_context_id is updated by register_session when a context is joined
                    Arc::new(HookListener::remote_with_agent(
                        remote.clone(),
                        Arc::clone(&remote.shared_context_id),
                        Arc::clone(mcp.session_id_arc()),
                        Arc::clone(mcp.agent_name_arc()),
                        pending_label_base.clone(),
                    ))
                }
            };

            tokio::spawn(async move {
                if let Err(e) = listener.serve(unix_listener).await {
                    tracing::error!("Hook listener error: {e}");
                }
            });

            tracing::info!(socket = %socket_path.display(), "Hook socket started");
        }

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

        // Cleanup socket on exit — ONLY if this process is the one that
        // bound it. Unlinking unconditionally (the old behavior) deletes
        // whatever now lives at this shared PPID-derived path, including a
        // successor process's live socket if one bound it after we lost our
        // own liveness check race — see `HookListener::bind_socket`.
        if owns_socket {
            let _ = tokio::fs::remove_file(&socket_path).await;
        }

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

    let native: serde_json::Value = match serde_json::from_str(input) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    let native_event = native.get("hook_event_name").and_then(|v| v.as_str()).map(String::from);
    let normalized = match args.source {
        Some(NativeHookSource::Claude) => kaijutsu_mcp::hook_adapter::HookSource::Claude.adapt(native),
        Some(NativeHookSource::Codex) => kaijutsu_mcp::hook_adapter::HookSource::Codex.adapt(native),
        None => serde_json::from_value(native).ok(),
    };
    let Some(event) = normalized else {
        tracing::debug!("Hook stdin is not valid JSON, failing open");
        return Ok(());
    };
    let event_session_id = event.session_id.clone();
    let compact = serde_json::to_string(&event)?;

    if args.dry_run {
        print!("{compact}");
        return Ok(());
    }

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
                    if parsed.is_deny() {
                        eprintln!("{}", parsed.reason.as_deref().unwrap_or("blocked by kaijutsu"));
                        std::process::exit(2);
                    }
                    if let (Some(source), Some(native_event)) = (args.source, native_event.as_deref()) {
                        let source = match source {
                            NativeHookSource::Claude => kaijutsu_mcp::hook_adapter::HookSource::Claude,
                            NativeHookSource::Codex => kaijutsu_mcp::hook_adapter::HookSource::Codex,
                        };
                        if let Some(output) = source.response(native_event, &parsed) { print!("{output}"); }
                    } else { print!("{response}"); }
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

/// Resolve `--key-fingerprint`/`--key-file` against their environment
/// fallbacks (`KAIJUTSU_KEY_FINGERPRINT`, `KAIJUTSU_KEY_FILE`) into a
/// [`KeySource`]. A flag wins over its variable. Naming both (after
/// resolution) is an error — never a silent pick of one over the other.
/// Naming neither keeps `KeySource::Agent`, trying every key the agent
/// holds.
///
/// An env variable set to the empty string counts as unset, not as a
/// value naming an empty fingerprint or path — an exported-but-blank
/// `KAIJUTSU_KEY_FINGERPRINT=""` must not collide with a `--key-file`
/// flag. A flag given as an empty string is not filtered the same way:
/// `KeySource::agent_key`/`from_file` still receive it and fail loudly
/// downstream, since a flag is a deliberate argument, not ambient
/// environment.
fn resolve_key_source(
    key_fingerprint: Option<String>,
    key_file: Option<PathBuf>,
    env_fingerprint: Option<String>,
    env_file: Option<String>,
) -> Result<KeySource> {
    let fingerprint = key_fingerprint.or(env_fingerprint.filter(|s| !s.is_empty()));
    let file =
        key_file.or_else(|| env_file.filter(|s| !s.is_empty()).map(PathBuf::from));

    match (fingerprint, file) {
        (Some(fingerprint), Some(path)) => Err(anyhow::anyhow!(
            "--key-fingerprint ({fingerprint}) and --key-file ({}) (or their \
             KAIJUTSU_KEY_FINGERPRINT/KAIJUTSU_KEY_FILE variables) name two \
             different keys; give exactly one",
            path.display()
        )),
        (Some(fingerprint), None) => Ok(KeySource::agent_key(fingerprint)),
        (None, Some(path)) => Ok(KeySource::from_file(path)),
        (None, None) => Ok(KeySource::Agent),
    }
}

/// Basenames `ssh` itself tries automatically when no `-i` is given — the
/// files a human's own login almost certainly uses. A `--key-file` naming
/// one of these under the user's `~/.ssh` is a personal key pressed into
/// service for the bridge, not a key minted for a model character.
const DEFAULT_PERSONAL_KEY_BASENAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_ed25519_sk",
    "id_ecdsa_sk",
];

/// True when `path` names one of [`DEFAULT_PERSONAL_KEY_BASENAMES`] (with or
/// without a trailing `.pub`) directly inside `ssh_dir` — pure so the
/// decision is testable without touching `$HOME` or the filesystem.
fn is_default_personal_key_path(path: &Path, ssh_dir: &Path) -> bool {
    if path.parent() != Some(ssh_dir) {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.strip_suffix(".pub").unwrap_or(name);
    DEFAULT_PERSONAL_KEY_BASENAMES.contains(&name)
}

/// Decide whether this `KeySource` probably authenticates as the human
/// rather than a bridge-specific identity, and if so, the one-line warning
/// to log. Pure: `ssh_dir` (typically `~/.ssh`) is passed in rather than read
/// from the environment, so the decision is testable in isolation.
///
/// Warns for the default `KeySource::Agent` (tries every key the agent
/// holds — lands as whichever principal owns the first one accepted) and for
/// `KeySource::File` naming a default personal key path. Never warns for
/// `KeySource::AgentKey` (one fingerprint, named on purpose) or a
/// `KeySource::File` outside the default-personal-key set.
fn personal_key_warning(key_source: &KeySource, ssh_dir: Option<&Path>) -> Option<String> {
    match key_source {
        KeySource::Agent => Some(
            "connecting with whatever key the SSH agent offers first; the bridge will act \
             as that principal — set KAIJUTSU_KEY_FINGERPRINT or --key-file to a \
             per-character key (docs/character.md, \"The bridge identity\")"
                .to_string(),
        ),
        KeySource::File { path, .. } => {
            let ssh_dir = ssh_dir?;
            is_default_personal_key_path(path, ssh_dir).then(|| {
                format!(
                    "--key-file (or KAIJUTSU_KEY_FILE) names {}, which looks like your \
                     personal SSH key; the bridge will act as you — point it at a \
                     per-character key instead (docs/character.md, \"The bridge identity\")",
                    path.display()
                )
            })
        }
        _ => None,
    }
}

/// Parse hook stdin as JSON and re-serialize compact (single line), also
/// extracting `session_id` (if present) for socket resolution. `None` if
/// `input` isn't valid JSON — the caller must never forward garbage.
#[cfg(test)]
fn normalize_hook_input(input: &str) -> Option<(String, Option<String>)> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    let session_id = value
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let compact = serde_json::to_string(&value).ok()?;
    Some((compact, session_id))
}

/// Generate the auto-register label: `{agent}-{cwd basename}-{MMDD-HHMM}` (UTC).
///
/// Deliberately no session-id suffix — startup agent detection can report a
/// PREVIOUS session's id (it scrapes the newest transcript file, which may
/// predate this session). `HookListener`'s label-stabilization handling
/// (`stabilize_context_label`) appends `-{first 8 chars}` to
/// `auto_register_base`'s prefix — NOT this timestamp — once a hook event
/// reveals the true id, so the stabilized label stays the same across an
/// MCP relaunch within the same Claude Code session even though this
/// timestamped one differs every time.
fn auto_register_label(cwd: &Path, unix_secs: u64, agent: &str) -> String {
    format!("{}-{}", auto_register_base(cwd, agent), format_stamp(unix_secs))
}

/// The label prefix stable for a whole agent session: `{agent}-{cwd
/// basename}`, with no launch timestamp. `auto_register_label`'s prefix
/// before the timestamp suffix — `stabilize_context_label` appends
/// `-{sid8}` to THIS (not to the timestamped label) once the true session
/// id is known, so relaunches of the same session converge on the same
/// stable label instead of minting a fresh one per process.
fn auto_register_base(cwd: &Path, agent: &str) -> String {
    let dirname = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("kaijutsu");
    format!("{agent}-{dirname}")
}

/// Short, stable label namespace for the detected MCP host.
fn agent_label_prefix(agent: Option<&str>) -> &'static str {
    match agent {
        Some("claude-code") => "cc",
        Some("codex") => "codex",
        _ => "mcp",
    }
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
    fn connect_is_declared_once_so_it_cannot_be_read_from_the_wrong_field() {
        // `--connect` with no subcommand is what both live MCP configs pass.
        let cli = Cli::try_parse_from(["kaijutsu-mcp", "--connect"]).expect("bare --connect parses");
        assert!(cli.serve.connect, "the flag must reach the field run_serve reads");
        assert!(cli.command.is_none(), "no subcommand means serve");
    }

    /// `--connect serve` used to set the top-level copy of the flag while
    /// `run_serve` read the subcommand's own — so it ran local and answered
    /// "requires --connect" to a caller that had passed exactly that. With one
    /// declaration the same input is a parse error instead of a wrong mode.
    #[test]
    fn the_old_silently_local_invocation_is_now_a_parse_error() {
        let err = Cli::try_parse_from(["kaijutsu-mcp", "--connect", "serve"])
            .expect_err("`serve` is not a subcommand any more");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::InvalidSubcommand,
            "an unrecognized positional must be refused, never absorbed"
        );
    }

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
        let label = auto_register_label(Path::new("/home/amy/src/kaijutsu"), 0, "cc");
        assert_eq!(label, "cc-kaijutsu-0101-0000");
    }

    #[test]
    fn auto_register_label_falls_back_when_cwd_has_no_basename() {
        // "/" has no file_name() component — must not panic.
        let label = auto_register_label(Path::new("/"), 0, "cc");
        assert_eq!(label, "cc-kaijutsu-0101-0000");
    }

    // -- auto_register_base (stable label prefix) --

    #[test]
    fn auto_register_base_has_no_timestamp() {
        // The stable prefix `stabilize_context_label` appends `-{sid8}` to —
        // it must be identical across two calls at different times, unlike
        // `auto_register_label`'s timestamped output.
        assert_eq!(
            auto_register_base(Path::new("/home/amy/src/kaijutsu"), "cc"),
            "cc-kaijutsu"
        );
    }

    #[test]
    fn auto_register_label_is_base_plus_stamp() {
        // auto_register_label must derive from auto_register_base, not
        // duplicate its own dirname logic — otherwise the two could drift
        // and a relaunch's stable label would silently stop matching an
        // earlier process's.
        let cwd = Path::new("/home/amy/src/candle");
        let base = auto_register_base(cwd, "cc");
        let label = auto_register_label(cwd, 1_700_000_000, "cc");
        assert_eq!(label, format!("{base}-{}", format_stamp(1_700_000_000)));
    }

    #[test]
    fn agent_label_prefix_distinguishes_codex_and_claude() {
        assert_eq!(agent_label_prefix(Some("claude-code")), "cc");
        assert_eq!(agent_label_prefix(Some("codex")), "codex");
        assert_eq!(agent_label_prefix(None), "mcp");
        assert_eq!(agent_label_prefix(Some("future-agent")), "mcp");
    }

    // -- resolve_key_source (--key-fingerprint / --key-file precedence) --

    #[test]
    fn resolve_key_source_defaults_to_agent_when_nothing_given() {
        let source = resolve_key_source(None, None, None, None).expect("no error");
        assert!(matches!(source, KeySource::Agent));
    }

    #[test]
    fn resolve_key_source_flag_fingerprint_wins_over_its_env_var() {
        let source = resolve_key_source(
            Some("SHA256:flag".to_string()),
            None,
            Some("SHA256:env".to_string()),
            None,
        )
        .expect("no error");
        match source {
            KeySource::AgentKey { fingerprint } => assert_eq!(fingerprint, "SHA256:flag"),
            other => panic!("expected AgentKey, got {other:?}"),
        }
    }

    #[test]
    fn resolve_key_source_env_fingerprint_used_when_flag_unset() {
        let source = resolve_key_source(None, None, Some("SHA256:env".to_string()), None)
            .expect("no error");
        match source {
            KeySource::AgentKey { fingerprint } => assert_eq!(fingerprint, "SHA256:env"),
            other => panic!("expected AgentKey, got {other:?}"),
        }
    }

    #[test]
    fn resolve_key_source_flag_file_wins_over_its_env_var() {
        let source = resolve_key_source(
            None,
            Some(PathBuf::from("/flag/key")),
            None,
            Some("/env/key".to_string()),
        )
        .expect("no error");
        match source {
            KeySource::File { path, passphrase } => {
                assert_eq!(path, PathBuf::from("/flag/key"));
                assert_eq!(passphrase, None);
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn resolve_key_source_env_file_used_when_flag_unset() {
        let source =
            resolve_key_source(None, None, None, Some("/env/key".to_string())).expect("no error");
        match source {
            KeySource::File { path, .. } => assert_eq!(path, PathBuf::from("/env/key")),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn resolve_key_source_both_given_after_resolution_is_an_error() {
        // Both from flags.
        let err = resolve_key_source(
            Some("SHA256:flag".to_string()),
            Some(PathBuf::from("/flag/key")),
            None,
            None,
        )
        .expect_err("both a fingerprint and a file must be refused");
        assert!(err.to_string().contains("SHA256:flag"));
        assert!(err.to_string().contains("/flag/key"));

        // One from a flag, the other from its variable — still both given.
        let err = resolve_key_source(
            Some("SHA256:flag".to_string()),
            None,
            None,
            Some("/env/key".to_string()),
        )
        .expect_err("a flag plus the OTHER option's env var must still be refused");
        assert!(err.to_string().contains("SHA256:flag"));
        assert!(err.to_string().contains("/env/key"));
    }

    #[test]
    fn resolve_key_source_empty_env_fingerprint_does_not_collide_with_a_file_flag() {
        // An exported-but-blank KAIJUTSU_KEY_FINGERPRINT must read as unset,
        // not as a fingerprint naming the empty string — otherwise a
        // --key-file flag would spuriously error as "both given".
        let source = resolve_key_source(
            None,
            Some(PathBuf::from("/flag/key")),
            Some(String::new()),
            None,
        )
        .expect("an empty env fingerprint must not collide with --key-file");
        match source {
            KeySource::File { path, .. } => assert_eq!(path, PathBuf::from("/flag/key")),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn resolve_key_source_empty_env_file_does_not_collide_with_a_fingerprint_flag() {
        let source = resolve_key_source(
            Some("SHA256:flag".to_string()),
            None,
            None,
            Some(String::new()),
        )
        .expect("an empty env file must not collide with --key-fingerprint");
        match source {
            KeySource::AgentKey { fingerprint } => assert_eq!(fingerprint, "SHA256:flag"),
            other => panic!("expected AgentKey, got {other:?}"),
        }
    }

    #[test]
    fn resolve_key_source_both_env_vars_empty_defaults_to_agent() {
        let source = resolve_key_source(None, None, Some(String::new()), Some(String::new()))
            .expect("no error");
        assert!(matches!(source, KeySource::Agent));
    }

    // -- is_default_personal_key_path / personal_key_warning --

    #[test]
    fn default_personal_key_basenames_under_ssh_dir_match() {
        let ssh_dir = Path::new("/home/amy/.ssh");
        for name in DEFAULT_PERSONAL_KEY_BASENAMES {
            assert!(
                is_default_personal_key_path(&ssh_dir.join(name), ssh_dir),
                "{name} should match"
            );
            // The .pub half of the pair is the same personal identity.
            assert!(
                is_default_personal_key_path(&ssh_dir.join(format!("{name}.pub")), ssh_dir),
                "{name}.pub should match"
            );
        }
    }

    #[test]
    fn a_custom_named_key_under_ssh_dir_does_not_match() {
        let ssh_dir = Path::new("/home/amy/.ssh");
        assert!(!is_default_personal_key_path(
            &ssh_dir.join("kaijutsu-lead"),
            ssh_dir
        ));
    }

    #[test]
    fn a_default_named_key_outside_ssh_dir_does_not_match() {
        // Same basename, wrong directory — a project-local key someone
        // happened to name id_ed25519 is not the user's personal identity.
        let ssh_dir = Path::new("/home/amy/.ssh");
        assert!(!is_default_personal_key_path(
            Path::new("/home/amy/src/kaijutsu/id_ed25519"),
            ssh_dir
        ));
    }

    #[test]
    fn agent_source_always_warns() {
        let warning = personal_key_warning(&KeySource::Agent, Some(Path::new("/home/amy/.ssh")))
            .expect("Agent must warn");
        assert!(warning.contains("KAIJUTSU_KEY_FINGERPRINT"));
        assert!(warning.contains("--key-file"));
        assert!(warning.contains("docs/character.md"));
    }

    #[test]
    fn agent_key_by_fingerprint_never_warns() {
        let source = KeySource::agent_key("SHA256:abc");
        assert!(personal_key_warning(&source, Some(Path::new("/home/amy/.ssh"))).is_none());
    }

    #[test]
    fn default_personal_key_file_warns_and_names_the_path() {
        let source = KeySource::from_file("/home/amy/.ssh/id_ed25519");
        let warning = personal_key_warning(&source, Some(Path::new("/home/amy/.ssh")))
            .expect("a default personal key file must warn");
        assert!(warning.contains("/home/amy/.ssh/id_ed25519"));
        assert!(warning.contains("--key-file"));
        assert!(warning.contains("KAIJUTSU_KEY_FILE"));
        assert!(warning.contains("docs/character.md"));
    }

    #[test]
    fn a_named_character_key_file_never_warns() {
        let source = KeySource::from_file("/home/amy/.ssh/kaijutsu-lead");
        assert!(personal_key_warning(&source, Some(Path::new("/home/amy/.ssh"))).is_none());
    }

    #[test]
    fn key_file_warning_is_silent_when_ssh_dir_is_unknown() {
        // No $HOME resolved — cannot judge "under ~/.ssh", so stay quiet
        // rather than guess.
        let source = KeySource::from_file("/home/amy/.ssh/id_ed25519");
        assert!(personal_key_warning(&source, None).is_none());
    }
}

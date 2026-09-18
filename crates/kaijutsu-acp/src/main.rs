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

use std::path::{Path, PathBuf};
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
    /// Connect to kaijutsu-server via SSH. Authentication comes from the SSH
    /// agent unless `--key-file` or `--key-fingerprint` names an identity.
    ///
    /// Kept as an explicit flag because kaijutsu-mcp has one. There is no
    /// embedded mode: an ACP session without a kernel has nothing to say.
    ///
    /// The connection flags this bridge shares with kaijutsu-mcp — `--host`,
    /// `--port`, `--insecure`, `--key-fingerprint`, `--key-file`, `--parent`,
    /// and the `KAIJUTSU_KEY_FINGERPRINT`/`KAIJUTSU_KEY_FILE` variables —
    /// mean the same thing in both, so one launcher's environment block can
    /// move to the other. The rest of each surface is its own.
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

    /// SSH agent identity to connect as, given as its OpenSSH
    /// `SHA256:<base64>` fingerprint (the string `ssh-add -l` prints).
    /// Falls back to `KAIJUTSU_KEY_FINGERPRINT`; a flag wins over its
    /// variable. Default, with `--key-file` also unset: try every key the
    /// agent holds. A fingerprint the agent does not offer fails the
    /// connection rather than falling back to another key.
    #[arg(long)]
    key_fingerprint: Option<String>,

    /// Private key file to connect with, read directly instead of through the
    /// SSH agent — what an unattended launcher or a container needs, where no
    /// agent is running. Falls back to `KAIJUTSU_KEY_FILE`; a flag wins over
    /// its variable. Default, with `--key-fingerprint` also unset: try every
    /// key the agent holds. The file must be unencrypted — an encrypted key
    /// fails the connection instead of prompting for a passphrase — and
    /// giving both `--key-fingerprint` and `--key-file` (after resolving
    /// their variables) is an error.
    #[arg(long)]
    key_file: Option<PathBuf>,

    /// rc bundle for contexts this bridge creates.
    ///
    /// `coder` by default: over ACP, kaijutsu *is* the agent and the client is
    /// the connected player's seat, so a new session should get the model-facing stance —
    /// unlike kaijutsu-mcp, where kaijutsu is the tool and `mcp` is right.
    #[arg(long, default_value = "coder")]
    context_type: String,

    /// Character that performs work in each new ACP context.
    ///
    /// The connected character becomes the context's director. The reviewer
    /// follows explicit assignment, delegation, or the configured default (Amy).
    /// The named character must already exist and be live.
    #[arg(long)]
    character: Option<String>,

    /// The context, by label or id, that new sessions are created under.
    /// Default: the kernel's only live root context. With several root
    /// contexts and no parent named, `session/new` fails and lists them.
    #[arg(long)]
    parent: Option<String>,

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

/// Resolve `--key-fingerprint`/`--key-file` against their environment
/// fallbacks (`KAIJUTSU_KEY_FINGERPRINT`, `KAIJUTSU_KEY_FILE`) into a
/// [`KeySource`]. A flag wins over its variable. Naming both (after
/// resolution) is an error, and the message names both values — silently
/// preferring one would authenticate as a principal the operator did not
/// choose. Naming neither keeps `KeySource::Agent`, trying every key the
/// agent holds.
///
/// An env variable set to the empty string counts as unset, not as a value
/// naming an empty fingerprint or path: an exported-but-blank
/// `KAIJUTSU_KEY_FINGERPRINT=""` must not collide with a `--key-file` flag.
/// A flag given as an empty string is not filtered the same way — a flag is
/// a deliberate argument, not ambient environment, so it reaches
/// `KeySource::agent_key`/`from_file` and fails loudly downstream.
///
/// This mirrors `kaijutsu-mcp`'s resolution
/// (`crates/kaijutsu-mcp/src/main.rs`, `resolve_key_source`) so an operator
/// can move one launcher's environment block to the other bridge and get the
/// same identity. The duplication is deliberate for now; see the note on
/// sharing in `docs/acp.md`.
fn resolve_key_source(
    key_fingerprint: Option<String>,
    key_file: Option<PathBuf>,
    env_fingerprint: Option<String>,
    env_file: Option<String>,
) -> Result<KeySource> {
    let fingerprint = key_fingerprint.or(env_fingerprint.filter(|s| !s.is_empty()));
    let file = key_file.or_else(|| env_file.filter(|s| !s.is_empty()).map(PathBuf::from));

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
/// files a human's own login almost certainly uses. A `--key-file` naming one
/// of these under the user's `~/.ssh` is a personal key pressed into service
/// for the bridge, not a key minted for a model character.
const DEFAULT_PERSONAL_KEY_BASENAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_ed25519_sk",
    "id_ecdsa_sk",
];

/// True when `path` names one of [`DEFAULT_PERSONAL_KEY_BASENAMES`] (with or
/// without a trailing `.pub`) directly inside `ssh_dir` — pure, so the
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

/// Decide whether this `KeySource` probably authenticates as the human rather
/// than a bridge-specific identity, and if so, the one-line warning to log.
/// Pure: `ssh_dir` (typically `~/.ssh`) is passed in rather than read from the
/// environment, so the decision is testable in isolation.
///
/// Warns for the default `KeySource::Agent` (tries every key the agent holds —
/// lands as whichever principal owns the first one accepted) and for
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

/// `~/.ssh`, for [`personal_key_warning`]. `None` when `HOME` is unset, which
/// makes the warning silent rather than guessing at a path.
fn ssh_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ssh"))
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

    let key_source = resolve_key_source(
        cli.key_fingerprint.clone(),
        cli.key_file.clone(),
        std::env::var("KAIJUTSU_KEY_FINGERPRINT").ok(),
        std::env::var("KAIJUTSU_KEY_FILE").ok(),
    )?;
    // stderr, never stdout: stdout is the ACP wire.
    if let Some(warning) = personal_key_warning(&key_source, ssh_dir().as_deref()) {
        tracing::warn!("{warning}");
    }

    let config = SshConfig {
        host: cli.host.clone(),
        port: cli.port,
        username: cli.user.clone().unwrap_or_else(whoami::username),
        key_source,
        insecure: cli.insecure,
    };

    tracing::info!(
        host = %config.host,
        port = config.port,
        user = %config.username,
        context_type = %cli.context_type,
        character = ?cli.character,
        "kaijutsu-acp starting"
    );

    let kernel = KernelBridge::connect(
        config,
        cli.context_type,
        cli.character,
        cli.parent,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_flag_keeps_the_agent() {
        let source = resolve_key_source(None, None, None, None).expect("agent is the default");
        assert!(matches!(source, KeySource::Agent));
    }

    #[test]
    fn a_key_file_reads_that_file() {
        let source = resolve_key_source(None, Some(PathBuf::from("/keys/bench")), None, None)
            .expect("a key file alone resolves");
        match source {
            KeySource::File { path, .. } => assert_eq!(path, PathBuf::from("/keys/bench")),
            other => panic!("expected a file key source, got {other:?}"),
        }
    }

    #[test]
    fn a_fingerprint_selects_one_agent_identity() {
        let source = resolve_key_source(Some("SHA256:abc".into()), None, None, None)
            .expect("a fingerprint alone resolves");
        match source {
            KeySource::AgentKey { fingerprint } => assert_eq!(fingerprint, "SHA256:abc"),
            other => panic!("expected an agent-key source, got {other:?}"),
        }
    }

    #[test]
    fn an_env_variable_supplies_the_key_file() {
        let source = resolve_key_source(None, None, None, Some("/env/key".into()))
            .expect("KAIJUTSU_KEY_FILE alone resolves");
        match source {
            KeySource::File { path, .. } => assert_eq!(path, PathBuf::from("/env/key")),
            other => panic!("expected a file key source, got {other:?}"),
        }
    }

    #[test]
    fn an_env_variable_supplies_the_fingerprint() {
        let source = resolve_key_source(None, None, Some("SHA256:env".into()), None)
            .expect("KAIJUTSU_KEY_FINGERPRINT alone resolves");
        match source {
            KeySource::AgentKey { fingerprint } => assert_eq!(fingerprint, "SHA256:env"),
            other => panic!("expected an agent-key source, got {other:?}"),
        }
    }

    #[test]
    fn a_flag_beats_its_variable() {
        let source = resolve_key_source(
            None,
            Some(PathBuf::from("/flag/key")),
            None,
            Some("/env/key".into()),
        )
        .expect("the flag wins");
        match source {
            KeySource::File { path, .. } => assert_eq!(path, PathBuf::from("/flag/key")),
            other => panic!("expected a file key source, got {other:?}"),
        }
    }

    /// An exported-but-blank variable is not a value. Without this, a shell
    /// profile carrying `export KAIJUTSU_KEY_FINGERPRINT=` would collide with
    /// a deliberate `--key-file` and refuse the connection.
    #[test]
    fn an_empty_variable_counts_as_unset() {
        let source = resolve_key_source(None, Some(PathBuf::from("/flag/key")), Some(String::new()), None)
            .expect("a blank variable must not collide with a flag");
        match source {
            KeySource::File { path, .. } => assert_eq!(path, PathBuf::from("/flag/key")),
            other => panic!("expected a file key source, got {other:?}"),
        }
    }

    #[test]
    fn a_variable_pair_is_refused_the_same_way_a_flag_pair_is() {
        let error = resolve_key_source(None, None, Some("SHA256:env".into()), Some("/env/key".into()))
            .expect_err("two identities must not resolve to one");
        let text = error.to_string();
        assert!(text.contains("SHA256:env"), "the error must name the fingerprint: {text}");
        assert!(text.contains("/env/key"), "the error must name the path: {text}");
    }

    #[test]
    fn the_default_agent_source_warns_about_acting_as_the_human() {
        let warning = personal_key_warning(&KeySource::Agent, Some(Path::new("/home/amy/.ssh")))
            .expect("the agent default must warn");
        assert!(warning.contains("KAIJUTSU_KEY_FINGERPRINT"), "{warning}");
    }

    #[test]
    fn a_named_agent_fingerprint_never_warns() {
        let source = KeySource::agent_key("SHA256:abc");
        assert!(personal_key_warning(&source, Some(Path::new("/home/amy/.ssh"))).is_none());
    }

    #[test]
    fn a_default_personal_key_file_warns_and_names_the_path() {
        let source = KeySource::from_file("/home/amy/.ssh/id_ed25519");
        let warning = personal_key_warning(&source, Some(Path::new("/home/amy/.ssh")))
            .expect("a personal key must warn");
        assert!(warning.contains("/home/amy/.ssh/id_ed25519"), "{warning}");
    }

    #[test]
    fn a_named_character_key_file_never_warns() {
        let source = KeySource::from_file("/home/amy/.ssh/kaijutsu-lead");
        assert!(personal_key_warning(&source, Some(Path::new("/home/amy/.ssh"))).is_none());
    }

    #[test]
    fn key_file_warning_is_silent_when_the_ssh_dir_is_unknown() {
        let source = KeySource::from_file("/home/amy/.ssh/id_ed25519");
        assert!(personal_key_warning(&source, None).is_none());
    }

    #[test]
    fn naming_both_identities_is_refused() {
        let error = resolve_key_source(Some("SHA256:abc".into()), Some(PathBuf::from("/keys/bench")), None, None)
            .expect_err("two identities must not resolve to one");
        assert!(error.to_string().contains("exactly one"), "{error}");
    }
}

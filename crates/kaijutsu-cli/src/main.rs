//! kjc: run one `kj` verb or one shell command against a running kernel,
//! print what it returned, and exit with its exit code.
//!
//! ```text
//! kjc --context verify kj block list --tail 3
//! kjc --host zorak --context verify sh 'git -C ~/src/kaijutsu log -1 --oneline'
//! ```
//!
//! Both run in the named context, as the principal the SSH key binds, through
//! the same RPCs every client uses: `executeKj` for a verb, and a shell
//! submission followed by `kj wait --operation` for a command, so the kernel
//! does the waiting. Commands author blocks in that context like any other
//! player's, so name a context kept for this rather than a working seat.

use std::process::ExitCode;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use kaijutsu_client::{ActorHandle, KeyArgs, SshConfig, spawn_actor};
use kaijutsu_types::{BlockFilter, BlockKind, ContextId, Status};

#[derive(Parser)]
#[command(name = "kjc", about = "Run one kj verb or shell command against a running kernel")]
struct Cli {
    /// SSH host.
    #[arg(long, default_value = "localhost")]
    host: String,

    /// SSH port.
    #[arg(long, default_value_t = 2222)]
    port: u16,

    /// Skip known_hosts verification (testing only).
    #[arg(long)]
    insecure: bool,

    #[command(flatten)]
    key: KeyArgs,

    /// The context to run in, by label. Commands author blocks there.
    #[arg(long, short = 'c', env = "KJC_CONTEXT")]
    context: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one kj verb, e.g. `kjc -c verify kj block list --tail 3`.
    Kj {
        /// The verb and its arguments, as at a kj prompt.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },
    /// Run one kaish command line and print its output.
    Sh {
        /// The command line, quoted as one argument.
        code: String,
        /// Seconds to wait for the command before reporting it still running.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => { eprintln!("kjc: {e}"); return ExitCode::FAILURE; }
    };
    // Cap'n Proto RPC types are !Send, so the actor lives in a LocalSet,
    // dropped inside the runtime so SSH teardown still has a reactor
    // (`kaijutsu-acp`'s main explains the panic this avoids).
    let local = tokio::task::LocalSet::new();
    let result = runtime.block_on(async move {
        let result = local.run_until(run(cli)).await;
        drop(local);
        result
    });
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => { eprintln!("kjc: {e:#}"); ExitCode::FAILURE }
    }
}

async fn run(cli: Cli) -> Result<u8> {
    let config = SshConfig {
        host: cli.host.clone(),
        port: cli.port,
        username: whoami::username(),
        key_source: cli.key.key_source()?,
        insecure: cli.insecure,
    };
    let actor = spawn_actor(config, None, "kjc".to_string(), true);
    connected(&actor, &cli.host, cli.port).await?;
    let context = actor.resolve_context_label(&cli.context).await
        .with_context(|| format!("resolve context {:?} on {}:{}", cli.context, cli.host, cli.port))?
        .ok_or_else(|| anyhow!("no context labeled {:?} on {}:{}; create one with \
            `kj context create {}`", cli.context, cli.host, cli.port, cli.context))?
        .id;
    match cli.command {
        Command::Kj { argv } => run_kj(&actor, context, argv).await,
        Command::Sh { code, timeout } => run_sh(&actor, context, &code, timeout).await,
    }
}

/// Wait, bounded, for the actor to connect. Its reconnect loop retries a
/// failing handshake forever, so an unbounded wait would hang silently.
async fn connected(actor: &ActorHandle, host: &str, port: u16) -> Result<()> {
    use kaijutsu_client::ConnectionStatus;
    let mut status = actor.watch_status();
    let _ = actor.whoami().await;
    let settled = tokio::time::timeout(std::time::Duration::from_secs(15), status.wait_for(|s| {
        matches!(s, ConnectionStatus::Connected { .. } | ConnectionStatus::Terminal { .. })
    })).await;
    match actor.current_status() {
        ConnectionStatus::Connected { .. } => Ok(()),
        ConnectionStatus::Terminal { reason } => bail!("{host}:{port}: connection refused for good: {reason}"),
        other if settled.is_err() => bail!("{host}:{port}: not connected after 15 s ({other:?})"),
        other => bail!("{host}:{port}: connection ended in {other:?}"),
    }
}

async fn run_kj(actor: &ActorHandle, context: ContextId, argv: Vec<String>) -> Result<u8> {
    let result = actor.execute_kj(context, argv).await.context("execute kj")?;
    print!("{}", result.stdout);
    eprint!("{}", result.stderr);
    if let Some(latch) = result.latch {
        eprintln!("kjc: `{}` on {} needs confirmation: {}", latch.command, latch.target, latch.message);
        return Ok(2);
    }
    Ok(exit_byte(result.exit_code))
}

async fn run_sh(actor: &ActorHandle, context: ContextId, code: &str, timeout: u64) -> Result<u8> {
    let submission = actor.shell_submit(code, context, true).await.context("submit shell command")?;
    if let Some(refusal) = submission.refusal {
        eprintln!("kjc: not run: {refusal}");
        return Ok(3);
    }
    let waited = actor.execute_kj(context, vec![
        "wait".into(), "--operation".into(), submission.operation_id.clone(),
        "--timeout".into(), timeout.to_string(),
    ]).await.context("wait for the shell operation")?;
    let filter = BlockFilter {
        kinds: vec![BlockKind::ToolResult],
        statuses: vec![Status::Done, Status::Error],
        parent_id: Some(submission.command_block_id),
        max_depth: 1,
        ..Default::default()
    };
    let result = actor.query_blocks(context, filter).await.context("read the command's result")?
        .into_iter().find(|block| block.is_shell());
    let Some(result) = result else {
        // `kj wait` returned without a settled result: still running, or
        // waiting on something. Its own report says which.
        print!("{}", waited.stdout);
        eprint!("{}", waited.stderr);
        bail!("operation {} has not settled", submission.operation_id);
    };
    print!("{}", result.content);
    if !result.content.is_empty() && !result.content.ends_with('\n') { println!(); }
    Ok(match result.exit_code {
        Some(code) => exit_byte(code),
        None if result.status == Status::Error => 1,
        None => 0,
    })
}

/// A process exit code from a command's: 0–255 as is, anything else 1.
fn exit_byte(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

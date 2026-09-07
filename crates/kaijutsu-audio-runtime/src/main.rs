use anyhow::{Context, Result, bail};
use clap::Parser;
use kaijutsu_audio_runtime::{Engine, Options};
use kaijutsu_client::{ConnectionStatus, KeySource, PeerConfig, SshConfig, spawn_actor};
use kaijutsu_types::ContextId;
use std::time::Duration;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(name = "kaijutsu-audiod", version, about = "Play kernel cues and report local MIDI over SSH")]
struct Cli {
    /// Kernel SSH host.
    #[arg(long, default_value = "localhost")]
    host: String,
    /// Kernel SSH port.
    #[arg(long, default_value_t = 2222)]
    port: u16,
    /// SSH username; defaults to the local user.
    #[arg(long)]
    user: Option<String>,
    /// Private key file; defaults to the SSH agent.
    #[arg(long)]
    key: Option<std::path::PathBuf>,
    /// Skip known_hosts verification for testing.
    #[arg(long)]
    insecure: bool,
    /// Existing context id or label for MIDI capture and external clock
    /// reports. Omit to disable both. Attach it to a track before capturing.
    #[arg(long)]
    context: Option<String>,
    /// Disable PCM output; MIDI remains enabled on Linux.
    #[arg(long, conflicts_with = "output")]
    no_audio: bool,
    /// Disable MIDI input, output, discovery and exchanges.
    #[arg(long, conflicts_with = "context")]
    no_midi: bool,
    /// Exact PCM output device name; defaults to the system output.
    #[arg(long)]
    output: Option<String>,
    /// List local PCM output names and exit without connecting.
    #[arg(long)]
    list_outputs: bool,
    /// Request Linux SCHED_RR priority for timing threads (1–99).
    /// Zero keeps the inherited policy. Permission failure warns and continues.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=99))]
    rt_priority: u8,
    /// Seconds allowed for initial connection and context resolution.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    connect_timeout: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false))
        .init();
    if cli.list_outputs {
        for name in kaijutsu_audio_runtime::output_names().map_err(anyhow::Error::msg)? { println!("{name}"); }
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    runtime.block_on(async move {
        let result = local.run_until(run(cli)).await;
        drop(local);
        result
    })
}

async fn run(cli: Cli) -> Result<()> {
    let options = Options {
        audio: !cli.no_audio,
        midi: !cli.no_midi && cfg!(target_os = "linux"),
        output: cli.output,
        rt_priority: cli.rt_priority,
        config_client: None,
    };
    if !options.audio && !options.midi { bail!("enable audio or MIDI"); }
    if cli.context.is_some() && !options.midi { bail!("MIDI capture requires a Linux MIDI backend"); }
    let ssh = SshConfig {
        host: cli.host,
        port: cli.port,
        username: cli.user.unwrap_or_else(whoami::username),
        key_source: cli.key.map(KeySource::from_file).unwrap_or(KeySource::Agent),
        insecure: cli.insecure,
    };
    let instance = format!("kaijutsu-audiod-{}", uuid::Uuid::new_v4());
    let actor = spawn_actor(ssh.clone(), None, instance.clone(), false);
    let context = tokio::time::timeout(Duration::from_secs(cli.connect_timeout), async {
        let mut status = actor.watch_status();
        // The first command wakes an idle actor and may return NotReady.
        // Readiness comes from the status level, not that command's result.
        let _ = actor.whoami().await;
        status.wait_for(|s| matches!(s, ConnectionStatus::Connected { .. } | ConnectionStatus::Terminal { .. }))
            .await.context("wait for kernel")?;
        if let ConnectionStatus::Terminal { reason } = actor.current_status() { bail!(reason); }
        let target = match cli.context.as_deref() {
            None => None,
            Some(reference) => {
                let context = if let Ok(id) = ContextId::parse(reference) {
                    actor.list_contexts().await?.into_iter().find(|c| c.id == id)
                } else { actor.resolve_context_label(reference).await? };
                let context = context.with_context(|| format!("capture context '{reference}' does not exist"))?;
                if context.archived || context.concluded_at.is_some() { bail!("capture context '{reference}' is not live"); }
                actor.join_context(context.id).await?;
                Some(context.id)
            }
        };
        Ok::<_, anyhow::Error>(target)
    }).await.context("kernel connection timed out")??;

    let (peer_tx, peer_rx) = std::sync::mpsc::channel();
    let node = format!("audio/{}", hostname::get()?.to_string_lossy());
    actor.attach_peer(PeerConfig { nick: node.clone(), instance: instance.clone() }, peer_tx)
        .await.context("register audio peer")?;
    let mut engine = Engine::start(actor, ssh.clone(), context, options.clone()).map_err(anyhow::Error::msg)?;
    let capture = kaijutsu_audio_runtime::CaptureControl::new(&engine, ssh.clone(), node, instance);
    tracing::info!(host = ssh.host, port = ssh.port, ?context, ?options, "audio node running; kernel drives playback");
    let mut health = tokio::time::interval(Duration::from_millis(100));
    let stopped = shutdown_signal();
    tokio::pin!(stopped);
    loop {
        tokio::select! {
            result = &mut stopped => { result?; break; }
            _ = health.tick() => {
                if engine.is_finished() {
                    engine.shutdown().map_err(anyhow::Error::msg)?;
                    bail!("audio runtime stopped unexpectedly");
                }
                while let Ok(request) = peer_rx.try_recv() {
                    let result = if request.action == "status" {
                        serde_json::to_vec(&serde_json::json!({
                            "audio": options.audio, "midi": options.midi,
                            "output": options.output, "context": context.map(|id| id.to_string()),
                            "rt_priority_requested": options.rt_priority,
                        })).map_err(|e| e.to_string())
                    } else {
                        let capture = capture.clone();
                        tokio::task::spawn_local(async move {
                            let result = capture.handle(&request.action, &request.params).await;
                            let _ = request.reply.send(result);
                        });
                        continue;
                    };
                    let _ = request.reply.send(result);
                }
                while engine.pulses.try_recv().is_ok() {}
            }
        }
    }
    capture.shutdown();
    engine.shutdown().map_err(anyhow::Error::msg)?;
    tracing::info!("audio node stopped");
    Ok(())
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_devices_cannot_have_dependent_options() {
        assert!(Cli::try_parse_from(["kaijutsu-audiod", "--no-audio", "--output", "speakers"]).is_err());
        assert!(Cli::try_parse_from(["kaijutsu-audiod", "--no-midi", "--context", "ear"]).is_err());
        assert!(Cli::try_parse_from(["kaijutsu-audiod", "--rt-priority", "100"]).is_err());
    }
}

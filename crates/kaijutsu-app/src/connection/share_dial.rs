//! Client-side `/r` share dialing — the app half of `docs/slash-r.md`.
//!
//! Mirrors `audio.rs`'s `CasPrefetch` shape: `russh_sftp` needs a genuine
//! Tokio runtime for its internal `tokio::spawn` calls (the server-side
//! `Handler` processing loop), which Bevy's `IoTaskPool` does not provide —
//! so this owns a small dedicated multi-thread runtime, off both the RPC
//! actor's `!Send` bootstrap `LocalSet` and the render-frame task pool.
//! Read-only in this slice — `ShareHandler` itself refuses every mutating op
//! regardless of a share's `:rw` label (`docs/slash-r.md` slice 1 scope).

use bevy::prelude::*;

use kaijutsu_client::{ShareHandler, ShareServerConfig, SshClient, SshConfig};
use kaijutsu_types::SSH_SHARE_SUBSYSTEM;

use super::actor_plugin::RpcConnectionState;

/// Registers `/r` share serving. A no-op (no runtime spun up, no systems
/// registered) when the user passed no `--share` flag — checked once here,
/// not per-frame.
pub struct ShareDialPlugin {
    pub ssh_config: SshConfig,
    pub share_config: Option<ShareServerConfig>,
}

impl Plugin for ShareDialPlugin {
    fn build(&self, app: &mut App) {
        let Some(share_config) = self.share_config.clone() else {
            return;
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("kaijutsu-share-server")
            .enable_all()
            .build()
            .expect("share-server tokio runtime");
        app.insert_resource(ShareDial {
            rt,
            ssh_config: self.ssh_config.clone(),
            share_config,
        })
        .add_systems(Update, dial_on_connect);
    }
}

#[derive(Resource)]
struct ShareDial {
    rt: tokio::runtime::Runtime,
    ssh_config: SshConfig,
    share_config: ShareServerConfig,
}

/// Re-offers the share session on every connect/reconnect edge
/// (`docs/slash-r.md`: "Reconnect re-offers automatically because the arg is
/// still there"). Detected locally off [`RpcConnectionState::connected`]
/// rather than the actor's `ServerEvent::Reconnected` — that event is
/// deliberately never sent for the very FIRST connect (it hydrates via the
/// bootstrap's `ActorReady` instead), and the share dial needs to fire on
/// both.
fn dial_on_connect(
    state: Res<RpcConnectionState>,
    dial: Res<ShareDial>,
    mut was_connected: Local<bool>,
) {
    if state.connected && !*was_connected {
        let ssh_config = dial.ssh_config.clone();
        let share_config = dial.share_config.clone();
        dial.rt.spawn(async move {
            if let Err(e) = serve_once(ssh_config, share_config).await {
                log::warn!("/r share session failed to start: {e}");
            }
        });
    }
    *was_connected = state.connected;
}

/// Dial the dedicated `kaijutsu-share` subsystem connection and serve until
/// the process exits.
///
/// `russh_sftp::server::run` spawns its own processing loop internally and
/// returns immediately — it exposes no "wait until closed" signal (the same
/// gap `kaijutsu-server/src/share.rs`'s `ClosedSignalStream` works around
/// server-side). There's no clean way to detect "the far end closed the
/// share channel" from here, so this task parks forever, keeping the
/// underlying `SshClient` (and its TCP connection) alive for exactly as long
/// as that loop needs it. Each reconnect leaks one `SshClient`'s Rust-side
/// handle for the process lifetime — acceptable at the reconnect rates this
/// app sees (laptop sleep/wake, not a tight loop), and documented rather than
/// silently accepted.
async fn serve_once(
    ssh_config: SshConfig,
    share_config: ShareServerConfig,
) -> Result<(), kaijutsu_client::SshError> {
    let mut ssh = SshClient::new(ssh_config);
    let channel = ssh.connect_subsystem(SSH_SHARE_SUBSYSTEM).await?;
    log::info!("/r share session dialed; serving configured shares");
    let handler = ShareHandler::new(share_config);
    russh_sftp::server::run(channel.into_stream(), handler).await;
    std::future::pending::<()>().await;
    // Unreachable — `pending()` never resolves — but the compiler still
    // needs a value of the declared return type here.
    Ok(())
}

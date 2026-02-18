//! Bootstrap thread for spawning RPC actors.
//!
//! Owns a tokio runtime + LocalSet so Cap'n Proto's !Send types stay on one thread.
//! The only job: receive spawn commands, create actors, send handles back to Bevy.

use std::sync::Mutex;
use std::thread;

use bevy::prelude::*;
use kaijutsu_client::{ActorHandle, SshConfig};
use tokio::sync::mpsc;

// ============================================================================
// Channel Types
// ============================================================================

/// Command sent from Bevy → bootstrap thread.
#[derive(Debug)]
pub enum BootstrapCommand {
    /// Spawn a new actor for the given kernel/context.
    SpawnActor {
        config: SshConfig,
        kernel_id: String,
        context_name: Option<String>,
        instance: String,
    },
}

/// Result sent from bootstrap thread → Bevy.
#[allow(dead_code)]
pub enum BootstrapResult {
    /// Actor spawned successfully.
    ActorReady {
        handle: ActorHandle,
        generation: u64,
        kernel_id: String,
        context_name: Option<String>,
    },
    /// Spawn failed (e.g. initial SSH connect failure).
    /// The actor will retry internally, but we report the first error.
    Error(String),
}

/// Channel pair for Bevy ↔ bootstrap thread communication.
///
/// `rx` is wrapped in `Mutex` because `tokio::mpsc::UnboundedReceiver` is
/// Send but !Sync — the Mutex makes it Sync with zero real contention
/// (single system polls it).
#[derive(Resource)]
pub struct BootstrapChannel {
    pub tx: mpsc::UnboundedSender<BootstrapCommand>,
    pub rx: Mutex<mpsc::UnboundedReceiver<BootstrapResult>>,
}

// ============================================================================
// Bootstrap Thread
// ============================================================================

/// Spawn the bootstrap thread and return a channel for communication.
///
/// The thread runs a single-threaded tokio runtime with a LocalSet,
/// which is required for Cap'n Proto's !Send types.
pub fn spawn_bootstrap_thread() -> BootstrapChannel {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (result_tx, result_rx) = mpsc::unbounded_channel();

    thread::Builder::new()
        .name("kaijutsu-bootstrap".into())
        .spawn(move || {
            bootstrap_thread(cmd_rx, result_tx);
        })
        .expect("Failed to spawn bootstrap thread");

    BootstrapChannel {
        tx: cmd_tx,
        rx: Mutex::new(result_rx),
    }
}

/// The bootstrap thread main function.
///
/// Creates a single-threaded tokio runtime and runs a LocalSet
/// so that Cap'n Proto's !Send RPC types can live here.
fn bootstrap_thread(
    mut cmd_rx: mpsc::UnboundedReceiver<BootstrapCommand>,
    result_tx: mpsc::UnboundedSender<BootstrapResult>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    rt.block_on(async {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut generation: u64 = 0;

                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        BootstrapCommand::SpawnActor {
                            config,
                            kernel_id,
                            context_name,
                            instance,
                        } => {
                            generation += 1;
                            let current_gen = generation;

                            log::info!(
                                "Bootstrap: spawning actor generation={} kernel={} context={:?} instance={}",
                                current_gen, kernel_id, context_name, instance
                            );

                            // spawn_actor creates the actor in this LocalSet.
                            // It will auto-connect on first command (lazy connect).
                            //
                            // NOTE: Each SpawnActor creates a fresh SSH connection.
                            // RpcClient/KernelHandle are !Send (capnp), so we can't
                            // extract them from the old actor via the Send channel.
                            // SSH handshake is ~10-50ms localhost, ~100-300ms remote.
                            // Future optimization: coordinate handoff locally within
                            // this LocalSet if latency becomes an issue.
                            let handle = kaijutsu_client::spawn_actor(
                                config,
                                kernel_id.clone(),
                                context_name.clone(),
                                instance,
                                None,
                            );

                            let _ = result_tx.send(BootstrapResult::ActorReady {
                                handle,
                                generation: current_gen,
                                kernel_id,
                                context_name,
                            });
                        }
                    }
                }

                log::debug!("Bootstrap thread exiting: command channel closed");
            })
            .await;
    });
}

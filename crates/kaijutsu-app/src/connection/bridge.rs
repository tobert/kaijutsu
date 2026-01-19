//! Bevy integration bridge for async connection
//!
//! Runs SSH + RPC in a dedicated thread, communicates with Bevy via channels.
//! Handles auto-connect and reconnection with exponential backoff.

use std::thread;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use tokio::sync::mpsc;

// Use types from the client library
use kaijutsu_client::{Identity, KernelConfig, KernelHandle, KernelInfo, RpcClient, SshConfig};

use crate::constants::{DEFAULT_KERNEL_ID, DEFAULT_SERVER_ADDRESS};

/// Commands sent from Bevy to the connection thread
#[derive(Debug)]
#[allow(dead_code)]
pub enum ConnectionCommand {
    /// Connect to a server via SSH
    ConnectSsh { config: SshConfig },
    /// Connect to a local TCP server (for testing)
    ConnectTcp { addr: String },
    /// Disconnect from server
    Disconnect,
    /// Get current identity
    Whoami,
    /// List available kernels
    ListKernels,
    /// Attach to a kernel by ID
    AttachKernel { id: String },
    /// Create a new kernel
    CreateKernel { config: KernelConfig },
    /// Detach from current kernel
    DetachKernel,

    // LLM operations (server-side)
    /// Send a prompt to the server-side LLM
    Prompt {
        content: String,
        model: Option<String>,
        cell_id: String,
    },
}

/// Events sent from the connection thread to Bevy
///
/// Note: Some variants/fields are sent by the server but not yet handled in the UI.
/// These are kept for completeness and future UI implementation.
#[derive(Debug, Clone, Message)]
#[allow(dead_code)] // TODO: Handle all variants in UI
pub enum ConnectionEvent {
    /// Successfully connected to server
    Connected,
    /// Disconnected from server (will auto-reconnect unless manual disconnect)
    Disconnected,
    /// Connection attempt failed
    ConnectionFailed(String),
    /// Attempting to reconnect (with attempt number)
    Reconnecting {
        attempt: u32,
        delay_secs: u32, // TODO: Display in UI
    },
    /// Identity received
    Identity(Identity),
    /// Kernel list received
    KernelList(Vec<KernelInfo>), // TODO: Display in kernel picker UI
    /// Attached to a kernel
    AttachedKernel(KernelInfo),
    /// Detached from a kernel
    DetachedKernel,
    /// Error occurred
    Error(String), // TODO: Display in notification/toast UI

    // LLM events (server-side)
    /// Prompt was sent to server-side LLM
    PromptSent {
        prompt_id: String,
        cell_id: String,
    }, // TODO: Track prompt state in UI
}

/// Resource holding the command sender
#[derive(Resource)]
pub struct ConnectionCommands(pub mpsc::UnboundedSender<ConnectionCommand>);

impl ConnectionCommands {
    pub fn send(&self, cmd: ConnectionCommand) {
        let _ = self.0.send(cmd);
    }
}

/// Resource holding the event receiver
#[derive(Resource)]
pub struct ConnectionEvents(pub mpsc::UnboundedReceiver<ConnectionEvent>);

/// Resource tracking current connection state
#[derive(Resource)]
pub struct ConnectionState {
    pub connected: bool,
    pub identity: Option<Identity>,
    pub current_kernel: Option<KernelInfo>,
    /// Target address for (re)connection
    pub target_addr: String,
    /// Whether auto-reconnect is enabled
    pub auto_reconnect: bool,
    /// Current reconnect attempt (0 = not reconnecting)
    pub reconnect_attempt: u32,
    /// Time of last connection attempt
    pub last_attempt: Option<Instant>,
    /// Whether we've ever successfully connected
    pub was_connected: bool,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self {
            connected: false,
            identity: None,
            current_kernel: None,
            target_addr: DEFAULT_SERVER_ADDRESS.to_string(),
            auto_reconnect: true,
            reconnect_attempt: 0,
            last_attempt: None,
            was_connected: false,
        }
    }
}

/// Plugin for connection management
pub struct ConnectionBridgePlugin;

impl Plugin for ConnectionBridgePlugin {
    fn build(&self, app: &mut App) {
        // Create channels
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel();

        // Spawn connection thread
        thread::spawn(move || {
            connection_thread(cmd_rx, evt_tx);
        });

        // Register resources and systems
        app.insert_resource(ConnectionCommands(cmd_tx))
            .insert_resource(ConnectionEvents(evt_rx))
            .init_resource::<ConnectionState>()
            .add_message::<ConnectionEvent>()
            .add_systems(
                Update,
                (
                    poll_connection_events,
                    update_connection_state,
                    auto_reconnect_system,
                ),
            );
    }
}

/// System that polls connection events and forwards them to Bevy's message system
fn poll_connection_events(
    mut receiver: ResMut<ConnectionEvents>,
    mut events: MessageWriter<ConnectionEvent>,
) {
    // Drain all pending events
    while let Ok(event) = receiver.0.try_recv() {
        events.write(event);
    }
}

/// System that updates connection state from events
fn update_connection_state(
    mut state: ResMut<ConnectionState>,
    mut events: MessageReader<ConnectionEvent>,
    cmds: Res<ConnectionCommands>,
) {
    for event in events.read() {
        match event {
            ConnectionEvent::Connected => {
                state.connected = true;
                state.was_connected = true;
                state.reconnect_attempt = 0;
                // Auto-attach to lobby kernel
                cmds.send(ConnectionCommand::AttachKernel {
                    id: DEFAULT_KERNEL_ID.to_string(),
                });
            }
            ConnectionEvent::Disconnected => {
                state.connected = false;
                state.identity = None;
                state.current_kernel = None;
                // Don't reset reconnect_attempt - let auto_reconnect_system handle it
            }
            ConnectionEvent::ConnectionFailed(_) => {
                state.connected = false;
                state.identity = None;
                state.current_kernel = None;
                state.last_attempt = Some(Instant::now());
                // Increment attempt counter for backoff
                if state.auto_reconnect {
                    state.reconnect_attempt += 1;
                }
            }
            ConnectionEvent::Reconnecting { attempt, .. } => {
                state.reconnect_attempt = *attempt;
            }
            ConnectionEvent::Identity(id) => {
                state.identity = Some(id.clone());
            }
            ConnectionEvent::AttachedKernel(info) => {
                state.current_kernel = Some(info.clone());
            }
            ConnectionEvent::DetachedKernel => {
                state.current_kernel = None;
            }
            _ => {}
        }
    }
}

/// Calculate backoff delay for reconnection attempts
fn backoff_delay(attempt: u32) -> Duration {
    // Exponential backoff: 1s, 2s, 4s, 8s, 16s, max 30s
    let secs = match attempt {
        0 => 1,
        1 => 2,
        2 => 4,
        3 => 8,
        4 => 16,
        _ => 30,
    };
    Duration::from_secs(secs)
}

/// System that handles auto-reconnection with exponential backoff
fn auto_reconnect_system(
    mut state: ResMut<ConnectionState>,
    cmds: Res<ConnectionCommands>,
    mut events: MessageWriter<ConnectionEvent>,
) {
    // Skip if connected, auto-reconnect disabled, or no target
    if state.connected || !state.auto_reconnect || state.target_addr.is_empty() {
        return;
    }

    // Check if we should attempt reconnection
    let should_attempt = match state.last_attempt {
        None => true, // Never attempted, try now (startup)
        Some(last) => {
            let delay = backoff_delay(state.reconnect_attempt);
            last.elapsed() >= delay
        }
    };

    if should_attempt {
        let attempt = state.reconnect_attempt + 1;
        let delay = backoff_delay(attempt);

        // Notify UI about reconnection attempt
        events.write(ConnectionEvent::Reconnecting {
            attempt,
            delay_secs: delay.as_secs() as u32,
        });

        // Update state
        state.last_attempt = Some(Instant::now());

        // Send connect command
        let addr = state.target_addr.clone();
        cmds.send(ConnectionCommand::ConnectTcp { addr });
    }
}

/// The connection thread - runs tokio with LocalSet for capnp-rpc
fn connection_thread(
    mut cmd_rx: mpsc::UnboundedReceiver<ConnectionCommand>,
    evt_tx: mpsc::UnboundedSender<ConnectionEvent>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    rt.block_on(async {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                connection_loop(&mut cmd_rx, &evt_tx).await;
            })
            .await;
    });
}

/// Main connection loop - handles commands and manages connection state
async fn connection_loop(
    cmd_rx: &mut mpsc::UnboundedReceiver<ConnectionCommand>,
    evt_tx: &mpsc::UnboundedSender<ConnectionEvent>,
) {
    use kaijutsu_client::SshClient;
    use tokio::net::TcpStream;
    use tokio_util::compat::TokioAsyncReadCompatExt;

    let mut _ssh_client: Option<SshClient> = None;
    let mut rpc_client: Option<RpcClient> = None;
    let mut current_kernel: Option<KernelHandle> = None;

    loop {
        let Some(cmd) = cmd_rx.recv().await else {
            break;
        };

        match cmd {
            ConnectionCommand::ConnectSsh { config } => {
                log::info!("Connecting via SSH to {}:{}", config.host, config.port);

                let mut client = SshClient::new(config);
                match client.connect().await {
                    Ok(channels) => {
                        let rpc_stream = channels.rpc.into_stream();
                        match RpcClient::new(rpc_stream).await {
                            Ok(rpc) => {
                                _ssh_client = Some(client);
                                rpc_client = Some(rpc);
                                let _ = evt_tx.send(ConnectionEvent::Connected);
                            }
                            Err(e) => {
                                let _ = evt_tx.send(ConnectionEvent::ConnectionFailed(format!(
                                    "RPC init failed: {}",
                                    e
                                )));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = evt_tx.send(ConnectionEvent::ConnectionFailed(format!(
                            "SSH failed: {}",
                            e
                        )));
                    }
                }
            }

            ConnectionCommand::ConnectTcp { addr } => {
                log::info!("Connecting via TCP to {}", addr);

                match TcpStream::connect(&addr).await {
                    Ok(stream) => match RpcClient::from_stream(stream.compat()).await {
                        Ok(rpc) => {
                            rpc_client = Some(rpc);
                            let _ = evt_tx.send(ConnectionEvent::Connected);
                        }
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::ConnectionFailed(format!(
                                "RPC init failed: {}",
                                e
                            )));
                        }
                    },
                    Err(e) => {
                        let _ = evt_tx.send(ConnectionEvent::ConnectionFailed(format!(
                            "TCP connect failed: {}",
                            e
                        )));
                    }
                }
            }

            ConnectionCommand::Disconnect => {
                if let Some(mut client) = _ssh_client.take() {
                    let _ = client.disconnect().await;
                }
                rpc_client = None;
                current_kernel = None;
                let _ = evt_tx.send(ConnectionEvent::Disconnected);
            }

            ConnectionCommand::Whoami => {
                if let Some(rpc) = &rpc_client {
                    match rpc.whoami().await {
                        Ok(identity) => {
                            let _ = evt_tx.send(ConnectionEvent::Identity(identity));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not connected".into()));
                }
            }

            ConnectionCommand::ListKernels => {
                if let Some(rpc) = &rpc_client {
                    match rpc.list_kernels().await {
                        Ok(kernels) => {
                            let _ = evt_tx.send(ConnectionEvent::KernelList(kernels));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not connected".into()));
                }
            }

            ConnectionCommand::AttachKernel { id } => {
                if let Some(rpc) = &rpc_client {
                    match rpc.attach_kernel(&id).await {
                        Ok(kernel) => match kernel.get_info().await {
                            Ok(info) => {
                                current_kernel = Some(kernel);
                                let _ = evt_tx.send(ConnectionEvent::AttachedKernel(info));
                            }
                            Err(e) => {
                                let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                            }
                        },
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not connected".into()));
                }
            }

            ConnectionCommand::CreateKernel { config } => {
                if let Some(rpc) = &rpc_client {
                    match rpc.create_kernel(config).await {
                        Ok(kernel) => match kernel.get_info().await {
                            Ok(info) => {
                                current_kernel = Some(kernel);
                                let _ = evt_tx.send(ConnectionEvent::AttachedKernel(info));
                            }
                            Err(e) => {
                                let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                            }
                        },
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not connected".into()));
                }
            }

            ConnectionCommand::DetachKernel => {
                if let Some(kernel) = current_kernel.take() {
                    let _ = kernel.detach().await;
                    let _ = evt_tx.send(ConnectionEvent::DetachedKernel);
                }
            }

            // LLM commands
            ConnectionCommand::Prompt {
                content,
                model,
                cell_id,
            } => {
                if let Some(kernel) = &current_kernel {
                    match kernel.prompt(&content, model.as_deref(), &cell_id).await {
                        Ok(prompt_id) => {
                            let _ = evt_tx.send(ConnectionEvent::PromptSent { prompt_id, cell_id });
                        }
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not attached to a kernel".into()));
                }
            }
        }
    }
}

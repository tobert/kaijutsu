//! Bevy integration bridge for async connection
//!
//! Runs SSH + RPC in a dedicated thread, communicates with Bevy via channels.

use std::thread;

use bevy::prelude::*;
use tokio::sync::mpsc;

use super::rpc::{Identity, RoomInfo, Row};
use super::ssh::SshConfig;

/// Commands sent from Bevy to the connection thread
#[derive(Debug)]
pub enum ConnectionCommand {
    /// Connect to a server
    Connect { config: SshConfig },
    /// Disconnect from server
    Disconnect,
    /// Get current identity
    Whoami,
    /// List available rooms
    ListRooms,
    /// Join a room by name
    JoinRoom { name: String },
    /// Send a message to the current room
    SendMessage { content: String },
    /// Execute code in the room's kernel
    Execute { code: String },
}

/// Events sent from the connection thread to Bevy
#[derive(Debug, Clone, Message)]
pub enum ConnectionEvent {
    /// Connection status changed
    Connected,
    Disconnected,
    ConnectionError(String),
    /// Identity received
    Identity(Identity),
    /// Room list received
    RoomList(Vec<RoomInfo>),
    /// Joined a room
    JoinedRoom { name: String },
    /// Left a room
    LeftRoom,
    /// New message received
    NewMessage(Row),
    /// Kernel execution started
    ExecutionStarted { exec_id: u64 },
    /// Kernel output received
    KernelOutput { exec_id: u64, output: String },
    /// Error occurred
    Error(String),
}

/// Resource holding the command sender
#[derive(Resource)]
pub struct ConnectionCommandSender(pub mpsc::UnboundedSender<ConnectionCommand>);

/// Resource holding the event receiver
#[derive(Resource)]
pub struct ConnectionEventReceiver(pub mpsc::UnboundedReceiver<ConnectionEvent>);

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
        app.insert_resource(ConnectionCommandSender(cmd_tx))
            .insert_resource(ConnectionEventReceiver(evt_rx))
            .add_message::<ConnectionEvent>()
            .add_systems(Update, poll_connection_events);
    }
}

/// System that polls connection events and forwards them to Bevy's message system
fn poll_connection_events(
    mut receiver: ResMut<ConnectionEventReceiver>,
    mut events: MessageWriter<ConnectionEvent>,
) {
    // Non-blocking poll for all available events
    while let Ok(event) = receiver.0.try_recv() {
        events.write(event);
    }
}

/// The connection thread - runs tokio with LocalSet for capnp-rpc
fn connection_thread(
    mut cmd_rx: mpsc::UnboundedReceiver<ConnectionCommand>,
    evt_tx: mpsc::UnboundedSender<ConnectionEvent>,
) {
    // Create a single-threaded tokio runtime
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
    use super::rpc::RpcClient;
    use super::ssh::SshClient;

    let mut ssh_client: Option<SshClient> = None;
    let mut rpc_client: Option<RpcClient> = None;

    loop {
        let Some(cmd) = cmd_rx.recv().await else {
            // Channel closed, exit
            break;
        };

        match cmd {
            ConnectionCommand::Connect { config } => {
                log::info!("Connecting to {}:{}", config.host, config.port);

                let mut client = SshClient::new(config);
                match client.connect().await {
                    Ok(channels) => {
                        // Convert RPC channel to stream and create RPC client
                        let rpc_stream = channels.rpc.into_stream();
                        match RpcClient::new(rpc_stream).await {
                            Ok(rpc) => {
                                ssh_client = Some(client);
                                rpc_client = Some(rpc);
                                let _ = evt_tx.send(ConnectionEvent::Connected);
                            }
                            Err(e) => {
                                let _ = evt_tx
                                    .send(ConnectionEvent::ConnectionError(format!("RPC: {}", e)));
                            }
                        }
                    }
                    Err(e) => {
                        let _ =
                            evt_tx.send(ConnectionEvent::ConnectionError(format!("SSH: {}", e)));
                    }
                }
            }

            ConnectionCommand::Disconnect => {
                if let Some(mut client) = ssh_client.take() {
                    let _ = client.disconnect().await;
                }
                rpc_client = None;
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
                    let _ = evt_tx.send(ConnectionEvent::Error("Not connected".to_string()));
                }
            }

            ConnectionCommand::ListRooms => {
                if let Some(rpc) = &rpc_client {
                    match rpc.list_rooms().await {
                        Ok(rooms) => {
                            let _ = evt_tx.send(ConnectionEvent::RoomList(rooms));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not connected".to_string()));
                }
            }

            ConnectionCommand::JoinRoom { name } => {
                // TODO: Implement room join with subscription handling
                let _ = evt_tx.send(ConnectionEvent::Error("Room join not yet implemented".to_string()));
            }

            ConnectionCommand::SendMessage { content: _ } => {
                // TODO: Implement message sending
                let _ = evt_tx.send(ConnectionEvent::Error("Send message not yet implemented".to_string()));
            }

            ConnectionCommand::Execute { code: _ } => {
                // TODO: Implement kernel execution
                let _ = evt_tx.send(ConnectionEvent::Error("Execute not yet implemented".to_string()));
            }
        }
    }
}

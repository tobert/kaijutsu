//! Bevy integration bridge for async connection
//!
//! Runs SSH + RPC in a dedicated thread, communicates with Bevy via channels.

use std::thread;

use bevy::prelude::*;
use tokio::sync::mpsc;

// Use types from the client library
use kaijutsu_client::{Identity, RoomConfig, RoomHandle, RoomInfo, Row, RpcClient, SshConfig};

/// Commands sent from Bevy to the connection thread
#[derive(Debug)]
pub enum ConnectionCommand {
    /// Connect to a server via SSH
    ConnectSsh { config: SshConfig },
    /// Connect to a local TCP server (for testing)
    ConnectTcp { addr: String },
    /// Disconnect from server
    Disconnect,
    /// Get current identity
    Whoami,
    /// List available rooms
    ListRooms,
    /// Join a room by name
    JoinRoom { name: String },
    /// Create a new room
    CreateRoom { config: RoomConfig },
    /// Leave current room
    LeaveRoom,
    /// Send a message to the current room
    SendMessage { content: String },
    /// Mention an agent
    MentionAgent { agent: String, content: String },
    /// Get room history
    GetHistory { limit: u32 },
}

/// Events sent from the connection thread to Bevy
#[derive(Debug, Clone, Message)]
pub enum ConnectionEvent {
    /// Connection status changed
    Connected,
    Disconnected,
    ConnectionFailed(String),
    /// Identity received
    Identity(Identity),
    /// Room list received
    RoomList(Vec<RoomInfo>),
    /// Joined a room
    JoinedRoom(RoomInfo),
    /// Left a room
    LeftRoom,
    /// New message (from send or history)
    NewMessage(Row),
    /// Multiple messages (from history)
    History(Vec<Row>),
    /// Error occurred
    Error(String),
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
#[derive(Resource, Default)]
pub struct ConnectionState {
    pub connected: bool,
    pub identity: Option<Identity>,
    pub current_room: Option<RoomInfo>,
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
            .add_systems(Update, (poll_connection_events, update_connection_state));
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
) {
    for event in events.read() {
        match event {
            ConnectionEvent::Connected => {
                state.connected = true;
            }
            ConnectionEvent::Disconnected | ConnectionEvent::ConnectionFailed(_) => {
                state.connected = false;
                state.identity = None;
                state.current_room = None;
            }
            ConnectionEvent::Identity(id) => {
                state.identity = Some(id.clone());
            }
            ConnectionEvent::JoinedRoom(info) => {
                state.current_room = Some(info.clone());
            }
            ConnectionEvent::LeftRoom => {
                state.current_room = None;
            }
            _ => {}
        }
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
    let mut current_room: Option<RoomHandle> = None;

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
                current_room = None;
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
                    let _ = evt_tx.send(ConnectionEvent::Error("Not connected".into()));
                }
            }

            ConnectionCommand::JoinRoom { name } => {
                if let Some(rpc) = &rpc_client {
                    match rpc.join_room(&name).await {
                        Ok(room) => match room.get_info().await {
                            Ok(info) => {
                                current_room = Some(room);
                                let _ = evt_tx.send(ConnectionEvent::JoinedRoom(info));
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

            ConnectionCommand::CreateRoom { config } => {
                if let Some(rpc) = &rpc_client {
                    match rpc.create_room(config).await {
                        Ok(room) => match room.get_info().await {
                            Ok(info) => {
                                current_room = Some(room);
                                let _ = evt_tx.send(ConnectionEvent::JoinedRoom(info));
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

            ConnectionCommand::LeaveRoom => {
                if let Some(room) = current_room.take() {
                    let _ = room.leave().await;
                    let _ = evt_tx.send(ConnectionEvent::LeftRoom);
                }
            }

            ConnectionCommand::SendMessage { content } => {
                if let Some(room) = &current_room {
                    match room.send(&content).await {
                        Ok(row) => {
                            let _ = evt_tx.send(ConnectionEvent::NewMessage(row));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not in a room".into()));
                }
            }

            ConnectionCommand::MentionAgent { agent, content } => {
                if let Some(room) = &current_room {
                    match room.mention(&agent, &content).await {
                        Ok(row) => {
                            let _ = evt_tx.send(ConnectionEvent::NewMessage(row));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not in a room".into()));
                }
            }

            ConnectionCommand::GetHistory { limit } => {
                if let Some(room) = &current_room {
                    match room.get_history(limit, 0).await {
                        Ok(rows) => {
                            let _ = evt_tx.send(ConnectionEvent::History(rows));
                        }
                        Err(e) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(e.to_string()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not in a room".into()));
                }
            }
        }
    }
}

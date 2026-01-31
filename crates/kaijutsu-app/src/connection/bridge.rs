//! Bevy integration bridge for async connection
//!
//! Runs SSH + RPC in a dedicated thread, communicates with Bevy via channels.
//! Handles auto-connect and reconnection with exponential backoff.

use std::thread;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use tokio::sync::mpsc;
use tokio::time::timeout;

// Use types from the client library
use kaijutsu_client::{
    Context, Identity, KernelConfig, KernelHandle, KernelInfo, RpcClient, SeatInfo, SshConfig,
};

/// Default timeout for RPC operations (short ops like attach, list)
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for LLM prompt operations (now returns immediately, streaming happens async)
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

use crate::constants::DEFAULT_KERNEL_ID;

/// Commands sent from Bevy to the connection thread
#[derive(Debug)]
#[allow(dead_code)]
pub enum ConnectionCommand {
    /// Connect to a server via SSH
    ConnectSsh { config: SshConfig },
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

    // Seat/context operations (for dashboard)
    /// List contexts in the current kernel
    ListContexts,
    /// List user's active seats across all kernels
    ListMySeats,
    /// Join a context (creates a seat)
    JoinContext { context: String, instance: String },
    /// Leave the current seat
    LeaveSeat,
    /// Take an existing seat (switch to it)
    TakeSeat {
        nick: String,
        instance: String,
        kernel: String,
        context: String,
    },

    // LLM operations (server-side)
    /// Send a prompt to the server-side LLM
    Prompt {
        content: String,
        model: Option<String>,
        cell_id: String,
    },

    // Shell operations (kaish REPL)
    /// Execute a shell command via kaish
    ShellExecute {
        command: String,
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

    // Seat/context events (for dashboard)
    /// List of contexts in the current kernel
    ContextsList(Vec<Context>),
    /// List of user's active seats
    MySeatsList(Vec<SeatInfo>),
    /// User took a seat
    SeatTaken { seat: SeatInfo },
    /// User left their seat
    SeatLeft,
    /// Initial block cell state received (full oplog for frontier-based sync)
    BlockCellInitialState {
        cell_id: String,
        /// Full oplog for document reconstruction
        ops: Vec<u8>,
        /// Block snapshots for immediate display
        blocks: Vec<kaijutsu_crdt::BlockSnapshot>,
    },

    // LLM events (server-side)
    /// Prompt was sent to server-side LLM
    PromptSent {
        prompt_id: String,
        cell_id: String,
    }, // TODO: Track prompt state in UI

    // Block streaming events (from server-side LLM processing)
    /// A block was inserted into a cell
    BlockInserted {
        cell_id: String,
        block: Box<kaijutsu_crdt::BlockSnapshot>,
        /// CRDT ops that created this block (for sync)
        ops: Vec<u8>,
    },
    /// CRDT operations for a block's text content
    BlockTextOps {
        cell_id: String,
        block_id: kaijutsu_crdt::BlockId,
        ops: Vec<u8>,
    },
    /// A block's status changed
    BlockStatusChanged {
        cell_id: String,
        block_id: kaijutsu_crdt::BlockId,
        status: kaijutsu_crdt::Status,
    },
    /// A block was deleted
    BlockDeleted {
        cell_id: String,
        block_id: kaijutsu_crdt::BlockId,
    },
    /// A block's collapsed state changed
    BlockCollapsedChanged {
        cell_id: String,
        block_id: kaijutsu_crdt::BlockId,
        collapsed: bool,
    },
    /// A block was moved to a new position
    BlockMoved {
        cell_id: String,
        block_id: kaijutsu_crdt::BlockId,
        after_id: Option<kaijutsu_crdt::BlockId>,
    },
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
    /// SSH configuration for (re)connection
    pub ssh_config: SshConfig,
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
            ssh_config: SshConfig::default(),
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
    // Skip if connected or auto-reconnect disabled
    if state.connected || !state.auto_reconnect {
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

        // Send connect command via SSH
        cmds.send(ConnectionCommand::ConnectSsh {
            config: state.ssh_config.clone(),
        });
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
                    let attach_result = timeout(RPC_TIMEOUT, async {
                        let kernel = rpc.attach_kernel(&id).await?;
                        let info = kernel.get_info().await?;
                        Ok::<_, kaijutsu_client::RpcError>((kernel, info))
                    }).await;

                    match attach_result {
                        Ok(Ok((kernel, info))) => {
                            // Subscribe to block events for real-time streaming updates
                            let callback = BlockEventsCallback {
                                evt_tx: evt_tx.clone(),
                            };
                            let client = capnp_rpc::new_client(callback);
                            if let Err(e) = kernel.subscribe_blocks(client).await {
                                log::warn!("Block subscription failed: {}", e);
                            } else {
                                log::info!("Subscribed to block events for kernel {}", id);
                            }

                            current_kernel = Some(kernel);
                            let _ = evt_tx.send(ConnectionEvent::AttachedKernel(info));
                        }
                        Ok(Err(e)) => {
                            let err_str = e.to_string();
                            if is_connection_error(&err_str) {
                                log::warn!("Connection lost during attach: {}", err_str);
                                rpc_client = None;
                                let _ = evt_tx.send(ConnectionEvent::Disconnected);
                            } else {
                                let _ = evt_tx.send(ConnectionEvent::Error(err_str));
                            }
                        }
                        Err(_) => {
                            log::warn!("Attach kernel RPC timed out");
                            rpc_client = None;
                            let _ = evt_tx.send(ConnectionEvent::Disconnected);
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
                                // Subscribe to block events for real-time streaming updates
                                // (same as AttachKernel)
                                let callback = BlockEventsCallback {
                                    evt_tx: evt_tx.clone(),
                                };
                                let client = capnp_rpc::new_client(callback);
                                if let Err(e) = kernel.subscribe_blocks(client).await {
                                    log::warn!("Block subscription failed: {}", e);
                                } else {
                                    log::info!("Subscribed to block events for new kernel {}", info.id);
                                }

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
                    match timeout(PROMPT_TIMEOUT, kernel.prompt(&content, model.as_deref(), &cell_id)).await {
                        Ok(Ok(prompt_id)) => {
                            let _ = evt_tx.send(ConnectionEvent::PromptSent { prompt_id, cell_id });
                        }
                        Ok(Err(e)) => {
                            let err_str = e.to_string();
                            // Check for connection-related errors
                            if is_connection_error(&err_str) {
                                log::warn!("Connection lost during prompt: {}", err_str);
                                rpc_client = None;
                                current_kernel = None;
                                let _ = evt_tx.send(ConnectionEvent::Disconnected);
                            } else {
                                let _ = evt_tx.send(ConnectionEvent::Error(err_str));
                            }
                        }
                        Err(_) => {
                            log::warn!("Prompt RPC timed out after {:?}", PROMPT_TIMEOUT);
                            // Timeout likely means connection is dead
                            rpc_client = None;
                            current_kernel = None;
                            let _ = evt_tx.send(ConnectionEvent::Disconnected);
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not attached to a kernel".into()));
                }
            }

            // Shell execution (kaish REPL with block output)
            ConnectionCommand::ShellExecute { command, cell_id } => {
                if let Some(kernel) = &current_kernel {
                    match timeout(PROMPT_TIMEOUT, kernel.shell_execute(&command, &cell_id)).await {
                        Ok(Ok(block_id)) => {
                            log::info!("Shell command block created: {:?}, cell_id={}", block_id, cell_id);
                            // Output will stream via block events
                        }
                        Ok(Err(e)) => {
                            let err_str = e.to_string();
                            if is_connection_error(&err_str) {
                                log::warn!("Connection lost during shell execute: {}", err_str);
                                rpc_client = None;
                                current_kernel = None;
                                let _ = evt_tx.send(ConnectionEvent::Disconnected);
                            } else {
                                let _ = evt_tx.send(ConnectionEvent::Error(err_str));
                            }
                        }
                        Err(_) => {
                            log::warn!("Shell execute RPC timed out after {:?}", PROMPT_TIMEOUT);
                            rpc_client = None;
                            current_kernel = None;
                            let _ = evt_tx.send(ConnectionEvent::Disconnected);
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not attached to a kernel".into()));
                }
            }

            // Seat/context commands
            ConnectionCommand::ListContexts => {
                if let Some(kernel) = &current_kernel {
                    match timeout(RPC_TIMEOUT, kernel.list_contexts()).await {
                        Ok(Ok(contexts)) => {
                            let _ = evt_tx.send(ConnectionEvent::ContextsList(contexts));
                        }
                        Ok(Err(e)) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(format!("Failed to list contexts: {}", e)));
                        }
                        Err(_) => {
                            log::warn!("ListContexts RPC timed out");
                            let _ = evt_tx.send(ConnectionEvent::Error("List contexts timed out".into()));
                        }
                    }
                } else {
                    // Return empty list if not attached
                    let _ = evt_tx.send(ConnectionEvent::ContextsList(vec![]));
                }
            }

            ConnectionCommand::ListMySeats => {
                if let Some(client) = &rpc_client {
                    match timeout(RPC_TIMEOUT, client.list_my_seats()).await {
                        Ok(Ok(seats)) => {
                            let _ = evt_tx.send(ConnectionEvent::MySeatsList(seats));
                        }
                        Ok(Err(e)) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(format!("Failed to list seats: {}", e)));
                        }
                        Err(_) => {
                            log::warn!("ListMySeats RPC timed out");
                            let _ = evt_tx.send(ConnectionEvent::Error("List seats timed out".into()));
                        }
                    }
                } else {
                    // Return empty list if not connected
                    let _ = evt_tx.send(ConnectionEvent::MySeatsList(vec![]));
                }
            }

            ConnectionCommand::JoinContext { context, instance } => {
                if let Some(kernel) = &current_kernel {
                    match timeout(RPC_TIMEOUT, kernel.join_context(&context, &instance)).await {
                        Ok(Ok(seat)) => {
                            // Use the document_id provided by the server (kernel's main document)
                            let document_id = seat.document_id.clone();

                            // Send SeatTaken FIRST - dashboard sets up conversation
                            let _ = evt_tx.send(ConnectionEvent::SeatTaken { seat });

                            // THEN fetch initial document state for frontier-based sync
                            // This provides the full oplog so the client can merge incremental ops
                            // Must come after SeatTaken so the document exists when we sync
                            match timeout(RPC_TIMEOUT, kernel.get_document_state(&document_id)).await {
                                Ok(Ok(state)) => {
                                    log::info!(
                                        "Fetched initial state for document_id={}: {} blocks, {} bytes oplog",
                                        document_id,
                                        state.blocks.len(),
                                        state.ops.len()
                                    );
                                    let _ = evt_tx.send(ConnectionEvent::BlockCellInitialState {
                                        cell_id: state.document_id,
                                        ops: state.ops,
                                        blocks: state.blocks,
                                    });
                                }
                                Ok(Err(e)) => {
                                    log::warn!("Failed to get initial block state: {}", e);
                                }
                                Err(_) => {
                                    log::warn!("get_document_state timed out for {}", document_id);
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(format!("Failed to join context: {}", e)));
                        }
                        Err(_) => {
                            log::warn!("JoinContext RPC timed out");
                            let _ = evt_tx.send(ConnectionEvent::Error("Join context timed out".into()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error("Not attached to a kernel".into()));
                }
            }

            ConnectionCommand::LeaveSeat => {
                if let Some(kernel) = &current_kernel {
                    match timeout(RPC_TIMEOUT, kernel.leave_seat()).await {
                        Ok(Ok(())) => {
                            let _ = evt_tx.send(ConnectionEvent::SeatLeft);
                        }
                        Ok(Err(e)) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(format!("Failed to leave seat: {}", e)));
                        }
                        Err(_) => {
                            log::warn!("LeaveSeat RPC timed out");
                            let _ = evt_tx.send(ConnectionEvent::Error("Leave seat timed out".into()));
                        }
                    }
                } else {
                    // If not attached, just send the event
                    let _ = evt_tx.send(ConnectionEvent::SeatLeft);
                }
            }

            ConnectionCommand::TakeSeat {
                nick: _,
                instance,
                kernel: kernel_name,
                context,
            } => {
                // TakeSeat is a convenience that attaches to kernel + joins context
                // For now, we just join the context if already attached to the right kernel
                if let Some(kernel) = &current_kernel {
                    // TODO: Check if we need to switch kernels first
                    // For now, just join the context with the given instance
                    match timeout(RPC_TIMEOUT, kernel.join_context(&context, &instance)).await {
                        Ok(Ok(seat)) => {
                            // Use the document_id provided by the server (kernel's main document)
                            let document_id = seat.document_id.clone();

                            // Send SeatTaken FIRST - dashboard sets up conversation
                            let _ = evt_tx.send(ConnectionEvent::SeatTaken { seat });

                            // THEN fetch initial document state for frontier-based sync
                            match timeout(RPC_TIMEOUT, kernel.get_document_state(&document_id)).await {
                                Ok(Ok(state)) => {
                                    log::info!(
                                        "Fetched initial state for document_id={}: {} blocks, {} bytes oplog",
                                        document_id,
                                        state.blocks.len(),
                                        state.ops.len()
                                    );
                                    let _ = evt_tx.send(ConnectionEvent::BlockCellInitialState {
                                        cell_id: state.document_id,
                                        ops: state.ops,
                                        blocks: state.blocks,
                                    });
                                }
                                Ok(Err(e)) => {
                                    log::warn!("Failed to get initial block state: {}", e);
                                }
                                Err(_) => {
                                    log::warn!("get_document_state timed out for {}", document_id);
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            let _ = evt_tx.send(ConnectionEvent::Error(format!("Failed to take seat: {}", e)));
                        }
                        Err(_) => {
                            log::warn!("TakeSeat RPC timed out");
                            let _ = evt_tx.send(ConnectionEvent::Error("Take seat timed out".into()));
                        }
                    }
                } else {
                    let _ = evt_tx.send(ConnectionEvent::Error(
                        format!("Not attached to kernel '{}' - attach first", kernel_name)
                    ));
                }
            }
        }
    }
}

/// Check if an error string indicates a connection problem
fn is_connection_error(err: &str) -> bool {
    let err_lower = err.to_lowercase();
    err_lower.contains("disconnected")
        || err_lower.contains("connection")
        || err_lower.contains("broken pipe")
        || err_lower.contains("reset by peer")
        || err_lower.contains("eof")
        || err_lower.contains("not connected")
}

// ============================================================================
// Block Events Callback
// ============================================================================

use std::rc::Rc;
use capnp::capability::Promise;
use kaijutsu_client::kaijutsu_capnp::block_events;

/// Callback struct for receiving block events from the server.
/// Note: Uses mpsc channel which is Send, so we can clone and send from Rc<Self>
struct BlockEventsCallback {
    evt_tx: mpsc::UnboundedSender<ConnectionEvent>,
}

#[allow(refining_impl_trait)]
impl block_events::Server for BlockEventsCallback {
    fn on_block_inserted(
        self: Rc<Self>,
        params: block_events::OnBlockInsertedParams,
        _results: block_events::OnBlockInsertedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let cell_id = match params.get_document_id() {
            Ok(s) => match s.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
            },
            Err(e) => return Promise::err(e),
        };

        let block = match params.get_block() {
            Ok(b) => match parse_block_snapshot(&b) {
                Ok(block) => block,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        // Extract CRDT ops that created this block
        let ops = match params.get_ops() {
            Ok(data) => data.to_vec(),
            Err(_) => vec![], // Empty if not present (backward compat)
        };

        let _ = self.evt_tx.send(ConnectionEvent::BlockInserted { cell_id, block: Box::new(block), ops });
        Promise::ok(())
    }

    fn on_block_deleted(
        self: Rc<Self>,
        params: block_events::OnBlockDeletedParams,
        _results: block_events::OnBlockDeletedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let cell_id = match params.get_document_id() {
            Ok(s) => match s.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let _ = self.evt_tx.send(ConnectionEvent::BlockDeleted { cell_id, block_id });
        Promise::ok(())
    }

    fn on_block_collapsed(
        self: Rc<Self>,
        params: block_events::OnBlockCollapsedParams,
        _results: block_events::OnBlockCollapsedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let cell_id = match params.get_document_id() {
            Ok(s) => match s.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let collapsed = params.get_collapsed();

        let _ = self.evt_tx.send(ConnectionEvent::BlockCollapsedChanged {
            cell_id,
            block_id,
            collapsed,
        });
        Promise::ok(())
    }

    fn on_block_moved(
        self: Rc<Self>,
        params: block_events::OnBlockMovedParams,
        _results: block_events::OnBlockMovedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let cell_id = match params.get_document_id() {
            Ok(s) => match s.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let after_id = if params.get_has_after_id() {
            match params.get_after_id() {
                Ok(b) => match parse_block_id(&b) {
                    Ok(id) => Some(id),
                    Err(e) => return Promise::err(e),
                },
                Err(e) => return Promise::err(e),
            }
        } else {
            None
        };

        let _ = self.evt_tx.send(ConnectionEvent::BlockMoved {
            cell_id,
            block_id,
            after_id,
        });
        Promise::ok(())
    }

    fn on_block_status_changed(
        self: Rc<Self>,
        params: block_events::OnBlockStatusChangedParams,
        _results: block_events::OnBlockStatusChangedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let cell_id = match params.get_document_id() {
            Ok(s) => match s.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let status = match params.get_status() {
            Ok(s) => match s {
                kaijutsu_client::kaijutsu_capnp::Status::Pending => kaijutsu_crdt::Status::Pending,
                kaijutsu_client::kaijutsu_capnp::Status::Running => kaijutsu_crdt::Status::Running,
                kaijutsu_client::kaijutsu_capnp::Status::Done => kaijutsu_crdt::Status::Done,
                kaijutsu_client::kaijutsu_capnp::Status::Error => kaijutsu_crdt::Status::Error,
            },
            Err(e) => return Promise::err(e.into()),
        };

        let _ = self.evt_tx.send(ConnectionEvent::BlockStatusChanged {
            cell_id,
            block_id,
            status,
        });
        Promise::ok(())
    }

    fn on_block_text_ops(
        self: Rc<Self>,
        params: block_events::OnBlockTextOpsParams,
        _results: block_events::OnBlockTextOpsResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let cell_id = match params.get_document_id() {
            Ok(s) => match s.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(e.to_string())),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let ops = match params.get_ops() {
            Ok(data) => data.to_vec(),
            Err(e) => return Promise::err(e),
        };

        let _ = self.evt_tx.send(ConnectionEvent::BlockTextOps {
            cell_id,
            block_id,
            ops,
        });
        Promise::ok(())
    }
}

// ============================================================================
// Block Snapshot Parser Helpers
// ============================================================================

fn parse_block_id(
    reader: &kaijutsu_client::kaijutsu_capnp::block_id::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockId, capnp::Error> {
    Ok(kaijutsu_crdt::BlockId {
        document_id: reader.get_document_id()?.to_str()?.to_owned(),
        agent_id: reader.get_agent_id()?.to_str()?.to_owned(),
        seq: reader.get_seq(),
    })
}

fn parse_block_snapshot(
    reader: &kaijutsu_client::kaijutsu_capnp::block_snapshot::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockSnapshot, capnp::Error> {
    let id_reader = reader.get_id()?;
    let id = kaijutsu_crdt::BlockId {
        document_id: id_reader.get_document_id()?.to_str()?.to_owned(),
        agent_id: id_reader.get_agent_id()?.to_str()?.to_owned(),
        seq: id_reader.get_seq(),
    };

    let parent_id = if reader.get_has_parent_id() {
        let pid_reader = reader.get_parent_id()?;
        Some(kaijutsu_crdt::BlockId {
            document_id: pid_reader.get_document_id()?.to_str()?.to_owned(),
            agent_id: pid_reader.get_agent_id()?.to_str()?.to_owned(),
            seq: pid_reader.get_seq(),
        })
    } else {
        None
    };

    let role = match reader.get_role()? {
        kaijutsu_client::kaijutsu_capnp::Role::User => kaijutsu_crdt::Role::User,
        kaijutsu_client::kaijutsu_capnp::Role::Model => kaijutsu_crdt::Role::Model,
        kaijutsu_client::kaijutsu_capnp::Role::System => kaijutsu_crdt::Role::System,
        kaijutsu_client::kaijutsu_capnp::Role::Tool => kaijutsu_crdt::Role::Tool,
    };

    let status = match reader.get_status()? {
        kaijutsu_client::kaijutsu_capnp::Status::Pending => kaijutsu_crdt::Status::Pending,
        kaijutsu_client::kaijutsu_capnp::Status::Running => kaijutsu_crdt::Status::Running,
        kaijutsu_client::kaijutsu_capnp::Status::Done => kaijutsu_crdt::Status::Done,
        kaijutsu_client::kaijutsu_capnp::Status::Error => kaijutsu_crdt::Status::Error,
    };

    let kind = match reader.get_kind()? {
        kaijutsu_client::kaijutsu_capnp::BlockKind::Text => kaijutsu_crdt::BlockKind::Text,
        kaijutsu_client::kaijutsu_capnp::BlockKind::Thinking => kaijutsu_crdt::BlockKind::Thinking,
        kaijutsu_client::kaijutsu_capnp::BlockKind::ToolCall => kaijutsu_crdt::BlockKind::ToolCall,
        kaijutsu_client::kaijutsu_capnp::BlockKind::ToolResult => kaijutsu_crdt::BlockKind::ToolResult,
        kaijutsu_client::kaijutsu_capnp::BlockKind::ShellCommand => kaijutsu_crdt::BlockKind::ShellCommand,
        kaijutsu_client::kaijutsu_capnp::BlockKind::ShellOutput => kaijutsu_crdt::BlockKind::ShellOutput,
    };

    let tool_call_id = if reader.get_has_tool_call_id() {
        let tc_reader = reader.get_tool_call_id()?;
        Some(kaijutsu_crdt::BlockId {
            document_id: tc_reader.get_document_id()?.to_str()?.to_owned(),
            agent_id: tc_reader.get_agent_id()?.to_str()?.to_owned(),
            seq: tc_reader.get_seq(),
        })
    } else {
        None
    };

    let tool_input = reader.get_tool_input()
        .ok()
        .and_then(|s| s.to_str().ok())
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(s).ok());

    // Read display hint from wire protocol
    let display_hint = if reader.get_has_display_hint() {
        reader.get_display_hint()
            .ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
    } else {
        None
    };

    Ok(kaijutsu_crdt::BlockSnapshot {
        id,
        parent_id,
        role,
        status,
        kind,
        content: reader.get_content()?.to_str()?.to_owned(),
        collapsed: reader.get_collapsed(),
        author: reader.get_author()?.to_str()?.to_owned(),
        created_at: reader.get_created_at(),
        tool_name: reader.get_tool_name().ok().and_then(|s| s.to_str().ok()).filter(|s| !s.is_empty()).map(|s| s.to_owned()),
        tool_input,
        tool_call_id,
        exit_code: if reader.get_has_exit_code() { Some(reader.get_exit_code()) } else { None },
        is_error: reader.get_is_error(),
        display_hint,
    })
}

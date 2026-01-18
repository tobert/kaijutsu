//! Cell synchronization with the server via CRDT operations.
//!
//! Handles:
//! - Auto-sync when attaching to a kernel
//! - Sending local edits to the server
//! - Receiving remote changes and applying them locally
//!
//! Uses block-based CRDT operations (BlockDocOp) for real-time sync.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::connection::{ConnectionCommand, ConnectionCommands, ConnectionEvent};
use kaijutsu_client::{CellKind as RemoteCellKind, CellOp, CellState, CellVersion, CrdtOp};

use super::components::{Cell, CellEditor, CellId, CellKind, CellPosition, PromptCell};
use crate::text::{GlyphonText, TextAreaConfig};

/// Marker component for cells that were deleted by the server.
/// Prevents delete_remote_cell from re-sending delete command.
#[derive(Component)]
pub struct RemotelyDeleted;

/// Tracks entities that were deleted by the server in the current frame.
/// This prevents delete_remote_cell from sending redundant delete commands.
#[derive(Resource, Default)]
pub struct RecentlyDeletedByServer(pub std::collections::HashSet<Entity>);

/// Queue of local entities waiting for server registration.
/// When we create a cell locally and send CreateCell to the server,
/// we add the entity here. When CellCreated comes back, we pop from
/// the queue and register the mapping.
#[derive(Resource, Default)]
pub struct PendingCellRegistrations(pub std::collections::VecDeque<Entity>);

/// Maps remote cell IDs to local entity IDs.
#[derive(Resource, Default)]
pub struct CellRegistry {
    /// Remote cell ID -> Local entity
    remote_to_local: HashMap<String, Entity>,
    /// Local entity -> Remote cell ID
    local_to_remote: HashMap<Entity, String>,
}

impl CellRegistry {
    /// Register a mapping between remote and local cell.
    pub fn register(&mut self, remote_id: String, entity: Entity) {
        self.remote_to_local.insert(remote_id.clone(), entity);
        self.local_to_remote.insert(entity, remote_id);
    }

    /// Unregister a cell.
    pub fn unregister(&mut self, entity: Entity) {
        if let Some(remote_id) = self.local_to_remote.remove(&entity) {
            self.remote_to_local.remove(&remote_id);
        }
    }

    /// Get local entity for a remote cell ID.
    pub fn get_local(&self, remote_id: &str) -> Option<Entity> {
        self.remote_to_local.get(remote_id).copied()
    }

    /// Get remote cell ID for a local entity.
    pub fn get_remote(&self, entity: Entity) -> Option<&str> {
        self.local_to_remote.get(&entity).map(|s| s.as_str())
    }
}

/// Convert local CellKind to remote CellKind.
fn local_kind_to_remote(kind: CellKind) -> RemoteCellKind {
    match kind {
        CellKind::Code => RemoteCellKind::Code,
        CellKind::Markdown => RemoteCellKind::Markdown,
        CellKind::Output => RemoteCellKind::Output,
        CellKind::System => RemoteCellKind::System,
        CellKind::UserMessage => RemoteCellKind::UserMessage,
        CellKind::AgentMessage => RemoteCellKind::AgentMessage,
    }
}

/// Convert remote CellKind to local CellKind.
fn remote_kind_to_local(kind: RemoteCellKind) -> CellKind {
    match kind {
        RemoteCellKind::Code => CellKind::Code,
        RemoteCellKind::Markdown => CellKind::Markdown,
        RemoteCellKind::Output => CellKind::Output,
        RemoteCellKind::System => CellKind::System,
        RemoteCellKind::UserMessage => CellKind::UserMessage,
        RemoteCellKind::AgentMessage => CellKind::AgentMessage,
    }
}

/// System: Trigger cell sync when attaching to a kernel.
/// Also sends pending CreateCell commands for local cells.
pub fn trigger_sync_on_attach(
    mut events: MessageReader<ConnectionEvent>,
    cmds: Option<Res<ConnectionCommands>>,
    registry: Res<CellRegistry>,
    editors: Query<&CellEditor>,
    cells: Query<(Entity, &Cell), Without<RemotelyDeleted>>,
    mut pending: ResMut<PendingCellRegistrations>,
) {
    let Some(cmds) = cmds else { return };

    for event in events.read() {
        if let ConnectionEvent::AttachedKernel(info) = event {
            info!("Attached to kernel {}, syncing cells...", info.name);

            // Build list of known cell versions with actual tracked versions
            let versions: Vec<CellVersion> = registry
                .local_to_remote
                .iter()
                .filter_map(|(entity, remote_id)| {
                    editors.get(*entity).ok().map(|editor| CellVersion {
                        cell_id: remote_id.clone(),
                        version: editor.version(),
                    })
                })
                .collect();

            cmds.send(ConnectionCommand::SyncCells { versions });

            // Send CreateCell for any local cells that aren't registered yet
            for (entity, cell) in cells.iter() {
                if registry.get_remote(entity).is_none() {
                    // Add to pending queue
                    pending.0.push_back(entity);

                    cmds.send(ConnectionCommand::CreateCell {
                        kind: local_kind_to_remote(cell.kind),
                        language: cell.language.clone(),
                        parent_id: cell.parent.as_ref().map(|p| p.0.clone()),
                    });

                    info!("Sending deferred CreateCell for entity {:?}", entity);
                }
            }
        }
    }
}

/// System: Handle cell sync results from server.
pub fn handle_cell_sync_result(
    mut commands: Commands,
    mut events: MessageReader<ConnectionEvent>,
    mut registry: ResMut<CellRegistry>,
    mut editors: Query<&mut CellEditor>,
    cmds: Option<Res<ConnectionCommands>>,
    cells: Query<&CellPosition, (With<Cell>, Without<PromptCell>)>,
    mut recently_deleted: ResMut<RecentlyDeletedByServer>,
    mut pending: ResMut<PendingCellRegistrations>,
) {
    // Clear the recently deleted set at the start of each frame
    recently_deleted.0.clear();
    for event in events.read() {
        match event {
            ConnectionEvent::CellSyncResult { patches, new_cells } => {
                info!(
                    "Cell sync: {} patches, {} new cells",
                    patches.len(),
                    new_cells.len()
                );

                // Handle patches - binary CRDT ops require server-side resolution
                // For now, request full state for cells that have patches
                for patch in patches {
                    if let Some(entity) = registry.get_local(&patch.cell_id) {
                        if let Ok(mut editor) = editors.get_mut(entity) {
                            // Binary CRDT patches require diamond-types integration
                            // For now, update version and request full state if patch has ops
                            if !patch.ops.is_empty() {
                                warn!(
                                    "Received binary patch for cell {} (v{} -> v{}), ops not yet supported - requesting full state",
                                    patch.cell_id, patch.from_version, patch.to_version
                                );
                                // Request full state from server
                                if let Some(ref cmds) = cmds {
                                    cmds.send(ConnectionCommand::GetCell {
                                        cell_id: patch.cell_id.clone(),
                                    });
                                }
                            }
                            // Note: We don't update version here as it's managed by the document
                        }
                    }
                }

                // Spawn new cells with proper row positioning
                // Track row offset for cells spawned in same frame (query doesn't see them yet)
                let mut row_offset = 0u32;
                for cell_state in new_cells {
                    spawn_remote_cell(&mut commands, &mut registry, cell_state, &cells, row_offset);
                    row_offset += 1;
                }
            }

            ConnectionEvent::CellCreated(cell_state) => {
                // Check if we have a local entity waiting for registration
                if let Some(local_entity) = pending.0.pop_front() {
                    // Register the local entity with the server-assigned ID
                    registry.register(cell_state.info.id.clone(), local_entity);

                    // Update the editor
                    if let Ok(mut editor) = editors.get_mut(local_entity) {
                        editor.mark_synced();
                    }

                    info!(
                        "Registered local entity {:?} with server cell ID {}",
                        local_entity, cell_state.info.id
                    );
                } else {
                    // No pending entity - this is a remotely-created cell
                    info!("Cell created remotely: {}", cell_state.info.id);
                    spawn_remote_cell(&mut commands, &mut registry, &cell_state, &cells, 0);
                }
            }

            ConnectionEvent::CellState(cell_state) => {
                // Update existing cell or spawn new one
                if let Some(entity) = registry.get_local(&cell_state.info.id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        // Apply server-authoritative content
                        editor.apply_server_content(cell_state.content.clone());
                        editor.mark_synced();
                        info!(
                            "Updated cell {} to version {}",
                            cell_state.info.id, cell_state.version
                        );
                    }
                } else {
                    spawn_remote_cell(&mut commands, &mut registry, cell_state, &cells, 0);
                }
            }

            ConnectionEvent::CellDeleted { cell_id } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    // Track this entity to prevent delete_remote_cell from
                    // sending a redundant delete command back to server
                    recently_deleted.0.insert(entity);
                    commands.entity(entity).insert(RemotelyDeleted);
                    commands.entity(entity).despawn();
                    registry.unregister(entity);
                    info!("Deleted cell {} (from server)", cell_id);
                }
            }

            ConnectionEvent::CellOpApplied {
                cell_id,
                new_version: _,
            } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        // Mark as synced - version is managed by the document
                        editor.mark_synced();
                    }
                }
            }

            ConnectionEvent::CellList(cells) => {
                info!("Received cell list with {} cells", cells.len());
                // This is informational - we use SyncCells for actual sync
            }

            _ => {}
        }
    }
}

/// Spawn a cell entity from remote state.
/// `row_offset` is used when spawning multiple cells in the same frame
/// (since the query won't see cells spawned earlier in the same frame).
fn spawn_remote_cell(
    commands: &mut Commands,
    registry: &mut ResMut<CellRegistry>,
    state: &CellState,
    existing_cells: &Query<&CellPosition, (With<Cell>, Without<PromptCell>)>,
    row_offset: u32,
) {
    // Skip if we already have this cell
    if registry.get_local(&state.info.id).is_some() {
        return;
    }

    // Calculate position based on actual existing cell positions
    // This prevents overlaps when cells are deleted and recreated
    // Note: PromptCell is excluded by Without<PromptCell> filter
    let base_row = existing_cells
        .iter()
        .map(|pos| pos.row)
        .max()
        .map(|max| max.saturating_add(1))
        .unwrap_or(0);

    // Add offset for cells spawned in the same frame
    let next_row = base_row.saturating_add(row_offset);

    let entity = commands
        .spawn((
            Cell {
                id: CellId(state.info.id.clone()),
                kind: remote_kind_to_local(state.info.kind),
                language: state.info.language.clone(),
                parent: state.info.parent_id.as_ref().map(|s| CellId(s.clone())),
            },
            CellEditor::default()
                .with_text(state.content.clone())
                .with_version(state.version),
            CellPosition::new(0, next_row),
            GlyphonText,
            TextAreaConfig {
                left: 20.0,
                top: 120.0,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: 20,
                    top: 120,
                    right: 720,
                    bottom: 520,
                },
                default_color: glyphon::Color::rgb(220, 220, 240),
            },
        ))
        .id();

    registry.register(state.info.id.clone(), entity);
    info!(
        "Spawned remote cell {} at row {}",
        state.info.id, next_row
    );
}

/// System: Send pending block operations to server.
///
/// This runs every frame and sends operations for cells that have pending edits.
/// Uses block-based CRDT operations for efficient delta sync.
pub fn send_block_operations(
    mut cells: Query<(Entity, &Cell, &mut CellEditor), Changed<CellEditor>>,
    registry: Res<CellRegistry>,
    cmds: Option<Res<ConnectionCommands>>,
) {
    let Some(cmds) = cmds else { return };

    for (entity, cell, mut editor) in cells.iter_mut() {
        // Get remote ID
        let Some(remote_id) = registry.get_remote(entity) else {
            // This is a local-only cell, not yet synced
            // Keep pending ops - they'll be sent when we get registered
            // via CellCreated event from the server
            debug!(
                "Cell {} not registered with server, keeping pending ops",
                cell.id.0,
            );
            continue;
        };

        // Take pending block operations from the editor's document
        let pending_ops = editor.take_pending_ops();

        if pending_ops.is_empty() {
            continue;
        }

        // Send each block operation to the server
        for op in pending_ops {
            cmds.send(ConnectionCommand::ApplyBlockOp {
                cell_id: remote_id.to_string(),
                op,
            });
        }

        // Mark as synced
        editor.mark_synced();
    }
}

/// System: Send pending cell operations to server (legacy fallback).
///
/// This is kept as a fallback for cases where block operations aren't available.
#[allow(dead_code)]
pub fn send_cell_operations_legacy(
    mut cells: Query<(Entity, &Cell, &mut CellEditor), Changed<CellEditor>>,
    registry: Res<CellRegistry>,
    cmds: Option<Res<ConnectionCommands>>,
) {
    let Some(cmds) = cmds else { return };

    for (entity, cell, mut editor) in cells.iter_mut() {
        if !editor.dirty {
            continue;
        }

        // Get remote ID
        let Some(remote_id) = registry.get_remote(entity) else {
            debug!(
                "Cell {} not registered with server, keeping pending ops",
                cell.id.0,
            );
            continue;
        };

        // Send full text as a replace operation (legacy fallback)
        let text = editor.text();
        let version = editor.version();

        let op = CellOp {
            cell_id: remote_id.to_string(),
            client_version: version,
            op: CrdtOp::FullState(text.into_bytes()),
        };

        cmds.send(ConnectionCommand::ApplyCellOp { op });
        editor.mark_synced();
    }
}

/// System: Handle incoming block events from the server.
///
/// Applies remote block operations to local cell editors.
pub fn handle_block_events(
    mut events: MessageReader<ConnectionEvent>,
    mut editors: Query<&mut CellEditor>,
    registry: Res<CellRegistry>,
) {
    for event in events.read() {
        match event {
            ConnectionEvent::BlockOpApplied { cell_id, new_version } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        editor.mark_synced();
                        debug!("Block op applied for cell {}, new version {}", cell_id, new_version);
                    }
                }
            }

            ConnectionEvent::BlockInserted {
                cell_id,
                block_id,
                after_id,
                content,
            } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        // Apply remote block insertion
                        if let Err(e) = editor.apply_remote_block_insert(
                            block_id.clone(),
                            after_id.clone(),
                            content.clone(),
                        ) {
                            warn!("Failed to apply remote block insert: {}", e);
                        }
                    }
                }
            }

            ConnectionEvent::BlockDeleted { cell_id, block_id } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        if let Err(e) = editor.apply_remote_block_delete(block_id) {
                            warn!("Failed to apply remote block delete: {}", e);
                        }
                    }
                }
            }

            ConnectionEvent::BlockEdited {
                cell_id,
                block_id,
                pos,
                insert,
                delete,
            } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        if let Err(e) = editor.apply_remote_block_edit(block_id, *pos, insert, *delete) {
                            warn!("Failed to apply remote block edit: {}", e);
                        }
                    }
                }
            }

            ConnectionEvent::BlockCollapsed {
                cell_id,
                block_id,
                collapsed,
            } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        if let Err(e) = editor.apply_remote_block_collapsed(block_id, *collapsed) {
                            warn!("Failed to apply remote block collapsed: {}", e);
                        }
                    }
                }
            }

            ConnectionEvent::BlockMoved {
                cell_id,
                block_id,
                after_id,
            } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        if let Err(e) = editor.apply_remote_block_move(block_id, after_id.as_ref()) {
                            warn!("Failed to apply remote block move: {}", e);
                        }
                    }
                }
            }

            ConnectionEvent::BlockCellState {
                cell_id,
                blocks,
                version,
            } => {
                if let Some(entity) = registry.get_local(cell_id) {
                    if let Ok(mut editor) = editors.get_mut(entity) {
                        // Replace editor content with server state
                        editor.apply_server_block_state(blocks.clone(), *version);
                        editor.mark_synced();
                        info!("Applied block cell state for cell {}, version {}", cell_id, version);
                    }
                }
            }

            _ => {}
        }
    }
}

/// System: Create a cell on the server when spawning locally.
/// Only sends CreateCell if we're attached to a kernel. Otherwise,
/// trigger_sync_on_attach will handle it when we attach.
pub fn create_remote_cell(
    new_cells: Query<(Entity, &Cell, &CellEditor), Added<Cell>>,
    registry: Res<CellRegistry>,
    mut pending: ResMut<PendingCellRegistrations>,
    conn_state: Res<crate::connection::ConnectionState>,
    cmds: Option<Res<ConnectionCommands>>,
) {
    let Some(cmds) = cmds else { return };

    for (entity, cell, _editor) in new_cells.iter() {
        // Skip if already registered (came from server)
        if registry.get_remote(entity).is_some() {
            continue;
        }

        // Only send CreateCell if we're attached to a kernel
        // Otherwise, trigger_sync_on_attach will send it when we attach
        if conn_state.current_kernel.is_some() {
            // Add to pending queue so we can match CellCreated response
            pending.0.push_back(entity);

            // Create on server
            cmds.send(ConnectionCommand::CreateCell {
                kind: local_kind_to_remote(cell.kind),
                language: cell.language.clone(),
                parent_id: cell.parent.as_ref().map(|p| p.0.clone()),
            });

            info!("Requested creation of cell on server, entity {:?} queued for registration", entity);
        } else {
            debug!("Cell {:?} created before kernel attached, will register on attach", entity);
        }
    }
}

/// System: Delete cell on server when despawning locally.
pub fn delete_remote_cell(
    mut removed: RemovedComponents<Cell>,
    registry: Res<CellRegistry>,
    recently_deleted: Res<RecentlyDeletedByServer>,
    cmds: Option<Res<ConnectionCommands>>,
) {
    let Some(cmds) = cmds else { return };

    for entity in removed.read() {
        // Skip if this cell was deleted by the server (avoid feedback loop)
        if recently_deleted.0.contains(&entity) {
            debug!("Skipping delete for entity {:?} (deleted by server)", entity);
            continue;
        }

        if let Some(remote_id) = registry.get_remote(entity) {
            cmds.send(ConnectionCommand::DeleteCell {
                cell_id: remote_id.to_string(),
            });
            info!("Requested deletion of cell {} on server", remote_id);
        }
    }
}

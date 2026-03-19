//! Document sync — server events → DocumentCache → MainCell.
//!
//! These systems handle the data flow from server block events through
//! the DocumentCache to the MainCell's CellEditor for rendering.

use bevy::prelude::*;
use kaijutsu_types::KernelId;

use crate::cell::{
    CachedDocument, CellEditor, ConversationScrollState, EditorEntities, LayoutGeneration,
    MainCell, ViewingConversation,
};
use crate::connection::{RpcResultMessage, ServerEventMessage};
use kaijutsu_client::ServerEvent;

/// Handle block events from the server, routing through DocumentCache.
///
/// Processes `ServerEventMessage` (streamed block events) and
/// `RpcResultMessage::ContextJoined` (initial document state).
///
/// Multi-context routing: all events go by context_id to the appropriate
/// CachedDocument. sync_main_cell_to_conversation reads the active entry.
pub fn handle_block_events(
    mut server_events: MessageReader<ServerEventMessage>,
    mut result_events: MessageReader<RpcResultMessage>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
    layout_gen: Res<LayoutGeneration>,
    mut pending_switch: ResMut<crate::cell::PendingContextSwitch>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
    mut sync_gen: ResMut<crate::connection::actor_plugin::SyncGeneration>,
    session_agent: Res<crate::cell::SessionAgent>,
    actor: Option<Res<crate::connection::RpcActor>>,
    channel: Res<crate::connection::RpcResultChannel>,
) {
    use kaijutsu_client::ServerEvent;

    let was_at_bottom = scroll_state.is_at_bottom();
    let agent_id = session_agent.0;

    // Handle initial document state from ContextJoined
    for result in result_events.read() {
        match result {
            RpcResultMessage::ContextJoined {
                membership,
                initial_sync,
            } => {
                let ctx_id = membership.context_id;

                if !doc_cache.contains(ctx_id) {
                    let mut synced = kaijutsu_client::SyncedDocument::new(ctx_id, agent_id);

                    if let Some(state) = initial_sync {
                        match synced.apply_sync_state(state) {
                            Ok(effect) => {
                                info!("Cache: initial sync for {} effect: {:?}", ctx_id, effect);
                            }
                            Err(e) => {
                                error!("Cache: initial sync error for {}: {}", ctx_id, e);
                            }
                        }
                    }

                    let cached = CachedDocument {
                        synced,
                        input: None,
                        input_pending_clear: false,
                        context_name: membership.context_id.short(),
                        synced_at_generation: sync_gen.0,
                        last_accessed: std::time::Instant::now(),
                        scroll_offset: 0.0,
                    };
                    doc_cache.insert(ctx_id, cached);
                } else if let Some(state) = initial_sync
                    && let Some(cached) = doc_cache.get_mut(ctx_id)
                {
                    match cached.synced.apply_sync_state(state) {
                        Ok(effect) => {
                            info!(
                                "Cache: reconnect refresh for {} effect: {:?}",
                                ctx_id, effect
                            );
                            cached.synced_at_generation = sync_gen.0;
                        }
                        Err(e) => {
                            error!("Cache: reconnect refresh error for {}: {}", ctx_id, e);
                        }
                    }
                }

                // Fetch input document state for the joined context
                if let Some(ref actor) = actor {
                    let handle = actor.handle.clone();
                    let tx = channel.sender();
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            match handle.get_input_state(ctx_id).await {
                                Ok(state) => {
                                    let _ = tx.send(RpcResultMessage::InputStateReceived {
                                        context_id: ctx_id,
                                        state,
                                    });
                                }
                                Err(e) => {
                                    log::warn!("get_input_state failed for {}: {}", ctx_id, e);
                                    let _ = tx.send(RpcResultMessage::InputStateReceived {
                                        context_id: ctx_id,
                                        state: kaijutsu_client::InputState {
                                            content: String::new(),
                                            ops: Vec::new(),
                                            version: 0,
                                        },
                                    });
                                }
                            }
                        })
                        .detach();
                }

                if doc_cache.active_id().is_none() {
                    doc_cache.set_active(ctx_id);
                }

                if pending_switch.0 == Some(ctx_id) {
                    info!(
                        "Pending context switch satisfied: {} joined, auto-switching",
                        ctx_id
                    );
                    pending_switch.0 = None;
                    switch_writer.write(crate::cell::ContextSwitchRequested { context_id: ctx_id });
                }
            }
            RpcResultMessage::InputStateReceived { context_id, state } => {
                let ctx_id = *context_id;
                if let Some(cached) = doc_cache.get_mut(ctx_id)
                    && cached.input.is_none()
                {
                    if state.ops.is_empty() {
                        cached.input = Some(kaijutsu_client::SyncedInput::new(ctx_id, agent_id));
                        info!("Initialized empty SyncedInput for {}", ctx_id);
                    } else {
                        match kaijutsu_client::SyncedInput::from_state(ctx_id, agent_id, &state.ops)
                        {
                            Ok(input) => {
                                info!(
                                    "Initialized SyncedInput for {} (text='{}')",
                                    ctx_id, state.content
                                );
                                cached.input = Some(input);
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to create SyncedInput from state for {}: {}",
                                    ctx_id, e
                                );
                                cached.input =
                                    Some(kaijutsu_client::SyncedInput::new(ctx_id, agent_id));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Handle streamed block events
    for ServerEventMessage(event) in server_events.read() {
        let event_ctx_id = match event {
            ServerEvent::BlockInserted { context_id, .. }
            | ServerEvent::BlockTextOps { context_id, .. }
            | ServerEvent::BlockStatusChanged { context_id, .. }
            | ServerEvent::BlockDeleted { context_id, .. }
            | ServerEvent::BlockCollapsedChanged { context_id, .. }
            | ServerEvent::BlockMoved { context_id, .. }
            | ServerEvent::SyncReset { context_id, .. } => Some(*context_id),
            _ => None,
        };

        if let Some(ctx_id) = event_ctx_id
            && let Some(cached) = doc_cache.get_mut(ctx_id)
        {
            let effect = cached.synced.apply_event(event);
            match &effect {
                kaijutsu_client::SyncEffect::Updated { .. }
                | kaijutsu_client::SyncEffect::FullSync { .. } => {
                    cached.synced_at_generation = sync_gen.0;
                }
                kaijutsu_client::SyncEffect::NeedsResync => {
                    cached.synced_at_generation = 0;
                    sync_gen.0 = sync_gen.0.wrapping_add(1);
                }
                kaijutsu_client::SyncEffect::Ignored => {}
            }
            trace!("Cache: event for {}: {:?}", ctx_id, effect);
        }
    }

    if was_at_bottom
        && layout_gen.0 > scroll_state.last_content_gen
        && !scroll_state.user_scrolled_this_frame
    {
        scroll_state.start_following();
        scroll_state.last_content_gen = layout_gen.0;
    }
}

/// Handle input document events (InputTextOps, InputCleared).
///
/// After submit or escape×3, `input_pending_clear` is set on the
/// CachedDocument. While set, TextOps are suppressed (they may carry
/// stale inserts from before the server cleared). When the server
/// confirms via InputCleared, the flag is cleared and a fresh input
/// state is re-fetched to restore SyncedInput with clean CRDT history.
pub fn handle_input_doc_events(
    mut server_events: MessageReader<ServerEventMessage>,
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
    mut overlay: Query<&mut crate::cell::InputOverlay>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut focus: ResMut<crate::input::focus::FocusArea>,
    session_agent: Res<crate::cell::SessionAgent>,
    actor: Option<Res<crate::connection::RpcActor>>,
    channel: Res<crate::connection::RpcResultChannel>,
) {
    use kaijutsu_client::ServerEvent;

    for ServerEventMessage(event) in server_events.read() {
        match event {
            ServerEvent::InputTextOps { context_id, ops } => {
                if let Some(cached) = doc_cache.get_mut(*context_id) {
                    // Suppress late TextOps during pending clear — they carry
                    // stale inserts from before the server's clear_input.
                    if cached.input_pending_clear {
                        trace!("Suppressed InputTextOps for {} (pending clear)", context_id);
                        continue;
                    }
                    if let Some(input) = &mut cached.input
                        && let Err(e) = input.apply_remote_ops(ops)
                    {
                        warn!(
                            "Failed to apply remote input ops for {}: {}, dropping input for re-sync",
                            context_id, e
                        );
                        cached.input = None;
                    }
                }
            }
            ServerEvent::InputCleared { context_id } => {
                let ctx_id = *context_id;

                // Clear the pending flag and drop the stale SyncedInput.
                // Re-fetch from server to get clean CRDT history.
                if let Some(cached) = doc_cache.get_mut(ctx_id) {
                    cached.input_pending_clear = false;
                    cached.input = None;
                }

                // Re-fetch input state — server's doc is now clean post-clear.
                // InputStateReceived handler will recreate SyncedInput.
                let agent_id = session_agent.0;
                if let Some(ref actor) = actor {
                    let handle = actor.handle.clone();
                    let tx = channel.sender();
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            match handle.get_input_state(ctx_id).await {
                                Ok(state) => {
                                    let _ = tx.send(RpcResultMessage::InputStateReceived {
                                        context_id: ctx_id,
                                        state,
                                    });
                                }
                                Err(e) => {
                                    log::warn!(
                                        "get_input_state re-fetch after clear failed for {}: {}",
                                        ctx_id,
                                        e
                                    );
                                    let _ = tx.send(RpcResultMessage::InputStateReceived {
                                        context_id: ctx_id,
                                        state: kaijutsu_client::InputState {
                                            content: String::new(),
                                            ops: Vec::new(),
                                            version: 0,
                                        },
                                    });
                                }
                            }
                        })
                        .detach();
                } else {
                    // No actor — create empty SyncedInput directly
                    if let Some(cached) = doc_cache.get_mut(ctx_id) {
                        cached.input = Some(kaijutsu_client::SyncedInput::new(ctx_id, agent_id));
                    }
                }

                if doc_cache.active_id() == Some(ctx_id) {
                    // Overlay may already be cleared by optimistic local clear.
                    if let Ok(mut overlay) = overlay.single_mut() {
                        overlay.text.clear();
                        overlay.cursor = 0;
                        overlay.selection_anchor = None;
                    }
                    if matches!(*focus, crate::input::focus::FocusArea::Compose) {
                        *focus = crate::input::focus::FocusArea::Conversation;
                    }
                    scroll_state.start_following();
                }
            }
            _ => {}
        }
    }
}

/// Sync the MainCell's content with the active document in DocumentCache.
pub fn sync_main_cell_to_conversation(
    doc_cache: Res<crate::cell::DocumentCache>,
    entities: Res<EditorEntities>,
    mut main_cell: Query<(&mut CellEditor, Option<&mut ViewingConversation>), With<MainCell>>,
    mut commands: Commands,
) {
    let Some(active_id) = doc_cache.active_id() else {
        return;
    };
    let Some(entity) = entities.main_cell else {
        return;
    };
    let Some(cached) = doc_cache.get(active_id) else {
        return;
    };

    let ctx_id = cached.synced.context_id();
    let sync_version = cached.synced.version();

    let Ok((mut editor, viewing_opt)) = main_cell.get_mut(entity) else {
        return;
    };

    let needs_sync = match viewing_opt {
        Some(ref viewing) => {
            viewing.conversation_id != ctx_id || viewing.last_sync_version != sync_version
        }
        None => true,
    };

    if !needs_sync {
        return;
    }

    let agent_id = editor.store.agent_id();
    let store_snap = cached.synced.snapshot();
    editor.store = kaijutsu_crdt::BlockStore::from_snapshot(store_snap, agent_id);
    editor.store.set_version(sync_version);

    if let Some(last_block) = editor.blocks().last() {
        let len = last_block.content.len();
        editor.cursor = crate::cell::BlockCursor::at(last_block.id, len);
    }

    match viewing_opt {
        Some(mut viewing) => {
            viewing.conversation_id = ctx_id;
            viewing.last_sync_version = sync_version;
        }
        None => {
            commands.entity(entity).insert(ViewingConversation {
                conversation_id: ctx_id,
                last_sync_version: sync_version,
            });
        }
    }

    trace!(
        "Synced MainCell to conversation {} (version {})",
        ctx_id, sync_version
    );
}

/// Handle context switch requests.
pub fn handle_context_switch(
    mut switch_events: MessageReader<crate::cell::ContextSwitchRequested>,
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut pending_switch: ResMut<crate::cell::PendingContextSwitch>,
    bootstrap: Res<crate::connection::BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
) {
    for event in switch_events.read() {
        let ctx_id = event.context_id;

        if !doc_cache.contains(ctx_id) {
            if pending_switch.0 == Some(ctx_id) {
                continue;
            }

            info!(
                "Context switch: cache miss for {}, spawning actor to join",
                ctx_id
            );
            pending_switch.0 = Some(ctx_id);

            let kernel_id = conn_state.kernel_id.unwrap_or_else(KernelId::nil);

            let instance = uuid::Uuid::new_v4().to_string();
            let _ = bootstrap
                .tx
                .send(crate::connection::BootstrapCommand::SpawnActor {
                    config: conn_state.ssh_config.clone(),
                    kernel_id,
                    context_id: Some(ctx_id),
                    instance,
                });
            continue;
        }

        if doc_cache.active_id() == Some(ctx_id) {
            continue;
        }

        if let Some(active_id) = doc_cache.active_id()
            && let Some(cached) = doc_cache.get_mut(active_id)
        {
            cached.scroll_offset = scroll_state.offset;
        }

        doc_cache.set_active(ctx_id);

        if let Some(cached) = doc_cache.get(ctx_id) {
            scroll_state.offset = cached.scroll_offset;
            scroll_state.target_offset = cached.scroll_offset;
            scroll_state.following = false;
            info!(
                "Context switch complete: {} (scroll: {:.0})",
                ctx_id, cached.scroll_offset
            );
        }
    }
}

/// Handle server-pushed context switches (fork, kj context switch).
///
/// Converts `ServerEvent::ContextSwitched` into `ContextSwitchRequested`,
/// which is handled by `handle_context_switch` above.
pub fn handle_server_context_switch(
    mut server_events: MessageReader<ServerEventMessage>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
) {
    for ServerEventMessage(event) in server_events.read() {
        if let ServerEvent::ContextSwitched { context_id } = event {
            info!("Server context switch → {}", context_id);
            switch_writer.write(crate::cell::ContextSwitchRequested {
                context_id: *context_id,
            });
        }
    }
}

/// Check if the active document is stale and trigger re-fetch.
pub fn check_cache_staleness(
    doc_cache: Res<crate::cell::DocumentCache>,
    sync_gen: Res<crate::connection::actor_plugin::SyncGeneration>,
    actor: Option<Res<crate::connection::RpcActor>>,
    mut checked_gen: Local<u64>,
) {
    if sync_gen.0 == *checked_gen {
        return;
    }
    *checked_gen = sync_gen.0;

    let Some(active_id) = doc_cache.active_id() else {
        return;
    };

    let Some(cached) = doc_cache.get(active_id) else {
        return;
    };

    if cached.synced_at_generation < sync_gen.0 {
        let Some(ref actor) = actor else {
            return;
        };

        info!(
            "Staleness detected: active doc {} synced_at={} < current={}",
            active_id, cached.synced_at_generation, sync_gen.0
        );

        let handle = actor.handle.clone();
        let ctx_id = active_id;

        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                match handle.get_context_sync(ctx_id).await {
                    Ok(sync) => {
                        info!(
                            "Staleness re-fetch complete for {}: {} bytes oplog",
                            ctx_id,
                            sync.ops.len()
                        );
                    }
                    Err(e) => {
                        warn!("Staleness re-fetch failed for {}: {}", ctx_id, e);
                    }
                }
            })
            .detach();
    }
}

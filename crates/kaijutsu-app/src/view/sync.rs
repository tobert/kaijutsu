//! Document sync — server events → DocumentCache → MainCell.
//!
//! These systems handle the data flow from server block events through
//! the DocumentCache to the MainCell's CellEditor for rendering.

use bevy::prelude::*;

use crate::cell::{
    CachedDocument, CellEditor, ConversationScrollState, EditorEntities, LayoutGeneration,
    MainCell, ViewingConversation,
};
use crate::connection::{RpcResultMessage, ServerEventMessage};
use crate::ui::screen::Screen;
use kaijutsu_client::ServerEvent;

/// The `Screen` a *landed* context switch should reveal, or `None` if the
/// current screen already shows the active context.
///
/// A context's conversation lives on [`Screen::Conversation`]. Any full-viewport
/// screen that hides it — the room (which owns the time well as furniture) and
/// the editor — must yield to Conversation when a context switch lands, so the
/// user actually sees the context they (or the kernel) switched to. This is the
/// *general* fix for the switch-doesn't-drive-Screen gap: it keys on the screen
/// being left, not on which writer requested the switch, so every switch path
/// (peer `switch_context`, server-pushed `ContextSwitched` from fork / `kj
/// context switch`, the dock, …) reveals the context uniformly. The editor-open
/// signal is the mirror of this: it drives `Screen::Editor` from its *own*
/// landing handler, not through here.
fn screen_revealing_switched_context(current: Screen) -> Option<Screen> {
    match current {
        Screen::Conversation => None,
        // Room (well or not) and Editor hide the conversation; reveal it.
        _ => Some(Screen::Conversation),
    }
}

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
    session_principal: Res<crate::cell::SessionPrincipal>,
    actor: Option<Res<crate::connection::RpcActor>>,
    channel: Res<crate::connection::RpcResultChannel>,
) {
    use kaijutsu_client::ServerEvent;

    let was_at_bottom = scroll_state.is_at_bottom();
    let principal_id = session_principal.0;

    // Handle initial document state from ContextJoined
    for result in result_events.read() {
        match result {
            RpcResultMessage::ContextJoined {
                membership,
                initial_sync,
            } => {
                let ctx_id = membership.context_id;

                match initial_sync {
                    // The store creates-or-refreshes the doc and marks it synced.
                    Some(state) => {
                        match doc_cache.apply_sync(ctx_id, state, principal_id, || {
                            membership.context_id.short()
                        }) {
                            Ok(created) => info!(
                                "Cache: {} for {}",
                                if created { "initial sync" } else { "reconnect refresh" },
                                ctx_id
                            ),
                            Err(e) => error!("Cache: sync error for {}: {}", ctx_id, e),
                        }
                    }
                    // The initial fetch failed — start an empty doc so the view
                    // has something to render; the staleness path fills it later.
                    None => {
                        if !doc_cache.contains(ctx_id) {
                            let synced = kaijutsu_client::SyncedDocument::new(ctx_id, principal_id);
                            let generation = doc_cache.generation();
                            doc_cache.insert(
                                ctx_id,
                                CachedDocument::new(synced, membership.context_id.short(), generation),
                            );
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
                        cached.input = Some(kaijutsu_client::SyncedInput::new(ctx_id, principal_id));
                        info!("Initialized empty SyncedInput for {}", ctx_id);
                    } else {
                        match kaijutsu_client::SyncedInput::from_state(ctx_id, principal_id, &state.ops)
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
                                    Some(kaijutsu_client::SyncedInput::new(ctx_id, principal_id));
                            }
                        }
                    }
                }
            }
            RpcResultMessage::ContextResynced { context_id, sync } => {
                // A staleness re-fetch (post-reconnect / post-lag) delivered the
                // full CRDT state. Merge it via the store — the idempotent catch-up
                // that heals blocks lost while the transport was down. Only an
                // already-cached doc is refreshed; an unknown/evicted context is
                // not resurrected here (the join bootstrap hydrates fresh ones).
                let ctx_id = *context_id;
                if doc_cache.contains(ctx_id) {
                    match doc_cache.apply_sync(ctx_id, sync, principal_id, String::new) {
                        Ok(_) => info!("Cache: staleness re-sync for {}", ctx_id),
                        Err(e) => error!("Cache: staleness re-sync error for {}: {}", ctx_id, e),
                    }
                }
            }
            _ => {}
        }
    }

    // Handle streamed block events
    for ServerEventMessage(event) in server_events.read() {
        // An actor-delivered post-reconnect resync carries the full CRDT state,
        // not a streamed delta — merge it via the store (idempotent), which marks
        // the doc fresh so check_cache_staleness won't re-fetch it. The eager
        // catch-up for the joined context; non-joined docs re-sync lazily off the
        // generation bump (ServerEvent::Reconnected).
        if let ServerEvent::ContextResynced { sync } = event {
            let ctx_id = sync.context_id;
            if doc_cache.contains(ctx_id) {
                match doc_cache.apply_sync(ctx_id, sync, principal_id, String::new) {
                    Ok(_) => info!("Cache: actor re-sync for {}", ctx_id),
                    Err(e) => error!("Cache: actor re-sync error for {}: {}", ctx_id, e),
                }
            }
            continue;
        }

        // The store routes the event to its context's doc and keeps the
        // generation/staleness in step (a NeedsResync bumps internally).
        doc_cache.apply_server_event(event);
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
    mut overlay: Query<&mut crate::cell::InputOverlay, With<crate::cell::InputOverlayMarker>>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut focus: ResMut<crate::input::focus::FocusArea>,
    session_principal: Res<crate::cell::SessionPrincipal>,
    actor: Option<Res<crate::connection::RpcActor>>,
    channel: Res<crate::connection::RpcResultChannel>,
) {
    use kaijutsu_client::ServerEvent;

    for ServerEventMessage(event) in server_events.read() {
        match event {
            ServerEvent::InputTextOps { context_id, ops, .. } => {
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
                let principal_id = session_principal.0;
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
                        cached.input = Some(kaijutsu_client::SyncedInput::new(ctx_id, principal_id));
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

    let principal_id = editor.store.principal_id();
    let store_snap = cached.synced.snapshot();
    editor.store = match kaijutsu_crdt::BlockStore::from_snapshot(store_snap, principal_id) {
        Ok(store) => store,
        Err(e) => {
            tracing::error!("Failed to restore snapshot for sync: {e}");
            return;
        }
    };
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
    mut scroll_offsets: ResMut<crate::cell::ScrollOffsets>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut pending_switch: ResMut<crate::cell::PendingContextSwitch>,
    bootstrap: Res<crate::connection::BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
    screen: Res<State<Screen>>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    for event in switch_events.read() {
        let ctx_id = event.context_id;

        // A nil ContextId reaching this system means something upstream
        // (kernel, RPC, or subscription layer) leaked a sentinel. Reject
        // it loudly — the cache-miss branch below would otherwise spawn
        // an actor to join nil and produce a useless round-trip failure.
        if ctx_id.is_nil() {
            warn!("handle_context_switch: refusing to switch to nil ContextId");
            continue;
        }

        if !doc_cache.contains(ctx_id) {
            if pending_switch.0 == Some(ctx_id) {
                continue;
            }

            // Cache-miss requires a real KernelId to spawn an actor against.
            // If there's no attached kernel, skip with a warning rather than
            // falling back to KernelId::nil — the spawn would fail downstream
            // anyway and the sentinel would leak into the BootstrapChannel.
            let Some(kernel_id) = conn_state.kernel_id else {
                warn!(
                    "handle_context_switch: cache miss for {} but no kernel attached; skipping",
                    ctx_id
                );
                continue;
            };

            info!(
                "Context switch: cache miss for {}, spawning actor to join",
                ctx_id
            );
            pending_switch.0 = Some(ctx_id);

            let instance = uuid::Uuid::new_v4().to_string();
            let _ = bootstrap
                .tx
                .send(crate::connection::BootstrapCommand::SpawnActor {
                    config: conn_state.ssh_config.clone(),
                    kernel_id: Some(kernel_id),
                    context_id: Some(ctx_id),
                    instance,
                });
            continue;
        }

        if doc_cache.active_id() == Some(ctx_id) {
            continue;
        }

        // Save the outgoing context's scroll position (view state).
        if let Some(active_id) = doc_cache.active_id() {
            scroll_offsets.0.insert(active_id, scroll_state.offset);
        }

        doc_cache.set_active(ctx_id);

        // A landed switch is an intent to *view* this context. If a
        // full-viewport screen is hiding the conversation (time well today,
        // editor later), yield to it so the switch is actually visible. Keying
        // on the screen being left — not the switch's source — is what makes
        // this general: every writer that funnels here reveals uniformly.
        if let Some(target) = screen_revealing_switched_context(*screen.get()) {
            info!("Context switch revealing {:?} from {:?}", target, screen.get());
            next_screen.set(target);
        }

        // Restore the incoming context's saved scroll (default: top).
        if doc_cache.contains(ctx_id) {
            let offset = scroll_offsets.0.get(&ctx_id).copied().unwrap_or(0.0);
            scroll_state.offset = offset;
            scroll_state.target_offset = offset;
            scroll_state.following = false;
            info!("Context switch complete: {} (scroll: {:.0})", ctx_id, offset);
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

/// Re-fetch the active document when the store marks it stale (after a
/// generation bump from reconnect or broadcast lag). The store owns the
/// staleness decision; this system only performs the IO and routes the result
/// back through `ContextResynced` for the store to merge.
pub fn check_cache_staleness(
    doc_cache: Res<crate::cell::DocumentCache>,
    actor: Option<Res<crate::connection::RpcActor>>,
    channel: Res<crate::connection::RpcResultChannel>,
    mut checked_gen: Local<u64>,
) {
    let generation = doc_cache.generation();
    if generation == *checked_gen {
        return;
    }
    *checked_gen = generation;

    let Some(ctx_id) = doc_cache.stale_active() else {
        return;
    };
    let Some(ref actor) = actor else {
        return;
    };

    info!("Staleness detected: active doc {} behind generation {}", ctx_id, generation);

    let handle = actor.handle.clone();
    let tx = channel.sender();

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle.get_context_sync(ctx_id).await {
                Ok(sync) => {
                    info!(
                        "Staleness re-fetch complete for {}: {} bytes oplog — applying",
                        ctx_id,
                        sync.ops.len()
                    );
                    // Route the fetched state back to the main thread; the store
                    // merges it (and marks the doc fresh).
                    let _ = tx.send(RpcResultMessage::ContextResynced {
                        context_id: ctx_id,
                        sync,
                    });
                }
                Err(e) => {
                    warn!("Staleness re-fetch failed for {}: {}", ctx_id, e);
                }
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Already on Conversation → no transition (don't churn the FSM on every
    /// in-conversation switch, e.g. dock clicks).
    #[test]
    fn switching_while_on_conversation_does_not_transition() {
        assert_eq!(
            screen_revealing_switched_context(Screen::Conversation),
            None
        );
    }

    /// The bug this fixes: a switch landing while the room (which owns the
    /// time well as furniture, zoomed in or not — `Screen::TimeWell` retired
    /// in Slice D, `lovely-swimming-prism.md`) owns the viewport must reveal
    /// the conversation, not leave the user staring at the room. Covers both
    /// the peer `switch_context` action and the server-pushed
    /// `ContextSwitched` (fork / `kj context switch`) — both funnel through
    /// `handle_context_switch`, which is where this decision is applied.
    #[test]
    fn switching_while_in_room_reveals_conversation() {
        assert_eq!(
            screen_revealing_switched_context(Screen::Room),
            Some(Screen::Conversation)
        );
    }
}

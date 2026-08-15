//! Document sync — the context change feed → DocumentCache → MainCell.
//!
//! docs/change-feed.md is normative here. The block document is a
//! `ContextMirror` (crates/kaijutsu-client/src/context_feed.rs) fed by a
//! per-context change feed, not a CRDT fed by `ServerEvent::BlockTextOps`.
//! `handle_block_events` still owns `ContextJoined`/input-state bootstrap;
//! `drain_context_feeds` is the new steady-state and recovery driver,
//! replacing the old `check_cache_staleness` poll.

use bevy::prelude::*;

use crate::cell::{
    CellEditor, ConversationScrollState, EditorEntities, LayoutGeneration, MainCell,
    ViewingConversation,
};
use crate::connection::{RpcResultMessage, ServerEventMessage};
use crate::ui::screen::Screen;
use kaijutsu_client::ServerEvent;
use kaijutsu_types::ContextId;

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

/// Handle context-join bookkeeping: membership (`RpcResultMessage::
/// ContextJoined`) and compose-input hydration (`InputStateReceived`).
///
/// The block document itself is no longer hydrated here. A `ContextJoined`
/// carries no CRDT state to apply any more — `drain_context_hydrations`
/// (below) installs the mirror once the background subscribe+`getBlocks`
/// finishes, on its own channel (a live `mpsc::Receiver<FeedEvent>` can't
/// ride `RpcResultMessage`: `MessageReader` hands out shared references, and
/// a receiver can't be moved out of one — see `ContextHydrationChannel`).
/// This function only reacts to the join *event* — set active, satisfy a
/// pending switch, kick off the input-doc fetch — plus the scroll-follow
/// bookkeeping that used to piggyback on the streamed-block loop this
/// function no longer has (streamed block/text changes ride the per-context
/// change feed now; see `drain_context_feeds`).
pub fn handle_block_events(
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
    let was_at_bottom = scroll_state.is_at_bottom();
    let principal_id = session_principal.0;

    for result in result_events.read() {
        match result {
            RpcResultMessage::ContextJoined { membership } => {
                let ctx_id = membership.context_id;

                // Fetch input document state for the joined context.
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
            _ => {}
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

/// Drain the change-feed hydration channel (`ContextHydrationChannel`) and
/// install the results into `DocumentCache`. Separate from `handle_block_events`
/// because a hydration result carries a live `mpsc::Receiver<FeedEvent>`,
/// which cannot travel through the `RpcResultMessage`/Bevy-Message path (see
/// that type's doc comment).
pub fn drain_context_hydrations(
    channel: Res<crate::connection::ContextHydrationChannel>,
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
) {
    use crate::connection::ContextHydration;
    for item in channel.drain() {
        match item {
            ContextHydration::Joined {
                context_id,
                mirror,
                feed,
            } => {
                info!("Cache: hydrated {} (version {})", context_id, mirror.version());
                doc_cache.install(context_id, mirror, Some(feed), || context_id.short());
            }
            ContextHydration::Snapshot {
                context_id,
                blocks,
                version,
            } => match doc_cache.apply_snapshot(context_id, blocks, version) {
                Ok(()) => info!("Cache: re-hydrated snapshot for {} (version {})", context_id, version),
                Err(e) => error!("Cache: snapshot apply error for {}: {}", context_id, e),
            },
            ContextHydration::JoinFailed { context_id } => {
                // The initial subscribe+hydrate failed — start an empty,
                // unfed mirror so the view has something to render. Nothing
                // will fill it further until the user switches away and back
                // (respawning the actor); there is no poll-driven staleness
                // fallback any more, the feed itself is the recovery path.
                if !doc_cache.contains(context_id) {
                    doc_cache.install(
                        context_id,
                        kaijutsu_client::ContextMirror::new(context_id),
                        None,
                        || context_id.short(),
                    );
                }
            }
        }
    }
}

/// Drain every followed context's change feed once per frame, applying
/// deliveries to their mirrors (`DocumentStore::drain_feeds`) and reacting to
/// `Resubscribed`/`Terminated`/`Desynced` by re-hydrating in the background
/// (docs/change-feed.md rules 21-28). This is the change feed's replacement
/// for the old poll-driven staleness sweep (`check_cache_staleness`, deleted)
/// — recovery is now driven by the feed itself, per context, instead of a
/// coarse generation bump on `ServerEvent::Reconnected`.
pub fn drain_context_feeds(
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
    actor: Option<Res<crate::connection::RpcActor>>,
    hydration_channel: Res<crate::connection::ContextHydrationChannel>,
) {
    let signals = doc_cache.drain_feeds();
    if signals.is_empty() {
        return;
    }
    let Some(ref actor) = actor else {
        // No actor to re-hydrate with — the signals still applied locally
        // (mirrors already reset by `drain_feeds`); recovery resumes once an
        // actor exists again.
        return;
    };

    for (context_id, signal) in signals {
        match signal {
            kaijutsu_client::FeedSignal::Updated => {}
            kaijutsu_client::FeedSignal::Desynced(e) => {
                error!(
                    "context mirror desynced for {}: {} — re-hydrating from a fresh snapshot",
                    context_id, e
                );
                spawn_snapshot_refetch(&actor.handle, context_id, &hydration_channel);
            }
            kaijutsu_client::FeedSignal::Resubscribed => {
                trace!("context {} resubscribed — re-hydrating from a fresh snapshot", context_id);
                spawn_snapshot_refetch(&actor.handle, context_id, &hydration_channel);
            }
            kaijutsu_client::FeedSignal::Terminated => {
                warn!("context {} feed terminated — re-subscribing and re-hydrating", context_id);
                spawn_full_rehydrate(&actor.handle, context_id, &hydration_channel);
            }
        }
    }
}

/// The `Resubscribed`/`Desynced` recovery: the existing feed receiver is
/// still good, so only a fresh snapshot is needed.
fn spawn_snapshot_refetch(
    handle: &kaijutsu_client::ActorHandle,
    context_id: ContextId,
    hydration_channel: &crate::connection::ContextHydrationChannel,
) {
    let handle = handle.clone();
    let tx = hydration_channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle
                .get_blocks_versioned(context_id, kaijutsu_types::BlockQuery::All)
                .await
            {
                Ok((blocks, version)) => {
                    let _ = tx.send(crate::connection::ContextHydration::Snapshot {
                        context_id,
                        blocks,
                        version,
                    });
                }
                Err(e) => log::warn!("re-hydrate snapshot fetch failed for {}: {}", context_id, e),
            }
        })
        .detach();
}

/// The `Terminated` recovery: the receiver is spent, so subscribe from
/// scratch and hydrate a brand-new mirror over the new receiver.
fn spawn_full_rehydrate(
    handle: &kaijutsu_client::ActorHandle,
    context_id: ContextId,
    hydration_channel: &crate::connection::ContextHydrationChannel,
) {
    let handle = handle.clone();
    let tx = hydration_channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match crate::connection::actor_plugin::hydrate_context(&handle, context_id).await {
                Ok((mirror, feed)) => {
                    let _ = tx.send(crate::connection::ContextHydration::Joined {
                        context_id,
                        mirror,
                        feed,
                    });
                }
                Err(e) => log::warn!("re-subscribe+hydrate failed for {}: {}", context_id, e),
            }
        })
        .detach();
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

    let ctx_id = cached.mirror.context_id();
    let sync_version = cached.mirror.version();

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

    // `CellEditor.store` is a `kaijutsu_crdt::BlockStore` — the local render
    // buffer every other view system already reads (block_border.rs,
    // render.rs, diff_view, timeline, …), unchanged by this migration. The
    // change-feed mirror holds plain `BlockSnapshot`s, not a CRDT, so this
    // materializes them via `insert_from_snapshot` (the same "remote sync /
    // restore" primitive the kernel and MCP use to load snapshots into a
    // fresh store) instead of `BlockStore::from_snapshot`'s CRDT-snapshot
    // round trip. `ContextMirror::blocks()` is already document order, so a
    // plain left-to-right insert reproduces it.
    let principal_id = editor.store.principal_id();
    let mut store = kaijutsu_crdt::BlockStore::new(ctx_id, principal_id);
    let mut after = None;
    let mut ok = true;
    for block in cached.mirror.blocks() {
        match store.insert_from_snapshot(block.clone(), after.as_ref()) {
            Ok(id) => after = Some(id),
            Err(e) => {
                tracing::error!(
                    "Failed to materialize block {} into the render buffer: {e}",
                    block.id
                );
                ok = false;
                break;
            }
        }
    }
    if !ok {
        return;
    }
    store.set_version(sync_version);
    editor.store = store;

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

// `check_cache_staleness` (the old poll-driven CRDT re-fetch) is gone —
// `drain_context_feeds`, above, replaces it. Each followed context now gets
// its own precise recovery signal straight from its change feed instead of a
// coarse generation bump checked once a frame.

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

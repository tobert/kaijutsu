//! Document sync — the context change feed → DocumentCache → MainCell.
//!
//! docs/change-feed.md is normative here. The block document is a
//! `ContextMirror` (crates/kaijutsu-client/src/context_feed.rs) fed by a
//! per-context change feed; the old `ServerEvent::BlockTextOps` wire is gone.
//! `handle_block_events` still owns `ContextJoined`; `drain_context_feeds`
//! is the steady-state and recovery driver, replacing the old
//! `check_cache_staleness` poll. The compose draft (Lane C slice 3) rides
//! this same mirror as an ordinary `Status::Draft` block — there is no more
//! separate input-doc hydration or `InputTextOps`/`InputCleared` handling
//! here; see `kaijutsu_client::DocumentEntry::draft_text`.

use bevy::prelude::*;

use crate::cell::{
    CellEditor, ConversationScrollState, EditorEntities, MainCell, ViewingConversation,
};
use crate::connection::{RpcResultMessage, ServerEventMessage};
use crate::ui::screen::Screen;
use kaijutsu_client::ServerEvent;
use kaijutsu_types::{ContextId, PrincipalId};

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

/// Handle context-join bookkeeping (`RpcResultMessage::ContextJoined`).
///
/// The block document itself is no longer hydrated here. A `ContextJoined`
/// carries no document state to apply any more — `drain_context_hydrations`
/// (below) installs the mirror once the background subscribe+`getBlocks`
/// finishes, on its own channel (a live `mpsc::Receiver<FeedEvent>` can't
/// ride `RpcResultMessage`: `MessageReader` hands out shared references, and
/// a receiver can't be moved out of one — see `ContextHydrationChannel`).
/// This function only reacts to the join *event* — set active, satisfy a
/// pending switch. It used to also re-enable scroll-follow whenever the view
/// was near the bottom and layout advanced (streamed block/text changes ride
/// the per-context change feed now; see `drain_context_feeds`) — that was
/// the sticky-follow yank bug: scrolling up within the 50px
/// `is_at_bottom()` band, pausing one frame, and the next streamed block
/// silently snapped the view back to the bottom.
/// Follow is sticky now — once the user scrolls away from the tail it stays
/// off until they explicitly return (`ScrollToEnd`/`start_following`, or
/// scrolling target back to true bottom, `ConversationScrollState::scroll_by`)
/// — so new content arriving is no longer a re-latch trigger. Compose-input
/// hydration is gone too (Lane C slice 3): the draft is an ordinary block,
/// so it arrives on the same change feed as everything else — no separate
/// fetch needed.
pub fn handle_block_events(
    mut result_events: MessageReader<RpcResultMessage>,
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
    mut pending_switch: ResMut<crate::cell::PendingContextSwitch>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
) {
    for result in result_events.read() {
        match result {
            RpcResultMessage::ContextJoined { membership } => {
                let ctx_id = membership.context_id;

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
            _ => {}
        }
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

/// Sync the MainCell's content with the active document in DocumentCache.
fn needs_render_sync(
    viewing: Option<&ViewingConversation>,
    context_id: ContextId,
    version: u64,
    principal_id: Option<PrincipalId>,
) -> bool {
    match viewing {
        Some(viewing) => {
            viewing.conversation_id != context_id
                || viewing.last_sync_version != version
                || viewing.principal_id != principal_id
        }
        None => true,
    }
}

pub fn sync_main_cell_to_conversation(
    doc_cache: Res<crate::cell::DocumentCache>,
    entities: Res<EditorEntities>,
    session_principal: Res<crate::cell::SessionPrincipal>,
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
    let authenticated_principal = session_principal.0;

    let Ok((mut editor, viewing_opt)) = main_cell.get_mut(entity) else {
        return;
    };

    let needs_sync = needs_render_sync(
        viewing_opt.as_deref(),
        ctx_id,
        sync_version,
        authenticated_principal,
    );

    if !needs_sync {
        return;
    }

    // `CellEditor.store` is a `RenderBlockStore` (`view::render_store`) —
    // the local render buffer every other view system already reads
    // (block_border.rs, the view systems, diff_view, timeline, …). The change-feed
    // mirror holds plain `BlockSnapshot`s, so
    // `RenderBlockStore::rebuild` materializes them into a fresh store via
    // `insert_from_snapshot`, carrying `collapsed` forward per id across the
    // rebuild (see that method's doc — this runs nearly every frame during
    // live streaming, not just at hydration boundaries, so without that a
    // user's expand/collapse would be wiped within a frame or two).
    // `ContextMirror::blocks()` is already document order, so a plain
    // left-to-right insert reproduces it.
    //
    // The local principal's own compose draft (Lane C slice 3: an ordinary
    // `Status::Draft` block riding this same mirror) is skipped here — it
    // already renders in the compose box, so materializing it into the
    // transcript too would double-render it. A co-player's draft is left
    // alone deliberately: watching a neighbor type is a feature, not a bug
    // (see `block_border.rs`'s `Status::Draft` handling).
    let principal_id = session_principal.0.unwrap_or_else(|| editor.store.principal_id());
    let store = editor.store.rebuild(ctx_id, principal_id, cached.mirror.blocks(), |block| {
        block.status == kaijutsu_types::Status::Draft
            && session_principal.0.is_some_and(|principal| block.id.principal_id == principal)
    });
    let mut store = match store {
        Ok(store) => store,
        Err(e) => {
            tracing::error!("Failed to materialize the render buffer: {e}");
            return;
        }
    };
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
            viewing.principal_id = authenticated_principal;
        }
        None => {
            commands.entity(entity).insert(ViewingConversation {
                conversation_id: ctx_id,
                last_sync_version: sync_version,
                principal_id: authenticated_principal,
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
        //
        // `following` is deliberately flattened to false here, even for a
        // context that was at the bottom when last viewed: a context switch
        // is a jump to a fixed remembered offset, not a live "go to bottom"
        // request, so it should never auto-engage follow. If the restored
        // context happens to be at the bottom and the user wants live
        // tracking again, `scroll_by`/`ScrollToEnd`/a submit re-engages it
        // through the normal explicit paths.
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

/// Where a `kj` result run against `current` moves the view. A structured
/// run stays addressed to its context, so a verb that moves the player
/// names the move in its data: `switched_to` (`kj context switch`, `kj
/// fork`), or `rotated_from` with `context_id` when the seat that rotated is
/// `current`. Rotating another seat by label moves nothing. An id that does
/// not parse is an error, never a stay. The tui follows the same rule
/// (`kaijutsu-tui`'s `run::follow_target`).
pub fn follow_target(data: Option<&serde_json::Value>, current: ContextId) -> Result<Option<ContextId>, String> {
    let Some(data) = data else { return Ok(None) };
    let id_at = |key: &str| -> Result<Option<ContextId>, String> {
        match data.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .ok_or_else(|| format!("`{key}` is not a string"))
                .and_then(|hex| ContextId::parse(hex).map_err(|e| format!("`{key}` {hex:?}: {e}")))
                .map(Some),
        }
    };
    if let Some(target) = id_at("switched_to")? {
        return Ok((target != current).then_some(target));
    }
    if id_at("rotated_from")? == Some(current) {
        return id_at("context_id")?
            .map(Some)
            .ok_or_else(|| "rotate result names no successor".to_string());
    }
    Ok(None)
}

/// Follow a finished `kj` line from the shell surface (`Ctrl+A r`'s
/// `kj context rotate`, a typed `kj context switch`): switch to the context
/// its data names, but only while the context it ran against is still the
/// one on screen. The hints line says what the tui's status row says: a
/// latch's message, else the first stdout line. A failed run, or data that
/// cannot be followed, says why.
pub fn follow_kj_results(
    mut results: MessageReader<RpcResultMessage>,
    doc_cache: Res<crate::cell::DocumentCache>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
    mut notice: ResMut<crate::ui::ask_sheet::AskNotice>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs_f64();
    for result in results.read() {
        let RpcResultMessage::KjLineFinished { from, result } = result else {
            continue;
        };
        if result.exit_code != 0 {
            let why = result.stderr.lines().next().unwrap_or("").trim();
            let text = format!("kj failed (exit {}){}{why}", result.exit_code, if why.is_empty() { "" } else { ": " });
            log::warn!("{text}");
            notice.say(text, now);
            continue;
        }
        if let Some(latch) = &result.latch {
            notice.say(latch.message.clone(), now);
            continue;
        }
        let said = result.stdout.lines().next().unwrap_or("done");
        match follow_target(result.data.as_ref(), *from) {
            Ok(Some(target)) if doc_cache.active_id() == Some(*from) => {
                info!("kj: following {} -> {}", from.short(), target.short());
                notice.say(said, now);
                switch_writer.write(crate::cell::ContextSwitchRequested { context_id: target });
            }
            Ok(Some(target)) => {
                notice.say(format!("{said}; {} moved to {}, the view stayed", from.short(), target.short()), now);
            }
            Ok(None) => notice.say(said, now),
            Err(e) => {
                log::warn!("kj result from {} cannot be followed: {e}", from.short());
                notice.say(format!("{said}; cannot follow: {e}"), now);
            }
        }
    }
}

// `check_cache_staleness` (the old poll-driven re-fetch) is gone —
// `drain_context_feeds`, above, replaces it. Each followed context now gets
// its own precise recovery signal straight from its change feed instead of a
// coarse generation bump checked once a frame.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticated_principal_change_rebuilds_the_same_context_version() {
        let context_id = ContextId::new();
        let prior = PrincipalId::new();
        let next = PrincipalId::new();
        let viewing = ViewingConversation {
            conversation_id: context_id,
            last_sync_version: 7,
            principal_id: Some(prior),
        };

        assert!(needs_render_sync(Some(&viewing), context_id, 7, Some(next)));
        assert!(!needs_render_sync(Some(&viewing), context_id, 7, Some(prior)));
    }

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

    fn rotate_data(from: ContextId, successor: ContextId) -> serde_json::Value {
        serde_json::json!({"context_id": successor.to_hex(), "rotated_from": from.to_hex()})
    }

    /// Rotating the seat on screen follows its successor; rotating another
    /// seat by label leaves the view where it is.
    #[test]
    fn a_rotate_result_follows_only_the_seat_it_ran_against() {
        let (from, next, other) = (ContextId::new(), ContextId::new(), ContextId::new());
        assert_eq!(follow_target(Some(&rotate_data(from, next)), from), Ok(Some(next)));
        assert_eq!(follow_target(Some(&rotate_data(other, next)), from), Ok(None));
        let no_successor = serde_json::json!({"rotated_from": from.to_hex()});
        assert!(follow_target(Some(&no_successor), from).is_err());
    }

    #[test]
    fn a_switch_result_follows_its_target_and_other_results_stay() {
        let (here, there) = (ContextId::new(), ContextId::new());
        let switched = serde_json::json!({"switched_to": there.to_hex()});
        assert_eq!(follow_target(Some(&switched), here), Ok(Some(there)));
        let info = serde_json::json!({"context_id": there.to_hex(), "label": "x"});
        assert_eq!(follow_target(Some(&info), here), Ok(None));
        assert_eq!(follow_target(None, here), Ok(None));
        let bad = serde_json::json!({"switched_to": "not-an-id"});
        assert!(follow_target(Some(&bad), here).is_err());
    }

    fn kj_app(active: ContextId) -> App {
        let mut app = App::new();
        app.add_message::<RpcResultMessage>()
            .add_message::<crate::cell::ContextSwitchRequested>()
            .init_resource::<crate::ui::ask_sheet::AskNotice>()
            .init_resource::<Time>()
            .init_resource::<crate::cell::DocumentCache>()
            .add_systems(Update, follow_kj_results);
        app.world_mut()
            .resource_mut::<crate::cell::DocumentCache>()
            .set_active(active);
        app
    }

    fn finished(from: ContextId, exit_code: i32, stderr: &str, data: Option<serde_json::Value>) -> RpcResultMessage {
        RpcResultMessage::KjLineFinished {
            from,
            result: kaijutsu_client::rpc::KjExecutionResult {
                exit_code,
                stdout: String::new(),
                stderr: stderr.into(),
                command_block_id: None,
                latch: None,
                data,
            },
        }
    }

    fn switches(app: &mut App) -> Vec<ContextId> {
        let msgs = app.world().resource::<Messages<crate::cell::ContextSwitchRequested>>();
        msgs.iter_current_update_messages().map(|m| m.context_id).collect()
    }

    #[test]
    fn a_rotation_of_the_seat_on_screen_switches_to_the_successor() {
        let (from, next) = (ContextId::new(), ContextId::new());
        let mut app = kj_app(from);
        app.world_mut().write_message(finished(from, 0, "", Some(rotate_data(from, next))));
        app.update();
        assert_eq!(switches(&mut app), vec![next]);
    }

    #[test]
    fn a_rotation_of_a_seat_no_longer_on_screen_does_not_move_the_view() {
        let (from, next, here) = (ContextId::new(), ContextId::new(), ContextId::new());
        let mut app = kj_app(here);
        app.world_mut().write_message(finished(from, 0, "", Some(rotate_data(from, next))));
        app.update();
        assert!(switches(&mut app).is_empty());
    }

    fn notice(app: &App) -> Option<String> {
        app.world().resource::<crate::ui::ask_sheet::AskNotice>().current(0.0).map(str::to_string)
    }

    /// As the tui says it: the first stdout line, or a latch's own message.
    #[test]
    fn a_finished_line_says_its_first_stdout_line_or_its_latch() {
        let here = ContextId::new();
        let mut app = kj_app(here);
        let mut said = finished(here, 0, "", None);
        if let RpcResultMessage::KjLineFinished { result, .. } = &mut said {
            result.stdout = "renamed to banto\nmore".into();
        }
        app.world_mut().write_message(said);
        app.update();
        assert_eq!(notice(&app).as_deref(), Some("renamed to banto"));
        let mut latched = finished(here, 0, "", None);
        if let RpcResultMessage::KjLineFinished { result, .. } = &mut latched {
            result.latch = Some(kaijutsu_client::rpc::KjLatch {
                command: "archive".into(),
                target: "x".into(),
                message: "run it again to confirm".into(),
            });
        }
        app.world_mut().write_message(latched);
        app.update();
        assert_eq!(notice(&app).as_deref(), Some("run it again to confirm"));
    }

    #[test]
    fn a_failed_rotation_says_why_and_stays() {
        let from = ContextId::new();
        let mut app = kj_app(from);
        app.world_mut()
            .write_message(finished(from, 1, "kj context rotate: lacks the 'operator' capability\n", None));
        app.update();
        assert!(switches(&mut app).is_empty());
        let notice = app.world().resource::<crate::ui::ask_sheet::AskNotice>();
        assert_eq!(
            notice.current(0.0),
            Some("kj failed (exit 1): kj context rotate: lacks the 'operator' capability")
        );
    }
}

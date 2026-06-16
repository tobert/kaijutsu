//! The layout tick: reconcile the keyed join against the polled context list,
//! spawn/despawn card entities, recompute the compacting band layout, and write
//! each card's target position + derived [`CardData`].
//!
//! Cadence: this runs off [`DriftState`], which already polls `list_contexts`
//! every few seconds (the layout-tick source from `docs/viz-substrate.md`).
//! `Join::reconcile` here is the **layout tick**; per-block status pulses (the
//! **data tick**) arrive separately and use `Join::touch` (see `super::status`,
//! task 6).

use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_client::ContextInfo;
use kaijutsu_types::ContextId;

use kaijutsu_client::ServerEvent;

use super::card::{assign_bands, card_from, layout_positions, lift};
use super::scene::{CARD_TEX_H, CARD_TEX_W, Card, CardTarget, TimeWellState};
use crate::connection::ServerEventMessage;
use crate::view::vello_ui_texture::{VelloUiScene, VelloUiTexture, create_vello_texture};

/// Reconcile the well against the latest polled context list.
///
/// Gated on `DriftState` changing so a static context set costs nothing. On a
/// change: diff via the join, despawn exits, recompute the layout over the full
/// current set, spawn enters at their target, and refresh every surviving card's
/// target + data (so compaction motion lands on all of them).
pub fn sync_time_well(
    mut commands: Commands,
    mut state: ResMut<TimeWellState>,
    drift: Res<crate::ui::drift::DriftState>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut cards: Query<(&mut Card, &mut CardTarget)>,
) {
    // The well shows live + concluded contexts; archived are hidden entirely.
    let visible: Vec<&ContextInfo> = drift.contexts.iter().filter(|c| !c.archived).collect();

    // Run when the poll changed the resource, or when the join is out of sync
    // with the current count. The count fallback covers first-enter (join was
    // cleared on exit) and dodges the Bevy run-condition change-detection
    // footgun where a screen-gated system can miss a change that happened while
    // it was dormant.
    if !drift.is_changed() && state.join.len() == visible.len() {
        return;
    }

    // ── Layout tick: diff the polled list against the join. ──
    let snapshot = visible.iter().map(|c| (c.id, (*c).clone()));
    let diff = state.join.reconcile(snapshot);
    if diff.is_empty() {
        return;
    }

    debug!(
        "time-well sync: +{} ~{} -{} (now {} cards)",
        diff.enter.len(),
        diff.update.len(),
        diff.exit.len(),
        state.join.len(),
    );

    // ── Despawn exits. ──
    for ex in &diff.exit {
        if let Some(e) = state.entities.remove(&ex.key) {
            commands.entity(e).despawn();
        }
    }

    // ── Recompute bands + layout over the full current set. ──
    // Stable key order from the join (BTreeMap order); positions key on id.
    let contexts: Vec<ContextInfo> = state
        .join
        .keys()
        .filter_map(|k| state.join.get(k).cloned())
        .collect();
    let bands = assign_bands(&contexts);
    let band_by_id: HashMap<ContextId, kaijutsu_viz::layout::Band> = contexts
        .iter()
        .map(|c| c.id)
        .zip(bands.iter().copied())
        .collect();
    let positions = layout_positions(&contexts, &bands, &state.layout);

    // ── Band-0 slot order: hot ids ascending by id (= the layout's slot order,
    // since order_key is the id rank). This is what `0–9` address. ──
    let mut hot_order: Vec<ContextId> = contexts
        .iter()
        .filter(|c| band_by_id.get(&c.id) == Some(&kaijutsu_viz::layout::Band::Hot))
        .map(|c| c.id)
        .collect();
    hot_order.sort_unstable();
    state.hot_order = hot_order;
    // Keep selection valid: drop it if its context left the well; default to the
    // first hot slot when nothing (valid) is selected.
    let selection_valid = state
        .selected
        .is_some_and(|id| state.entities.contains_key(&id) || state.join.contains(&id));
    if !selection_valid {
        state.selected = state.hot_order.first().copied();
    }

    // Resolve a target Vec3 per id up front (needs &state.geom).
    let geom = state.geom;
    let target_of = |id: &ContextId| positions.get(id).map(|p| lift(p, &geom));

    // ── Spawn entered cards at their resolved position. ──
    let card_mesh = state
        .card_mesh
        .clone()
        .expect("card_mesh built on enter_time_well");

    // Collect enters first to avoid holding an immutable borrow of the diff while
    // mutating `state`.
    let entered: Vec<(ContextId, ContextInfo)> = diff
        .enter
        .iter()
        .map(|e| (e.key, e.value.clone()))
        .collect();

    for (id, info) in entered {
        let band = band_by_id
            .get(&id)
            .copied()
            .unwrap_or(kaijutsu_viz::layout::Band::Hot);
        let data = card_from(&info, band);
        let pos = target_of(&id).unwrap_or(Vec3::ZERO);

        // Per-card RTT texture: the vello scene (accent bg + text, built by
        // `text::build_card_scenes`) rasterizes into this and the material
        // samples it. White base color so the texture's own colors show through;
        // Blend so the rounded-rect corners stay transparent.
        let tex_w = CARD_TEX_W as u32;
        let tex_h = CARD_TEX_H as u32;
        let image = create_vello_texture(&mut images, tex_w, tex_h);
        let material = materials.add(StandardMaterial {
            base_color: Color::WHITE,
            base_color_texture: Some(image.clone()),
            unlit: true,
            cull_mode: None,
            double_sided: true,
            // Mask, not Blend: masked alpha is order-independent, so cards never
            // swap draw order under transparent depth-sorting (the bg is opaque;
            // only the rounded corners fall below the cutoff and get discarded).
            alpha_mode: AlphaMode::Mask(0.5),
            ..default()
        });

        let entity = commands
            .spawn((
                Card {
                    context_id: id,
                    data,
                    status: None,
                },
                CardTarget(pos),
                Mesh3d(card_mesh.clone()),
                MeshMaterial3d(material),
                Transform::from_translation(pos),
                Visibility::Inherited,
                VelloUiScene::default(),
                VelloUiTexture {
                    image,
                    width: tex_w,
                    height: tex_h,
                },
                Name::new(format!("Card({})", id.short())),
            ))
            .id();

        state.entities.insert(id, entity);
    }

    // ── Refresh surviving cards: target (compaction) + data (metadata). ──
    // Snapshot the id→entity pairs to avoid borrowing `state` inside the loop.
    let pairs: Vec<(ContextId, Entity)> = state.entities.iter().map(|(&id, &e)| (id, e)).collect();
    for (id, entity) in pairs {
        let Ok((mut card, mut target)) = cards.get_mut(entity) else {
            continue; // just-spawned this frame; already correct
        };
        if let Some(pos) = target_of(&id) {
            target.0 = pos;
        }
        if let Some(info) = state.join.get(&id) {
            let band = band_by_id.get(&id).copied().unwrap_or(card.data.band);
            // Only rewrite when the derived card actually changed, so `Changed<Card>`
            // (the scene-rebuild trigger) doesn't fire every poll for static cards.
            let next = card_from(info, band);
            if card.data != next {
                card.data = next;
            }
        }
    }
}

/// The **data tick**: map block status events onto the matching card's `status`.
///
/// Taps the app's existing `ServerEvent` stream (the same one drift listens to).
/// A status change mutates only `Card.status` — never the entity set or
/// positions — so it triggers a card-scene rebuild (the status glyph) but never
/// a relayout, honoring the two-cadence split from `docs/viz-substrate.md`.
///
/// Coverage note: this reflects only contexts the app is *already subscribed to*
/// (active / cached). A dedicated `subscribeBlocksFiltered` over the full visible
/// set — so every rim card pulses — is the documented follow-up (gap 3).
pub fn apply_block_status(
    mut events: MessageReader<ServerEventMessage>,
    mut cards: Query<&mut Card>,
) {
    for ServerEventMessage(ev) in events.read() {
        let ServerEvent::BlockStatusChanged {
            context_id, status, ..
        } = ev
        else {
            continue;
        };
        for mut card in cards.iter_mut() {
            if card.context_id == *context_id && card.status != Some(*status) {
                card.status = Some(*status);
            }
        }
    }
}

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

use super::card::{
    ClusterAssignment, assign_bands, band_orders, card_base_scale, card_from, ring_seat_rotated,
    spiral_positions,
};
use super::scene::{CARD_TEX_H, CARD_TEX_W, Card, CardTarget, RingSeat, TimeWellState};
use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};
use super::panel::create_msdf_panel;

/// Current wall-clock time as Unix-epoch millis. Idle age (`now -
/// last_activity_at`) is real elapsed time, not app-uptime — so this reads
/// `SystemTime`, **not** Bevy's `Res<Time>` (which is startup-relative and
/// would misclassify every context as ancient after any nontrivial uptime).
/// Fails loudly on a clock set before the epoch rather than silently
/// defaulting to 0 (which would band everything as maximally idle).
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch")
        .as_millis() as i64
}

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
    mut materials: ResMut<Assets<crate::shaders::WellCardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut cards: Query<(&mut Card, &mut CardTarget, &mut RingSeat)>,
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
    let now = now_millis();
    let bands = assign_bands(&contexts, now);
    let band_by_id: HashMap<ContextId, kaijutsu_viz::layout::Band> = contexts
        .iter()
        .map(|c| c.id)
        .zip(bands.iter().copied())
        .collect();
    // Snapshot the cluster map so we can both feed the slot ordering and read
    // labels in the spawn/refresh loops below without re-borrowing `state`
    // (which those loops mutate). It's Horizon-sized — a cheap clone.
    let cluster_of = state.cluster_of.clone();
    // The whole well as one ordered spiral (mouth → throat) plus each card's
    // `(band, within_index)` position — the flat order is the digit-jump address;
    // the pair is what the ring geometry (`ring_seat`/`card_base_scale`) keys on.
    let (order, pos_of) = spiral_positions(&contexts, &bands);
    state.spiral_order = order;

    // Ring-centric layout: each band ring's cards in within-ring (recency) order.
    // This is the nav's source of truth — `(focused_ring, ring_pos)` indexes it.
    state.ring_cards = band_orders(&contexts, &bands);

    // Snapshot per-band ring length + the eased per-ring rotation as plain locals
    // so the placement closures don't borrow `state` (which we mutate just below).
    let ring_len: [usize; super::card::N_BANDS] =
        std::array::from_fn(|i| state.ring_cards[i].len());
    let ring_rotation = state.ring_rotation;

    // Re-derive the selection from the seat `(focused_ring, ring_pos)`, clamping
    // the position to the (possibly changed) focused ring, and spin that ring so
    // the selected card sits at the gate. Selection now follows *position*, not a
    // context id — the ring-centric model.
    let fr = state.focused_ring.min(super::card::N_BANDS - 1);
    state.focused_ring = fr;
    let flen = ring_len[fr];
    let ring_pos = super::card::carry_ring_pos(state.ring_pos, flen);
    state.ring_pos = ring_pos;
    let sel = state.ring_cards[fr].get(ring_pos).copied();
    state.selected = sel;
    let cur_tgt = state.ring_rotation_target[fr];
    state.ring_rotation_target[fr] =
        super::card::spin_target_to_gate(cur_tgt, ring_pos, flen.max(1));

    // Resolve a target position + base scale per id: seat it on its band's ring
    // at its within-band index, spaced by the ring length, spun by the ring's
    // current rotation.
    let target_of = |id: &ContextId| {
        pos_of.get(id).map(|&(b, wi)| {
            ring_seat_rotated(b, wi, ring_len[b.index()], ring_rotation[b.index()])
        })
    };
    let scale_of = |id: &ContextId| {
        pos_of.get(id).map(|&(b, wi)| card_base_scale(b, wi)).unwrap_or(1.0)
    };
    // A card's discrete ring seat (band + within-ring index + ring length) — the
    // durable position `spin_rings` re-derives the live `CardTarget` from.
    let seat_of = |id: &ContextId| {
        pos_of.get(id).map(|&(b, wi)| RingSeat {
            band: b,
            within_index: wi,
            ring_len: ring_len[b.index()],
        })
    };

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
            .unwrap_or(kaijutsu_viz::layout::Band::HotNow);
        let cluster_label = cluster_of.get(&id).map(|a| a.label.clone());
        let data = card_from(&info, band, cluster_label);
        let pos = target_of(&id).unwrap_or(Vec3::ZERO);

        // Per-card RTT texture: the vello scene (accent bg + text, built by
        // `text::build_card_scenes`) rasterizes into this and the `WellCardMaterial`
        // samples it (the texture = content layer; the shader is where slice-2 FX
        // will live). Masked alpha keeps the rounded-rect corners transparent.
        // Per-card MSDF panel: the RTT texture `text::build_card_scenes` lays glyphs
        // into and the `WellCardMaterial` samples (the texture = content layer; the
        // shader draws the body + rims). Masked alpha keeps the corners transparent.
        let (image, panel) = create_msdf_panel(&mut images, CARD_TEX_W as u32, CARD_TEX_H as u32);
        let material = materials.add(crate::shaders::WellCardMaterial {
            texture: image,
            accent: super::scene::accent_vec4(&data.accent),
            params: Vec4::ZERO,
            shape: super::scene::card_shape(),
            border: Vec4::ZERO,
        });

        let entity = commands
            .spawn((
                Card {
                    context_id: id,
                    data,
                    // Live activity comes straight off the poll now (kernel-derived,
                    // see `ContextInfo.live_status`) — covers every visible card, not
                    // just the active context. Drives the card pulse.
                    status: Some(info.live_status),
                    selected: false,
                    in_lineage: false,
                    drifting: false,
                    base_scale: scale_of(&id),
                },
                CardTarget(pos),
                seat_of(&id).unwrap_or(RingSeat {
                    band,
                    within_index: 0,
                    ring_len: 1,
                }),
                Mesh3d(card_mesh.clone()),
                MeshMaterial3d(material),
                Transform::from_translation(pos),
                Visibility::Inherited,
                panel,
                Name::new(format!("Card({})", id.short())),
            ))
            .id();

        state.entities.insert(id, entity);
    }

    // ── Refresh surviving cards: target (compaction) + data (metadata). ──
    // Snapshot the id→entity pairs to avoid borrowing `state` inside the loop.
    let pairs: Vec<(ContextId, Entity)> = state.entities.iter().map(|(&id, &e)| (id, e)).collect();
    for (id, entity) in pairs {
        let Ok((mut card, mut target, mut seat)) = cards.get_mut(entity) else {
            continue; // just-spawned this frame; already correct
        };
        if let Some(pos) = target_of(&id) {
            target.0 = pos;
        }
        // Update the discrete ring seat so `spin_rings` re-seats it correctly if
        // its within-ring index or ring length shifted this tick.
        if let Some(next_seat) = seat_of(&id) {
            seat.band = next_seat.band;
            seat.within_index = next_seat.within_index;
            seat.ring_len = next_seat.ring_len;
        }
        // Base scale follows the card's (possibly shifted) spiral index. Guarded
        // so a static card doesn't trip `Changed<Card>` (text rebuild) each poll.
        let ns = scale_of(&id);
        if (card.base_scale - ns).abs() > 1e-3 {
            card.base_scale = ns;
        }
        if let Some(info) = state.join.get(&id) {
            let band = band_by_id.get(&id).copied().unwrap_or(card.data.band);
            // Only rewrite when the derived card actually changed, so `Changed<Card>`
            // (the scene-rebuild trigger) doesn't fire every poll for static cards.
            let cluster_label = cluster_of.get(&id).map(|a| a.label.clone());
            let next = card_from(info, band, cluster_label);
            if card.data != next {
                card.data = next;
            }
            // Live status rides the same poll; same change-guard so the pulse
            // flips without re-rasterizing static cards every poll.
            let next_status = Some(info.live_status);
            if card.status != next_status {
                card.status = next_status;
            }
        }
    }
}

/// How often to poll semantic clusters for the haystack (seconds). Clusters
/// change slowly, so this is a coarse cadence — independent of the drift poll.
const CLUSTER_POLL_INTERVAL: f64 = 8.0;

/// Smallest cluster the haystack cares about — pairs and up; a singleton isn't a
/// cluster worth grouping or labeling.
const MIN_CLUSTER_SIZE: u32 = 2;

/// Poll semantic clusters (the band-2 angle + label source) while the well is
/// open.
///
/// Mirrors `ui::drift::poll_drift_state`: clone the handle, spawn an IO task,
/// ship the result back over `RpcResultChannel`. Throttled and well-only (the
/// haystack is the only consumer). An empty result — e.g. the kernel has no
/// semantic index — is fine: band-2 then falls back to creation-id order.
pub fn poll_clusters(
    actor: Option<Res<RpcActor>>,
    time: Res<Time>,
    mut last_poll: Local<f64>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };
    let elapsed = time.elapsed_secs_f64();
    if elapsed - *last_poll < CLUSTER_POLL_INTERVAL {
        return;
    }
    *last_poll = elapsed;

    let handle = actor.handle.clone();
    let tx = result_channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle.get_clusters(MIN_CLUSTER_SIZE).await {
                Ok(clusters) => {
                    let _ = tx.send(RpcResultMessage::ClustersReceived { clusters });
                }
                Err(e) => log::debug!("time-well: get_clusters failed: {e}"),
            }
        })
        .detach();
}

/// Drain `ClustersReceived` into `TimeWellState.cluster_of` (id → assignment).
///
/// A separate `MessageReader` from drift's drain — Bevy readers keep independent
/// cursors, so this doesn't steal events from the drift drain. Each event is a
/// complete cluster snapshot, so we rebuild the whole id→assignment map.
pub fn apply_clusters(
    mut state: ResMut<TimeWellState>,
    mut events: MessageReader<RpcResultMessage>,
) {
    for ev in events.read() {
        if let RpcResultMessage::ClustersReceived { clusters } = ev {
            let mut map: HashMap<ContextId, ClusterAssignment> = HashMap::new();
            for c in clusters {
                let assignment = ClusterAssignment {
                    cluster_id: c.cluster_id,
                    label: c.label.clone(),
                };
                for id in &c.context_ids {
                    map.insert(*id, assignment.clone());
                }
            }
            debug!(
                "time-well clusters: {} clusters, {} contexts assigned",
                clusters.len(),
                map.len()
            );
            state.cluster_of = map;
        }
    }
}

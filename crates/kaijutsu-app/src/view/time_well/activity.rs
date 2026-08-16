//! The well's pulse: a pure-ish activity model for the base ring deck.
//!
//! # DISABLED — nothing feeds this today (Amy, 2026-08-15)
//!
//! The model below is live, correct, and unit-tested. **No system calls
//! [`RingActivity::record`]**, so the energy sits at zero and the rings render
//! calm. That is deliberate and temporary, and it is stated here rather than
//! left for someone to discover by wondering why the well never lights up.
//!
//! What it used to do: ingest the **kernel-wide** `ServerEvent` stream and
//! weigh each block event as a heartbeat — token streaming loudest, because it
//! means a model is writing right now. That worked, and it cost the whole
//! event stream: the app received every token of every context in order to
//! choose a brightness. It was also the last consumer of the raw
//! storage-engine wire events (docs/change-feed.md), which is how it came up.
//!
//! Amy's call: *"we should temporarily disable that activity glow in the app
//! pending a redesign. we'll be doing embeddings for a lot of that content
//! kernel side and maybe we can emit something more useful and derived."* So
//! the signal is meant to come back richer and from the kernel — not "how many
//! events went by" but something about what is actually happening — and the
//! decay/ripple math here is kept precisely because it is the part that will
//! still be right when it does.
//!
//! To bring it back: feed [`RingActivity::record`] from whatever the kernel
//! emits, and re-register an ingest system in `time_well/mod.rs`.
//!
//! ---
//!
//! It decays recorded heartbeats over time and exposes:
//!
//! - a smoothed global **energy** level (drives the ring brightness + flow speed
//!   + core spin in `well_rings.wgsl`),
//! - a bounded set of live **ripples**, each fired at the *angle* of the context
//!   that produced the event — so a busy conversation throws a wavefront out from
//!   its direction on the ring (the "localize to context angle" behavior), and
//! - a per-context decaying activity level (for a future per-card reaction).
//!
//! The energy/ripple math is pure and unit-tested; only the angle (which comes
//! from a card's world position) and the event→weight mapping touch Bevy/client
//! types. The brightness/animation itself lives in the shader, not here.

use std::collections::HashMap;

use bevy::prelude::Resource;
use kaijutsu_types::ContextId;

/// Max simultaneous ripples the shader renders (must match the array length in
/// `WellRingsMaterial`/`well_rings.wgsl`). A busy system keeps the freshest.
pub const MAX_RIPPLES: usize = 8;

/// Energy a unit-weight event injects (scaled by the event's weight).
#[allow(dead_code)] // Unfed while the glow is disabled — see the module doc.
const ENERGY_PER_EVENT: f32 = 0.34;
/// Energy ceiling (well past 1.0 so a flurry pins the rings bright; the shader
/// reads `energy` directly and the HDR bloom soft-clamps the visual).
#[allow(dead_code)] // Unfed while the glow is disabled — see the module doc.
const ENERGY_MAX: f32 = 4.0;
/// Global energy exponential decay rate (per second). Higher = quicker calm.
const ENERGY_DECAY: f32 = 1.6;

/// Per-context activity ceiling + decay (a touch faster than global so a single
/// card's glow tracks *its* traffic, not the whole system's afterglow). Public:
/// `live::sync_card_live_uniforms` normalizes by this for the chatter lane.
pub const CONTEXT_MAX: f32 = 3.0;
const CONTEXT_DECAY: f32 = 2.2;
/// Below this a per-context entry is dropped (keeps the map from growing).
const CONTEXT_EPSILON: f32 = 1e-2;

/// Seconds a ripple lives (from spawn to fully expanded + faded). The shader
/// maps `age / RIPPLE_LIFETIME` to the wavefront radius, so this is also the
/// time a ping takes to travel from the core to the rim.
pub const RIPPLE_LIFETIME: f32 = 1.8;

/// One expanding wavefront on the ring deck, fired by a context's event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ripple {
    /// Where on the ring it emanates, in **world** ring-plane radians
    /// (`atan2(card.y, card.x)`); the shader windows the wavefront around it.
    pub angle: f32,
    /// Seconds since spawn; `age / RIPPLE_LIFETIME` is the normalized radius.
    pub age: f32,
    /// Initial strength (0..1, from the event weight) — fades with age.
    pub intensity: f32,
}

/// Live activity state for the well's base rings. A Bevy resource, but the
/// `record`/`tick` math is plain and unit-tested.
#[derive(Resource, Default)]
pub struct RingActivity {
    /// Smoothed global energy (0..[`ENERGY_MAX`]).
    pub energy: f32,
    /// Decaying per-context activity, keyed by context.
    pub per_context: HashMap<ContextId, f32>,
    /// Live ripples (≤ [`MAX_RIPPLES`]).
    pub ripples: Vec<Ripple>,
}

impl RingActivity {
    /// Record one kernel event for `ctx` at ring `angle` with `weight`
    /// (token-streaming weighs most; bookkeeping least — see [`event_signal`]).
    /// Bumps global energy + the per-context level and fires a ripple. At the
    /// ripple cap the **oldest** ripple is evicted so the freshest wavefronts
    /// survive a flurry.
    #[allow(dead_code)] // The disabled glow's entry point — see the module doc.
    pub fn record(&mut self, ctx: ContextId, angle: f32, weight: f32) {
        self.energy = (self.energy + ENERGY_PER_EVENT * weight).min(ENERGY_MAX);

        let e = self.per_context.entry(ctx).or_insert(0.0);
        *e = (*e + weight).min(CONTEXT_MAX);

        if self.ripples.len() >= MAX_RIPPLES
            && let Some((i, _)) = self
                .ripples
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.age.total_cmp(&b.1.age))
        {
            self.ripples.swap_remove(i);
        }
        self.ripples.push(Ripple {
            angle,
            age: 0.0,
            intensity: weight.clamp(0.0, 1.0),
        });
    }

    /// Advance time: decay global + per-context energy (exponential, frame-rate
    /// independent), age the ripples, and drop the expired/negligible ones.
    pub fn tick(&mut self, dt: f32) {
        let g = (-ENERGY_DECAY * dt).exp();
        self.energy *= g;
        if self.energy < 1e-3 {
            self.energy = 0.0;
        }

        let c = (-CONTEXT_DECAY * dt).exp();
        self.per_context.retain(|_, v| {
            *v *= c;
            *v > CONTEXT_EPSILON
        });

        for r in &mut self.ripples {
            r.age += dt;
        }
        self.ripples.retain(|r| r.age < RIPPLE_LIFETIME);
    }

    /// Current activity level for one context (0.0 if quiet/unknown). Drives
    /// the per-card chatter glow: `live::sync_card_live_uniforms` normalizes
    /// this by [`CONTEXT_MAX`] into the card material's `dim.y` lane.
    pub fn context_energy(&self, ctx: &ContextId) -> f32 {
        self.per_context.get(ctx).copied().unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(n: u8) -> ContextId {
        ContextId::from_bytes([n; 16])
    }

    #[test]
    fn record_raises_energy_then_tick_decays_toward_zero() {
        let mut a = RingActivity::default();
        assert_eq!(a.energy, 0.0);

        a.record(ctx(1), 0.0, 1.0);
        let after_record = a.energy;
        assert!(after_record > 0.0, "an event injects energy");

        a.tick(0.5);
        assert!(a.energy < after_record, "energy decays over time");

        // A long quiet stretch returns to fully calm.
        for _ in 0..50 {
            a.tick(0.5);
        }
        assert_eq!(a.energy, 0.0, "energy snaps to 0 once negligible");
    }

    #[test]
    fn event_weight_scales_energy_injection() {
        let mut loud = RingActivity::default();
        let mut quiet = RingActivity::default();
        loud.record(ctx(1), 0.0, 1.0);
        quiet.record(ctx(1), 0.0, 0.25);
        assert!(
            loud.energy > quiet.energy,
            "a heavier event injects more energy"
        );
    }

    #[test]
    fn ripple_spawns_ages_and_expires() {
        let mut a = RingActivity::default();
        a.record(ctx(1), 1.23, 1.0);
        assert_eq!(a.ripples.len(), 1);
        assert_eq!(a.ripples[0].age, 0.0);
        assert_eq!(a.ripples[0].angle, 1.23, "ripple carries the context angle");

        a.tick(RIPPLE_LIFETIME * 0.5);
        assert_eq!(a.ripples.len(), 1);
        assert!(a.ripples[0].age > 0.0, "ripple ages");

        a.tick(RIPPLE_LIFETIME); // total age now > lifetime
        assert!(a.ripples.is_empty(), "ripple expires after its lifetime");
    }

    #[test]
    fn ripples_cap_at_max_evicting_the_oldest() {
        let mut a = RingActivity::default();
        // Fire MAX_RIPPLES, aging between each so they have distinct ages.
        for i in 0..MAX_RIPPLES {
            a.record(ctx(i as u8), i as f32, 1.0);
            a.tick(0.01);
        }
        assert_eq!(a.ripples.len(), MAX_RIPPLES);
        let oldest_angle = a.ripples[0].angle; // the first fired = oldest

        // One more over the cap: the oldest is evicted, the newest survives.
        a.record(ctx(99), 9.99, 1.0);
        assert_eq!(a.ripples.len(), MAX_RIPPLES, "stays capped");
        assert!(
            !a.ripples.iter().any(|r| r.angle == oldest_angle),
            "the oldest ripple was evicted"
        );
        assert!(
            a.ripples.iter().any(|r| r.angle == 9.99),
            "the freshest ripple survives"
        );
    }

    #[test]
    fn per_context_energy_decays_and_is_dropped() {
        let mut a = RingActivity::default();
        a.record(ctx(1), 0.0, 1.0);
        a.record(ctx(2), 0.0, 1.0);
        assert!(a.context_energy(&ctx(1)) > 0.0);

        // ctx(1) keeps getting traffic; ctx(2) goes quiet.
        for _ in 0..3 {
            a.record(ctx(1), 0.0, 1.0);
            a.tick(0.1);
        }
        assert!(
            a.context_energy(&ctx(1)) > a.context_energy(&ctx(2)),
            "the busy context reads hotter than the idle one"
        );

        // Long quiet stretch evicts the idle entry entirely.
        for _ in 0..50 {
            a.tick(0.5);
        }
        assert_eq!(a.context_energy(&ctx(2)), 0.0);
        assert!(
            !a.per_context.contains_key(&ctx(2)),
            "negligible per-context entries are pruned"
        );
    }

}

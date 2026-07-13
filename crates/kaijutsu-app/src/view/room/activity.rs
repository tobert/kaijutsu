//! The room's ambient pulse: per-bearing decaying activity fed from the same
//! kernel-wide `ServerEvent` stream the well already ingests
//! (`view/time_well/activity.rs` is the sibling model for the well's rings).
//! "The shell adds renderers, not wire" (`docs/scenes/shell.md`) — this is a
//! thin room-owned consumer that routes events to *bearings* instead of
//! contexts.
//!
//! One live signal today: **beat/track events** ([`ServerEvent::BeatSync`]) →
//! the **East** tracks bearing, so a jam warms the tracks marker. The
//! rhythmic *breathing* on top of this comes from the well's beat phasors
//! ([`super::super::time_well::live::WellBeats::global_envelope`]) read
//! directly by the glow system — this decaying level is the sustained "a
//! track is rolling" warmth under the pulse.
//!
//! Slice A also routed context chatter (block inserts / text ops / status) to
//! a **Center** console bearing, warming the slice-A `ConsoleEmblem`
//! placeholder. That placeholder — and the glow branch reading it — is gone
//! now that the real time well stands at the console bearing with its own
//! (richer) energy model, so [`event_bearing`] dropped the Center mapping
//! entirely (freeze-fix slice, 2026-07-11) rather than keep accumulating
//! activity nothing reads. [`Bearing::Center`] itself stays — room geometry
//! still uses it — only this event mapping stopped feeding it.
//!
//! The math is pure and unit-tested; only [`event_bearing`] touches client
//! types. The Bevy ingest + glow live in [`super`].

use kaijutsu_client::ServerEvent;

use super::bearing::Bearing;

/// Per-bearing activity ceiling (a flurry pins it here; the glow gain reads the
/// normalized 0..1 fraction). Matches the well's `CONTEXT_MAX` scale so the two
/// ambient models read alike. **Amy-tunable.**
pub const BEARING_MAX: f32 = 3.0;
/// Exponential decay rate (per second) — a touch quick so a bearing's glow
/// tracks *its* recent traffic, not a long afterglow.
const BEARING_DECAY: f32 = 2.2;
/// Below this a bearing's level snaps to zero (a settled marker stops writing).
const BEARING_EPSILON: f32 = 1e-3;

/// Decaying activity for each [`Bearing`], keyed by [`Bearing::index`]. A Bevy
/// resource, but `record`/`tick` are plain and unit-tested.
#[derive(bevy::prelude::Resource)]
pub struct BearingActivity {
    levels: [f32; Bearing::COUNT],
}

impl Default for BearingActivity {
    fn default() -> Self {
        Self { levels: [0.0; Bearing::COUNT] }
    }
}

impl BearingActivity {
    /// Inject `weight` of activity at `bearing`, saturating at [`BEARING_MAX`].
    pub fn record(&mut self, bearing: Bearing, weight: f32) {
        let e = &mut self.levels[bearing.index()];
        *e = (*e + weight).min(BEARING_MAX);
    }

    /// Advance time: exponential decay of every bearing (frame-rate
    /// independent), snapping negligible levels to zero.
    pub fn tick(&mut self, dt: f32) {
        let k = (-BEARING_DECAY * dt).exp();
        for v in &mut self.levels {
            *v *= k;
            if *v < BEARING_EPSILON {
                *v = 0.0;
            }
        }
    }

    /// Raw activity level at `bearing` (0.0 when quiet).
    pub fn level(&self, bearing: Bearing) -> f32 {
        self.levels[bearing.index()]
    }

    /// Activity at `bearing` normalized to 0..1 by [`BEARING_MAX`] — the glow
    /// gain multiplier.
    pub fn normalized(&self, bearing: Bearing) -> f32 {
        (self.level(bearing) / BEARING_MAX).clamp(0.0, 1.0)
    }
}

/// Map a kernel event to `(bearing, weight)`, or `None` for events that aren't
/// room-ambient signal. Only beat syncs feed room-ambient activity today (the
/// tracks/East bearing — a clock is rolling).
///
/// **North (VFS/`Station::Vfs`) is deliberately absent here too**, but for a
/// different reason than the Center drop below: it's not that nothing should
/// feed it, it's that THIS function is the wrong seam for it. VFS churn
/// arrives as a digest's cumulative TOTAL per path, not a discrete pulse like
/// `BeatSync` — turning "total" into "an event just happened" needs baseline
/// state across calls (`view::fsn::heat::FsnHeat::observe`'s whole reason for
/// existing: first-sighting and kernel-restart re-baselining, `FsnHeat`'s own
/// module doc). A stateless `ev -> (bearing, weight)` match can't hold that
/// baseline. The FSN heat ingest (arriving in a follow-up — the one system
/// lane A2 deliberately leaves unbuilt, `view::fsn::heat`'s own module doc)
/// records North's `BearingActivity` directly, alongside recording into
/// `FsnHeat` itself, rather than routing through this function.
///
/// Block/chatter events are deliberately NOT mapped here (freeze-fix slice,
/// 2026-07-11): they used to warm a `Center` bearing for the slice-A
/// `ConsoleEmblem` placeholder, but that placeholder — and the glow branch
/// reading it — is gone now that the real time well stands at the console
/// bearing with its own richer chatter/energy model
/// ([`super::super::time_well::activity::event_signal`] → per-context
/// ripples + global energy, a model this mapping never had). Dropped the
/// Center arms rather than leave them accumulating into a resource with no
/// reader (`docs/issues.md`'s now-resolved "`BearingActivity(Center)` has no
/// reader left" entry). [`Bearing::Center`] itself stays (room geometry still
/// uses it) — only this event mapping stops feeding it.
pub fn event_bearing(ev: &ServerEvent) -> Option<(Bearing, f32)> {
    match ev {
        ServerEvent::BeatSync { .. } => Some((Bearing::East, 1.0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::BlockId;
    use kaijutsu_types::{ContextId, PrincipalId, Status};

    fn ctx(n: u8) -> ContextId {
        ContextId::from_bytes([n; 16])
    }

    #[test]
    fn record_raises_a_bearing_then_tick_decays_it_to_zero() {
        let mut a = BearingActivity::default();
        assert_eq!(a.level(Bearing::East), 0.0);

        a.record(Bearing::East, 1.0);
        let after = a.level(Bearing::East);
        assert!(after > 0.0, "an event injects activity");

        a.tick(0.5);
        assert!(a.level(Bearing::East) < after, "activity decays over time");

        for _ in 0..50 {
            a.tick(0.5);
        }
        assert_eq!(a.level(Bearing::East), 0.0, "snaps to 0 once negligible");
    }

    #[test]
    fn record_saturates_at_the_ceiling() {
        let mut a = BearingActivity::default();
        for _ in 0..100 {
            a.record(Bearing::Center, 1.0);
        }
        assert_eq!(a.level(Bearing::Center), BEARING_MAX, "pinned, not runaway");
        assert!((a.normalized(Bearing::Center) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bearings_decay_independently() {
        let mut a = BearingActivity::default();
        a.record(Bearing::East, 2.0);
        a.record(Bearing::Center, 2.0);
        // Only East keeps getting fed.
        for _ in 0..3 {
            a.record(Bearing::East, 2.0);
            a.tick(0.1);
        }
        assert!(
            a.level(Bearing::East) > a.level(Bearing::Center),
            "the fed bearing reads hotter than the idle one"
        );
        assert_eq!(a.level(Bearing::North), 0.0, "an untouched bearing stays dark");
    }

    #[test]
    fn beat_sync_lands_on_the_east_tracks_bearing() {
        let ev = ServerEvent::BeatSync {
            context_id: ctx(1),
            beat_ref: kaijutsu_audio::BeatRef::new(0.0, 2.0),
        };
        let (b, w) = event_bearing(&ev).expect("beat sync is activity");
        assert_eq!(b, Bearing::East, "the jam warms the tracks bearing");
        assert!(w > 0.0);
    }

    #[test]
    fn block_inserts_are_no_longer_room_ambient_signal() {
        // Freeze-fix slice, 2026-07-11: block/chatter events used to warm the
        // Center bearing for the retired `ConsoleEmblem` placeholder. The
        // real time well now owns chatter/energy entirely through its own
        // richer model — this locks in the drop so a future edit doesn't
        // quietly resurrect a write with no reader (`docs/issues.md`'s
        // now-resolved "`BearingActivity(Center)` has no reader left" entry).
        let block = kaijutsu_types::BlockSnapshot::text(
            BlockId::new(ctx(1), PrincipalId::nil(), 0),
            None,
            kaijutsu_types::Role::User,
            "hello",
        );
        let ev = ServerEvent::BlockInserted {
            context_id: ctx(1),
            block: Box::new(block),
            ops: vec![],
        };
        assert!(event_bearing(&ev).is_none(), "block inserts no longer feed room-ambient activity");
    }

    #[test]
    fn block_status_changes_are_no_longer_room_ambient_signal() {
        let bid = BlockId::new(ctx(1), PrincipalId::nil(), 0);
        let err = ServerEvent::BlockStatusChanged {
            context_id: ctx(1),
            block_id: bid,
            status: Status::Error,
        };
        let done = ServerEvent::BlockStatusChanged {
            context_id: ctx(1),
            block_id: bid,
            status: Status::Done,
        };
        assert!(event_bearing(&err).is_none(), "not even an error status feeds room-ambient activity");
        assert!(event_bearing(&done).is_none());
    }
}

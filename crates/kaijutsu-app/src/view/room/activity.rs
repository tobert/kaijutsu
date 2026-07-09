//! The room's ambient pulse: per-bearing decaying activity fed from the same
//! kernel-wide `ServerEvent` stream the well already ingests
//! (`view/time_well/activity.rs` is the sibling model for the well's rings).
//! "The shell adds renderers, not wire" (`docs/scenes/shell.md`) — this is a
//! thin room-owned consumer that routes events to *bearings* instead of
//! contexts.
//!
//! Two live signals for slice A (everything else stays static LDR):
//! - **beat/track events** ([`ServerEvent::BeatSync`]) → the **East** tracks
//!   bearing, so a jam warms the tracks marker. The rhythmic *breathing* on
//!   top of this comes from the well's beat phasors
//!   ([`super::super::time_well::live::WellBeats::global_envelope`]) read
//!   directly by the glow system — this decaying level is the sustained "a
//!   track is rolling" warmth under the pulse.
//! - **context chatter** (block inserts / text ops / status) → the **Center**
//!   console, so the well emblem glows when contexts are talking (the optional
//!   console-chatter tell in the brief).
//!
//! The math is pure and unit-tested; only [`event_bearing`] touches client
//! types. The Bevy ingest + glow live in [`super`].

use kaijutsu_client::ServerEvent;
use kaijutsu_types::Status;

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
/// room-ambient signal. Beat syncs are the tracks bearing (a clock is rolling);
/// block activity is the console (contexts talking). Weights mirror the well's
/// [`super::super::time_well::activity::event_signal`] so the two ambiences
/// agree on what "loud" means.
pub fn event_bearing(ev: &ServerEvent) -> Option<(Bearing, f32)> {
    match ev {
        ServerEvent::BeatSync { .. } => Some((Bearing::East, 1.0)),
        ServerEvent::BlockTextOps { .. } => Some((Bearing::Center, 1.0)),
        ServerEvent::BlockInserted { .. } => Some((Bearing::Center, 0.8)),
        ServerEvent::BlockOutputChanged { .. } => Some((Bearing::Center, 0.6)),
        ServerEvent::BlockStatusChanged { status, .. } => {
            let w = match status {
                Status::Running => 0.5,
                Status::Error => 0.9,
                _ => 0.2,
            };
            Some((Bearing::Center, w))
        }
        ServerEvent::BlockMetadataChanged { .. } => Some((Bearing::Center, 0.25)),
        ServerEvent::BlockMoved { .. } => Some((Bearing::Center, 0.2)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::BlockId;
    use kaijutsu_types::{ContextId, PrincipalId};

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
    fn block_chatter_lands_on_the_center_console() {
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
        let (b, _) = event_bearing(&ev).expect("a block insert is chatter");
        assert_eq!(b, Bearing::Center, "contexts talking glow the console");
    }

    #[test]
    fn error_status_weighs_more_than_a_plain_flip() {
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
        let (_, ew) = event_bearing(&err).unwrap();
        let (_, dw) = event_bearing(&done).unwrap();
        assert!(ew > dw, "an error is louder than a quiet completion");
    }
}

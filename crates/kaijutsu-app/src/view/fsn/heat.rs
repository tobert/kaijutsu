//! FSN "heat" — per-directory VFS churn, decaying like `room::activity`'s
//! [`super::super::room::activity::BearingActivity`] but keyed by VFS path
//! instead of compass bearing, and ancestor-attenuated so a hot leaf warms
//! its parents too (a storm in `/a/b/c` should read as SOMETHING happening
//! under `/a`, dimmer, even before you dive there).
//!
//! # Where this fits
//!
//! [`ingest_vfs_activity`] (the stitch that joined lane A2 to lane K's wire)
//! drains [`ServerEvent::VfsActivity`] digests off the shared server-event
//! stream into this resource — per-directory entries heat their own paths
//! (ancestors attenuated), and the digest's global total feeds the room's
//! N-bearing archway glow through [`BearingActivity`] (recorded HERE, not in
//! `room::activity::event_bearing` — that fn is stateless, and absolute
//! totals need the baselines this resource owns; see its doc). Consumers:
//! `apply_fsn_lod`'s hue/gain lift, `sync_ship_glow`, the ambient decay
//! tick.
//!
//! # Baseline semantics (why "first sighting" and "total < last" both read 0)
//!
//! The wire signal this expects is a **cumulative total** per path (a digest
//! counter, not a per-event delta) — the same shape `room::activity`'s
//! `BeatSync` event ISN'T (that one already arrives as discrete pulses).
//! [`FsnHeat::observe`] converts "cumulative total" into "delta since I last
//! looked" by tracking `last_seen` per path:
//! - **First sighting** of a path sets the baseline and returns `0.0` — so
//!   connecting to a kernel that already has a large `total` doesn't ignite
//!   a gold storm across the whole tree on the very first frame (there was
//!   no actual *event*, just this client catching up).
//! - **`total < last_seen`** means the counter went backwards — the kernel
//!   restarted (or otherwise reset its own counter) — so this re-baselines
//!   the same way a first sighting would, rather than under/overflowing a
//!   `u64` subtraction into a nonsense multi-exabyte delta.
//! - Otherwise the delta feeds [`scaled_weight`] and the baseline advances.
//!
//! [`FsnHeat::observe_global`] is the same three-way rule against one shared
//! "/" baseline, for a caller that wants a single whole-tree churn signal
//! rather than one per path (e.g. the ship's own crest, `super::scene::
//! sync_ship_glow`).

use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_client::ServerEvent;

use crate::connection::actor_plugin::ServerEventMessage;
use crate::view::room::activity::BearingActivity;
use crate::view::room::bearing::Bearing;

/// Per-path heat ceiling (a churn storm pins here; [`FsnHeat::normalized`]
/// reads the 0..1 fraction). Deliberately smaller headroom than the room's
/// `BEARING_MAX` (3.0) — heat compounds up ancestor chains
/// ([`FsnHeat::record`]), so a lower ceiling keeps a saturated leaf from
/// forcing every ancestor toward saturation too. **Amy-tunable.**
pub const HEAT_MAX: f32 = 6.0;

/// Exponential decay rate (per second) — much slower than the room's
/// `BEARING_DECAY` (2.2/s): a churn storm should *linger* as visible embers
/// for a while after the writes stop, not snap back to cold the instant
/// traffic pauses (the room's bearings track *live* activity; heat tracks
/// *recent history*). **Amy-tunable.**
pub const HEAT_DECAY: f32 = 0.35;

/// Below this a path's heat snaps to zero AND the key is dropped from the
/// map (unlike the room's fixed-size array, this is a HashMap keyed by an
/// unbounded set of VFS paths — a settled path must actually vacate the map,
/// not just sit at a float epsilon forever).
const HEAT_EPSILON: f32 = 1e-3;

/// Per-level falloff applied to every step up the ancestor chain in
/// [`FsnHeat::record`] — `/a/b/c`'s write warms `/a/b` at this fraction,
/// `/a` at its square, `/` at its cube (three levels up from `/a/b/c`).
/// **Amy-tunable.**
pub const HEAT_ANCESTOR_ATTENUATION: f32 = 0.5;

/// How much [`FsnHeat::normalized`] can lift `apply_fsn_lod`'s wireframe
/// gain at full heat (`gain = base_gain * (1.0 + h * HEAT_GAIN_LIFT)`) —
/// mirrors the room's `gain_active` role but as this module's own constant
/// (heat's lift is a fixed multiplier, not a themed palette gain — nothing
/// in `docs/color.md`'s `[scene.gains]` table owns this yet). **Amy-tunable.**
pub const HEAT_GAIN_LIFT: f32 = 0.6;

/// Trim a trailing slash from a digest path, EXCEPT the root itself (`"/"`
/// must stay `"/"`, not become empty) — digest paths may or may not carry
/// one depending on the producer; [`FsnHeat`]'s keys need one canonical
/// form so `"/a/b/"` and `"/a/b"` accumulate into the same entry rather than
/// silently splitting heat across two keys.
pub fn normalize_digest_path(p: &str) -> &str {
    if p == "/" {
        p
    } else {
        p.strip_suffix('/').unwrap_or(p)
    }
}

/// Turn a cumulative counter's delta into a heat weight: `0` for `delta ==
/// 0` (no event, no weight), else `log2(1 + delta) * K` — a log curve so one
/// stray write reads as a small, visible tick (`K` is tuned so `delta == 1`
/// lands around 0.5: a "something happened" nudge, not a flash) while a
/// hundred-file batch doesn't linearly blow straight through [`HEAT_MAX`].
/// **`K` is Amy-tunable** (`HEAT_WEIGHT_K`, below).
pub fn scaled_weight(delta: u64) -> f32 {
    if delta == 0 {
        return 0.0;
    }
    ((1.0 + delta as f64).log2() as f32) * HEAT_WEIGHT_K
}

/// [`scaled_weight`]'s log-curve gain — picked so `delta == 1` scores ≈0.5
/// (`log2(2) == 1.0`, so `K == 0.5` directly). **Amy-tunable.**
pub const HEAT_WEIGHT_K: f32 = 0.5;

/// FSN churn heat: decaying per-path activity plus the cumulative-counter
/// baselines [`observe`](FsnHeat::observe)/[`observe_global`](FsnHeat::observe_global)
/// need to turn a digest's running totals into deltas. Modeled on
/// [`super::super::room::activity::BearingActivity`]'s shape (record + tick +
/// normalized), keyed by VFS path instead of a fixed bearing array — see the
/// module doc for the baseline/restart semantics.
#[derive(Resource, Default)]
pub struct FsnHeat {
    levels: HashMap<String, f32>,
    last_seen: HashMap<String, u64>,
    last_seen_global: Option<u64>,
}

impl FsnHeat {
    /// Convert a path's cumulative digest total into a delta-based weight:
    /// `0.0` on first sighting or a backward-moving total (kernel restart —
    /// both re-baseline rather than compute a delta), else
    /// [`scaled_weight`]'s log curve on `total - last_seen`. Does NOT call
    /// [`record`](Self::record) itself — the caller decides where the
    /// resulting weight lands (a path's own heat, its ancestors, or both);
    /// this fn's only job is the counter-to-delta conversion.
    pub fn observe(&mut self, path: &str, total: u64) -> f32 {
        let path = normalize_digest_path(path);
        let weight = match self.last_seen.get(path) {
            None => 0.0,
            Some(&last) if total < last => 0.0,
            Some(&last) => scaled_weight(total - last),
        };
        self.last_seen.insert(path.to_string(), total);
        weight
    }

    /// [`observe`](Self::observe)'s same three-way rule against one shared
    /// global baseline (no per-path key) — a single whole-tree churn signal.
    pub fn observe_global(&mut self, total: u64) -> f32 {
        let weight = match self.last_seen_global {
            None => 0.0,
            Some(last) if total < last => 0.0,
            Some(last) => scaled_weight(total - last),
        };
        self.last_seen_global = Some(total);
        weight
    }

    /// Inject `w` of heat at `dir`, saturating at [`HEAT_MAX`], AND at every
    /// ancestor up to and including `"/"`, each step attenuated by
    /// [`HEAT_ANCESTOR_ATTENUATION`] raised to its distance from `dir` (one
    /// level up = ×attenuation, two = ×attenuation², …) — a leaf's churn
    /// reads as a dimmer glow radiating up through its parents, so a storm
    /// deep in an unvisited subtree still shows *something* is happening
    /// before the player ever dives there.
    pub fn record(&mut self, dir: &str, w: f32) {
        if w <= 0.0 {
            return;
        }
        let dir = normalize_digest_path(dir);
        self.bump(dir, w);

        let mut current = dir.to_string();
        let mut atten = HEAT_ANCESTOR_ATTENUATION;
        while let Some((parent, _name)) = super::layout::split_parent(&current) {
            self.bump(parent, w * atten);
            current = parent.to_string();
            atten *= HEAT_ANCESTOR_ATTENUATION;
        }
    }

    fn bump(&mut self, path: &str, w: f32) {
        let e = self.levels.entry(path.to_string()).or_insert(0.0);
        *e = (*e + w).min(HEAT_MAX);
    }

    /// Advance time: exponential decay of every tracked path
    /// (frame-rate-independent, mirrors `BearingActivity::tick`), snapping
    /// negligible levels to zero AND dropping the now-zero key — unlike the
    /// room's fixed-size bearing array, this map's key set is unbounded
    /// (every VFS path that ever heated), so a settled path must actually
    /// leave the map rather than accumulate forever as dead epsilon entries.
    pub fn tick(&mut self, dt: f32) {
        let k = (-HEAT_DECAY * dt).exp();
        self.levels.retain(|_, v| {
            *v *= k;
            *v >= HEAT_EPSILON
        });
    }

    /// Raw heat level at `dir` (0.0 if never recorded or since decayed away).
    pub fn level(&self, dir: &str) -> f32 {
        self.levels
            .get(normalize_digest_path(dir))
            .copied()
            .unwrap_or(0.0)
    }

    /// Heat at `dir` normalized to 0..1 by [`HEAT_MAX`] — `apply_fsn_lod`'s
    /// gain-lift and hue-shift multiplier.
    pub fn normalized(&self, dir: &str) -> f32 {
        (self.level(dir) / HEAT_MAX).clamp(0.0, 1.0)
    }
}

/// Ambient decay tick, run every frame regardless of `Screen` — heat should
/// keep cooling even while the player isn't looking at the FSN world (unlike
/// the room's `BearingActivity`, which only matters while the room itself is
/// live), so a stale storm has actually faded by the next dive rather than
/// greeting the player at full brightness from a decay tick that never ran.
/// Registered ungated in `fsn::mod`.
pub fn tick_fsn_heat(mut heat: ResMut<FsnHeat>, time: Res<Time>) {
    heat.tick(time.delta_secs());
}

/// Drain `ServerEvent::VfsActivity` digests into [`FsnHeat`] and the room's
/// N-bearing glow — the stitch between lane K's wire (kernel-native activity
/// digests) and lane A's heat machinery. Registered UNGATED (`fsn::mod`):
/// digests arrive on whatever screen is up, and heat must accumulate while
/// the player sits in the room watching the archway/windows — that's the
/// whole ambient point.
///
/// Own [`MessageReader`] cursor — safe alongside the other
/// `ServerEventMessage` readers (`room::ingest_room_activity`, the time
/// well's live ingest); each reader drains independently.
pub fn ingest_vfs_activity(
    mut events: MessageReader<ServerEventMessage>,
    mut heat: ResMut<FsnHeat>,
    mut bearings: ResMut<BearingActivity>,
) {
    for ServerEventMessage(ev) in events.read() {
        if let ServerEvent::VfsActivity { entries, global_total } = ev {
            let north = apply_digest(
                &mut heat,
                entries.iter().map(|e| (e.path.as_str(), e.total)),
                *global_total,
            );
            if north > 0.0 {
                // North = the DATA HORIZON archway. Recorded here rather
                // than `room::activity::event_bearing` because absolute
                // totals need the baselines FsnHeat owns (that fn's doc).
                bearings.record(Bearing::North, north);
            }
        }
    }
}

/// One digest's worth of updates: every entry (a DIRECTORY path — lane K
/// keys activity by the owning directory) observes its absolute total and,
/// when that yields a real delta, records heat at itself + attenuated
/// ancestors. Returns the whole-tree weight for the caller to route into the
/// room's North bearing (0.0 on a first-sighting/re-baseline digest — no
/// archway flash on connect). Split from the system for direct testing.
fn apply_digest<'a>(
    heat: &mut FsnHeat,
    entries: impl Iterator<Item = (&'a str, u64)>,
    global_total: u64,
) -> f32 {
    for (path, total) in entries {
        let w = heat.observe(path, total);
        if w > 0.0 {
            heat.record(path, w);
        }
    }
    heat.observe_global(global_total)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── observe / observe_global: baseline semantics ──

    #[test]
    fn first_sighting_sets_the_baseline_and_returns_zero() {
        let mut heat = FsnHeat::default();
        assert_eq!(
            heat.observe("/a", 500),
            0.0,
            "no gold storm on first connect"
        );
    }

    #[test]
    fn a_later_observe_scores_the_delta_since_last_seen() {
        let mut heat = FsnHeat::default();
        heat.observe("/a", 100);
        let w = heat.observe("/a", 105);
        assert_eq!(w, scaled_weight(5));
        assert!(w > 0.0);
    }

    #[test]
    fn a_shrinking_total_re_baselines_and_reads_zero() {
        let mut heat = FsnHeat::default();
        heat.observe("/a", 1000);
        // Kernel restarted: its own counter reset lower than what we last saw.
        let w = heat.observe("/a", 3);
        assert_eq!(
            w, 0.0,
            "a backward-moving total must re-baseline, not underflow"
        );
        // The NEXT observe scores against the new (lower) baseline.
        let w2 = heat.observe("/a", 10);
        assert_eq!(w2, scaled_weight(7));
    }

    #[test]
    fn observe_global_has_the_same_three_way_rule() {
        let mut heat = FsnHeat::default();
        assert_eq!(heat.observe_global(50), 0.0, "first sighting");
        assert_eq!(heat.observe_global(60), scaled_weight(10));
        assert_eq!(heat.observe_global(2), 0.0, "restart re-baselines");
    }

    #[test]
    fn observe_paths_track_independent_baselines() {
        let mut heat = FsnHeat::default();
        heat.observe("/a", 10);
        heat.observe("/b", 999);
        assert_eq!(heat.observe("/a", 15), scaled_weight(5));
        assert_eq!(heat.observe("/b", 1000), scaled_weight(1));
    }

    // ── scaled_weight ──

    #[test]
    fn scaled_weight_of_zero_delta_is_zero() {
        assert_eq!(scaled_weight(0), 0.0);
    }

    #[test]
    fn scaled_weight_of_one_delta_is_a_small_visible_nudge() {
        let w = scaled_weight(1);
        assert!(
            w > 0.0 && w < 1.0,
            "delta=1 should read as small-but-visible, got {w}"
        );
    }

    #[test]
    fn scaled_weight_is_monotone_increasing() {
        let a = scaled_weight(1);
        let b = scaled_weight(10);
        let c = scaled_weight(1000);
        assert!(a < b && b < c, "{a} < {b} < {c}");
    }

    #[test]
    fn scaled_weight_is_log_ish_not_linear() {
        // A 100x jump in delta must NOT scale the weight 100x (log, not linear).
        let small = scaled_weight(1);
        let big = scaled_weight(100);
        assert!(
            big < small * 100.0,
            "log curve must sublinearly scale: {small} -> {big}"
        );
    }

    // ── record: saturation + ancestor attenuation ──

    #[test]
    fn record_saturates_at_heat_max() {
        let mut heat = FsnHeat::default();
        for _ in 0..100 {
            heat.record("/a/b", 5.0);
        }
        assert_eq!(heat.level("/a/b"), HEAT_MAX);
    }

    #[test]
    fn record_warms_ancestors_with_attenuation() {
        let mut heat = FsnHeat::default();
        heat.record("/a/b", 1.0);
        let leaf = heat.level("/a/b");
        let mid = heat.level("/a");
        let root = heat.level("/");
        assert_eq!(leaf, 1.0);
        assert!(
            (mid - 1.0 * HEAT_ANCESTOR_ATTENUATION).abs() < 1e-6,
            "mid={mid}"
        );
        assert!(
            (root - 1.0 * HEAT_ANCESTOR_ATTENUATION.powi(2)).abs() < 1e-6,
            "root={root}"
        );
        assert!(
            leaf > mid && mid > root,
            "heat must fade with ancestor distance"
        );
    }

    #[test]
    fn record_at_root_only_warms_root_once() {
        let mut heat = FsnHeat::default();
        heat.record("/", 2.0);
        assert_eq!(heat.level("/"), 2.0);
    }

    #[test]
    fn record_of_a_non_positive_weight_is_a_no_op() {
        let mut heat = FsnHeat::default();
        heat.record("/a", 0.0);
        heat.record("/a", -1.0);
        assert_eq!(heat.level("/a"), 0.0);
    }

    // ── tick: decay-to-zero snap + key removal ──

    #[test]
    fn tick_decays_toward_zero_and_removes_the_key() {
        let mut heat = FsnHeat::default();
        heat.record("/a", 3.0);
        assert!(heat.level("/a") > 0.0);
        for _ in 0..200 {
            heat.tick(0.5);
        }
        assert_eq!(heat.level("/a"), 0.0, "must decay all the way to zero");
        assert!(
            heat.levels.is_empty(),
            "a fully-decayed path must vacate the map"
        );
    }

    #[test]
    fn tick_decays_independently_per_path() {
        let mut heat = FsnHeat::default();
        heat.record("/hot", 5.0);
        heat.record("/cold", 0.1);
        heat.tick(0.1);
        assert!(
            heat.level("/hot") > heat.level("/cold") * 5.0,
            "hot must stay hotter"
        );
    }

    // ── normalized ──

    #[test]
    fn normalized_clamps_to_0_1() {
        let mut heat = FsnHeat::default();
        assert_eq!(heat.normalized("/nope"), 0.0);
        heat.record("/a", HEAT_MAX * 10.0);
        assert!((heat.normalized("/a") - 1.0).abs() < 1e-6);
    }

    // ── normalize_digest_path ──

    #[test]
    fn normalize_digest_path_trims_trailing_slash_except_root() {
        assert_eq!(normalize_digest_path("/"), "/");
        assert_eq!(normalize_digest_path("/a/b/"), "/a/b");
        assert_eq!(normalize_digest_path("/a/b"), "/a/b");
    }

    #[test]
    fn record_and_observe_agree_on_normalized_paths() {
        // A trailing-slash write and a bare-path read must hit the SAME
        // entry, not silently split heat across two keys.
        let mut heat = FsnHeat::default();
        heat.record("/a/b/", 1.0);
        assert_eq!(heat.level("/a/b"), 1.0);
    }

    // ── apply_digest: the lane-K stitch ──

    #[test]
    fn first_digest_baselines_everything_without_heat() {
        let mut heat = FsnHeat::default();
        let north = apply_digest(&mut heat, [("/src", 40_u64), ("/etc", 7)].into_iter(), 47);
        assert_eq!(north, 0.0, "no archway flash on connect");
        assert_eq!(heat.level("/src"), 0.0);
        assert_eq!(heat.level("/"), 0.0, "no ancestor heat either");
    }

    #[test]
    fn second_digest_heats_entries_ancestors_and_north() {
        let mut heat = FsnHeat::default();
        apply_digest(&mut heat, [("/src", 40_u64)].into_iter(), 40);
        let north = apply_digest(&mut heat, [("/src", 45_u64)].into_iter(), 45);

        let w = scaled_weight(5);
        assert_eq!(heat.level("/src"), w, "entry heats at itself (it IS a directory)");
        assert_eq!(
            heat.level("/"),
            w * HEAT_ANCESTOR_ATTENUATION,
            "parent warms attenuated"
        );
        assert_eq!(north, w, "global delta routes to the caller for North");
    }

    #[test]
    fn unchanged_entries_in_a_later_digest_add_no_heat() {
        let mut heat = FsnHeat::default();
        apply_digest(&mut heat, [("/src", 40_u64)].into_iter(), 40);
        apply_digest(&mut heat, [("/src", 45_u64)].into_iter(), 45);
        let before = heat.level("/src");
        // Same absolute total again (e.g. a full-resync digest after
        // reconnect re-sends what we already saw): delta 0, no re-heat.
        let north = apply_digest(&mut heat, [("/src", 45_u64)].into_iter(), 45);
        assert_eq!(heat.level("/src"), before);
        assert_eq!(north, 0.0);
    }
}

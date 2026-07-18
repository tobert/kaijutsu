//! Mouse-wheel scroll sensitivity — a reflected, per-client-configurable
//! resource mirroring the metronome click config (`crate::metronome`,
//! `docs/config-crdt-ownership.md` "Per-client config").
//!
//! Unlike the metronome click (nested inside the `Metronome` resource),
//! `ScrollConfig` is itself the `Resource` and is `Reflect`-registered so it
//! is live-mutable over BRP — a scroll-feel tweak doesn't need a reconnect to
//! try.
//!
//! Fixes a physical-vs-logical DPI bug on the way in: winit's `Pixel`-unit
//! wheel events (Wayland high-res wheels / touchpads) carry PHYSICAL px
//! (bevy_winit passes `absolute.to_physical(scale_factor)`), but downstream
//! `ScrollPosition` consumes LOGICAL px. [`wheel_delta_px`] converts
//! physical→logical (divide by scale factor) before applying the gain, so
//! the pixel path no longer mis-scales on HiDPI.

use bevy::input::mouse::MouseScrollUnit;
use bevy::prelude::*;

/// Per-client scroll gains, resolved from `/etc/client/scroll.toml`
/// (`docs/config-crdt-ownership.md` "Per-client config"). Serde `default`
/// makes every field optional in the TOML — falls back to the shipped
/// gains — so a partial file is valid and a missing/failed fetch keeps the
/// default.
#[derive(Resource, Reflect, serde::Deserialize, Clone, Copy, Debug, PartialEq)]
#[reflect(Resource)]
#[serde(default, deny_unknown_fields)]
pub struct ScrollConfig {
    /// Multiplier for winit `Line`-unit events (notched wheels / X11): px per notch.
    pub line_gain: f32,
    /// Multiplier for winit `Pixel`-unit events (Wayland high-res wheels / touchpads),
    /// applied AFTER converting the physical-px delta to logical px.
    pub pixel_gain: f32,
    /// Exponential easing rate for smooth wheel scrolling (per second). Higher =
    /// snappier glide, lower = floatier. Applied by `view::scroll::smooth_scroll`
    /// when the user has scrolled (follow mode does its own snap). Live-tunable
    /// over BRP like the gains.
    pub smooth_speed: f32,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        // Must match assets/defaults/scroll.toml (the embedded seed).
        Self { line_gain: 40.0, pixel_gain: 3.0, smooth_speed: 45.0 }
    }
}

/// Quantum for the high-res `Pixel` scroll lane, in LOGICAL px — one text row.
/// Sub-quantum motion accumulates and only scrolls once a full row is crossed,
/// which is what makes terminal scrolling feel crisp instead of mushy. A module
/// constant, not a `ScrollConfig` field, on purpose: it's an internal feel unit,
/// not a per-client knob (the two gains stay the only config surface).
pub const PIXEL_QUANTUM_PX: f32 = 20.0;

/// Accumulate `desired` logical-px scroll into `accum` and return the whole-
/// quantum amount to emit now, leaving the sub-quantum remainder in `accum`
/// (sign-preserving, so reversing direction unwinds cleanly). `quantum <= 0.0`
/// disables quantization (pass-through). This is what turns the mushy high-res
/// pixel stream into crisp row-sized steps.
pub fn quantize_step(accum: &mut f32, desired: f32, quantum: f32) -> f32 {
    if quantum <= 0.0 {
        return desired;
    }
    *accum += desired;
    let steps = (*accum / quantum).trunc(); // toward zero → remainder keeps sign
    *accum -= steps * quantum;
    steps * quantum
}

/// Raw winit wheel event → logical-px scroll delta (before the sign flip
/// `dispatch_input` applies). Pure and unit-testable without a Window or a
/// running app — the TDD seam for the gain math.
///
/// `Line` events carry no physical/logical distinction (they're notch
/// counts, not pixels), so `scale_factor` only affects the `Pixel` branch.
pub fn wheel_delta_px(unit: MouseScrollUnit, y: f32, scale_factor: f32, cfg: &ScrollConfig) -> f32 {
    match unit {
        MouseScrollUnit::Line => y * cfg.line_gain,
        MouseScrollUnit::Pixel => (y / scale_factor.max(f32::EPSILON)) * cfg.pixel_gain,
    }
}

/// Apply a per-client `scroll.toml` fetched over RPC (the bootstrap sends it
/// as [`RpcResultMessage::ScrollConfigReceived`][crate::connection::actor_plugin::RpcResultMessage::ScrollConfigReceived],
/// resolved through the `/etc/client/<id>/…` → `/etc/client/…` cascade). A
/// parse failure keeps the current config and logs loudly — never a silent
/// revert to the shipped gains (mirrors `metronome::apply_metronome_config`).
pub fn apply_scroll_config(
    mut results: MessageReader<crate::connection::actor_plugin::RpcResultMessage>,
    mut config: ResMut<ScrollConfig>,
) {
    use crate::connection::actor_plugin::RpcResultMessage;
    for result in results.read() {
        if let RpcResultMessage::ScrollConfigReceived(toml) = result {
            match toml::from_str::<ScrollConfig>(toml) {
                Ok(cfg) => {
                    log::info!("applied scroll config: {cfg:?}");
                    *config = cfg;
                }
                Err(e) => log::error!("scroll.toml is unparseable: {e}; keeping current config"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_events_ignore_scale_factor() {
        let cfg = ScrollConfig::default();
        assert_eq!(wheel_delta_px(MouseScrollUnit::Line, 1.0, 2.0, &cfg), 40.0);
    }

    #[test]
    fn pixel_events_at_1x_scale_apply_only_the_gain() {
        let cfg = ScrollConfig::default();
        assert_eq!(wheel_delta_px(MouseScrollUnit::Pixel, 10.0, 1.0, &cfg), 30.0);
    }

    #[test]
    fn pixel_events_convert_physical_to_logical_before_the_gain() {
        // scale 2.0: 10 physical px -> 5 logical px -> * 3.0 gain = 15.0.
        let cfg = ScrollConfig::default();
        assert_eq!(wheel_delta_px(MouseScrollUnit::Pixel, 10.0, 2.0, &cfg), 15.0);
    }

    #[test]
    fn pixel_path_is_not_a_naive_1_to_1_pass_through() {
        // Guards the original bug: physical px handed straight to a logical
        // consumer with no gain applied at all (unit y == output delta).
        let cfg = ScrollConfig::default();
        assert_ne!(wheel_delta_px(MouseScrollUnit::Pixel, 10.0, 1.0, &cfg), 10.0);
    }

    #[test]
    fn a_typo_is_rejected_rather_than_silently_defaulting() {
        assert!(toml::from_str::<ScrollConfig>("liine_gain = 10.0\n").is_err());
    }

    #[test]
    fn the_shipped_default_parses_to_exactly_the_compiled_in_default() {
        let shipped: ScrollConfig =
            toml::from_str(include_str!("../../../../assets/defaults/scroll.toml"))
                .expect("shipped scroll.toml parses");
        assert_eq!(shipped, ScrollConfig::default(), "seed must match the Default impl");
    }

    #[test]
    fn apply_scroll_config_updates_the_resource() {
        use crate::connection::actor_plugin::RpcResultMessage;

        let mut app = App::new();
        app.init_resource::<ScrollConfig>()
            .add_message::<RpcResultMessage>()
            .add_systems(Update, apply_scroll_config);
        app.world_mut().write_message(RpcResultMessage::ScrollConfigReceived(
            "line_gain = 20.0\npixel_gain = 1.5\n".to_string(),
        ));
        app.update();

        assert_eq!(
            *app.world().resource::<ScrollConfig>(),
            // smooth_speed absent from the TOML → serde fills it from the
            // struct default (45.0), which is the point: partial files merge.
            ScrollConfig { line_gain: 20.0, pixel_gain: 1.5, smooth_speed: 45.0 },
        );
    }

    #[test]
    fn apply_keeps_current_config_on_unparseable_toml() {
        use crate::connection::actor_plugin::RpcResultMessage;

        let mut app = App::new();
        app.init_resource::<ScrollConfig>()
            .add_message::<RpcResultMessage>()
            .add_systems(Update, apply_scroll_config);
        app.world_mut().write_message(RpcResultMessage::ScrollConfigReceived(
            "this is not valid toml =".to_string(),
        ));
        app.update();

        assert_eq!(
            *app.world().resource::<ScrollConfig>(),
            ScrollConfig::default(),
            "a parse failure must not zero out the config",
        );
    }

    #[test]
    fn quantize_step_sub_quantum_accumulates_and_emits_nothing() {
        let mut a = 0.0_f32;
        assert_eq!(quantize_step(&mut a, 12.0, 20.0), 0.0);
        assert_eq!(a, 12.0);
    }

    #[test]
    fn quantize_step_crossing_one_quantum_emits_exactly_one() {
        let mut a = 0.0_f32;
        assert_eq!(quantize_step(&mut a, 12.0, 20.0), 0.0);
        assert_eq!(a, 12.0);
        // 12 + 12 = 24 -> 1 whole quantum (20), remainder 4.
        assert_eq!(quantize_step(&mut a, 12.0, 20.0), 20.0);
        assert_eq!(a, 4.0);
    }

    #[test]
    fn quantize_step_big_flick_emits_multiple_quanta_at_once() {
        let mut a = 0.0_f32;
        // 105 / 20 = 5.25 -> 5 whole quanta (100), remainder 5.
        assert_eq!(quantize_step(&mut a, 105.0, 20.0), 100.0);
        assert_eq!(a, 5.0);
    }

    #[test]
    fn quantize_step_negative_direction_is_sign_preserving() {
        let mut a = 0.0_f32;
        // -25 / 20 = -1.25 -> trunc toward zero = -1 quantum (-20), remainder -5.
        assert_eq!(quantize_step(&mut a, -25.0, 20.0), -20.0);
        assert_eq!(a, -5.0);
    }

    #[test]
    fn quantize_step_reversal_unwinds_the_remainder_before_emitting_again() {
        // Bank 15 (under one quantum, nothing emitted yet)...
        let mut a = 15.0_f32;
        // ...then reverse hard: 15 + (-30) = -15. |-15| < quantum(20), so this
        // reversal only unwinds the banked remainder — it does NOT cross a
        // quantum boundary, so nothing emits yet. (trunc(-15/20) = trunc(-0.75)
        // = -0.0, which is numerically 0.0 — IEEE 754 zero-sign quirk.)
        assert_eq!(quantize_step(&mut a, -30.0, 20.0), 0.0);
        assert_eq!(a, -15.0);
        // The remainder stays within (-quantum, quantum) and keeps the new
        // (negative) sign — one more small negative nudge now crosses.
        assert_eq!(quantize_step(&mut a, -6.0, 20.0), -20.0);
        assert_eq!(a, -1.0);
    }

    #[test]
    fn quantize_step_zero_quantum_is_pass_through() {
        let mut a = 0.0_f32;
        assert_eq!(quantize_step(&mut a, 7.3, 0.0), 7.3);
        // Pass-through never touches accum.
        assert_eq!(a, 0.0);
    }
}

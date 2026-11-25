//! Operational weather glyphs and live HUD telemetry types (`docs/tui.md`).
//!
//! Maps the `vibe` choice options (`/config/kernel/council/vibe.json`) to
//! two-cell emoji glyphs with `Emoji_Presentation` (no `U+FE0F` variation
//! selectors, so terminal and `unicode-width` cell counts agree) and formats
//! probability distributions and token entropies for the TUI HUD.

use serde::{Deserialize, Serialize};

/// Ordered `(option_id, glyph, meaning)` table matching `/config/kernel/council/vibe.json`.
pub const VIBE_OPTIONS: [(&str, &str, &str); 8] = [
    ("flow", "🌊", "routine progress, clear path, cruising"),
    ("probe", "🔍", "investigating, reading code, testing a hypothesis"),
    ("locked", "🎯", "crisp answer, verified fix, nailed it"),
    ("spark", "✨", "neat insight, surprise finding, creative leap"),
    ("knot", "🪢", "gnarly edge case, tight coupling, tricky trade-off"),
    ("fog", "🌀", "uncertain, missing context, ambiguous or guessing"),
    ("spicy", "🔥", "bold, irreversible, or high-blast-radius move"),
    ("tea", "🍵", "calm wrap-up, waiting on human, ready for handoff"),
];

/// Look up the two-cell emoji glyph for a `vibe` option id.
pub fn vibe_glyph(option: &str) -> Option<&'static str> {
    VIBE_OPTIONS
        .iter()
        .find_map(|(id, glyph, _)| (*id == option).then_some(*glyph))
}

/// Format a choice distribution `(option_id, probability)` into one or two
/// weather glyphs:
/// - Returns `None` when no known option reaches `0.25`.
/// - Returns the top glyph alone when `top >= 0.55`, `second < 0.30`, or
///   `top - second > 0.20`.
/// - Returns the top two glyphs side-by-side when split (`second >= 0.30` and
///   `top - second <= 0.20`).
pub fn format_vibe_glyphs<'a>(
    probs: impl IntoIterator<Item = (&'a str, f64)>,
) -> Option<String> {
    let mut ranked: Vec<(&'static str, f64)> = probs
        .into_iter()
        .filter(|(_, p)| p.is_finite() && *p > 0.0)
        .filter_map(|(id, p)| vibe_glyph(id).map(|g| (g, p)))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let &(top_glyph, top_p) = ranked.first()?;
    if top_p < 0.25 {
        return None;
    }
    if let Some(&(second_glyph, second_p)) = ranked.get(1)
        && top_p < 0.55
        && second_p >= 0.30
        && (top_p - second_p) <= 0.20
        && second_glyph != top_glyph
    {
        return Some(format!("{top_glyph}{second_glyph}"));
    }
    Some(top_glyph.to_string())
}

const SPARK_BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Quantize a slice of token entropies (in nats, clamped to `0.0..=3.0`) into a
/// compact sparkline of up to `max_bars` block glyphs (`▁`..`█`).
pub fn format_entropy_spark(entropies: &[f32], max_bars: usize) -> Option<String> {
    if entropies.is_empty() || max_bars == 0 {
        return None;
    }
    let tail = &entropies[entropies.len().saturating_sub(max_bars)..];
    let mut out = String::with_capacity(tail.len() * 3);
    for &h in tail {
        let clamped = if h.is_finite() { h.clamp(0.0, 3.0) } else { 0.0 };
        let idx = ((clamped / 3.0) * 7.0).round() as usize;
        out.push(SPARK_BARS[idx.min(7)]);
    }
    Some(out)
}

/// Live HUD telemetry and posture for one context (`docs/tui.md`).
///
/// Carried on `ContextHandleInfo` so the TUI's separator strip and status row
/// render council telemetry, gate activity, refusal holds, and posture without
/// per-context RPC loops. Every field defaults to its empty/off state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextHud {
    /// Bound model's backend speaks Council v2 (`BackendKind::Council`).
    pub is_council_backend: bool,
    /// Last recorded turn was a policy refusal (`HELD`).
    pub held_refusal: bool,
    /// Active residual control vectors (`context_controls` rows).
    pub control_count: u32,
    /// Leaf name of the bound `--decide` program when set (e.g. `"haiku.json"`).
    pub decide_program: Option<String>,
    /// Short name of the active gate judge when configured for this context
    /// (e.g. `"shell-gate"`, `"gpt-6-luna"`, `"jev-1.13"`).
    pub gate_label: Option<String>,
    /// Consecutive `seat_bump` streak on this context within the active window.
    pub bump_streak: u32,
    /// Configured bump limit or streak escalation threshold, when known (`0` = none).
    pub bump_limit: u32,
    /// Flavor of the most recent bump in the active streak (e.g. `"network"`).
    pub last_bump_flavor: Option<String>,
    /// Tool name of a gate model call currently in flight on this context.
    pub judging_tool: Option<String>,
    /// Unix-epoch milliseconds when `judging_tool` started (`None` when not judging).
    pub judging_started_at: Option<u64>,
    /// Latest operational weather emoji(s) from a Council v2 riding decision or
    /// hosted Decisions judge (e.g. `"🌊"` or `"🔍🪢"`).
    pub vibe: Option<String>,
    /// Council v2 streaming phase while a generation is open (`"thinking"`, `"writing"`, `"calling"`).
    pub council_phase: Option<String>,
    /// Latest Council v2 generation speed in tokens/sec (`None` when unknown).
    pub council_tps: Option<f32>,
    /// Latest Council v2 time-to-first-token in milliseconds (`None` when unknown).
    pub council_ttft_ms: Option<u32>,
    /// Rolling token-entropy sparkline (e.g. `"▂▃▅▂"`) or mean entropy label from Council v2.
    pub council_entropy: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vibe_glyph_is_single_codepoint_without_variation_selector() {
        for (id, expected, _) in VIBE_OPTIONS {
            let glyph = vibe_glyph(id).expect(id);
            assert_eq!(glyph, expected);
            assert_eq!(glyph.chars().count(), 1, "{id} glyph must have no U+FE0F");
        }
        assert_eq!(vibe_glyph("unknown"), None);
    }

    #[test]
    fn confident_vibe_picks_one_glyph_and_split_vibe_picks_two() {
        assert_eq!(
            format_vibe_glyphs([("flow", 0.72), ("probe", 0.18), ("tea", 0.10)]),
            Some("🌊".to_string())
        );
        assert_eq!(
            format_vibe_glyphs([("probe", 0.44), ("knot", 0.36), ("fog", 0.20)]),
            Some("🔍🪢".to_string())
        );
        // Flat noise where nothing reaches 0.25 shows nothing.
        assert_eq!(
            format_vibe_glyphs([
                ("flow", 0.125),
                ("probe", 0.125),
                ("locked", 0.125),
                ("spark", 0.125),
                ("knot", 0.125),
                ("fog", 0.125),
                ("spicy", 0.125),
                ("tea", 0.125),
            ]),
            None
        );
    }

    #[test]
    fn entropy_spark_quantizes_tail_into_eight_levels() {
        assert_eq!(format_entropy_spark(&[], 4), None);
        assert_eq!(
            format_entropy_spark(&[0.0, 1.0, 2.0, 3.0], 4),
            Some("▁▃▆█".to_string())
        );
        // Keeps the newest `max_bars` samples.
        assert_eq!(
            format_entropy_spark(&[3.0, 3.0, 0.0, 0.0, 3.0, 0.0], 4),
            Some("▁▁█▁".to_string())
        );
    }
}

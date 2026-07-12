//! TOML-based theme parser for Kaijutsu.
//!
//! The theme arrives over RPC from the kernel's CRDT-owned config (see
//! `connection/actor_plugin.rs`); this module only parses the TOML payload.
//! There is no host-side theme file and no disk-load path — the kernel is
//! the sole owner of theme configuration.
//!
//! One payload, two color lanes (docs/color.md): [`parse_theme_data`] yields
//! the raw `ThemeData`, from which the caller derives both the UI `Theme`
//! and the 3D `ScenePalette`.
//!
//! Theme values are plain hex strings and numbers — no computed colors.

use kaijutsu_types::theme::ThemeData;

/// Parse a TOML string into `ThemeData` — the single parse both color lanes
/// derive from (`Theme::from` for the UI lane, `ScenePalette::from_scene_data`
/// for the scene lane).
pub fn parse_theme_data(content: &str) -> Result<ThemeData, String> {
    toml::from_str(content).map_err(|e| format!("TOML parse error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Theme;

    const DEFAULT_THEME_TOML: &str = include_str!("../../../../assets/defaults/theme.toml");

    #[test]
    fn test_default_theme_toml_parses() {
        let theme = Theme::from(parse_theme_data(DEFAULT_THEME_TOML).unwrap());
        // The shipped file must agree with ThemeData::default() (the
        // data-layer defaults; kaijutsu-types pins the full equality).
        // Compared through the data layer rather than ui::Theme::default()
        // so a deliberate compiled-fallback divergence can't hide a
        // file/data drift.
        let srgba = theme.bg.to_srgba();
        let expected = Theme::from(ThemeData::default()).bg.to_srgba();
        assert!((srgba.red - expected.red).abs() < 0.01);
    }

    #[test]
    fn test_font_rainbow_param() {
        // Default is true
        let td: ThemeData = parse_theme_data(DEFAULT_THEME_TOML).unwrap();
        assert!(td.font_rainbow);

        // Override to false
        let modified = DEFAULT_THEME_TOML.replace("font_rainbow = true", "font_rainbow = false");
        let theme = Theme::from(parse_theme_data(&modified).unwrap());
        assert!(!theme.font_rainbow);
    }

    #[test]
    fn test_invalid_toml_returns_error() {
        let result = parse_theme_data("[invalid");
        assert!(result.is_err());
    }
}

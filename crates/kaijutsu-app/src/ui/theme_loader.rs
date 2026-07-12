//! TOML-based theme parser for Kaijutsu.
//!
//! The theme arrives over RPC from the kernel's CRDT-owned config (see
//! `connection/actor_plugin.rs`); this module only parses the TOML payload
//! into a `Theme`. There is no host-side theme file and no disk-load path —
//! the kernel is the sole owner of theme configuration.
//!
//! Theme values are plain hex strings and numbers — no computed colors.

use kaijutsu_types::theme::ThemeData;

use super::theme::Theme;

/// Parse a TOML string into a Theme.
pub fn parse_theme_toml(content: &str) -> Result<Theme, String> {
    let theme_data: ThemeData =
        toml::from_str(content).map_err(|e| format!("TOML parse error: {e}"))?;
    Ok(Theme::from(theme_data))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_THEME_TOML: &str = include_str!("../../../../assets/defaults/theme.toml");

    #[test]
    fn test_default_theme_toml_parses() {
        let theme = parse_theme_toml(DEFAULT_THEME_TOML).unwrap();
        // bg should be close to #1a1b26
        let srgba = theme.bg.to_srgba();
        assert!((srgba.red - 0.102).abs() < 0.01);
    }

    #[test]
    fn test_font_rainbow_param() {
        // Default is true
        let td: ThemeData = toml::from_str(DEFAULT_THEME_TOML).unwrap();
        assert!(td.font_rainbow);

        // Override to false
        let modified = DEFAULT_THEME_TOML.replace("font_rainbow = true", "font_rainbow = false");
        let theme = parse_theme_toml(&modified).unwrap();
        assert!(!theme.font_rainbow);
    }

    #[test]
    fn test_invalid_toml_returns_error() {
        let result = parse_theme_toml("[invalid");
        assert!(result.is_err());
    }
}

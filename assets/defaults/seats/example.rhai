// Seat-Specific Configuration Template
//
// Copy this file to {your-seat-id}.rhai to customize settings
// for a specific computer/display.
//
// Seat ID is typically your hostname (e.g., "amy-desktop", "amy-laptop").
//
// Values here OVERRIDE the base theme.rhai - only include values
// you want to change. Unset values inherit from base.
//
// Available functions:
//   hex("#rrggbb")        -> [r, g, b, 1.0]
//   hexa("#rrggbb", a)    -> [r, g, b, a]
//   rgba(r, g, b, a)      -> [r, g, b, a]  (values 0.0-1.0)
//   rgb(r, g, b)          -> [r, g, b, 1.0]

// ============================================================================
// Display Settings (common seat overrides)
// ============================================================================

// For high-DPI displays (4K, Retina):
// let font_stem_darkening = 0.10;
// let frame_content_padding = 16.0;
// let hud_font_size = 14.0;

// For lower resolution displays:
// let font_stem_darkening = 0.20;
// let frame_content_padding = 10.0;
// let hud_font_size = 11.0;

// ============================================================================
// Layout Adjustments
// ============================================================================

// Different input area sizes per screen
// let input_docked_height = 250.0;
// let input_overlay_width_pct = 0.7;

// ============================================================================
// Accessibility
// ============================================================================

// Higher contrast text
// let fg = hex("#ffffff");
// let fg_dim = hex("#8090a0");

// More visible cursor
// let cursor_normal = rgba(1.0, 1.0, 0.0, 1.0);
// let cursor_insert = rgba(0.0, 1.0, 0.5, 1.0);

// Disable effects for performance
// let effect_glow_intensity = 0.0;
// let effect_breathe_amplitude = 0.0;
// let font_glow_intensity = 0.0;

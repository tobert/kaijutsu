// Kaijutsu Default Theme
//
// This file defines the visual appearance of the Kaijutsu client.
// Edit this file to customize colors, fonts, and effects.
//
// Changes require an app restart to take effect (for now).
// Seat-specific overrides go in seats/{seat_id}.rhai
//
// Available functions:
//   hex("#rrggbb")        -> [r, g, b, 1.0]
//   hexa("#rrggbb", a)    -> [r, g, b, a]
//   rgba(r, g, b, a)      -> [r, g, b, a]  (values 0.0-1.0)
//   rgb(r, g, b)          -> [r, g, b, 1.0]

// ============================================================================
// Base UI Colors
// ============================================================================

let bg = hex("#1a1b26");                    // Main background
let panel_bg = hexa("#1a1b26", 0.95);       // Panel backgrounds (semi-transparent)
let fg = hex("#c0caf5");                    // Main text
let fg_dim = hex("#565f89");                // Secondary/muted text
let accent = hex("#7aa2f7");                // Primary accent (links, focus)
let accent2 = hex("#9ece6a");               // Success/positive
let border = hex("#3b4261");                // Border color
let selection_bg = hexa("#7aa2f7", 0.30);   // Selection background

// Row type colors (conversation left borders)
let row_tool = hex("#bb9af7");              // Tool calls
let row_result = hex("#e0af68");            // Tool results

// ============================================================================
// Semantic Colors
// ============================================================================

let error = hex("#f7768e");                 // Error/negative
let warning = hex("#e0af68");               // Warning
let success = hex("#9ece6a");               // Success

// ============================================================================
// Mode Colors (vim-style indicator text)
// ============================================================================

let mode_normal = hex("#7aa2f7");           // Normal mode
let mode_chat = hex("#9ece6a");             // Chat/insert mode (also accepts mode_insert)
let mode_shell = hex("#e0af68");            // Shell/command mode
let mode_visual = hex("#bb9af7");           // Visual selection mode

// ============================================================================
// Cursor Colors (Vec4: [r, g, b, a])
// ============================================================================

let cursor_normal = rgba(0.48, 0.64, 0.97, 0.8);   // Block cursor in normal mode
let cursor_insert = rgba(0.62, 0.81, 0.42, 0.9);   // Beam cursor in insert mode
let cursor_visual = rgba(0.73, 0.60, 0.97, 0.7);   // Block cursor in visual mode

// ============================================================================
// ANSI Terminal Colors
// ============================================================================

// Standard colors
let ansi_black = hex("#15161e");
let ansi_red = hex("#f7768e");
let ansi_green = hex("#9ece6a");
let ansi_yellow = hex("#e0af68");
let ansi_blue = hex("#7aa2f7");
let ansi_magenta = hex("#bb9af7");
let ansi_cyan = hex("#7dcfff");
let ansi_white = hex("#a9b1d6");

// Bright variants
let ansi_bright_black = hex("#414868");
let ansi_bright_red = hex("#f7768e");
let ansi_bright_green = hex("#9ece6a");
let ansi_bright_yellow = hex("#e0af68");
let ansi_bright_blue = hex("#7aa2f7");
let ansi_bright_magenta = hex("#bb9af7");
let ansi_bright_cyan = hex("#7dcfff");
let ansi_bright_white = hex("#c0caf5");

// ============================================================================
// Frame Configuration
// ============================================================================

// Frame structure
let frame_corner_size = 16.0;               // Corner cut size
let frame_edge_thickness = 2.0;             // Border thickness
let frame_content_padding = 12.0;           // Inner padding

// Frame colors (with alpha)
let frame_base = hexa("#1a1b26", 0.95);
let frame_focused = hexa("#7aa2f7", 0.15);
let frame_insert = hexa("#9ece6a", 0.12);
let frame_visual = hexa("#bb9af7", 0.12);
let frame_unfocused = hexa("#1a1b26", 0.80);
let frame_edge = hexa("#3b4261", 0.6);

// Frame shader params [intensity, _, _, _]
let frame_params_base = rgba(0.0, 0.0, 0.0, 0.0);
let frame_params_focused = rgba(0.3, 0.0, 0.0, 0.0);
let frame_params_unfocused = rgba(0.0, 0.0, 0.0, 0.0);

// Edge dimming [r, g, b, a] multipliers
let frame_edge_dim_unfocused = rgba(0.5, 0.5, 0.5, 0.6);
let frame_edge_dim_focused = rgba(1.0, 1.0, 1.0, 1.0);

// ============================================================================
// Shader Effect Parameters
// ============================================================================

// Glow effect
let effect_glow_radius = 4.0;
let effect_glow_intensity = 0.3;
let effect_glow_falloff = 2.0;

// Sheen effect (subtle shine)
let effect_sheen_speed = 0.5;
let effect_sheen_sparkle_threshold = 0.95;

// Breathing effect (subtle pulse)
let effect_breathe_speed = 1.0;
let effect_breathe_amplitude = 0.05;

// Chasing border effect
let effect_chase_speed = 2.0;
let effect_chase_width = 0.15;
let effect_chase_glow_radius = 8.0;
let effect_chase_glow_intensity = 0.5;
let effect_chase_color_cycle = 0.0;

// ============================================================================
// Input Area Configuration
// ============================================================================

let input_minimized_height = 48.0;
let input_docked_height = 200.0;
let input_overlay_width_pct = 0.6;
let input_backdrop_color = hexa("#1a1b26", 0.85);

// ============================================================================
// Font Rendering Quality (MSDF)
// ============================================================================

let font_stem_darkening = 0.15;             // Stronger stems at small sizes
let font_hint_amount = 0.3;                 // Hinting strength
let font_taa_enabled = true;                // Temporal AA
let font_horz_scale = 1.0;                  // Horizontal stretch
let font_vert_scale = 1.0;                  // Vertical stretch
let font_text_bias = 0.5;                   // SDF threshold bias

// Font effects
let font_glow_intensity = 0.0;              // Glow disabled by default
let font_glow_spread = 2.0;
let font_glow_color = rgba(0.48, 0.64, 0.97, 0.5);
let font_rainbow = false;                   // Rainbow text effect

// ============================================================================
// Constellation View (Agent Visualization)
// ============================================================================

let constellation_layout_radius = 200.0;
let constellation_node_size = 24.0;
let constellation_node_size_focused = 32.0;
let constellation_node_glow_idle = hexa("#565f89", 0.3);
let constellation_node_glow_active = hexa("#7aa2f7", 0.6);
let constellation_node_glow_streaming = hexa("#9ece6a", 0.8);
let constellation_node_glow_error = hexa("#f7768e", 0.7);
let constellation_connection_glow = 0.3;
let constellation_connection_color = hexa("#3b4261", 0.5);
let constellation_particle_budget = 1000;
let constellation_orbital_speed = 0.5;

// ============================================================================
// HUD Configuration
// ============================================================================

let hud_panel_bg = hexa("#1a1b26", 0.85);
let hud_panel_glow = hexa("#7aa2f7", 0.2);
let hud_panel_glow_intensity = 0.3;
let hud_orbital_curve_radius = 300.0;
let hud_text_color = hex("#c0caf5");
let hud_font_size = 12.0;

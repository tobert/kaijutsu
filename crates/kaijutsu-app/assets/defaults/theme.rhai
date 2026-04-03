// Kaijutsu theme — managed by Kaijutsu, do not edit directly.
//
// This file is written by Kaijutsu on first launch. Direct edits will be
// preserved across updates (unlike bindings.rhai which is regenerated).
// The built-in defaults apply for any variable not set here.
//
// Loaded from ~/.config/kaijutsu/theme.rhai on startup.
//
// ## Color API (kaijutsu-rhai stdlib)
//
//   hex("#rrggbb")              // Validate and normalize hex color
//   hexa("#rrggbb", alpha)      // Hex color with alpha (0.0–1.0)
//   rgb(r, g, b)                // RGB to hex (0–255 per channel)
//   rgba(r, g, b, a)            // RGB+alpha to hex (r/g/b: 0–255, a: 0.0–1.0)
//   hsl(h, s, l)                // HSL to hex (h: 0–360, s/l: 0–100)
//   oklch(l, c, h)              // Oklch to hex (perceptually uniform)
//   color_mix(hex1, hex2, t)    // Mix in Oklab space
//   color_lighten(hex, amount)  // Lighten in Oklch
//   color_darken(hex, amount)   // Darken in Oklch
//   hue_shift(hex, degrees)     // Shift hue in Oklch
//
// Math functions also available: sin, cos, lerp, clamp, PI(), etc.
//
// ## Module import
//
//   import "palette" as p;      // Load ~/.config/kaijutsu/palette.rhai
//   let bg = p::background;     // Reference vars from palette
//
// ## Available variables (all optional — omit to use built-in defaults)
//
//   bg, fg, fg_dim              // Base text and background colors
//   panel_bg, selection_bg      // Panel and selection backgrounds (support alpha)
//   accent, accent2             // Accent/highlight colors
//   border                      // Default border color
//   row_tool, row_result        // Tool call / result row backgrounds
//   error, warning, success     // Semantic status colors
//   mode_normal, mode_insert, mode_chat, mode_shell, mode_visual  // Mode indicator colors
//   mode_label_normal, mode_label_insert, mode_label_visual       // Dock HUD label text
//   mode_label_shell, mode_label_constellation, mode_label_stack  // (Rhai-scriptable)
//   mode_label_input
//   cursor_normal, cursor_insert, cursor_visual      // Cursor colors (Vec4)
//
//   // Block borders
//   block_border_tool_call, block_border_tool_result
//   block_border_error, block_border_thinking, block_border_drift
//   block_border_user, block_border_assistant
//   block_border_thickness, block_border_corner_radius
//   block_border_glow_radius, block_border_glow_intensity, block_border_padding
//   text_glow_radius, text_glow_color
//
//   // Constellation
//   constellation_base_radius, constellation_ring_spacing
//   constellation_node_size, constellation_node_size_focused
//   constellation_node_glow_idle, constellation_node_glow_active
//   constellation_node_glow_streaming, constellation_node_glow_error
//   constellation_connection_glow, constellation_connection_color
//   constellation_particle_budget
//
//   // Frame/pane borders
//   frame_corner_size, frame_edge_thickness, frame_content_padding
//   frame_base, frame_focused, frame_insert, frame_visual
//   frame_unfocused, frame_edge
//
//   // Input overlay
//   input_minimized_height, input_docked_height, input_overlay_width_pct
//   input_backdrop_color, compose_border, compose_bg, modal_backdrop
//
//   // Effects (shader params)
//   effect_glow_radius, effect_glow_intensity, effect_glow_falloff
//   effect_sheen_speed, effect_sheen_sparkle_threshold
//   effect_breathe_speed, effect_breathe_amplitude
//   effect_chase_speed, effect_chase_width, effect_chase_glow_radius
//   effect_chase_glow_intensity, effect_chase_color_cycle
//
//   // ANSI palette
//   ansi_black, ansi_red, ansi_green, ansi_yellow
//   ansi_blue, ansi_magenta, ansi_cyan, ansi_white
//   ansi_bright_black, ansi_bright_red, ansi_bright_green, ansi_bright_yellow
//   ansi_bright_blue, ansi_bright_magenta, ansi_bright_cyan, ansi_bright_white
//
//   // Layout spacing
//   indent_width                // Horizontal indent per nesting level (px, default 24)
//   role_header_height          // Height for role divider lines (px, default 20)
//   role_header_spacing         // Gap below role dividers (px, default 4)
//   label_font_size             // Border label font size (px, default 11)
//   label_inset                 // Label horizontal inset from edge (px, default 12)
//   label_pad                   // Padding around label text (px, default 6)
//
//   // Font effects
//   font_rainbow                // Enable rainbow text effect (bool)

// Leave this file empty to use all built-in defaults, or override variables:
//
// Example (Tokyo Night):
//   let bg = hex("#1a1b26");
//   let fg = hex("#c0caf5");
//   let accent = hex("#7aa2f7");

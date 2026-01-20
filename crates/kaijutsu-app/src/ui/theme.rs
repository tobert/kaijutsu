use bevy::prelude::*;

#[derive(Resource)]
pub struct Theme {
    pub bg: Color,
    pub panel_bg: Color,
    pub fg_dim: Color,
    pub accent: Color,
    pub accent2: Color,
    pub border: Color,
    // Row type colors
    pub row_tool: Color,
    pub row_result: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg: Color::srgb(0.05, 0.07, 0.09),
            panel_bg: Color::srgba(0.05, 0.07, 0.09, 0.9),
            fg_dim: Color::srgb(0.5, 0.5, 0.5),
            accent: Color::srgb(0.34, 0.65, 1.0),
            accent2: Color::srgb(0.97, 0.47, 0.73),
            border: Color::srgb(0.19, 0.21, 0.24),
            // Row type colors - left border accents
            row_tool: Color::srgb(0.83, 0.6, 0.13),    // Orange - tool calls
            row_result: Color::srgb(0.25, 0.73, 0.31), // Green - tool results
        }
    }
}

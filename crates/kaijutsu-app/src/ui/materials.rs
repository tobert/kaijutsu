//! Material cache for pre-created material handles
//!
//! Panel builders need access to materials but can't get mutable access to
//! asset storage during spawn (as they run inside Commands closures).
//! This resource holds pre-created handles that builders can clone.

use bevy::prelude::*;

use crate::shaders::nine_slice::ChasingBorderMaterial;
use crate::ui::theme::Theme;

/// Pre-created material handles for use by panel builders.
///
/// Initialized at startup with materials from the theme.
/// Panel builders clone handles from here instead of creating new materials.
#[derive(Resource)]
pub struct MaterialCache {
    /// Standard chasing border for dashboard columns
    pub chasing_border: Handle<ChasingBorderMaterial>,
}

/// Setup system that creates the MaterialCache resource.
///
/// Must run before LayoutPlugin so builders can access materials.
pub fn setup_material_cache(
    mut commands: Commands,
    mut materials: ResMut<Assets<ChasingBorderMaterial>>,
    theme: Res<Theme>,
) {
    let chasing_border = materials.add(
        ChasingBorderMaterial::from_theme(theme.accent, Color::WHITE)
            .with_thickness(1.0)
            .with_glow(
                theme.effect_chase_glow_radius,
                theme.effect_chase_glow_intensity,
            )
            .with_chase_speed(theme.effect_chase_speed)
            .with_chase_width(theme.effect_chase_width)
            .with_color_cycle(theme.effect_chase_color_cycle),
    );

    commands.insert_resource(MaterialCache { chasing_border });

    info!("MaterialCache initialized with chasing border material");
}

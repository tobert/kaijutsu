//! Mini-render system for constellation node previews
//!
//! Renders simplified context previews to textures for display in constellation nodes.
//! Each context gets its own render-to-texture camera that produces a small preview
//! showing context info and activity state.
//!
//! Currently disabled — card nodes don't use render-to-texture previews.
//! Will be resurrected when MSDF text content preview is added inside cards.

#![allow(dead_code)]

use bevy::{
    camera::RenderTarget,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
};

use super::{ActivityState, Constellation, ContextNode};
use crate::ui::theme::Theme;

/// Size of mini-render textures (square)
pub const MINI_RENDER_SIZE: u32 = 256;

/// Component marking a mini-render camera
#[derive(Component)]
pub struct MiniRenderCamera;

/// Component marking a mini-render root UI node
#[derive(Component)]
pub struct MiniRenderRoot {
    /// Context ID this root belongs to
    pub context_id: String,
}

/// Resource tracking mini-render state
#[derive(Resource, Default)]
pub struct MiniRenderRegistry {
    /// Map from context_id to (camera_entity, root_entity, image_handle)
    pub renders: Vec<MiniRenderEntry>,
}

/// Entry in the mini-render registry
pub struct MiniRenderEntry {
    pub context_id: String,
    pub camera_entity: Entity,
    pub root_entity: Entity,
    pub image: Handle<Image>,
}

/// Setup mini-render systems
pub fn setup_mini_render_systems(app: &mut App) {
    app.init_resource::<MiniRenderRegistry>().add_systems(
        Update,
        (
            spawn_mini_renders,
            update_mini_render_content,
            cleanup_removed_mini_renders,
        )
            .chain(),
    );
}

/// Create a render target image for mini-renders
fn create_render_target(images: &mut Assets<Image>) -> Handle<Image> {
    let size = Extent3d {
        width: MINI_RENDER_SIZE,
        height: MINI_RENDER_SIZE,
        ..default()
    };

    // Create an image configured as a render target
    let mut image = Image::new_fill(
        size,
        TextureDimension::D2,
        &[0, 0, 0, 0], // Transparent black
        TextureFormat::Bgra8UnormSrgb,
        bevy::asset::RenderAssetUsages::default(),
    );

    // Enable render target usage
    image.texture_descriptor.usage =
        TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::RENDER_ATTACHMENT;

    images.add(image)
}

/// Spawn mini-render cameras and UI roots for new constellation nodes
fn spawn_mini_renders(
    mut commands: Commands,
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    mut images: ResMut<Assets<Image>>,
    mut registry: ResMut<MiniRenderRegistry>,
) {
    // Find contexts that need mini-renders
    for ctx_node in &constellation.nodes {
        // Skip if already has mini-render
        if registry
            .renders
            .iter()
            .any(|r| r.context_id == ctx_node.context_id)
        {
            continue;
        }

        // Create render target texture
        let image_handle = create_render_target(&mut images);

        // Spawn mini-render camera (renders before main camera)
        let camera_entity = commands
            .spawn((
                Camera2d,
                Camera {
                    order: -10, // Render well before main camera
                    clear_color: ClearColorConfig::Custom(theme.panel_bg.with_alpha(0.9)),
                    ..default()
                },
                RenderTarget::Image(image_handle.clone().into()),
                MiniRenderCamera,
            ))
            .id();

        // Spawn UI root that renders to this camera
        let root_entity = spawn_mini_render_ui(
            &mut commands,
            camera_entity,
            ctx_node,
            &theme,
        );

        // Register the mini-render
        registry.renders.push(MiniRenderEntry {
            context_id: ctx_node.context_id.clone(),
            camera_entity,
            root_entity,
            image: image_handle.clone(),
        });

        info!(
            "Spawned mini-render for context: {}",
            ctx_node.context_id
        );
    }
}

/// Spawn the UI content for a mini-render
fn spawn_mini_render_ui(
    commands: &mut Commands,
    camera_entity: Entity,
    ctx_node: &ContextNode,
    theme: &Theme,
) -> Entity {
    let context_name = &ctx_node.context_id;
    let activity_color = activity_to_mini_color(ctx_node.activity, theme);

    commands
        .spawn((
            MiniRenderRoot {
                context_id: ctx_node.context_id.clone(),
            },
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                padding: UiRect::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(theme.panel_bg.with_alpha(0.0)), // Transparent, camera clears
            UiTargetCamera(camera_entity),
        ))
        .with_children(|parent| {
            // Activity indicator dot
            parent.spawn((
                Node {
                    width: Val::Px(16.0),
                    height: Val::Px(16.0),
                    margin: UiRect::bottom(Val::Px(8.0)),
                    border_radius: BorderRadius::all(Val::Px(8.0)),
                    ..default()
                },
                BackgroundColor(activity_color),
            ));

            // Context name
            parent.spawn((
                Text::new(truncate_name(context_name, 12)),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(theme.fg),
            ));

            // Activity state label
            parent.spawn((
                Text::new(activity_label(ctx_node.activity)),
                TextFont {
                    font_size: 10.0,
                    ..default()
                },
                TextColor(theme.fg_dim),
            ));

            // Joined indicator (replaces owner nick badge)
            if ctx_node.joined {
                parent.spawn((
                    Text::new("joined"),
                    TextFont {
                        font_size: 9.0,
                        ..default()
                    },
                    TextColor(theme.fg_dim.with_alpha(0.7)),
                    Node {
                        margin: UiRect::top(Val::Px(4.0)),
                        ..default()
                    },
                ));
            }
        })
        .id()
}

/// Update mini-render content when activity changes
fn update_mini_render_content(
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    _registry: Res<MiniRenderRegistry>, // Reserved for future content updates
    mut mini_roots: Query<(&MiniRenderRoot, &Children)>,
    mut background_colors: Query<&mut BackgroundColor>,
    mut texts: Query<&mut Text>,
    mut text_colors: Query<&mut TextColor>,
) {
    if !constellation.is_changed() {
        return;
    }

    for (root, children) in mini_roots.iter_mut() {
        let Some(ctx_node) = constellation.node_by_id(&root.context_id) else {
            continue;
        };

        let activity_color = activity_to_mini_color(ctx_node.activity, &theme);

        // Update children (activity dot, name, state label, owner)
        // Child order: [activity_dot, name, state_label, owner]
        let mut child_iter = children.iter();

        // Activity dot (first child)
        if let Some(dot_entity) = child_iter.next() {
            if let Ok(mut bg) = background_colors.get_mut(dot_entity) {
                bg.0 = activity_color;
            }
        }

        // Skip name (doesn't change)
        child_iter.next();

        // Activity state label (third child)
        if let Some(label_entity) = child_iter.next() {
            if let Ok(mut text) = texts.get_mut(label_entity) {
                *text = Text::new(activity_label(ctx_node.activity));
            }
            // Update color based on activity
            if let Ok(mut color) = text_colors.get_mut(label_entity) {
                color.0 = match ctx_node.activity {
                    ActivityState::Error => theme.accent2,
                    ActivityState::Streaming => theme.accent,
                    _ => theme.fg_dim,
                };
            }
        }
    }
}

/// Clean up mini-renders for removed contexts
fn cleanup_removed_mini_renders(
    mut commands: Commands,
    constellation: Res<Constellation>,
    mut registry: ResMut<MiniRenderRegistry>,
) {
    // Find and remove renders for contexts that no longer exist
    let to_remove: Vec<String> = registry
        .renders
        .iter()
        .filter(|r| constellation.node_by_id(&r.context_id).is_none())
        .map(|r| r.context_id.clone())
        .collect();

    for context_id in to_remove {
        if let Some(idx) = registry.renders.iter().position(|r| r.context_id == context_id) {
            let entry = registry.renders.remove(idx);
            commands.entity(entry.camera_entity).despawn();
            commands.entity(entry.root_entity).despawn(); // Bevy 0.18: despawn handles children
            info!("Cleaned up mini-render for removed context: {}", context_id);
        }
    }
}

// ============================================================================
// HELPERS
// ============================================================================

/// Get color for activity indicator in mini-render
fn activity_to_mini_color(activity: ActivityState, theme: &Theme) -> Color {
    match activity {
        ActivityState::Idle => theme.fg_dim.with_alpha(0.5),
        ActivityState::Active => theme.accent.with_alpha(0.8),
        ActivityState::Streaming => theme.constellation_node_glow_streaming,
        ActivityState::Waiting => theme.warning.with_alpha(0.8),
        ActivityState::Error => theme.accent2,
        ActivityState::Completed => theme.success,
    }
}

/// Get label text for activity state
fn activity_label(activity: ActivityState) -> &'static str {
    match activity {
        ActivityState::Idle => "idle",
        ActivityState::Active => "active",
        ActivityState::Streaming => "streaming...",
        ActivityState::Waiting => "waiting",
        ActivityState::Error => "error",
        ActivityState::Completed => "done",
    }
}

/// Truncate a name to max length with ellipsis
fn truncate_name(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else if max_len > 3 {
        format!("{}...", &name[..max_len - 3])
    } else {
        name[..max_len].to_string()
    }
}

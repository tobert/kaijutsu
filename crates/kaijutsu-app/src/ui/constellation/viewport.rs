//! 3D viewport for constellation rendering via Bevy's `ViewportNode`.
//!
//! Replaces the 2D UiMaterial-based constellation with a full 3D scene rendered
//! into the flexbox layout. The 3D camera renders to a texture which is displayed
//! inside the ConstellationContainer UI node.
//!
//! All 3D content uses `RenderLayers::layer(1)` to isolate from the main 2D UI.

use bevy::{
    asset::RenderAssetUsages,
    camera::{RenderTarget, visibility::RenderLayers},
    picking::{Pickable, mesh_picking::MeshPickingCamera},
    prelude::*,
    render::render_resource::{TextureDimension, TextureFormat, TextureUsages},
    ui::widget::ViewportNode,
};

use super::ConstellationContainer;

/// Marker for the 3D camera used by the constellation viewport.
#[derive(Component)]
pub struct ConstellationCamera3d;

/// Marker for the translucent ball boundary sphere.
#[derive(Component)]
pub struct BallBoundary;

/// Resource tracking whether the 3D viewport has been set up.
#[derive(Resource, Default)]
pub struct ViewportState {
    pub camera_entity: Option<Entity>,
    pub image_handle: Option<Handle<Image>>,
}

/// The render layer used for all constellation 3D content.
const CONSTELLATION_LAYER: usize = 1;

/// Set up the constellation rendering systems (3D viewport version).
pub fn setup_viewport_systems(app: &mut App) {
    app.init_resource::<ViewportState>()
        .add_systems(
            Update,
            setup_constellation_3d,
        );
}

/// One-time setup: create render target, 3D camera, test geometry, and wire
/// the ViewportNode into the ConstellationContainer.
fn setup_constellation_3d(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut viewport_state: ResMut<ViewportState>,
    container_q: Query<Entity, (With<ConstellationContainer>, Without<ViewportNode>)>,
) {
    // Only run once — when container exists but doesn't have ViewportNode yet
    if viewport_state.camera_entity.is_some() {
        return;
    }

    let Ok(container_entity) = container_q.single() else {
        return;
    };

    // Create render target image — size will be auto-synced by Bevy's
    // `update_viewport_render_target_size` system
    let mut image = Image::new_uninit(
        default(),
        TextureDimension::D2,
        TextureFormat::Bgra8UnormSrgb,
        RenderAssetUsages::all(),
    );
    image.texture_descriptor.usage =
        TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::RENDER_ATTACHMENT;
    let image_handle = images.add(image);

    // Spawn 3D camera on layer 1
    let camera_entity = commands
        .spawn((
            ConstellationCamera3d,
            MeshPickingCamera,
            Camera3d::default(),
            Camera {
                order: -1, // Render before the UI camera
                clear_color: ClearColorConfig::Custom(Color::srgba(0.02, 0.02, 0.06, 1.0)),
                ..default()
            },
            RenderTarget::Image(image_handle.clone().into()),
            Transform::from_xyz(0.0, 0.0, 3.0).looking_at(Vec3::ZERO, Vec3::Y),
            RenderLayers::layer(CONSTELLATION_LAYER),
        ))
        .id();

    // Translucent ball boundary sphere (r=0.95) — IGNORE picking so clicks
    // pass through to the node spheres inside
    let boundary_mesh = meshes.add(Sphere::new(0.95));
    commands.spawn((
        BallBoundary,
        Pickable::IGNORE,
        Mesh3d(boundary_mesh),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(0.3, 0.4, 0.8, 0.04),
            alpha_mode: AlphaMode::Blend,
            double_sided: true,
            cull_mode: None,
            ..default()
        })),
        Transform::IDENTITY,
        RenderLayers::layer(CONSTELLATION_LAYER),
    ));

    // Point light visible to both layers (illuminates the 3D scene)
    commands.spawn((
        PointLight {
            intensity: 2000.0,
            range: 20.0,
            ..default()
        },
        Transform::from_xyz(2.0, 3.0, 4.0),
        RenderLayers::from_layers(&[0, CONSTELLATION_LAYER]),
    ));

    // Add ViewportNode to the constellation container
    commands.entity(container_entity).insert(
        ViewportNode::new(camera_entity),
    );

    viewport_state.camera_entity = Some(camera_entity);
    viewport_state.image_handle = Some(image_handle);

    info!("Constellation 3D viewport initialized with test spheres");
}

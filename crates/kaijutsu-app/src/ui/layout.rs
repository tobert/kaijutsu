//! RON-driven layout system for Kaijutsu
//!
//! Replaces hardcoded UI spawning with RON-defined layouts, enabling:
//! - New view variants without Rust changes
//! - Hot-reload during development
//! - Future Rhai scripting of layouts
//! - Documentation of existing views in machine-readable format
//!
//! ## Architecture
//!
//! ```text
//! LayoutPreset (RON asset)
//! └── LayoutNode (tree)
//!     ├── Container { direction, children, flex }
//!     └── Panel { id, flex }
//!
//! PanelRegistry (Resource)
//! └── Maps panel IDs → PanelBuilder functions
//!
//! LayoutReconciler
//! └── Walks LayoutNode tree, spawns entities via PanelRegistry
//! ```

use bevy::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;

use super::hud::HudPosition;

// ============================================================================
// LAYOUT TYPES
// ============================================================================

/// A node in the layout tree.
///
/// Layout trees describe the flex structure of a view. Containers
/// provide direction and grouping, panels are leaf nodes that
/// reference registered panel types.
#[derive(Debug, Clone, Deserialize, Reflect)]
pub enum LayoutNode {
    /// Container node - groups children with flex direction
    Container {
        /// Flex direction (Row or Column)
        direction: LayoutDirection,
        /// Child nodes
        #[serde(default)]
        children: Vec<LayoutNode>,
        /// Flex grow factor (default 1.0)
        #[serde(default = "default_flex")]
        flex: f32,
        /// Padding in pixels (applied to all sides)
        #[serde(default)]
        padding: f32,
        /// Gap between children in pixels (row_gap for Column, column_gap for Row)
        #[serde(default)]
        gap: f32,
    },
    /// Panel node - references a registered panel type
    Panel {
        /// Panel type ID (must be registered in PanelRegistry)
        id: String,
        /// Flex grow factor (default 1.0)
        #[serde(default = "default_flex")]
        flex: f32,
    },
}

fn default_flex() -> f32 {
    1.0
}

/// Flex direction for container nodes.
#[derive(Debug, Clone, Copy, Default, Deserialize, Reflect, PartialEq, Eq)]
pub enum LayoutDirection {
    /// Stack children vertically (column)
    #[default]
    Column,
    /// Stack children horizontally (row)
    Row,
}

impl From<LayoutDirection> for FlexDirection {
    fn from(dir: LayoutDirection) -> Self {
        match dir {
            LayoutDirection::Column => FlexDirection::Column,
            LayoutDirection::Row => FlexDirection::Row,
        }
    }
}

/// HUD placement in a layout.
#[derive(Debug, Clone, Deserialize, Reflect)]
pub struct HudPlacement {
    /// Screen position
    pub position: HudPosition,
    /// Widget name (must match HudContent enum variant)
    pub widget: String,
}

/// Complete layout preset, loadable from RON.
///
/// A preset defines a complete view layout including:
/// - Panel arrangement (the tree)
/// - HUD overlay positions
#[derive(Debug, Clone, Deserialize, Asset, TypePath)]
pub struct LayoutPreset {
    /// Preset name (for display/reference)
    pub name: String,
    /// Root layout node
    pub root: LayoutNode,
    /// HUD placements (overlay widgets)
    #[serde(default)]
    pub huds: Vec<HudPlacement>,
}

// ============================================================================
// PANEL REGISTRY
// ============================================================================

/// Opaque handle to a panel type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelTypeId(u32);

/// Context passed to panel builder functions.
pub struct PanelSpawnContext<'a> {
    /// Parent entity to spawn under
    pub parent: Entity,
    /// Flex grow factor from layout
    pub flex: f32,
    /// Current theme
    pub theme: &'a crate::ui::theme::Theme,
}

/// Function type for panel spawning.
///
/// Panel builders receive a Commands reference and context,
/// return the root entity of the spawned panel.
pub type PanelBuilder = Box<dyn Fn(&mut Commands, &PanelSpawnContext) -> Entity + Send + Sync>;

/// Registry mapping panel names to spawn capabilities.
///
/// Panels are registered at startup with a name and optional builder.
/// The layout reconciler uses this to spawn panels from RON layouts.
#[derive(Resource, Default)]
pub struct PanelRegistry {
    name_to_id: HashMap<String, PanelTypeId>,
    id_to_name: HashMap<PanelTypeId, String>,
    builders: HashMap<PanelTypeId, PanelBuilder>,
    next_id: u32,
}

impl PanelRegistry {
    /// Register a panel type by name (without builder).
    ///
    /// Use this for panels that are spawned by existing systems
    /// and don't need RON-driven spawning yet.
    pub fn register(&mut self, name: impl Into<String>) -> PanelTypeId {
        let name = name.into();
        if let Some(&id) = self.name_to_id.get(&name) {
            return id;
        }
        let id = PanelTypeId(self.next_id);
        self.next_id += 1;
        self.name_to_id.insert(name.clone(), id);
        self.id_to_name.insert(id, name);
        id
    }

    /// Register a panel type with a builder function.
    ///
    /// The builder will be called by the layout reconciler to spawn
    /// panel entities when the layout is applied.
    pub fn register_with_builder(
        &mut self,
        name: impl Into<String>,
        builder: impl Fn(&mut Commands, &PanelSpawnContext) -> Entity + Send + Sync + 'static,
    ) -> PanelTypeId {
        let id = self.register(name);
        self.builders.insert(id, Box::new(builder));
        id
    }

    /// Get panel type ID by name.
    pub fn get(&self, name: &str) -> Option<PanelTypeId> {
        self.name_to_id.get(name).copied()
    }

    /// Get panel name by ID.
    pub fn name(&self, id: PanelTypeId) -> Option<&str> {
        self.id_to_name.get(&id).map(|s| s.as_str())
    }

    /// Check if a panel has a builder registered.
    pub fn has_builder(&self, id: PanelTypeId) -> bool {
        self.builders.contains_key(&id)
    }

    /// Spawn a panel using its registered builder.
    ///
    /// Returns None if the panel has no builder registered.
    pub fn spawn(
        &self,
        id: PanelTypeId,
        commands: &mut Commands,
        ctx: &PanelSpawnContext,
    ) -> Option<Entity> {
        self.builders.get(&id).map(|builder| builder(commands, ctx))
    }

    /// Get all registered panel names.
    pub fn panel_names(&self) -> impl Iterator<Item = &str> {
        self.name_to_id.keys().map(|s| s.as_str())
    }
}

// ============================================================================
// LOADED LAYOUTS
// ============================================================================

/// Resource holding loaded layout presets.
#[derive(Resource, Default)]
pub struct LoadedLayouts {
    /// Presets keyed by name
    pub presets: HashMap<String, Handle<LayoutPreset>>,
    /// Currently active layout name (if any)
    pub active: Option<String>,
}

impl LoadedLayouts {
    /// Get a layout handle by name.
    pub fn get(&self, name: &str) -> Option<&Handle<LayoutPreset>> {
        self.presets.get(name)
    }

    /// Get list of available layout names.
    pub fn available_layouts(&self) -> Vec<&str> {
        self.presets.keys().map(|s| s.as_str()).collect()
    }
}

// ============================================================================
// LAYOUT SWITCHING
// ============================================================================

/// Message to request a layout switch.
///
/// Send this message to switch to a different layout preset.
/// The layout reconciler will apply the new layout on the next frame.
#[derive(Message)]
pub struct SwitchLayoutRequest {
    /// Name of the layout preset to switch to
    pub layout_name: String,
}

/// Message sent when a layout switch completes.
#[derive(Message)]
pub struct LayoutSwitched {
    /// Name of the layout that was applied
    pub layout_name: String,
}

// ============================================================================
// LAYOUT RON ASSET LOADER
// ============================================================================

/// Asset loader for LayoutPreset RON files.
#[derive(Default, bevy::reflect::TypePath)]
pub struct LayoutPresetLoader;

impl bevy::asset::AssetLoader for LayoutPresetLoader {
    type Asset = LayoutPreset;
    type Settings = ();
    type Error = LayoutLoadError;

    async fn load(
        &self,
        reader: &mut dyn bevy::asset::io::Reader,
        _settings: &Self::Settings,
        _load_context: &mut bevy::asset::LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        let text = std::str::from_utf8(&bytes)?;
        let preset: LayoutPreset = ron::from_str(text)?;
        Ok(preset)
    }

    fn extensions(&self) -> &[&str] {
        &["layout.ron"]
    }
}

/// Error type for layout loading.
#[derive(Debug, thiserror::Error)]
pub enum LayoutLoadError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("RON parse error: {0}")]
    Ron(#[from] ron::error::SpannedError),
}

// ============================================================================
// PLUGIN
// ============================================================================

/// Plugin for the RON-driven layout system.
pub struct LayoutPlugin;

impl Plugin for LayoutPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<LayoutPreset>()
            .init_asset_loader::<LayoutPresetLoader>()
            .init_resource::<PanelRegistry>()
            .init_resource::<LoadedLayouts>()
            .register_type::<LayoutNode>()
            .register_type::<LayoutDirection>()
            // Layout switching messages
            .add_message::<SwitchLayoutRequest>()
            .add_message::<LayoutSwitched>()
            .add_systems(Startup, (register_builtin_panels, load_layout_presets).chain())
            .add_systems(
                Update,
                (
                    validate_layouts.run_if(resource_changed::<LoadedLayouts>),
                    // Handle layout switch requests
                    handle_layout_switch,
                    // Reconciler runs when ViewStack changes
                    super::layout_reconciler::on_view_change,
                )
                    .chain(),
            );
    }
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Register all built-in panel types.
///
/// Panels are registered without builders initially. The existing hardcoded
/// UI continues to work alongside the layout system. When migrating to
/// fully RON-driven layouts, builders would be added here.
///
/// # Migration Pattern
///
/// To migrate a panel to RON-driven spawning:
/// 1. Create a builder function that spawns the panel's entities
/// 2. Replace `registry.register("PanelName")` with
///    `registry.register_with_builder("PanelName", builder_fn)`
/// 3. Remove the hardcoded spawn code from the original location
///
/// Example builder:
/// ```ignore
/// registry.register_with_builder("DagView", |commands, ctx| {
///     commands.spawn((
///         DagViewMarker,
///         Node {
///             flex_grow: ctx.flex,
///             flex_direction: FlexDirection::Column,
///             overflow: Overflow::clip(),
///             ..default()
///         },
///     ))
///     .id()
/// });
/// ```
fn register_builtin_panels(mut registry: ResMut<PanelRegistry>) {
    // =========================================================================
    // CONVERSATION VIEW PANELS
    // =========================================================================

    // DagView - the scrollable conversation content area
    registry.register_with_builder("DagView", |commands, ctx| {
        commands
            .spawn((
                crate::cell::ConversationContainer,
                Node {
                    flex_grow: ctx.flex,
                    flex_direction: FlexDirection::Column,
                    overflow: Overflow::clip(),
                    padding: UiRect::axes(Val::Px(16.0), Val::Px(4.0)),
                    ..default()
                },
            ))
            .id()
    });

    // InputShadow - reserves space at bottom for docked input (legacy)
    registry.register_with_builder("InputShadow", |commands, ctx| {
        commands
            .spawn((
                super::state::InputShadow,
                crate::cell::PromptContainer,
                Node {
                    width: Val::Percent(100.0),
                    flex_grow: ctx.flex,
                    // Height controlled by sync_input_shadow_height system
                    min_height: Val::Px(0.0),
                    ..default()
                },
            ))
            .id()
    });

    // ComposeBlock - inline editable block at end of conversation
    // This is the "compose block" that replaces the floating prompt
    registry.register_with_builder("ComposeBlock", |commands, ctx| {
        commands
            .spawn((
                crate::cell::ComposeBlock::default(),
                crate::text::MsdfText,
                crate::text::MsdfTextAreaConfig::default(),
                Node {
                    width: Val::Percent(100.0),
                    min_height: Val::Px(60.0),
                    padding: UiRect::all(Val::Px(12.0)),
                    margin: UiRect::new(
                        Val::Px(40.0),  // left margin matches conversation blocks
                        Val::Px(40.0),  // right margin
                        Val::Px(8.0),   // top margin
                        Val::Px(16.0),  // bottom margin
                    ),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    flex_grow: ctx.flex,
                    ..default()
                },
                // Distinct border color for compose block
                BorderColor::all(Color::srgba(0.4, 0.6, 0.9, 0.6)),
                BackgroundColor(Color::srgba(0.1, 0.1, 0.15, 0.8)),
            ))
            .id()
    });

    // ConstellationMini - currently no builder, uses placeholder
    // TODO: Move constellation spawning to a builder
    registry.register("ConstellationMini");

    // InputFrame and PromptCell - spawned by the frame_assembly system
    // These are handled by the existing input layer system, not the layout system
    registry.register("InputFrame");
    registry.register("PromptCell");

    // =========================================================================
    // DASHBOARD VIEW PANELS
    // =========================================================================

    // Dashboard columns - spawn marker that filler system detects
    // The filler adds chasing border decoration and inner scrollable container
    registry.register_with_builder("KernelList", |commands, ctx| {
        commands
            .spawn((
                crate::dashboard::KernelListColumn,
                Node {
                    flex_grow: ctx.flex,
                    flex_direction: FlexDirection::Column,
                    ..default()
                },
            ))
            .id()
    });

    registry.register_with_builder("ContextList", |commands, ctx| {
        commands
            .spawn((
                crate::dashboard::ContextListColumn,
                Node {
                    flex_grow: ctx.flex,
                    flex_direction: FlexDirection::Column,
                    ..default()
                },
            ))
            .id()
    });

    registry.register_with_builder("SeatsList", |commands, ctx| {
        commands
            .spawn((
                crate::dashboard::SeatsListColumn,
                Node {
                    flex_grow: ctx.flex,
                    flex_direction: FlexDirection::Column,
                    ..default()
                },
            ))
            .id()
    });

    // DashboardFooter - the Take Seat footer row
    registry.register_with_builder("DashboardFooter", |commands, ctx| {
        commands
            .spawn((
                crate::dashboard::DashboardFooter,
                Node {
                    width: Val::Percent(100.0),
                    flex_grow: ctx.flex,
                    padding: UiRect::all(Val::Px(20.0)),
                    border: UiRect::top(Val::Px(1.0)),
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    column_gap: Val::Px(12.0),
                    ..default()
                },
            ))
            .id()
    });

    // SeatSelector stays in header chrome (not part of dashboard layout)
    registry.register("SeatSelector");

    // =========================================================================
    // OVERLAY PANELS
    // =========================================================================

    // ExpandedBlockEditor - currently no builder
    registry.register("ExpandedBlockEditor");

    info!(
        "Registered {} panel types ({} with builders): {:?}",
        registry.name_to_id.len(),
        registry.builders.len(),
        registry.panel_names().collect::<Vec<_>>()
    );
}

/// Load layout presets from assets/layouts/.
fn load_layout_presets(asset_server: Res<AssetServer>, mut layouts: ResMut<LoadedLayouts>) {
    // Load all standard layouts
    let layout_files = [
        ("conversation", "layouts/conversation.layout.ron"),
        ("dashboard", "layouts/dashboard.layout.ron"),
        ("expanded_block", "layouts/expanded_block.layout.ron"),
    ];

    for (name, path) in layout_files {
        let handle = asset_server.load(path);
        layouts.presets.insert(name.to_string(), handle);
        info!("Loading layout preset: {} from {}", name, path);
    }
}

/// Validate that loaded layouts reference registered panels.
fn validate_layouts(
    layouts: Res<LoadedLayouts>,
    presets: Res<Assets<LayoutPreset>>,
    registry: Res<PanelRegistry>,
) {
    for (name, handle) in &layouts.presets {
        if let Some(preset) = presets.get(handle) {
            validate_node(&preset.root, &registry, name);
        }
    }
}

fn validate_node(node: &LayoutNode, registry: &PanelRegistry, preset_name: &str) {
    match node {
        LayoutNode::Panel { id, .. } => {
            if registry.get(id).is_none() {
                warn!(
                    "Layout '{}' references unknown panel '{}' - panel will not be spawned",
                    preset_name, id
                );
            }
        }
        LayoutNode::Container { children, .. } => {
            for child in children {
                validate_node(child, registry, preset_name);
            }
        }
    }
}

/// Handle layout switch requests.
///
/// When a [SwitchLayoutRequest] message is received, this system updates
/// the `LoadedLayouts.active` field. The reconciler then picks up the change
/// and applies the new layout.
fn handle_layout_switch(
    mut requests: MessageReader<SwitchLayoutRequest>,
    mut layouts: ResMut<LoadedLayouts>,
    mut switched_writer: MessageWriter<LayoutSwitched>,
) {
    for request in requests.read() {
        // Verify the layout exists
        if !layouts.presets.contains_key(&request.layout_name) {
            warn!(
                "Layout switch requested for unknown layout: '{}'",
                request.layout_name
            );
            continue;
        }

        info!("Switching to layout: '{}'", request.layout_name);
        layouts.active = Some(request.layout_name.clone());

        // Notify listeners that the layout changed
        switched_writer.write(LayoutSwitched {
            layout_name: request.layout_name.clone(),
        });
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_panel_registry() {
        let mut registry = PanelRegistry::default();

        let id1 = registry.register("TestPanel");
        let id2 = registry.register("AnotherPanel");
        let id3 = registry.register("TestPanel"); // Duplicate

        assert_eq!(id1, id3); // Same name → same ID
        assert_ne!(id1, id2); // Different name → different ID

        assert_eq!(registry.get("TestPanel"), Some(id1));
        assert_eq!(registry.get("AnotherPanel"), Some(id2));
        assert_eq!(registry.get("NonExistent"), None);

        assert_eq!(registry.name(id1), Some("TestPanel"));
    }

    #[test]
    fn test_parse_layout_node() {
        let ron = r#"
            Container(
                direction: Column,
                children: [
                    Panel(id: "Header", flex: 0.0),
                    Container(
                        direction: Row,
                        children: [
                            Panel(id: "Sidebar", flex: 0.0),
                            Panel(id: "Content", flex: 1.0),
                        ],
                        flex: 1.0,
                    ),
                ],
                flex: 1.0,
            )
        "#;

        let node: LayoutNode = ron::from_str(ron).unwrap();

        match node {
            LayoutNode::Container {
                direction,
                children,
                flex,
                ..
            } => {
                assert_eq!(direction, LayoutDirection::Column);
                assert_eq!(flex, 1.0);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("Expected Container"),
        }
    }

    #[test]
    fn test_parse_layout_preset() {
        let ron = r#"
            (
                name: "test",
                root: Container(
                    direction: Column,
                    children: [
                        Panel(id: "Main", flex: 1.0),
                    ],
                    flex: 1.0,
                ),
                huds: [
                    (position: TopRight, widget: "status"),
                ],
            )
        "#;

        let preset: LayoutPreset = ron::from_str(ron).unwrap();
        assert_eq!(preset.name, "test");
        assert_eq!(preset.huds.len(), 1);
        assert_eq!(preset.huds[0].widget, "status");
    }
}

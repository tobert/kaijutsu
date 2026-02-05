//! Layout reconciler - spawns UI entities from RON layout trees
//!
//! The reconciler walks a [LayoutNode] tree and spawns/despawns entities
//! to match. It uses [LayoutManaged] markers to track which entities
//! belong to which layout.
//!
//! ## Architecture
//!
//! ```text
//! LayoutPreset (RON)
//! └── LayoutNode tree
//!     ├── Container → spawns flex container entity
//!     └── Panel → calls PanelRegistry builder
//!
//! Reconciler walks tree:
//! 1. Despawn old LayoutManaged entities
//! 2. Spawn new containers + panels from tree
//! 3. Mark with LayoutManaged for next reconcile
//! ```

use bevy::prelude::*;

use super::layout::{
    LayoutDirection, LayoutNode, LayoutPreset, LoadedLayouts, PanelRegistry, PanelSpawnContext,
};
use super::theme::Theme;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for entities managed by the layout system.
///
/// Entities with this marker are spawned by the reconciler and will be
/// despawned when the layout changes or is re-reconciled.
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component)]
pub struct LayoutManaged {
    /// Name of the layout preset this entity belongs to
    pub layout_name: String,
    /// Path through the tree (indices at each level)
    pub node_path: Vec<usize>,
}

/// Marker for a container entity created by the reconciler.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct LayoutContainer;

/// Marker for a panel placeholder entity.
///
/// When a panel doesn't have a builder registered, we spawn a placeholder
/// that can be filled in by other systems.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct LayoutPanelPlaceholder {
    /// Panel ID from the layout
    pub panel_id: String,
}

// ============================================================================
// RECONCILER
// ============================================================================

/// Reconcile current UI with target layout.
///
/// This is the main entry point for applying a layout. It:
/// 1. Despawns all entities marked with [LayoutManaged] for this layout
/// 2. Spawns new entities from the layout tree
/// 3. Marks new entities with [LayoutManaged]
///
/// The simple strategy is despawn-all/respawn-all. A future optimization
/// could diff the trees and only change what's needed.
pub fn reconcile_layout(
    commands: &mut Commands,
    layouts: &LoadedLayouts,
    presets: &Assets<LayoutPreset>,
    registry: &PanelRegistry,
    theme: &Theme,
    existing: &Query<(Entity, &LayoutManaged)>,
    root_entity: Entity,
    layout_name: &str,
) {
    // Get the layout preset
    let Some(handle) = layouts.presets.get(layout_name) else {
        warn!("Layout '{}' not found in LoadedLayouts", layout_name);
        return;
    };
    let Some(preset) = presets.get(handle) else {
        // Asset not loaded yet - will be called again when ready
        debug!("Layout '{}' asset not loaded yet", layout_name);
        return;
    };

    // Check if this layout has any panels with builders
    // If not, skip reconciliation to avoid spawning empty placeholders
    // alongside existing hardcoded content
    if !layout_has_builders(&preset.root, registry) {
        debug!(
            "Layout '{}' has no panel builders, skipping reconciliation",
            layout_name
        );
        return;
    }

    // Despawn all existing layout-managed entities for this layout
    for (entity, managed) in existing.iter() {
        if managed.layout_name == layout_name {
            commands.entity(entity).despawn();
        }
    }

    // Spawn the tree under root_entity
    spawn_node(
        commands,
        &preset.root,
        registry,
        theme,
        root_entity,
        layout_name,
        vec![],
    );

    info!(
        "Reconciled layout '{}' under {:?}",
        layout_name, root_entity
    );
}

/// Recursively spawn a layout node and its children.
fn spawn_node(
    commands: &mut Commands,
    node: &LayoutNode,
    registry: &PanelRegistry,
    theme: &Theme,
    parent: Entity,
    layout_name: &str,
    path: Vec<usize>,
) {
    match node {
        LayoutNode::Container {
            direction,
            children,
            flex,
            padding,
            gap,
        } => {
            // Determine gap direction based on flex direction
            let (row_gap, column_gap) = match direction {
                LayoutDirection::Column => (Val::Px(*gap), Val::Px(0.0)),
                LayoutDirection::Row => (Val::Px(0.0), Val::Px(*gap)),
            };

            // Spawn container entity
            let container = commands
                .spawn((
                    LayoutContainer,
                    LayoutManaged {
                        layout_name: layout_name.to_string(),
                        node_path: path.clone(),
                    },
                    Node {
                        flex_direction: FlexDirection::from(*direction),
                        flex_grow: *flex,
                        width: if *flex > 0.0 {
                            Val::Percent(100.0)
                        } else {
                            Val::Auto
                        },
                        height: if *flex > 0.0 {
                            Val::Percent(100.0)
                        } else {
                            Val::Auto
                        },
                        padding: UiRect::all(Val::Px(*padding)),
                        row_gap,
                        column_gap,
                        ..default()
                    },
                ))
                .id();
            commands.entity(parent).add_child(container);

            // Spawn children
            for (i, child) in children.iter().enumerate() {
                let mut child_path = path.clone();
                child_path.push(i);
                spawn_node(
                    commands,
                    child,
                    registry,
                    theme,
                    container,
                    layout_name,
                    child_path,
                );
            }
        }
        LayoutNode::Panel { id, flex } => {
            if let Some(panel_id) = registry.get(id) {
                let ctx = PanelSpawnContext { flex: *flex };

                if let Some(entity) = registry.spawn(panel_id, commands, ctx) {
                    // Mark spawned panel as layout-managed and add as child
                    commands.entity(entity).insert(LayoutManaged {
                        layout_name: layout_name.to_string(),
                        node_path: path,
                    });
                    commands.entity(parent).add_child(entity);
                } else {
                    // No builder - spawn placeholder
                    spawn_panel_placeholder(commands, parent, layout_name, &path, id, *flex);
                }
            } else {
                // Unknown panel - spawn placeholder with warning style
                warn!(
                    "Layout '{}' panel '{}' not registered",
                    layout_name, id
                );
                spawn_panel_placeholder(commands, parent, layout_name, &path, id, *flex);
            }
        }
    }
}

/// Check if a layout tree has any panels with registered builders.
///
/// Returns true if at least one panel has a builder, false if all panels
/// would spawn as placeholders.
fn layout_has_builders(node: &LayoutNode, registry: &PanelRegistry) -> bool {
    match node {
        LayoutNode::Panel { id, .. } => {
            if let Some(panel_id) = registry.get(id) {
                registry.has_builder(panel_id)
            } else {
                false
            }
        }
        LayoutNode::Container { children, .. } => {
            children.iter().any(|child| layout_has_builders(child, registry))
        }
    }
}

/// Spawn a placeholder for panels without builders.
fn spawn_panel_placeholder(
    commands: &mut Commands,
    parent: Entity,
    layout_name: &str,
    path: &[usize],
    panel_id: &str,
    flex: f32,
) {
    let placeholder = commands
        .spawn((
            LayoutPanelPlaceholder {
                panel_id: panel_id.to_string(),
            },
            LayoutManaged {
                layout_name: layout_name.to_string(),
                node_path: path.to_vec(),
            },
            Node {
                flex_grow: flex,
                // Placeholders take up space but are invisible
                min_width: if flex == 0.0 { Val::Px(0.0) } else { Val::Auto },
                min_height: if flex == 0.0 { Val::Px(0.0) } else { Val::Auto },
                ..default()
            },
        ))
        .id();
    commands.entity(parent).add_child(placeholder);
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// System that reconciles layout when ViewStack or active layout changes.
///
/// This is the main driver for the layout system. When either:
/// - The current view changes (ViewStack)
/// - A layout switch is requested (LoadedLayouts.active)
/// - The current view's root exists but has no layout (startup timing fix)
///
/// ...it triggers a reconcile with the appropriate layout.
///
/// The reconciler spawns into the correct view root container:
/// - Dashboard view → DashboardRoot
/// - Conversation view → ConversationRoot
pub fn on_view_change(
    view_stack: Res<super::state::ViewStack>,
    mut commands: Commands,
    layouts: Res<LoadedLayouts>,
    presets: Res<Assets<LayoutPreset>>,
    registry: Res<PanelRegistry>,
    theme: Res<Theme>,
    existing: Query<(Entity, &LayoutManaged)>,
    conversation_root: Query<Entity, With<super::state::ConversationRoot>>,
    dashboard_root: Query<Entity, With<crate::dashboard::DashboardRoot>>,
    children_query: Query<&Children>,
) {
    // Helper to check if a root needs its initial layout
    let needs_layout = |root: Option<Entity>| -> bool {
        root.map(|e| children_query.get(e).map(|c| c.is_empty()).unwrap_or(true))
            .unwrap_or(false)
    };

    let conv_root = conversation_root.single().ok();
    let dash_root = dashboard_root.single().ok();

    // Check if EITHER root needs initial layout (startup timing fix)
    let conv_needs_layout = needs_layout(conv_root);
    let dash_needs_layout = needs_layout(dash_root);

    let current_view = view_stack.current();
    let view_changed = view_stack.is_changed() || layouts.is_changed();

    // Track what we've reconciled to avoid duplicates
    let mut reconciled_conv = false;
    let mut reconciled_dash = false;

    // Reconcile conversation root if it needs initial layout
    if conv_needs_layout {
        if let Some(root) = conv_root {
            reconcile_layout(
                &mut commands,
                &layouts,
                &presets,
                &registry,
                &theme,
                &existing,
                root,
                "conversation",
            );
            reconciled_conv = true;
        }
    }

    // Reconcile dashboard root if it needs initial layout
    if dash_needs_layout {
        if let Some(root) = dash_root {
            reconcile_layout(
                &mut commands,
                &layouts,
                &presets,
                &registry,
                &theme,
                &existing,
                root,
                "dashboard",
            );
            reconciled_dash = true;
        }
    }

    // For view changes, reconcile current view (if not already done above)
    if view_changed {
        let (root_entity, layout_name, already_done) = match current_view.root_container() {
            super::state::ViewRootContainer::Conversation => {
                (conv_root, "conversation", reconciled_conv)
            }
            super::state::ViewRootContainer::Dashboard => {
                (dash_root, "dashboard", reconciled_dash)
            }
        };

        if !already_done {
            if let Some(root) = root_entity {
                let layout_name = layouts.active.as_deref().unwrap_or(layout_name);
                reconcile_layout(
                    &mut commands,
                    &layouts,
                    &presets,
                    &registry,
                    &theme,
                    &existing,
                    root,
                    layout_name,
                );
            }
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_managed_marker() {
        let marker = LayoutManaged {
            layout_name: "test".to_string(),
            node_path: vec![0, 1, 2],
        };
        assert_eq!(marker.layout_name, "test");
        assert_eq!(marker.node_path, vec![0, 1, 2]);
    }
}

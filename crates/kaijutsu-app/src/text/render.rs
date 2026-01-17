//! Render node for glyphon text rendering.

use bevy::prelude::*;
use bevy::render::{
    render_graph::{NodeRunError, RenderGraphContext, RenderLabel, ViewNode},
    render_resource::*,
    renderer::RenderContext,
    view::ViewTarget,
};

use super::plugin::RenderTextResources;

/// Label for the text render node.
#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
pub struct TextRenderNodeLabel;

/// Render node that draws glyphon text.
#[derive(Default)]
pub struct TextRenderNode;

impl TextRenderNode {
    pub const NAME: TextRenderNodeLabel = TextRenderNodeLabel;
}

impl ViewNode for TextRenderNode {
    type ViewQuery = &'static ViewTarget;

    fn run(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        view_target: &ViewTarget,
        world: &World,
    ) -> Result<(), NodeRunError> {
        let Some(resources) = world.get_resource::<RenderTextResources>() else {
            return Ok(());
        };

        let Some(ref resources) = resources.0 else {
            return Ok(());
        };

        // Get the render target view
        let color_attachment = view_target.get_color_attachment();

        // Create a render pass
        let mut render_pass = render_context
            .command_encoder()
            .begin_render_pass(&RenderPassDescriptor {
                label: Some("glyphon_text_render_pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: color_attachment.view,
                    resolve_target: color_attachment.resolve_target,
                    ops: Operations {
                        load: LoadOp::Load, // Don't clear - render on top
                        store: StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

        // Render the text
        match resources
            .renderer
            .render(&resources.atlas, &resources.viewport, &mut render_pass)
        {
            Ok(()) => {
                // Only log once to avoid spam
                static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    bevy::log::info!("glyphon render() succeeded");
                }
            }
            Err(e) => {
                bevy::log::error!("glyphon render() failed: {:?}", e);
            }
        }

        Ok(())
    }
}

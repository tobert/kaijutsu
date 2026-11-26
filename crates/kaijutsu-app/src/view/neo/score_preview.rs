use bevy::prelude::*;
use kaijutsu_abc::engrave::{EngravingOptions, engrave_to_svg};
use kaijutsu_types::{BlockId, BlockSnapshot};

use crate::text::{SvgFontDb, svg_raster::rasterize_svg};
use crate::view::{block_render::create_svg_raster_image, diff_view::content_hash};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ScorePreview {
    pub image: Handle<Image>,
    pub size: UVec2,
}

#[derive(Default)]
pub(super) struct ScorePreviewCache {
    source: Option<(BlockId, u64)>,
    preview: Option<ScorePreview>,
}

impl ScorePreviewCache {
    pub fn preview(
        &mut self,
        block: &BlockSnapshot,
        fonts: Option<&SvgFontDb>,
        images: &mut Assets<Image>,
    ) -> Option<ScorePreview> {
        let source = (block.id, content_hash(&block.content));
        if self.source != Some(source) {
            self.source = Some(source);
            self.preview = render(&block.content, fonts, images);
        }
        self.preview.clone()
    }
}

fn render(source: &str, fonts: Option<&SvgFontDb>, images: &mut Assets<Image>) -> Option<ScorePreview> {
    let mut tune = kaijutsu_abc::parse(source).value.into_iter().next()?;
    if tune.voices.iter().all(|voice| voice.elements.is_empty()) {
        return None;
    }
    // The card heading already gives the title.
    tune.header.title.clear();
    let svg = engrave_to_svg(&tune, &EngravingOptions { margin: 8.0, ..default() });
    let options = fonts.map(SvgFontDb::usvg_options).unwrap_or_default();
    let tree = usvg::Tree::from_str(&svg, &options).ok()?;
    let size = Vec2::new(tree.size().width(), tree.size().height());
    let scale = (Vec2::new(1536.0, 768.0) / size).min_element();
    let physical = (size * scale).ceil().as_uvec2().max(UVec2::ONE);
    let rgba = rasterize_svg(&tree, physical.x, physical.y, f64::from(scale))?;
    Some(ScorePreview {
        image: create_svg_raster_image(images, physical.x, physical.y, rgba),
        size: physical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, ContentType, ContextId, PrincipalId};

    #[test]
    fn previews_render_ink_and_refresh_when_the_score_is_edited() {
        let id = BlockId::new(ContextId::from_bytes([1; 16]), PrincipalId::from_bytes([2; 16]), 1);
        let mut cache = ScorePreviewCache::default();
        let mut images = Assets::<Image>::default();
        let score = |content| BlockSnapshotBuilder::new(id, BlockKind::Text)
            .content_type(ContentType::Abc).content(content).build();
        let block = score("X:1\nT:Rehearsal\nK:C\nCDEF GABc|");
        let first = cache.preview(&block, None, &mut images).unwrap();
        let data = images.get(&first.image).unwrap().data.as_ref().unwrap();
        assert!(data.chunks_exact(4).any(|pixel| pixel[3] == 255));
        assert_eq!(cache.preview(&block, None, &mut images).unwrap().image, first.image);

        let edited = score("X:1\nT:Rehearsal\nK:C\nCBAG FEDC|");
        assert_ne!(cache.preview(&edited, None, &mut images).unwrap().image, first.image);
        assert!(cache.preview(&score("X:1\nK:C\n"), None, &mut images).is_none());
    }
}

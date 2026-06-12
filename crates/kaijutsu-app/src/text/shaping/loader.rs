//! `.ttf` asset loader for [`VelloFont`] (phase 3).
//!
//! Registering the bytes into the shared font collection is load-bearing: the
//! family name it returns is what `layout()` pins as the `FontStack`, so the
//! loader and the shaper must share one collection. Replaces the fork's
//! dependency on its SVG-shared `VectorLoaderError` with a local error.

use bevy::asset::{AssetLoader, LoadContext, io::Reader};
use bevy::reflect::TypePath;

use super::context::{LOCAL_FONT_CONTEXT, get_global_font_context};
use super::font::VelloFont;

#[derive(Debug, thiserror::Error)]
pub enum FontLoaderError {
    #[error("failed to read font asset: {0}")]
    Io(#[from] std::io::Error),
}

/// Register font bytes into the shared collection and capture the family name.
pub(crate) fn load_into_font_context(bytes: Vec<u8>) -> VelloFont {
    LOCAL_FONT_CONTEXT.with_borrow_mut(|font_context| {
        if font_context.is_none() {
            *font_context = Some(get_global_font_context().clone());
        }
        let font_context = font_context.as_mut().unwrap();
        let registered_fonts = font_context.collection.register_fonts(bytes.into(), None);
        let Some((family_id, _font_info)) = registered_fonts.first() else {
            // Crash rather than ship a font that can't be shaped — a silent
            // empty family would render nothing for this asset.
            panic!("font asset registered no families");
        };
        let family_name = font_context
            .collection
            .family_name(*family_id)
            .expect("registered family has no name")
            .to_string();
        VelloFont { family_name }
    })
}

#[derive(Default, TypePath)]
pub struct VelloFontLoader;

impl AssetLoader for VelloFontLoader {
    type Asset = VelloFont;
    type Settings = ();
    type Error = FontLoaderError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(load_into_font_context(bytes))
    }

    fn extensions(&self) -> &[&str] {
        &["ttf"]
    }
}

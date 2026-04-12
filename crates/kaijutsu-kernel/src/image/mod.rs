//! Image generation backends and the streaming generation pipeline.
//!
//! `ImageBackend` is a trait: Gemini today, local Stable Diffusion later.
//! `generate_image_block` creates a placeholder block in `Status::Running`,
//! spawns a background task that streams bytes into CAS staging, seals the
//! result, and flips the block to `Status::Done`.

pub mod backend;
pub mod gemini;
pub mod registry;

pub use backend::{ImageBackend, ImageError, ImageGenOpts, ImageStream};
pub use gemini::GeminiBackend;
pub use registry::ImageBackendRegistry;

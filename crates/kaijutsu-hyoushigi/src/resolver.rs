//! The one capability: the [`Resolver`] trait and the committed view it reads.
//!
//! Everything content-specific is a `Resolver`; the scheduler never sees
//! anything narrower. Adding a modality — text, MIDI, audio, a model turn, a
//! tool call — is a new `impl Resolver`, never a change to the substrate.

use crate::cell::{Cell, ResolverId};
use crate::content::{ContentRef, ContextHash};
use kaijutsu_types::Tick;
use std::time::Duration;
use thiserror::Error;

/// The read-only **committed** view handed to a resolver.
///
/// A resolver is *only* ever handed committed context — an uncommitted cell
/// simply has no view to give. That is how "a speculation reads committed + past
/// + ambient, never another speculation" is enforced *structurally* rather than
/// by a runtime check.
pub trait ResolverCtx {
    /// The playhead position this resolve targets.
    fn now(&self) -> Tick;
    /// An ambient input by key (resolver-interpreted bytes), e.g. a beat counter.
    fn ambient(&self, key: &str) -> Option<Vec<u8>>;
    /// The most recent committed content at or before `tick`.
    fn content_before(&self, tick: Tick) -> Option<ContentRef>;
}

/// What a `resolve` produces: the content **bytes** for this cell, plus any cells
/// it emits into the open future (or appends to the past as a recorded memory).
///
/// The bytes live in the open future (RAM) until commit; only at commit does the
/// engine **crystallize** them to CAS and stamp the committed cell with the
/// resulting [`ContentRef`]. On a squash the resolution — bytes and emitted cells
/// alike — is simply discarded, which is why speculation must be side-effect-free.
/// Emitted cells may never rewrite a committed cell — that is the engine's barrier.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub emitted: Vec<Cell>,
}

impl Resolution {
    pub fn new(bytes: impl Into<Vec<u8>>, mime: impl Into<String>) -> Self {
        Self {
            bytes: bytes.into(),
            mime: mime.into(),
            emitted: Vec::new(),
        }
    }

    pub fn with_emitted(mut self, emitted: Vec<Cell>) -> Self {
        self.emitted = emitted;
        self
    }

    /// The content-addressed reference these bytes crystallize to. The hash *is*
    /// the memoization key — identical bytes → identical ref → no recompute.
    pub fn content_ref(&self) -> ContentRef {
        ContentRef::of(&self.bytes, self.mime.clone())
    }
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("resolver failed: {0}")]
    Failed(String),
}

/// The one content-specific capability. A recipe names a `Resolver` by
/// [`ResolverId`]; the engine looks it up and drives it through the lifecycle.
pub trait Resolver {
    fn id(&self) -> ResolverId;

    /// Wall-clock estimate; feeds lead-time derivation
    /// (`speculate_at = start − beats_for(estimate × safety)`).
    fn estimate_cost(&self, params: &serde_json::Value, rctx: &dyn ResolverCtx) -> Duration;

    /// The equivalence class this resolve is valid for — snapshotted at
    /// `speculate_at`, recomputed at `commit_deadline` to commit-or-squash.
    fn compute_basis(&self, params: &serde_json::Value, rctx: &dyn ResolverCtx) -> ContextHash;

    /// Produce content + emitted cells. Must be idempotent, reversible, and
    /// side-effect-free — it may run speculatively and be discarded on a squash.
    fn resolve(
        &self,
        params: &serde_json::Value,
        rctx: &dyn ResolverCtx,
    ) -> Result<Resolution, ResolveError>;
}

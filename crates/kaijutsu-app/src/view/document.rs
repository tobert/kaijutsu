//! App-side handle to the client-owned multi-context document store.
//!
//! The CRDT document state — and, incrementally, the sync logic that maintains
//! it — lives in [`kaijutsu_client::DocumentStore`]; the app holds it as a Bevy
//! `Resource` and reads snapshots to render. [`DocumentCache`] is a thin newtype
//! so existing call-sites (`doc_cache.active_id()`, `.get_mut()`, …) keep working
//! through `Deref`, and [`CachedDocument`] is just the client's `DocumentEntry`.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use bevy::prelude::*;
use kaijutsu_types::ContextId;

/// The cached per-context document (CRDT doc + compose input + bookkeeping).
/// Lives in the client now; re-exported under the historical name.
pub use kaijutsu_client::DocumentEntry as CachedDocument;

/// Per-context conversation scroll offset — pure *view* state, so it lives
/// app-side rather than on the client's document store (which owns document
/// data, not how the app scrolls it). Saved when leaving a context and restored
/// on switch-back. Entries for evicted contexts are harmless (one `f32` each).
#[derive(Resource, Default)]
pub struct ScrollOffsets(pub HashMap<ContextId, f32>);

/// App-side Bevy `Resource` wrapping the client-owned [`DocumentStore`].
///
/// Deref forwards every accessor (`active_id`, `get`, `get_mut`, `insert`,
/// `set_active`, `iter`, …) to the store, so the app's systems read and mutate
/// document state without re-implementing it.
#[derive(Resource, Default)]
pub struct DocumentCache(pub kaijutsu_client::DocumentStore);

impl Deref for DocumentCache {
    type Target = kaijutsu_client::DocumentStore;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for DocumentCache {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

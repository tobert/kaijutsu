//! Generic async result slot.
//!
//! Wraps `Arc<Mutex<Option<T>>>` for the pattern where an async task fills a result
//! and a Bevy system polls for it.

use bevy::prelude::*;
use std::sync::{Arc, Mutex};

/// Generic async result slot. Insert as a `Resource` for each async fetch.
///
/// Pattern:
/// 1. System calls `slot.sender()` and passes the `Arc` to an async task
/// 2. Async task fills `*arc.lock().unwrap() = Some(result)`
/// 3. Poll system calls `slot.take()` each frame
#[derive(Resource)]
pub struct AsyncSlot<T: Send + Sync + 'static>(Arc<Mutex<Option<T>>>);

impl<T: Send + Sync + 'static> Default for AsyncSlot<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}

impl<T: Send + Sync + 'static> AsyncSlot<T> {
    /// Get a clone of the inner `Arc` to pass to an async task.
    pub fn sender(&self) -> Arc<Mutex<Option<T>>> {
        self.0.clone()
    }

    /// Take the result if available, leaving `None` in its place.
    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap().take()
    }

    /// Clear any pending result.
    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
}

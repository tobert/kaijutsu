//! Context turn locks, cached conversations, and generation-scoped interrupts.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::RwLock as TokioRwLock;
use kaijutsu_types::ContextId;
use crate::ConversationMailbox;
use super::interrupt::ContextInterruptState;

/// Runtime state shared by interactive and headless model turns.
pub struct TurnState {
    conversations: Arc<ConversationCache>,
    pub(super) context_interrupts: Arc<TokioRwLock<HashMap<ContextId, Arc<ContextInterruptState>>>>,
    generation: AtomicU64,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            conversations: Arc::new(ConversationCache::new(64)),
            context_interrupts: Arc::new(TokioRwLock::new(HashMap::new())),
            generation: AtomicU64::new(0),
        }
    }
}

impl TurnState {
    pub fn conversations(&self) -> &Arc<ConversationCache> {
        &self.conversations
    }

    /// Create a fresh `ContextInterruptState` for a new prompt, replacing any previous entry.
    ///
    /// `CancellationToken` cannot be reset — so each prompt gets a new one.
    /// Returns the interrupt state and its generation number. The generation
    /// must be passed to `remove_interrupt` to prevent the race where stream A's
    /// cleanup removes stream B's newer interrupt.
    pub async fn create_interrupt(
        &self,
        context_id: ContextId,
    ) -> (Arc<ContextInterruptState>, u64) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let state = ContextInterruptState::new(generation);
        let mut map = self.context_interrupts.write().await;
        map.insert(context_id, state.clone());
        (state, generation)
    }

    /// Look up an existing interrupt state for a context.
    ///
    /// Returns `None` if the context has no active interrupt (nothing running).
    pub async fn get_interrupt(&self, context_id: ContextId) -> Option<Arc<ContextInterruptState>> {
        let map = self.context_interrupts.read().await;
        map.get(&context_id).cloned()
    }

    /// Remove the interrupt state for a context (called when stream finishes).
    ///
    /// Only removes the entry if `generation` matches the current state's
    /// generation, preventing a stale stream from removing a newer stream's
    /// interrupt state.
    pub async fn remove_interrupt(&self, context_id: ContextId, generation: u64) {
        let mut map = self.context_interrupts.write().await;
        if let Some(state) = map.get(&context_id)
            && state.generation == generation
        {
            map.remove(&context_id);
        }
    }
}

/// A context's turn lock and live conversation. Reset waits for the next
/// lock acquisition; an active turn keeps its mailbox until it finishes.
pub struct ConversationSession {
    mailbox: tokio::sync::Mutex<ConversationMailbox>,
    reset_pending: AtomicBool,
}

impl ConversationSession {
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ConversationMailbox> {
        let mut mailbox = self.mailbox.lock().await;
        if self.reset_pending.swap(false, Ordering::SeqCst) {
            *mailbox = ConversationMailbox::new();
        }
        mailbox
    }
}

/// Per-context turn ownership and cached conversations.
///
/// Lookup and eviction share one registry lock. An entry can leave the
/// registry only when no caller holds it, so every active or waiting turn
/// for a context uses the same mutex. Idle LRU eviction still causes cold
/// hydration on next use; see `docs/conversation-session.md`.
///
/// Also owns the content-hash image cache used by the LLM stream.
pub struct ConversationCache {
    entries: parking_lot::Mutex<HashMap<ContextId, (Arc<ConversationSession>, std::time::Instant)>>,
    max_contexts: usize,
    image_cache: Arc<crate::llm::image_cache::ImageBase64Cache>,
}

impl ConversationCache {
    /// Create a cache with the given idle conversation capacity.
    /// Active sessions may temporarily exceed it.
    pub fn new(max_contexts: usize) -> Self {
        let max_images = max_contexts.saturating_mul(4).max(16);
        Self {
            entries: parking_lot::Mutex::new(HashMap::new()),
            max_contexts,
            image_cache: Arc::new(crate::llm::image_cache::ImageBase64Cache::new(
                max_images,
            )),
        }
    }

    /// Borrow the per-hash image cache shared across contexts.
    pub fn image_cache(&self) -> &crate::llm::image_cache::ImageBase64Cache {
        &self.image_cache
    }

    /// Get the context's session. Hold its lock for the entire turn.
    pub fn get_or_create(&self, ctx: ContextId) -> Arc<ConversationSession> {
        let mut entries = self.entries.lock();
        if let Some((session, accessed)) = entries.get_mut(&ctx) {
            *accessed = std::time::Instant::now();
            return session.clone();
        }

        if entries.len() >= self.max_contexts {
            let oldest = entries.iter()
                .filter(|(_, (session, _))| Arc::strong_count(session) == 1)
                .min_by_key(|(_, (_, accessed))| *accessed)
                .map(|(ctx, _)| *ctx);
            if let Some(ctx) = oldest {
                entries.remove(&ctx);
            }
        }

        let session = Arc::new(ConversationSession {
            mailbox: tokio::sync::Mutex::new(ConversationMailbox::new()),
            reset_pending: AtomicBool::new(false),
        });
        entries.insert(ctx, (session.clone(), std::time::Instant::now()));
        session
    }

    /// Make the next turn hydrate cold without replacing an active turn's
    /// lock. Use after filling a previously hydrated block in place:
    /// `ConversationMailbox::catch_up` only sees blocks it has not folded.
    pub fn evict(&self, ctx: ContextId) {
        let mut entries = self.entries.lock();
        if let Some((session, _)) = entries.get(&ctx) {
            if Arc::strong_count(session) == 1 {
                entries.remove(&ctx);
            } else {
                session.reset_pending.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod conversation_cache_tests {
    use super::*;

    #[tokio::test]
    async fn reset_waits_for_the_active_turn_without_replacing_its_lock() {
        use futures::FutureExt;

        let cache = ConversationCache::new(4);
        let ctx = ContextId::new();
        let first = cache.get_or_create(ctx);
        let mut active = first.lock().await;
        active.catch_up(&[]);

        cache.evict(ctx);
        let second = cache.get_or_create(ctx);
        assert!(Arc::ptr_eq(&first, &second), "reset must preserve turn exclusion");
        assert!(second.lock().now_or_never().is_none(), "next turn must wait");
        assert!(active.is_materialized(), "reset must not interrupt the active turn");
        drop(active);

        let mut next = second.lock().await;
        assert!(!next.is_materialized(), "next turn must hydrate cold");
        next.catch_up(&[]);
        drop(next);
        assert!(first.lock().await.is_materialized(), "reset is consumed once");
    }

    #[test]
    fn simultaneous_first_lookups_share_one_turn_lock() {
        let cache = ConversationCache::new(4);
        let ctx = ContextId::new();
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16).map(|_| scope.spawn(|| {
                barrier.wait();
                cache.get_or_create(ctx)
            })).collect();
            let sessions: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert!(sessions.iter().all(|session| Arc::ptr_eq(&sessions[0], session)));
        });
    }

    #[tokio::test]
    async fn cache_pressure_preserves_active_turns_and_resets_idle_conversations() {
        let cache = ConversationCache::new(1);
        let ctx = ContextId::new();
        let first = cache.get_or_create(ctx);
        first.lock().await.catch_up(&[]);
        let other_ctx = ContextId::new();
        let other = cache.get_or_create(other_ctx);
        other.lock().await.catch_up(&[]);
        assert!(Arc::ptr_eq(&first, &cache.get_or_create(ctx)));
        assert!(first.lock().await.is_materialized());
        drop(other);
        drop(first);

        let third = cache.get_or_create(ContextId::new());
        let cold = cache.get_or_create(other_ctx);
        assert!(!cold.lock().await.is_materialized());
        drop(third);
    }

}

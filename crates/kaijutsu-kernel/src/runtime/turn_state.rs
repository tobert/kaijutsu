//! Context turn locks, cached conversations, and turn-scoped interrupts.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use kaijutsu_types::{ContextId, TurnId};
use crate::ConversationMailbox;
use super::interrupt::ContextInterruptState;

struct ActiveTurn {
    began: std::time::Instant,
    interrupt: Arc<ContextInterruptState>,
}

type ActiveTurns = HashMap<ContextId, std::collections::BTreeMap<TurnId, ActiveTurn>>;

/// Owns one admitted turn's liveness and interrupt registration.
/// Dropping one lease cannot clear another turn in the same context.
#[must_use]
pub struct TurnLease {
    active: Arc<parking_lot::Mutex<ActiveTurns>>,
    context: ContextId,
    id: TurnId,
    delivery: Option<crate::hyoushigi::model::OutputDelivery>,
    blocks: std::cell::RefCell<Vec<kaijutsu_types::BlockId>>,
    interrupt: Arc<ContextInterruptState>,
}

impl TurnLease {
    pub fn id(&self) -> TurnId { self.id }
    pub(super) fn track_block(&self, block: kaijutsu_types::BlockId) {
        assert_eq!(block.context_id, self.context, "a turn owns blocks in its context");
        self.blocks.borrow_mut().push(block);
    }
    pub(super) fn settle_blocks(&self, documents: &crate::SharedBlockStore) -> Result<usize, String> {
        documents.fail_running_blocks(self.context, &self.blocks.borrow()).map_err(|error| error.to_string())
    }

    pub(crate) fn deliver_to(&mut self, delivery: crate::hyoushigi::model::OutputDelivery) {
        assert!(self.delivery.is_none(), "one score delivery per admitted turn");
        self.delivery = Some(delivery);
    }
    pub(super) async fn finish(mut self, event: &crate::flows::TurnFlow) {
        assert_eq!(self.id, event.turn_id(), "terminal outcome belongs to its lease");
        if let Some(delivery) = self.delivery.take() {
            // A dropped receiver means the score has already cancelled or settled.
            delivery.finish(event, self.interrupt.clone()).await;
        }
    }
    pub(crate) fn interrupt(&self) -> Arc<ContextInterruptState> { self.interrupt.clone() }
    pub(crate) fn context(&self) -> ContextId { self.context }
}

impl Drop for TurnLease {
    fn drop(&mut self) {
        let mut active = self.active.lock();
        if let Some(turns) = active.get_mut(&self.context) {
            turns.remove(&self.id);
            if turns.is_empty() { active.remove(&self.context); }
        }
    }
}

/// Runtime state shared by interactive and headless model turns.
pub struct TurnState {
    conversations: Arc<ConversationCache>,
    active: Arc<parking_lot::Mutex<ActiveTurns>>,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            conversations: Arc::new(ConversationCache::new(64)),
            active: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }
}

impl TurnState {
    pub fn conversations(&self) -> &Arc<ConversationCache> { &self.conversations }

    pub fn begin(&self, context: ContextId) -> TurnLease {
        self.begin_with_interrupt(context, ContextInterruptState::new())
    }

    /// Automatic continuation may reserve an idle context, never queue behind
    /// another request that already owns that context's next turn.
    pub(super) fn begin_if_idle(&self, context: ContextId) -> Option<TurnLease> {
        self.register(context, ContextInterruptState::new(), true)
    }

    pub(super) fn begin_with_interrupt(&self, context: ContextId, interrupt: Arc<ContextInterruptState>) -> TurnLease {
        self.register(context, interrupt, false).expect("unconditional turn admission")
    }

    fn register(&self, context: ContextId, interrupt: Arc<ContextInterruptState>, idle_only: bool) -> Option<TurnLease> {
        let mut active = self.active.lock();
        if idle_only && active.contains_key(&context) { return None; }
        let id = TurnId::new();
        active.entry(context).or_default().insert(id, ActiveTurn {
            began: std::time::Instant::now(), interrupt: interrupt.clone(),
        });
        Some(TurnLease { active: self.active.clone(), context, id, delivery: None, blocks: Default::default(), interrupt })
    }

    pub fn active_count(&self, context: ContextId) -> usize {
        self.active.lock().get(&context).map_or(0, |turns| turns.len())
    }

    /// Interrupt every accepted turn, including work waiting for the context lock.
    pub fn interrupt(&self, context: ContextId, immediate: bool) -> bool {
        let active = self.active.lock();
        let Some(turns) = active.get(&context) else { return false; };
        for turn in turns.values() {
            if immediate { turn.interrupt.hard(); } else { turn.interrupt.soft(); }
        }
        true
    }

    pub fn in_flight(&self) -> Vec<(ContextId, std::time::Duration)> {
        let now = std::time::Instant::now();
        let mut rows: Vec<_> = self.active.lock().iter().map(|(context, turns)| {
            let began = turns.values().map(|turn| turn.began).min().expect("registered context has a turn");
            (*context, now.saturating_duration_since(began))
        }).collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        rows
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
mod turn_lease_tests {
    use super::*;

    #[test]
    fn interruption_reaches_every_accepted_turn_and_ends_with_its_lease() {
        let state = TurnState::default();
        let context = ContextId::new();
        let first = state.begin(context);
        let second = state.begin(context);
        assert!(state.begin_if_idle(context).is_none());
        assert!(state.interrupt(context, false));
        for lease in [&first, &second] {
            assert!(lease.interrupt.stop_after_turn.load(Ordering::Relaxed));
            assert!(!lease.interrupt.cancel.is_cancelled());
        }
        drop(first);
        assert_eq!(state.active_count(context), 1);
        assert!(state.interrupt(context, true));
        assert!(second.interrupt.cancel.is_cancelled());
        drop(second);
        assert!(!state.interrupt(context, true));
        let next = state.begin_if_idle(context).unwrap();
        assert!(!next.interrupt.cancel.is_cancelled());
        assert!(!next.interrupt.stop_after_turn.load(Ordering::Relaxed));
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

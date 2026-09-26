//! Context turn locks, cached conversations, and turn-scoped interrupts.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use kaijutsu_types::{BlockId, ContextId, PrincipalId, SessionId, TurnId};
use crate::ConversationMailbox;
use super::interrupt::ContextInterruptState;

struct ActiveTurn {
    began: std::time::Instant,
    interrupt: Arc<ContextInterruptState>,
}

type ActiveTurns = HashMap<ContextId, std::collections::BTreeMap<TurnId, ActiveTurn>>;

/// Input that arrived while a turn runs, which that turn has not yet
/// delivered to its model. The block is already durable; `wake` says what
/// the input asks of the turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveInput {
    pub block: BlockId,
    pub wake: Wake,
}

/// Whether live input earns the turn another inference when it arrives
/// during the final one, and what starts after the turn if it was never
/// delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// A player's submit continues the turn, and starts the next turn.
    Submit { principal: PrincipalId, session: SessionId },
    /// A completion notice continues the turn when its notice allows an
    /// automatic resume; after the turn, its continuation policy decides.
    Completion { source: super::completion_notice::Source, resume_allowed: bool },
    /// A drift arrival rides any delivery but never extends or starts a
    /// turn, just as a drift into an idle context starts none.
    Drift,
}

impl Wake {
    pub(crate) fn continues_turn(&self) -> bool {
        match self {
            Self::Submit { .. } => true,
            Self::Completion { resume_allowed, .. } => *resume_allowed,
            Self::Drift => false,
        }
    }
}

/// The running turn's door for new input. The turn holding a context's
/// conversation lock opens it and closes it before releasing the lock.
struct Ingress {
    turn: TurnId,
    interrupt: Arc<ContextInterruptState>,
    /// Undelivered input, oldest first. Each keeps its own wake.
    pending: Vec<LiveInput>,
    /// Blocks carried by an inference that completed. Delivery reads the
    /// log, so input can reach the model before it is offered.
    delivered: std::collections::HashSet<BlockId>,
}

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
    /// Whether a timed delivery waits on this turn. Work a beat is waiting for
    /// takes no inference its caller did not budget for.
    pub(crate) fn owes_timed_delivery(&self) -> bool { self.delivery.is_some() }
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
    ingress: parking_lot::Mutex<Ingresses>,
}

#[derive(Default)]
struct Ingresses {
    open: HashMap<ContextId, Ingress>,
    /// Blocks the context's last finished turn delivered. A submit's offer
    /// can arrive after the turn that delivered its block has closed.
    answered: HashMap<ContextId, std::collections::HashSet<BlockId>>,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            conversations: Arc::new(ConversationCache::new(64)),
            active: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            ingress: parking_lot::Mutex::new(Ingresses::default()),
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

    /// Open `context`'s ingress for the turn that holds its conversation lock.
    pub(crate) fn open_ingress(&self, context: ContextId, turn: TurnId, interrupt: Arc<ContextInterruptState>) {
        let mut ingress = self.ingress.lock();
        assert!(!ingress.open.contains_key(&context), "one turn at a time holds a context's conversation lock");
        // This turn's hydration folds whatever the last turn answered.
        ingress.answered.remove(&context);
        ingress.open.insert(context, Ingress { turn, interrupt, pending: Vec::new(), delivered: Default::default() });
    }

    /// Hand input to the context's running turn. Refused when no turn is
    /// running or the running turn is stopping; the caller then handles the
    /// input as it would for an idle context. Input the last finished turn
    /// already delivered is accepted, since that turn answered it.
    pub(crate) fn offer_input(&self, context: ContextId, input: LiveInput) -> bool {
        let mut ingress = self.ingress.lock();
        let Some(open) = ingress.open.get_mut(&context) else {
            return ingress.answered.get(&context).is_some_and(|answered| answered.contains(&input.block));
        };
        if open.interrupt.cancel.is_cancelled() || open.interrupt.stop_after_turn.load(Ordering::Relaxed) {
            return false;
        }
        if !open.delivered.contains(&input.block) && !open.pending.iter().any(|p| p.block == input.block) {
            open.pending.push(input);
        }
        true
    }

    pub(super) fn pending_inputs(&self, context: ContextId, turn: TurnId) -> Vec<LiveInput> {
        let ingress = self.ingress.lock();
        let open = ingress.open.get(&context).expect("a delivering turn holds its ingress open");
        assert_eq!(open.turn, turn, "ingress belongs to the turn that opened it");
        open.pending.clone()
    }

    /// Record that an inference carrying `blocks` completed. Input offered
    /// after them stays pending.
    pub(super) fn settle_input(&self, context: ContextId, turn: TurnId, blocks: &[BlockId]) {
        let mut ingress = self.ingress.lock();
        let open = ingress.open.get_mut(&context).expect("a delivering turn holds its ingress open");
        assert_eq!(open.turn, turn, "ingress belongs to the turn that opened it");
        open.delivered.extend(blocks.iter().copied());
        let delivered = &open.delivered;
        open.pending.retain(|input| !delivered.contains(&input.block));
    }

    /// Close the ingress and return input the turn accepted but never delivered.
    pub(crate) fn close_ingress(&self, context: ContextId, turn: TurnId) -> Vec<LiveInput> {
        let mut ingress = self.ingress.lock();
        let open = ingress.open.remove(&context).expect("the turn that opened an ingress closes it");
        assert_eq!(open.turn, turn, "ingress belongs to the turn that opened it");
        ingress.answered.insert(context, open.delivered);
        open.pending
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
mod ingress_tests {
    use super::*;

    fn input(block: BlockId) -> LiveInput {
        LiveInput { block, wake: Wake::Submit { principal: PrincipalId::new(), session: SessionId::new() } }
    }

    #[test]
    fn only_a_running_turn_that_is_not_stopping_accepts_input() {
        let state = TurnState::default();
        let context = ContextId::new();
        let note = input(BlockId::new(context, PrincipalId::new(), 1));
        assert!(!state.offer_input(context, note.clone()), "no turn is running");

        let lease = state.begin(context);
        state.open_ingress(context, lease.id(), lease.interrupt());
        assert!(state.offer_input(context, note.clone()));
        assert_eq!(state.pending_inputs(context, lease.id()), vec![note.clone()]);

        lease.interrupt().soft();
        assert!(!state.offer_input(context, note.clone()), "a stopping turn refuses new input");
        let cancelled = state.begin(ContextId::new());
        state.open_ingress(cancelled.context(), cancelled.id(), cancelled.interrupt());
        cancelled.interrupt().hard();
        assert!(!state.offer_input(cancelled.context(), note.clone()), "a cancelled turn refuses new input");
        assert!(state.close_ingress(cancelled.context(), cancelled.id()).is_empty());
        assert_eq!(state.close_ingress(context, lease.id()), vec![note.clone()], "accepted input outlives the refusal");
        assert!(!state.offer_input(context, input(BlockId::new(context, PrincipalId::new(), 2))),
            "a closed ingress refuses input it did not deliver");
    }

    #[test]
    fn settling_older_input_keeps_newer_input_pending() {
        let state = TurnState::default();
        let context = ContextId::new();
        let principal = PrincipalId::new();
        let (older, newer) = (input(BlockId::new(context, principal, 1)), input(BlockId::new(context, principal, 2)));
        let lease = state.begin(context);
        state.open_ingress(context, lease.id(), lease.interrupt());
        assert!(state.offer_input(context, older.clone()));
        assert!(state.offer_input(context, newer.clone()));
        state.settle_input(context, lease.id(), &[older.block]);
        assert_eq!(state.pending_inputs(context, lease.id()), vec![newer.clone()]);
        state.settle_input(context, lease.id(), &[newer.block]);
        assert!(state.close_ingress(context, lease.id()).is_empty());
    }

    /// Each pending input keeps its own wake: a drift arriving after a note
    /// must not cost the note its follow-up turn.
    #[test]
    fn a_later_drift_keeps_an_earlier_submit_pending() {
        let state = TurnState::default();
        let context = ContextId::new();
        let principal = PrincipalId::new();
        let note = input(BlockId::new(context, principal, 1));
        let drift = LiveInput { block: BlockId::new(context, principal, 2), wake: Wake::Drift };
        let lease = state.begin(context);
        state.open_ingress(context, lease.id(), lease.interrupt());
        assert!(state.offer_input(context, note.clone()));
        assert!(state.offer_input(context, drift.clone()));
        assert!(state.offer_input(context, note.clone()), "a repeated offer is accepted once");
        assert_eq!(state.close_ingress(context, lease.id()), vec![note, drift]);
    }

    #[test]
    fn only_submits_and_resumable_completions_continue_a_turn() {
        let source = super::super::completion_notice::Source::Shell("op".into());
        assert!(input(BlockId::new(ContextId::new(), PrincipalId::new(), 1)).wake.continues_turn());
        assert!(Wake::Completion { source: source.clone(), resume_allowed: true }.continues_turn());
        assert!(!Wake::Completion { source, resume_allowed: false }.continues_turn());
        assert!(!Wake::Drift.continues_turn());
    }

    #[test]
    fn input_delivered_before_its_offer_is_not_pending() {
        let state = TurnState::default();
        let context = ContextId::new();
        let note = input(BlockId::new(context, PrincipalId::new(), 1));
        let lease = state.begin(context);
        state.open_ingress(context, lease.id(), lease.interrupt());
        state.settle_input(context, lease.id(), &[note.block]);
        assert!(state.offer_input(context, note), "the running turn still accepts it");
        assert!(state.close_ingress(context, lease.id()).is_empty(), "and owes it no further turn");
    }

    #[test]
    fn input_delivered_by_a_finished_turn_is_not_offered_again() {
        let state = TurnState::default();
        let context = ContextId::new();
        let note = input(BlockId::new(context, PrincipalId::new(), 1));
        let lease = state.begin(context);
        state.open_ingress(context, lease.id(), lease.interrupt());
        state.settle_input(context, lease.id(), &[note.block]);
        assert!(state.close_ingress(context, lease.id()).is_empty());
        assert!(state.offer_input(context, note),
            "the finished turn answered it, so its submit starts no turn of its own");
        drop(lease);

        let next = state.begin(context);
        state.open_ingress(context, next.id(), next.interrupt());
        let later = input(BlockId::new(context, PrincipalId::new(), 2));
        assert!(state.close_ingress(context, next.id()).is_empty());
        assert!(!state.offer_input(context, later), "only answered input is absorbed after a close");
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

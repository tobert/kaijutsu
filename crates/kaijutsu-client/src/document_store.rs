//! Multi-context document store.
//!
//! Holds a [`ContextMirror`] (the change-feed applier, `context_feed.rs`) per
//! joined context, enabling instant context switching and LRU eviction. Holds
//! mirrors and feed receivers; callers still drive snapshot fetching and
//! recovery after reconnects.
//!
//! Bevy-free on purpose: the app wraps this in a `Resource` newtype, but the
//! store itself is plain Rust so its logic is unit-testable without a world.
//!
//! **Migration note (docs/change-feed.md).** The block document used to be a
//! diamond-types-backed document (`SyncedDocument`), fed by `ServerEvent::BlockTextOps`
//! and friends. It is now a plain projection (`ContextMirror`) fed by a
//! per-context change feed (`ActorHandle::subscribe_context` +
//! `get_blocks_versioned`). The store's job shrank to match: it no longer
//! decides *when* a document is stale (the feed says so, per context, via
//! `FeedEvent::Resubscribed`/`Terminated`) — it only holds the mirror, holds
//! the live feed receiver alongside it, and drains that receiver on request.
//! The old `generation`/`stale_active`/`mark_synced` staleness tracking is
//! gone with it; there is nothing left for it to gate.
//!
//! **Compose input (Lane C slice 3, retired Lane C slice 4).** The compose
//! draft used to be a separate `SyncedInput` kernel document, hydrated via
//! `getInputState` and kept live by `InputTextOps`/`InputCleared` server
//! events. It is now an ordinary block (`Role::User`, `Status::Draft`,
//! `ephemeral`, one per `(context, principal)`) that rides the same change
//! feed as every other block, so it already lands in `mirror.blocks()` — see
//! [`crate::draft_ledger::draft_of`]. `SyncedInput` and the input-doc wire events
//! it depended on are deleted (2026-08-16); this store never held one.

use std::collections::HashMap;
use std::time::Instant;

use kaijutsu_types::{BlockSnapshot, ContextId};
use tokio::sync::mpsc;

use crate::{ContextMirror, FeedEvent, MirrorError};

/// A cached document for a single context: its change-feed mirror, the live
/// feed receiver that keeps it current, a monotonic recovery epoch for
/// rejecting obsolete async responses, and the bookkeeping the store needs
/// to pick eviction victims.
pub struct DocumentEntry {
    /// The context's projected block state, maintained from the change feed
    /// (docs/change-feed.md). No text engine lives here.
    pub mirror: ContextMirror,
    /// The context's live change feed. `None` between a `Terminated` signal
    /// and the app re-subscribing — draining is a no-op while it's `None`,
    /// never a panic.
    pub feed: Option<mpsc::Receiver<FeedEvent>>,
    /// Monotonic epoch bumped on every install and every recovery signal
    /// (`Resubscribed`, `Desynced`, `Terminated`). Async snapshot/re-subscribe
    /// responses carry the epoch they were spawned for so obsolete responses
    /// are rejected rather than clobbering newer state.
    pub recovery_epoch: u64,
    /// Context name (e.g. the kernel_id or user-supplied name).
    pub context_name: String,
    /// When this document was last accessed (for LRU eviction).
    pub last_accessed: Instant,
}

impl DocumentEntry {
    /// A freshly-installed entry around `mirror`, optionally already wired to
    /// a live `feed`.
    pub fn new(
        mirror: ContextMirror,
        feed: Option<mpsc::Receiver<FeedEvent>>,
        context_name: String,
    ) -> Self {
        Self {
            mirror,
            feed,
            recovery_epoch: 1,
            context_name,
            last_accessed: Instant::now(),
        }
    }

    fn bump_epoch(&mut self) -> u64 {
        self.recovery_epoch = self.recovery_epoch.saturating_add(1);
        self.recovery_epoch
    }

    /// Drain every event currently queued on this entry's feed, applying
    /// `Changed` deliveries to the mirror in order (rule 16) and reporting
    /// what happened. Non-blocking — returns as soon as the channel has
    /// nothing more to give right now, or immediately if there is no feed to
    /// drain.
    ///
    /// On `Resubscribed`, `Desynced`, or `Terminated`, the mirror switches to
    /// buffering mode (`ContextMirror::begin_rehydrate`, preserving the last
    /// good blocks on screen while buffering incoming deltas) and bumps
    /// `recovery_epoch`. Callers pass the returned `epoch` to
    /// [`DocumentStore::apply_snapshot_for_epoch`] or
    /// [`DocumentStore::install_for_epoch`] so obsolete responses are rejected.
    pub fn drain_feed(&mut self) -> Vec<FeedSignal> {
        let mut signals = Vec::new();
        let Some(rx) = self.feed.as_mut() else {
            return signals;
        };
        loop {
            match rx.try_recv() {
                Ok(FeedEvent::Changed(delivery)) => match self.mirror.receive(delivery) {
                    Ok(()) => signals.push(FeedSignal::Updated),
                    Err(e) => {
                        self.mirror.begin_rehydrate();
                        self.recovery_epoch = self.recovery_epoch.saturating_add(1);
                        signals.push(FeedSignal::Desynced {
                            error: e,
                            epoch: self.recovery_epoch,
                        });
                    }
                },
                Ok(FeedEvent::Resubscribed) => {
                    // Same receiver, new epoch: switch to buffering mode so
                    // post-reconnect deltas wait for the snapshot while last
                    // good blocks stay renderable.
                    self.mirror.begin_rehydrate();
                    self.recovery_epoch = self.recovery_epoch.saturating_add(1);
                    signals.push(FeedSignal::Resubscribed {
                        epoch: self.recovery_epoch,
                    });
                }
                Ok(FeedEvent::Terminated { .. }) => {
                    // Not resumable (rule 28): the receiver is spent, so
                    // there is nothing left to drain this call.
                    self.mirror.begin_rehydrate();
                    self.feed = None;
                    self.recovery_epoch = self.recovery_epoch.saturating_add(1);
                    signals.push(FeedSignal::Terminated {
                        epoch: self.recovery_epoch,
                    });
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // The sender side is gone without an explicit
                    // `Terminated` (e.g. the actor itself tore down) — same
                    // recovery as if it had said so.
                    self.mirror.begin_rehydrate();
                    self.feed = None;
                    self.recovery_epoch = self.recovery_epoch.saturating_add(1);
                    signals.push(FeedSignal::Terminated {
                        epoch: self.recovery_epoch,
                    });
                    break;
                }
            }
        }
        signals
    }
}

/// What draining one context's feed produced. `Updated` needs no reaction
/// beyond a redraw; the other three all mean "the mirror entered recovery at
/// `epoch` — fetch a fresh snapshot," and the caller (which owns the actor
/// handle, i.e. I/O the store cannot perform itself) decides how:
///
/// - [`Self::Resubscribed`] / [`Self::Desynced`]: the existing feed receiver
///   is still good — fetch with `ActorHandle::fetch_context_snapshot` and
///   hydrate via [`DocumentStore::apply_snapshot_for_epoch`].
/// - [`Self::Terminated`]: the receiver is spent — call
///   `ActorHandle::hydrate_context` for a new mirror + receiver and land via
///   [`DocumentStore::install_for_epoch`].
#[derive(Debug, PartialEq, Eq)]
pub enum FeedSignal {
    /// A delivery applied cleanly (or was buffered pre-hydration).
    Updated,
    /// The actor already re-subscribed on the same receiver.
    Resubscribed { epoch: u64 },
    /// The feed ended and will not resume; `DocumentEntry::feed` is now
    /// `None`.
    Terminated { epoch: u64 },
    /// A delivery could not be applied to the mirror — never expected under
    /// normal operation (see `drain_feed`'s doc comment). Carried so the
    /// caller can surface the fault instead of masking it as an ordinary
    /// reconnect.
    Desynced { error: MirrorError, epoch: u64 },
}

/// Multi-context document store — the authoritative source for all document
/// state. The active entry is what the renderer draws.
pub struct DocumentStore {
    /// Map from context_id → cached document state.
    documents: HashMap<ContextId, DocumentEntry>,
    /// Currently active (rendered) context_id.
    active_id: Option<ContextId>,
    /// Most-recently-used context IDs (front = most recent).
    mru: Vec<ContextId>,
    /// Maximum number of cached documents before LRU eviction.
    max_cached: usize,
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: HashMap::new(),
            active_id: None,
            mru: Vec::new(),
            max_cached: 8,
        }
    }
}

impl DocumentStore {
    /// Get the active context ID.
    pub fn active_id(&self) -> Option<ContextId> {
        self.active_id
    }

    /// Get a reference to a cached document by context_id.
    pub fn get(&self, context_id: ContextId) -> Option<&DocumentEntry> {
        self.documents.get(&context_id)
    }

    /// Get a mutable reference to a cached document by context_id.
    pub fn get_mut(&mut self, context_id: ContextId) -> Option<&mut DocumentEntry> {
        self.documents.get_mut(&context_id)
    }

    /// Check if a document is cached.
    pub fn contains(&self, context_id: ContextId) -> bool {
        self.documents.contains_key(&context_id)
    }

    /// Insert a new cached document. Evicts LRU entry if at capacity.
    pub fn insert(&mut self, context_id: ContextId, cached: DocumentEntry) {
        if self.documents.len() >= self.max_cached {
            self.evict_lru();
        }
        self.documents.insert(context_id, cached);
        self.touch_mru(context_id);
    }

    /// Set the active document. Returns the previous active_id if changed.
    pub fn set_active(&mut self, context_id: ContextId) -> Option<ContextId> {
        let previous = self.active_id.take();
        self.active_id = Some(context_id);
        self.touch_mru(context_id);

        if let Some(doc) = self.documents.get_mut(&context_id) {
            doc.last_accessed = Instant::now();
        }

        previous
    }

    /// Get MRU-ordered context IDs (most recent first).
    pub fn mru_ids(&self) -> &[ContextId] {
        &self.mru
    }

    /// Number of cached documents.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Whether the store holds no documents.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Iterate over all cached documents.
    pub fn iter(&self) -> impl Iterator<Item = (ContextId, &DocumentEntry)> {
        self.documents.iter().map(|(&k, v)| (k, v))
    }

    // ── change-feed lifecycle (the app holds none of this) ───────────────

    /// Install (or replace) a context's mirror and feed receiver wholesale.
    /// The initial subscribe+hydrate for a newly-joined context, or the
    /// `FeedEvent::Terminated` recovery (a brand-new receiver replaces the
    /// dead one). `feed: None` is a legitimate state — a hydrate that
    /// couldn't even get an initial snapshot still wants *something* to
    /// render, so an empty, unfed mirror is installed rather than nothing.
    pub fn install(
        &mut self,
        context_id: ContextId,
        mirror: ContextMirror,
        feed: Option<mpsc::Receiver<FeedEvent>>,
        name: impl FnOnce() -> String,
    ) {
        if let Some(entry) = self.documents.get_mut(&context_id) {
            entry.mirror = mirror;
            entry.feed = feed;
            entry.bump_epoch();
        } else {
            self.insert(context_id, DocumentEntry::new(mirror, feed, name()));
        }
    }

    /// Epoch-checked [`Self::install`].
    ///
    /// When `expected_epoch` is `Some(epoch)`, installs only if `context_id`
    /// is still present at that exact recovery epoch; otherwise rejects the
    /// obsolete response and returns `false`. When `expected_epoch` is `None`
    /// (initial join), installs unconditionally and returns `true`.
    pub fn install_for_epoch(
        &mut self,
        context_id: ContextId,
        expected_epoch: Option<u64>,
        mirror: ContextMirror,
        feed: Option<mpsc::Receiver<FeedEvent>>,
        name: impl FnOnce() -> String,
    ) -> bool {
        if let Some(expected) = expected_epoch {
            let Some(entry) = self.documents.get_mut(&context_id) else {
                return false;
            };
            if entry.recovery_epoch != expected {
                return false;
            }
            entry.mirror = mirror;
            entry.feed = feed;
            entry.bump_epoch();
            return true;
        }
        self.install(context_id, mirror, feed, name);
        true
    }

    /// Re-hydrate an already-followed context's mirror from a fresh
    /// snapshot, keeping its existing feed receiver untouched — the
    /// `Resubscribed`/`Desynced` recovery path (docs/change-feed.md rules
    /// 21-26, replayed on the buffering mirror `drain_feed` left behind).
    /// A context that was left or evicted before the fetch completed is a
    /// no-op, not an error.
    pub fn apply_snapshot(
        &mut self,
        context_id: ContextId,
        blocks: Vec<BlockSnapshot>,
        version: u64,
    ) -> Result<(), MirrorError> {
        let Some(entry) = self.documents.get_mut(&context_id) else {
            return Ok(());
        };
        entry.mirror.apply_snapshot(blocks, version)
    }

    /// Epoch-checked [`Self::apply_snapshot`].
    ///
    /// Applies the snapshot only if `context_id` is still present at `epoch`.
    /// Returns `Ok(true)` if applied, or `Ok(false)` if the response is
    /// obsolete (evicted or superseded by a newer recovery/install).
    pub fn apply_snapshot_for_epoch(
        &mut self,
        context_id: ContextId,
        epoch: u64,
        blocks: Vec<BlockSnapshot>,
        version: u64,
    ) -> Result<bool, MirrorError> {
        let Some(entry) = self.documents.get_mut(&context_id) else {
            return Ok(false);
        };
        if entry.recovery_epoch != epoch {
            return Ok(false);
        }
        entry.mirror.apply_snapshot(blocks, version)?;
        Ok(true)
    }

    /// Current recovery epoch for `context_id`, if cached.
    pub fn recovery_epoch(&self, context_id: ContextId) -> Option<u64> {
        self.documents.get(&context_id).map(|e| e.recovery_epoch)
    }

    /// Drain every followed context's change feed once. Returns
    /// `(context_id, signal)` pairs in no particular cross-context order —
    /// the caller reacts to each; see [`FeedSignal`] for what each variant
    /// asks for.
    pub fn drain_feeds(&mut self) -> Vec<(ContextId, FeedSignal)> {
        let mut out = Vec::new();
        for (&ctx_id, entry) in self.documents.iter_mut() {
            for signal in entry.drain_feed() {
                out.push((ctx_id, signal));
            }
        }
        out
    }

    /// Move a context_id to the front of the MRU list.
    fn touch_mru(&mut self, context_id: ContextId) {
        self.mru.retain(|&id| id != context_id);
        self.mru.insert(0, context_id);
    }

    /// Evict the least-recently-used document (never the active one).
    fn evict_lru(&mut self) {
        let evict_id = self
            .mru
            .iter()
            .rev()
            .find(|&&id| self.active_id != Some(id))
            .copied();

        if let Some(id) = evict_id {
            self.documents.remove(&id);
            self.mru.retain(|&mid| mid != id);
            log::info!("DocumentStore: evicted LRU document {}", id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContextChange, ContextDelivery};
    use kaijutsu_types::{BlockKind, PrincipalId, Role};

    fn ctx() -> ContextId {
        ContextId::new()
    }

    fn block(context_id: ContextId, seq: u64, content: &str) -> BlockSnapshot {
        let id = kaijutsu_types::BlockId::new(context_id, PrincipalId::new(), seq);
        let mut snap = BlockSnapshot::text(id, None, Role::User, content);
        snap.kind = BlockKind::Text;
        snap
    }

    /// A delivery batching several mutations, each carrying its OWN version —
    /// the real wire shape (`context_feed.rs`'s `apply()`: "two carried events
    /// never share a version"). A delivery with a single change just gives
    /// that one change `version` as its version.
    fn versioned_delivery(context_id: ContextId, events: Vec<(u64, ContextChange)>) -> ContextDelivery {
        let version = events.last().map(|(v, _)| *v).unwrap_or(0);
        ContextDelivery {
            context_id,
            events: events
                .into_iter()
                .map(|(version, change)| crate::context_feed::VersionedChange { version, change })
                .collect(),
            version,
        }
    }

    #[test]
    fn install_creates_then_apply_snapshot_updates() {
        let mut store = DocumentStore::default();
        let c = ctx();
        store.install(c, ContextMirror::new(c), None, || "ctx".into());
        assert!(store.contains(c));
        assert_eq!(store.get(c).unwrap().mirror.blocks().len(), 0);

        store.apply_snapshot(c, vec![block(c, 1, "a")], 1).unwrap();
        assert_eq!(store.get(c).unwrap().mirror.blocks().len(), 1);
        assert_eq!(store.get(c).unwrap().mirror.version(), 1);

        store
            .apply_snapshot(c, vec![block(c, 1, "a"), block(c, 2, "b")], 2)
            .unwrap();
        assert_eq!(store.get(c).unwrap().mirror.blocks().len(), 2);
        assert_eq!(store.get(c).unwrap().mirror.version(), 2);
    }

    #[test]
    fn install_over_an_existing_entry_replaces_mirror_and_feed_in_place() {
        // The `FeedEvent::Terminated` recovery path: a brand-new mirror and
        // receiver replace the dead ones on the SAME entry, so MRU/active
        // bookkeeping survives the recovery untouched.
        let mut store = DocumentStore::default();
        let c = ctx();
        store.install(c, ContextMirror::new(c), None, || "ctx".into());
        store.set_active(c);

        let mut fresh = ContextMirror::new(c);
        fresh.apply_snapshot(vec![block(c, 1, "reborn")], 5).unwrap();
        let (_tx, rx) = mpsc::channel(1);
        store.install(c, fresh, Some(rx), || panic!("existing entry must not re-derive a name"));

        assert_eq!(store.len(), 1, "install on an existing context must not duplicate the entry");
        assert_eq!(store.active_id(), Some(c), "active_id survives a mid-session recovery");
        assert_eq!(store.get(c).unwrap().mirror.version(), 5);
    }

    #[test]
    fn apply_snapshot_for_an_uninstalled_context_is_a_no_op() {
        let mut store = DocumentStore::default();
        let c = ctx();
        assert!(store.apply_snapshot(c, vec![], 1).is_ok());
        assert!(!store.contains(c), "a snapshot for a context never installed must not resurrect one");
    }

    #[test]
    fn draining_an_uninstalled_or_unfed_store_yields_nothing() {
        let mut store = DocumentStore::default();
        assert!(store.drain_feeds().is_empty());

        let c = ctx();
        store.install(c, ContextMirror::new(c), None, || "ctx".into());
        assert!(
            store.drain_feeds().is_empty(),
            "an entry with no feed drains to nothing, not a panic"
        );
    }

    /// The point of the whole migration, proven at the store's own seam: a
    /// delivery arrives on a plain channel carrying `ContextChange` — never
    /// storage-engine bytes, never a `ServerEvent::BlockTextOps` — and reaches
    /// the document's block state via `DocumentStore::drain_feeds` alone.
    #[test]
    fn draining_the_feed_applies_a_text_delivery_with_no_text_engine_involved() {
        let mut store = DocumentStore::default();
        let c = ctx();
        let b = block(c, 1, "");
        let id = b.id;

        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![b], 1).unwrap();
        let (tx, rx) = mpsc::channel(8);
        store.install(c, mirror, Some(rx), || "ctx".into());

        tx.try_send(FeedEvent::Changed(versioned_delivery(
            c,
            vec![
                (
                    2,
                    ContextChange::TextAppended {
                        block_id: id,
                        suffix: "hello".into(),
                    },
                ),
                (
                    3,
                    ContextChange::StatusChanged {
                        block_id: id,
                        status: kaijutsu_types::Status::Done,
                    },
                ),
            ],
        )))
        .expect("buffered channel has room");

        let signals = store.drain_feeds();
        assert_eq!(signals, vec![(c, FeedSignal::Updated)]);

        let entry = store.get(c).unwrap();
        let seen = entry.mirror.block(&id).unwrap();
        assert_eq!(seen.content, "hello");
        assert_eq!(seen.status, kaijutsu_types::Status::Done);
        assert_eq!(entry.mirror.version(), 3);
    }

    #[test]
    fn a_resubscribed_signal_buffers_the_mirror_and_keeps_the_receiver() {
        let mut store = DocumentStore::default();
        let c = ctx();
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![block(c, 1, "before")], 3).unwrap();
        let (tx, rx) = mpsc::channel(8);
        store.install(c, mirror, Some(rx), || "ctx".into());

        tx.try_send(FeedEvent::Resubscribed).unwrap();
        let signals = store.drain_feeds();
        assert_eq!(signals, vec![(c, FeedSignal::Resubscribed { epoch: 2 })]);

        let entry = store.get(c).unwrap();
        assert!(!entry.mirror.is_hydrated(), "Resubscribed must switch mirror into buffering mode");
        assert_eq!(entry.mirror.blocks().len(), 1, "last good blocks stay visible during recovery");
        assert!(entry.feed.is_some(), "Resubscribed reuses the SAME receiver, never re-subscribes");
    }

    #[test]
    fn a_terminated_signal_buffers_the_mirror_and_drops_the_receiver() {
        let mut store = DocumentStore::default();
        let c = ctx();
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![block(c, 1, "before")], 3).unwrap();
        let (tx, rx) = mpsc::channel(8);
        store.install(c, mirror, Some(rx), || "ctx".into());

        tx.try_send(FeedEvent::Terminated {
            reason: crate::subscriptions::SubscriptionEndReason::SlowSubscriber,
            delivered_version: 3,
        })
        .unwrap();
        let signals = store.drain_feeds();
        assert_eq!(signals, vec![(c, FeedSignal::Terminated { epoch: 2 })]);

        let entry = store.get(c).unwrap();
        assert!(!entry.mirror.is_hydrated());
        assert!(entry.feed.is_none(), "Terminated is not resumable — the dead receiver must be dropped");
    }

    #[test]
    fn obsolete_snapshot_from_earlier_epoch_is_rejected() {
        let mut store = DocumentStore::default();
        let c = ctx();
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![block(c, 1, "v1")], 1).unwrap();
        let (tx, rx) = mpsc::channel(8);
        store.install(c, mirror, Some(rx), || "ctx".into());

        // First reconnect -> epoch 2
        tx.try_send(FeedEvent::Resubscribed).unwrap();
        let s1 = store.drain_feeds();
        assert_eq!(s1, vec![(c, FeedSignal::Resubscribed { epoch: 2 })]);

        // Second reconnect before first snapshot lands -> epoch 3
        tx.try_send(FeedEvent::Resubscribed).unwrap();
        let s2 = store.drain_feeds();
        assert_eq!(s2, vec![(c, FeedSignal::Resubscribed { epoch: 3 })]);

        // Epoch 3 snapshot lands first
        assert!(store
            .apply_snapshot_for_epoch(c, 3, vec![block(c, 1, "v20")], 20)
            .unwrap());
        assert_eq!(store.get(c).unwrap().mirror.version(), 20);

        // Stale epoch 2 snapshot arrives late -> rejected, does not clobber v20
        assert!(!store
            .apply_snapshot_for_epoch(c, 2, vec![block(c, 1, "v12")], 12)
            .unwrap());
        assert_eq!(store.get(c).unwrap().mirror.version(), 20);
        assert_eq!(store.get(c).unwrap().mirror.blocks()[0].content, "v20");
    }

    #[test]
    fn obsolete_terminated_install_from_earlier_epoch_is_rejected() {
        let mut store = DocumentStore::default();
        let c = ctx();
        let (tx, rx) = mpsc::channel(8);
        store.install(c, ContextMirror::new(c), Some(rx), || "ctx".into());

        tx.try_send(FeedEvent::Terminated {
            reason: crate::subscriptions::SubscriptionEndReason::SlowSubscriber,
            delivered_version: 1,
        })
        .unwrap();
        let signals = store.drain_feeds();
        assert_eq!(signals, vec![(c, FeedSignal::Terminated { epoch: 2 })]);

        // A newer recovery (epoch 2) lands first and advances the epoch to 3.
        let mut newer = ContextMirror::new(c);
        newer.apply_snapshot(vec![block(c, 1, "fresh")], 10).unwrap();
        assert!(store.install_for_epoch(c, Some(2), newer, None, || "ctx".into()));
        assert_eq!(store.recovery_epoch(c), Some(3));

        // A duplicate or stale recovery for epoch 2 arrives late -> rejected.
        let mut stale = ContextMirror::new(c);
        stale.apply_snapshot(vec![block(c, 1, "stale")], 4).unwrap();
        assert!(!store.install_for_epoch(c, Some(2), stale, None, || "ctx".into()));
        assert_eq!(store.get(c).unwrap().mirror.version(), 10);
    }
}

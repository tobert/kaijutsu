//! Multi-context document store.
//!
//! Holds a [`ContextMirror`] (the change-feed applier, `context_feed.rs`) and
//! a [`SyncedInput`] compose-input CRDT per joined context, enabling instant
//! context switching and LRU eviction. This is the client-owned home for
//! *all* conversation document state — the app is a renderer over it, not
//! the owner of the recovery mechanics.
//!
//! Bevy-free on purpose: the app wraps this in a `Resource` newtype, but the
//! store itself is plain Rust so its logic is unit-testable without a world.
//!
//! **Migration note (docs/change-feed.md).** The block document used to be a
//! diamond-types CRDT (`SyncedDocument`), fed by `ServerEvent::BlockTextOps`
//! and friends. It is now a plain projection (`ContextMirror`) fed by a
//! per-context change feed (`ActorHandle::subscribe_context` +
//! `get_blocks_versioned`). The store's job shrank to match: it no longer
//! decides *when* a document is stale (the feed says so, per context, via
//! `FeedEvent::Resubscribed`/`Terminated`) — it only holds the mirror, holds
//! the live feed receiver alongside it, and drains that receiver on request.
//! The old `generation`/`stale_active`/`mark_synced` staleness dance is gone
//! with it; there is nothing left for it to gate. The compose-input CRDT
//! (`SyncedInput`) is untouched — it stays a raw DTE surface by design (see
//! docs/change-feed.md open question 2).

use std::collections::HashMap;
use std::time::Instant;

use kaijutsu_types::{BlockSnapshot, ContextId};
use tokio::sync::mpsc;

use crate::{ContextMirror, FeedEvent, MirrorError, SyncedInput};

/// A cached document for a single context: its change-feed mirror, the live
/// feed receiver that keeps it current, compose input, and the bookkeeping
/// the store needs to pick eviction victims.
#[allow(dead_code)]
pub struct DocumentEntry {
    /// The context's projected block state, maintained from the change feed
    /// (docs/change-feed.md). No CRDT lives here.
    pub mirror: ContextMirror,
    /// The context's live change feed. `None` between a `Terminated` signal
    /// and the app re-subscribing — draining is a no-op while it's `None`,
    /// never a panic.
    pub feed: Option<mpsc::Receiver<FeedEvent>>,
    /// CRDT-backed input document (compose scratchpad).
    /// `None` until the input state is fetched from the server.
    pub input: Option<SyncedInput>,
    /// Context name (e.g. the kernel_id or user-supplied name).
    pub context_name: String,
    /// When this document was last accessed (for LRU eviction).
    pub last_accessed: Instant,
    /// Set after submit/escape×3 to suppress late-arriving TextOps and
    /// SyncedInput restoration. Cleared when `InputCleared` arrives from
    /// the server and triggers a clean re-fetch.
    pub input_pending_clear: bool,
}

impl DocumentEntry {
    /// A freshly-installed entry around `mirror`, optionally already wired to
    /// a live `feed`. Input is unset until fetched.
    pub fn new(
        mirror: ContextMirror,
        feed: Option<mpsc::Receiver<FeedEvent>>,
        context_name: String,
    ) -> Self {
        Self {
            mirror,
            feed,
            input: None,
            context_name,
            last_accessed: Instant::now(),
            input_pending_clear: false,
        }
    }

    /// Drain every event currently queued on this entry's feed, applying
    /// `Changed` deliveries to the mirror in order (rule 16) and reporting
    /// what happened. Non-blocking — returns as soon as the channel has
    /// nothing more to give right now, or immediately if there is no feed to
    /// drain.
    ///
    /// A delivery the mirror refuses (`MirrorError`) is never silently
    /// dropped: the mirror is rebuilt fresh (same recovery as
    /// `Resubscribed`) and the fault rides back as [`FeedSignal::Desynced`]
    /// so the caller can log it loudly. This should not happen in normal
    /// operation — `ContextMirror::receive` already discards a stale
    /// straggler rather than erroring on it — so seeing one is itself
    /// diagnostic.
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
                        self.mirror = ContextMirror::new(self.mirror.context_id());
                        signals.push(FeedSignal::Desynced(e));
                    }
                },
                Ok(FeedEvent::Resubscribed) => {
                    // Same receiver, new epoch: nothing published during the
                    // outage rode it, so the mirror held before this is
                    // stale and is replaced wholesale, never patched.
                    self.mirror = ContextMirror::new(self.mirror.context_id());
                    signals.push(FeedSignal::Resubscribed);
                }
                Ok(FeedEvent::Terminated { .. }) => {
                    // Not resumable (rule 28): the receiver is spent, so
                    // there is nothing left to drain this call.
                    self.mirror = ContextMirror::new(self.mirror.context_id());
                    self.feed = None;
                    signals.push(FeedSignal::Terminated);
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // The sender side is gone without an explicit
                    // `Terminated` (e.g. the actor itself tore down) — same
                    // recovery as if it had said so.
                    self.mirror = ContextMirror::new(self.mirror.context_id());
                    self.feed = None;
                    signals.push(FeedSignal::Terminated);
                    break;
                }
            }
        }
        signals
    }
}

/// What draining one context's feed produced. `Updated` needs no reaction
/// beyond a redraw; the other three all mean "the mirror was just reset —
/// fetch a fresh snapshot," and the caller (which owns the actor handle,
/// i.e. I/O the store cannot perform itself) decides how:
///
/// - [`Self::Resubscribed`] / [`Self::Desynced`]: the existing feed receiver
///   is still good — just re-fetch with `get_blocks_versioned` and hydrate
///   via [`DocumentStore::apply_snapshot`].
/// - [`Self::Terminated`]: the receiver is spent — call
///   `subscribe_context` again for a new one, then fetch and hydrate via
///   [`DocumentStore::install`].
#[derive(Debug, PartialEq, Eq)]
pub enum FeedSignal {
    /// A delivery applied cleanly (or was buffered pre-hydration).
    Updated,
    /// The actor already re-subscribed on the same receiver.
    Resubscribed,
    /// The feed ended and will not resume; `DocumentEntry::feed` is now
    /// `None`.
    Terminated,
    /// A delivery could not be applied to the mirror — never expected under
    /// normal operation (see `drain_feed`'s doc comment). Carried so the
    /// caller can surface the fault instead of masking it as an ordinary
    /// reconnect.
    Desynced(MirrorError),
}

/// Multi-context document store — the authoritative source for all document
/// state. The active entry is what the renderer draws.
#[allow(dead_code)]
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

#[allow(dead_code)]
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
        } else {
            self.insert(context_id, DocumentEntry::new(mirror, feed, name()));
        }
    }

    /// Re-hydrate an already-followed context's mirror from a fresh
    /// snapshot, keeping its existing feed receiver untouched — the
    /// `Resubscribed`/`Desynced` recovery path (docs/change-feed.md rules
    /// 21-26, replayed on the ALREADY-RESET mirror `drain_feed` left
    /// behind). A context that was left or evicted before the fetch
    /// completed is a no-op, not an error: the fetch just lost the race
    /// against the user leaving.
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
    use kaijutsu_crdt::{BlockKind, PrincipalId, Role};

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
        // receiver replace the dead ones on the SAME entry, so MRU/active/
        // input bookkeeping survives the recovery untouched.
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
    /// CRDT bytes, never a `ServerEvent::BlockTextOps` — and reaches the
    /// document's block state via `DocumentStore::drain_feeds` alone.
    #[test]
    fn draining_the_feed_applies_a_text_delivery_with_no_crdt_involved() {
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
    fn a_resubscribed_signal_resets_the_mirror_and_keeps_the_receiver() {
        let mut store = DocumentStore::default();
        let c = ctx();
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![block(c, 1, "before")], 3).unwrap();
        let (tx, rx) = mpsc::channel(8);
        store.install(c, mirror, Some(rx), || "ctx".into());

        tx.try_send(FeedEvent::Resubscribed).unwrap();
        let signals = store.drain_feeds();
        assert_eq!(signals, vec![(c, FeedSignal::Resubscribed)]);

        let entry = store.get(c).unwrap();
        assert!(!entry.mirror.is_hydrated(), "Resubscribed must throw the stale mirror away");
        assert!(entry.feed.is_some(), "Resubscribed reuses the SAME receiver, never re-subscribes");
    }

    #[test]
    fn a_terminated_signal_resets_the_mirror_and_drops_the_receiver() {
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
        assert_eq!(signals, vec![(c, FeedSignal::Terminated)]);

        let entry = store.get(c).unwrap();
        assert!(!entry.mirror.is_hydrated());
        assert!(entry.feed.is_none(), "Terminated is not resumable — the dead receiver must be dropped");
    }
}

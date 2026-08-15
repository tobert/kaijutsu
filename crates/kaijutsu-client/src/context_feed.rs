//! The context change feed, client side (docs/change-feed.md).
//!
//! Two things live here, and the second is the more important one.
//!
//! [`context_feed_channel`] builds the Cap'n Proto observer the kernel pushes
//! deliveries at, and hands back a receiver of typed [`FeedEvent`]s. That is
//! plumbing.
//!
//! [`ContextMirror`] is the part worth sharing. Every client that follows a
//! context has to obey the same rules — apply a delivery in order, append a
//! suffix, replace on a replace, never compare character counts, subscribe
//! before fetching, buffer until the snapshot lands, discard what the snapshot
//! already includes. Those rules are short, and each one exists because
//! ignoring it corrupted something. Writing them three times, in ACP and MCP
//! and the app, would be three chances to get one subtly wrong; the kernel
//! stays smart and the clients stay thin, so the applier is written and tested
//! once, here.

use std::collections::HashMap;
use std::rc::Rc;

use capnp::capability::Promise;
use kaijutsu_crdt::ContextId;
use kaijutsu_types::{BlockId, BlockMetadata, BlockSnapshot, OutputData, Status};
use tokio::sync::mpsc;

use crate::kaijutsu_capnp::{context_event, context_observer};
use crate::rpc::{parse_block_id, parse_block_metadata, parse_block_snapshot, parse_output_data};
use crate::subscriptions::SubscriptionEndReason;

// ============================================================================
// Typed deliveries
// ============================================================================

/// One accepted change to a context — a domain fact, already classified by the
/// kernel. Nothing here is an encoding a client has to interpret.
#[derive(Clone, Debug)]
pub enum ContextChange {
    /// A block was authored. `after_id` is the block it landed after; `None`
    /// means the beginning. The position is carried because a `BlockSnapshot`
    /// has no ordering key of its own.
    BlockInserted {
        block: Box<BlockSnapshot>,
        after_id: Option<BlockId>,
    },
    BlockDeleted {
        block_id: BlockId,
    },
    BlockMoved {
        block_id: BlockId,
        after_id: Option<BlockId>,
    },
    /// Text was added at the end. Append `suffix` to what you hold.
    TextAppended {
        block_id: BlockId,
        suffix: String,
    },
    /// Text changed in some other way. Replace what you hold with `content`.
    TextReplaced {
        block_id: BlockId,
        content: String,
    },
    StatusChanged {
        block_id: BlockId,
        status: Status,
    },
    CollapsedChanged {
        block_id: BlockId,
        collapsed: bool,
    },
    ExcludedChanged {
        block_id: BlockId,
        excluded: bool,
    },
    MetadataChanged {
        block_id: BlockId,
        metadata: Box<BlockMetadata>,
    },
    OutputChanged {
        block_id: BlockId,
        output: Option<Box<OutputData>>,
    },
}

/// One delivery: an ordered batch, and the version it brings the client to.
#[derive(Clone, Debug)]
pub struct ContextDelivery {
    pub context_id: ContextId,
    pub events: Vec<ContextChange>,
    /// Applying `events` in order makes the client's state exactly the context
    /// at this version.
    pub version: u64,
}

/// What arrives on a feed.
#[derive(Clone, Debug)]
pub enum FeedEvent {
    Changed(ContextDelivery),
    /// The connection dropped and the actor has already re-subscribed. Nothing
    /// the kernel published during the outage is on this feed, so the mirror
    /// held before it is stale: fetch a fresh snapshot and hydrate a new
    /// mirror. Continuing to apply deltas to the old one would silently skip
    /// the gap.
    Resubscribed,
    /// The feed ended and is **not** resumable: re-subscribe and refetch.
    /// `delivered_version` is the last version actually applied, useful for
    /// logging how far behind the client had fallen — never for patching
    /// forward.
    Terminated {
        reason: SubscriptionEndReason,
        delivered_version: u64,
    },
}

// ============================================================================
// The observer callback
// ============================================================================

/// Implements the Cap'n Proto `ContextObserver::Server` trait, forwarding each
/// delivery into an mpsc channel.
///
/// An **mpsc** channel, deliberately, where the older event forwarders use a
/// broadcast: a broadcast drops the oldest message when a receiver lags, and
/// this feed's entire contract is that a client never silently misses a change.
/// A full channel here holds the RPC promise open, which is backpressure the
/// kernel already knows how to handle — it has its own delivery timeout and
/// terminates a subscriber that cannot keep up, loudly.
struct ContextObserverForwarder {
    event_tx: mpsc::Sender<FeedEvent>,
}

#[allow(refining_impl_trait)]
impl context_observer::Server for ContextObserverForwarder {
    fn on_context_changed(
        self: Rc<Self>,
        params: context_observer::OnContextChangedParams,
        _results: context_observer::OnContextChangedResults,
    ) -> Promise<(), capnp::Error> {
        let delivery = match parse_delivery(params) {
            Ok(d) => d,
            Err(e) => return Promise::err(e),
        };
        let tx = self.event_tx.clone();
        Promise::from_future(async move {
            tx.send(FeedEvent::Changed(delivery))
                .await
                .map_err(|_| capnp::Error::failed("context feed receiver dropped".into()))
        })
    }

    fn on_terminated(
        self: Rc<Self>,
        params: context_observer::OnTerminatedParams,
        _results: context_observer::OnTerminatedResults,
    ) -> Promise<(), capnp::Error> {
        let p = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let reason = match p.get_reason() {
            Ok(crate::kaijutsu_capnp::SubscriptionEndReason::SlowSubscriber) => {
                SubscriptionEndReason::SlowSubscriber
            }
            Ok(crate::kaijutsu_capnp::SubscriptionEndReason::ServerShutdown) => {
                SubscriptionEndReason::ServerShutdown
            }
            Ok(crate::kaijutsu_capnp::SubscriptionEndReason::Superseded) => {
                SubscriptionEndReason::Superseded
            }
            // An enumerant this build does not know. Honest beats guessed.
            Err(_) => SubscriptionEndReason::Unknown,
        };
        let delivered_version = p.get_delivered_version();
        let tx = self.event_tx.clone();
        Promise::from_future(async move {
            // A dropped receiver here is not an error worth failing the call
            // for: the feed is over either way.
            let _ = tx
                .send(FeedEvent::Terminated {
                    reason,
                    delivered_version,
                })
                .await;
            Ok(())
        })
    }
}

/// Build a `ContextObserver` callback client plus the receiver its deliveries
/// land on. Pass the client to `subscribe_context`; drain the receiver.
pub fn context_feed_channel(
    capacity: usize,
) -> (context_observer::Client, mpsc::Receiver<FeedEvent>) {
    let (tx, rx) = mpsc::channel(capacity);
    let client: context_observer::Client =
        capnp_rpc::new_client(ContextObserverForwarder { event_tx: tx });
    (client, rx)
}

fn parse_delivery(
    params: context_observer::OnContextChangedParams,
) -> Result<ContextDelivery, capnp::Error> {
    let p = params.get()?;
    let context_id = ContextId::try_from_slice(p.get_context_id()?).ok_or_else(|| {
        capnp::Error::failed("invalid context ID on a context feed delivery".into())
    })?;
    let version = p.get_version();
    let list = p.get_events()?;
    let mut events = Vec::with_capacity(list.len() as usize);
    for reader in list.iter() {
        events.push(parse_change(reader)?);
    }
    Ok(ContextDelivery {
        context_id,
        events,
        version,
    })
}

fn parse_change(reader: context_event::Reader<'_>) -> Result<ContextChange, capnp::Error> {
    let to_capnp = |e: crate::rpc::RpcError| capnp::Error::failed(e.to_string());
    Ok(match reader.which()? {
        context_event::BlockInserted(r) => {
            let r = r?;
            let block = parse_block_snapshot(&r.get_block()?).map_err(to_capnp)?;
            let after_id = if r.get_has_after_id() {
                Some(parse_block_id(&r.get_after_id()?).map_err(to_capnp)?)
            } else {
                None
            };
            ContextChange::BlockInserted {
                block: Box::new(block),
                after_id,
            }
        }
        context_event::BlockDeleted(r) => ContextChange::BlockDeleted {
            block_id: parse_block_id(&r?).map_err(to_capnp)?,
        },
        context_event::BlockMoved(r) => {
            let r = r?;
            ContextChange::BlockMoved {
                block_id: parse_block_id(&r.get_block_id()?).map_err(to_capnp)?,
                after_id: if r.get_has_after_id() {
                    Some(parse_block_id(&r.get_after_id()?).map_err(to_capnp)?)
                } else {
                    None
                },
            }
        }
        context_event::TextAppended(r) => {
            let r = r?;
            ContextChange::TextAppended {
                block_id: parse_block_id(&r.get_block_id()?).map_err(to_capnp)?,
                suffix: r.get_suffix()?.to_str()?.to_owned(),
            }
        }
        context_event::TextReplaced(r) => {
            let r = r?;
            ContextChange::TextReplaced {
                block_id: parse_block_id(&r.get_block_id()?).map_err(to_capnp)?,
                content: r.get_content()?.to_str()?.to_owned(),
            }
        }
        context_event::StatusChanged(r) => {
            let r = r?;
            ContextChange::StatusChanged {
                block_id: parse_block_id(&r.get_block_id()?).map_err(to_capnp)?,
                status: match r.get_status()? {
                    crate::kaijutsu_capnp::Status::Pending => Status::Pending,
                    crate::kaijutsu_capnp::Status::Running => Status::Running,
                    crate::kaijutsu_capnp::Status::Done => Status::Done,
                    crate::kaijutsu_capnp::Status::Error => Status::Error,
                },
            }
        }
        context_event::CollapsedChanged(r) => {
            let r = r?;
            ContextChange::CollapsedChanged {
                block_id: parse_block_id(&r.get_block_id()?).map_err(to_capnp)?,
                collapsed: r.get_value(),
            }
        }
        context_event::ExcludedChanged(r) => {
            let r = r?;
            ContextChange::ExcludedChanged {
                block_id: parse_block_id(&r.get_block_id()?).map_err(to_capnp)?,
                excluded: r.get_value(),
            }
        }
        context_event::MetadataChanged(r) => {
            let r = r?;
            ContextChange::MetadataChanged {
                block_id: parse_block_id(&r.get_block_id()?).map_err(to_capnp)?,
                metadata: Box::new(parse_block_metadata(r.get_metadata()?)),
            }
        }
        context_event::OutputChanged(r) => {
            let r = r?;
            ContextChange::OutputChanged {
                block_id: parse_block_id(&r.get_block_id()?).map_err(to_capnp)?,
                output: Some(Box::new(parse_output_data(r.get_output()?)?)),
            }
        }
    })
}

// ============================================================================
// The mirror — one tested implementation of the client rules
// ============================================================================

/// Why a delivery could not be applied.
#[derive(Debug, PartialEq, Eq)]
pub enum MirrorError {
    /// A delivery arrived for a context this mirror does not follow. Applying
    /// it would mix two version counters into one, so it is refused rather
    /// than ignored.
    WrongContext {
        expected: ContextId,
        got: ContextId,
    },
    /// A change named a block the mirror does not hold. The mirror is stale or
    /// a delivery was lost — either way the honest repair is a fresh snapshot,
    /// not a guess.
    UnknownBlock(BlockId),
    /// A delivery arrived at or below the version already applied. The caller
    /// should re-subscribe and refetch: version going backwards means this
    /// mirror is not following one ordered feed.
    VersionWentBackwards { have: u64, got: u64 },
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongContext { expected, got } => {
                write!(f, "delivery for {got} on a mirror following {expected}")
            }
            Self::UnknownBlock(id) => write!(f, "change for a block the mirror has never seen: {id}"),
            Self::VersionWentBackwards { have, got } => {
                write!(f, "delivery at version {got} after {have} was applied")
            }
        }
    }
}

impl std::error::Error for MirrorError {}

/// A client's view of one context, maintained from the change feed.
///
/// # The recovery dance, and why its order is not negotiable
///
/// ```text
/// subscribe  ->  buffer whatever arrives
///            ->  getBlocks (blocks AND the version they were read at)
///            ->  apply_snapshot(blocks, version)
///            ->  apply() each later delivery
/// ```
///
/// Fetching before subscribing loses any change that lands in the gap.
/// Applying a delivery before the snapshot corrupts the text it describes. The
/// version resolves the overlap: anything at or below the snapshot's version is
/// already in the snapshot and is dropped, and applying it twice — an append
/// especially — is corruption, not a no-op.
#[derive(Debug)]
pub struct ContextMirror {
    context_id: ContextId,
    /// Blocks in document order.
    blocks: Vec<BlockSnapshot>,
    /// `block_id` -> index into `blocks`, rebuilt on any positional change.
    index: HashMap<BlockId, usize>,
    version: u64,
    /// The version the snapshot was read at. Everything at or below it is
    /// provably already in `blocks`, so a delivery that old is a duplicate to
    /// discard rather than a fault to report — see `receive`.
    snapshot_version: u64,
    /// Deliveries received before the snapshot arrived.
    buffered: Vec<ContextDelivery>,
    snapshot_applied: bool,
}

impl ContextMirror {
    /// A mirror with no snapshot yet. Deliveries are buffered until
    /// [`Self::apply_snapshot`] is called.
    pub fn new(context_id: ContextId) -> Self {
        Self {
            context_id,
            blocks: Vec::new(),
            index: HashMap::new(),
            version: 0,
            snapshot_version: 0,
            buffered: Vec::new(),
            snapshot_applied: false,
        }
    }

    pub fn context_id(&self) -> ContextId {
        self.context_id
    }

    /// The version this mirror's state corresponds to.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Blocks in document order.
    pub fn blocks(&self) -> &[BlockSnapshot] {
        &self.blocks
    }

    pub fn block(&self, id: &BlockId) -> Option<&BlockSnapshot> {
        self.index.get(id).map(|i| &self.blocks[*i])
    }

    /// Has a snapshot been applied? Until it has, everything is buffered.
    pub fn is_hydrated(&self) -> bool {
        self.snapshot_applied
    }

    /// Take a delivery. Before the snapshot it is buffered; after, applied.
    ///
    /// This is the only entry point a client needs, and it is safe to call in
    /// any order relative to [`Self::apply_snapshot`]. A straggler that was
    /// already in the snapshot is **discarded**, not reported: the snapshot was
    /// read at a version, so everything at or below it is provably included,
    /// and a client should not have to race its fetch against the channel to
    /// avoid a spurious error.
    ///
    /// (It used to be an error, and the first client to migrate had to write a
    /// `select!` racing `getBlocks` against the receiver to dodge it. That is
    /// the kind of thing this type exists to hold once rather than three
    /// times.)
    pub fn receive(&mut self, delivery: ContextDelivery) -> Result<(), MirrorError> {
        if delivery.context_id != self.context_id {
            return Err(MirrorError::WrongContext {
                expected: self.context_id,
                got: delivery.context_id,
            });
        }
        if !self.snapshot_applied {
            self.buffered.push(delivery);
            return Ok(());
        }
        if delivery.version <= self.snapshot_version {
            // Already in the snapshot. Applying it would double an append.
            return Ok(());
        }
        self.apply(delivery)
    }

    /// Install a snapshot fetched with `getBlocks`, then drain whatever was
    /// buffered while it was in flight.
    ///
    /// Buffered deliveries at or below `version` are dropped — the snapshot
    /// already contains them.
    pub fn apply_snapshot(
        &mut self,
        blocks: Vec<BlockSnapshot>,
        version: u64,
    ) -> Result<(), MirrorError> {
        self.blocks = blocks;
        self.reindex();
        self.version = version;
        self.snapshot_version = version;
        self.snapshot_applied = true;

        let mut buffered = std::mem::take(&mut self.buffered);
        buffered.sort_by_key(|d| d.version);
        for delivery in buffered {
            if delivery.version <= version {
                continue;
            }
            self.apply(delivery)?;
        }
        Ok(())
    }

    /// Apply one delivery to a hydrated mirror, in order.
    ///
    /// A delivery that does not advance the version and is NOT covered by the
    /// snapshot is still an error, and deliberately so: it means a delivery
    /// this mirror never applied arrived after a later one: an inversion. Both
    /// applying it (out of order) and dropping it (losing a change) corrupt,
    /// so the honest move is to refuse and let the caller refetch.
    fn apply(&mut self, delivery: ContextDelivery) -> Result<(), MirrorError> {
        if delivery.version <= self.version {
            return Err(MirrorError::VersionWentBackwards {
                have: self.version,
                got: delivery.version,
            });
        }
        for change in delivery.events {
            self.apply_change(change)?;
        }
        self.version = delivery.version;
        Ok(())
    }

    fn apply_change(&mut self, change: ContextChange) -> Result<(), MirrorError> {
        match change {
            ContextChange::BlockInserted { block, after_id } => {
                let at = self.position_after(after_id.as_ref())?;
                self.blocks.insert(at, *block);
                self.reindex();
            }
            ContextChange::BlockDeleted { block_id } => {
                let i = self.require(&block_id)?;
                self.blocks.remove(i);
                self.reindex();
            }
            ContextChange::BlockMoved { block_id, after_id } => {
                let from = self.require(&block_id)?;
                let block = self.blocks.remove(from);
                // Reindex BEFORE resolving the anchor: every block after the
                // one just removed shifted down, and an anchor resolved against
                // the stale index lands the block one slot too far right — off
                // the end of the vector when the anchor was last.
                self.reindex();
                let at = self.position_after(after_id.as_ref())?;
                self.blocks.insert(at, block);
                self.reindex();
            }
            ContextChange::TextAppended { block_id, suffix } => {
                let i = self.require(&block_id)?;
                self.blocks[i].content.push_str(&suffix);
            }
            ContextChange::TextReplaced { block_id, content } => {
                let i = self.require(&block_id)?;
                self.blocks[i].content = content;
            }
            ContextChange::StatusChanged { block_id, status } => {
                let i = self.require(&block_id)?;
                self.blocks[i].status = status;
            }
            ContextChange::CollapsedChanged {
                block_id,
                collapsed,
            } => {
                let i = self.require(&block_id)?;
                self.blocks[i].collapsed = collapsed;
            }
            ContextChange::ExcludedChanged { block_id, excluded } => {
                let i = self.require(&block_id)?;
                self.blocks[i].excluded = excluded;
            }
            ContextChange::MetadataChanged { block_id, metadata } => {
                let i = self.require(&block_id)?;
                self.blocks[i].apply_metadata(&metadata);
            }
            ContextChange::OutputChanged { block_id, output } => {
                let i = self.require(&block_id)?;
                self.blocks[i].output = output.map(|o| *o);
            }
        }
        Ok(())
    }

    fn require(&self, id: &BlockId) -> Result<usize, MirrorError> {
        self.index
            .get(id)
            .copied()
            .ok_or(MirrorError::UnknownBlock(*id))
    }

    /// Where a block anchored after `after_id` belongs. `None` is the
    /// beginning; an anchor the mirror does not hold is an error, not a
    /// silent append to the end — landing a block in the wrong place is the
    /// kind of quiet wrongness this migration exists to remove.
    fn position_after(&self, after_id: Option<&BlockId>) -> Result<usize, MirrorError> {
        match after_id {
            None => Ok(0),
            Some(id) => self.require(id).map(|i| i + 1),
        }
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (i, block) in self.blocks.iter().enumerate() {
            self.index.insert(block.id, i);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::{BlockKind, PrincipalId, Role};

    fn ctx() -> ContextId {
        ContextId::new()
    }

    fn block(context_id: ContextId, seq: u64, content: &str) -> BlockSnapshot {
        let id = BlockId::new(context_id, PrincipalId::new(), seq);
        let mut snap = BlockSnapshot::text(id, None, Role::User, content);
        snap.kind = BlockKind::Text;
        snap
    }

    fn delivery(
        context_id: ContextId,
        version: u64,
        events: Vec<ContextChange>,
    ) -> ContextDelivery {
        ContextDelivery {
            context_id,
            events,
            version,
        }
    }

    /// The whole point of the feed: a client that only appends suffixes and
    /// swaps in replacements ends up with the kernel's text.
    #[test]
    fn applying_a_delivery_reproduces_the_text() {
        let c = ctx();
        let b = block(c, 1, "");
        let id = b.id;
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![b], 10).unwrap();

        mirror
            .receive(delivery(
                c,
                11,
                vec![
                    ContextChange::TextAppended {
                        block_id: id,
                        suffix: "こんにちは".into(),
                    },
                    ContextChange::TextAppended {
                        block_id: id,
                        suffix: "、世界".into(),
                    },
                ],
            ))
            .unwrap();
        mirror
            .receive(delivery(
                c,
                12,
                vec![ContextChange::TextReplaced {
                    block_id: id,
                    content: "はい、世界".into(),
                }],
            ))
            .unwrap();

        assert_eq!(mirror.block(&id).unwrap().content, "はい、世界");
        assert_eq!(mirror.version(), 12);
    }

    /// Rule 21-26. The client subscribes first, so deliveries can arrive while
    /// the snapshot is in flight. Everything at or below the snapshot's version
    /// is already IN the snapshot — replaying an append there would double it.
    #[test]
    fn buffered_deliveries_at_or_below_the_snapshot_are_discarded() {
        let c = ctx();
        let b = block(c, 1, "abc");
        let id = b.id;
        let mut mirror = ContextMirror::new(c);

        // Arrived while getBlocks was in flight, in the wrong order, and one of
        // them is already included in the snapshot we are about to apply.
        mirror
            .receive(delivery(
                c,
                6,
                vec![ContextChange::TextAppended {
                    block_id: id,
                    suffix: "e".into(),
                }],
            ))
            .unwrap();
        mirror
            .receive(delivery(
                c,
                5,
                vec![ContextChange::TextAppended {
                    block_id: id,
                    suffix: "d".into(),
                }],
            ))
            .unwrap();
        assert!(!mirror.is_hydrated());

        // The snapshot already contains everything through version 5.
        mirror.apply_snapshot(vec![b], 5).unwrap();

        assert_eq!(
            mirror.block(&id).unwrap().content,
            "abce",
            "the version-5 append is already in the snapshot and must not be replayed"
        );
        assert_eq!(mirror.version(), 6);
    }

    /// The straggler case: a delivery that raced the snapshot fetch and lost.
    ///
    /// A client is allowed to fetch first and drain after — it should not have
    /// to race `getBlocks` against its receiver to avoid a spurious error. The
    /// snapshot was read AT a version, so anything at or below it is provably
    /// already applied, and the only correct thing to do is drop it.
    #[test]
    fn a_delivery_the_snapshot_already_includes_is_discarded_not_refused() {
        let c = ctx();
        let b = block(c, 1, "abc");
        let id = b.id;
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![b], 5).unwrap();

        // Arrived late, on the wire, after the snapshot was installed.
        mirror
            .receive(delivery(
                c,
                5,
                vec![ContextChange::TextAppended {
                    block_id: id,
                    suffix: "c".into(),
                }],
            ))
            .expect("a straggler the snapshot covers is not an error");
        mirror
            .receive(delivery(
                c,
                3,
                vec![ContextChange::TextAppended {
                    block_id: id,
                    suffix: "b".into(),
                }],
            ))
            .expect("an older straggler likewise");

        assert_eq!(
            mirror.block(&id).unwrap().content,
            "abc",
            "neither straggler may be applied — that would double the text"
        );
        assert_eq!(mirror.version(), 5);
    }

    /// The distinction that makes the discard above safe: once the mirror has
    /// advanced PAST the snapshot under its own steam, a delivery that fails to
    /// advance is an inversion — a change that arrived after a later one and
    /// was never applied. Applying it is out of order and dropping it loses a
    /// change, so it must be refused.
    #[test]
    fn an_inversion_above_the_snapshot_is_still_refused() {
        let c = ctx();
        let b = block(c, 1, "abc");
        let id = b.id;
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![b], 5).unwrap();

        mirror
            .receive(delivery(
                c,
                7,
                vec![ContextChange::TextAppended {
                    block_id: id,
                    suffix: "d".into(),
                }],
            ))
            .unwrap();

        // Version 6 never arrived before 7 did. It is not in the snapshot
        // (which stopped at 5) and it was not applied, so this mirror is
        // missing a change no matter what it does with this delivery.
        let err = mirror
            .receive(delivery(
                c,
                6,
                vec![ContextChange::TextAppended {
                    block_id: id,
                    suffix: "MISSING".into(),
                }],
            ))
            .unwrap_err();
        assert_eq!(
            err,
            MirrorError::VersionWentBackwards { have: 7, got: 6 }
        );
        assert_eq!(mirror.block(&id).unwrap().content, "abcd");
    }

    /// Document order comes from the feed's positions, not from any ordering
    /// key on the snapshot — there isn't one on the wire.
    #[test]
    fn inserts_moves_and_deletes_maintain_document_order() {
        let c = ctx();
        let first = block(c, 1, "first");
        let first_id = first.id;
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![first], 1).unwrap();

        let second = block(c, 2, "second");
        let second_id = second.id;
        let third = block(c, 3, "third");
        let third_id = third.id;

        // Append after first, then insert at the very beginning.
        mirror
            .receive(delivery(
                c,
                2,
                vec![ContextChange::BlockInserted {
                    block: Box::new(second),
                    after_id: Some(first_id),
                }],
            ))
            .unwrap();
        mirror
            .receive(delivery(
                c,
                3,
                vec![ContextChange::BlockInserted {
                    block: Box::new(third),
                    after_id: None,
                }],
            ))
            .unwrap();
        assert_eq!(
            mirror.blocks().iter().map(|b| b.id).collect::<Vec<_>>(),
            vec![third_id, first_id, second_id]
        );

        // Move the head to the end, then delete the middle.
        mirror
            .receive(delivery(
                c,
                4,
                vec![ContextChange::BlockMoved {
                    block_id: third_id,
                    after_id: Some(second_id),
                }],
            ))
            .unwrap();
        mirror
            .receive(delivery(
                c,
                5,
                vec![ContextChange::BlockDeleted {
                    block_id: second_id,
                }],
            ))
            .unwrap();
        assert_eq!(
            mirror.blocks().iter().map(|b| b.id).collect::<Vec<_>>(),
            vec![first_id, third_id]
        );
    }

    /// An anchor the mirror does not hold means it is stale or lost a
    /// delivery. Landing the block somewhere plausible instead would be the
    /// quiet wrongness this whole migration is about.
    #[test]
    fn an_unknown_anchor_is_an_error_not_a_guess() {
        let c = ctx();
        let b = block(c, 1, "only");
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![b], 1).unwrap();

        let stranger = block(c, 99, "stranger");
        let stranger_id = stranger.id;
        let orphan = block(c, 100, "orphan");
        let err = mirror
            .receive(delivery(
                c,
                2,
                vec![ContextChange::BlockInserted {
                    block: Box::new(orphan),
                    after_id: Some(stranger_id),
                }],
            ))
            .unwrap_err();
        assert_eq!(err, MirrorError::UnknownBlock(stranger_id));
    }

    /// Two contexts have two version counters. Mixing them would make either
    /// one meaningless, so a stray delivery is refused rather than dropped.
    #[test]
    fn a_delivery_for_another_context_is_refused() {
        let mine = ctx();
        let theirs = ctx();
        let mut mirror = ContextMirror::new(mine);
        mirror.apply_snapshot(vec![], 1).unwrap();

        let err = mirror.receive(delivery(theirs, 2, vec![])).unwrap_err();
        assert_eq!(
            err,
            MirrorError::WrongContext {
                expected: mine,
                got: theirs
            }
        );
    }

    /// Rule 19's trap, as a test: a replace can keep the character count. A
    /// mirror that compared lengths would report no change here.
    #[test]
    fn a_replace_that_keeps_the_length_still_changes_the_text() {
        let c = ctx();
        let b = block(c, 1, "abcdef");
        let id = b.id;
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![b], 1).unwrap();

        mirror
            .receive(delivery(
                c,
                2,
                vec![ContextChange::TextReplaced {
                    block_id: id,
                    content: "ABCdef".into(),
                }],
            ))
            .unwrap();
        assert_eq!(mirror.block(&id).unwrap().content, "ABCdef");
    }

    /// Status and metadata are ordinary facts on the same feed, so a tool's
    /// final output text and its completion arrive in ONE delivery — the race
    /// the old per-event surface allowed.
    #[test]
    fn one_delivery_carries_final_text_and_completion_together() {
        let c = ctx();
        let b = block(c, 1, "");
        let id = b.id;
        let mut mirror = ContextMirror::new(c);
        mirror.apply_snapshot(vec![b], 1).unwrap();

        mirror
            .receive(delivery(
                c,
                2,
                vec![
                    ContextChange::TextAppended {
                        block_id: id,
                        suffix: "done.\n".into(),
                    },
                    ContextChange::StatusChanged {
                        block_id: id,
                        status: Status::Done,
                    },
                ],
            ))
            .unwrap();

        let seen = mirror.block(&id).unwrap();
        assert_eq!(seen.content, "done.\n");
        assert_eq!(seen.status, Status::Done);
    }
}

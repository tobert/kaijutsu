//! Materialize a committed cell into a CRDT block.
//!
//! This answers the data-model reconciliation in code (no longer deferred):
//!
//! - **Content type** — the [`ContentRef`](crate::ContentRef)'s open-string MIME
//!   projects to the closed [`ContentType`] render hint via
//!   [`ContentType::from_mime`], unknown → `Plain`. One typing system, the closed
//!   enum a *projection* of the open string — never two competing systems.
//! - **Byte homes** — text inlines in `content`; binary content puts its CAS hash
//!   in `content` as the reference (the exact convention `img_block` already uses:
//!   `role = Asset`, the 32-hex hash as the content string, the real MIME in the
//!   CAS sidecar). The cell's `ContentRef` hash is the durable anchor either way.
//! - **Position** — the committed cell's `Tick` rides on the block's new `tick`
//!   field, the kernel's semantic coordinate (distinct from `order_key`).
//!
//! Single-writer for now: the kernel is the sole sequencer, so no write-barrier
//! enforcement is needed yet (that lands with multi-writer timelines).

use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, ContentType, Role};

use crate::cell::{Body, Cell};
use crate::engine::Timeline;

impl Timeline {
    /// Materialize a committed (concrete) cell as a [`BlockSnapshot`] tagged with
    /// its tick position. Returns `None` for a cell that isn't concrete — there is
    /// nothing durable to persist until it crosses the write barrier.
    pub fn materialize(&self, cell: &Cell, id: BlockId) -> Option<BlockSnapshot> {
        let Body::Concrete(cref) = &cell.body else {
            return None;
        };

        let content_type = ContentType::from_mime(&cref.mime);
        // The substrate never switches on the MIME; this is the only place the
        // open string meets the closed render-hint enum.
        let is_text = cref.mime.starts_with("text/");

        // The cell's lane rides onto the block as `track` (lane identity, not the
        // author — the author is `id.principal_id`, who PLAYED). `track.is_some()`
        // becomes the "came off the timeline" discriminator downstream.
        let mut b = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .tick(cell.span.start)
            .track(cell.track.clone())
            .content_type(content_type);

        if is_text {
            // Text inlines in `content` (the conversation substrate's bread and butter).
            b = b.role(Role::Model);
            if let Some(bytes) = self.content_bytes(&cref.hash) {
                b = b.content(String::from_utf8_lossy(bytes).into_owned());
            }
        } else {
            // Binary/large: the CAS hash *is* the content pointer — `img_block`'s
            // convention. The bytes stay in CAS; `content` holds the 32-hex hash.
            b = b.role(Role::Asset).content(cref.hash.as_str().to_string());
        }

        Some(b.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{ContextQuery, Fallback, Recipe, ResolverId};
    use crate::content::ContextHash;
    use crate::resolver::{ResolveError, Resolution, Resolver, ResolverCtx};
    use crate::{Cell, Span, Tick, TickClock, TickDelta};
    use kaijutsu_types::{ContextId, PrincipalId};
    use std::time::Duration;

    /// A resolver that emits fixed bytes under a chosen MIME — lets a test drive a
    /// committed cell of any content type through the real speculative loop.
    struct Fixed {
        bytes: Vec<u8>,
        mime: String,
    }

    impl Resolver for Fixed {
        fn id(&self) -> ResolverId {
            ResolverId::new("fixed")
        }
        fn estimate_cost(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::from_secs(1)
        }
        fn compute_basis(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(
            &self,
            _p: &serde_json::Value,
            _c: &dyn ResolverCtx,
        ) -> Result<Resolution, ResolveError> {
            Ok(Resolution::new(self.bytes.clone(), self.mime.clone()))
        }
    }

    fn commit_one(mime: &str, bytes: &[u8]) -> (Timeline, Cell) {
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        tl.register_resolver(Box::new(Fixed {
            bytes: bytes.to_vec(),
            mime: mime.to_string(),
        }));
        let cell = Cell::deferred_on(
            Span::instant(Tick::new(10)),
            Recipe {
                resolver: ResolverId::new("fixed"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            kaijutsu_types::TrackId::solo(),
            PrincipalId::beat(),
        );
        tl.schedule(cell).unwrap();
        tl.advance_to(Tick::new(10));
        let committed = tl.committed()[0].clone();
        (tl, committed)
    }

    fn block_id() -> BlockId {
        BlockId::new(ContextId::new(), PrincipalId::new(), 1)
    }

    #[test]
    fn text_cell_inlines_content_and_projects_mime() {
        let (tl, cell) = commit_one("text/markdown", b"# hi");
        let block = tl.materialize(&cell, block_id()).unwrap();

        assert_eq!(block.content, "# hi");
        assert_eq!(block.content_type, ContentType::Markdown); // open mime → closed hint
        assert_eq!(block.role, Role::Model);
        assert_eq!(block.tick, Some(Tick::new(10))); // the timeline coordinate rides along
    }

    #[test]
    fn binary_cell_references_cas_by_hash() {
        let (tl, cell) = commit_one("audio/midi", b"MThd....");
        let block = tl.materialize(&cell, block_id()).unwrap();

        // No `Midi` render variant exists yet → unknown MIME projects to Plain,
        // gracefully, with zero substrate change. The bytes live in CAS.
        assert_eq!(block.content_type, ContentType::Plain);
        assert_eq!(block.role, Role::Asset);
        // content holds the 32-hex CAS hash (img_block convention), and it resolves.
        assert_eq!(block.content.len(), 32);
        let Body::Concrete(cref) = &cell.body else {
            unreachable!()
        };
        assert_eq!(block.content, cref.hash.as_str());
        assert_eq!(tl.content_bytes(&cref.hash), Some(b"MThd....".as_slice()));
        assert_eq!(block.tick, Some(Tick::new(10)));
    }

    /// T17 (design §8 Phase 5, materialize half) — a materialized snapshot carries
    /// its cell's lane (`track`) verbatim, for both a player-played cell and a
    /// `beat()`-played fallback. The block's author (id.principal_id) is the
    /// caller's concern (it passes the BlockId); materialize only stamps the lane.
    #[test]
    fn materialized_snapshot_carries_track() {
        use kaijutsu_types::TrackId;

        // Player case: a cell on the "bass" lane played by a real principal.
        let player = PrincipalId::new();
        let (tl, mut cell) = commit_one("audio/midi", b"MThd-player");
        cell.track = TrackId::new("bass").unwrap();
        cell.played_by = player;
        let id = BlockId::new(ContextId::new(), player, 7);
        let snap = tl.materialize(&cell, id).unwrap();
        assert_eq!(
            snap.track,
            Some(TrackId::new("bass").unwrap()),
            "the lane rides onto the snapshot"
        );
        // The author is whatever BlockId the caller minted — the lane is NOT the
        // author. (One track spans player + beat() principals.)
        assert_eq!(snap.id.principal_id, player);

        // Fallback case: same lane, but the transport played it (beat()).
        let (tl2, mut fb) = commit_one("audio/midi", b"MThd-vamp");
        fb.track = TrackId::new("bass").unwrap();
        fb.played_by = PrincipalId::beat();
        let id2 = BlockId::new(ContextId::new(), PrincipalId::beat(), 1);
        let snap2 = tl2.materialize(&fb, id2).unwrap();
        assert_eq!(snap2.track, Some(TrackId::new("bass").unwrap()));
        assert_eq!(snap2.id.principal_id, PrincipalId::beat());
    }

    #[test]
    fn deferred_cell_has_nothing_to_materialize() {
        let cell = Cell::deferred_on(
            Span::instant(Tick::new(1)),
            Recipe {
                resolver: ResolverId::new("fixed"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            kaijutsu_types::TrackId::solo(),
            PrincipalId::beat(),
        );
        let tl = Timeline::new(TickClock::default());
        assert!(tl.materialize(&cell, block_id()).is_none());
    }
}

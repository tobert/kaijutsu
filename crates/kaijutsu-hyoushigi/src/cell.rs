//! Cells: the three-part contract (position, body, state) and the recipe data.

use crate::content::ContentRef;
use kaijutsu_types::{PrincipalId, Span, TickDelta, TrackId};
use serde::{Deserialize, Serialize};

/// Identifier of a resolver, looked up in the engine's registry. Recipes carry
/// this id (data), not a closure, so cells persist and round-trip through storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResolverId(pub String);

impl ResolverId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a recipe reads from context — declares the slice that feeds
/// `compute_basis`. Too strict thrashes, too loose commits stale content; the
/// real projection is per-resolver and unwritten until a resolver exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextQuery {
    /// How far back in committed history this recipe reads (toward the past).
    pub lookback: TickDelta,
    /// Named ambient inputs this recipe depends on (resolver-interpreted).
    pub ambient_keys: Vec<String>,
}

/// The required real-time miss handler. Every [`Recipe`] carries one — an
/// omitted fallback is impossible by construction, so a miss with no time to
/// recover can never reach undefined behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fallback {
    /// Emit nothing — the playhead passes a hole.
    Skip,
    /// Reuse the last committed content of this lane.
    UseLastGood,
    /// A pre-baked literal, always available.
    Literal(ContentRef),
}

/// A deferred way to produce content: resolver id + params + the context it reads
/// + a required fallback. Data, not a closure — so cells round-trip through storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recipe {
    pub resolver: ResolverId,
    pub params: serde_json::Value,
    pub query: ContextQuery,
    pub fallback: Fallback,
}

/// How a cell produces its content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Body {
    /// A literal or a crystallized prior result — born committed.
    Concrete(ContentRef),
    /// Resolved on demand against context.
    Deferred(Recipe),
}

/// A cell's position in the lifecycle.
///
/// The block model's `Status` is a *lossy projection* of this: Committed→Done,
/// Squashed/Failed→Error, the speculating states invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellState {
    /// Deferred, not yet started.
    Pending,
    /// `resolve` is running against a snapshotted basis.
    Speculating,
    /// Resolved; awaiting the commit deadline.
    Speculated,
    /// Past the write barrier — immutable, crystallized to CAS. Terminal.
    Committed,
    /// Basis diverged at the deadline — discarded; may re-speculate.
    Squashed,
    /// An illegal/irrecoverable transition. Terminal.
    Failed,
}

impl CellState {
    /// The legal lifecycle edges. Illegal transitions must `Err` and leave state
    /// untouched (crash-over-corruption). `Committed` and `Failed` are terminal —
    /// the write barrier means a committed cell can never transition again.
    pub fn can_advance_to(self, next: CellState) -> bool {
        use CellState::*;
        matches!(
            (self, next),
            (Pending, Speculating)
                | (Speculating, Speculated)
                | (Speculating, Squashed)
                | (Speculated, Committed)
                | (Speculated, Squashed)
                | (Squashed, Speculating) // re-speculate if time remains
                | (Pending, Failed)
                | (Speculating, Failed)
                | (Speculated, Failed)
                | (Squashed, Failed)
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, CellState::Committed | CellState::Failed)
    }
}

/// A `Cell` is exactly three things: a **position**, a way to **produce
/// content**, and a **state**. Content *type* is deliberately not one of them —
/// it lives inside [`ContentRef`], opaque to the core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub span: Span,
    pub body: Body,
    pub state: CellState,
    /// The lane this cell belongs to. Required — an untracked cell is
    /// meaningless once tracks are first-class. No serde default: a stored
    /// track-less cell is corruption, and the codec should say so when cell
    /// persistence lands.
    ///
    /// LANE IDENTITY ONLY. `track` is the *only* lane key; `played_by` (the
    /// scheduling principal) is never a lane key — one track's cells span
    /// multiple principals (player + `beat()` fallbacks).
    pub track: TrackId,
    /// Who played: the principal whose turn produced this content, or
    /// [`PrincipalId::beat()`] when the transport itself did (fallback repeats,
    /// literals). Becomes `BlockId.principal_id` at materialization. This is
    /// provenance, **not** a lane key — attributing a vamp-insurance repeat to a
    /// player would be false provenance.
    pub played_by: PrincipalId,
}

impl Cell {
    /// A concrete cell is born `Committed`, carrying its lane (`track`) and its
    /// player (`played_by`).
    pub fn concrete_on(
        span: Span,
        content: ContentRef,
        track: TrackId,
        played_by: PrincipalId,
    ) -> Self {
        Self {
            span,
            body: Body::Concrete(content),
            state: CellState::Committed,
            track,
            played_by,
        }
    }

    /// A deferred cell starts `Pending`, carrying its lane (`track`) and its
    /// player (`played_by`).
    pub fn deferred_on(
        span: Span,
        recipe: Recipe,
        track: TrackId,
        played_by: PrincipalId,
    ) -> Self {
        Self {
            span,
            body: Body::Deferred(recipe),
            state: CellState::Pending,
            track,
            played_by,
        }
    }

    pub fn is_deferred(&self) -> bool {
        matches!(self.body, Body::Deferred(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::Tick;
    use serde_json::json;

    fn recipe() -> Recipe {
        Recipe {
            resolver: ResolverId::new("echo"),
            params: json!({ "beat": 1 }),
            query: ContextQuery::default(),
            fallback: Fallback::Skip,
        }
    }

    #[test]
    fn concrete_is_born_committed() {
        let c = Cell::concrete_on(
            Span::instant(Tick::new(0)),
            ContentRef::of(b"hi", "text/plain"),
            TrackId::solo(),
            PrincipalId::beat(),
        );
        assert_eq!(c.state, CellState::Committed);
        assert!(!c.is_deferred());
    }

    #[test]
    fn deferred_starts_pending() {
        let c = Cell::deferred_on(
            Span::instant(Tick::new(4)),
            recipe(),
            TrackId::solo(),
            PrincipalId::beat(),
        );
        assert_eq!(c.state, CellState::Pending);
        assert!(c.is_deferred());
    }

    #[test]
    fn lifecycle_legal_edges() {
        use CellState::*;
        assert!(Pending.can_advance_to(Speculating));
        assert!(Speculating.can_advance_to(Speculated));
        assert!(Speculated.can_advance_to(Committed));
        assert!(Speculated.can_advance_to(Squashed));
        assert!(Squashed.can_advance_to(Speculating)); // re-speculate
        assert!(Speculating.can_advance_to(Failed));
    }

    #[test]
    fn lifecycle_rejects_illegal_edges() {
        use CellState::*;
        // can't skip straight to committed
        assert!(!Pending.can_advance_to(Committed));
        // the write barrier: committed is terminal
        assert!(!Committed.can_advance_to(Speculating));
        assert!(!Committed.can_advance_to(Failed));
        assert!(Committed.is_terminal());
        assert!(Failed.is_terminal());
    }

    #[test]
    fn recipe_round_trips_through_storage() {
        // cells persist: a recipe is data, not a closure
        let r = recipe();
        let bytes = serde_json::to_vec(&r).unwrap();
        let back: Recipe = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(r, back);
    }
}

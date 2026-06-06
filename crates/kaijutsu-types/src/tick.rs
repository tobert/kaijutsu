//! Logical timeline coordinates: [`Tick`] (position) and [`TickDelta`] (duration).
//!
//! This is the point-vs-vector (affine) distinction — the same one `std::time`
//! draws between `Instant` and `Duration` — applied to logical timeline
//! positions. [`Tick`] is an absolute position; [`TickDelta`] is a signed offset
//! between two positions. Adding two positions (`Tick + Tick`) is a *compile
//! error* by design: there is no `Add<Tick> for Tick` impl, exactly as
//! `Instant + Instant` is rejected.
//!
//! The coordinate carries **no wall-clock**. Mapping `Tick ↔ wall-clock` is a
//! separate domain bound at the driver/PPQ boundary (PPQ + tempo + epoch) — see
//! `docs/hyoushigi.md`. Both newtypes wrap `i64`: [`Tick`] is monotone and
//! ordered (the write barrier asks "is this behind the commit point?"), while
//! [`TickDelta`] is signed because an emitted cell may append to the *past*, so
//! `earlier − later` is legitimately negative.

use serde::{Deserialize, Serialize};
use std::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};

/// An absolute position on a timeline — a *point*.
///
/// Monotone and totally ordered. Combine with a [`TickDelta`] to move; subtract
/// two `Tick`s to measure the gap. `Tick + Tick` does not compile.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Tick(pub i64);

/// A signed duration/offset between two positions — a *vector*.
///
/// May be negative (an offset toward the past). Closed under `+`/`-` and scalar
/// multiplication.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TickDelta(pub i64);

impl Tick {
    /// The timeline origin.
    pub const ZERO: Tick = Tick(0);

    #[inline]
    pub const fn new(value: i64) -> Self {
        Tick(value)
    }

    /// The raw logical coordinate. Use sparingly — prefer the algebra.
    #[inline]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl TickDelta {
    /// A zero-length offset (an *instant* span has `len == ZERO`).
    pub const ZERO: TickDelta = TickDelta(0);

    #[inline]
    pub const fn new(value: i64) -> Self {
        TickDelta(value)
    }

    #[inline]
    pub const fn get(self) -> i64 {
        self.0
    }

    /// True for a strictly-forward offset.
    #[inline]
    pub const fn is_forward(self) -> bool {
        self.0 > 0
    }
}

// --- The affine algebra ---------------------------------------------------
//
//   Tick      + TickDelta → Tick        (point offset by a vector)
//   TickDelta + Tick      → Tick        (commutative convenience)
//   Tick      − TickDelta → Tick
//   Tick      − Tick      → TickDelta   (difference of points is a vector)
//   TickDelta ± TickDelta → TickDelta
//   TickDelta × i64       → TickDelta   (and i64 × TickDelta)
//   −TickDelta            → TickDelta
//
// Deliberately *absent*: `Add<Tick> for Tick` and any `i64`-on-`Tick` op.
// Adding two positions is meaningless; the compile-fail tests pin that.

impl Add<TickDelta> for Tick {
    type Output = Tick;
    #[inline]
    fn add(self, rhs: TickDelta) -> Tick {
        Tick(self.0 + rhs.0)
    }
}

impl Add<Tick> for TickDelta {
    type Output = Tick;
    #[inline]
    fn add(self, rhs: Tick) -> Tick {
        Tick(self.0 + rhs.0)
    }
}

impl Sub<TickDelta> for Tick {
    type Output = Tick;
    #[inline]
    fn sub(self, rhs: TickDelta) -> Tick {
        Tick(self.0 - rhs.0)
    }
}

impl Sub<Tick> for Tick {
    type Output = TickDelta;
    #[inline]
    fn sub(self, rhs: Tick) -> TickDelta {
        TickDelta(self.0 - rhs.0)
    }
}

impl Add for TickDelta {
    type Output = TickDelta;
    #[inline]
    fn add(self, rhs: TickDelta) -> TickDelta {
        TickDelta(self.0 + rhs.0)
    }
}

impl Sub for TickDelta {
    type Output = TickDelta;
    #[inline]
    fn sub(self, rhs: TickDelta) -> TickDelta {
        TickDelta(self.0 - rhs.0)
    }
}

impl Mul<i64> for TickDelta {
    type Output = TickDelta;
    #[inline]
    fn mul(self, rhs: i64) -> TickDelta {
        TickDelta(self.0 * rhs)
    }
}

impl Mul<TickDelta> for i64 {
    type Output = TickDelta;
    #[inline]
    fn mul(self, rhs: TickDelta) -> TickDelta {
        TickDelta(self * rhs.0)
    }
}

impl Neg for TickDelta {
    type Output = TickDelta;
    #[inline]
    fn neg(self) -> TickDelta {
        TickDelta(-self.0)
    }
}

impl AddAssign<TickDelta> for Tick {
    #[inline]
    fn add_assign(&mut self, rhs: TickDelta) {
        self.0 += rhs.0;
    }
}

impl SubAssign<TickDelta> for Tick {
    #[inline]
    fn sub_assign(&mut self, rhs: TickDelta) {
        self.0 -= rhs.0;
    }
}

/// A half-open position-and-extent on a timeline.
///
/// `start` is a position; `len` is a duration. `len == TickDelta::ZERO` is an
/// instant. The end position is `start + len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: Tick,
    pub len: TickDelta,
}

impl Span {
    #[inline]
    pub const fn new(start: Tick, len: TickDelta) -> Self {
        Span { start, len }
    }

    /// An instant at `start` (`len == 0`).
    #[inline]
    pub const fn instant(start: Tick) -> Self {
        Span {
            start,
            len: TickDelta::ZERO,
        }
    }

    /// The end position, `start + len`.
    #[inline]
    pub fn end(self) -> Tick {
        self.start + self.len
    }

    #[inline]
    pub const fn is_instant(self) -> bool {
        self.len.0 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_plus_vector_is_point() {
        assert_eq!(Tick::new(10) + TickDelta::new(5), Tick::new(15));
        // commutes
        assert_eq!(TickDelta::new(5) + Tick::new(10), Tick::new(15));
    }

    #[test]
    fn point_minus_point_is_vector() {
        assert_eq!(Tick::new(15) - Tick::new(10), TickDelta::new(5));
        // and is signed: earlier − later goes negative
        assert_eq!(Tick::new(10) - Tick::new(15), TickDelta::new(-5));
    }

    #[test]
    fn point_minus_vector_is_point() {
        assert_eq!(Tick::new(15) - TickDelta::new(5), Tick::new(10));
        // a negative delta walks forward
        assert_eq!(Tick::new(10) - TickDelta::new(-5), Tick::new(15));
    }

    #[test]
    fn vectors_are_closed_under_add_sub() {
        assert_eq!(TickDelta::new(3) + TickDelta::new(4), TickDelta::new(7));
        assert_eq!(TickDelta::new(3) - TickDelta::new(4), TickDelta::new(-1));
    }

    #[test]
    fn vector_scales_either_side() {
        assert_eq!(TickDelta::new(3) * 4, TickDelta::new(12));
        assert_eq!(4 * TickDelta::new(3), TickDelta::new(12));
        assert_eq!(-TickDelta::new(3), TickDelta::new(-3));
    }

    #[test]
    fn assign_ops_walk_a_point() {
        let mut t = Tick::new(0);
        t += TickDelta::new(24);
        t -= TickDelta::new(1);
        assert_eq!(t, Tick::new(23));
    }

    #[test]
    fn ticks_order_as_positions() {
        assert!(Tick::new(1) < Tick::new(2));
        let mut v = [Tick::new(3), Tick::new(1), Tick::new(2)];
        v.sort();
        assert_eq!(v, [Tick::new(1), Tick::new(2), Tick::new(3)]);
    }

    #[test]
    fn span_geometry() {
        let s = Span::new(Tick::new(8), TickDelta::new(4));
        assert_eq!(s.end(), Tick::new(12));
        assert!(!s.is_instant());
        assert!(Span::instant(Tick::new(8)).is_instant());
    }

    #[test]
    fn serde_is_transparent() {
        // newtypes serialize as the bare integer
        assert_eq!(serde_json::to_string(&Tick::new(42)).unwrap(), "42");
        assert_eq!(serde_json::to_string(&TickDelta::new(-7)).unwrap(), "-7");
        let t: Tick = serde_json::from_str("42").unwrap();
        assert_eq!(t, Tick::new(42));
    }
}

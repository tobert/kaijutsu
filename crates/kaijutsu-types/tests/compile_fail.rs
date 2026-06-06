//! Compile-fail tests pinning the affine `Tick`/`TickDelta` algebra.
//!
//! These are the "tests that will fail when we make the mistake": if someone
//! ever adds `Add<Tick> for Tick` (or an `i64`-on-`Tick` op), the meaningless
//! operation would start compiling and this harness would catch it.

#[test]
fn tick_arithmetic_rejects_meaningless_ops() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}

//! Regression test for a chunking-transparency counterexample found by
//! `fuzz/fuzz_targets/chunked_equivalence.rs` within its first minute of
//! running (60s, ~millions of iterations, no seed corpus).
//!
//! # The finding
//!
//! `docs/ansi-and-beyond.md` ("Chunking") and this crate's doc comments
//! assert that `vte` carries partial state across `feed` calls such that
//! splitting the input anywhere never changes the result. That claim is
//! **false** for a specific adversarial shape: an incomplete multi-byte
//! UTF-8 lead byte left pending at the end of one `feed` call, where the
//! *next* `feed` call's bytes begin with the continuation byte(s)
//! immediately followed by a control byte (ESC and friends are valid
//! one-byte UTF-8 code points, 0x00-0x7F).
//!
//! The bug lives in `vte` 0.15.0's `Parser::advance_partial_utf8`
//! (`vte-0.15.0/src/lib.rs:668-718`), not in this crate. To resume a
//! pending codepoint it speculatively copies up to 3 more bytes from the
//! new chunk into its 4-byte `partial_utf8` buffer and validates the whole
//! buffer with `str::from_utf8`. A trailing ASCII control byte (e.g. ESC,
//! 0x1B) validates as an ordinary one-byte `char` under `str::from_utf8` —
//! it doesn't know control bytes are special — so the validated run can
//! extend past the first codepoint. The code then does exactly what its own
//! comment says: "we only care about the first character... we just ignore
//! the rest" — but it still reports the *whole* validated span (through the
//! ignored control byte) as consumed. The control byte is silently dropped:
//! never printed, never routed through `Perform::execute`/the escape state
//! machine, and never seen again. In the one-shot (non-chunked) path the
//! same bytes never enter `advance_partial_utf8` at all — `advance_ground`
//! finds the ESC via `memchr` and dispatches it correctly — so only the
//! chunked path loses the byte.
//!
//! # Impact on `ansi-strip`
//!
//! Concretely: for input `[0xCD, 0xAE, 0x1B, 0xFF]` (`0xCD 0xAE` = U+036E, a
//! combining mark, followed by ESC and an invalid lead byte), one-shot
//! `strip` yields `"\u{36E}"` (the ESC starts a two-byte escape that
//! produces nothing, per this crate's "cursor/erase vanish" policy).
//! Chunking at byte offset 1 — `feed(&data[..1]); feed(&data[1..]);` —
//! yields `"\u{36E}\u{FFFD}"` instead: the swallowed ESC byte is never
//! recognized as starting an escape sequence, so the following `0xFF` falls
//! through to plain UTF-8 decoding and becomes a replacement character.
//!
//! This is a genuine, if extremely narrow, violation of the "chunk anywhere,
//! same result" contract: it requires an *invalid or non-ASCII-plus-partial*
//! UTF-8 lead byte to be the very last byte of one `feed` call, with a
//! control byte immediately after the completed codepoint in the next
//! `feed` call. Sequences chunked byte-at-a-time do not trigger it (each
//! `advance_partial_utf8` call then only ever has 1 byte available to copy),
//! nor does chunking that keeps a complete UTF-8 run entirely within one
//! `feed` call — which is why `tests/properties.rs`'s fixed `nasty_inputs`
//! corpus, exhaustively split at every position/pair of positions, never
//! happened to hit this shape.
//!
//! # Status
//!
//! Filed as a known issue (see `docs/issues.md`) rather than worked around
//! here: fixing it would mean this crate reimplementing part of `vte`'s
//! partial-UTF-8 resumption, which is exactly the kind of ad-hoc parser
//! surface `docs/ansi-and-beyond.md` says to avoid. This test pins the
//! *current* (buggy) behavior so a `vte` upgrade that fixes it is caught —
//! flip the assertion to expect equality once it does, and delete this
//! test's rationale comment in favor of one line noting which `vte` version
//! fixed it.

use kaijutsu_ansi::{StripParser, strip};

#[test]
fn known_bug_vte_015_drops_esc_after_chunked_partial_utf8() {
    let data: [u8; 4] = [0xCD, 0xAE, 0x1B, 0xFF];

    let one_shot = strip(&data);
    assert_eq!(
        one_shot.0, "\u{36E}",
        "one-shot behavior changed \u{2014} re-derive this test's expectations"
    );
    assert!(one_shot.1.is_empty());

    let mut parser = StripParser::new();
    parser.feed(&data[..1]);
    parser.feed(&data[1..]);
    let chunked = parser.finish();

    // This is the bug: chunked output currently DIVERGES from one-shot.
    // A U+FFFD leaks in because the ESC byte was silently swallowed by
    // `vte::Parser::advance_partial_utf8` instead of starting an escape
    // sequence. If this assertion starts failing, `vte` likely fixed the
    // upstream issue — see the module doc for what to do next.
    assert_ne!(
        chunked, one_shot,
        "vte's partial-UTF-8 chunking bug appears to be fixed upstream; \
         update this test (and docs/issues.md) to assert equality instead"
    );
    assert_eq!(chunked.0, "\u{36E}\u{FFFD}");

    // Byte-at-a-time chunking of the same bytes does NOT trigger the bug —
    // `advance_partial_utf8` only ever sees one byte to copy at a time, so
    // it never over-validates past the ESC. This is what makes the bug easy
    // to miss: it depends on chunk *sizes*, not merely chunk *positions*.
    let mut byte_at_a_time = StripParser::new();
    for b in &data {
        byte_at_a_time.feed(std::slice::from_ref(b));
    }
    assert_eq!(byte_at_a_time.finish(), one_shot);
}

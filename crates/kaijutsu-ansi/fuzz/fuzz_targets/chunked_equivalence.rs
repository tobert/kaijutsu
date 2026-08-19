//! Chunking fuzz target: however `strip`'s input bytes are split into
//! `StripParser::feed` calls, the result must equal one-shot `strip`. This
//! is the crate's core streaming claim (`docs/ansi-and-beyond.md`,
//! "Chunking") — a chunk boundary may fall mid escape sequence, mid SGR
//! parameter, or mid UTF-8 codepoint, and the result must not change.

#![no_main]

use arbitrary::Arbitrary;
use kaijutsu_ansi::{strip, StripParser};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
struct Input {
    data: Vec<u8>,
    /// Raw split-point candidates; each is reduced mod `data.len() + 1` and
    /// used as a byte offset. Arbitrary length and values, so chunk count
    /// and boundary positions both vary freely under the fuzzer.
    split_points: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let Input { data, split_points } = input;

    let one_shot = strip(&data);

    // Derive sorted, deduplicated split offsets in [0, data.len()].
    let modulus = data.len() as u64 + 1;
    let mut offsets: Vec<usize> = split_points
        .iter()
        .map(|&b| (u64::from(b) % modulus) as usize)
        .collect();
    offsets.push(0);
    offsets.push(data.len());
    offsets.sort_unstable();
    offsets.dedup();

    let mut parser = StripParser::new();
    for window in offsets.windows(2) {
        let (start, end) = (window[0], window[1]);
        parser.feed(&data[start..end]);
    }
    let chunked = parser.finish();

    assert_eq!(
        chunked, one_shot,
        "chunked result diverged from one-shot for {} byte input split at {offsets:?}",
        data.len()
    );
});

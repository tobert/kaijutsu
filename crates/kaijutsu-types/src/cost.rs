//! Writing a cost: micro-dollars as dollars, the same everywhere it is shown.

/// `$1.24`. The precision scales with the amount: thousandths under ten
/// cents (`$0.042`), cents to a hundred dollars, whole dollars past it.
pub fn format_micro_dollars(micros: u64) -> String {
    if micros < 100_000 {
        let thousandths = micros.saturating_add(500) / 1_000;
        format!("${}.{:03}", thousandths / 1_000, thousandths % 1_000)
    } else if micros < 100_000_000 {
        let cents = micros.saturating_add(5_000) / 10_000;
        format!("${}.{:02}", cents / 100, cents % 100)
    } else {
        format!("${}", micros.saturating_add(500_000) / 1_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::format_micro_dollars;

    #[test]
    fn precision_scales_with_the_amount() {
        for (micros, text) in [
            (0, "$0.000"),
            (42_000, "$0.042"),
            (99_600, "$0.100"),
            (100_000, "$0.10"),
            (1_240_000, "$1.24"),
            (99_994_000, "$99.99"),
            (123_400_000, "$123"),
            (u64::MAX, "$18446744073709"),
        ] {
            assert_eq!(format_micro_dollars(micros), text, "{micros}");
        }
    }
}

//! Pricing: what a call's tokens cost under one price.
//!
//! A price is [`ModelPriceRow`] rates in integer micro-dollars per million
//! tokens, so tokens times a rate over `1_000_000` is micro-dollars. Cost is
//! `None` when the price has no rate for tokens the call used: a cost we
//! cannot compute is unknown, never a low number.
//!
//! Providers count input differently. Anthropic's `input` leaves out cache
//! reads and writes, which are billed on their own; an OpenAI-compatible
//! provider's `input` already includes the cache hit, so the hit is billed at
//! the cache rate and only the rest at the input rate ([`InputStyle`]).

use std::collections::HashMap;

use kaijutsu_types::ContextId;

use crate::kernel_db::{CostGroup, KernelDb, KernelDbResult, ModelPriceRow};
use crate::llm::BackendKind;

/// How a provider's `input` count relates to its cache counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputStyle {
    /// `input` leaves out cache reads and writes (Anthropic).
    ExcludesCache,
    /// `input` includes the cache hit (OpenAI-compatible, megakernel).
    IncludesCacheHit,
}

impl InputStyle {
    pub fn for_kind(kind: &BackendKind) -> Self {
        match kind {
            BackendKind::Anthropic => Self::ExcludesCache,
            _ => Self::IncludesCacheHit,
        }
    }
}

/// Raw token counts for one call or a sum of calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Every cache write, whatever its TTL.
    pub cache_write: u64,
    /// The part of `cache_write` written with the 1-hour TTL.
    pub cache_write_1h: u64,
}

impl From<&CostGroup> for Tokens {
    fn from(group: &CostGroup) -> Self {
        let n = |v: i64| v.max(0) as u64;
        Self {
            input: n(group.input_tokens),
            output: n(group.output_tokens),
            cache_read: n(group.cache_read_tokens),
            cache_write: n(group.cache_write_tokens),
            cache_write_1h: n(group.cache_write_1h_tokens),
        }
    }
}

/// The cost of `tokens` under `price`, in micro-dollars, or `None` when a
/// kind of token was used that the price has no rate for.
pub fn cost_micros(price: &ModelPriceRow, style: InputStyle, tokens: &Tokens) -> Option<u64> {
    // A rate is needed only for tokens the call used.
    let rate = |tokens: u64, rate: Option<i64>| -> Option<u128> {
        if tokens == 0 {
            Some(0)
        } else {
            rate.map(|r| tokens as u128 * r.max(0) as u128)
        }
    };
    let fresh_input = match style {
        InputStyle::ExcludesCache => tokens.input,
        InputStyle::IncludesCacheHit => tokens.input.saturating_sub(tokens.cache_read),
    };
    let write_1h = tokens.cache_write_1h.min(tokens.cache_write);
    let write_5m = tokens.cache_write - write_1h;
    let total = rate(fresh_input, Some(price.input))?
        + rate(tokens.output, Some(price.output))?
        + rate(tokens.cache_read, price.cache_read)?
        + rate(write_5m, price.cache_write_5m)?
        + rate(write_1h, price.cache_write_1h)?;
    // Round to the nearest micro-dollar once, over the whole sum.
    u64::try_from((total + 500_000) / 1_000_000).ok()
}

/// What a context's turns have cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextCost {
    /// Micro-dollars over the calls that could be priced.
    pub micros: u64,
    /// Calls priced.
    pub priced_calls: u64,
    /// Calls with no price in force, or a price missing a rate they needed.
    pub unpriced_calls: u64,
}

/// Every context's cost, each turn priced by the price in force when it
/// started. `style_of` maps a provider name to how it counts input.
pub fn context_costs(
    db: &KernelDb,
    style_of: impl Fn(&str) -> InputStyle,
) -> KernelDbResult<HashMap<ContextId, ContextCost>> {
    let mut prices: HashMap<i64, Option<ModelPriceRow>> = HashMap::new();
    let mut costs: HashMap<ContextId, ContextCost> = HashMap::new();
    for group in db.context_cost_groups(None)? {
        let calls = group.llm_calls.max(0) as u64;
        let price = match group.price_id {
            Some(id) => match prices.entry(id) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut().clone(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(db.get_model_price(id)?).clone(),
            },
            None => None,
        };
        let micros = price
            .as_ref()
            .and_then(|p| cost_micros(p, style_of(&group.provider), &Tokens::from(&group)));
        let entry = costs.entry(group.context_id).or_default();
        match micros {
            Some(m) => {
                entry.micros = entry.micros.saturating_add(m);
                entry.priced_calls += calls;
            }
            None => entry.unpriced_calls += calls,
        }
    }
    Ok(costs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(cache_read: Option<i64>, w5: Option<i64>, w1h: Option<i64>) -> ModelPriceRow {
        ModelPriceRow {
            price_id: 1,
            provider: "p".into(),
            model: "m".into(),
            effective_from: 0,
            input: 3_000_000,
            output: 15_000_000,
            cache_read,
            cache_write_5m: w5,
            cache_write_1h: w1h,
            source: String::new(),
            set_by: None,
            created_at: 0,
        }
    }

    fn full() -> ModelPriceRow {
        price(Some(300_000), Some(3_750_000), Some(6_000_000))
    }

    /// Anthropic: input, output, cache reads, and cache writes split by TTL
    /// each at their own rate. $3 in, $15 out, $0.30 read, $3.75 / $6 write.
    #[test]
    fn claude_tokens_are_priced_each_at_its_own_rate() {
        let tokens = Tokens { input: 1_000, output: 2_000, cache_read: 10_000, cache_write: 4_000, cache_write_1h: 1_000 };
        // 1k*3 + 2k*15 + 10k*0.3 + 3k*3.75 + 1k*6 = 3000+30000+3000+11250+6000 micro-cents-per-M... in micro-dollars/1e6
        // = (1000*3_000_000 + 2000*15_000_000 + 10_000*300_000 + 3_000*3_750_000 + 1_000*6_000_000) / 1e6
        assert_eq!(cost_micros(&full(), InputStyle::ExcludesCache, &tokens), Some(53_250));
    }

    /// An OpenAI-compatible provider counts the cache hit inside `input`, so
    /// only the rest is billed at the input rate.
    #[test]
    fn a_cache_hit_inside_input_is_not_billed_twice() {
        let tokens = Tokens { input: 11_000, output: 0, cache_read: 10_000, ..Default::default() };
        // (1_000 * 3_000_000 + 10_000 * 300_000) / 1e6 = 6_000
        assert_eq!(cost_micros(&full(), InputStyle::IncludesCacheHit, &tokens), Some(6_000));
        // The same counts under Anthropic's convention are 11k fresh plus 10k read.
        assert_eq!(cost_micros(&full(), InputStyle::ExcludesCache, &tokens), Some(36_000));
    }

    /// A rate left unknown makes the cost unknown only when those tokens were
    /// used; a call that used none of them is still priced.
    #[test]
    fn a_missing_cache_rate_makes_the_cost_unknown_only_when_it_was_needed() {
        let no_cache = price(None, None, None);
        let plain = Tokens { input: 1_000, output: 1_000, ..Default::default() };
        assert_eq!(cost_micros(&no_cache, InputStyle::ExcludesCache, &plain), Some(18_000));
        let read = Tokens { cache_read: 1, ..plain };
        assert_eq!(cost_micros(&no_cache, InputStyle::ExcludesCache, &read), None);
        let write = Tokens { cache_write: 1, ..plain };
        assert_eq!(cost_micros(&no_cache, InputStyle::ExcludesCache, &write), None);
        let only_5m = price(None, Some(3_750_000), None);
        let w5 = Tokens { cache_write: 100, ..plain };
        assert!(cost_micros(&only_5m, InputStyle::ExcludesCache, &w5).is_some());
        let w1 = Tokens { cache_write: 100, cache_write_1h: 100, ..plain };
        assert_eq!(cost_micros(&only_5m, InputStyle::ExcludesCache, &w1), None);
    }

    /// A free model is a price of zero, and costs zero.
    #[test]
    fn a_zero_price_costs_nothing_and_rounds_to_the_nearest_micro_dollar() {
        let free = ModelPriceRow { input: 0, output: 0, cache_read: Some(0), cache_write_5m: Some(0), cache_write_1h: Some(0), ..full() };
        let tokens = Tokens { input: 9_999, output: 9_999, cache_read: 9_999, cache_write: 9_999, cache_write_1h: 5 };
        assert_eq!(cost_micros(&free, InputStyle::ExcludesCache, &tokens), Some(0));
        // 1 token at $3/M is 3 micro-dollars; half a micro-dollar rounds up.
        let one = Tokens { input: 1, ..Default::default() };
        assert_eq!(cost_micros(&full(), InputStyle::ExcludesCache, &one), Some(3));
        let half = ModelPriceRow { input: 500_000, ..full() };
        assert_eq!(cost_micros(&half, InputStyle::ExcludesCache, &one), Some(1));
    }
}

use super::*;
use crate::kernel_db::*;

fn new_price(model: &str, effective_from: i64, input: i64, output: i64) -> NewModelPrice {
    NewModelPrice {
        provider: "anthropic".into(),
        model: model.into(),
        effective_from,
        input,
        output,
        cache_read: Some(input / 10),
        cache_write_5m: Some(input * 5 / 4),
        cache_write_1h: Some(input * 2),
        source: "test".into(),
        set_by: None,
    }
}

/// The price in force is the latest one at or before the instant, a later
/// insert wins a tie on `effective_from`, and a model with no price
/// before the instant has none: unknown, not free.
#[test]
fn the_effective_price_is_the_latest_at_or_before_the_instant() {
    let db = KernelDb::temporary().unwrap();
    let first = db.insert_model_price(&new_price("m", 1_000, 3_000_000, 15_000_000)).unwrap();
    let second = db.insert_model_price(&new_price("m", 5_000, 1_000_000, 5_000_000)).unwrap();
    let fix = db.insert_model_price(&new_price("m", 5_000, 1_500_000, 5_000_000)).unwrap();
    let at = |t| db.effective_model_price("anthropic", "m", t).unwrap().map(|p| p.price_id);
    assert_eq!(at(999), None);
    assert_eq!(at(1_000), Some(first));
    assert_eq!(at(4_999), Some(first));
    assert_eq!(at(5_000), Some(fix), "the later insert corrects the earlier one");
    assert_ne!(first, second);
    assert_eq!(db.effective_model_price("anthropic", "other", 9_000).unwrap(), None);
    assert_eq!(db.list_model_prices(Some(("anthropic", "m"))).unwrap().len(), 3);
}

/// History is append-only: the database refuses to rewrite or remove a
/// price, and refuses a negative rate.
#[test]
fn prices_cannot_be_rewritten_or_removed_or_negative() {
    let db = KernelDb::temporary().unwrap();
    let id = db.insert_model_price(&new_price("m", 1_000, 3_000_000, 15_000_000)).unwrap();
    let update = db.conn.execute("UPDATE model_prices SET input_micros = 1 WHERE price_id = ?1", [id]);
    assert!(update.unwrap_err().to_string().contains("append-only"));
    let delete = db.conn.execute("DELETE FROM model_prices WHERE price_id = ?1", [id]);
    assert!(delete.unwrap_err().to_string().contains("append-only"));
    let mut negative = new_price("m", 2_000, 1, 1);
    negative.output = -1;
    assert!(matches!(db.insert_model_price(&negative), Err(KernelDbError::Validation(_))));
    assert_eq!(db.get_model_price(id).unwrap().unwrap().input, 3_000_000);
}

/// Turns are summed per price in force when each started: a price change
/// splits a context's turns into two groups, and a turn before any price
/// is a group with no price.
#[test]
fn cost_groups_split_turns_by_the_price_in_force_at_their_start() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cost-groups"));
    insert_context_with_doc(&db, &ctx, ws_id);
    let old = db.insert_model_price(&new_price("claude-haiku-5-5", 2_000, 1_000_000, 5_000_000)).unwrap();
    let new = db.insert_model_price(&new_price("claude-haiku-5-5", 4_000, 800_000, 4_000_000)).unwrap();
    for (started_at, input) in [(1_000, 1), (2_500, 10), (3_000, 20), (4_500, 100)] {
        let mut start = turn_start(ctx.context_id);
        start.started_at = started_at;
        db.begin_turn(&start).unwrap();
        db.add_turn_usage(start.turn_id, RawUsage { input, output: 2, cache_write: 5, cache_write_1h: 3, ..Default::default() }).unwrap();
    }
    let groups = db.context_cost_groups(Some(ctx.context_id)).unwrap();
    let by_price: Vec<(Option<i64>, i64, i64)> =
        groups.iter().map(|g| (g.price_id, g.llm_calls, g.input_tokens)).collect();
    assert_eq!(by_price, vec![(None, 1, 1), (Some(old), 2, 30), (Some(new), 1, 100)]);
    assert_eq!(groups[1].cache_write_1h_tokens, 6);
    assert!(db.context_cost_groups(Some(ContextId::new())).unwrap().is_empty());
    assert_eq!(db.context_cost_groups(None).unwrap().len(), 3);
}

/// A context's cost sums its turns, each at the price in force when it
/// started; turns with no price are counted apart, never as free.
#[test]
fn a_contexts_cost_prices_each_turn_by_the_price_in_force() {
    use crate::pricing::{context_costs, ContextCost, InputStyle};
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cost"));
    insert_context_with_doc(&db, &ctx, ws_id);
    let per_million = |provider: &str, effective_from, input| NewModelPrice {
        provider: provider.into(),
        model: "claude-haiku-5-5".into(),
        effective_from,
        input,
        output: input * 5,
        cache_read: Some(0),
        cache_write_5m: Some(0),
        cache_write_1h: Some(0),
        source: String::new(),
        set_by: None,
    };
    db.insert_model_price(&per_million("anthropic", 2_000, 1_000_000)).unwrap();
    db.insert_model_price(&per_million("anthropic", 4_000, 2_000_000)).unwrap();
    for (provider, started_at) in [("anthropic", 1_000), ("anthropic", 3_000), ("anthropic", 5_000), ("other", 5_000)] {
        let mut start = turn_start(ctx.context_id);
        start.provider = provider.into();
        start.started_at = started_at;
        db.begin_turn(&start).unwrap();
        db.add_turn_usage(start.turn_id, RawUsage { input: 1_000_000, ..Default::default() }).unwrap();
    }
    let costs = context_costs(&db, None, |p| (p == "anthropic").then_some(InputStyle::ExcludesCache)).unwrap();
    assert_eq!(
        costs[&ctx.context_id],
        ContextCost { micros: 3_000_000, priced_calls: 2, unpriced_calls: 2 }
    );
    // A provider that is no longer configured has an unknown input
    // convention, so even a priced turn on it is unpriced.
    let unknown = context_costs(&db, Some(ctx.context_id), |_| None).unwrap();
    assert_eq!(unknown[&ctx.context_id], ContextCost { micros: 0, priced_calls: 0, unpriced_calls: 4 });
}

fn new_tool_price(provider: &str, tool: &str, effective_from: i64, per_call: i64) -> NewToolPrice {
    NewToolPrice {
        provider: provider.into(),
        tool: tool.into(),
        effective_from,
        per_call,
        source: "test".into(),
        set_by: None,
    }
}

/// Tool prices follow the model-price rules: the latest `effective_from` at
/// or before the instant wins, a later insert wins a tie, and no price is
/// unknown. They are keyed by provider and tool.
#[test]
fn the_effective_tool_price_is_the_latest_at_or_before_the_instant() {
    let db = KernelDb::temporary().unwrap();
    let first = db.insert_tool_price(&new_tool_price("anthropic", "web_search", 1_000, 10_000)).unwrap();
    let fix = db.insert_tool_price(&new_tool_price("anthropic", "web_search", 1_000, 12_000)).unwrap();
    let later = db.insert_tool_price(&new_tool_price("anthropic", "web_search", 5_000, 8_000)).unwrap();
    db.insert_tool_price(&new_tool_price("alibaba", "web_search", 1_000, 5_000)).unwrap();
    let at = |provider, tool, t| db.effective_tool_price(provider, tool, t).unwrap().map(|p| p.price_id);
    assert_eq!(at("anthropic", "web_search", 999), None);
    assert_eq!(at("anthropic", "web_search", 1_000), Some(fix), "the later insert corrects the earlier one");
    assert_eq!(at("anthropic", "web_search", 5_000), Some(later));
    assert_eq!(at("anthropic", "web_extractor", 9_000), None);
    assert_ne!(first, fix);
    assert_eq!(db.list_tool_prices(None).unwrap().len(), 4);
    assert_eq!(db.list_tool_prices(Some(("anthropic", "web_search"))).unwrap().len(), 3);
    let row = db.get_tool_price(later).unwrap().unwrap();
    assert_eq!((row.provider.as_str(), row.tool.as_str(), row.per_call, row.source.as_str()), ("anthropic", "web_search", 8_000, "test"));
}

/// Tool-price history is append-only like model-price history, and refuses a
/// negative or unnamed price.
#[test]
fn tool_prices_cannot_be_rewritten_or_removed_or_invalid() {
    let db = KernelDb::temporary().unwrap();
    let id = db.insert_tool_price(&new_tool_price("anthropic", "web_search", 1_000, 10_000)).unwrap();
    let update = db.conn.execute("UPDATE tool_prices SET per_call_micros = 1 WHERE price_id = ?1", [id]);
    assert!(update.unwrap_err().to_string().contains("append-only"));
    let delete = db.conn.execute("DELETE FROM tool_prices WHERE price_id = ?1", [id]);
    assert!(delete.unwrap_err().to_string().contains("append-only"));
    assert!(matches!(db.insert_tool_price(&new_tool_price("anthropic", "web_search", 2_000, -1)), Err(KernelDbError::Validation(_))));
    assert!(matches!(db.insert_tool_price(&new_tool_price("", "web_search", 2_000, 1)), Err(KernelDbError::Validation(_))));
    assert!(matches!(db.insert_tool_price(&new_tool_price("anthropic", "", 2_000, 1)), Err(KernelDbError::Validation(_))));
    assert_eq!(db.get_tool_price(id).unwrap().unwrap().per_call, 10_000);
}

/// Tool uses are grouped per context, provider, tool and the tool price in
/// force when the turn started.
#[test]
fn tool_use_groups_split_by_the_price_in_force_at_turn_start() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("tool-groups"));
    insert_context_with_doc(&db, &ctx, ws_id);
    let old = db.insert_tool_price(&new_tool_price("anthropic", "web_search", 2_000, 10_000)).unwrap();
    let new = db.insert_tool_price(&new_tool_price("anthropic", "web_search", 4_000, 8_000)).unwrap();
    for (started_at, calls) in [(1_000, 1), (2_500, 2), (3_000, 4), (4_500, 8)] {
        let mut start = turn_start(ctx.context_id);
        start.started_at = started_at;
        db.begin_turn(&start).unwrap();
        db.add_turn_tool_uses(start.turn_id, &[("web_search", calls)]).unwrap();
    }
    let groups = db.context_tool_use_groups(Some(ctx.context_id)).unwrap();
    let shape: Vec<(Option<i64>, i64)> = groups.iter().map(|g| (g.price_id, g.calls)).collect();
    assert_eq!(shape, vec![(None, 1), (Some(old), 6), (Some(new), 8)]);
    assert!(db.context_tool_use_groups(Some(ContextId::new())).unwrap().is_empty());
    assert_eq!(db.context_tool_use_groups(None).unwrap().len(), 3);
}

/// A tool use adds its priced cost to the context; one with no price in force
/// counts as unpriced calls and never as free. A price set after the turn
/// began does not reach back.
#[test]
fn a_contexts_cost_includes_priced_tool_uses_and_counts_unpriced_ones() {
    use crate::pricing::{context_costs, ContextCost, InputStyle};
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("tool-cost"));
    insert_context_with_doc(&db, &ctx, ws_id);
    db.insert_model_price(&NewModelPrice {
        provider: "anthropic".into(), model: "claude-haiku-5-5".into(), effective_from: 0,
        input: 1_000_000, output: 5_000_000, cache_read: Some(0), cache_write_5m: Some(0), cache_write_1h: Some(0),
        source: String::new(), set_by: None,
    }).unwrap();
    db.insert_tool_price(&new_tool_price("anthropic", "web_search", 2_000, 10_000)).unwrap();
    // (started_at, tool): a turn before the price, one priced, one for a tool
    // with no price at all.
    for (started_at, tool) in [(1_000, "web_search"), (3_000, "web_search"), (3_000, "web_extractor")] {
        let mut start = turn_start(ctx.context_id);
        start.started_at = started_at;
        db.begin_turn(&start).unwrap();
        db.add_turn_usage(start.turn_id, RawUsage { input: 1_000_000, ..Default::default() }).unwrap();
        db.add_turn_tool_uses(start.turn_id, &[(tool, 3)]).unwrap();
    }
    let style = |p: &str| (p == "anthropic").then_some(InputStyle::ExcludesCache);
    // 3 turns * $1.00 tokens, plus 3 searches * $0.01 = $3.03; 3 model calls
    // and 3 searches are priced. The 3 searches
    // before the price and the 3 extractions have no price.
    assert_eq!(
        context_costs(&db, None, style).unwrap()[&ctx.context_id],
        ContextCost { micros: 3_030_000, priced_calls: 6, unpriced_calls: 6 }
    );
}

/// A tool price is keyed by provider: two providers that price the same
/// tool at different rates each price their own turns.
#[test]
fn tool_uses_are_priced_by_their_own_providers_row() {
    use crate::pricing::{context_costs, ContextCost};
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("tool-providers"));
    insert_context_with_doc(&db, &ctx, ws_id);
    let cheap = db.insert_tool_price(&new_tool_price("alibaba", "web_search", 0, 5_000)).unwrap();
    let dear = db.insert_tool_price(&new_tool_price("anthropic", "web_search", 0, 10_000)).unwrap();
    for provider in ["anthropic", "alibaba"] {
        let mut start = turn_start(ctx.context_id);
        start.provider = provider.into();
        start.started_at = 1_000;
        db.begin_turn(&start).unwrap();
        db.add_turn_tool_uses(start.turn_id, &[("web_search", 2)]).unwrap();
    }
    let groups = db.context_tool_use_groups(Some(ctx.context_id)).unwrap();
    let shape: Vec<(&str, Option<i64>, i64)> = groups.iter().map(|g| (g.provider.as_str(), g.price_id, g.calls)).collect();
    assert_eq!(shape, vec![("alibaba", Some(cheap), 2), ("anthropic", Some(dear), 2)]);
    // 2 * $0.005 + 2 * $0.01
    assert_eq!(
        context_costs(&db, None, |_| None).unwrap()[&ctx.context_id],
        ContextCost { micros: 30_000, priced_calls: 4, unpriced_calls: 0 }
    );
}

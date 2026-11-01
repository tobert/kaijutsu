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

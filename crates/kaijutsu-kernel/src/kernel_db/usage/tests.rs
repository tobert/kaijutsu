use super::*;

/// A fresh context has never completed an LLM call — absence must be
/// honest `None`, never a fabricated zero (a zero would be
/// indistinguishable from "measured and genuinely empty").
#[test]
fn context_usage_absent_by_default() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("usage-absent"));
    insert_context_with_doc(&db, &ctx, ws_id);
    assert!(db.get_context_usage(ctx.context_id).unwrap().is_none());
}

/// A turn's row keeps what it started with, and each provider call adds
/// its raw counts: the second call does not replace the first, unlike the
/// `context_usage` gauge.
#[test]
fn turn_usage_sums_every_call_and_finish_closes_the_row() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("turn-sum"));
    insert_context_with_doc(&db, &ctx, ws_id);
    let start = turn_start(ctx.context_id);
    db.begin_turn(&start).unwrap();
    db.add_turn_usage(start.turn_id, RawUsage { input: 10, output: 5, cache_read: 1000, cache_write: 200, cache_write_1h: 150, reasoning: 0 }).unwrap();
    db.add_turn_usage(start.turn_id, RawUsage { input: 20, output: 7, cache_read: 1200, cache_write: 0, cache_write_1h: 0, reasoning: 3 }).unwrap();
    let running = db.get_turn(start.turn_id).unwrap().unwrap();
    assert_eq!(running.start, start);
    assert_eq!((running.ended_at, running.stop_reason.clone()), (None, None));
    db.finish_turn(start.turn_id, 2_000, "end_turn").unwrap();

    let done = db.get_turn(start.turn_id).unwrap().unwrap();
    assert_eq!((done.ended_at, done.stop_reason.as_deref()), (Some(2_000), Some("end_turn")));
    assert_eq!(
        (done.llm_calls, done.input_tokens, done.output_tokens, done.cache_read_tokens,
         done.cache_write_tokens, done.cache_write_1h_tokens, done.reasoning_tokens),
        (2, 30, 12, 2200, 200, 150, 3),
    );
    assert_eq!(db.context_turn_totals(ctx.context_id).unwrap().cache_write_1h_tokens, 150);
}

/// A count or an end with nowhere to land is an error, not a no-op: the
/// turn has no row, or the turn already ended.
#[test]
fn turn_writes_fail_loudly_without_a_running_row() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("turn-loud"));
    insert_context_with_doc(&db, &ctx, ws_id);
    let missing = TurnId::new();
    assert!(matches!(db.add_turn_usage(missing, RawUsage::default()), Err(KernelDbError::NotFound(_))));
    assert!(matches!(db.finish_turn(missing, 1, "end_turn"), Err(KernelDbError::NotFound(_))));

    let start = turn_start(ctx.context_id);
    db.begin_turn(&start).unwrap();
    assert!(db.begin_turn(&start).is_err(), "a turn id starts once");
    db.finish_turn(start.turn_id, 2_000, "end_turn").unwrap();
    assert!(matches!(db.finish_turn(start.turn_id, 3_000, "failed"), Err(KernelDbError::NotFound(_))));
    assert!(matches!(db.add_turn_usage(start.turn_id, RawUsage::default()), Err(KernelDbError::NotFound(_))));
    assert_eq!(db.get_turn(start.turn_id).unwrap().unwrap().ended_at, Some(2_000));
}

#[test]
fn context_turn_totals_sum_the_context_s_turns_only() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ours = make_context_row(Some("turn-ours"));
    let theirs = make_context_row(Some("turn-theirs"));
    insert_context_with_doc(&db, &ours, ws_id);
    insert_context_with_doc(&db, &theirs, ws_id);
    assert_eq!(db.context_turn_totals(ours.context_id).unwrap(), TurnTotals::default());
    for (ctx, input) in [(ours.context_id, 10), (ours.context_id, 32), (theirs.context_id, 999)] {
        let mut start = turn_start(ctx);
        start.started_at += input as i64;
        db.begin_turn(&start).unwrap();
        db.add_turn_usage(start.turn_id, RawUsage { input, output: 1, ..Default::default() }).unwrap();
    }
    let totals = db.context_turn_totals(ours.context_id).unwrap();
    assert_eq!((totals.turns, totals.llm_calls, totals.input_tokens, totals.output_tokens), (2, 2, 42, 2));
    let listed = db.list_turns(ours.context_id, 10).unwrap();
    assert_eq!(listed.iter().map(|t| t.input_tokens).collect::<Vec<_>>(), vec![32, 10], "newest first");
    assert_eq!(db.list_turns(ours.context_id, 1).unwrap().len(), 1);
}

#[test]
fn context_usage_set_and_get_round_trips() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("usage-roundtrip"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let row = ContextUsageRow {
        context_id: ctx.context_id,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        input_tokens: 1234,
        output_tokens: 567,
        cache_read_tokens: 800,
        cache_write_tokens: 100,
        reasoning_tokens: 0,
        cache_ttl_secs: 3600,
        updated_at: now_millis(),
    };
    db.set_context_usage(&row).unwrap();

    let loaded = db.get_context_usage(ctx.context_id).unwrap().unwrap();
    assert_eq!(loaded, row);
}

/// The whole point of the accumulation model: the second `set_context_usage`
/// call must REPLACE the first, never add to it. This is what makes the
/// stored value "how full is my context right now" instead of a lifetime
/// spend counter — see the `context_usage` table doc comment in `SCHEMA`.
#[test]
fn context_usage_upsert_overwrites_not_sums() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("usage-overwrite"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.set_context_usage(&ContextUsageRow {
        context_id: ctx.context_id,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cache_ttl_secs: 0,
        updated_at: 1,
    })
    .unwrap();
    db.set_context_usage(&ContextUsageRow {
        context_id: ctx.context_id,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        input_tokens: 150,
        output_tokens: 30,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cache_ttl_secs: 0,
        updated_at: 2,
    })
    .unwrap();

    let loaded = db.get_context_usage(ctx.context_id).unwrap().unwrap();
    assert_eq!(
        (loaded.input_tokens, loaded.output_tokens),
        (150, 30),
        "second write must replace the first, not sum with it (100+150=250 would be wrong)"
    );
}

/// `list_all_context_usage` is the bulk counterpart to
/// `get_context_usage` — used by `listContexts` so the wire's
/// `contextWindow`/`contextUsedTokens`/`contextUsedPct` don't cost one
/// DB round-trip per context.
#[test]
fn list_all_context_usage_returns_every_row() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx_a = make_context_row(Some("usage-list-a"));
    let ctx_b = make_context_row(Some("usage-list-b"));
    insert_context_with_doc(&db, &ctx_a, ws_id);
    insert_context_with_doc(&db, &ctx_b, ws_id);

    // ctx_b never completes a call — must be absent from the bulk list,
    // same honesty as get_context_usage's None.
    db.set_context_usage(&ContextUsageRow {
        context_id: ctx_a.context_id,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cache_ttl_secs: 0,
        updated_at: 1,
    })
    .unwrap();

    let rows = db.list_all_context_usage().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].context_id, ctx_a.context_id);
}

/// The one percentage-math site: `kj context info --json` and the
/// wire's `contextUsedPct` both resolve through `context_used_pct`, so
/// this pins down the definition itself (used / window * 100, no
/// clamping) and the honest-unknown short-circuit.
#[test]
fn context_used_pct_computes_from_last_call_fill() {
    let usage = ContextUsageRow {
        context_id: ContextId::new(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        input_tokens: 40_000,
        output_tokens: 10_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cache_ttl_secs: 0,
        updated_at: 1,
    };
    assert_eq!(context_used_pct(&usage, Some(200_000)), Some(25.0));
}

#[test]
fn context_used_pct_unknown_window_is_none_not_zero() {
    let usage = ContextUsageRow {
        context_id: ContextId::new(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-20250514".into(),
        input_tokens: 40_000,
        output_tokens: 10_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cache_ttl_secs: 0,
        updated_at: 1,
    };
    assert_eq!(context_used_pct(&usage, None), None);
}

/// A genuinely fresh usage row (0 tokens either side, e.g. right after
/// the first empty turn) against a known window must compute a real
/// `Some(0.0)` — this is the exact case the wire's `-1.0` sentinel
/// exists to keep distinct from "window unknown".
#[test]
fn context_used_pct_zero_usage_is_some_zero_not_none() {
    let usage = ContextUsageRow {
        context_id: ContextId::new(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cache_ttl_secs: 0,
        updated_at: 1,
    };
    assert_eq!(context_used_pct(&usage, Some(200_000)), Some(0.0));
}

/// A zero window is a config error, and dividing by it yields `NaN`
/// (with zero usage) or `+Inf` (with any usage). Neither is caught
/// downstream: the wire carries the percentage as `Float32` and the
/// client's unknown test is `pct < 0.0`, which is FALSE for both — so
/// garbage would reach the dock badge looking like a real number.
/// Treat it as unknown instead.
#[test]
fn context_used_pct_zero_window_is_none_not_nan_or_inf() {
    let mut usage = ContextUsageRow {
        context_id: ContextId::new(),
        provider: "anthropic".into(),
        model: "misconfigured".into(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cache_ttl_secs: 0,
        updated_at: 1,
    };
    // 0/0 would be NaN — and NaN slips past every `< 0.0` sentinel test.
    assert_eq!(context_used_pct(&usage, Some(0)), None);

    // n/0 would be +Inf — same story.
    usage.input_tokens = 1_000;
    assert_eq!(context_used_pct(&usage, Some(0)), None);
}

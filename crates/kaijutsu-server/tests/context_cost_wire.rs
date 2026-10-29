//! A context's cost over the wire: turns recorded in the kernel database are
//! priced by the price in force when each started, and `listContexts` carries
//! the result to the client. A turn with no price is counted apart, never
//! shown as free.

mod common;
use common::*;

use kaijutsu_kernel::kernel_db::{NewModelPrice, TurnStart};
use kaijutsu_kernel::RawUsage;
use kaijutsu_types::{PrincipalId, TurnId};

fn record_turn(
    kernel: &kaijutsu_server::SharedKernel,
    context: kaijutsu_types::ContextId,
    started_at: i64,
    input: u64,
) {
    let db = kernel.kernel_db.lock();
    let turn_id = TurnId::new();
    db.begin_turn(&TurnStart {
        turn_id,
        context_id: context,
        performer: PrincipalId::new(),
        role: "default".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        started_at,
    })
    .unwrap();
    db.add_turn_usage(turn_id, RawUsage { input, output: 0, ..Default::default() }).unwrap();
}

#[test]
fn cost_reaches_the_client_priced_by_the_price_in_force() {
    run_local(async {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "cost-wire-test").await.unwrap();
        let cost_of = |contexts: Vec<kaijutsu_client::ContextInfo>| {
            contexts.into_iter().find(|c| c.id == context).expect("the context is listed").cost
        };

        // No call recorded: no cost, not a zero cost.
        assert_eq!(cost_of(kernel.list_contexts().await.unwrap()), None);

        // One call with no price: unknown, counted apart.
        record_turn(&server, context, 1_000, 1_000_000);
        assert_eq!(
            cost_of(kernel.list_contexts().await.unwrap()),
            Some(kaijutsu_client::ContextCost { micros: 0, priced_calls: 0, unpriced_calls: 1 })
        );

        // A price from before the turn: $2 per million input tokens.
        server.kernel_db.lock().insert_model_price(&NewModelPrice {
            provider: "mock".into(),
            model: "mock-model".into(),
            effective_from: 500,
            input: 2_000_000,
            output: 10_000_000,
            cache_read: Some(0),
            cache_write_5m: Some(0),
            cache_write_1h: Some(0),
            source: "wire test".into(),
            set_by: None,
        })
        .unwrap();
        record_turn(&server, context, 2_000, 500_000);
        assert_eq!(
            cost_of(kernel.list_contexts().await.unwrap()),
            Some(kaijutsu_client::ContextCost { micros: 3_000_000, priced_calls: 2, unpriced_calls: 0 }),
            "1M + 0.5M input tokens at $2 per million"
        );
    });
}

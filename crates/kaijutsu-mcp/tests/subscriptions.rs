//! Integration tests for MCP `2026-07-28` resource templates, slimmed
//! `list_resources`, coalesced `subscriptions/listen`, and `/events`, `/turns`,
//! and `/drift` resource reads.

use std::time::Duration;

use kaijutsu_mcp::subscriptions::{DriftRecord, SubscriptionHub};
use kaijutsu_mcp::KaijutsuMcp;
use kaijutsu_types::{BlockKind, ContentType, ContextId, DocKind, PrincipalId, Role, Status};
use rmcp::model::{
    ClientConfig, ProtocolVersion, ReadResourceRequestParams, ResourceContents,
    ServerNotification, SubscriptionFilter,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt};
use rmcp::ServiceExt;

fn discover_2026() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    }
}

fn read_text_payload(result: rmcp::model::ReadResourceResult) -> serde_json::Value {
    let first = result.contents.into_iter().next().expect("resource content");
    let text = match first {
        ResourceContents::TextResourceContents { text, .. } => text,
        other => panic!("expected text resource contents, got {other:?}"),
    };
    serde_json::from_str(&text).expect("valid JSON resource payload")
}

#[tokio::test]
async fn resource_templates_and_slim_list_resources() {
    let principal = PrincipalId::new();
    let store = kaijutsu_kernel::shared_block_store(principal);
    let ctx_id = ContextId::new();
    store
        .create_document(ctx_id, DocKind::Conversation, None)
        .unwrap();
    store
        .insert_block_as(
            ctx_id,
            None,
            None,
            Role::User,
            BlockKind::Text,
            "hello block",
            Status::Done,
            ContentType::Plain,
            Some(principal),
        )
        .unwrap();

    let server = KaijutsuMcp::with_store(store);
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });

    let client = ClientConfig::default()
        .serve_with_lifecycle(client_io, discover_2026())
        .await
        .unwrap();

    let templates = client
        .list_all_resource_templates()
        .await
        .expect("list_resource_templates");
    let uri_templates: Vec<&str> = templates.iter().map(|t| t.uri_template.as_str()).collect();
    for expected in [
        "kaijutsu://docs/{id}",
        "kaijutsu://blocks/{id}/{key}",
        "kaijutsu://contexts/{id}/drift",
        "kaijutsu://contexts/{id}/turns",
        "kaijutsu://contexts/{id}/events",
    ] {
        assert!(
            uri_templates.contains(&expected),
            "missing template {expected} in {uri_templates:?}"
        );
    }

    let resources = client.list_all_resources().await.expect("list_resources");
    let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
    assert!(uris.contains(&"kaijutsu://docs"), "missing kaijutsu://docs in {uris:?}");
    let doc_uri = format!("kaijutsu://docs/{}", ctx_id.to_hex());
    assert!(uris.contains(&doc_uri.as_str()), "missing {doc_uri} in {uris:?}");
    assert!(
        !uris.iter().any(|u| u.starts_with("kaijutsu://blocks/")),
        "list_resources must not enumerate individual blocks: {uris:?}"
    );

    client.cancel().await.unwrap();
    server_task.await.unwrap();
}

#[tokio::test]
async fn subscriptions_listen_coalesces_and_filters_events_and_turns() {
    let hub = SubscriptionHub::with_coalesce_window(Duration::from_millis(25));
    let server = KaijutsuMcp::new().with_subscription_hub(hub.clone());

    let (server_io, client_io) = tokio::io::duplex(8192);
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });

    let client = ClientConfig::default()
        .serve_with_lifecycle(client_io, discover_2026())
        .await
        .unwrap();

    let ctx_id = ContextId::new();
    let other_ctx = ContextId::new();
    let events_uri = format!("kaijutsu://contexts/{}/events", ctx_id.to_hex());
    let turns_uri = format!("kaijutsu://contexts/{}/turns", ctx_id.to_hex());

    let filter = SubscriptionFilter::builder()
        .resource_subscription(events_uri.clone())
        .resource_subscription(turns_uri.clone())
        .build();
    let mut sub = client.peer().listen(filter).await.expect("listen");

    // Push a burst of 10 events for subscribed ctx and 3 for unsubscribed ctx.
    for i in 0..10 {
        hub.record_event(
            ctx_id,
            "block_inserted",
            Some(format!("blk-{i}")),
            Some(serde_json::json!({ "index": i })),
        );
    }
    for i in 0..3 {
        hub.record_event(other_ctx, "block_inserted", Some(format!("other-{i}")), None);
    }

    let first = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("timed out waiting for coalesced events notification")
        .expect("subscription error")
        .expect("expected notification");
    match first {
        ServerNotification::ResourceUpdatedNotification(n) => {
            assert_eq!(n.params.uri, events_uri);
        }
        other => panic!("expected ResourceUpdatedNotification, got {other:?}"),
    }

    // No second notification should arrive for the same burst or for other_ctx.
    let extra = tokio::time::timeout(Duration::from_millis(80), sub.next()).await;
    assert!(
        extra.is_err(),
        "expected burst to coalesce into a single notification, got {extra:?}"
    );

    // Read /events and verify all 10 records are present in order.
    let events_res = client
        .read_resource(ReadResourceRequestParams::new(events_uri.clone()))
        .await
        .expect("read /events");
    let events_json = read_text_payload(events_res);
    assert_eq!(events_json["context_id"], ctx_id.to_hex());
    let events = events_json["events"].as_array().expect("events array");
    assert_eq!(events.len(), 10);
    assert_eq!(events[0]["block_id"], "blk-0");
    assert_eq!(events[9]["block_id"], "blk-9");

    // Push a burst of turn events for ctx_id.
    let principal = PrincipalId::new().to_hex();
    hub.record_turn_event(
        ctx_id,
        "turn_started",
        "turn-1",
        &principal,
        None,
        None,
        None,
    );
    hub.record_turn_event(
        ctx_id,
        "turn_completed",
        "turn-1",
        &principal,
        Some("end_turn".to_string()),
        Some("blk-out".to_string()),
        None,
    );

    let mut received_uris = Vec::new();
    for _ in 0..2 {
        let notif = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("timed out waiting for turn/event notification")
            .expect("subscription error")
            .expect("expected notification");
        if let ServerNotification::ResourceUpdatedNotification(n) = notif {
            received_uris.push(n.params.uri);
        }
    }
    assert!(received_uris.contains(&turns_uri), "{received_uris:?}");
    assert!(received_uris.contains(&events_uri), "{received_uris:?}");

    let turns_res = client
        .read_resource(ReadResourceRequestParams::new(turns_uri.clone()))
        .await
        .expect("read /turns");
    let turns_json = read_text_payload(turns_res);
    assert_eq!(turns_json["context_id"], ctx_id.to_hex());
    let turns = turns_json["events"].as_array().expect("turns array");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["kind"], "turn_started");
    assert_eq!(turns[1]["kind"], "turn_completed");
    assert_eq!(turns[1]["stop_reason"], "end_turn");

    sub.cancel().await.unwrap();
    client.cancel().await.unwrap();
    server_task.await.unwrap();
}

#[tokio::test]
async fn drift_subscriber_tracking_and_local_drift_delivery() {
    let hub = SubscriptionHub::with_coalesce_window(Duration::from_millis(20));
    let server = KaijutsuMcp::new().with_subscription_hub(hub.clone());

    let (server_io, client_io) = tokio::io::duplex(8192);
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });

    let client = ClientConfig::default()
        .serve_with_lifecycle(client_io, discover_2026())
        .await
        .unwrap();

    let ctx_id = ContextId::new();
    let drift_uri = format!("kaijutsu://contexts/{}/drift", ctx_id.to_hex());

    assert!(
        !hub.has_drift_subscriber(ctx_id),
        "no drift subscriber before listen"
    );

    let filter = SubscriptionFilter::builder()
        .resource_subscription(drift_uri.clone())
        .build();
    let mut sub = client.peer().listen(filter).await.expect("listen drift");

    // Wait briefly for listen() handler to register the subscriber guard.
    tokio::time::timeout(Duration::from_secs(1), async {
        while !hub.has_drift_subscriber(ctx_id) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("has_drift_subscriber should become true while listening");

    // Push a local drift and verify notification + read_resource drains it.
    let src_ctx = ContextId::new();
    hub.push_local_drift(
        ctx_id,
        DriftRecord {
            id: 42,
            source_ctx: src_ctx.to_hex(),
            target_ctx: ctx_id.to_hex(),
            content: "heads up from sibling".to_string(),
            source_model: "test-model".to_string(),
            drift_kind: "push".to_string(),
            created_at: 12345,
        },
    );

    let notif = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("timed out waiting for drift notification")
        .expect("subscription error")
        .expect("expected notification");
    match notif {
        ServerNotification::ResourceUpdatedNotification(n) => {
            assert_eq!(n.params.uri, drift_uri);
        }
        other => panic!("expected ResourceUpdatedNotification, got {other:?}"),
    }

    let drift_res = client
        .read_resource(ReadResourceRequestParams::new(drift_uri.clone()))
        .await
        .expect("read /drift");
    let drift_json = read_text_payload(drift_res);
    assert_eq!(drift_json["context_id"], ctx_id.to_hex());
    let drifts = drift_json["drifts"].as_array().expect("drifts array");
    assert_eq!(drifts.len(), 1);
    assert_eq!(drifts[0]["id"], 42);
    assert_eq!(drifts[0]["content"], "heads up from sibling");

    // Reading again after drain returns empty drifts.
    let empty_res = client
        .read_resource(ReadResourceRequestParams::new(drift_uri.clone()))
        .await
        .expect("read /drift second time");
    let empty_json = read_text_payload(empty_res);
    assert_eq!(empty_json["drifts"].as_array().unwrap().len(), 0);

    // Cancel subscription and verify has_drift_subscriber flips back to false.
    sub.cancel().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while hub.has_drift_subscriber(ctx_id) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("has_drift_subscriber should return to false after cancel");

    client.cancel().await.unwrap();
    server_task.await.unwrap();
}

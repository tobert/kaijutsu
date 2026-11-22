//! End-to-end tests for `kaijutsu-mcp` service mode over Unix domain sockets:
//! Streamable HTTP MCP (`/mcp`), `subscriptions/listen`, `GET /health`, and
//! unified `POST /hook`.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kaijutsu_mcp::hook_listener::HookListener;
use kaijutsu_mcp::hook_types::{HookEvent, HookResponse, PingResponse, ToolInfo};
use kaijutsu_mcp::service::{send_hook_http_uds, serve, HookRouter, ServiceConfig};
use kaijutsu_mcp::subscriptions::SubscriptionHub;
use kaijutsu_mcp::KaijutsuMcp;
use kaijutsu_types::{BlockKind, ContextId, DocKind, PrincipalId};
use rmcp::model::{
    ClientConfig, ProtocolVersion, ReadResourceRequestParams, ResourceContents,
    ServerNotification, SubscriptionFilter,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt};
use rmcp::transport::StreamableHttpClientTransport;
use tokio_util::sync::CancellationToken;

fn unique_temp_socket(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "kj-mcp-svc-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("mcp.sock")
}

fn discover_2026() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    }
}

#[tokio::test]
async fn uds_service_serves_mcp_subscriptions_and_post_hook() {
    let principal = PrincipalId::new();
    let store = kaijutsu_kernel::shared_block_store(principal);
    let ctx_id = ContextId::new();
    store
        .create_document(ctx_id, DocKind::Conversation, None)
        .unwrap();

    let hub = SubscriptionHub::with_coalesce_window(Duration::from_millis(20));
    let mcp = KaijutsuMcp::with_store(store.clone()).with_subscription_hub(hub.clone());
    let listener = Arc::new(
        HookListener::local(store.clone(), ctx_id)
            .with_subscriptions(hub.clone())
            .with_multi_session(true),
    );
    let hook_router = Arc::new(HookRouter::new(listener));

    let socket_path = unique_temp_socket("e2e");
    let cancel = CancellationToken::new();
    let config = ServiceConfig {
        listen_unix: Some(socket_path.clone()),
        listen_http: None,
        cancellation_token: cancel.clone(),
    };

    let server_task = tokio::spawn(serve(mcp, hook_router, config));

    // Wait for the Unix socket to be bound.
    tokio::time::timeout(Duration::from_secs(2), async {
        while !socket_path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("service Unix socket never appeared");

    // 1. Connect over UDS using rmcp StreamableHttpClientTransport (2026-07-28).
    let transport = StreamableHttpClientTransport::from_unix_socket(
        socket_path.to_str().unwrap(),
        "http://localhost/mcp",
    );
    let client = ClientConfig::default()
        .serve_with_lifecycle(transport, discover_2026())
        .await
        .expect("UDS MCP handshake");

    let tools = client.list_all_tools().await.expect("list_tools");
    assert!(
        tools.iter().any(|t| t.name == "whoami"),
        "expected whoami tool in {:?}",
        tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>()
    );

    let templates = client
        .list_all_resource_templates()
        .await
        .expect("list_resource_templates");
    assert!(
        templates
            .iter()
            .any(|t| t.uri_template == "kaijutsu://contexts/{id}/events"),
        "expected events resource template"
    );

    // 2. Subscribe via subscriptions/listen and receive coalesced update.
    let events_uri = format!("kaijutsu://contexts/{}/events", ctx_id.to_hex());
    let filter = SubscriptionFilter::builder()
        .resource_subscription(events_uri.clone())
        .build();
    let mut sub = client.peer().listen(filter).await.expect("listen");

    for i in 0..5 {
        hub.record_event(ctx_id, "block_inserted", Some(format!("uds-{i}")), None);
    }

    let notif = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("timed out waiting for UDS notification")
        .expect("subscription stream error")
        .expect("expected notification");
    match notif {
        ServerNotification::ResourceUpdatedNotification(n) => {
            assert_eq!(n.params.uri, events_uri);
        }
        other => panic!("expected ResourceUpdatedNotification, got {other:?}"),
    }

    let events_res = client
        .read_resource(ReadResourceRequestParams::new(events_uri))
        .await
        .expect("read /events");
    let first = events_res.contents.into_iter().next().expect("content");
    let text = match first {
        ResourceContents::TextResourceContents { text, .. } => text,
        other => panic!("expected text content, got {other:?}"),
    };
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(parsed["events"].as_array().unwrap().len(), 5);

    // 3. Send POST /hook (ping and tool.after) over the same Unix socket.
    let ping_reply = send_hook_http_uds(&socket_path, r#"{"ping":true}"#)
        .await
        .expect("ping HTTP request")
        .expect("ping response body");
    let ping: PingResponse = serde_json::from_str(&ping_reply).expect("parse PingResponse");
    assert_eq!(ping.status, "ok");

    let hook_event = HookEvent {
        event: "tool.after".to_string(),
        source: "antigravity".to_string(),
        session_id: Some("conv-uds-1".to_string()),
        timestamp: None,
        cwd: Some("/tmp/demo".to_string()),
        model: None,
        transcript_path: None,
        tool: Some(ToolInfo {
            name: "run_command".to_string(),
            input: serde_json::json!({"CommandLine": "ls"}),
            output: Some("ok".to_string()),
            error: None,
            duration_ms: Some(5),
        }),
        file: None,
        prompt: None,
        response: None,
        reason: None,
        principal_id: None,
        agent_type: None,
        trigger: None,
    };
    let hook_json = serde_json::to_string(&hook_event).unwrap();
    let hook_reply = send_hook_http_uds(&socket_path, &hook_json)
        .await
        .expect("POST /hook request")
        .expect("POST /hook response body");
    let hook_resp: HookResponse = serde_json::from_str(&hook_reply).expect("parse HookResponse");
    assert_eq!(hook_resp.block, "allow");

    // Verify the hook authored the tool call block in the shared store.
    let snapshots = store.block_snapshots(ctx_id).unwrap();
    assert!(
        snapshots.iter().any(|b| b.kind == BlockKind::ToolCall),
        "POST /hook should have authored a ToolCall block"
    );

    // 4. Graceful shutdown cleans up the Unix socket file.
    sub.cancel().await.unwrap();
    client.cancel().await.unwrap();
    cancel.cancel();
    server_task.await.unwrap().expect("clean service shutdown");
    assert!(
        !socket_path.exists(),
        "Unix socket should be unlinked on graceful shutdown"
    );
}

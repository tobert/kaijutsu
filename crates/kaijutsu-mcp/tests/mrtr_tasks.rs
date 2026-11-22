//! Integration tests for server-side MCP MRTR elicitation, Tasks, and
//! `notifications/cancelled` tracking on `KaijutsuMcp`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use kaijutsu_client::{KeySource, SshConfig};
use kaijutsu_mcp::KaijutsuMcp;
use kaijutsu_server::{SshServer, SshServerConfig};
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CancelTaskParams, CancelledNotificationParam,
    ClientCapabilities, ClientConfig, ContentBlock, ElicitRequestParams, ElicitResult,
    ElicitationAction, ErrorCode, GetTaskParams, Implementation, ProtocolVersion, RequestId,
    TaskPayload, TaskStatus,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RequestContext, RoleClient, ServiceError};
use rmcp::{ErrorData as McpError, ServiceExt};
use tokio::net::TcpListener;
use tokio::task::LocalSet;
use tokio_util::sync::CancellationToken;

/// capnp-rpc requires a current-thread runtime with a LocalSet.
fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

static ROOT_KEYS: OnceLock<Mutex<HashMap<SocketAddr, KeySource>>> = OnceLock::new();

fn root_keys() -> &'static Mutex<HashMap<SocketAddr, KeySource>> {
    ROOT_KEYS.get_or_init(|| Mutex::new(HashMap::new()))
}

const MCP_CHARACTER: &str = "mcp-session";

async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = SshServerConfig::ephemeral(addr.port());
    kaijutsu_kernel::kernel_db::KernelDb::open(
        config
            .data_dir
            .as_ref()
            .expect("an ephemeral config has a data dir")
            .join("kernel.db"),
    )
    .expect("open the ephemeral kernel.db")
    .insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
        rc_dir: Some("mcp".into()),
        console_ctx: None,
        mcp_hub_ctx: None,
        principal_id: kaijutsu_types::PrincipalId::new(),
        name: MCP_CHARACTER.to_string(),
        created_at: 0,
        retired_at: None,
        handoff_ctx: None,
        root_ctx: None,
        root: false,
    })
    .expect("seed the mcp session character");
    root_keys()
        .lock()
        .unwrap()
        .insert(addr, KeySource::InMemory(config.root_key()));

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    addr
}

async fn connect_mcp(addr: SocketAddr) -> KaijutsuMcp {
    let key_source = root_keys()
        .lock()
        .unwrap()
        .get(&addr)
        .cloned()
        .expect("connect_mcp requires a server started through start_server");
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source,
        insecure: true,
    };
    // Pass `None` for session_id so `register_session` with no label triggers elicitation.
    KaijutsuMcp::connect_with_config(config, "mrtr-test", None)
        .with_character(Some(MCP_CHARACTER.to_string()))
}

fn discover_2026() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    }
}

fn extract_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Test client handler that accepts form elicitation with a fixed label.
#[derive(Clone)]
struct ElicitingClient {
    config: ClientConfig,
    label: String,
    elicited_count: Arc<Mutex<usize>>,
}

impl ElicitingClient {
    fn new(protocol_version: ProtocolVersion, label: impl Into<String>) -> Self {
        let caps = ClientCapabilities::builder().enable_elicitation().build();
        let config = ClientConfig::new(caps, Implementation::new("mrtr-test-client", "0.1.0"))
            .with_protocol_version(protocol_version);
        Self {
            config,
            label: label.into(),
            elicited_count: Arc::new(Mutex::new(0)),
        }
    }
}

impl ClientHandler for ElicitingClient {
    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, McpError> {
        match request {
            ElicitRequestParams::FormElicitationParams { .. } => {
                *self.elicited_count.lock().unwrap() += 1;
                Ok(ElicitResult::new(ElicitationAction::Accept)
                    .with_content(serde_json::json!({ "label": self.label })))
            }
            _ => Ok(ElicitResult::new(ElicitationAction::Decline)),
        }
    }
}

#[test]
fn mrtr_elicitation_round_trip_and_tampered_state_rejection() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;

        // Wait briefly for the SSH actor to connect so register_session succeeds on first try.
        if let kaijutsu_mcp::Backend::Remote(remote) = mcp.backend() {
            let mut status = remote.actor.watch_status();
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                status.wait_for(|s| matches!(s, kaijutsu_client::ConnectionStatus::Connected { .. })),
            )
            .await;
            // Trigger actor dial if still idle by calling list_contexts once.
            for _ in 0..50 {
                if remote.actor.list_contexts().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        let (server_io, client_io) = tokio::io::duplex(16384);
        let server_task = tokio::task::spawn_local(async move {
            let running = mcp.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });

        let handler = ElicitingClient::new(ProtocolVersion::V_2026_07_28, "elicited-session");
        let elicited_count = handler.elicited_count.clone();
        let client = handler
            .serve_with_lifecycle(client_io, discover_2026())
            .await
            .unwrap();

        // 1. Tampered requestState must be rejected with INVALID_PARAMS.
        let tampered_params = CallToolRequestParams::new("register_session")
            .with_arguments(serde_json::Map::new())
            .with_request_state("rs1.tampered.invalidtag");
        let err = client
            .peer()
            .call_tool_once(tampered_params)
            .await
            .expect_err("tampered requestState must be rejected");
        match err {
            ServiceError::McpError(mcp_err) => {
                assert_eq!(
                    mcp_err.code,
                    ErrorCode::INVALID_PARAMS,
                    "expected INVALID_PARAMS for tampered requestState, got {mcp_err:?}"
                );
            }
            other => panic!("expected ServiceError::McpError, got {other:?}"),
        }

        // 2. High-level call_tool automatically drives the MRTR elicitation round-trip.
        let result = client
            .call_tool(
                CallToolRequestParams::new("register_session")
                    .with_arguments(serde_json::Map::new()),
            )
            .await
            .expect("MRTR register_session should succeed");

        assert_eq!(
            *elicited_count.lock().unwrap(),
            1,
            "client elicitation handler should be invoked once"
        );
        let text = extract_text(&result);
        let parsed: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("invalid json `{text}`: {e}"));
        assert_eq!(parsed["success"], true, "register_session reply: {parsed}");
        assert_eq!(parsed["label"], "elicited-session");

        client.cancel().await.unwrap();
        server_task.await.unwrap();
    });
}

#[test]
fn legacy_2025_11_25_inline_elicitation_round_trip() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;

        if let kaijutsu_mcp::Backend::Remote(remote) = mcp.backend() {
            for _ in 0..50 {
                if remote.actor.list_contexts().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        let (server_io, client_io) = tokio::io::duplex(16384);
        let server_task = tokio::task::spawn_local(async move {
            let running = mcp.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });

        let handler = ElicitingClient::new(ProtocolVersion::V_2025_11_25, "legacy-elicited");
        let elicited_count = handler.elicited_count.clone();
        let client = handler
            .serve_with_lifecycle(client_io, ClientLifecycleMode::Initialize)
            .await
            .unwrap();

        let result = client
            .call_tool(
                CallToolRequestParams::new("register_session")
                    .with_arguments(serde_json::Map::new()),
            )
            .await
            .expect("legacy inline elicitation register_session should succeed");

        assert_eq!(*elicited_count.lock().unwrap(), 1);
        let text = extract_text(&result);
        let parsed: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("invalid json `{text}`: {e}"));
        assert_eq!(parsed["success"], true, "register_session reply: {parsed}");
        assert_eq!(parsed["label"], "legacy-elicited");

        client.cancel().await.unwrap();
        server_task.await.unwrap();
    });
}

#[test]
fn sep2663_shell_task_round_trip_and_non_task_client_sync() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;

        // Pre-register a session so `shell` can execute commands.
        for _ in 0..100 {
            let reply = mcp
                .register_session_auto(Some("tasks-test-session".to_string()))
                .await;
            if serde_json::from_str::<serde_json::Value>(&reply)
                .is_ok_and(|v| v["success"] == true)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // --- Client 1: declares tasks capability ---
        let mcp_for_tasks = mcp.clone();
        let (server_io, client_io) = tokio::io::duplex(16384);
        let server_task = tokio::task::spawn_local(async move {
            let running = mcp_for_tasks.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });

        let task_caps = ClientCapabilities::builder().enable_tasks().build();
        let client = ClientConfig::new(task_caps, Implementation::new("tasks-test-client", "0.1.0"))
            .serve_with_lifecycle(client_io, discover_2026())
            .await
            .unwrap();

        let mut args = serde_json::Map::new();
        args.insert("command".to_string(), serde_json::json!("echo hello-task"));
        let response = client
            .peer()
            .call_tool_once(CallToolRequestParams::new("shell").with_arguments(args))
            .await
            .expect("call_tool_once for shell");

        let task_id = match response {
            CallToolResponse::Task(create_task) => {
                assert_eq!(create_task.task.status, TaskStatus::Working);
                create_task.task.task_id
            }
            other => panic!("expected CallToolResponse::Task, got {other:?}"),
        };

        // Poll tasks/get until Completed.
        let mut completed_result = None;
        for _ in 0..100 {
            let got = client
                .peer()
                .get_task(GetTaskParams::new(task_id.clone()))
                .await
                .expect("tasks/get");
            if got.task.status().is_terminal() {
                match got.task.payload {
                    TaskPayload::Completed { result } => {
                        completed_result = Some(result);
                        break;
                    }
                    other => panic!("expected Completed payload, got {other:?}"),
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let completed = completed_result.expect("task should reach Completed");
        let stdout = completed
            .get("structuredContent")
            .and_then(|s| s.get("stdout"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        assert!(
            stdout.contains("hello-task"),
            "expected stdout to contain 'hello-task', got {completed:?}"
        );

        // Cooperative cancel on a long-running sleep task.
        let mut sleep_args = serde_json::Map::new();
        sleep_args.insert("command".to_string(), serde_json::json!("sleep 30"));
        let sleep_resp = client
            .peer()
            .call_tool_once(CallToolRequestParams::new("shell").with_arguments(sleep_args))
            .await
            .expect("call_tool_once for sleep");
        let sleep_task_id = match sleep_resp {
            CallToolResponse::Task(ct) => ct.task.task_id,
            other => panic!("expected Task for sleep, got {other:?}"),
        };
        client
            .peer()
            .cancel_task(CancelTaskParams::new(sleep_task_id.clone()))
            .await
            .expect("tasks/cancel");

        let mut cancelled = false;
        for _ in 0..100 {
            let got = client
                .peer()
                .get_task(GetTaskParams::new(sleep_task_id.clone()))
                .await
                .expect("tasks/get after cancel");
            if got.task.status().is_terminal() {
                assert_eq!(got.task.status(), TaskStatus::Cancelled);
                cancelled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(cancelled, "sleep task should settle to Cancelled");

        client.cancel().await.unwrap();
        server_task.await.unwrap();

        // --- Client 2: does NOT declare tasks capability ---
        let (server_io2, client_io2) = tokio::io::duplex(16384);
        let server_task2 = tokio::task::spawn_local(async move {
            let running = mcp.serve(server_io2).await.unwrap();
            let _ = running.waiting().await;
        });

        let sync_client = ClientConfig::default()
            .serve_with_lifecycle(client_io2, discover_2026())
            .await
            .unwrap();

        let mut sync_args = serde_json::Map::new();
        sync_args.insert("command".to_string(), serde_json::json!("echo sync-shell"));
        let sync_resp = sync_client
            .peer()
            .call_tool_once(CallToolRequestParams::new("shell").with_arguments(sync_args))
            .await
            .expect("sync call_tool_once for shell");
        match sync_resp {
            CallToolResponse::Complete(res) => {
                let stdout = res
                    .structured_content
                    .as_ref()
                    .and_then(|s| s.get("stdout"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                assert!(stdout.contains("sync-shell"), "got {res:?}");
            }
            other => panic!("expected CallToolResponse::Complete for non-task client, got {other:?}"),
        }

        sync_client.cancel().await.unwrap();
        server_task2.await.unwrap();
    });
}

#[tokio::test]
async fn on_cancelled_cancels_matching_inflight_token() {
    let server = KaijutsuMcp::new();
    let req_id = RequestId::Number(4242);
    let token = CancellationToken::new();
    server.track_inflight_token(req_id.clone(), token.clone());
    assert!(!token.is_cancelled());

    let (server_io, client_io) = tokio::io::duplex(8192);
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });

    let client = ClientConfig::default()
        .serve_with_lifecycle(client_io, discover_2026())
        .await
        .unwrap();

    // Complete inline bootstrap with a first request before sending notifications.
    let _ = client.list_tools(None).await.expect("list_tools bootstrap");

    client
        .peer()
        .notify_cancelled(CancelledNotificationParam::new(
            Some(req_id),
            Some("test cancellation".to_string()),
        ))
        .await
        .expect("send notifications/cancelled");

    tokio::time::timeout(Duration::from_secs(2), token.cancelled())
        .await
        .expect("inflight token should be cancelled by notifications/cancelled");

    client.cancel().await.unwrap();
    server_task.await.unwrap();
}

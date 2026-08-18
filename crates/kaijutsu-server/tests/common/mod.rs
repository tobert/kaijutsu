//! Shared helpers for kaijutsu-server integration tests.
//!
//! Provides `run_local`, `start_server`, `connect_client`, and utilities
//! for exercising the full SSH + Cap'n Proto stack.

use std::net::SocketAddr;

use tokio::task::LocalSet;

use kaijutsu_client::{KeySource, RpcClient, SshConfig};
use kaijutsu_server::{SshServer, SshServerConfig};

/// Run async test code on a single-threaded runtime with LocalSet (capnp-rpc requirement).
pub fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// Start an SSH server on an ephemeral port and return the address.
///
/// The listener is pre-bound so connections queue during kernel initialization.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn start_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = SshServerConfig::ephemeral(addr.port());

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    addr
}

/// Seed a `mock`-kind backend into the kernel DB at `data_dir` and make it the
/// default, so `initialize_kernel_models()` brings up a registry a test can
/// drive without a live provider.
///
/// Model configuration is SQL-native now (`models.toml` is demolished), so the
/// injection point is the DB itself rather than a host file the kernel reads
/// once at seed time. The server opens the same `kernel.db` a moment later and
/// `ensure_factory_backends` — an absent-only floor — leaves the `llm_defaults`
/// row we write here alone.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub fn seed_mock_backend(data_dir: &std::path::Path) {
    seed_mock_backend_with_model(data_dir, "mock-model");
}

/// Like `seed_mock_backend`, but with a caller-chosen `default_model`.
///
/// Needed to exercise rc scripts (like the coder stance) that branch on the
/// bound model: setting the REGISTRY DEFAULT here (not a per-context
/// override) is what lets a test prove a context inherited its model rather
/// than had one stamped on its row — the distinction the S00-stance.kai
/// `.resolved_model` fix depends on.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub fn seed_mock_backend_with_model(data_dir: &std::path::Path, default_model: &str) {
    use kaijutsu_kernel::kernel_db::{BackendRow, KernelDb, LlmDefaultsRow};
    use kaijutsu_types::{BackendId, PrincipalId};

    std::fs::create_dir_all(data_dir).expect("create data dir");
    let db = KernelDb::open(data_dir.join("kernel.db")).expect("open kernel db for mock seed");
    db.upsert_backend(&BackendRow {
        backend_id: BackendId::new(),
        name: "mock".to_string(),
        kind: "mock".to_string(),
        base_url: None,
        api_key_env: None,
        api_key_file: None,
        key_optional: true,
        request_timeout_secs: None,
        created_at: 0,
        created_by: PrincipalId::system(),
    })
    .expect("seed mock backend");
    db.set_llm_defaults(&LlmDefaultsRow {
        default_backend: "mock".to_string(),
        default_model: default_model.to_string(),
        max_tokens: Some(16384),
        temperature: None,
        top_p: None,
        effort: None,
        thinking_budget: None,
        thinking_style: None,
    })
    .expect("point defaults at the mock backend");
}

/// Start a server whose ephemeral kernel DB carries a mock LLM backend.
///
/// This makes `initialize_kernel_models()` register a "mock" backend so that
/// `KjDispatcher.summarize()` and other LLM-dependent paths work in tests.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn start_server_with_mock_llm() -> SocketAddr {
    start_server_with_mock_llm_model("mock-model").await
}

/// Like `start_server_with_mock_llm`, but the mock backend's registry-default
/// model is caller-chosen instead of hardcoded to `"mock-model"`.
///
/// The model must be in place before any context is created — rc create
/// lifecycle scripts (e.g. the coder stance) read the resolved model during
/// context creation, so setting it afterward is too late to affect them.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn start_server_with_mock_llm_model(default_model: &str) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = SshServerConfig::ephemeral(addr.port());
    if let Some(ref data_dir) = config.data_dir {
        seed_mock_backend_with_model(data_dir, default_model);
    }

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    addr
}

/// Start a server with caller-supplied config + data dir, so the test can
/// pre-populate the kernel DB and inspect kernel state on the filesystem.
///
/// Like `start_server_with_mock_llm`, but the directory is provided
/// rather than created in `/tmp`. Use for live-eval runs where the
/// artifact dir should survive on failure.
#[allow(dead_code)]
pub async fn start_server_with_state_dir(state_dir: std::path::PathBuf) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mut config = SshServerConfig::ephemeral(addr.port());
    config.config_dir = Some(state_dir.clone());
    config.data_dir = Some(state_dir);

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    addr
}

/// Start a plain ephemeral server (like `start_server`), but also hand back
/// the live `SharedKernel` it constructed — via `SshServer`'s test-only
/// `run_on_listener_with_kernel_sink` — so a test can reach kernel-internal
/// buses (e.g. `LedgerFlow`) that have no dedicated RPC to drive them.
///
/// See `ledger_events_wire.rs` for the motivating case: unlike a permission
/// ask (drivable via `hook_add` + `call_mcp_tool`), producing a genuine
/// approval-ledger change means exercising the ledger's rule/escalation
/// machinery — kernel territory, not this crate's. This lets the wire test
/// stay honest about testing the bridge (kaijutsu-server's job) without
/// reaching for kernel internals it doesn't own.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn start_server_with_kernel_handle() -> (SocketAddr, kaijutsu_server::SharedKernel) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = SshServerConfig::ephemeral(addr.port());
    let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server
            .run_on_listener_with_kernel_sink(listener, kernel_tx)
            .await
        {
            log::error!("Server error: {}", e);
        }
    });

    let kernel = kernel_rx.await.expect("server dropped the kernel handle before sending it");
    tokio::task::yield_now().await;
    (addr, kernel)
}

/// Connect to server with ephemeral key.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn connect_client(addr: SocketAddr) -> RpcClient {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: KeySource::ephemeral(),
        insecure: true,
    };

    let mut ssh_client = kaijutsu_client::SshClient::new(config);
    let rpc_channel = ssh_client.connect().await.expect("SSH connect failed");
    RpcClient::new(rpc_channel.into_stream())
        .await
        .expect("RPC client init failed")
}

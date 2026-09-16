//! Shared helpers for kaijutsu-server integration tests.
//!
//! Provides `run_local`, `start_server`, `connect_client`, and utilities
//! for exercising the full SSH + Cap'n Proto stack.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use russh::keys::PrivateKey;
use tokio::task::LocalSet;

use kaijutsu_client::{KernelHandle, KeySource, RpcClient, SshConfig};
use kaijutsu_server::{SshServer, SshServerConfig};
use kaijutsu_types::{BlockId, BlockKind, BlockQuery, ContextId, Status};

/// Run async test code on a single-threaded runtime with LocalSet (capnp-rpc requirement).
pub fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// The root SSH key bound for each server this process has started, keyed by
/// bound address. `SshServerConfig::ephemeral` mints a fresh root character
/// and key per call (`SshServerConfig::root_key`), so `connect_client`
/// cannot reuse a single well-known key — it needs the exact key bound for
/// the server it's dialing. Every `start_server*` helper below registers its
/// key here (before the config moves into the spawned server task) and
/// `connect_client` looks it up by address, so none of this module's ~150
/// callers had to change to pass a key through by hand.
static ROOT_KEYS: OnceLock<Mutex<HashMap<SocketAddr, Arc<PrivateKey>>>> = OnceLock::new();

fn root_keys() -> &'static Mutex<HashMap<SocketAddr, Arc<PrivateKey>>> {
    ROOT_KEYS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record `key` as the way to authenticate against the server bound at
/// `addr`. Call once per `start_server*` helper, before the config (and its
/// key) moves into the spawned server task.
fn register_root_key(addr: SocketAddr, key: Arc<PrivateKey>) {
    root_keys().lock().unwrap().insert(addr, key);
}

/// The key registered for `addr` by a `start_server*` helper. Panics if none
/// was registered — `connect_client` only works against a server started
/// through this module, matching every other helper's fail-loud style.
fn root_key_for(addr: SocketAddr) -> Arc<PrivateKey> {
    root_keys().lock().unwrap().get(&addr).cloned().unwrap_or_else(|| {
        panic!(
            "no root key registered for {addr}: connect_client only works against a \
             server started through one of this module's start_server* helpers"
        )
    })
}

/// Start an SSH server on an ephemeral port and return the address.
///
/// The listener is pre-bound so connections queue during kernel initialization.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn start_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = SshServerConfig::ephemeral(addr.port());
    register_root_key(addr, config.root_key());

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
        idle_timeout_secs: None,
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

/// Start a mock-model server and expose its live kernel for a wire test that
/// needs to arrange durable identity state before driving the RPC surface.
#[allow(dead_code)]
pub async fn start_server_with_mock_llm_kernel_handle(
) -> (SocketAddr, kaijutsu_server::SharedKernel) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = SshServerConfig::ephemeral(addr.port());
    register_root_key(addr, config.root_key());
    if let Some(ref data_dir) = config.data_dir {
        seed_mock_backend_with_model(data_dir, "mock-model");
    }
    let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server
            .run_on_listener_with_kernel_sink(listener, kernel_tx)
            .await
        {
            log::error!("Server error: {e}");
        }
    });
    let kernel = kernel_rx.await.expect("server dropped the kernel handle before sending it");
    tokio::task::yield_now().await;
    (addr, kernel)
}

/// Arrange the identity a model turn needs on an ephemeral kernel: a sheet
/// for the shipped default reviewer (`amy`) and a performer assigned to
/// `context`. The reviewer is left unset on the context so the turn resolves
/// it through the default path, the way a fresh context does.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub fn seed_turn_identity(
    kernel: &kaijutsu_server::SharedKernel,
    context: kaijutsu_types::ContextId,
) -> kaijutsu_types::PrincipalId {
    use kaijutsu_kernel::kernel_db::CharacterRow;
    let sheet = |name: &str| CharacterRow {
        principal_id: kaijutsu_types::PrincipalId::new(),
        name: name.to_string(),
        created_at: kaijutsu_types::now_millis() as i64,
        retired_at: None,
        handoff_ctx: None, root_ctx: None, root: false,
    };
    let performer = sheet("mock-performer");
    let db = kernel.kernel_db.lock();
    db.insert_character(&sheet("amy"))
        .expect("seed the default reviewer's character sheet");
    db.insert_character(&performer)
        .expect("seed the performer's character sheet");
    db.update_context_review(context, Some(performer.principal_id), None)
        .expect("assign the performer");
    performer.principal_id
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
    register_root_key(addr, config.root_key());
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

    // Unlike the other start_server* helpers, `state_dir` is caller-supplied
    // and may already carry state from an earlier boot (the restart
    // simulation in `context_label_resolve.rs` and `context_origin_host.rs`
    // calls this twice against the same directory). So the root character
    // and key are seeded directly into IT, not into the throwaway tempdir
    // `SshServerConfig::ephemeral` would otherwise create and discard.
    // `init_root` is idempotent on a name that's already root and binds this
    // call's freshly generated key alongside any key an earlier call bound,
    // so a second call against the same state_dir keeps the server bootable
    // and hands back a key that also authenticates.
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    let auth_db_path = state_dir.join("auth.db");
    let root_key = PrivateKey::random(&mut rand_v10::rng(), russh::keys::Algorithm::Ed25519)
        .expect("generate root key for state dir");
    {
        let kernel_db = kaijutsu_kernel::kernel_db::KernelDb::open(state_dir.join("kernel.db"))
            .expect("open kernel db for state dir");
        let auth_db =
            kaijutsu_server::AuthDb::open(&auth_db_path).expect("open auth db for state dir");
        kaijutsu_server::init::init_root(
            &kernel_db,
            &auth_db,
            SshServerConfig::EPHEMERAL_ROOT,
            root_key.public_key(),
            Some("state dir root"),
        )
        .expect("init root character for state dir");
    }
    register_root_key(addr, Arc::new(root_key));

    let mut config = SshServerConfig::ephemeral(addr.port());
    config.config_dir = Some(state_dir.clone());
    config.data_dir = Some(state_dir);
    config.auth_db_path = Some(auth_db_path);

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
    register_root_key(addr, config.root_key());
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

/// Run `code` through `shell_execute` and poll until its output block
/// reaches a terminal status, timing out after `timeout_ms`.
///
/// Returns `(command_block_id, output_content, output_status)`. Panics on
/// timeout — `e2e_kj_workflow.rs`'s pattern, lifted here so more than one
/// test file can drive an ordinary (non-gated) `kj` command without
/// duplicating the poll loop.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn shell_exec_wait_timeout(
    kernel: &KernelHandle,
    code: &str,
    context_id: ContextId,
    timeout_ms: u64,
) -> (BlockId, String, Status) {
    let cmd_block_id = kernel
        .shell_execute(code, context_id, false)
        .await
        .unwrap_or_else(|e| panic!("shell_execute({code:?}) failed: {e}"));

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    loop {
        if std::time::Instant::now() > deadline {
            let blocks = kernel
                .get_blocks(context_id, &BlockQuery::All)
                .await
                .unwrap_or_default();
            panic!(
                "shell_exec_wait({code:?}) timed out after {timeout_ms}ms.\n\
                 cmd_block_id={cmd_block_id:?}\n\
                 blocks ({} total): {blocks:#?}",
                blocks.len()
            );
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let blocks = kernel
            .get_blocks(context_id, &BlockQuery::All)
            .await
            .unwrap_or_else(|e| panic!("get_blocks failed while polling {code:?}: {e}"));

        if let Some(output) = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_block_id))
        {
            match output.status {
                Status::Done | Status::Error => {
                    return (cmd_block_id, output.content.clone(), output.status);
                }
                _ => continue,
            }
        }
    }
}

/// [`shell_exec_wait_timeout`] with the shared 10-second default.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn shell_exec_wait(
    kernel: &KernelHandle,
    code: &str,
    context_id: ContextId,
) -> (BlockId, String, Status) {
    shell_exec_wait_timeout(kernel, code, context_id, 10_000).await
}

/// The `KeySource` that authenticates against a server `start_server*`
/// started at `addr`. For a test that needs to drive the SSH/RPC connection
/// itself (a raw wire version, an unbound subsystem, an SFTP subsystem)
/// rather than going through `connect_client`, but still wants to connect AS
/// the real root character rather than an unknown, rejected key.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub fn root_key_source(addr: SocketAddr) -> KeySource {
    KeySource::InMemory(root_key_for(addr))
}

/// Connect to a server started by one of this module's `start_server*`
/// helpers, authenticating with the root key that helper bound for `addr`.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn connect_client(addr: SocketAddr) -> RpcClient {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: root_key_source(addr),
        insecure: true,
    };

    let mut ssh_client = kaijutsu_client::SshClient::new(config);
    let rpc_channel = ssh_client.connect().await.expect("SSH connect failed");
    RpcClient::new(rpc_channel.into_stream())
        .await
        .expect("RPC client init failed")
}

/// Create `label` of type `default` through `kj context create`, run from
/// the kernel's only root context (`kaijutsu_client::choose_parent`).
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn create_context(kernel: &KernelHandle, label: &str) -> Result<kaijutsu_types::ContextId, String> {
    create_context_typed(kernel, label, "default").await
}

/// Create `label` of `context_type` through `kj context create`, run from
/// the kernel's only root context.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn create_context_typed(
    kernel: &KernelHandle,
    label: &str,
    context_type: &str,
) -> Result<kaijutsu_types::ContextId, String> {
    let contexts = kernel.list_contexts().await.map_err(|e| e.to_string())?;
    let parent = kaijutsu_client::choose_parent(None, &contexts)?;
    let argv = kaijutsu_client::context_create_argv(label, context_type, None);
    let result = kernel.execute_kj_quiet(parent.context_id, &argv).await.map_err(|e| e.to_string())?;
    kaijutsu_client::context_id_from_create_result(&result)
}

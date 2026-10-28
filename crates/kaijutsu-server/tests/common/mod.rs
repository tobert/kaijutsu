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
    // Retained channel/task destructors still need the runtime during teardown.
    let _entered = rt.enter();
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
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub fn register_root_key(addr: SocketAddr, key: Arc<PrivateKey>) {
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

/// Start an ephemeral server whose temporary rc bindings allow host subprocesses.
#[allow(dead_code)] // Shared helper: only host-exec tests need the opt-in.
pub async fn start_server_with_host_exec() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = SshServerConfig::ephemeral(addr.port()).with_host_exec();
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

/// Seed a `mock`-kind backend into the kernel DB at `data_dir`, and a `mock`
/// cast that runs it for every context type a test creates, which the
/// ephemeral performer plays, so `initialize_kernel_models()` brings up a
/// registry a test can drive without a live provider and a context made as
/// [`PERFORMER`] binds a model.
///
/// Model configuration is SQL-native (`models.toml` is demolished), so the
/// injection point is the DB itself rather than a host file the kernel reads
/// once at seed time. The server opens the same `kernel.db` a moment later,
/// and `mock` is its only backend: a kernel ships none.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub fn seed_mock_backend(data_dir: &std::path::Path) {
    seed_mock_backend_with_model(data_dir, "mock-model");
}

/// Like `seed_mock_backend`, but the cast's slots run a caller-chosen model.
///
/// Needed to exercise rc scripts (like the coder stance) that branch on the
/// bound model: the model comes from the cast the performer's instance binds,
/// so it is set before any context is created, and a test can show a context
/// ran on its binding rather than on anything stamped on its row.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub fn seed_mock_backend_with_model(data_dir: &std::path::Path, model: &str) {
    use kaijutsu_kernel::kernel_db::{BackendRow, CastRow, CastSlotRow, KernelDb};
    use kaijutsu_types::{BackendId, CastId, PrincipalId};

    std::fs::create_dir_all(data_dir).expect("create data dir");
    let db = KernelDb::open(data_dir.join("kernel.db")).expect("open kernel db for mock seed");
    let backend = db
        .upsert_backend(&BackendRow {
            backend_id: BackendId::new(),
            name: "mock".to_string(),
            kind: "mock".to_string(),
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout_secs: None,
            idle_timeout_secs: None,
            max_concurrent: None,
            created_at: 0,
            created_by: PrincipalId::system(),
        })
        .expect("seed mock backend");
    let cast = CastRow {
        cast_id: CastId::new(),
        label: "mock".to_string(),
        description: None,
        created_at: 0,
        created_by: PrincipalId::system(),
    };
    db.insert_cast(&cast).expect("seed the mock cast");
    for role in ["default", "coder", "musician", "director", "assistant", "mcp", "toolie"] {
        db.set_cast_slot(&CastSlotRow {
            cast_id: cast.cast_id,
            role: role.to_string(),
            backend_id: backend.backend_id,
            model: model.to_string(),
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            effort: None,
            thinking_budget: None,
            thinking_style: None,
            loadout: None,
            extra: None,
        })
        .expect("seed a mock cast slot");
    }
    // An ephemeral kernel has its performer; an offline home does not, and a
    // test there points its own character at the `mock` cast.
    if let Some(performer) = db.get_character_by_name(PERFORMER).expect("read the ephemeral performer") {
        db.set_character_cast(performer.principal_id, Some(cast.cast_id))
            .expect("point the performer at the mock cast");
    }
}

/// Set up `config`'s kernel the way `kaijutsu-server bootstrap` does, with the
/// mock backend's `model` as its default: the backend, the defaults, `ROOT`,
/// and its performer, all through `kj` as the ephemeral root character. Call
/// it before the server boots; it holds the kernel lock while it runs.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn bootstrap_mock(config: &SshServerConfig, model: &str) {
    let data_dir = config.data_dir.as_ref().expect("an ephemeral config has a data dir");
    let root = kaijutsu_kernel::kernel_db::KernelDb::open(data_dir.join("kernel.db"))
        .expect("open kernel.db")
        .list_characters(false)
        .expect("read characters")
        .into_iter()
        .find(|row| row.root)
        .expect("an ephemeral config has a root character")
        .name;
    let opts = kaijutsu_server::bootstrap::BootstrapOptions {
        root,
        root_key: config.root_key().public_key().clone(),
        key_comment: None,
        backends: vec![kaijutsu_server::bootstrap::BackendChoice::mock(model)],
        console_performer: "root".to_string(),
    };
    kaijutsu_server::bootstrap::bootstrap(config, &opts).await.expect("bootstrap the test kernel");
    // A test's contexts are made by the ephemeral performer, whose cast runs
    // the mock model for every context type.
    seed_mock_backend_with_model(data_dir, model);
}

/// Start a server whose ephemeral kernel DB carries a mock LLM backend and a
/// cast the ephemeral performer plays.
///
/// This makes `initialize_kernel_models()` register a "mock" backend so that
/// model turns and other LLM-dependent paths work in tests.
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

/// Arrange the identity a model turn needs on an ephemeral kernel: the
/// character that performs `context`, a new one when the context has no
/// instance yet. The reviewer is left unset on the
/// context, so the turn resolves it through the walk to the ephemeral root
/// character that plays the root context above it.
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
    let db = kernel.kernel_db.lock();
    // A context made by `create_context` already has a context character;
    // that character is its performer.
    if let Some(character) = db.context_character(context).expect("read the context's context character") {
        return character.principal_id;
    }
    let performer = sheet("mock-performer");
    db.insert_character(&performer)
        .expect("seed the performer's character sheet");
    db.assign_test_review(context, Some(performer.principal_id), None)
        .expect("assign the performer");
    // A context answers to its senpai's holder. One made from a context
    // nobody performs has no reviewer, so home it under a root console.
    let routable = db.senpai_of(context).ok().flatten()
        .is_some_and(|senpai| db.context_holder(senpai).ok().flatten().is_some());
    if !routable {
        let console = db.list_characters(false).expect("list characters").into_iter()
            .find(|c| c.root).and_then(|c| c.root_ctx);
        if let Some(console) = console.filter(|console| *console != context) {
            db.set_context_senpai(context, Some(console)).expect("home the seeded context under a root console");
        }
    }
    performer.principal_id
}

/// Like `start_server_with_mock_llm`, but the cast's model is caller-chosen
/// instead of hardcoded to `"mock-model"`.
///
/// The model must be in place before any context is created — a context binds
/// it when it is made, and rc create lifecycle scripts (e.g. the coder
/// stance) read the resolved model during context creation.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn start_server_with_mock_llm_model(model: &str) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = SshServerConfig::ephemeral(addr.port());
    register_root_key(addr, config.root_key());
    if let Some(ref data_dir) = config.data_dir {
        seed_mock_backend_with_model(data_dir, model);
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
    start_state_dir_server(state_dir).await.addr
}

/// A server running on a caller-supplied state directory, with the task that
/// runs it. One kernel per data directory (`docs/server-cli.md`): a test
/// that boots again over the same directory stops this one first, which
/// releases its lock.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub struct StateDirServer {
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl StateDirServer {
    /// Stop the server and wait until its task is gone.
    #[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
    pub async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// [`start_server_with_state_dir`], handing back the running server so the
/// caller can stop it.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn start_state_dir_server(state_dir: std::path::PathBuf) -> StateDirServer {
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
        SshServerConfig::seed_ephemeral_performer(&kernel_db);
    }
    register_root_key(addr, Arc::new(root_key));

    let mut config = SshServerConfig::ephemeral(addr.port());
    config.config_dir = Some(state_dir.clone());
    config.data_dir = Some(state_dir);
    config.auth_db_path = Some(auth_db_path);

    let task = tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    StateDirServer { addr, task }
}

/// Start a plain ephemeral server (like `start_server`), but also hand back
/// the live `SharedKernel` it constructed — via `SshServer`'s test-only
/// `run_on_listener_with_kernel_sink` — so a test can reach kernel-internal
/// buses (e.g. `LedgerFlow`) that have no dedicated RPC to drive them.
///
/// See `ledger_events_wire.rs` for the motivating case: producing a genuine
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

/// Write the shipped `gate.toml` into `config`'s kernel config with `keys`
/// added to its global allow tier. For a test whose model or sheetless
/// credential runs setup verbs such as `kj context create`: those are
/// uncovered, so they would ask, and the ask is not what the test is about.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub fn allow_in_shipped_gate(config: &kaijutsu_server::SshServerConfig, keys: &[&str]) {
    let path = config.config_mounts.host_dir(kaijutsu_types::paths::CONFIG_ROOT).join("gate.toml");
    std::fs::create_dir_all(path.parent().expect("gate.toml has a parent dir")).expect("create the kernel config dir");
    let shipped = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/defaults/gate.toml"))
        .expect("read the shipped gate.toml");
    let global = "[global]\nallow = [\n";
    assert_eq!(shipped.matches(global).count(), 1, "the shipped gate.toml carries one global allow tier");
    let added: String = keys.iter().map(|k| format!("  {k:?},\n")).collect();
    std::fs::write(&path, shipped.replacen(global, &format!("{global}{added}"), 1)).expect("write gate.toml");
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

/// The live, non-root character `create_context` casts: the ephemeral
/// kernel's own (`SshServerConfig::EPHEMERAL_PERFORMER`), since its root
/// performs only its root console.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub const PERFORMER: &str = kaijutsu_server::SshServerConfig::EPHEMERAL_PERFORMER;

/// Create `label` of type `default` as [`PERFORMER`] through `kj context create`,
/// run from the kernel's only root context (`kaijutsu_client::choose_parent`).
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn create_context(kernel: &KernelHandle, label: &str) -> Result<kaijutsu_types::ContextId, String> {
    create_context_typed(kernel, label, "default").await
}

/// [`create_context`] with a `context_type`.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn create_context_typed(
    kernel: &KernelHandle,
    label: &str,
    context_type: &str,
) -> Result<kaijutsu_types::ContextId, String> {
    create_context_typed_as(kernel, label, context_type, PERFORMER).await
}

/// Create `label` of type `default` as the live, non-root `character`.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn create_context_as(kernel: &KernelHandle, label: &str, character: &str) -> Result<kaijutsu_types::ContextId, String> {
    create_context_typed_as(kernel, label, "default", character).await
}

/// Create `label` of `context_type` as `character` through `kj context create`,
/// run from the kernel's only root context.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn create_context_typed_as(
    kernel: &KernelHandle,
    label: &str,
    context_type: &str,
    character: &str,
) -> Result<kaijutsu_types::ContextId, String> {
    let contexts = kernel.list_contexts().await.map_err(|e| e.to_string())?;
    let parent = kaijutsu_client::choose_parent(None, &contexts)?;
    let argv = kaijutsu_client::context_create_argv(label, context_type, character, None);
    let result = kernel.execute_kj_quiet(parent.context_id, &argv).await.map_err(|e| e.to_string())?;
    kaijutsu_client::context_id_from_create_result(&result)
}

/// Run `kj context <verb> <context> [args]` from the kernel's only root
/// context, and panic on a nonzero exit.
#[allow(dead_code)] // Shared helper: not every test binary that compiles `common` uses it.
pub async fn context_verb(kernel: &KernelHandle, verb: &str, context: kaijutsu_types::ContextId, args: &[&str]) {
    let contexts = kernel.list_contexts().await.expect("list contexts");
    let root = kaijutsu_client::choose_parent(None, &contexts).expect("the kernel's only root context");
    let mut argv = vec!["context".to_string(), verb.to_string(), context.to_hex()];
    argv.extend(args.iter().map(|arg| arg.to_string()));
    let result = kernel.execute_kj_quiet(root.context_id, &argv).await.expect("executeKj");
    assert_eq!(result.exit_code, 0, "kj {}: {}", argv.join(" "), result.stderr);
}

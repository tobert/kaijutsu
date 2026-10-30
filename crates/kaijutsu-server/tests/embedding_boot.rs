//! Embedding backend configuration and semantic index initialization at boot.
use kaijutsu_kernel::{KernelDb, kernel_db::{BackendRow, EmbeddingConfigRow}};
use kaijutsu_server::{config_mounts::ConfigMounts, rpc::create_shared_kernel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn vector_reply(dimensions: usize) -> String {
    let body = serde_json::json!({"data": [{"index": 0, "embedding": vec![1.0; dimensions]}]}).to_string();
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}")
}

/// A backend row of kind `openai` at `base_url`, its key in a file, and the
/// `embedding_config` row that names it.
fn configure(db: &KernelDb, dir: &std::path::Path, base_url: Option<String>, key: Option<&str>, row: EmbeddingConfigRow) {
    let key_file = key.map(|key| {
        let path = dir.join("embedding-key");
        std::fs::write(&path, format!("{key}\n")).unwrap();
        path.to_string_lossy().into_owned()
    });
    db.upsert_backend(&BackendRow {
        backend_id: kaijutsu_types::BackendId::new(),
        name: "openrouter".into(), kind: "openai".into(), base_url,
        // An explicitly named variable does not fall back to the standard one,
        // so a missing key stays missing whatever the host exports.
        api_key_env: key.is_none().then(|| "KAIJUTSU_EMBEDDING_TEST_UNSET_KEY".to_owned()),
        api_key_file: key_file, key_optional: false,
        request_timeout_secs: None, idle_timeout_secs: None, max_concurrent: None,
        created_at: 0, created_by: kaijutsu_types::PrincipalId::system(),
    }).unwrap();
    db.set_embedding_config(&row).unwrap();
}

fn row(dimensions: i64, timeout_ms: i64) -> EmbeddingConfigRow {
    EmbeddingConfigRow {
        enabled: true, backend: "openrouter".into(), model: "boot-test".into(),
        dimensions, profile: "plain".into(), timeout_ms, max_in_flight: 2, max_context_bytes: 2048,
    }
}

async fn boot(dir: &std::path::Path) -> std::sync::Arc<kaijutsu_server::rpc::SharedKernelState> {
    create_shared_kernel(None, &ConfigMounts::new(dir.join("config")), Some(dir), &[], &[]).await.unwrap()
}

#[tokio::test]
async fn boot_probes_the_provider_and_indexes_at_the_configured_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let n = stream.read(&mut request).await.unwrap();
        let request = std::str::from_utf8(&request[..n]).unwrap().to_owned();
        stream.write_all(vector_reply(1024).as_bytes()).await.unwrap();
        request
    });
    let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
    configure(&db, dir.path(), Some(base_url), Some("sk-boot"), row(1024, 2000));
    insert_root_character(&db);
    drop(db);
    let shared = boot(dir.path()).await;
    let index = wait_for_index(&shared).await.expect("the probe must initialize the index");
    assert_eq!(index.embedder().model_name(), "boot-test");
    assert_eq!(index.embedder().dimensions(), 1024);
    let request = server.await.unwrap();
    assert!(request.starts_with("POST /api/v1/embeddings "), "{request}");
    assert!(request.to_ascii_lowercase().contains("authorization: bearer sk-boot"), "{request}");
}

/// A council server needs no key and renders its own task prompts: boot
/// pins its identity and indexes at the configured size.
#[tokio::test]
async fn boot_embeds_through_a_council_server() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = gogi_fake::Config::default();
    config.models.push(gogi_fake::FakeModel::embedding("embed-toy-1", 7));
    let council = gogi_fake::Server::start(config).unwrap();
    let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
    db.upsert_backend(&BackendRow {
        backend_id: kaijutsu_types::BackendId::new(),
        name: "spaceghost".into(), kind: "mk".into(), base_url: Some(council.url()),
        api_key_env: None, api_key_file: None, key_optional: true,
        request_timeout_secs: None, idle_timeout_secs: None, max_concurrent: None,
        created_at: 0, created_by: kaijutsu_types::PrincipalId::system(),
    }).unwrap();
    db.set_embedding_config(&EmbeddingConfigRow {
        backend: "spaceghost".into(), model: "embed-toy-1".into(), dimensions: 32,
        ..row(32, 2000)
    }).unwrap();
    insert_root_character(&db);
    drop(db);
    let shared = boot(dir.path()).await;
    let index = wait_for_index(&shared).await.expect("a council server must initialize the index");
    assert_eq!(index.embedder().model_name(), "embed-toy-1");
    assert_eq!(index.embedder().dimensions(), 32);
}

#[tokio::test]
async fn unavailable_provider_leaves_index_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    drop(listener);
    let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
    configure(&db, dir.path(), Some(base_url), Some("sk-boot"), row(1024, 100));
    insert_root_character(&db);
    drop(db);
    let shared = boot(dir.path()).await;
    assert!(wait_for_index(&shared).await.is_none(), "must not substitute another embedding model");
}

/// A model that returns a different vector size than the row says must not
/// feed an index of the wrong size.
#[tokio::test]
async fn a_provider_returning_other_dimensions_leaves_index_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        stream.write_all(vector_reply(768).as_bytes()).await.unwrap();
    });
    let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
    configure(&db, dir.path(), Some(base_url), Some("sk-boot"), row(1024, 2000));
    insert_root_character(&db);
    drop(db);
    let shared = boot(dir.path()).await;
    assert!(wait_for_index(&shared).await.is_none());
    server.await.unwrap();
}

/// No key, or a row naming a backend that is not there, leaves the index off
/// and boot standing.
#[tokio::test]
async fn a_backend_without_a_key_or_a_row_without_a_backend_leaves_index_unavailable() {
    for (what, key, backend) in [("no key", None, "openrouter"), ("no such backend", Some("sk-boot"), "missing")] {
        let dir = tempfile::tempdir().unwrap();
        let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
        configure(&db, dir.path(), Some("http://127.0.0.1:9/api/v1".into()), key,
            EmbeddingConfigRow { backend: backend.into(), ..row(1024, 100) });
        insert_root_character(&db);
        drop(db);
        let shared = boot(dir.path()).await;
        assert!(wait_for_index(&shared).await.is_none(), "{what}");
    }
}

/// Boot does not wait for the provider: one that accepts and never answers
/// must not hold kernel construction for its timeout.
#[tokio::test]
async fn a_silent_provider_does_not_hold_boot() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let held = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        drop(stream);
    });
    let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
    configure(&db, dir.path(), Some(base_url), Some("sk-boot"), row(1024, 30_000));
    insert_root_character(&db);
    drop(db);
    let started = std::time::Instant::now();
    let shared = boot(dir.path()).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(10),
        "boot waited {:?} on a silent embedding provider", started.elapsed());
    assert!(shared.kj_dispatcher.semantic_index().is_none(), "no index before the provider answers");
    held.abort();
}

/// The index attaches in the background once the service answers. Wait for
/// it, bounded, and report what arrived.
async fn wait_for_index(shared: &kaijutsu_server::rpc::SharedKernelState) -> Option<std::sync::Arc<kaijutsu_index::SemanticIndex>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(index) = shared.kj_dispatcher.semantic_index() { return Some(index); }
        if std::time::Instant::now() > deadline { return None; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// `create_shared_kernel` refuses to start against a `kernel.db` with no
/// live root character (`docs/character.md`, "Bootstrap: the person creates
/// themself"). These tests drive `create_shared_kernel` directly on a
/// hand-built `kernel.db`, so seed the root row the same way
/// `kaijutsu-server init` would, without going through a real SSH boot.
fn insert_root_character(db: &KernelDb) {
    db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
        principal_id: kaijutsu_types::PrincipalId::new(),
        name: "tester".to_string(),
        created_at: kaijutsu_types::now_millis() as i64,
        retired_at: None,
        handoff_ctx: None,
        root_ctx: None,
        root: true,
    })
    .expect("insert root character for embedding_boot fixture");
}

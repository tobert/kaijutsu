//! Service discovery, configuration, and semantic index initialization at boot.
use kaijutsu_kernel::{KernelDb, kernel_db::EmbeddingConfigRow};
use kaijutsu_server::{config_mounts::ConfigMounts, rpc::create_shared_kernel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn boot_uses_discovered_dimensions_and_no_builtin_model_files() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let n = stream.read(&mut request).await.unwrap();
        assert!(std::str::from_utf8(&request[..n]).unwrap().starts_with("GET /v1/models "));
        stream.write_all(concat!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
            "[{\"kind\":\"embedder\",\"id\":\"boot-test\",\"weight_hash\":\"",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "\",\"hidden_size\":1024}]").as_bytes()).await.unwrap();
    });
    let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
    db.set_embedding_config(&EmbeddingConfigRow { enabled: true, endpoint,
        timeout_ms: 2000, max_in_flight: 2, max_context_bytes: 2048 }).unwrap();
    insert_root_character(&db);
    drop(db);
    let shared = create_shared_kernel(None, &ConfigMounts::new(dir.path().join("config")), Some(dir.path()), &[], &[]).await.unwrap();
    let index = wait_for_index(&shared).await.expect("service discovery must initialize the index");
    assert_eq!(index.embedder().model_name(), "boot-test");
    assert_eq!(index.embedder().dimensions(), 1024);
    server.await.unwrap();
}

#[tokio::test]
async fn unavailable_service_leaves_index_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
    db.set_embedding_config(&EmbeddingConfigRow { enabled: true, endpoint,
        timeout_ms: 100, max_in_flight: 1, max_context_bytes: 2048 }).unwrap();
    insert_root_character(&db);
    drop(db);
    let shared = create_shared_kernel(None, &ConfigMounts::new(dir.path().join("config")), Some(dir.path()), &[], &[]).await.unwrap();
    assert!(wait_for_index(&shared).await.is_none(), "must not substitute another embedding model");
}

/// Boot does not wait for the embedding service: a service that accepts and
/// never answers must not hold kernel construction for its timeout.
#[tokio::test]
async fn a_silent_service_does_not_hold_boot() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let held = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        drop(stream);
    });
    let db = KernelDb::open(dir.path().join("kernel.db")).unwrap();
    db.set_embedding_config(&EmbeddingConfigRow { enabled: true, endpoint,
        timeout_ms: 30_000, max_in_flight: 1, max_context_bytes: 2048 }).unwrap();
    insert_root_character(&db);
    drop(db);
    let started = std::time::Instant::now();
    let shared = create_shared_kernel(None, &ConfigMounts::new(dir.path().join("config")), Some(dir.path()), &[], &[]).await.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10),
        "boot waited {:?} on a silent embedding service", started.elapsed());
    assert!(shared.kj_dispatcher.semantic_index().is_none(), "no index before the service answers");
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

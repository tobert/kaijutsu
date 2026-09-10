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
    drop(db);
    let shared = create_shared_kernel(None, &ConfigMounts::new(dir.path().join("config")), Some(dir.path())).await.unwrap();
    let index = shared.semantic_index.as_ref().expect("service discovery must initialize the index");
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
    drop(db);
    let shared = create_shared_kernel(None, &ConfigMounts::new(dir.path().join("config")), Some(dir.path())).await.unwrap();
    assert!(shared.semantic_index.is_none(), "must not substitute another embedding model");
}

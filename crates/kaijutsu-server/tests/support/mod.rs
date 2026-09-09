pub fn disable_embeddings(data_dir: &std::path::Path) {
    let db = kaijutsu_kernel::KernelDb::open(data_dir.join("kernel.db")).unwrap();
    db.set_embedding_config(&kaijutsu_kernel::kernel_db::EmbeddingConfigRow {
        enabled: false, endpoint: "http://127.0.0.1:9".into(),
        timeout_ms: 100, max_in_flight: 1, max_context_bytes: 2048,
    }).unwrap();
}

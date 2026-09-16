pub fn disable_embeddings(data_dir: &std::path::Path) {
    let db = kaijutsu_kernel::KernelDb::open(data_dir.join("kernel.db")).unwrap();
    db.set_embedding_config(&kaijutsu_kernel::kernel_db::EmbeddingConfigRow {
        enabled: false, endpoint: "http://127.0.0.1:9".into(),
        timeout_ms: 100, max_in_flight: 1, max_context_bytes: 2048,
    }).unwrap();
}

/// Make `name` a live root character in `data_dir`'s `kernel.db`, the way
/// `kaijutsu-server init` does, so `create_shared_kernel` will start.
#[allow(dead_code)]
pub fn init_root(data_dir: &std::path::Path, name: &str) -> kaijutsu_types::PrincipalId {
    let principal_id = kaijutsu_types::PrincipalId::new();
    let db = kaijutsu_kernel::KernelDb::open(data_dir.join("kernel.db")).unwrap();
    db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
        principal_id,
        name: name.to_string(),
        created_at: 1,
        retired_at: None,
        handoff_ctx: None,
        root_ctx: None,
        root: true,
    }).unwrap();
    principal_id
}

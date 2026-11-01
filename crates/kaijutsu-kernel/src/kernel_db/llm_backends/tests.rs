use super::*;

/// A database whose `backend_models` predates the tunable columns opens,
/// keeps its rows with every tunable unset, and stores tunables after.
#[test]
fn backend_models_gain_tunables_on_existing_databases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let backend_id = BackendId::new();
    {
        let db = KernelDb::open(&path).unwrap();
        db.conn.execute_batch(
            "DROP TABLE backend_models;
             CREATE TABLE backend_models (
                 backend_id BLOB NOT NULL REFERENCES backends(backend_id) ON DELETE CASCADE,
                 model_id TEXT NOT NULL,
                 context_window INTEGER CHECK (context_window IS NULL OR context_window > 0),
                 extra TEXT,
                 PRIMARY KEY (backend_id, model_id));",
        ).unwrap();
        db.conn.execute(
            "INSERT INTO backends (backend_id, name, kind, key_optional, created_at, created_by)
             VALUES (?1, 'old', 'openai', 1, 0, ?2)",
            params![blob_param(backend_id.as_bytes()), blob_param(PrincipalId::system().as_bytes())],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO backend_models (backend_id, model_id, context_window) VALUES (?1, 'm', 1000)",
            params![blob_param(backend_id.as_bytes())],
        ).unwrap();
    }
    let db = KernelDb::open(&path).unwrap();
    let rows = db.list_backend_models(backend_id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].context_window, Some(1000));
    assert_eq!(rows[0].tunables, ModelTunablesRow::default());
    let mut row = rows[0].clone();
    row.tunables.max_tokens = Some(65536);
    row.tunables.effort = Some("high".into());
    db.set_backend_model(&row).unwrap();
    assert_eq!(db.list_backend_models(backend_id).unwrap()[0].tunables.max_tokens, Some(65536));
    assert!(db.conn.execute(
        "UPDATE backend_models SET max_tokens = 0 WHERE model_id = 'm'", [],
    ).is_err(), "the migrated column carries its CHECK");
}

fn embedding_row() -> EmbeddingConfigRow {
    EmbeddingConfigRow {
        enabled: true, backend: "openrouter".into(), model: "qwen/qwen3-embedding-8b".into(),
        dimensions: 1024, profile: "qwen3".into(),
        timeout_ms: 30_000, max_in_flight: 2, max_context_bytes: 2048,
    }
}

/// Both older shapes (a builtin model directory, then a service endpoint)
/// name no backend and the kernel ships no default, so the rebuild leaves
/// the table empty: the semantic index stays off until an operator names
/// a backend and model.
#[test]
fn legacy_embedding_config_migrates_to_an_unconfigured_service() {
    for (shape, legacy) in [
        ("builtin model", "CREATE TABLE embedding_config (id INTEGER PRIMARY KEY, enabled INTEGER NOT NULL,
            model_dir TEXT NOT NULL, dimensions INTEGER NOT NULL, max_tokens INTEGER NOT NULL);
         INSERT INTO embedding_config VALUES (1, 1, '/old/model', 384, 123);"),
        ("service endpoint", "CREATE TABLE embedding_config (id INTEGER PRIMARY KEY, enabled INTEGER NOT NULL,
            endpoint TEXT NOT NULL, timeout_ms INTEGER NOT NULL, max_in_flight INTEGER NOT NULL,
            max_context_bytes INTEGER NOT NULL);
         INSERT INTO embedding_config VALUES (1, 1, 'http://embedder.example:8088', 30000, 2, 2048);"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        {
            let db = KernelDb::open(&path).unwrap();
            db.conn.execute_batch(&format!("DROP TABLE embedding_config; {legacy}")).unwrap();
        }
        let db = KernelDb::open(&path).unwrap();
        assert_eq!(db.get_embedding_config().unwrap(), None, "{shape}");
        db.set_embedding_config(&embedding_row()).unwrap();
        drop(db);
        assert_eq!(KernelDb::open(&path).unwrap().get_embedding_config().unwrap(), Some(embedding_row()), "{shape}");
    }
}

/// The old-shape test is whether the table lacks `backend`, not whether it
/// carries an old column: a table with the new columns keeps its row.
#[test]
fn a_table_with_the_new_columns_keeps_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        db.set_embedding_config(&embedding_row()).unwrap();
        db.conn.execute_batch("ALTER TABLE embedding_config ADD COLUMN endpoint TEXT;").unwrap();
    }
    assert_eq!(KernelDb::open(&path).unwrap().get_embedding_config().unwrap(), Some(embedding_row()));
}

#[test]
fn an_embedding_row_is_refused_when_it_cannot_work() {
    let db = KernelDb::temporary().unwrap();
    for (what, row) in [
        ("empty backend", EmbeddingConfigRow { backend: " ".into(), ..embedding_row() }),
        ("empty model", EmbeddingConfigRow { model: String::new(), ..embedding_row() }),
        ("zero dimensions", EmbeddingConfigRow { dimensions: 0, ..embedding_row() }),
        ("zero timeout", EmbeddingConfigRow { timeout_ms: 0, ..embedding_row() }),
        ("zero concurrency", EmbeddingConfigRow { max_in_flight: 0, ..embedding_row() }),
        ("zero projection", EmbeddingConfigRow { max_context_bytes: 0, ..embedding_row() }),
    ] {
        assert!(db.set_embedding_config(&row).is_err(), "accepted: {what}");
    }
    let error = db.set_embedding_config(&EmbeddingConfigRow { profile: "gemini".into(), ..embedding_row() })
        .unwrap_err().to_string();
    assert!(error.contains("plain") && error.contains("qwen3") && error.contains("embeddinggemma"),
        "an unknown profile names the valid ones: {error}");
    assert_eq!(db.get_embedding_config().unwrap(), None, "a refusal writes nothing");
}

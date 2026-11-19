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

/// A backend named `openrouter` and its `qwen3` embedding model row.
fn embedding_backend(db: &KernelDb) -> BackendId {
    let backend_id = BackendId::new();
    db.conn.execute(
        "INSERT INTO backends (backend_id, name, kind, base_url, key_optional, created_at, created_by)
         VALUES (?1, 'openrouter', 'openai', 'https://openrouter.ai/api/v1', 1, 0, ?2)",
        params![blob_param(backend_id.as_bytes()), blob_param(PrincipalId::system().as_bytes())],
    ).unwrap();
    db.set_backend_model(&BackendModelRow {
        backend_id, model_id: "qwen/qwen3-embedding-8b".into(), context_window: None, extra: None,
        tunables: Default::default(), embedding_profile: Some("qwen3".into()),
    }).unwrap();
    backend_id
}

fn embedding_row(backend_id: BackendId) -> EmbeddingConfigRow {
    EmbeddingConfigRow {
        enabled: true, backend_id, model: "qwen/qwen3-embedding-8b".into(),
        dimensions: 1024, timeout_ms: 30_000, max_in_flight: 2, max_context_bytes: 2048,
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
        let backend_id = {
            let db = KernelDb::open(&path).unwrap();
            let backend_id = embedding_backend(&db);
            db.conn.execute_batch(&format!("DROP TABLE embedding_config; {legacy}")).unwrap();
            backend_id
        };
        let db = KernelDb::open(&path).unwrap();
        assert_eq!(db.get_embedding_config().unwrap(), None, "{shape}");
        db.set_embedding_config(&embedding_row(backend_id)).unwrap();
        drop(db);
        assert_eq!(KernelDb::open(&path).unwrap().get_embedding_config().unwrap(), Some(embedding_row(backend_id)), "{shape}");
    }
}

/// The shape that named its backend by name and held the profile itself.
const NAME_SHAPE: &str = "DROP TABLE embedding_config;
    CREATE TABLE embedding_config (
        id INTEGER NOT NULL PRIMARY KEY DEFAULT 1 CHECK (id = 1),
        enabled INTEGER NOT NULL DEFAULT 1,
        backend TEXT NOT NULL CHECK (backend != ''),
        model TEXT NOT NULL CHECK (model != ''),
        dimensions INTEGER NOT NULL CHECK (dimensions > 0),
        profile TEXT NOT NULL DEFAULT 'plain',
        timeout_ms INTEGER NOT NULL CHECK (timeout_ms > 0),
        max_in_flight INTEGER NOT NULL CHECK (max_in_flight > 0),
        max_context_bytes INTEGER NOT NULL CHECK (max_context_bytes > 0));";

/// A row that named its backend keeps working: it points at the backend by
/// id, and its profile moves onto the model row, made if it was missing. The
/// name matches without case, as the old reader matched it.
#[test]
fn a_row_naming_its_backend_moves_to_its_id_and_the_model_row_takes_the_profile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let backend_id = BackendId::new();
    {
        let db = KernelDb::open(&path).unwrap();
        db.conn.execute(
            "INSERT INTO backends (backend_id, name, kind, base_url, key_optional, created_at, created_by)
             VALUES (?1, 'ollama', 'openai', 'http://localhost:11434/v1', 1, 0, ?2)",
            params![blob_param(backend_id.as_bytes()), blob_param(PrincipalId::system().as_bytes())],
        ).unwrap();
        db.conn.execute_batch(&format!("{NAME_SHAPE}
            INSERT INTO embedding_config VALUES (1, 0, 'Ollama', 'embeddinggemma-2', 768, 'embeddinggemma', 9000, 3, 4096);
            ALTER TABLE backend_models DROP COLUMN embedding_profile;")).unwrap();
    }
    let db = KernelDb::open(&path).unwrap();
    assert_eq!(db.get_embedding_config().unwrap(), Some(EmbeddingConfigRow {
        enabled: false, backend_id, model: "embeddinggemma-2".into(),
        dimensions: 768, timeout_ms: 9000, max_in_flight: 3, max_context_bytes: 4096,
    }));
    let model = db.list_backend_models(backend_id).unwrap().into_iter()
        .find(|m| m.model_id == "embeddinggemma-2").expect("the model row is made");
    assert_eq!(model.embedding_profile.as_deref(), Some("embeddinggemma"));
    drop(db);
    assert!(KernelDb::open(&path).unwrap().get_embedding_config().unwrap().is_some(), "a second open keeps it");
}

/// A row naming a backend that is gone could never have run; it is dropped.
#[test]
fn a_row_naming_a_missing_backend_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        db.conn.execute_batch(&format!("{NAME_SHAPE}
            INSERT INTO embedding_config VALUES (1, 1, 'gone', 'm', 8, 'plain', 100, 1, 64);")).unwrap();
    }
    let db = KernelDb::open(&path).unwrap();
    assert_eq!(db.get_embedding_config().unwrap(), None);
    assert!(!KernelDb::has_column(&db.conn, "embedding_config", "backend").unwrap(), "the table has the new shape");
}

/// The old-shape test is whether the table lacks a backend column, not
/// whether it carries an old column: a table with the new columns keeps its row.
#[test]
fn a_table_with_the_new_columns_keeps_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let backend_id = {
        let db = KernelDb::open(&path).unwrap();
        let backend_id = embedding_backend(&db);
        db.set_embedding_config(&embedding_row(backend_id)).unwrap();
        db.conn.execute_batch("ALTER TABLE embedding_config ADD COLUMN endpoint TEXT;").unwrap();
        backend_id
    };
    assert_eq!(KernelDb::open(&path).unwrap().get_embedding_config().unwrap(), Some(embedding_row(backend_id)));
}

#[test]
fn an_embedding_row_is_refused_when_it_cannot_work() {
    let db = KernelDb::temporary().unwrap();
    let backend_id = embedding_backend(&db);
    for (what, row) in [
        ("empty model", EmbeddingConfigRow { model: String::new(), ..embedding_row(backend_id) }),
        ("zero dimensions", EmbeddingConfigRow { dimensions: 0, ..embedding_row(backend_id) }),
        ("zero timeout", EmbeddingConfigRow { timeout_ms: 0, ..embedding_row(backend_id) }),
        ("zero concurrency", EmbeddingConfigRow { max_in_flight: 0, ..embedding_row(backend_id) }),
        ("zero projection", EmbeddingConfigRow { max_context_bytes: 0, ..embedding_row(backend_id) }),
        ("no such backend", embedding_row(BackendId::new())),
    ] {
        assert!(db.set_embedding_config(&row).is_err(), "accepted: {what}");
    }
    let error = db.set_embedding_config(&EmbeddingConfigRow { model: "no-profile".into(), ..embedding_row(backend_id) })
        .unwrap_err().to_string();
    assert!(error.contains("--embedding-profile") && error.contains("openrouter/no-profile"),
        "a model without a profile names the command that records one: {error}");
    assert_eq!(db.get_embedding_config().unwrap(), None, "a refusal writes nothing");
}

/// An embedding profile is one of the index's tokens; a typo names them.
#[test]
fn a_model_row_refuses_an_unknown_embedding_profile() {
    let db = KernelDb::temporary().unwrap();
    let backend_id = embedding_backend(&db);
    let error = db.set_backend_model(&BackendModelRow {
        backend_id, model_id: "m".into(), context_window: None, extra: None,
        tunables: Default::default(), embedding_profile: Some("gemini".into()),
    }).unwrap_err().to_string();
    assert!(error.contains("plain") && error.contains("qwen3") && error.contains("embeddinggemma"), "{error}");
}

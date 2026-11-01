use super::*;

// ── Context model pins + Track D preset cast narrowing ──────────────

/// `migrate_preset_cast_narrowing` against a pre-Track-D `presets` table
/// (still carrying `provider`/`model`): the legacy columns are dropped
/// and a preset that had a provider/model loses it; no cast column is
/// added.
/// The pre-character-slot shape: the cast tables and `characters.cast_id`,
/// which a fresh database no longer creates.
fn with_cast_tables(db: &KernelDb) {
    KernelDb::ensure_legacy_cast_tables(&db.conn).unwrap();
    db.conn.execute("ALTER TABLE characters ADD COLUMN cast_id BLOB REFERENCES casts(cast_id) ON DELETE SET NULL", []).unwrap();
}

fn legacy_cast(db: &KernelDb, label: &str, slots: &[(&str, &str, Option<&str>)], backend: BackendId) -> Vec<u8> {
    let cast_id = CastId::new().as_bytes().to_vec();
    db.conn.execute(
        "INSERT INTO casts (cast_id, label, created_by) VALUES (?1, ?2, ?3)",
        params![cast_id, label, blob_param(PrincipalId::system().as_bytes())],
    ).unwrap();
    for (role, model, loadout) in slots {
        db.conn.execute(
            "INSERT INTO cast_slots (cast_id, role, backend_id, model, max_tokens, temperature, loadout)
             VALUES (?1, ?2, ?3, ?4, 4096, 0.5, ?5)",
            params![cast_id, role, blob_param(backend.as_bytes()), model, loadout],
        ).unwrap();
    }
    cast_id
}

/// A database still on casts is copied aside before they move, and the
/// copy holds the casts. A database already on character slots makes no
/// copy.
#[test]
fn a_database_on_casts_is_copied_aside_before_it_migrates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    drop(KernelDb::open(&path).unwrap());
    assert!(!KernelDb::pre_character_slots_backup_path(&path).exists(), "a database on the new shape makes no backup");
    {
        let conn = Connection::open(&path).unwrap();
        KernelDb::ensure_legacy_cast_tables(&conn).unwrap();
        conn.execute("ALTER TABLE characters ADD COLUMN cast_id BLOB", []).unwrap();
        conn.execute(
            "INSERT INTO casts (cast_id, label, created_by) VALUES (?1, 'house', ?2)",
            params![CastId::new().as_bytes().to_vec(), blob_param(PrincipalId::system().as_bytes())],
        ).unwrap();
    }

    let db = KernelDb::open(&path).unwrap();

    assert!(!KernelDb::has_table(&db.conn, "casts").unwrap(), "the casts were dropped");
    let backup = KernelDb::pre_character_slots_backup_path(&path);
    let copy = Connection::open(&backup).unwrap();
    let labels: Vec<String> = copy.prepare("SELECT label FROM casts").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<SqliteResult<_>>().unwrap();
    assert_eq!(labels, vec!["house"], "the copy holds what the migration dropped");
}

/// A copy that cannot be made refuses with the reason, and a database
/// that has no casts is not copied at all.
#[test]
fn a_failed_backup_refuses_naming_the_path() {
    let db = KernelDb::temporary().unwrap();
    KernelDb::ensure_legacy_cast_tables(&db.conn).unwrap();
    let unwritable = Path::new("/nonexistent-kernel-db-directory/kernel.db");
    let error = KernelDb::back_up_before_character_slots(&db.conn, unwritable).unwrap_err().to_string();
    assert!(error.contains("kernel.db.pre-character-slots") && error.contains("could not be made"), "{error}");

    KernelDb::migrate_character_slots(&db.conn).unwrap();
    KernelDb::back_up_before_character_slots(&db.conn, unwritable)
        .expect("a database with no casts needs no copy, so an unwritable path is not consulted");
}

/// Each character gets a copy of the slots of the cast it names; a
/// character with no cast gets none; the casts, the cast column and the
/// marker follow, and a second run changes nothing.
#[test]
fn character_slots_migration_copies_each_characters_cast_and_drops_the_casts() {
    let db = KernelDb::temporary().unwrap();
    with_cast_tables(&db);
    let backend = db.upsert_backend(&BackendRow {
        backend_id: BackendId::new(), name: "mock".into(), kind: "mock".into(), base_url: None, api_key_env: None,
        api_key_file: None, key_optional: true, request_timeout_secs: None, idle_timeout_secs: None,
        max_concurrent: None, created_at: 0, created_by: PrincipalId::system(),
    }).unwrap().backend_id;
    let [a, b, c] = seed_characters(&db, &["has-house", "no-cast", "also-house"])[..] else { unreachable!() };
    let house = legacy_cast(&db, "house", &[("coder", "m-coder", Some("lo")), ("distill", "m-distill", None)], backend);
    legacy_cast(&db, "orphan", &[("coder", "m-orphan", None)], backend);
    for who in [a, c] {
        db.conn.execute("UPDATE characters SET cast_id = ?1 WHERE principal_id = ?2", params![house, blob_param(who.as_bytes())]).unwrap();
    }

    KernelDb::migrate_character_slots(&db.conn).unwrap();

    for who in [a, c] {
        let slots = db.list_character_slots(who).unwrap();
        let roles: Vec<(&str, &str)> = slots.iter().map(|s| (s.role.as_str(), s.model.as_str())).collect();
        assert_eq!(roles, vec![("coder", "m-coder"), ("distill", "m-distill")]);
        assert_eq!((slots[0].max_tokens, slots[0].temperature, slots[0].backend_id), (Some(4096), Some(0.5), backend));
    }
    assert!(db.list_character_slots(b).unwrap().is_empty(), "a character with no cast gets no slot");
    assert!(!KernelDb::has_table(&db.conn, "casts").unwrap() && !KernelDb::has_table(&db.conn, "cast_slots").unwrap());
    assert!(!KernelDb::has_column(&db.conn, "characters", "cast_id").unwrap());

    KernelDb::migrate_character_slots(&db.conn).unwrap();
    assert_eq!(db.list_character_slots(a).unwrap().len(), 2, "a second run changes nothing");
}

/// A database that has not been through the context character migration
/// still has its casts to read, so the move refuses and writes nothing.
#[test]
fn character_slots_migration_refuses_before_the_context_character_migration() {
    let db = KernelDb::temporary().unwrap();
    with_cast_tables(&db);
    db.conn.execute("ALTER TABLE characters ADD COLUMN default_cast_id BLOB", []).unwrap();
    let error = KernelDb::migrate_character_slots(&db.conn).unwrap_err().to_string();
    assert!(error.contains("context character migration"), "{error}");
    assert!(KernelDb::has_table(&db.conn, "casts").unwrap(), "nothing was dropped");
}

/// A database on the new shape has no cast tables and does nothing.
#[test]
fn character_slots_migration_is_a_no_op_on_a_fresh_database() {
    let db = KernelDb::temporary().unwrap();
    assert!(!KernelDb::has_table(&db.conn, "casts").unwrap(), "a fresh database creates no casts");
    KernelDb::migrate_character_slots(&db.conn).unwrap();
    assert!(KernelDb::has_table(&db.conn, "character_slots").unwrap());
}

#[test]
fn preset_cast_narrowing_drops_the_legacy_columns() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE casts (
            cast_id BLOB NOT NULL PRIMARY KEY,
            label TEXT NOT NULL UNIQUE
         );
         CREATE TABLE presets (
            preset_id BLOB NOT NULL PRIMARY KEY,
            label TEXT NOT NULL UNIQUE,
            description TEXT,
            provider TEXT,
            model TEXT,
            system_prompt TEXT,
            consent_mode TEXT NOT NULL DEFAULT 'collaborative',
            created_at INTEGER NOT NULL,
            created_by BLOB NOT NULL
         );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO presets
            (preset_id, label, description, provider, model, system_prompt,
             consent_mode, created_at, created_by)
         VALUES (?1, 'legacy-research', NULL, 'anthropic', 'claude-opus-4-6', NULL,
                 'autonomous', 0, ?2)",
        params![vec![1u8; 16], vec![2u8; 16]],
    )
    .unwrap();

    KernelDb::migrate_preset_cast_narrowing(&conn).unwrap();

    let columns: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(presets)").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<SqliteResult<_>>()
            .unwrap()
    };
    assert!(!columns.contains(&"provider".to_string()), "provider column dropped");
    assert!(!columns.contains(&"model".to_string()), "model column dropped");
    assert!(!columns.contains(&"cast_id".to_string()), "no cast column is added");

    // Idempotent re-run: already-migrated columns, no-op.
    KernelDb::migrate_preset_cast_narrowing(&conn).unwrap();
}

/// A brand-new DB (current `SCHEMA`, no legacy columns ever existed) is
/// a clean no-op — the guard must not error just because there was
/// nothing to narrow.
#[test]
fn preset_cast_narrowing_is_a_no_op_on_a_fresh_db() {
    let db = KernelDb::temporary().unwrap();
    // temporary() already ran it once as part of open; running again
    // directly must still be a clean no-op.
    KernelDb::migrate_preset_cast_narrowing(&db.conn).unwrap();
}


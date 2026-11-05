use super::*;

fn has_type_column(db: &KernelDb, table: &str) -> bool {
    KernelDb::has_column(&db.conn, table, "context_type").unwrap()
}

fn recorded(db: &KernelDb) -> bool {
    db.conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [KernelDb::TYPE_MELT_MIGRATION],
            |r| r.get(0),
        )
        .unwrap()
}

fn backed_up(path: &Path) -> bool {
    KernelDb::pre_type_melt_backup_path(path).exists()
}

/// A fresh database never has either type column: its DDL does not create
/// them, the migration records itself with nothing to drop, and no copy is
/// made.
///
/// Falsified by a `context_type` column in `SCHEMA` for either table.
#[test]
fn a_fresh_database_has_no_type_columns_and_makes_no_copy() {
    let ddl = Connection::open_in_memory().unwrap();
    ddl.execute_batch(SCHEMA).unwrap();
    for table in ["contexts", "characters"] {
        assert!(!KernelDb::has_column(&ddl, table, "context_type").unwrap(), "SCHEMA creates no {table}.context_type");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let db = KernelDb::open(&path).unwrap();
    assert!(!has_type_column(&db, "contexts"), "a fresh contexts table has no context_type");
    assert!(!has_type_column(&db, "characters"), "a fresh characters table has no context_type");
    assert!(recorded(&db), "a fresh database records type-melt-v1");
    drop(db);
    drop(KernelDb::open(&path).unwrap());
    assert!(!backed_up(&path), "a fresh database makes no copy");
}

/// A database from before the melt runs every migration that reads the type
/// first and then loses both columns: the backfill and rename gave the
/// characters their directories from the old types, the console pointer was
/// set from the old `console` type, and contexts, characters, slots and
/// pointers come through intact. The copy holds the database as it was.
///
/// Falsified by dropping the columns before the migrations that read them
/// (the director would end with no directory and amy with no console), or by
/// leaving either column in place.
#[test]
fn an_old_database_runs_the_whole_chain_and_ends_without_the_type_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (amy, root_console, banto, console, handoff, worker, backend) = {
        let db = KernelDb::open(&path).unwrap();
        let ws = setup_test_db(&db);
        let (amy, root_console) = db.seed_test_root_console().unwrap();
        into_pre_type_melt(&db);
        set_legacy_context_type(&db, root_console, "root");
        let [banto, worker] = seed_characters(&db, &["banto", "worker"])[..] else { unreachable!() };
        for (who, old) in [(banto, "director"), (worker, "default")] {
            db.conn.execute(
                "UPDATE characters SET rc_dir = NULL WHERE principal_id = ?1",
                params![blob_param(who.as_bytes())],
            ).unwrap();
            set_legacy_character_type(&db, who, Some(old));
        }
        db.conn.execute("UPDATE characters SET rc_dir = NULL WHERE principal_id = ?1", params![blob_param(amy.as_bytes())]).unwrap();

        let console = make_context_row(Some("console"));
        insert_context_with_doc(&db, &console, ws);
        db.update_performer(console.context_id, Some(banto)).unwrap();
        db.set_context_senpai(console.context_id, Some(root_console)).unwrap();
        set_legacy_context_type(&db, console.context_id, "console");

        let handoff = make_context_row(Some("handoff/banto"));
        insert_context_with_doc(&db, &handoff, ws);
        db.set_context_senpai(handoff.context_id, Some(root_console)).unwrap();
        set_legacy_context_type(&db, handoff.context_id, "handoff");
        db.set_character_handoff_ctx(banto, Some(handoff.context_id)).unwrap();

        let backend = db.upsert_backend(&BackendRow {
            backend_id: BackendId::new(), name: "deepseek".into(), kind: "deepseek".into(), base_url: None,
            api_key_env: None, api_key_file: None, key_optional: true, request_timeout_secs: None,
            idle_timeout_secs: None, max_concurrent: None, created_at: 0, created_by: PrincipalId::system(),
        }).unwrap().backend_id;
        db.conn.execute(
            "INSERT INTO context_slots (context_id, role, backend_id, model, max_tokens) VALUES (?1, 'model', ?2, 'deepseek-flash', 4096)",
            params![blob_param(console.context_id.as_bytes()), blob_param(backend.as_bytes())],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO character_slots (principal_id, role, backend_id, model) VALUES (?1, 'distill', ?2, 'deepseek-flash')",
            params![blob_param(banto.as_bytes()), blob_param(backend.as_bytes())],
        ).unwrap();
        for name in [KernelDb::RC_DIR_BACKFILL_MIGRATION, KernelDb::RC_DIR_RENAME_MIGRATION] {
            db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [name]).unwrap();
        }
        (amy, root_console, banto, console.context_id, handoff.context_id, worker, backend)
    };

    let db = KernelDb::open(&path).unwrap();

    assert!(!has_type_column(&db, "contexts"), "contexts.context_type is dropped");
    assert!(!has_type_column(&db, "characters"), "characters.context_type is dropped");
    assert!(recorded(&db));
    // The migrations that read the type ran first, in the same open.
    let sheet = |who| db.get_character(who).unwrap().unwrap();
    assert_eq!(sheet(banto).rc_dir.as_deref(), Some("banto"), "director was backfilled, then renamed");
    assert_eq!(sheet(worker).rc_dir.as_deref(), Some("coder"), "default was backfilled, then renamed");
    assert_eq!(sheet(amy).rc_dir.as_deref(), Some("person"));
    assert_eq!(sheet(amy).console_ctx, Some(console), "the console pointer came from the old console type");
    assert_eq!(sheet(amy).root_ctx, Some(root_console));
    assert_eq!(sheet(banto).handoff_ctx, Some(handoff));
    // Contexts and their fields are intact.
    let row = |ctx| db.get_context(ctx).unwrap().unwrap();
    assert_eq!(row(console).label.as_deref(), Some("console"));
    assert_eq!(row(console).performer, Some(banto));
    assert_eq!(row(console).senpai_ctx, Some(root_console));
    assert_eq!(row(handoff).performer, None);
    assert_eq!(row(root_console).performer, Some(amy));
    // Slots are intact.
    let slots = db.list_context_slots(console).unwrap();
    assert_eq!(slots.len(), 1, "{slots:?}");
    assert_eq!((slots[0].role.as_str(), slots[0].model.as_str(), slots[0].backend_id), ("model", "deepseek-flash", backend));
    let roles: Vec<String> = db.list_character_slots(banto).unwrap().into_iter().map(|s| s.role).collect();
    assert_eq!(roles, vec!["distill".to_string()]);

    // The copy is the database before the open: it still has the columns
    // and the old values.
    assert!(backed_up(&path));
    let copy = KernelDb::open_read_only(KernelDb::pre_type_melt_backup_path(&path)).unwrap();
    assert!(has_type_column(&copy, "contexts") && has_type_column(&copy, "characters"));
    let old: String = copy.conn.query_row(
        "SELECT context_type FROM characters WHERE principal_id = ?1",
        params![blob_param(banto.as_bytes())],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(old, "director");

    // A second open changes nothing.
    drop(db);
    let db = KernelDb::open(&path).unwrap();
    assert!(!has_type_column(&db, "contexts"));
    assert_eq!(db.get_character(banto).unwrap().unwrap().rc_dir.as_deref(), Some("banto"));
}

/// The copy is made only when the migration has work: a column to drop. A
/// recorded database makes none, and a copy that cannot be made refuses,
/// naming the path.
#[test]
fn the_database_is_copied_aside_only_when_a_type_column_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::TYPE_MELT_MIGRATION]).unwrap();
    }
    drop(KernelDb::open(&path).unwrap());
    assert!(!backed_up(&path), "no type column: nothing to drop, no copy");

    let db = KernelDb::open(&path).unwrap();
    add_legacy_type_columns(&db);
    drop(db);
    drop(KernelDb::open(&path).unwrap());
    assert!(!backed_up(&path), "a recorded migration makes no copy");

    let db = KernelDb::open(&path).unwrap();
    into_pre_type_melt(&db);
    let unwritable = Path::new("/nonexistent-kernel-db-directory/kernel.db");
    let error = KernelDb::back_up_before_type_melt(&db.conn, unwritable).unwrap_err().to_string();
    assert!(error.contains("kernel.db.pre-type-melt") && error.contains("could not be made"), "{error}");
    drop(db);
    drop(KernelDb::open(&path).unwrap());
    assert!(backed_up(&path), "a type column to drop needs the copy");
}

/// A database from before `characters.context_type` still backfills each
/// character's directory from its contexts' types: the open adds the column
/// for the migrations that read it, and `type-melt-v1` drops both.
///
/// Falsified by dropping that guarded `ALTER`: the backfill finds no
/// `characters.context_type`, records itself and fills nothing.
#[test]
fn an_old_database_without_the_character_type_still_votes_from_its_contexts() {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let (_amy, root_console) = db.seed_test_root_console().unwrap();
    into_pre_type_melt(&db);
    let [lead] = seed_characters(&db, &["lead"])[..] else { unreachable!() };
    db.conn.execute("UPDATE characters SET rc_dir = NULL WHERE principal_id = ?1", params![blob_param(lead.as_bytes())]).unwrap();
    let lane = make_context_row(Some("mcp-1"));
    insert_context_with_doc(&db, &lane, ws);
    db.update_performer(lane.context_id, Some(lead)).unwrap();
    db.set_context_senpai(lane.context_id, Some(root_console)).unwrap();
    set_legacy_context_type(&db, lane.context_id, "mcp");
    db.conn.execute("ALTER TABLE characters DROP COLUMN context_type", []).unwrap();
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    assert_eq!(db.get_character(lead).unwrap().unwrap().rc_dir.as_deref(), Some("mcp"), "the live contexts voted");
    assert!(!has_type_column(&db, "contexts") && !has_type_column(&db, "characters"));
}

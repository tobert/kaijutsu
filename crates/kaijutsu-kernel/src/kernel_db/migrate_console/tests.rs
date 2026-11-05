use super::*;

fn labeled(db: &KernelDb, ws: WorkspaceId, label: &str, context_type: &str) -> ContextRow {
    let row = make_context_row(Some(label));
    insert_context_with_doc(db, &row, ws);
    set_legacy_context_type(db, row.context_id, context_type);
    row
}

fn label_of(db: &KernelDb, row: &ContextRow) -> Option<String> {
    db.get_context(row.context_id).unwrap().unwrap().label
}

fn migrated(db: &KernelDb) -> bool {
    db.conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [KernelDb::CONSOLE_LABEL_MIGRATION],
            |r| r.get(0),
        )
        .unwrap()
}

fn kept_notes(db: &KernelDb) -> Vec<ContextId> {
    db.context_character_migration_report()
        .unwrap()
        .into_iter()
        .filter(|n| n.kind == "root_label_kept")
        .filter_map(|n| n.context_id)
        .collect()
}

/// Open again as a database that has not run the migration yet.
fn run(db: &KernelDb) -> KernelDbResult<()> {
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::CONSOLE_LABEL_MIGRATION]).unwrap();
    KernelDb::apply_additive_migrations(&db.conn)
}

/// A live console labeled ROOT takes the label `console`; a second open
/// changes nothing.
#[test]
fn a_console_labeled_root_becomes_console() {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let old = labeled(&db, ws, "ROOT", "console");

    run(&db).unwrap();

    assert_eq!(label_of(&db, &old).as_deref(), Some("console"));
    assert!(migrated(&db));
    assert_eq!(kept_notes(&db), vec![]);
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    assert_eq!(label_of(&db, &old).as_deref(), Some("console"), "idempotent");
}

/// A ROOT of another type is a different context that happens to carry the
/// word: it keeps its label and is reported.
#[test]
fn a_root_label_on_another_type_is_kept_and_reported() {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let director = labeled(&db, ws, "ROOT", "director");
    let console = labeled(&db, ws, "other", "console");

    run(&db).unwrap();

    assert_eq!(label_of(&db, &director).as_deref(), Some("ROOT"));
    assert_eq!(label_of(&db, &console).as_deref(), Some("other"));
    assert_eq!(kept_notes(&db), vec![director.context_id]);
    assert!(migrated(&db));
}

/// A live context already holding `console` makes the relabel ambiguous: the
/// open refuses, names both, and changes nothing.
#[test]
fn a_live_console_label_already_taken_refuses_loudly() {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let old = labeled(&db, ws, "ROOT", "console");
    let holder = labeled(&db, ws, "console", "default");

    let error = run(&db).unwrap_err().to_string();

    assert!(error.contains("ROOT") && error.contains("console"), "{error}");
    assert!(error.contains(&old.context_id.short()) && error.contains(&holder.context_id.short()), "{error}");
    assert_eq!(label_of(&db, &old).as_deref(), Some("ROOT"));
    assert!(!migrated(&db));
}

/// An archived context labeled ROOT is left alone, and an archived holder of
/// `console` does not block the relabel.
#[test]
fn archived_contexts_neither_move_nor_block() {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let live = labeled(&db, ws, "ROOT", "console");
    let mut dead = labeled(&db, ws, "console", "console");
    db.archive_context(dead.context_id).unwrap();
    dead = db.get_context(dead.context_id).unwrap().unwrap();
    let mut old = labeled(&db, ws, "x", "console");
    db.archive_context(old.context_id).unwrap();
    db.conn
        .execute("UPDATE contexts SET label = 'ROOT' WHERE context_id = ?1", params![blob_param(old.context_id.as_bytes())])
        .unwrap();
    old = db.get_context(old.context_id).unwrap().unwrap();

    run(&db).unwrap();

    assert_eq!(label_of(&db, &live).as_deref(), Some("console"));
    assert_eq!(label_of(&db, &old).as_deref(), Some("ROOT"));
    assert_eq!(label_of(&db, &dead).as_deref(), Some("console"));
}

/// A database with no ROOT is marked migrated and nothing else changes.
#[test]
fn a_fresh_database_is_a_no_op() {
    let db = KernelDb::temporary().unwrap();
    run(&db).unwrap();
    assert!(migrated(&db));
    assert_eq!(db.context_character_migration_report().unwrap().len(), 0);
}

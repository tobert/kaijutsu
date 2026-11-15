use super::*;

/// The database as a kernel from before `context-shadows-v1` left it: the
/// `judge_shadows` table holds the shadow, `context_shadows` does not exist,
/// and the migration is not recorded.
fn old_database(path: &Path) -> (ContextId, ContextId) {
    let db = KernelDb::open(path).unwrap();
    let ws = setup_test_db(&db);
    let seat = make_context_row(Some("seat"));
    insert_context_with_doc(&db, &seat, ws);
    let mut shadow = make_context_row(Some("judge-seat"));
    shadow.forked_from = Some(seat.context_id);
    insert_context_with_doc(&db, &shadow, ws);
    db.conn
        .execute(
            "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(seat.context_id.as_bytes()), blob_param(shadow.context_id.as_bytes())],
        )
        .unwrap();
    db.conn
        .execute_batch(
            "DROP TABLE rendition_index; DROP TABLE shadow_renditions; DROP TABLE context_shadows;
             CREATE TABLE judge_shadows (
                 context_id BLOB NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE
             );",
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO judge_shadows (context_id) VALUES (?1)",
            params![blob_param(shadow.context_id.as_bytes())],
        )
        .unwrap();
    db.conn
        .execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::CONTEXT_SHADOWS_MIGRATION])
        .unwrap();
    (seat.context_id, shadow.context_id)
}

/// An old database's judge shadows come through opening as kind `judge`, its
/// marker table is gone, and a context that was no shadow is still none.
///
/// Falsified by dropping the old table before copying it.
#[test]
fn judge_shadows_carry_over_as_kind_judge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (seat, shadow) = old_database(&path);
    let db = KernelDb::open(&path).unwrap();
    let row = db.get_context(shadow).unwrap().unwrap();
    assert_eq!(db.shadow_kind(&row).unwrap().as_deref(), Some("judge"));
    assert!(db.is_judge_shadow(&row).unwrap());
    assert_eq!(db.shadow_kind(&db.get_context(seat).unwrap().unwrap()).unwrap(), None);
    assert!(!KernelDb::has_table(&db.conn, "judge_shadows").unwrap(), "the old table is dropped");
}

/// The migration runs once: a later open does not look for the old table or
/// copy anything again.
#[test]
fn the_migration_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (_, shadow) = old_database(&path);
    drop(KernelDb::open(&path).unwrap());
    let db = KernelDb::open(&path).unwrap();
    assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM context_shadows", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    let row = db.get_context(shadow).unwrap().unwrap();
    assert!(db.is_judge_shadow(&row).unwrap());
}

/// A database that has not run `council-backends-v1` either still opens: that
/// migration writes the shadows the old rule told into `context_shadows`, and
/// the old table's rows join them.
#[test]
fn the_older_migration_still_runs_on_an_old_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (_, shadow) = old_database(&path);
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::COUNCIL_BACKENDS_MIGRATION]).unwrap();
    }
    let db = KernelDb::open(&path).unwrap();
    let row = db.get_context(shadow).unwrap().unwrap();
    assert!(db.is_judge_shadow(&row).unwrap());
}

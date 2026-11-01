use super::*;

#[test]
fn failed_well_known_publication_leaves_only_a_discardable_document() {
    for role in [WellKnownRole::DriftQueue, WellKnownRole::LostFound] {
        let db = KernelDb::temporary().unwrap();
        let workspace = setup_test_db(&db);
        let row = make_context_row(Some("unpublished"));
        db.insert_document(&DocumentRow {
            document_id: row.context_id, workspace_id: workspace,
            doc_kind: DocKind::Conversation, language: None, path: None,
            created_at: row.created_at, created_by: row.created_by,
        }).unwrap();
        db.conn.execute_batch("CREATE TRIGGER reject_role BEFORE INSERT ON well_known_contexts BEGIN SELECT RAISE(ABORT, 'injected role fault'); END;").unwrap();
        let error = db.insert_well_known_context(&row, workspace, role).unwrap_err();
        assert!(error.to_string().contains("injected role fault"), "{error}");
        assert!(db.get_context(row.context_id).unwrap().is_none());
        assert!(db.well_known_context(role).unwrap().is_none());
        assert!(db.get_document(row.context_id).unwrap().is_some());
        assert!(db.delete_document(row.context_id).unwrap());
        db.conn.execute_batch("DROP TRIGGER reject_role").unwrap();
        db.insert_well_known_context(&row, workspace, role).unwrap();
        assert_eq!(db.well_known_context(role).unwrap(), Some(row.context_id));
        assert!(db.get_context(row.context_id).unwrap().is_some());
    }
}

#[test]
fn document_deletion_refuses_live_and_archived_contexts() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("retained"));
    insert_context_with_doc(&db, &ctx, ws_id);
    for archived in [false, true] {
        if archived {
            db.archive_context(ctx.context_id).unwrap();
        }
        let error = db.delete_document(ctx.context_id).expect_err("context history must survive document deletion");
        assert!(error.to_string().contains("archive"), "{error}");
        assert!(db.get_document(ctx.context_id).unwrap().is_some());
        assert!(db.get_context(ctx.context_id).unwrap().is_some());
    }
}

/// Round-trips a block's provenance row and confirms it cascades away
/// with its document.
#[test]
fn block_provenance_roundtrip_and_cascade() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let doc_id = ContextId::new();
    db.insert_document(&DocumentRow {
        document_id: doc_id,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: now_millis(),
        created_by: PrincipalId::system(),
    })
    .unwrap();

    let block_id = BlockId::new(doc_id, PrincipalId::system(), 7);
    let original = b"\x1b[31mred\x1b[0m plain";

    assert_eq!(db.get_block_provenance(&block_id, "ansi-strip").unwrap(), None);

    db.insert_block_provenance(&block_id, "ansi-strip", 1, original).unwrap();
    let (version, bytes) = db
        .get_block_provenance(&block_id, "ansi-strip")
        .unwrap()
        .expect("row just written");
    assert_eq!(version, 1);
    assert_eq!(bytes, original, "byte-exact round trip");

    // OR REPLACE: a re-run updates in place (version bumps, no dup row).
    db.insert_block_provenance(&block_id, "ansi-strip", 2, original).unwrap();
    assert_eq!(
        db.list_block_provenance(&block_id).unwrap(),
        vec![("ansi-strip".to_string(), 2, original.len())]
    );

    // Cascade with the document — the eviction story provenance rides on.
    db.conn
        .execute(
            "DELETE FROM documents WHERE document_id = ?1",
            params![blob_param(doc_id.as_bytes())],
        )
        .unwrap();
    assert_eq!(db.get_block_provenance(&block_id, "ansi-strip").unwrap(), None);
}

// ── 30. insert_document constraint classification, by read-back ─────
//
// `insert_document` failures used to be classified by matching
// `e.to_string()` against "UNIQUE constraint" / "already exists", which
// conflated the PRIMARY KEY conflict (same document, benign) with the
// `idx_documents_path` conflict (a different document claiming a taken
// path, divergent) and was only correct by luck of the message text.
// These prove the typed, read-back-based classification.

/// 1. Re-inserting a `DocumentRow` with an already-used `document_id`
///    must classify as `DuplicateDocument`, carrying the right id.
#[test]
fn insert_document_duplicate_id_is_typed_duplicate_document() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let doc_id = ContextId::new();
    let row = DocumentRow {
        document_id: doc_id,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: now_millis(),
        created_by: PrincipalId::system(),
    };
    db.insert_document(&row).unwrap();

    // Same document_id again — same benign PRIMARY KEY conflict that
    // `BlockStore::create_document` recovers from.
    let err = db.insert_document(&row).unwrap_err();
    match err {
        KernelDbError::DuplicateDocument(id) => assert_eq!(id, doc_id),
        other => panic!("expected DuplicateDocument({doc_id}), got: {other}"),
    }
}

/// 2. Guard: `idx_documents_path` is a *partial* unique index
///    (`WHERE path IS NOT NULL`) — two documents with `path: None` in the
///    same workspace must both insert cleanly. If someone "fixes" the index
///    into a full unique index, this must fail loudly.
#[test]
fn insert_document_two_null_paths_in_same_workspace_both_succeed() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    for _ in 0..2 {
        db.insert_document(&DocumentRow {
            document_id: ContextId::new(),
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        })
        .unwrap();
    }
}

// 3. The existing FK-violation test (test 22, `fk_violation_is_validation_error`)
// stays green — see that test above; not duplicated here.


fn file_doc_row(id: ContextId, ws_id: WorkspaceId) -> DocumentRow {
    DocumentRow {
        document_id: id,
        workspace_id: ws_id,
        doc_kind: DocKind::File,
        language: Some("rust".to_string()),
        path: None,
        created_at: now_millis(),
        created_by: PrincipalId::system(),
    }
}

#[test]
fn insert_document_with_op_commits_the_row_and_the_op() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let id = ContextId::new();

    db.insert_document_with_op(&file_doc_row(id, ws_id), 1, b"payload")
        .unwrap();

    assert!(db.get_document(id).unwrap().is_some(), "the row must be committed");
    assert_eq!(
        db.load_oplog_since(id, 0).unwrap(),
        vec![(1i64, b"payload".to_vec())],
        "the first op must be committed with the row"
    );
}

/// The direction that matters: a failing op insert must take the document
/// row down with it. Forced by planting an orphan oplog row (FKs off) at
/// the seq the insert will use, so the op hits the oplog PRIMARY KEY.
#[test]
fn insert_document_with_op_rolls_the_row_back_when_the_op_fails() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let id = ContextId::new();

    db.conn.execute("PRAGMA foreign_keys = OFF", []).unwrap();
    db.conn
        .execute(
            "INSERT INTO oplog (document_id, seq, payload) VALUES (?1, ?2, ?3)",
            params![blob_param(id.as_bytes()), 1i64, b"squatter".as_slice()],
        )
        .unwrap();
    db.conn.execute("PRAGMA foreign_keys = ON", []).unwrap();

    let err = db
        .insert_document_with_op(&file_doc_row(id, ws_id), 1, b"payload")
        .unwrap_err();

    assert!(
        db.get_document(id).unwrap().is_none(),
        "the document row must roll back with the failed op, got: {err}"
    );
}

/// The other direction: a duplicate document id fails before the op is
/// written, so no op lands for a row that was never created here.
#[test]
fn insert_document_with_op_writes_no_op_when_the_row_is_a_duplicate() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let id = ContextId::new();

    db.insert_document_with_op(&file_doc_row(id, ws_id), 1, b"first")
        .unwrap();
    let err = db
        .insert_document_with_op(&file_doc_row(id, ws_id), 2, b"second")
        .unwrap_err();

    assert!(
        matches!(err, KernelDbError::DuplicateDocument(dup) if dup == id),
        "expected DuplicateDocument({id}), got: {err}"
    );
    assert_eq!(
        db.load_oplog_since(id, 0).unwrap(),
        vec![(1i64, b"first".to_vec())],
        "the second call must leave no op behind"
    );
}

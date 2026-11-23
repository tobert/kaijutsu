use super::*;

fn setup_db_with_character(name: &str) -> (KernelDb, WorkspaceId, PrincipalId) {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let principal = PrincipalId::new();
    db.insert_character(&CharacterRow {
        principal_id: principal,
        name: name.to_string(),
        created_at: now_millis(),
        retired_at: None,
        handoff_ctx: None,
        root: false,
        root_ctx: None,
        rc_dir: None,
        console_ctx: None,
        mcp_hub_ctx: None,
    })
    .unwrap();
    (db, ws, principal)
}

fn make_topic_doc(db: &KernelDb, ws: WorkspaceId, created_by: PrincipalId) -> ContextId {
    let doc_id = ContextId::new();
    db.insert_document(&DocumentRow {
        document_id: doc_id,
        workspace_id: ws,
        doc_kind: DocKind::Topic,
        language: None,
        path: None,
        created_at: now_millis(),
        created_by,
    })
    .unwrap();
    doc_id
}

fn make_context(db: &KernelDb, ws: WorkspaceId, performer: PrincipalId, label: Option<&str>) -> ContextId {
    let mut row = make_context_row(label);
    row.performer = Some(performer);
    row.created_by = performer;
    let id = row.context_id;
    db.insert_context_with_document(&row, ws).unwrap();
    id
}

#[test]
fn character_board_crud_and_check_constraints() {
    let (db, _ws, char_id) = setup_db_with_character("librarian");

    // Character board without slug violates CHECK constraint.
    let bad_char_board = BoardRow {
        board_id: BoardId::new(),
        owner_character: Some(char_id),
        slug: None,
        description: String::new(),
        write_policy: BoardWritePolicy::Open,
        created_by: char_id,
        created_at: 1000,
        archived_at: None,
    };
    assert!(
        db.insert_board(&bad_char_board).is_err(),
        "character board without slug must fail CHECK"
    );

    // Context board with slug violates CHECK constraint.
    let bad_ctx_board = BoardRow {
        board_id: BoardId::new(),
        owner_character: None,
        slug: Some("illegal".to_string()),
        description: String::new(),
        write_policy: BoardWritePolicy::Open,
        created_by: char_id,
        created_at: 1000,
        archived_at: None,
    };
    assert!(
        db.insert_board(&bad_ctx_board).is_err(),
        "context board with slug must fail CHECK"
    );

    // Valid character board succeeds and round-trips.
    let kb_id = BoardId::new();
    let kb = BoardRow {
        board_id: kb_id,
        owner_character: Some(char_id),
        slug: Some("kb".to_string()),
        description: "House knowledge base".to_string(),
        write_policy: BoardWritePolicy::Curated,
        created_by: char_id,
        created_at: 2000,
        archived_at: None,
    };
    db.insert_board(&kb).unwrap();
    assert_eq!(db.get_board(kb_id).unwrap(), Some(kb.clone()));
    assert_eq!(
        db.get_character_board_by_slug(char_id, "kb").unwrap(),
        Some(kb.clone())
    );

    // Duplicate live slug for same character fails.
    let dup = BoardRow {
        board_id: BoardId::new(),
        ..kb.clone()
    };
    assert!(db.insert_board(&dup).is_err());

    // Update policy and description.
    db.update_board(kb_id, Some(BoardWritePolicy::Locked), Some("Frozen KB"))
        .unwrap();
    let updated = db.get_board(kb_id).unwrap().unwrap();
    assert_eq!(updated.write_policy, BoardWritePolicy::Locked);
    assert_eq!(updated.description, "Frozen KB");

    // Archive hides from default listing and frees slug for a new board.
    assert!(db.archive_board(kb_id).unwrap());
    assert!(!db.archive_board(kb_id).unwrap());
    assert_eq!(db.get_character_board_by_slug(char_id, "kb").unwrap(), None);
    assert!(db.list_character_boards(char_id, false).unwrap().is_empty());
    assert_eq!(db.list_character_boards(char_id, true).unwrap().len(), 1);

    // Reusing slug after archive succeeds.
    db.insert_board(&dup).unwrap();
    assert_eq!(
        db.get_character_board_by_slug(char_id, "kb")
            .unwrap()
            .unwrap()
            .board_id,
        dup.board_id
    );
}

#[test]
fn topic_slug_unique_among_non_archived_and_reusable_after_archive() {
    let (db, ws, char_id) = setup_db_with_character("sol");
    let board_id = BoardId::new();
    db.insert_board(&BoardRow {
        board_id,
        owner_character: Some(char_id),
        slug: Some("notes".to_string()),
        description: String::new(),
        write_policy: BoardWritePolicy::Open,
        created_by: char_id,
        created_at: 1000,
        archived_at: None,
    })
    .unwrap();

    let doc1 = make_topic_doc(&db, ws, char_id);
    let t1 = BoardTopicRow {
        document_id: doc1,
        board_id,
        slug: "ci-failures".to_string(),
        title: "CI failures".to_string(),
        status: TopicStatus::Open,
        locked: false,
        tags: vec!["ci".to_string(), "urgent".to_string()],
        promoted_from: None,
        created_by: char_id,
        created_at: 1000,
        last_post_at: 1000,
        post_count: 1,
    };
    db.insert_topic(&t1).unwrap();
    assert_eq!(db.get_topic(doc1).unwrap(), Some(t1.clone()));
    assert_eq!(
        db.get_topic_by_slug(board_id, "ci-failures").unwrap(),
        Some(t1.clone())
    );

    // Duplicate non-archived slug on the same board fails.
    let doc2 = make_topic_doc(&db, ws, char_id);
    let t2 = BoardTopicRow {
        document_id: doc2,
        ..t1.clone()
    };
    assert!(db.insert_topic(&t2).is_err());

    // Archiving t1 frees the slug for t2.
    db.set_topic_status(doc1, TopicStatus::Archived).unwrap();
    assert_eq!(db.get_topic_by_slug(board_id, "ci-failures").unwrap(), None);
    db.insert_topic(&t2).unwrap();
    assert_eq!(
        db.get_topic_by_slug(board_id, "ci-failures")
            .unwrap()
            .unwrap()
            .document_id,
        doc2
    );

    // Bump stats and lock/unlock.
    db.bump_topic_post_stats(doc2, 5000).unwrap();
    db.set_topic_locked(doc2, true).unwrap();
    let refreshed = db.get_topic(doc2).unwrap().unwrap();
    assert_eq!(refreshed.post_count, 2);
    assert_eq!(refreshed.last_post_at, 5000);
    assert!(refreshed.locked);

    // Prefix resolution by short prefix and full id.
    let by_full = db.resolve_topic_prefix(&doc2.to_hex()).unwrap();
    assert_eq!(by_full.document_id, doc2);
    let by_short = db.resolve_topic_prefix(&doc2.short()).unwrap();
    assert_eq!(by_short.document_id, doc2);
}

#[test]
fn ensure_context_board_is_lazy_and_idempotent() {
    let (db, ws, char_id) = setup_db_with_character("banto");
    let ctx = make_context(&db, ws, char_id, Some("work"));

    assert_eq!(db.get_context(ctx).unwrap().unwrap().board_id, None);

    let b1 = db.ensure_context_board(ctx, char_id).unwrap();
    assert_eq!(b1.owner_character, None);
    assert_eq!(b1.slug, None);
    assert_eq!(b1.write_policy, BoardWritePolicy::Open);
    assert_eq!(db.get_context(ctx).unwrap().unwrap().board_id, Some(b1.board_id));
    assert_eq!(db.count_live_contexts_for_board(b1.board_id).unwrap(), 1);

    // Second call returns the same board without creating another.
    let b2 = db.ensure_context_board(ctx, char_id).unwrap();
    assert_eq!(b1.board_id, b2.board_id);
}

#[test]
fn rotation_carries_board_id_and_keeps_board_live_until_final_context_archives() {
    let (db, ws, char_id) = setup_db_with_character("banto");
    let pred = make_context(&db, ws, char_id, Some("banto-main"));
    let succ = make_context(&db, ws, char_id, None);

    let board = db.ensure_context_board(pred, char_id).unwrap();

    // Rotate pred -> succ.
    db.commit_rotation(pred, succ).unwrap();

    let pred_row = db.get_context(pred).unwrap().unwrap();
    let succ_row = db.get_context(succ).unwrap().unwrap();
    // Predecessor is archived but keeps its board_id pointer for kohai.
    assert!(pred_row.is_archived());
    assert_eq!(pred_row.board_id, Some(board.board_id));
    // Successor received the same board_id and is live.
    assert!(!succ_row.is_archived());
    assert_eq!(succ_row.board_id, Some(board.board_id));
    assert_eq!(db.count_live_contexts_for_board(board.board_id).unwrap(), 1);
    // Board itself stays live!
    assert_eq!(db.get_board(board.board_id).unwrap().unwrap().archived_at, None);

    // Archiving the final live context (succ) archives the board.
    assert!(db.archive_context(succ).unwrap());
    assert_eq!(db.count_live_contexts_for_board(board.board_id).unwrap(), 0);
    assert!(
        db.get_board(board.board_id)
            .unwrap()
            .unwrap()
            .archived_at
            .is_some(),
        "board must be archived once no live context references it"
    );
}

#[test]
fn retiring_character_archives_character_boards_and_context_boards() {
    let (db, ws, char_id) = setup_db_with_character("scout");
    let char_board_id = BoardId::new();
    db.insert_board(&BoardRow {
        board_id: char_board_id,
        owner_character: Some(char_id),
        slug: Some("memory".to_string()),
        description: String::new(),
        write_policy: BoardWritePolicy::Curated,
        created_by: char_id,
        created_at: 1000,
        archived_at: None,
    })
    .unwrap();

    let ctx = make_context(&db, ws, char_id, Some("scout-ctx"));
    let ctx_board = db.ensure_context_board(ctx, char_id).unwrap();

    let retired = db.retire_with_contexts(char_id, now_millis()).unwrap();
    assert!(retired.is_some());

    assert!(
        db.get_board(char_board_id)
            .unwrap()
            .unwrap()
            .archived_at
            .is_some(),
        "character board must be archived on character retire"
    );
    assert!(
        db.get_board(ctx_board.board_id)
            .unwrap()
            .unwrap()
            .archived_at
            .is_some(),
        "context board must be archived when character's final context archives"
    );
}

#[test]
fn list_topics_recent_and_topic_ids_filtering() {
    let (db, ws, char_id) = setup_db_with_character("librarian");
    let b1 = BoardId::new();
    let b2 = BoardId::new();
    for (bid, slug) in [(b1, "kb"), (b2, "bugs")] {
        db.insert_board(&BoardRow {
            board_id: bid,
            owner_character: Some(char_id),
            slug: Some(slug.to_string()),
            description: String::new(),
            write_policy: BoardWritePolicy::Open,
            created_by: char_id,
            created_at: 1000,
            archived_at: None,
        })
        .unwrap();
    }

    let d_open = make_topic_doc(&db, ws, char_id);
    let d_pinned = make_topic_doc(&db, ws, char_id);
    let d_resolved = make_topic_doc(&db, ws, char_id);
    let d_archived = make_topic_doc(&db, ws, char_id);
    let d_other = make_topic_doc(&db, ws, char_id);

    let topics = [
        (d_open, b1, "open-one", TopicStatus::Open, vec!["rust", "db"], 3000),
        (d_pinned, b1, "pinned-one", TopicStatus::Pinned, vec!["rust"], 1000),
        (d_resolved, b1, "resolved-one", TopicStatus::Resolved, vec!["db"], 2000),
        (d_archived, b1, "archived-one", TopicStatus::Archived, vec!["rust"], 4000),
        (d_other, b2, "bug-one", TopicStatus::Open, vec!["rust"], 5000),
    ];
    for (doc_id, board_id, slug, status, tags, ts) in topics {
        db.insert_topic(&BoardTopicRow {
            document_id: doc_id,
            board_id,
            slug: slug.to_string(),
            title: slug.to_string(),
            status,
            locked: false,
            tags: tags.into_iter().map(String::from).collect(),
            promoted_from: None,
            created_by: char_id,
            created_at: ts,
            last_post_at: ts,
            post_count: 1,
        })
        .unwrap();
    }

    // list_topics: pinned sorts first, then last_post_at DESC.
    let all_b1 = db.list_topics(b1, None, None, None).unwrap();
    assert_eq!(all_b1.len(), 4);
    assert_eq!(all_b1[0].document_id, d_pinned);

    // Filter by status.
    let only_open = db.list_topics(b1, Some(TopicStatus::Open), None, None).unwrap();
    assert_eq!(only_open.len(), 1);
    assert_eq!(only_open[0].document_id, d_open);

    // Filter by tag + limit.
    let tag_rust = db.list_topics(b1, None, Some("rust"), Some(2)).unwrap();
    assert_eq!(tag_rust.len(), 2);
    assert_eq!(tag_rust[0].document_id, d_pinned);
    assert_eq!(tag_rust[1].document_id, d_archived);

    // list_recent_topics excludes archived, respects board_filter, since_ms, limit.
    let recent_all = db.list_recent_topics(None, None, None).unwrap();
    let recent_ids: Vec<ContextId> = recent_all.iter().map(|t| t.document_id).collect();
    assert_eq!(recent_ids, vec![d_other, d_open, d_resolved, d_pinned]);

    let recent_since = db.list_recent_topics(None, Some(2500), Some(1)).unwrap();
    assert_eq!(recent_since.len(), 1);
    assert_eq!(recent_since[0].document_id, d_other);

    let recent_b1 = db.list_recent_topics(Some(b1), Some(2000), None).unwrap();
    assert_eq!(
        recent_b1.iter().map(|t| t.document_id).collect::<Vec<_>>(),
        vec![d_open, d_resolved]
    );

    // list_topic_ids excludes archived and filters by board.
    assert_eq!(
        db.list_topic_ids(None).unwrap(),
        vec![d_other, d_open, d_resolved, d_pinned]
    );
    assert_eq!(
        db.list_topic_ids(Some(b1)).unwrap(),
        vec![d_open, d_resolved, d_pinned]
    );
    assert_eq!(db.list_topic_ids(Some(b2)).unwrap(), vec![d_other]);
}

#[test]
fn legacy_db_without_contexts_board_id_migrates_cleanly_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let ctx_id = {
        let db = KernelDb::open(&path).unwrap();
        let ws = setup_test_db(&db);
        let row = make_context_row(Some("legacy-ctx"));
        let id = row.context_id;
        db.insert_context_with_document(&row, ws).unwrap();
        // Simulate a pre-boards kernel.db: no idx_contexts_board and no contexts.board_id column.
        db.conn
            .execute_batch(
                "DROP INDEX IF EXISTS idx_contexts_board;
                 ALTER TABLE contexts DROP COLUMN board_id;",
            )
            .unwrap();
        id
    };

    // Reopening must run SCHEMA + apply_additive_migrations cleanly without failing on missing board_id.
    let db = KernelDb::open(&path).expect("pre-boards kernel.db must migrate cleanly on open");
    let has_idx: bool = db
        .conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'idx_contexts_board')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(has_idx, "idx_contexts_board must be created after additive migration");
    let loaded = db.get_context(ctx_id).unwrap().expect("legacy context row must survive");
    assert_eq!(loaded.board_id, None);
}

#[test]
fn archived_boards_excluded_from_recent_and_unfiltered_topic_ids() {
    let (db, ws, char_id) = setup_db_with_character("librarian");
    let live_board = BoardId::new();
    let archived_board = BoardId::new();
    for (bid, slug) in [(live_board, "live"), (archived_board, "retired")] {
        db.insert_board(&BoardRow {
            board_id: bid,
            owner_character: Some(char_id),
            slug: Some(slug.to_string()),
            description: String::new(),
            write_policy: BoardWritePolicy::Open,
            created_by: char_id,
            created_at: 1000,
            archived_at: None,
        })
        .unwrap();
    }

    let d_live = make_topic_doc(&db, ws, char_id);
    let d_retired = make_topic_doc(&db, ws, char_id);
    for (doc_id, board_id, slug, ts) in [
        (d_live, live_board, "live-topic", 2000),
        (d_retired, archived_board, "retired-topic", 3000),
    ] {
        db.insert_topic(&BoardTopicRow {
            document_id: doc_id,
            board_id,
            slug: slug.to_string(),
            title: slug.to_string(),
            status: TopicStatus::Open,
            locked: false,
            tags: vec![],
            promoted_from: None,
            created_by: char_id,
            created_at: ts,
            last_post_at: ts,
            post_count: 1,
        })
        .unwrap();
    }

    db.archive_board(archived_board).unwrap();

    let recent_all = db.list_recent_topics(None, None, None).unwrap();
    assert_eq!(
        recent_all.iter().map(|t| t.document_id).collect::<Vec<_>>(),
        vec![d_live],
        "archived board's topics must not appear in unfiltered list_recent_topics"
    );
    assert_eq!(
        db.list_topic_ids(None).unwrap(),
        vec![d_live],
        "archived board's topics must not appear in unfiltered list_topic_ids"
    );
}

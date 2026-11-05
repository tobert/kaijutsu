use super::*;

/// `origin_host` is deliberately NOT part of `write_context`'s own
/// INSERT (see the field's doc comment) — it is set via the dedicated
/// `set_origin_host`, exactly like `update_label`. Round-trip through
/// both: a context created with no host known, then stamped after the
/// fact, ends up with the value `get_context` reads back — and does NOT
/// silently vanish or get coerced to empty-string (which would break the
/// "-" display fallback, since `Some("")` and `None` render identically
/// but mean different things to a caller checking `.is_some()`).
#[test]
fn set_origin_host_round_trips_through_get_context() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("origin-host-set"));
    insert_context_with_doc(&db, &row, ws_id);

    // Freshly created: no host reported yet.
    assert_eq!(db.get_context(row.context_id).unwrap().unwrap().origin_host, None);

    db.set_origin_host(row.context_id, Some("zorak")).unwrap();
    assert_eq!(
        db.get_context(row.context_id).unwrap().unwrap().origin_host,
        Some("zorak".to_string()),
        "set_origin_host must be visible on the very next get_context read"
    );

    // Clearing (host = None) must also round-trip, not merely no-op.
    db.set_origin_host(row.context_id, None).unwrap();
    assert_eq!(
        db.get_context(row.context_id).unwrap().unwrap().origin_host,
        None,
        "set_origin_host(None) must clear a previously-set value"
    );
}

/// `set_origin_host` on an unknown context must fail loudly
/// (`NotFound`), same convention as `update_label`
/// — never a silent no-op that could be mistaken for success.
#[test]
fn set_origin_host_unknown_context_is_not_found() {
    let db = KernelDb::temporary().unwrap();
    let err = db.set_origin_host(ContextId::new(), Some("moltar")).unwrap_err();
    assert!(matches!(err, KernelDbError::NotFound(_)), "got: {err:?}");
}

/// `list_active_contexts`/`list_all_contexts`/`fork_children`/
/// `context_dag`/`fork_lineage`/`find_context_by_label` all share the
/// same `contexts` column list — `context_dag` must carry `origin_host`
/// through to its rows.
#[test]
fn origin_host_survives_the_context_dag_cte_read() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("origin-host-dag"));
    insert_context_with_doc(&db, &row, ws_id);
    db.set_origin_host(row.context_id, Some("moltar")).unwrap();

    let dag = db.context_dag().unwrap();
    let (found, depth) = dag
        .into_iter()
        .find(|(r, _)| r.context_id == row.context_id)
        .expect("row must appear in the DAG (it's a root — no forked_from)");
    assert_eq!(found.origin_host, Some("moltar".to_string()));
    assert_eq!(depth, 0, "a root with no parent is depth 0");
}

/// `touch_context_activity` stamps `last_activity_at`, and pre-touch rows
/// read back `None` (not silently defaulted) — the list SELECTs are what
/// backfill via COALESCE, not this row-level accessor.
#[test]
fn touch_context_activity_stamps_and_advances() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("touch-me"));
    let cid = row.context_id;
    let created_at = row.created_at;
    insert_context_with_doc(&db, &row, ws_id);

    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.last_activity_at, None);

    let t0 = created_at + 1000;
    db.touch_context_activity(cid, t0).unwrap();
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.last_activity_at, Some(t0));
    assert!(loaded.last_activity_at.unwrap() > created_at);

    // A second touch advances further (not a first-write-wins latch like
    // concluded_at — activity is repeatedly re-stamped).
    let t1 = t0 + 1000;
    db.touch_context_activity(cid, t1).unwrap();
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.last_activity_at, Some(t1));
}

#[test]
fn context_lifecycle() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("main"));
    let cid = row.context_id;

    insert_context_with_doc(&db, &row, ws_id);

    // Get
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.label, Some("main".into()));

    // Update label
    db.update_label(cid, Some("primary")).unwrap();
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.label, Some("primary".into()));

    // Archive
    assert!(db.archive_context(cid).unwrap());

    // list_active excludes
    let active = db.list_active_contexts().unwrap();
    assert!(active.is_empty());

    // list_all includes
    let all = db.list_all_contexts().unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].archived_at.is_some());
}

#[test]
fn label_uniqueness() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let row1 = make_context_row(Some("shared"));
    insert_context_with_doc(&db, &row1, ws_id);

    // Same label → conflict (doc insert ok, context label conflicts).
    // Labels are globally unique within the kernel.
    let row2 = make_context_row(Some("shared"));
    db.insert_document(&DocumentRow {
        document_id: row2.context_id,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: row2.created_at,
        created_by: row2.created_by,
    })
    .unwrap();
    let err = db.insert_context(&row2).unwrap_err();
    assert!(matches!(err, KernelDbError::LabelConflict(_)));

    // NULL + NULL → OK (multiple unlabeled contexts allowed)
    let n1 = make_context_row(None);
    let n2 = make_context_row(None);
    insert_context_with_doc(&db, &n1, ws_id);
    insert_context_with_doc(&db, &n2, ws_id);
}

#[test]
fn tag_resolution() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let ctx1 = make_context_row(Some("opusplan"));
    let ctx2 = make_context_row(Some("sonnet"));
    insert_context_with_doc(&db, &ctx1, ws_id);
    insert_context_with_doc(&db, &ctx2, ws_id);

    // Exact label match
    let resolved = db.resolve_context("opusplan").unwrap();
    assert_eq!(resolved, ctx1.context_id);

    // Prefix match
    let resolved = db.resolve_context("opus").unwrap();
    assert_eq!(resolved, ctx1.context_id);
}

#[test]
fn resolve_context_basic() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let ctx = make_context_row(Some("unique-label"));
    insert_context_with_doc(&db, &ctx, ws_id);

    // Exact label
    let r = db.resolve_context("unique-label").unwrap();
    assert_eq!(r, ctx.context_id);

    // Label prefix
    let r = db.resolve_context("unique").unwrap();
    assert_eq!(r, ctx.context_id);

    // Short id (entropy tail)
    let r = db.resolve_context(&ctx.context_id.short()).unwrap();
    assert_eq!(r, ctx.context_id);

    // Not found
    let err = db.resolve_context("nonexistent").unwrap_err();
    assert!(matches!(err, KernelDbError::NotFound(_)));
}

#[test]
fn null_labels_coexist() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    // Insert 5 contexts with NULL label — all succeed
    for _ in 0..5 {
        let row = make_context_row(None);
        insert_context_with_doc(&db, &row, ws_id);
    }

    let all = db.list_all_contexts().unwrap();
    assert_eq!(all.len(), 5);
}

#[test]
fn archive_excludes_from_active() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let parent = make_context_row(Some("parent"));
    let mut child = make_context_row(Some("child"));
    child.forked_from = Some(parent.context_id);
    insert_context_with_doc(&db, &parent, ws_id);
    insert_context_with_doc(&db, &child, ws_id);

    // Before archive
    let children = db.fork_children(parent.context_id).unwrap();
    assert_eq!(children.len(), 1);

    // Archive child
    db.archive_context(child.context_id).unwrap();

    // list_active excludes
    let active = db.list_active_contexts().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].context_id, parent.context_id);

    // fork_children excludes archived
    let children = db.fork_children(parent.context_id).unwrap();
    assert!(children.is_empty());
}

#[test]
fn context_row_to_context() {
    let creator = PrincipalId::new();
    let parent = ContextId::new();
    let now = now_millis();

    let row = ContextRow {
        context_id: ContextId::new(),
        label: Some("test".into()),
        system_prompt: None,
        context_state: ContextState::Live,
        created_at: now,
        created_by: creator,
        forked_from: Some(parent),
        fork_kind: Some(ForkKind::Full),
        archived_at: None,
        workspace_id: None,
        preset_id: None,
        concluded_at: None,
        last_activity_at: None,
        promoted_at: None,
        demoted_at: None,
        paused_at: None,
        origin_host: None,
        performer: None, senpai_ctx: None, persona: None,
    };

    let ctx = row.to_context();
    assert_eq!(ctx.id, row.context_id);
    assert_eq!(ctx.label, Some("test".into()));
    assert_eq!(ctx.forked_from, Some(parent));
    assert_eq!(ctx.created_by, creator);
    assert_eq!(ctx.created_at, now as u64);
}

#[test]
fn roundtrip_create_and_recover() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let creator = PrincipalId::new();
    let parent_id = ContextId::new();

    // Insert parent first (forked_from FK requires it to exist)
    let parent = ContextRow {
        context_id: parent_id,
        label: Some("parent".into()),
        system_prompt: Some("You are helpful.".into()),
        context_state: ContextState::Live,
        created_at: 1000,
        created_by: creator,
        forked_from: None,
        fork_kind: None,
        archived_at: None,
        workspace_id: None,
        preset_id: None,
        concluded_at: None,
        last_activity_at: None,
        promoted_at: None,
        demoted_at: None,
        paused_at: None,
        origin_host: None,
        performer: None, senpai_ctx: None, persona: None,
    };
    insert_context_with_doc(&db, &parent, ws_id);

    // Insert child forked from parent.
    let child_id = ContextId::new();
    let child = ContextRow {
        context_id: child_id,
        label: Some("child-fork".into()),
        system_prompt: Some("Be concise.".into()),
        context_state: ContextState::Live,
        created_at: 2000,
        created_by: creator,
        forked_from: Some(parent_id),
        fork_kind: Some(ForkKind::Full),
        archived_at: None,
        workspace_id: None,
        preset_id: None,
        concluded_at: None,
        last_activity_at: None,
        promoted_at: None,
        demoted_at: None,
        paused_at: None,
        origin_host: None,
        performer: None, senpai_ctx: None, persona: None,
    };
    insert_context_with_doc(&db, &child, ws_id);

    // Read back and verify all 16 fields
    let recovered = db.get_context(child_id).unwrap().expect("child not found");
    assert_eq!(recovered.context_id, child_id);
    assert_eq!(recovered.label, Some("child-fork".into()));
    assert_eq!(recovered.system_prompt, Some("Be concise.".into()));
    assert_eq!(recovered.created_at, 2000);
    assert_eq!(recovered.created_by, creator);
    assert_eq!(recovered.forked_from, Some(parent_id));
    assert_eq!(recovered.fork_kind, Some(ForkKind::Full));
    assert!(recovered.archived_at.is_none());
    assert!(recovered.workspace_id.is_none());
    assert!(recovered.preset_id.is_none());
    assert_eq!(recovered.performer, None, "NULL played_by preserves today's behavior");

    // Verify list_active_contexts returns both
    let active = db.list_active_contexts().unwrap();
    assert_eq!(active.len(), 2);

    // Verify to_context() preserves forked_from
    let ctx = recovered.to_context();
    assert_eq!(ctx.forked_from, Some(parent_id));
    assert_eq!(ctx.created_by, creator);

    // A fixture's performer is the principal of the context's context
    // character.
    let character = PrincipalId::new();
    db.update_performer(child_id, Some(character)).unwrap();
    let played = db.get_context(child_id).unwrap().unwrap();
    assert_eq!(played.performer, Some(character));
    let context_character = db.context_character(child_id).unwrap().expect("a performed context has a context character");
    assert_eq!(context_character.principal_id, character);
    db.update_performer(child_id, Some(character)).unwrap();
    assert_eq!(db.context_character(child_id).unwrap().unwrap(), context_character, "the same character changes nothing");
    assert!(db.update_performer(child_id, None).is_err(), "a performer cannot be cleared");
    assert_eq!(db.get_context(child_id).unwrap().unwrap().performer, Some(character));
}

#[test]
fn resolve_context_full_id_reaches_an_archived_context() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("ghost"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);
    assert!(db.archive_context(cid).unwrap());

    // Fuzzy forms stay scoped to the active set — the label is gone…
    let err = db.resolve_context("ghost").unwrap_err();
    assert!(matches!(err, KernelDbError::NotFound(_)));

    // …but the exact full id is the resurrection handle.
    let hyphenated = cid.to_string();
    assert_eq!(db.resolve_context(&hyphenated).unwrap(), cid);
    let bare_hex = hyphenated.replace('-', "");
    assert_eq!(db.resolve_context(&bare_hex).unwrap(), cid);
}

/// `write_context` must persist every field the `ContextRow` carries —
/// including the time-well placement stamps and `last_activity_at`. Today
/// every insert caller passes `None` for all four, but an INSERT that
/// omits the columns would silently drop values any future caller sets.
#[test]
fn insert_context_round_trips_placement_stamps() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let mut row = make_context_row(Some("stamped-at-birth"));
    row.last_activity_at = Some(1_111);
    row.promoted_at = Some(2_222);
    row.demoted_at = None; // promoted/demoted are mutually exclusive
    row.paused_at = Some(4_444);
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.last_activity_at, Some(1_111));
    assert_eq!(loaded.promoted_at, Some(2_222));
    assert_eq!(loaded.demoted_at, None);
    assert_eq!(loaded.paused_at, Some(4_444));
}

/// The list SELECTs order by `COALESCE(last_activity_at, created_at)` —
/// prove the recency competition actually reads `last_activity_at` when
/// it's set (the other list-order tests always leave it `None`, so they
/// only ever exercise the `created_at` fallback).
#[test]
fn list_orders_by_last_activity_over_created_at() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    // `old` was created first but touched most recently; `new` was
    // created later and never touched.
    let mut old = make_context_row(Some("old-but-hot"));
    old.created_at = 1_000;
    let mut new = make_context_row(Some("new-but-idle"));
    new.created_at = 2_000;
    insert_context_with_doc(&db, &old, ws_id);
    insert_context_with_doc(&db, &new, ws_id);
    db.touch_context_activity(old.context_id, 3_000).unwrap();

    let active = db.list_active_contexts().unwrap();
    let order: Vec<ContextId> = active.iter().map(|c| c.context_id).collect();
    assert_eq!(
        order,
        vec![new.context_id, old.context_id],
        "ascending COALESCE order: new (created 2000, untouched) before \
         old (touched 3000) — last_activity_at must beat created_at"
    );
}

#[test]
fn an_archived_context_keeps_its_label_and_frees_it() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let old = make_context_row(Some("console"));
    insert_context_with_doc(&db, &old, ws_id);
    assert!(db.archive_context(old.context_id).unwrap());

    let new = make_context_row(Some("console"));
    insert_context_with_doc(&db, &new, ws_id);

    let archived = db.get_context(old.context_id).unwrap().unwrap();
    assert_eq!(archived.label.as_deref(), Some("console"), "the archived row lost its name");
    let holder = db.find_context_by_label("console").unwrap().expect("the live holder");
    assert_eq!(holder.context_id, new.context_id);
}

#[test]
fn two_live_contexts_still_cannot_share_a_label() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    insert_context_with_doc(&db, &make_context_row(Some("console")), ws_id);
    let dup = make_context_row(Some("console"));
    let err = db
        .insert_context_with_document(&dup, ws_id)
        .expect_err("a second live holder of one label must be refused");
    assert!(err.to_string().contains("already in use"), "{err}");
}

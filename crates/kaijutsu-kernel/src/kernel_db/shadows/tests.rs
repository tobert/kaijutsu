use super::*;

/// A source context, and a child of it that answers to it, in a fresh database.
fn fixture() -> (KernelDb, ContextRow, ContextRow) {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let source = make_context_row(Some("source"));
    insert_context_with_doc(&db, &source, ws);
    let shadow = shadow_of(&db, &source, "shadow");
    (db, source, shadow)
}

/// A child of `source` that answers to it, as a shadow does.
fn shadow_of(db: &KernelDb, source: &ContextRow, label: &str) -> ContextRow {
    let ws = db.get_or_create_default_workspace(PrincipalId::system()).unwrap();
    let mut row = make_context_row(Some(label));
    row.forked_from = Some(source.context_id);
    insert_context_with_doc(db, &row, ws);
    db.conn
        .execute(
            "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(source.context_id.as_bytes()), blob_param(row.context_id.as_bytes())],
        )
        .unwrap();
    db.get_context(row.context_id).unwrap().unwrap()
}

fn block(context: ContextId, seq: u64) -> BlockId {
    BlockId::new(context, PrincipalId::system(), seq)
}

/// A shadow has the kind it was marked with, and a context that is no shadow has none.
///
/// Falsified by reading the kind from the first row of any kind.
#[test]
fn a_shadow_has_the_kind_it_was_marked_with() {
    let (db, source, shadow) = fixture();
    let judge = shadow_of(&db, &source, "judge");
    db.mark_shadow(shadow.context_id, DESCRIBE).unwrap();
    db.mark_judge_shadow(judge.context_id).unwrap();
    assert_eq!(db.shadow_kind(&shadow).unwrap().as_deref(), Some(DESCRIBE));
    assert_eq!(db.shadow_kind(&judge).unwrap().as_deref(), Some(JUDGE));
    assert_eq!(db.shadow_kind(&source).unwrap(), None);
    assert!(db.is_judge_shadow(&judge).unwrap());
    assert!(!db.is_judge_shadow(&shadow).unwrap(), "a describe shadow is no judge shadow");
}

/// A row that answers to a context other than its parent is no shadow, marked or not.
#[test]
fn a_shadow_answers_to_its_parent() {
    let (db, source, shadow) = fixture();
    db.mark_shadow(shadow.context_id, DESCRIBE).unwrap();
    let other = shadow_of(&db, &source, "other");
    db.conn
        .execute(
            "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(other.context_id.as_bytes()), blob_param(shadow.context_id.as_bytes())],
        )
        .unwrap();
    let moved = db.get_context(shadow.context_id).unwrap().unwrap();
    assert_eq!(db.shadow_kind(&moved).unwrap(), None);
}

/// Archiving a source archives every live shadow of it, whatever its kind, in
/// the transaction that archives the source; a child that is no shadow stays live.
///
/// Falsified by archiving only the judge kind.
#[test]
fn archiving_a_source_archives_all_its_shadows() {
    let (db, source, describe) = fixture();
    let judge = shadow_of(&db, &source, "judge");
    let plain = shadow_of(&db, &source, "plain");
    db.mark_shadow(describe.context_id, DESCRIBE).unwrap();
    db.mark_judge_shadow(judge.context_id).unwrap();
    let archived = db.archive_with_shadows(source.context_id).unwrap();
    assert!(archived.context);
    let mut got = archived.shadows.clone();
    got.sort();
    let mut want = vec![describe.context_id, judge.context_id];
    want.sort();
    assert_eq!(got, want);
    for id in want {
        assert!(db.get_context(id).unwrap().unwrap().archived_at.is_some());
    }
    assert!(db.get_context(plain.context_id).unwrap().unwrap().archived_at.is_none());
}

/// A source has at most one live shadow of a kind to find, and the finder
/// skips an archived one.
#[test]
fn the_live_shadow_of_a_kind_is_found_by_its_source() {
    let (db, source, describe) = fixture();
    db.mark_shadow(describe.context_id, DESCRIBE).unwrap();
    assert_eq!(db.live_shadow(source.context_id, DESCRIBE).unwrap().unwrap().context_id, describe.context_id);
    assert!(db.live_shadow(source.context_id, JUDGE).unwrap().is_none());
    db.archive_with_shadows(source.context_id).unwrap();
    assert!(db.live_shadow(source.context_id, DESCRIBE).unwrap().is_none());
}

/// A rendition link names its source block and leaves the span empty, and the
/// index finds the rendition by content, kind, and maker.
///
/// Falsified by a lookup that ignores the maker.
#[test]
fn a_rendition_is_linked_and_indexed_by_content_kind_and_maker() {
    let (db, source, shadow) = fixture();
    db.mark_shadow(shadow.context_id, DESCRIBE).unwrap();
    let rendition = block(shadow.context_id, 1);
    let from = block(source.context_id, 7);
    let key = IndexKey::new("abc", DESCRIBE, "ds/flash");
    db.record_rendition(&Rendition { block: rendition, source: from, span: None }, Some(&key)).unwrap();
    assert_eq!(db.rendition_count(shadow.context_id).unwrap(), 1);
    let links = db.renditions_of(shadow.context_id).unwrap();
    assert_eq!(links, vec![Rendition { block: rendition, source: from, span: None }]);
    assert_eq!(db.indexed_rendition(&key).unwrap(), Some(rendition));
    assert_eq!(db.indexed_rendition(&IndexKey::new("abc", DESCRIBE, "ds/other")).unwrap(), None);
    assert_eq!(db.indexed_rendition(&IndexKey::new("abd", DESCRIBE, "ds/flash")).unwrap(), None);
    let (span_start, span_end): (Option<i64>, Option<i64>) = db
        .conn
        .query_row("SELECT span_start, span_end FROM shadow_renditions", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((span_start, span_end), (None, None));
}

/// A second rendition under the same key is refused: the index holds one
/// result for a key, and the first stays.
#[test]
fn a_key_names_one_rendition() {
    let (db, source, shadow) = fixture();
    db.mark_shadow(shadow.context_id, DESCRIBE).unwrap();
    let key = IndexKey::new("abc", DESCRIBE, "ds/flash");
    let from = block(source.context_id, 7);
    db.record_rendition(&Rendition { block: block(shadow.context_id, 1), source: from, span: None }, Some(&key)).unwrap();
    let error = db
        .record_rendition(&Rendition { block: block(shadow.context_id, 2), source: from, span: None }, Some(&key))
        .unwrap_err();
    assert!(error.to_string().to_lowercase().contains("unique") || error.to_string().contains("constraint"), "{error}");
    assert_eq!(db.rendition_count(shadow.context_id).unwrap(), 1, "the refused link is not left behind");
    assert_eq!(db.indexed_rendition(&key).unwrap(), Some(block(shadow.context_id, 1)));
}

/// The link and the index are rows, so they survive closing and opening the file.
///
/// Falsified by keeping the links in memory.
#[test]
fn renditions_survive_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (source_id, shadow_id) = {
        let db = KernelDb::open(&path).unwrap();
        let ws = setup_test_db(&db);
        let source = make_context_row(Some("source"));
        insert_context_with_doc(&db, &source, ws);
        let shadow = shadow_of(&db, &source, "shadow");
        db.mark_shadow(shadow.context_id, DESCRIBE).unwrap();
        db.record_rendition(
            &Rendition { block: block(shadow.context_id, 1), source: block(source.context_id, 7), span: None },
            Some(&IndexKey::new("abc", DESCRIBE, "ds/flash")),
        )
        .unwrap();
        (source.context_id, shadow.context_id)
    };
    let db = KernelDb::open(&path).unwrap();
    assert_eq!(db.rendition_count(shadow_id).unwrap(), 1);
    assert_eq!(
        db.indexed_rendition(&IndexKey::new("abc", DESCRIBE, "ds/flash")).unwrap(),
        Some(block(shadow_id, 1))
    );
    assert_eq!(db.renditions_of(shadow_id).unwrap()[0].source, block(source_id, 7));
}

/// A description shadow takes no chat turn, and says what it is for. A judge
/// shadow and the source keep the rule they had.
///
/// Falsified by refusing every shadow, or none.
#[test]
fn a_description_shadow_takes_no_chat_turn() {
    let (db, source, describe) = fixture();
    let judge = shadow_of(&db, &source, "judge");
    db.mark_shadow(describe.context_id, DESCRIBE).unwrap();
    db.mark_judge_shadow(judge.context_id).unwrap();
    let why = db.chat_turn_refusal(describe.context_id).unwrap().expect("refused");
    assert!(why.contains("description shadow") && why.contains("kj shadow describe"), "{why}");
    assert_eq!(db.chat_turn_refusal(judge.context_id).unwrap(), None);
    assert_eq!(db.chat_turn_refusal(source.context_id).unwrap(), None);
}

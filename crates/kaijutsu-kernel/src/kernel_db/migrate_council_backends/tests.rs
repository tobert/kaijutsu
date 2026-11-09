use super::*;

fn forget(db: &KernelDb) {
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::COUNCIL_BACKENDS_MIGRATION]).unwrap();
}

fn recorded(db: &KernelDb) -> bool {
    db.conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [KernelDb::COUNCIL_BACKENDS_MIGRATION],
            |r| r.get(0),
        )
        .unwrap()
}

fn kind(db: &KernelDb, backend: BackendId) -> String {
    db.get_backend(backend).unwrap().unwrap().kind
}

fn marked(db: &KernelDb, id: ContextId) -> bool {
    db.conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM judge_shadows WHERE context_id = ?1)",
            params![blob_param(id.as_bytes())],
            |r| r.get(0),
        )
        .unwrap()
}

/// Insert a backend row as an older database holds it: raw SQL, since
/// `upsert_backend` refuses the retired kind `mk`.
fn old_backend(db: &KernelDb, name: &str, kind: &str) -> BackendId {
    let id = BackendId::new();
    db.conn.execute(
        "INSERT INTO backends (backend_id, name, kind, key_optional, created_by) VALUES (?1, ?2, ?3, 1, ?4)",
        params![blob_param(id.as_bytes()), name, kind, blob_param(PrincipalId::system().as_bytes())],
    ).unwrap();
    id
}

/// A database from before `council-backends-v1`: an `mk` backend, a judge
/// whose model is on it, a seat, the judge's shadow of the seat (a child that
/// answers to it), and a context the judge performs that answers elsewhere.
struct Old {
    db: KernelDb,
    mk: BackendId,
    deepseek: BackendId,
    shadow: ContextId,
    elsewhere: ContextId,
    chat_child: ContextId,
}

fn old() -> Old {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let mk = old_backend(&db, "mk-zorak", "mk");
    let deepseek = old_backend(&db, "deepseek", "deepseek");
    let character = |name: &str, backend: BackendId| {
        let principal_id = PrincipalId::new();
        db.insert_character(&CharacterRow {
            rc_dir: Some(name.into()), console_ctx: None, mcp_hub_ctx: None, principal_id, name: name.into(),
            created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        db.set_character_model(&CharacterModelRow {
            principal_id, backend_id: backend, model: "m".into(), max_tokens: Some(512), temperature: None,
            top_p: None, effort: None, thinking_budget: None, thinking_style: None,
        }).unwrap();
        principal_id
    };
    let judge = character("judge", mk);
    let coder = character("coder", deepseek);
    let context = |label: &str, parent: Option<ContextId>, senpai: Option<ContextId>, performer: PrincipalId| {
        let mut row = make_context_row(Some(label));
        row.forked_from = parent;
        insert_context_with_doc(&db, &row, ws);
        db.conn.execute(
            "UPDATE contexts SET performer = ?1, senpai_ctx = ?2 WHERE context_id = ?3",
            params![blob_param(performer.as_bytes()), senpai.map(|s| s.as_bytes().to_vec()), blob_param(row.context_id.as_bytes())],
        ).unwrap();
        row.context_id
    };
    let seat = context("seat", None, None, coder);
    let other = context("other", None, None, coder);
    let shadow = context("judge-seat", Some(seat), Some(seat), judge);
    let elsewhere = context("judge-elsewhere", Some(seat), Some(other), judge);
    let chat_child = context("coder-child", Some(seat), Some(seat), coder);
    forget(&db);
    Old { db, mk, deepseek, shadow, elsewhere, chat_child }
}

/// The migration renames kind `mk` to `council` and gives the shadows the
/// old rule told a `judge_shadows` row: a context that answers to its parent,
/// performed by a character whose model was on an `mk` backend. A context
/// that answers elsewhere, and a child performed by a chat character, stay
/// unmarked. Other kinds keep theirs.
///
/// Falsified by renaming before the backfill reads the old kind, or by
/// marking every child that answers to its parent.
#[test]
fn mk_backends_become_council_and_the_old_shadows_are_marked() {
    let o = old();
    KernelDb::migrate_council_backends(&o.db.conn).unwrap();
    assert_eq!(kind(&o.db, o.mk), "council");
    assert_eq!(kind(&o.db, o.deepseek), "deepseek");
    assert!(marked(&o.db, o.shadow), "the judge's child that answers to its seat is its shadow");
    assert!(!marked(&o.db, o.elsewhere), "a judge's context that answers elsewhere is no shadow");
    assert!(!marked(&o.db, o.chat_child), "a chat character's child is no shadow");
    assert!(recorded(&o.db));
}

/// The migration runs once: a backend later named `mk` by hand, or a new
/// child shaped like a shadow, is left as it is on the next open.
///
/// Falsified by running the migration on every open.
#[test]
fn the_migration_runs_once() {
    let o = old();
    KernelDb::migrate_council_backends(&o.db.conn).unwrap();
    o.db.conn.execute("UPDATE backends SET kind = 'mk' WHERE backend_id = ?1", params![blob_param(o.mk.as_bytes())]).unwrap();
    o.db.conn.execute("DELETE FROM judge_shadows", []).unwrap();
    KernelDb::migrate_council_backends(&o.db.conn).unwrap();
    assert_eq!(kind(&o.db, o.mk), "mk");
    assert!(!marked(&o.db, o.shadow));
}

/// A fresh database records the migration with nothing to change.
#[test]
fn a_fresh_database_records_the_migration() {
    let db = KernelDb::temporary().unwrap();
    assert!(recorded(&db));
}

use super::*;

/// A root character with a console, and a worker character with slots.
struct Rig {
    db: KernelDb,
    ws: WorkspaceId,
    amy: PrincipalId,
    coder: PrincipalId,
    console: ContextRow,
}

fn rig() -> Rig {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let [amy, coder] = seed_characters(&db, &["amy", "coder"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    db.seed_test_slots(coder, "mock", "m", &["coder", "distill"]).unwrap();
    let console = senpai_ctx_row(&db, ws, "console", None, Some(amy));
    assert_eq!(db.root_console(amy).unwrap(), Some(console.context_id), "the fixture points the root at its console");
    Rig { db, ws, amy, coder, console }
}

impl Rig {
    /// A live context forked from `parent` with the given performer and senpai.
    fn context(&self, label: &str, parent: Option<&ContextRow>, senpai: Option<&ContextRow>, performer: Option<PrincipalId>) -> ContextRow {
        let mut row = make_context_row(Some(label));
        row.forked_from = parent.map(|p| p.context_id);
        insert_context_with_doc(&self.db, &row, self.ws);
        if let Some(performer) = performer {
            self.db.update_performer(row.context_id, Some(performer)).unwrap();
        }
        if let Some(senpai) = senpai {
            self.db.set_context_senpai(row.context_id, Some(senpai.context_id)).unwrap();
        }
        row
    }

    fn fold(&self) -> KernelDbResult<()> {
        KernelDb::apply_additive_migrations(&self.db.conn)
    }

    fn notes(&self, kind: &str) -> Vec<ContextId> {
        self.db
            .context_character_migration_report()
            .unwrap()
            .into_iter()
            .filter(|n| n.kind == kind)
            .filter_map(|n| n.context_id)
            .collect()
    }

    fn migrated(&self) -> bool {
        self.db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
                [KernelDb::CONTEXT_FOLD_MIGRATION],
                |r| r.get(0),
            )
            .unwrap()
    }
}

/// The fold keeps every performer, senpai, persona and slot, rebuilds the
/// slots' foreign key onto `contexts`, drops the table, and a second open
/// changes nothing.
#[test]
fn the_fold_keeps_every_performer_senpai_and_slot_and_drops_the_table() {
    let r = rig();
    let seat = r.context("seat", None, Some(&r.console), Some(r.coder));
    let lane = r.context("lane", Some(&seat), Some(&seat), Some(r.coder));
    let before: Vec<ContextRow> = [&r.console, &seat, &lane].iter().map(|c| r.db.get_context(c.context_id).unwrap().unwrap()).collect();
    let slots_before = r.db.list_context_slots(lane.context_id).unwrap();
    assert_eq!(slots_before.len(), 2);
    into_pre_fold(&r.db);
    assert!(KernelDb::has_table(&r.db.conn, "context_characters").unwrap());
    assert!(!KernelDb::has_column(&r.db.conn, "contexts", "performer").unwrap());

    r.fold().unwrap();

    assert!(!KernelDb::has_table(&r.db.conn, "context_characters").unwrap(), "the table is dropped");
    for old in &before {
        let new = r.db.get_context(old.context_id).unwrap().unwrap();
        assert_eq!((new.performer, new.senpai_ctx, new.persona.clone()), (old.performer, old.senpai_ctx, old.persona.clone()), "{:?}", old.label);
    }
    assert_eq!(r.db.list_context_slots(lane.context_id).unwrap(), slots_before, "the slots are as they were");
    let references: Vec<String> = r
        .db
        .conn
        .prepare("SELECT \"table\" FROM pragma_foreign_key_list('context_slots') ORDER BY 1")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<SqliteResult<_>>()
        .unwrap();
    assert_eq!(references, vec!["backends", "contexts"], "context_slots references contexts, not the dropped table");
    assert!(r.migrated());
    assert_eq!(r.db.senpai_of(r.console.context_id).unwrap(), None, "the root console stays a top");
    assert_eq!(r.notes("senpai_assigned"), vec![], "every senpai was already there");
    // The guards are back, and a new context can take slots.
    let error = r.db.conn.execute("UPDATE contexts SET performer = NULL WHERE context_id = ?1", params![blob_param(seat.context_id.as_bytes())]).unwrap_err().to_string();
    assert!(error.contains("never changed"), "{error}");
    let fresh = r.context("fresh", None, Some(&r.console), Some(r.coder));
    assert_eq!(r.db.list_context_slots(fresh.context_id).unwrap().len(), 2);

    let report_len = r.db.context_character_migration_report().unwrap().len();
    r.fold().unwrap();
    assert_eq!(r.db.context_character_migration_report().unwrap().len(), report_len, "a second open changes nothing");
}

/// A live context with no senpai that is not a root console gets one: the
/// only live root's console. It is reported, and an archived context keeps NULL.
#[test]
fn a_live_context_with_no_senpai_gets_the_only_live_roots_console() {
    let r = rig();
    let scratch = r.context("scratch", None, None, None);
    let hub = r.context("hub", None, None, None);
    let log = r.context("log", Some(&hub), None, None);
    let mut dead = r.context("dead", None, None, None);
    r.db.archive_context(dead.context_id).unwrap();
    dead.archived_at = Some(1);
    into_pre_fold(&r.db);

    r.fold().unwrap();

    for ctx in [&scratch, &hub, &log] {
        assert_eq!(r.db.senpai_of(ctx.context_id).unwrap(), Some(r.console.context_id), "{:?}", ctx.label);
        assert_eq!(r.db.context_character(ctx.context_id).unwrap(), None, "and still no performer");
    }
    let mut assigned = r.notes("senpai_assigned");
    assigned.sort();
    let mut expected = vec![scratch.context_id, hub.context_id, log.context_id];
    expected.sort();
    assert_eq!(assigned, expected);
    assert_eq!(r.db.senpai_of(dead.context_id).unwrap(), None, "an archived context keeps NULL");
    assert_eq!(r.notes("archived_no_senpai"), vec![dead.context_id]);
    assert_eq!(r.db.senpai_of(r.console.context_id).unwrap(), None);
    assert_eq!(r.db.route_review(ReviewQuery::ask(log.context_id, r.coder)).unwrap().reviewer(), r.amy);
    // An archived context with no senpai is not touched by the guard.
    r.db.update_label(dead.context_id, Some("dead-renamed")).unwrap();
    r.db.set_context_senpai(dead.context_id, Some(r.console.context_id)).unwrap();
}

/// With several live roots a context takes the console of the root at the
/// top of its `forked_from` lineage; one with no such lineage refuses the
/// whole migration, naming it, and nothing changes.
#[test]
fn with_several_roots_the_forked_from_lineage_decides_and_an_ambiguous_context_refuses() {
    let r = rig();
    let [bob] = seed_characters(&r.db, &["bob"])[..] else { unreachable!() };
    r.db.update_character_root(bob, true).unwrap();
    let bob_console = senpai_ctx_row(&r.db, r.ws, "bob", None, Some(bob));
    assert_eq!(r.db.root_console(bob).unwrap(), Some(bob_console.context_id));
    let lane = r.context("bob-lane", None, Some(&bob_console), Some(r.coder));
    let child = r.context("bob-child", Some(&lane), None, None);
    let grandchild = r.context("bob-grandchild", Some(&child), None, None);
    let orphan = r.context("orphan", None, None, None);
    into_pre_fold(&r.db);

    let error = r.fold().unwrap_err().to_string();
    assert!(error.contains("orphan") && !error.contains("bob-child") && !error.contains("bob-grandchild"), "{error}");
    assert!(error.contains("nothing was changed"), "{error}");
    assert!(KernelDb::has_table(&r.db.conn, "context_characters").unwrap(), "the table is still there");
    assert!(!KernelDb::has_column(&r.db.conn, "contexts", "performer").unwrap(), "no column was added");
    assert!(!r.migrated());

    r.db.conn.execute(
        "UPDATE contexts SET forked_from = ?1 WHERE context_id = ?2",
        params![blob_param(lane.context_id.as_bytes()), blob_param(orphan.context_id.as_bytes())],
    ).unwrap();
    r.fold().unwrap();
    for ctx in [&child, &grandchild, &orphan] {
        assert_eq!(r.db.senpai_of(ctx.context_id).unwrap(), Some(bob_console.context_id), "{:?}", ctx.label);
    }
    assert_eq!(r.db.lineage_root(orphan.context_id).unwrap(), bob);
    assert_eq!(r.db.lineage_root(r.console.context_id).unwrap(), r.amy);
}

/// A kernel with no live root cannot place a live context without a senpai.
#[test]
fn a_live_context_with_no_senpai_and_no_live_root_refuses() {
    let r = rig();
    let scratch = r.context("scratch", None, None, None);
    assert!(r.db.retire_character(r.amy, 1).unwrap());
    into_pre_fold(&r.db);
    let error = r.fold().unwrap_err().to_string();
    assert!(error.contains("scratch") && error.contains("0 live roots"), "{error}");
    let _ = scratch;
}

/// A fresh database runs no fold and leaves no marker.
#[test]
fn a_fresh_database_runs_no_fold() {
    let db = KernelDb::temporary().unwrap();
    assert!(!KernelDb::has_table(&db.conn, "context_characters").unwrap());
    let marked: bool = db
        .conn
        .query_row("SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = 'context-fold-v1')", [], |r| r.get(0))
        .unwrap();
    assert!(!marked);
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    assert!(KernelDb::has_column(&db.conn, "contexts", "performer").unwrap());
}

/// A database with the table is copied aside before the fold; one without
/// makes no copy; a copy that cannot be made refuses naming the path.
#[test]
fn the_database_is_copied_aside_before_the_fold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    drop(KernelDb::open(&path).unwrap());
    assert!(!KernelDb::pre_context_fold_backup_path(&path).exists(), "a folded database makes no backup");
    {
        let db = KernelDb::open(&path).unwrap();
        let ws = setup_test_db(&db);
        let [amy] = seed_characters(&db, &["amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        senpai_ctx_row(&db, ws, "console", None, Some(amy));
        into_pre_fold(&db);
    }
    let db = KernelDb::open(&path).unwrap();
    assert!(!KernelDb::has_table(&db.conn, "context_characters").unwrap(), "the open folded it");
    let backup = KernelDb::pre_context_fold_backup_path(&path);
    let copy = Connection::open(&backup).unwrap();
    assert!(KernelDb::has_table(&copy, "context_characters").unwrap(), "the copy holds the table the fold dropped");
    assert!(!KernelDb::has_column(&copy, "contexts", "performer").unwrap());

    let pre = KernelDb::temporary().unwrap();
    KernelDb::ensure_legacy_context_characters(&pre.conn).unwrap();
    let unwritable = Path::new("/nonexistent-kernel-db-directory/kernel.db");
    let error = KernelDb::back_up_before_context_fold(&pre.conn, unwritable).unwrap_err().to_string();
    assert!(error.contains("kernel.db.pre-context-fold") && error.contains("could not be made"), "{error}");
    KernelDb::back_up_before_context_fold(&db.conn, unwritable).expect("a folded database needs no copy");
}

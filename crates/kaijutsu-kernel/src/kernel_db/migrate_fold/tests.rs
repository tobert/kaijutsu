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

/// A context with no senpai that is not a root console gets one, live or
/// archived: the only live root's console. Each is reported.
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
    let mut expected = vec![scratch.context_id, hub.context_id, log.context_id, dead.context_id];
    expected.sort();
    assert_eq!(assigned, expected);
    assert_eq!(r.db.senpai_of(dead.context_id).unwrap(), Some(r.console.context_id), "an archived context gets one too");
    assert_eq!(r.notes("archived_no_senpai"), vec![], "none is left without");
    assert_eq!(r.db.senpai_of(r.console.context_id).unwrap(), None);
    assert_eq!(r.db.route_review(ReviewQuery::ask(log.context_id, r.coder)).unwrap().reviewer(), r.amy);
}

/// An archived context that no rule can place keeps NULL, is reported as
/// `archived_no_senpai`, never refuses the migration, and can be given a
/// senpai later.
#[test]
fn an_archived_context_no_rule_can_place_keeps_null_and_never_refuses() {
    let r = rig();
    let dead = r.context("dead", None, None, None);
    r.db.archive_context(dead.context_id).unwrap();
    assert!(r.db.retire_character(r.amy, 1).unwrap());
    into_pre_fold(&r.db);

    r.fold().unwrap();

    assert_eq!(r.db.senpai_of(dead.context_id).unwrap(), None);
    assert_eq!(r.notes("archived_no_senpai"), vec![dead.context_id]);
    assert_eq!(r.notes("senpai_assigned"), vec![]);
    r.db.update_label(dead.context_id, Some("dead-renamed")).unwrap();
    r.db.set_context_senpai(dead.context_id, Some(r.console.context_id)).unwrap();
}

/// With several live roots an archived context takes the console its
/// `forked_from` lineage reaches, like a live one.
#[test]
fn an_archived_context_takes_the_lineage_console_when_several_roots_are_live() {
    let r = rig();
    let [bob] = seed_characters(&r.db, &["bob"])[..] else { unreachable!() };
    r.db.update_character_root(bob, true).unwrap();
    let bob_console = senpai_ctx_row(&r.db, r.ws, "bob", None, Some(bob));
    let lane = r.context("bob-lane", None, Some(&bob_console), Some(r.coder));
    let dead = r.context("dead-child", Some(&lane), None, None);
    r.db.archive_context(dead.context_id).unwrap();
    let lost = r.context("dead-orphan", None, None, None);
    r.db.archive_context(lost.context_id).unwrap();
    into_pre_fold(&r.db);

    r.fold().unwrap();

    assert_eq!(r.db.senpai_of(dead.context_id).unwrap(), Some(bob_console.context_id));
    assert_eq!(r.notes("senpai_assigned"), vec![dead.context_id]);
    assert_eq!(r.db.senpai_of(lost.context_id).unwrap(), None, "no rule reaches it");
    assert_eq!(r.notes("archived_no_senpai"), vec![lost.context_id]);
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

/// A database from before context characters, with a performer-less context
/// that has a senpai, folds with that senpai kept.
#[test]
fn a_pre_context_character_database_keeps_the_senpai_of_a_performer_less_context() {
    let r = rig();
    let lane = r.context("lane", None, Some(&r.console), Some(r.coder));
    let bare = r.context("bare", None, Some(&lane), None);
    assert_eq!(r.db.get_context(bare.context_id).unwrap().unwrap().performer, None);
    into_pre_context_character(&r.db);
    // A database this old has neither guard.
    r.db.conn.execute_batch("DROP TRIGGER IF EXISTS contexts_performer_fixed; DROP TRIGGER IF EXISTS contexts_senpai_required;").unwrap();
    // It ran `senpai-edges-v1` and set `senpai_ctx`; `context-characters-v1` and the fold are ahead.
    r.db.conn
        .execute(
            "DELETE FROM kernel_migrations WHERE name IN (?1, ?2)",
            params![KernelDb::CONTEXT_CHARACTERS_MIGRATION, KernelDb::CONTEXT_FOLD_MIGRATION],
        )
        .unwrap();
    r.db.conn
        .execute("INSERT OR IGNORE INTO kernel_migrations (name) VALUES (?1)", [KernelDb::SENPAI_MIGRATION])
        .unwrap();

    r.fold().unwrap();

    assert!(r.migrated());
    let after = r.db.get_context(bare.context_id).unwrap().unwrap();
    assert_eq!((after.performer, after.senpai_ctx), (None, Some(lane.context_id)), "the senpai survives the whole chain");
    assert_eq!(r.notes("senpai_assigned"), vec![], "the fold had nothing to assign");
}

/// A dangling foreign key anywhere in the database refuses the fold and
/// leaves the old shape in place.
#[test]
fn a_dangling_foreign_key_refuses_the_fold_and_changes_nothing() {
    let r = rig();
    let bare = r.context("bare", None, Some(&r.console), None);
    into_pre_fold(&r.db);
    r.db.conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    r.db.conn
        .execute(
            "INSERT INTO context_edges (edge_id, source_id, target_id, created_at) VALUES (?1, ?2, ?3, 0)",
            params![
                uuid::Uuid::now_v7().as_bytes().as_slice(),
                blob_param(bare.context_id.as_bytes()),
                blob_param(ContextId::new().as_bytes())
            ],
        )
        .unwrap();
    r.db.conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();

    let error = KernelDb::migrate_context_fold(&r.db.conn).unwrap_err().to_string();
    assert!(error.contains("context fold left a dangling foreign key on context_edges"), "{error}");
    assert!(KernelDb::has_table(&r.db.conn, "context_characters").unwrap(), "the table is still there");
    assert!(!r.migrated());
}

/// Setting a senpai again clears the notes that describe the old one and keeps
/// the rest, for that context only.
#[test]
fn setting_a_senpai_clears_the_notes_about_it_and_keeps_the_others() {
    let r = rig();
    let scratch = r.context("scratch", None, None, None);
    let other = r.context("other", None, None, None);
    let dead = r.context("dead", None, None, None);
    r.db.archive_context(dead.context_id).unwrap();
    into_pre_fold(&r.db);
    r.fold().unwrap();
    let add = |kind: &str, ctx: &ContextRow, note: &str| {
        r.db.conn
            .execute(
                "INSERT INTO context_character_migration_report (kind, context_id, note, recorded_at) VALUES (?1, ?2, ?3, 1)",
                params![kind, blob_param(ctx.context_id.as_bytes()), note],
            )
            .unwrap();
    };
    add("archived_no_senpai", &scratch, "archived with no senpai");
    add("dropped", &scratch, "a root console has no senpai: senpai_ctx x is not carried");
    add("unmapped", &scratch, "no performer: its senpai_ctx x is kept");
    add("dropped", &scratch, "has no context character, so its cast and model override run nothing");
    add("root_label_kept", &scratch, "the label stays");
    add("dropped", &other, "senpai_ctx y has no context character");
    assert_eq!(r.notes("senpai_assigned").len(), 3);

    r.db.set_context_senpai(scratch.context_id, Some(r.console.context_id)).unwrap();

    let left: Vec<(String, Option<ContextId>)> = r
        .db
        .context_character_migration_report()
        .unwrap()
        .into_iter()
        .map(|n| (n.kind, n.context_id))
        .collect();
    let mine: Vec<&str> = left.iter().filter(|(_, c)| *c == Some(scratch.context_id)).map(|(k, _)| k.as_str()).collect();
    assert_eq!(mine, vec!["dropped", "root_label_kept"], "only the cast note and the label note stay: {left:?}");
    assert!(left.contains(&("senpai_assigned".to_string(), Some(other.context_id))), "another context keeps its notes");
    assert!(left.contains(&("dropped".to_string(), Some(other.context_id))));
}

use super::*;

/// A database shaped like zorak's: a root with its root console, a
/// director-typed character, an MCP lead with no type whose live contexts are
/// `mcp`, a character with no type and one live context of a host-only type,
/// and a character with nothing live.
struct Rig {
    db: KernelDb,
    ws: WorkspaceId,
    amy: PrincipalId,
    root_console: ContextRow,
}

fn character(db: &KernelDb, name: &str, root: bool, context_type: Option<&str>) -> PrincipalId {
    let principal_id = PrincipalId::new();
    db.insert_character(&CharacterRow {
        rc_dir: None, console_ctx: None, mcp_hub_ctx: None,
        context_type: context_type.map(str::to_string),
        principal_id, name: name.to_string(), created_at: 0,
        retired_at: None, handoff_ctx: None, root_ctx: None, root,
    }).unwrap();
    principal_id
}

fn rig() -> Rig {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let amy = character(&db, "amy", true, None);
    let mut root_console = make_context_row(Some("amy"));
    root_console.context_type = "root".into();
    insert_context_with_doc(&db, &root_console, ws);
    db.update_performer(root_console.context_id, Some(amy)).unwrap();
    db.set_character_root_ctx(amy, Some(root_console.context_id)).unwrap();
    Rig { db, ws, amy, root_console }
}

impl Rig {
    /// A live context of `context_type` performed by `performer`, answering
    /// to the root console.
    fn context(&self, label: &str, context_type: &str, performer: Option<PrincipalId>) -> ContextRow {
        let mut row = make_context_row(Some(label));
        row.context_type = context_type.into();
        insert_context_with_doc(&self.db, &row, self.ws);
        if let Some(performer) = performer {
            self.db.update_performer(row.context_id, Some(performer)).unwrap();
        }
        self.db.set_context_senpai(row.context_id, Some(self.root_console.context_id)).unwrap();
        row
    }

    fn archived(&self, label: &str, context_type: &str, performer: PrincipalId) -> ContextRow {
        let row = self.context(label, context_type, Some(performer));
        self.db.archive_context(row.context_id).unwrap();
        row
    }

    /// Forget the migration and run the opens' migrations again, as a
    /// database from before it would.
    fn backfill(&self) {
        self.db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();
        KernelDb::apply_additive_migrations(&self.db.conn).unwrap();
    }

    fn rc_dir(&self, name: &str) -> Option<String> {
        self.db.get_character_by_name(name).unwrap().unwrap().rc_dir
    }

    fn notes(&self, kind: &str) -> Vec<(Option<ContextId>, String)> {
        self.db
            .context_character_migration_report()
            .unwrap()
            .into_iter()
            .filter(|n| n.kind == kind)
            .map(|n| (n.context_id, n.note))
            .collect()
    }
}

/// Each character takes the directory its sheet or its live contexts name;
/// the rest stay NULL and are reported by name, and boot does not fail.
#[test]
fn the_backfill_gives_each_character_the_directory_its_sheet_or_live_contexts_name() {
    let r = rig();
    let banto = character(&r.db, "banto", false, Some("director"));
    let lead = character(&r.db, "kaijutsu-lead", false, None);
    let bassist = character(&r.db, "bassist", false, None);
    let ghost = character(&r.db, "ghost", false, None);
    let set = character(&r.db, "already", false, Some("coder"));
    r.db.set_character_rc_dir(set, Some("toolie")).unwrap();
    r.context("banto", "director", Some(banto));
    r.context("mcp-1", "mcp", Some(lead));
    r.context("mcp-2", "mcp", Some(lead));
    // Archived contexts do not vote.
    r.archived("old-coder-1", "coder", lead);
    r.archived("old-coder-2", "coder", lead);
    r.archived("old-coder-3", "coder", lead);
    r.context("bass", "bassist", Some(bassist));
    r.archived("gone", "coder", ghost);

    r.backfill();

    assert_eq!(r.rc_dir("amy").as_deref(), Some("root"), "a root gets root");
    assert_eq!(r.rc_dir("banto").as_deref(), Some("director"), "a typed character gets its type");
    assert_eq!(r.rc_dir("kaijutsu-lead").as_deref(), Some("mcp"), "an MCP lead gets its live contexts' type");
    assert_eq!(r.rc_dir("bassist").as_deref(), Some("bassist"), "a host-only directory goes to the character that plays it");
    assert_eq!(r.rc_dir("ghost"), None, "nothing live leaves it unset");
    assert_eq!(r.rc_dir("already").as_deref(), Some("toolie"), "a set directory is kept");
    let unset = r.notes("rc_dir_unset");
    assert_eq!(unset.len(), 1, "{unset:?}");
    assert_eq!(unset[0].0, None);
    assert!(unset[0].1.contains("ghost") && unset[0].1.contains("kj character set ghost --rc <dir>"), "{}", unset[0].1);

    // Recorded once: a second open changes nothing, even for a character
    // made with no directory since.
    let late = character(&r.db, "late", false, Some("coder"));
    KernelDb::apply_additive_migrations(&r.db.conn).unwrap();
    assert_eq!(r.db.get_character(late).unwrap().unwrap().rc_dir, None);
    assert_eq!(r.notes("rc_dir_unset").len(), 1, "a second open reports nothing new");
    let _ = (r.amy, ghost);
}

/// A tie among the live contexts' types is not guessed; judge contexts run
/// `judge/` whoever performs them, so they do not vote.
#[test]
fn a_tie_is_left_unset_and_judge_contexts_do_not_vote() {
    let r = rig();
    let tied = character(&r.db, "tied", false, None);
    r.context("t-coder", "coder", Some(tied));
    r.context("t-default", "default", Some(tied));
    let seat = character(&r.db, "seat", false, None);
    r.context("s-coder", "coder", Some(seat));
    r.context("s-judge-1", "judge", Some(seat));
    r.context("s-judge-2", "judge", Some(seat));

    r.backfill();

    assert_eq!(r.rc_dir("tied"), None);
    assert_eq!(r.rc_dir("seat").as_deref(), Some("coder"));
    let unset = r.notes("rc_dir_unset");
    assert_eq!(unset.len(), 1, "{unset:?}");
    assert!(unset[0].1.contains("tied") && unset[0].1.contains("'coder'") && unset[0].1.contains("'default'"), "{}", unset[0].1);
}

/// A root's console pointer is set from its one live console that answers to
/// its root console; several leave it NULL and each is reported.
#[test]
fn the_backfill_points_a_root_at_its_one_live_console() {
    let r = rig();
    let banto = character(&r.db, "banto", false, Some("director"));
    let console = r.context("console", "console", Some(banto));
    r.archived("console-old", "console", banto);
    r.context("not-console", "coder", Some(banto));

    r.backfill();

    assert_eq!(r.db.get_character(r.amy).unwrap().unwrap().console_ctx, Some(console.context_id));
    assert!(r.notes("console_ambiguous").is_empty());
}

#[test]
fn several_live_consoles_leave_the_pointer_unset_and_name_them() {
    let r = rig();
    let banto = character(&r.db, "banto", false, Some("director"));
    let one = r.context("console", "console", Some(banto));
    let two = r.context("console-2", "console", Some(banto));

    r.backfill();

    assert_eq!(r.db.get_character(r.amy).unwrap().unwrap().console_ctx, None);
    let notes = r.notes("console_ambiguous");
    let named: Vec<Option<ContextId>> = notes.iter().map(|(c, _)| *c).collect();
    assert_eq!(named, vec![Some(one.context_id), Some(two.context_id)]);
    assert!(notes[0].1.contains("console-2") && notes[0].1.contains("kj character set amy --console"), "{}", notes[0].1);
}

/// A console pointer that is already set is kept.
#[test]
fn a_set_console_pointer_is_kept() {
    let r = rig();
    let banto = character(&r.db, "banto", false, None);
    let first = r.context("console", "console", Some(banto));
    r.db.set_character_console_ctx(r.amy, Some(first.context_id)).unwrap();
    r.db.archive_context(first.context_id).unwrap();
    r.context("console-2", "console", Some(banto));

    r.backfill();

    assert_eq!(r.db.get_character(r.amy).unwrap().unwrap().console_ctx, Some(first.context_id));
}

/// A fresh database records the migration with nothing to change and makes
/// no copy; a database with characters and no record is copied aside first;
/// a copy that cannot be made refuses, naming the path.
#[test]
fn the_database_is_copied_aside_before_the_backfill() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    drop(KernelDb::open(&path).unwrap());
    assert!(!KernelDb::pre_rc_dir_backup_path(&path).exists(), "a fresh database makes no backup");
    {
        let db = KernelDb::open(&path).unwrap();
        let recorded: bool = db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [KernelDb::RC_DIR_BACKFILL_MIGRATION], |r| r.get(0),
        ).unwrap();
        assert!(recorded, "a fresh database records the migration");
        character(&db, "amy", true, None);
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();
    }
    let db = KernelDb::open(&path).unwrap();
    assert_eq!(db.get_character_by_name("amy").unwrap().unwrap().rc_dir.as_deref(), Some("root"), "the open backfilled it");
    let backup = KernelDb::pre_rc_dir_backup_path(&path);
    let copy = KernelDb::open_read_only(&backup).unwrap();
    assert_eq!(copy.get_character_by_name("amy").unwrap().unwrap().rc_dir, None, "the copy is the database before the backfill");

    let unwritable = Path::new("/nonexistent-kernel-db-directory/kernel.db");
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();
    let error = KernelDb::back_up_before_rc_dir_backfill(&db.conn, unwritable).unwrap_err().to_string();
    assert!(error.contains("kernel.db.pre-rc-dir") && error.contains("could not be made"), "{error}");
    KernelDb::migrate_rc_dir_backfill(&db.conn).unwrap();
    KernelDb::back_up_before_rc_dir_backfill(&db.conn, unwritable).expect("a backfilled database needs no copy");
}

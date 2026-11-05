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

/// A character with the legacy `characters.context_type` the backfill reads.
fn character(db: &KernelDb, name: &str, root: bool, context_type: Option<&str>) -> PrincipalId {
    let principal_id = PrincipalId::new();
    db.insert_character(&CharacterRow {
        rc_dir: None, console_ctx: None, mcp_hub_ctx: None,
        principal_id, name: name.to_string(), created_at: 0,
        retired_at: None, handoff_ctx: None, root_ctx: None, root,
    }).unwrap();
    set_legacy_character_type(db, principal_id, context_type);
    principal_id
}

fn rig() -> Rig {
    rig_on(KernelDb::temporary().unwrap())
}

fn rig_on(db: KernelDb) -> Rig {
    let ws = setup_test_db(&db);
    let amy = character(&db, "amy", true, None);
    let root_console = make_context_row(Some("amy"));
    insert_context_with_doc(&db, &root_console, ws);
    set_legacy_context_type(&db, root_console.context_id, "root");
    db.update_performer(root_console.context_id, Some(amy)).unwrap();
    db.set_character_root_ctx(amy, Some(root_console.context_id)).unwrap();
    Rig { db, ws, amy, root_console }
}

impl Rig {
    /// A live context of `context_type` performed by `performer`, answering
    /// to the root console.
    fn context(&self, label: &str, context_type: &str, performer: Option<PrincipalId>) -> ContextRow {
        let row = make_context_row(Some(label));
        insert_context_with_doc(&self.db, &row, self.ws);
        set_legacy_context_type(&self.db, row.context_id, context_type);
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

    fn report_principals(&self, kind: &str) -> Vec<Option<PrincipalId>> {
        self.db
            .context_character_migration_report()
            .unwrap()
            .into_iter()
            .filter(|n| n.kind == kind)
            .map(|n| n.principal_id)
            .collect()
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
    // Archived contexts vote only when no live one can.
    r.archived("old-coder-1", "coder", lead);
    r.archived("old-coder-2", "coder", lead);
    r.archived("old-coder-3", "coder", lead);
    r.context("bass", "bassist", Some(bassist));

    r.backfill();

    assert_eq!(r.rc_dir("amy").as_deref(), Some("root"), "a root gets root");
    assert_eq!(r.rc_dir("banto").as_deref(), Some("director"), "a typed character gets its type");
    assert_eq!(r.rc_dir("kaijutsu-lead").as_deref(), Some("mcp"), "an MCP lead gets its live contexts' type");
    assert_eq!(r.rc_dir("bassist").as_deref(), Some("bassist"), "a host-only directory goes to the character that plays it");
    assert_eq!(r.rc_dir("ghost"), None, "no context at all leaves it unset");
    assert_eq!(r.rc_dir("already").as_deref(), Some("toolie"), "a set directory is kept");
    let unset = r.notes("rc_dir_unset");
    assert_eq!(unset.len(), 1, "{unset:?}");
    assert_eq!(unset[0].0, None);
    assert_eq!(r.report_principals("rc_dir_unset"), vec![Some(ghost)], "the note names the character");
    assert!(unset[0].1.contains("ghost") && unset[0].1.contains("kj character set ghost --rc <dir>"), "{}", unset[0].1);

    // Recorded once: a second open changes nothing, even for a character
    // made with no directory since.
    let late = character(&r.db, "late", false, Some("coder"));
    KernelDb::apply_additive_migrations(&r.db.conn).unwrap();
    assert_eq!(r.db.get_character(late).unwrap().unwrap().rc_dir, None);
    assert_eq!(r.notes("rc_dir_unset").len(), 1, "a second open reports nothing new");
    let _ = (r.amy, ghost);
}

/// A tie among the live contexts' types is not guessed; judge contexts, which
/// a seat's character performed before `slot-roles-v1`, do not vote.
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

/// A console pointer that is already set is kept, even on an archived
/// context (a database from before archiving cleared the pointer).
#[test]
fn a_set_console_pointer_is_kept() {
    let r = rig();
    let banto = character(&r.db, "banto", false, None);
    let first = r.context("console", "console", Some(banto));
    r.db.archive_context(first.context_id).unwrap();
    r.db.set_character_console_ctx(r.amy, Some(first.context_id)).unwrap();
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
    db.conn.execute("UPDATE characters SET rc_dir = NULL", []).unwrap();
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();
    let error = KernelDb::back_up_before_rc_dir_backfill(&db.conn, unwritable).unwrap_err().to_string();
    assert!(error.contains("kernel.db.pre-rc-dir") && error.contains("could not be made"), "{error}");
    KernelDb::migrate_rc_dir_backfill(&db.conn).unwrap();
    KernelDb::back_up_before_rc_dir_backfill(&db.conn, unwritable).expect("a backfilled database needs no copy");
}

/// An MCP lead whose sessions are all archived (they archive at process
/// shutdown) still votes: with no live context, the archived ones count. A
/// live context outvotes archived ones, and an archived tie leaves NULL.
#[test]
fn archived_contexts_vote_when_nothing_is_live() {
    let r = rig();
    let lead = character(&r.db, "lead", false, None);
    r.archived("mcp-1", "mcp", lead);
    r.archived("mcp-2", "mcp", lead);
    r.archived("old-coder", "coder", lead);
    let tied = character(&r.db, "tied", false, None);
    r.archived("t-1", "mcp", tied);
    r.archived("t-2", "coder", tied);

    r.backfill();

    assert_eq!(r.rc_dir("lead").as_deref(), Some("mcp"));
    assert_eq!(r.rc_dir("tied"), None);
    let unset = r.notes("rc_dir_unset");
    assert_eq!(unset.len(), 1, "{unset:?}");
    assert!(unset[0].1.contains("tied") && unset[0].1.contains("'mcp'") && unset[0].1.contains("'coder'"), "{}", unset[0].1);
}

/// A character that plays only judge contexts is not told it has no live
/// context: the note says judge contexts cast no vote.
#[test]
fn a_judge_only_character_is_told_why_it_has_no_vote() {
    let r = rig();
    let seat = character(&r.db, "judge-only", false, None);
    r.context("j1", "judge", Some(seat));

    r.backfill();

    assert_eq!(r.rc_dir("judge-only"), None);
    let unset = r.notes("rc_dir_unset");
    assert_eq!(unset.len(), 1, "{unset:?}");
    assert!(unset[0].1.contains("judge") && !unset[0].1.contains("no live context"), "{}", unset[0].1);
    assert!(unset[0].1.contains("kj character set judge-only --rc <dir>"), "{}", unset[0].1);
}

/// An empty `rc_dir` is unset: the backfill fills it.
#[test]
fn an_empty_rc_dir_is_backfilled() {
    let r = rig();
    let blank = character(&r.db, "blank", false, Some("coder"));
    r.db.set_character_rc_dir(blank, Some("")).unwrap();

    r.backfill();

    assert_eq!(r.rc_dir("blank").as_deref(), Some("coder"));
}

/// Naming a directory clears the character's unset note.
#[test]
fn setting_the_rc_dir_clears_the_unset_note() {
    let r = rig();
    let ghost = character(&r.db, "ghost", false, None);
    r.backfill();
    assert_eq!(r.report_principals("rc_dir_unset"), vec![Some(ghost)]);

    r.db.set_character_rc_dir(ghost, Some("coder")).unwrap();

    assert!(r.report_principals("rc_dir_unset").is_empty());
}

fn backed_up(path: &Path) -> bool {
    KernelDb::pre_rc_dir_backup_path(path).exists()
}

/// The copy is made only when the backfill would change something: a
/// character with no directory, or a root with no console pointer and a live
/// console to point at.
#[test]
fn the_copy_is_skipped_when_nothing_would_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        let named = character(&db, "named", false, Some("coder"));
        db.set_character_rc_dir(named, Some("coder")).unwrap();
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();
    }
    drop(KernelDb::open(&path).unwrap());
    assert!(!backed_up(&path), "every directory is set and there is no root: nothing to change");

    // A character with no directory is a change.
    {
        let db = KernelDb::open(&path).unwrap();
        character(&db, "bare", false, Some("coder"));
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();
    }
    drop(KernelDb::open(&path).unwrap());
    assert!(backed_up(&path), "a character with no rc directory needs the copy");
}

#[test]
fn a_root_with_a_live_console_to_point_at_needs_the_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let r = rig_on(KernelDb::open(&path).unwrap());
        let banto = character(&r.db, "banto", false, Some("director"));
        r.db.set_character_rc_dir(banto, Some("director")).unwrap();
        r.db.set_character_rc_dir(r.amy, Some("root")).unwrap();
        r.context("console", "console", Some(banto));
        r.db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();
    }
    drop(KernelDb::open(&path).unwrap());
    assert!(backed_up(&path), "a console pointer the backfill would set needs the copy");
}

/// A root whose console pointer the backfill could not set (no live console)
/// does not cost a copy.
#[test]
fn a_root_with_no_console_to_point_at_needs_no_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let r = rig_on(KernelDb::open(&path).unwrap());
        r.db.set_character_rc_dir(r.amy, Some("root")).unwrap();
        r.db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_BACKFILL_MIGRATION]).unwrap();
    }
    drop(KernelDb::open(&path).unwrap());
    assert!(!backed_up(&path));
}

/// A database whose `contexts` and `characters` tables have no
/// `context_type` opens, and the migrations that read it record themselves
/// without work.
#[test]
fn migrations_that_read_context_type_record_themselves_when_the_column_is_gone() {
    let db = KernelDb::temporary().unwrap();
    assert!(!KernelDb::has_column(&db.conn, "contexts", "context_type").unwrap());
    assert!(!KernelDb::has_column(&db.conn, "characters", "context_type").unwrap());
    let guarded = [KernelDb::HOUSE_MIGRATION, KernelDb::CONSOLE_LABEL_MIGRATION, KernelDb::RC_DIR_BACKFILL_MIGRATION];
    for name in guarded {
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [name]).unwrap();
    }

    KernelDb::migrate_house_capability(&db.conn).unwrap();
    KernelDb::migrate_console_label(&db.conn).unwrap();
    KernelDb::migrate_rc_dir_backfill(&db.conn).unwrap();

    for name in guarded {
        let recorded: bool = db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)", [name], |r| r.get(0),
        ).unwrap();
        assert!(recorded, "{name} records itself");
    }
}

// ── rc-dir-rename-v1 ──

impl Rig {
    /// Forget the rename and run the opens' migrations again, as a database
    /// from before it would.
    fn rename(&self) {
        self.db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_RENAME_MIGRATION]).unwrap();
        KernelDb::apply_additive_migrations(&self.db.conn).unwrap();
    }
}

fn named(db: &KernelDb, name: &str, dir: &str) -> PrincipalId {
    let id = character(db, name, false, None);
    db.set_character_rc_dir(id, Some(dir)).unwrap();
    id
}

/// Each character on a retired directory moves to the directory that
/// absorbed it, and each move is reported naming the character; every other
/// directory stays. Recorded once.
#[test]
fn the_rename_moves_each_retired_directory_and_reports_it() {
    let r = rig();
    r.db.set_character_rc_dir(r.amy, Some("root")).unwrap();
    let banto = named(&r.db, "banto", "director");
    let console = named(&r.db, "console-player", "console");
    let shell = named(&r.db, "shell-player", "shell");
    let plain = named(&r.db, "plain", "default");
    named(&r.db, "coder", "coder");
    named(&r.db, "bassist", "bassist");

    r.rename();

    for (name, dir) in [("amy", "person"), ("banto", "banto"), ("console-player", "banto"), ("plain", "coder"), ("coder", "coder"), ("bassist", "bassist")] {
        assert_eq!(r.rc_dir(name).as_deref(), Some(dir), "{name}");
    }
    // `person/` belongs to roots: a non-root on `shell` is left with no
    // directory rather than handed the root loadout.
    assert_eq!(r.rc_dir("shell-player"), None);
    let moved = r.report_principals("rc_dir_renamed");
    assert_eq!(moved.len(), 5, "{moved:?}");
    for id in [r.amy, banto, console, shell, plain] {
        assert!(moved.contains(&Some(id)), "{id:?} is reported");
    }
    let notes = r.notes("rc_dir_renamed");
    assert!(notes.iter().any(|(_, n)| n.contains("banto") && n.contains("'director'") && n.contains("'banto'")), "{notes:?}");
    assert!(notes.iter().any(|(_, n)| n.contains("plain") && n.contains("'default'") && n.contains("'coder'")), "{notes:?}");

    // Recorded once: a character set to a retired directory since is left as
    // it is, and nothing new is reported.
    named(&r.db, "late", "director");
    KernelDb::apply_additive_migrations(&r.db.conn).unwrap();
    assert_eq!(r.rc_dir("late").as_deref(), Some("director"));
    assert_eq!(r.notes("rc_dir_renamed").len(), 5);
}

/// `person/` is the merge of `root/` and `shell/`, so a root moving from
/// either to it gets wider bindings; a non-root on either gets no directory.
#[test]
fn only_roots_move_to_person() {
    let r = rig();
    r.db.set_character_rc_dir(r.amy, Some("shell")).unwrap();
    named(&r.db, "root-player", "root");
    named(&r.db, "shell-player", "shell");
    let second = character(&r.db, "second-root", true, None);
    r.db.set_character_rc_dir(second, Some("root")).unwrap();
    r.rename();
    assert_eq!(r.rc_dir("amy").as_deref(), Some("person"));
    assert_eq!(r.rc_dir("second-root").as_deref(), Some("person"));
    assert_eq!(r.rc_dir("root-player"), None);
    assert_eq!(r.rc_dir("shell-player"), None);
    let notes = r.notes("rc_dir_renamed");
    assert_eq!(notes.len(), 4, "every move is reported: {notes:?}");
    let note = |name: &str| notes.iter().map(|(_, n)| n).find(|n| n.starts_with(name)).unwrap_or_else(|| panic!("{name}: {notes:?}"));
    let left = note("shell-player");
    assert!(left.contains("no rc directory") && left.contains("'shell'") && !left.contains("moved from"), "{left}");
    assert!(left.contains("kj character set shell-player --rc <dir>") && left.contains("refuses"), "{left}");
    let widened = note("second-root");
    assert!(widened.contains("'root'") && widened.contains("'person'") && widened.contains("wider than"), "{widened}");
    assert!(!widened.contains("possibly"), "{widened}");
}

/// The note says the character's contexts now run the new directory's
/// bindings, wider for `shell` and `director`, narrower for `default`, and
/// names the commands that check and change it.
#[test]
fn the_rename_note_says_the_bindings_widen_or_narrow() {
    let r = rig();
    named(&r.db, "plain", "default");
    named(&r.db, "banto-player", "director");
    r.rename();
    let notes = r.notes("rc_dir_renamed");
    let note = |name: &str| notes.iter().map(|(_, n)| n).find(|n| n.starts_with(name)).unwrap_or_else(|| panic!("{name}: {notes:?}"));
    let narrowed = note("plain");
    assert!(narrowed.contains("narrower") && narrowed.contains("'coder'") && narrowed.contains("bindings"), "{narrowed}");
    assert!(!narrowed.contains("wider"), "{narrowed}");
    let widened = note("banto-player");
    assert!(widened.contains("wider") && widened.contains("'banto'") && widened.contains("bindings"), "{widened}");
    assert!(!widened.contains("narrower"), "{widened}");
    for n in [narrowed, widened] {
        assert!(n.contains("kj binding show") && n.contains("kj character set") && n.contains("--rc"), "{n}");
        assert!(!n.contains("in one of them"), "the check must hold with no live context: {n}");
    }
}

/// A database from before the backfill (characters with a type and no
/// directory) gets the backfill's directories and their renames in the same
/// open: a root ends on `person`, a director on `banto`.
#[test]
fn an_old_shaped_database_ends_on_the_renamed_directories() {
    let r = rig();
    character(&r.db, "banto", false, Some("director"));
    character(&r.db, "worker", false, Some("default"));
    character(&r.db, "toolie", false, Some("toolie"));
    for name in [KernelDb::RC_DIR_BACKFILL_MIGRATION, KernelDb::RC_DIR_RENAME_MIGRATION] {
        r.db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [name]).unwrap();
    }
    KernelDb::apply_additive_migrations(&r.db.conn).unwrap();
    assert_eq!(r.rc_dir("amy").as_deref(), Some("person"));
    assert_eq!(r.rc_dir("banto").as_deref(), Some("banto"));
    assert_eq!(r.rc_dir("worker").as_deref(), Some("coder"));
    assert_eq!(r.rc_dir("toolie").as_deref(), Some("toolie"));
}

fn renamed_backup(path: &Path) -> bool {
    KernelDb::pre_rc_dir_rename_backup_path(path).exists()
}

/// A database whose `rc_dir` column exists but whose rows are NULL gets the
/// retired names from the backfill in the same open, so the rename's copy is
/// made before either migration changes it.
#[test]
fn the_rename_copy_counts_the_rows_the_backfill_will_fill() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        character(&db, "banto", false, Some("director"));
        for name in [KernelDb::RC_DIR_BACKFILL_MIGRATION, KernelDb::RC_DIR_RENAME_MIGRATION] {
            db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [name]).unwrap();
        }
    }
    let db = KernelDb::open(&path).unwrap();
    assert_eq!(db.get_character_by_name("banto").unwrap().unwrap().rc_dir.as_deref(), Some("banto"), "both migrations ran in the open");
    assert!(renamed_backup(&path), "the rename's copy exists");
    let copy = KernelDb::open_read_only(KernelDb::pre_rc_dir_rename_backup_path(&path)).unwrap();
    assert_eq!(copy.get_character_by_name("banto").unwrap().unwrap().rc_dir, None, "the copy is the database before either migration");
}

/// A fresh database records the rename and makes no copy. A database with a
/// character on a retired directory and no record is copied aside first, and
/// the copy holds the old directory. One with nothing to move makes no copy.
/// A copy that cannot be made refuses, naming the path.
#[test]
fn the_database_is_copied_aside_before_the_rename_only_when_it_has_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        let recorded: bool = db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [KernelDb::RC_DIR_RENAME_MIGRATION], |r| r.get(0),
        ).unwrap();
        assert!(recorded, "a fresh database records the rename");
        named(&db, "coder", "coder");
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_RENAME_MIGRATION]).unwrap();
    }
    drop(KernelDb::open(&path).unwrap());
    assert!(!renamed_backup(&path), "nothing on a retired directory: no copy");
    {
        let db = KernelDb::open(&path).unwrap();
        named(&db, "banto", "director");
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_RENAME_MIGRATION]).unwrap();
    }
    let db = KernelDb::open(&path).unwrap();
    assert_eq!(db.get_character_by_name("banto").unwrap().unwrap().rc_dir.as_deref(), Some("banto"));
    let copy = KernelDb::open_read_only(KernelDb::pre_rc_dir_rename_backup_path(&path)).unwrap();
    assert_eq!(copy.get_character_by_name("banto").unwrap().unwrap().rc_dir.as_deref(), Some("director"), "the copy is the database before the rename");

    let unwritable = Path::new("/nonexistent-kernel-db-directory/kernel.db");
    db.conn.execute("UPDATE characters SET rc_dir = 'console' WHERE name = 'banto'", []).unwrap();
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::RC_DIR_RENAME_MIGRATION]).unwrap();
    let error = KernelDb::back_up_before_rc_dir_rename(&db.conn, unwritable).unwrap_err().to_string();
    assert!(error.contains("kernel.db.pre-rc-dir-rename") && error.contains("could not be made"), "{error}");
    KernelDb::migrate_rc_dir_rename(&db.conn).unwrap();
    KernelDb::back_up_before_rc_dir_rename(&db.conn, unwritable).expect("a renamed database needs no copy");
}

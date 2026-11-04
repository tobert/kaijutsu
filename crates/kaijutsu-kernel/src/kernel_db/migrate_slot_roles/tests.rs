use super::*;

/// A database shaped like one before `slot-roles-v1`: slots keyed by context
/// type on contexts and characters, and a live judge shadow. Rows are written
/// with raw SQL, since no verb writes these roles any more.
struct Rig {
    db: KernelDb,
    ws: WorkspaceId,
    deepseek: BackendId,
    mk: BackendId,
}

fn rig() -> Rig {
    rig_on(KernelDb::temporary().unwrap())
}

fn rig_on(db: KernelDb) -> Rig {
    let ws = setup_test_db(&db);
    let backend = |name: &str, kind: &str| {
        db.upsert_backend(&BackendRow {
            backend_id: BackendId::new(), name: name.into(), kind: kind.into(), base_url: None, api_key_env: None,
            api_key_file: None, key_optional: true, request_timeout_secs: None, idle_timeout_secs: None,
            max_concurrent: None, created_at: 0, created_by: PrincipalId::system(),
        }).unwrap().backend_id
    };
    let deepseek = backend("deepseek", "deepseek");
    let mk = backend("mk-zorak", "mk");
    Rig { db, ws, deepseek, mk }
}

impl Rig {
    fn character(&self, name: &str, context_type: Option<&str>) -> PrincipalId {
        let principal_id = PrincipalId::new();
        self.db.insert_character(&CharacterRow {
            rc_dir: Some(name.into()), console_ctx: None, mcp_hub_ctx: None,
            context_type: context_type.map(str::to_string), principal_id, name: name.into(), created_at: 0,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        principal_id
    }

    fn character_slot(&self, who: PrincipalId, role: &str, backend: BackendId, model: &str, max_tokens: Option<i64>) {
        self.db.conn.execute(
            "INSERT INTO character_slots (principal_id, role, backend_id, model, max_tokens, temperature)
             VALUES (?1, ?2, ?3, ?4, ?5, 0.4)",
            params![blob_param(who.as_bytes()), role, blob_param(backend.as_bytes()), model, max_tokens],
        ).unwrap();
    }

    fn context(&self, label: &str, context_type: &str) -> ContextRow {
        let mut row = make_context_row(Some(label));
        row.context_type = context_type.into();
        insert_context_with_doc(&self.db, &row, self.ws);
        row
    }

    fn context_slot(&self, ctx: &ContextRow, role: &str, model: &str) {
        self.db.conn.execute(
            "INSERT INTO context_slots (context_id, role, backend_id, model, max_tokens) VALUES (?1, ?2, ?3, ?4, 4096)",
            params![blob_param(ctx.context_id.as_bytes()), role, blob_param(self.deepseek.as_bytes()), model],
        ).unwrap();
    }

    fn context_roles(&self, ctx: &ContextRow) -> Vec<(String, String)> {
        self.db.list_context_slots(ctx.context_id).unwrap().into_iter().map(|s| (s.role, s.model)).collect()
    }

    fn character_roles(&self, who: PrincipalId) -> Vec<String> {
        self.db.list_character_slots(who).unwrap().into_iter().map(|s| s.role).collect()
    }

    fn forget(&self) {
        self.db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SLOT_ROLES_MIGRATION]).unwrap();
    }

    /// Forget the migration and run the opens' migrations again, as a
    /// database from before it would.
    fn migrate(&self) {
        self.forget();
        KernelDb::apply_additive_migrations(&self.db.conn).unwrap();
    }

    fn notes(&self) -> Vec<ContextCharacterMigrationNote> {
        self.db.context_character_migration_report().unwrap().into_iter().filter(|n| n.kind == "slot_dropped").collect()
    }
}

/// In each context the slot keyed by the context's type becomes `model`;
/// `judge` and every other type-keyed slot go; `distill` and `gate` stay.
///
/// Falsified by keeping a type-keyed slot, or by renaming a slot whose role
/// is not the context's own type.
#[test]
fn a_context_s_type_slot_becomes_model_and_other_type_slots_go() {
    let r = rig();
    let coder = r.context("coder-lane", "coder");
    for (role, model) in [("coder", "the-coder"), ("distill", "the-distill"), ("gate", "the-gate"), ("judge", "the-judge"), ("musician", "the-musician")] {
        r.context_slot(&coder, role, model);
    }
    let bare = r.context("bare", "mcp");
    r.context_slot(&bare, "distill", "the-distill");

    r.migrate();

    assert_eq!(
        r.context_roles(&coder),
        [("distill".into(), "the-distill".into()), ("gate".into(), "the-gate".into()), ("model".into(), "the-coder".into())],
    );
    assert_eq!(r.context_roles(&bare), [("distill".into(), "the-distill".into())], "no type slot, no model");
}

/// The never-changed triggers come back with the migration: after it, a
/// slot still refuses an update and a delete.
#[test]
fn the_never_changed_triggers_survive_the_migration() {
    let r = rig();
    let coder = r.context("coder-lane", "coder");
    r.context_slot(&coder, "coder", "the-coder");

    r.forget();
    KernelDb::migrate_slot_roles(&r.db.conn).unwrap();

    let id = blob_param(coder.context_id.as_bytes());
    let update = r.db.conn.execute("UPDATE context_slots SET model = 'x' WHERE context_id = ?1", params![id]).unwrap_err();
    assert!(update.to_string().contains("never changed"), "{update}");
    let delete = r.db.conn.execute("DELETE FROM context_slots WHERE context_id = ?1", params![id]).unwrap_err();
    assert!(delete.to_string().contains("never changed"), "{delete}");
}

/// A character with no model and a slot keyed by its own type takes that
/// slot as its model; every role other than `distill` and `gate` then goes,
/// and each dropped slot is reported with its character, role and model.
///
/// Falsified by adopting a slot whose role is not the character's type, by
/// replacing a model the character already has, or by dropping silently.
#[test]
fn a_character_s_type_slot_becomes_its_model_and_the_rest_are_reported() {
    let r = rig();
    let banto = r.character("banto", Some("director"));
    r.character_slot(banto, "director", r.deepseek, "ds-pro", Some(8192));
    r.character_slot(banto, "distill", r.deepseek, "ds-flash", Some(4096));
    r.character_slot(banto, "gate", r.mk, "gemma", None);
    r.character_slot(banto, "judge", r.mk, "gemma", None);
    let modeled = r.character("modeled", Some("coder"));
    r.db.set_character_model(&CharacterModelRow {
        principal_id: modeled, backend_id: r.deepseek, model: "kept".into(), max_tokens: Some(1000),
        temperature: None, top_p: None, effort: None, thinking_budget: None, thinking_style: None,
    }).unwrap();
    r.character_slot(modeled, "coder", r.deepseek, "ds-coder", Some(2000));
    let lead = r.character("kaijutsu-lead", None);
    r.character_slot(lead, "mcp", r.deepseek, "ds-mcp", Some(2000));

    r.migrate();

    let model = r.db.get_character_model(banto).unwrap().expect("the type slot became the model");
    assert_eq!((model.backend_id, model.model.as_str(), model.max_tokens, model.temperature), (r.deepseek, "ds-pro", Some(8192), Some(0.4)));
    assert_eq!(r.character_roles(banto), ["distill", "gate"]);
    assert_eq!(r.db.get_character_model(modeled).unwrap().unwrap().model, "kept", "a model is never replaced");
    assert!(r.character_roles(modeled).is_empty());
    assert!(r.db.get_character_model(lead).unwrap().is_none(), "a slot that is not the character's type is no model");
    assert!(r.character_roles(lead).is_empty());

    let notes = r.notes();
    let dropped: Vec<(Option<PrincipalId>, bool)> =
        notes.iter().map(|n| (n.principal_id, n.context_id.is_none())).collect();
    assert_eq!(dropped, [(Some(banto), true), (Some(lead), true), (Some(modeled), true)], "{notes:?}");
    assert!(notes[0].note.contains("banto") && notes[0].note.contains("`judge`") && notes[0].note.contains("mk-zorak/gemma"), "{}", notes[0].note);
    assert!(notes[1].note.contains("kaijutsu-lead") && notes[1].note.contains("`mcp`"), "{}", notes[1].note);
    assert!(notes[2].note.contains("modeled") && notes[2].note.contains("`coder`") && notes[2].note.contains("deepseek/ds-coder"), "{}", notes[2].note);
}

/// Live judge shadows are archived; the next gate call makes one performed
/// by the judge character. An archived one stays as it is.
#[test]
fn live_judge_shadows_are_archived() {
    let r = rig();
    let seat = r.context("seat", "coder");
    let live = r.context("judge-seat", "judge");
    r.context_slot(&live, "judge", "the-judge");
    r.context_slot(&live, "coder", "the-coder");
    let archived = r.context("judge-old", "judge");
    r.db.archive_context(archived.context_id).unwrap();
    let archived_at = r.db.get_context(archived.context_id).unwrap().unwrap().archived_at;

    r.migrate();

    let row = r.db.get_context(live.context_id).unwrap().unwrap();
    assert!(row.archived_at.is_some(), "the live shadow is archived");
    assert_eq!(row.context_state, kaijutsu_types::ContextState::Archived);
    assert_eq!(r.db.get_context(archived.context_id).unwrap().unwrap().archived_at, archived_at, "an archived one is left alone");
    assert!(r.db.get_context(seat.context_id).unwrap().unwrap().archived_at.is_none(), "the seat stays live");
    assert_eq!(
        r.context_roles(&live),
        [("distill".into(), "the-judge".into()), ("model".into(), "the-judge".into())],
        "its own type slot became model, and its distill fallback",
    );
}

/// The migration runs once: a row written after it is recorded is left
/// alone by the next open.
#[test]
fn the_migration_runs_once() {
    let r = rig();
    r.migrate();
    let late = r.context("late", "coder");
    r.context_slot(&late, "coder", "late-model");
    KernelDb::apply_additive_migrations(&r.db.conn).unwrap();
    assert_eq!(r.context_roles(&late), [("coder".into(), "late-model".into())]);
}

fn backed_up(path: &Path) -> bool {
    KernelDb::pre_slot_roles_backup_path(path).exists()
}

/// A fresh database records the migration and makes no copy; one with a
/// type-keyed slot and no record is copied aside first, and the copy holds
/// the slot as it was; one with nothing to change makes no copy.
#[test]
fn the_database_is_copied_aside_only_when_the_migration_changes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    drop(KernelDb::open(&path).unwrap());
    assert!(!backed_up(&path), "a fresh database makes no backup");
    {
        let db = KernelDb::open(&path).unwrap();
        let recorded: bool = db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)", [KernelDb::SLOT_ROLES_MIGRATION], |r| r.get(0),
        ).unwrap();
        assert!(recorded, "a fresh database records the migration");
        let r = rig_on(db);
        let kept = r.context("kept", "coder");
        r.context_slot(&kept, "distill", "the-distill");
        r.forget();
    }
    drop(KernelDb::open(&path).unwrap());
    assert!(!backed_up(&path), "a distill slot is nothing to change");
    {
        let db = KernelDb::open(&path).unwrap();
        let ws = db.get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let deepseek = db.get_backend_by_name("deepseek").unwrap().unwrap().backend_id;
        let r = Rig { db, ws, deepseek, mk: deepseek };
        let lane = r.context("lane", "coder");
        r.context_slot(&lane, "coder", "the-coder");
        r.forget();
    }
    let db = KernelDb::open(&path).unwrap();
    assert!(backed_up(&path), "a type-keyed slot needs the copy");
    let lane = db.find_context_by_label("lane").unwrap().unwrap();
    assert!(db.context_slot(lane.context_id, "model").unwrap().is_some(), "the open migrated it");
    let copy = KernelDb::open_read_only(&KernelDb::pre_slot_roles_backup_path(&path)).unwrap();
    assert!(copy.context_slot(lane.context_id, "coder").unwrap().is_some(), "the copy is the database before the migration");

    let unwritable = Path::new("/nonexistent-kernel-db-directory/kernel.db");
    let deepseek = db.get_backend_by_name("deepseek").unwrap().unwrap().backend_id;
    let r = Rig { db, ws: WorkspaceId::new(), deepseek, mk: deepseek };
    let stray = r.character("stray", None);
    r.character_slot(stray, "coder", r.deepseek, "m", Some(10));
    r.forget();
    let error = KernelDb::back_up_before_slot_roles(&r.db.conn, unwritable).unwrap_err().to_string();
    assert!(error.contains("kernel.db.pre-slot-roles") && error.contains("could not be made"), "{error}");
    KernelDb::migrate_slot_roles(&r.db.conn).unwrap();
    KernelDb::back_up_before_slot_roles(&r.db.conn, unwritable).expect("a migrated database needs no copy");
}

/// A database whose `contexts` and `characters` no longer have
/// `context_type` opens: the migration drops the roles it can tell are not
/// `model`, `distill` or `gate`, and records itself.
#[test]
fn the_migration_records_itself_when_context_type_is_gone() {
    let r = rig();
    let lane = r.context("lane", "coder");
    r.context_slot(&lane, "judge", "the-judge");
    r.context_slot(&lane, "distill", "the-distill");
    r.db.conn.execute("ALTER TABLE contexts DROP COLUMN context_type", []).unwrap();
    r.db.conn.execute("ALTER TABLE characters DROP COLUMN context_type", []).unwrap();
    r.forget();

    KernelDb::migrate_slot_roles(&r.db.conn).unwrap();

    let recorded: bool = r.db.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)", [KernelDb::SLOT_ROLES_MIGRATION], |r| r.get(0),
    ).unwrap();
    assert!(recorded);
    let roles: Vec<String> = {
        let mut stmt = r.db.conn.prepare("SELECT role FROM context_slots WHERE context_id = ?1 ORDER BY role").unwrap();
        stmt.query_map(params![blob_param(lane.context_id.as_bytes())], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(roles, ["distill"]);
}

impl Rig {
    /// Make `who` the performer of `ctx`, as a context made before the slot
    /// roles moved would have.
    fn perform(&self, ctx: &ContextRow, who: PrincipalId) {
        self.db.conn.execute(
            "UPDATE contexts SET performer = ?1 WHERE context_id = ?2",
            params![blob_param(who.as_bytes()), blob_param(ctx.context_id.as_bytes())],
        ).unwrap();
    }

    fn no_model_notes(&self) -> Vec<ContextCharacterMigrationNote> {
        self.db.context_character_migration_report().unwrap().into_iter().filter(|n| n.kind == "no_model").collect()
    }
}

/// A live context with a performer whose slots hold no role equal to its
/// type had no model, and keeps none; the migration reports it on the
/// context, naming the roles it dropped and the rotation that gives the
/// context its character's model. A context with a model, one with no
/// performer, and an archived one are not reported.
///
/// Falsified by dropping the slots silently, or by reporting a context that
/// kept a model.
#[test]
fn a_context_left_with_no_model_is_reported_with_its_rotation() {
    let r = rig();
    let coder = r.character("coder", Some("coder"));
    let odd = r.context("odd-lane", "coder");
    r.perform(&odd, coder);
    r.context_slot(&odd, "musician", "the-musician");
    r.context_slot(&odd, "judge", "the-judge");
    let fine = r.context("fine-lane", "coder");
    r.perform(&fine, coder);
    r.context_slot(&fine, "coder", "the-coder");
    r.context_slot(&fine, "musician", "the-musician");
    let unplayed = r.context("unplayed", "coder");
    r.context_slot(&unplayed, "musician", "the-musician");
    let gone = r.context("gone-lane", "coder");
    r.perform(&gone, coder);
    r.context_slot(&gone, "musician", "the-musician");
    r.db.archive_context(gone.context_id).unwrap();

    r.migrate();

    assert!(r.context_roles(&odd).is_empty(), "it had no model and keeps none");
    let notes = r.no_model_notes();
    assert_eq!(notes.iter().map(|n| n.context_id).collect::<Vec<_>>(), [Some(odd.context_id)], "{notes:?}");
    let note = &notes[0].note;
    assert!(note.contains("`judge`") && note.contains("`musician`"), "names the dropped roles: {note}");
    assert!(note.contains("`kj context rotate odd-lane`"), "names the fix: {note}");
}

/// An old slot that breaks a `character_models` CHECK cannot become its
/// character's model. The open goes on: the slot is dropped and reported,
/// saying why, and a slot whose character has no sheet is dropped with no
/// report row to name it by.
///
/// Falsified by failing the open on either row.
#[test]
fn an_old_slot_that_breaks_a_check_is_reported_and_the_open_goes_on() {
    let r = rig();
    let banto = r.character("banto", Some("director"));
    r.db.conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
    r.db.conn.execute(
        "INSERT INTO character_slots (principal_id, role, backend_id, model, max_tokens, temperature)
         VALUES (?1, 'director', ?2, 'ds-pro', 8192, 9.0)",
        params![blob_param(banto.as_bytes()), blob_param(r.deepseek.as_bytes())],
    ).unwrap();
    r.db.conn.execute_batch("PRAGMA ignore_check_constraints = OFF; PRAGMA foreign_keys = OFF").unwrap();
    let nobody = PrincipalId::new();
    r.db.conn.execute(
        "INSERT INTO character_slots (principal_id, role, backend_id, model, max_tokens) VALUES (?1, 'coder', ?2, 'm', 10)",
        params![blob_param(nobody.as_bytes()), blob_param(r.deepseek.as_bytes())],
    ).unwrap();
    r.db.conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();

    // Only this migration: the open's earlier table rebuilds refuse a
    // dangling foreign key of their own.
    r.forget();
    KernelDb::migrate_slot_roles(&r.db.conn).unwrap();

    assert!(r.db.get_character_model(banto).unwrap().is_none(), "the slot could not become the model");
    assert!(r.character_roles(banto).is_empty(), "and is dropped");
    let notes = r.notes();
    assert_eq!(notes.iter().map(|n| n.principal_id).collect::<Vec<_>>(), [Some(banto)], "{notes:?}");
    assert!(notes[0].note.contains("`director`") && notes[0].note.contains("could not become"), "{}", notes[0].note);
    let left: i64 = r.db.conn.query_row(
        "SELECT COUNT(*) FROM character_slots WHERE role NOT IN ('distill', 'gate')", [], |row| row.get(0),
    ).unwrap();
    assert_eq!(left, 0, "every old role is gone");
}

/// A context whose model moved to role `model` on a chat backend, with no
/// `distill` slot, distills on that model, as a context made now does; one
/// with its own `distill` keeps it, and one whose model is a council server
/// gets none.
///
/// Falsified by leaving a migrated chat context with nothing to distill on.
#[test]
fn a_migrated_chat_model_is_the_distill_model_of_a_context_with_none() {
    let r = rig();
    let lane = r.context("lane", "coder");
    r.context_slot(&lane, "coder", "the-coder");
    let kept = r.context("kept", "coder");
    r.context_slot(&kept, "coder", "the-coder");
    r.context_slot(&kept, "distill", "the-distill");
    let council = r.context("council-lane", "council");
    r.db.conn.execute(
        "INSERT INTO context_slots (context_id, role, backend_id, model) VALUES (?1, 'council', ?2, 'gemma')",
        params![blob_param(council.context_id.as_bytes()), blob_param(r.mk.as_bytes())],
    ).unwrap();

    r.migrate();

    assert_eq!(r.context_roles(&lane), [("distill".into(), "the-coder".into()), ("model".into(), "the-coder".into())]);
    let distill = r.db.context_slot(lane.context_id, "distill").unwrap().unwrap();
    assert_eq!((distill.backend_id, distill.max_tokens), (r.deepseek, Some(4096)), "a copy of the model row");
    assert_eq!(r.context_roles(&kept), [("distill".into(), "the-distill".into()), ("model".into(), "the-coder".into())]);
    assert_eq!(r.context_roles(&council), [("model".into(), "gemma".into())]);
}

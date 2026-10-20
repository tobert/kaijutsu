use rusqlite::params;

use kaijutsu_types::{DocKind, PresetId, TurnId, WorkspaceId};

use super::*;
use crate::kernel_db::{
    BackendModelRow, BackendRow, CastRow, CastSlotRow, CharacterRow, ContextRow, DocumentRow, ModelTunablesRow,
    blob_param, insert_context_with_doc, into_pre_instance, make_context_row, now_millis,
    set_stored_played_by, setup_test_db,
};

struct World {
    db: KernelDb,
    ws: WorkspaceId,
    anthropic: BackendId,
    deepseek: BackendId,
    /// Next `created_at` a test context takes, so creation order is explicit.
    clock: std::cell::Cell<i64>,
    /// Whether the database has the pre-instance columns a migration reads.
    legacy: bool,
}

/// A database on the current schema.
fn world() -> World {
    build_world(false)
}

/// A database with the schema `instances-v1` migrates from.
fn legacy_world() -> World {
    build_world(true)
}

fn build_world(legacy: bool) -> World {
    let db = KernelDb::temporary().unwrap();
    if legacy {
        into_pre_instance(&db);
    }
    let ws = setup_test_db(&db);
    let backend = |name: &str, kind: &str| {
        db.upsert_backend(&BackendRow {
            backend_id: BackendId::new(),
            name: name.into(),
            kind: kind.into(),
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: false,
            request_timeout_secs: None,
            idle_timeout_secs: None,
            max_concurrent: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        })
        .unwrap()
        .backend_id
    };
    let anthropic = backend("anthropic", "anthropic");
    let deepseek = backend("deepseek", "deepseek");
    World { db, ws, anthropic, deepseek, clock: std::cell::Cell::new(1_000), legacy }
}

impl World {
    fn character(&self, name: &str) -> PrincipalId {
        let principal_id = PrincipalId::new();
        self.db
            .insert_character(&CharacterRow {
                principal_id,
                name: name.into(),
                created_at: 0,
                retired_at: None,
                handoff_ctx: None,
                root_ctx: None,
                root: false,
            })
            .unwrap();
        principal_id
    }

    fn model_row(&self, backend: BackendId, model: &str, tunables: ModelTunablesRow, window: Option<i64>) {
        self.db
            .set_backend_model(&BackendModelRow {
                backend_id: backend,
                model_id: model.into(),
                context_window: window,
                extra: None,
                tunables,
            })
            .unwrap();
    }

    fn slot(&self, cast: CastId, role: &str, backend: BackendId, model: &str, max_tokens: Option<i64>) -> CastSlotRow {
        let row = CastSlotRow {
            cast_id: cast,
            role: role.into(),
            backend_id: backend,
            model: model.into(),
            max_tokens,
            temperature: None,
            top_p: None,
            effort: None,
            thinking_budget: None,
            thinking_style: None,
            loadout: None,
            extra: None,
        };
        self.db.set_cast_slot(&row).unwrap();
        row
    }

    fn cast(&self, label: &str) -> CastId {
        let cast_id = CastId::new();
        self.db
            .insert_cast(&CastRow {
                cast_id,
                label: label.into(),
                description: None,
                created_at: 0,
                created_by: PrincipalId::system(),
            })
            .unwrap();
        cast_id
    }

    fn default_cast(&self, character: PrincipalId, cast: CastId) {
        let column = if self.legacy { "default_cast_id" } else { "cast_id" };
        self.db
            .conn
            .execute(
                &format!("UPDATE characters SET {column} = ?1 WHERE principal_id = ?2"),
                params![blob_param(cast.as_bytes()), blob_param(character.as_bytes())],
            )
            .unwrap();
    }

    /// The standard cast: coder, judge and gate slots on deepseek `ds-flash`, whose
    /// model row carries `max_tokens` 4096 and a 64000 window. The judge slot sets
    /// its own `max_tokens`.
    fn house(&self) -> CastId {
        self.model_row(
            self.deepseek,
            "ds-flash",
            ModelTunablesRow { max_tokens: Some(4096), top_p: Some(0.9), ..Default::default() },
            Some(64_000),
        );
        let house = self.cast("house");
        self.slot(house, "coder", self.deepseek, "ds-flash", None);
        self.slot(house, "judge", self.deepseek, "ds-flash", Some(2048));
        self.slot(house, "gate", self.deepseek, "ds-flash", None);
        house
    }

    fn ctx(&self, label: &str, ty: &str, played_by: Option<PrincipalId>, forked_from: Option<&ContextRow>) -> ContextRow {
        assert!(self.legacy || played_by.is_none(), "only a legacy database stores a performer; start an instance instead");
        let mut row = make_context_row(Some(label));
        row.context_type = ty.into();
        row.forked_from = forked_from.map(|f| f.context_id);
        row.created_at = self.clock.get();
        self.clock.set(self.clock.get() + 1);
        insert_context_with_doc(&self.db, &row, self.ws);
        if played_by.is_some() {
            set_stored_played_by(&self.db, row.context_id, played_by);
        }
        row.played_by = played_by;
        row
    }

    fn exec(&self, sql: &str, params: impl rusqlite::Params) {
        self.db.conn.execute(sql, params).unwrap();
    }

    fn archive(&self, ctx: &ContextRow) {
        self.exec(
            "UPDATE contexts SET archived_at = 5 WHERE context_id = ?1",
            params![blob_param(ctx.context_id.as_bytes())],
        );
    }

    fn set_cast(&self, ctx: &ContextRow, cast: CastId) {
        self.exec(
            "UPDATE contexts SET cast_id = ?1 WHERE context_id = ?2",
            params![blob_param(cast.as_bytes()), blob_param(ctx.context_id.as_bytes())],
        );
    }

    fn set_senpai(&self, ctx: &ContextRow, senpai: &ContextRow) {
        self.exec(
            "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(senpai.context_id.as_bytes()), blob_param(ctx.context_id.as_bytes())],
        );
    }

    fn set_override(&self, ctx: &ContextRow, provider: &str, model: &str) {
        self.exec(
            "UPDATE contexts SET provider = ?1, model = ?2 WHERE context_id = ?3",
            params![provider, model, blob_param(ctx.context_id.as_bytes())],
        );
    }

    fn rotated_from(&self, ctx: &ContextRow, value: &str) {
        self.db.set_context_env(ctx.context_id, "ROTATED_FROM", value).unwrap();
    }

    /// Run `instances-v1` the way an upgrading kernel does: the marker is absent.
    fn migrate(&self) -> KernelDbResult<()> {
        self.exec("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::INSTANCES_MIGRATION]);
        KernelDb::migrate_instances(&self.db.conn)
    }

    fn instance(&self, ctx: &ContextRow) -> Option<CharacterInstanceId> {
        self.db.context_instance(ctx.context_id).unwrap().map(|(instance, _)| instance)
    }

    fn seq(&self, ctx: &ContextRow) -> Option<i64> {
        self.db.context_instance(ctx.context_id).unwrap().map(|(_, seq)| seq)
    }

    fn count(&self, table: &str) -> i64 {
        self.db.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    fn notes(&self, kind: &str) -> Vec<InstanceMigrationNote> {
        self.db.instance_migration_report().unwrap().into_iter().filter(|n| n.kind == kind).collect()
    }
}

// ── API ─────────────────────────────────────────────────────────────

#[test]
fn start_copies_the_cast_and_fills_unset_parameters_from_the_model_row() {
    let w = world();
    let house = w.house();
    let mut coder = w.slot(house, "coder", w.deepseek, "ds-flash", None);
    coder.temperature = Some(0.2);
    w.db.set_cast_slot(&coder).unwrap();
    let who = w.character("coder");
    let conv = w.ctx("conv", "coder", None, None);

    let instance = w.db.start_instance(who, None, Some(house), Some("be terse"), conv.context_id, who).unwrap();

    let slot = w.db.binding_slot(instance.instance_id, 1, "coder").unwrap().expect("coder slot");
    assert_eq!(slot.max_tokens, Some(4096), "the model row supplies the slot's unset max_tokens");
    assert_eq!(slot.temperature, Some(0.2), "a parameter the slot sets wins");
    assert_eq!(slot.top_p, Some(0.9), "an unset parameter takes the model row's value");
    assert_eq!(slot.context_window, Some(64_000));
    assert_eq!(slot.effort, None, "an unset parameter stays unset: no value of our own");
    let judge = w.db.binding_slot(instance.instance_id, 1, "judge").unwrap().unwrap();
    assert_eq!(judge.max_tokens, Some(2048), "the slot's own max_tokens wins over the model row");
    let bindings = w.db.list_instance_bindings(instance.instance_id).unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].cast_label.as_deref(), Some("house"));
    assert_eq!(bindings[0].persona.as_deref(), Some("be terse"));
    assert_eq!(w.db.context_instance(conv.context_id).unwrap(), Some((instance.instance_id, 1)));
    assert_eq!(w.db.instance_conversation(instance.instance_id).unwrap(), Some(conv.context_id));
}

#[test]
fn a_later_cast_change_moves_no_binding_and_an_appended_binding_reads_it_fresh() {
    let w = world();
    let house = w.house();
    let who = w.character("coder");
    let first = w.ctx("first", "coder", None, None);
    let instance = w.db.start_instance(who, None, Some(house), None, first.context_id, who).unwrap();

    w.model_row(w.anthropic, "claude-x", ModelTunablesRow { max_tokens: Some(9000), ..Default::default() }, None);
    w.slot(house, "coder", w.anthropic, "claude-x", None);
    let second = w.ctx("second", "coder", None, None);
    let binding = w.db.append_binding(instance.instance_id, Some(house), None, second.context_id).unwrap();

    assert_eq!(binding.seq, 2);
    let old = w.db.binding_slot(instance.instance_id, 1, "coder").unwrap().unwrap();
    assert_eq!((old.model.as_str(), old.backend_id), ("ds-flash", w.deepseek), "seq 1 is a copy");
    let new = w.db.binding_slot(instance.instance_id, 2, "coder").unwrap().unwrap();
    assert_eq!((new.model.as_str(), new.backend_id, new.max_tokens), ("claude-x", w.anthropic, Some(9000)));
    assert_eq!(w.db.instance_conversation(instance.instance_id).unwrap(), Some(second.context_id));
    assert_eq!(w.db.context_instance(first.context_id).unwrap(), Some((instance.instance_id, 1)));
    assert_eq!(w.db.context_instance(second.context_id).unwrap(), Some((instance.instance_id, 2)));
}

#[test]
fn a_slot_with_no_max_tokens_anywhere_refuses_naming_cast_role_and_model() {
    let w = world();
    let bare = w.cast("bare");
    w.slot(bare, "coder", w.deepseek, "no-row-model", None);
    let who = w.character("coder");
    let conv = w.ctx("conv", "coder", None, None);

    let error = w.db.start_instance(who, None, Some(bare), None, conv.context_id, who).unwrap_err().to_string();

    for needle in ["bare", "coder", "no-row-model", "max_tokens"] {
        assert!(error.contains(needle), "{needle} missing from: {error}");
    }
    assert_eq!(w.count("character_instances"), 0, "a refused start writes nothing");
    assert_eq!(w.count("instance_bindings"), 0);
    assert_eq!(w.db.context_instance(conv.context_id).unwrap(), None);

    let house = w.house();
    let instance = w.db.start_instance(who, None, Some(house), None, conv.context_id, who).unwrap();
    let next = w.ctx("next", "coder", None, None);
    w.slot(house, "coder", w.deepseek, "no-row-model", None);
    let error = w.db.append_binding(instance.instance_id, Some(house), None, next.context_id).unwrap_err().to_string();
    assert!(error.contains("house") && error.contains("coder") && error.contains("no-row-model"), "{error}");
    assert_eq!(w.db.list_instance_bindings(instance.instance_id).unwrap().len(), 1, "a refused append writes nothing");
    assert_eq!(w.db.context_instance(next.context_id).unwrap(), None);
}

#[test]
fn a_binding_may_have_no_slot_for_a_role() {
    let w = world();
    let house = w.house();
    let who = w.character("coder");
    let with_cast = w.ctx("with-cast", "coder", None, None);
    let instance = w.db.start_instance(who, None, Some(house), None, with_cast.context_id, who).unwrap();
    assert!(w.db.binding_slot(instance.instance_id, 1, "musician").unwrap().is_none());

    let castless = w.ctx("castless", "coder", None, None);
    let other = w.db.start_instance(who, None, None, None, castless.context_id, who).unwrap();
    assert!(w.db.binding_slot(other.instance_id, 1, "coder").unwrap().is_none());
    assert!(w.db.list_binding_slots(other.instance_id, 1).unwrap().is_empty());
    assert_eq!(w.db.list_instance_bindings(other.instance_id).unwrap()[0].cast_label, None);
}

#[test]
fn bindings_slots_and_context_membership_are_never_changed() {
    let w = world();
    let house = w.house();
    let who = w.character("coder");
    let conv = w.ctx("conv", "coder", None, None);
    let other = w.ctx("other", "coder", None, None);
    let instance = w.db.start_instance(who, None, Some(house), None, conv.context_id, who).unwrap();
    let second = w.db.start_instance(who, None, None, None, other.context_id, who).unwrap();

    for (sql, what) in [
        ("UPDATE instance_bindings SET persona = 'x'", "binding update"),
        ("DELETE FROM instance_bindings", "binding delete"),
        ("UPDATE instance_slots SET max_tokens = 1", "slot update"),
        ("DELETE FROM instance_slots", "slot delete"),
    ] {
        let error = w.db.conn.execute(sql, []).unwrap_err().to_string();
        assert!(error.contains("never changed"), "{what}: {error}");
    }
    let error = w.db.conn
        .execute(
            "UPDATE contexts SET instance_id = ?1 WHERE context_id = ?2",
            params![blob_param(second.instance_id.as_bytes()), blob_param(conv.context_id.as_bytes())],
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("set once"), "{error}");
    let error = w.db.conn
        .execute("UPDATE contexts SET binding_seq = 7 WHERE context_id = ?1", params![blob_param(conv.context_id.as_bytes())])
        .unwrap_err()
        .to_string();
    assert!(error.contains("set once"), "{error}");
    w.exec("UPDATE contexts SET label = 'renamed' WHERE context_id = ?1", params![blob_param(conv.context_id.as_bytes())]);
    assert_eq!(w.db.context_instance(conv.context_id).unwrap(), Some((instance.instance_id, 1)));
}

/// An instance is live while its conversation is: archiving the newest
/// binding's conversation makes it inactive, and promoting it makes it live
/// again. Forks and other contexts of the instance do not decide.
#[test]
fn an_instance_is_inactive_while_its_conversation_is_archived() {
    let w = world();
    let who = w.character("coder");
    let conv = w.ctx("conv", "coder", None, None);
    let fork = w.ctx("fork", "coder", None, Some(&conv));
    let instance = w.db.start_instance(who, None, None, None, conv.context_id, who).unwrap().instance_id;
    w.db.join_instance(fork.context_id, instance, 1).unwrap();
    assert!(w.db.instance_is_active(instance).unwrap());

    w.db.archive_context(fork.context_id).unwrap();
    assert!(w.db.instance_is_active(instance).unwrap(), "an archived fork leaves the instance live");
    w.db.archive_context(conv.context_id).unwrap();
    assert!(!w.db.instance_is_active(instance).unwrap(), "an archived conversation makes the instance inactive");

    let next = w.ctx("next", "coder", None, None);
    w.db.append_binding(instance, None, None, next.context_id).unwrap();
    assert!(w.db.instance_is_active(instance).unwrap(), "the newest binding's conversation decides, not the first");
    w.db.archive_context(next.context_id).unwrap();
    assert!(!w.db.instance_is_active(instance).unwrap());
    w.db.promote_context(next.context_id).unwrap();
    assert!(w.db.instance_is_active(instance).unwrap(), "promoting the conversation makes the instance live again");
    assert!(matches!(w.db.instance_is_active(CharacterInstanceId::new()).unwrap_err(), KernelDbError::NotFound(_)));
}

/// Every reader of a context's performer gets the principal of its instance;
/// a root's console is its root instance's conversation.
#[test]
fn the_performer_and_the_root_console_are_read_from_the_instance() {
    let w = world();
    let who = w.character("coder");
    let conv = w.ctx("conv", "coder", None, None);
    assert_eq!(w.db.get_context(conv.context_id).unwrap().unwrap().played_by, None, "a context with no instance performs nothing");
    w.db.start_instance(who, None, None, None, conv.context_id, who).unwrap();
    for read in [
        w.db.get_context(conv.context_id).unwrap().unwrap(),
        w.db.find_context_by_label("conv").unwrap().unwrap(),
        w.db.list_active_contexts().unwrap().into_iter().find(|r| r.context_id == conv.context_id).unwrap(),
        w.db.list_all_contexts().unwrap().into_iter().find(|r| r.context_id == conv.context_id).unwrap(),
        w.db.fork_lineage(conv.context_id).unwrap().remove(0).0,
    ] {
        assert_eq!(read.played_by, Some(who), "the instance's character");
    }
    assert_eq!(w.db.contexts_played_by(who).unwrap().len(), 1);

    let amy = w.character("amy");
    w.db.update_character_root(amy, true).unwrap();
    let console = w.ctx("amy-console", "root", None, None);
    assert_eq!(w.db.get_character(amy).unwrap().unwrap().root_ctx, None);
    w.db.start_instance(amy, None, None, None, console.context_id, amy).unwrap();
    for sheet in [
        w.db.get_character(amy).unwrap().unwrap(),
        w.db.get_character_by_name("amy").unwrap().unwrap(),
        w.db.list_characters(false).unwrap().into_iter().find(|c| c.principal_id == amy).unwrap(),
    ] {
        assert_eq!(sheet.root_ctx, Some(console.context_id));
    }
}

/// A row that sets `played_by` is refused: the performer is the instance's
/// character, and no column stores it.
#[test]
fn a_context_row_that_sets_the_performer_is_refused() {
    let w = world();
    let who = w.character("coder");
    let mut row = make_context_row(Some("sets-played-by"));
    w.db.insert_document(&DocumentRow {
        document_id: row.context_id,
        workspace_id: w.ws,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: row.created_at,
        created_by: row.created_by,
    }).unwrap();
    row.played_by = Some(who);
    let error = w.db.insert_context(&row).unwrap_err().to_string();
    assert!(error.contains("played_by") && error.contains("instance"), "{error}");
    assert!(w.db.get_context(row.context_id).unwrap().is_none());
    row.played_by = None;
    w.db.insert_context(&row).expect("the same row without a performer is accepted");
}

#[test]
fn a_context_joins_one_instance_once() {
    let w = world();
    let house = w.house();
    let who = w.character("coder");
    let conv = w.ctx("conv", "coder", None, None);
    let fork = w.ctx("fork", "coder", None, Some(&conv));
    let instance = w.db.start_instance(who, None, Some(house), None, conv.context_id, who).unwrap();

    w.db.join_instance(fork.context_id, instance.instance_id, 1).unwrap();
    assert_eq!(w.db.context_instance(fork.context_id).unwrap(), Some((instance.instance_id, 1)));
    assert!(w.db.join_instance(fork.context_id, instance.instance_id, 1).is_err(), "no second join");
    let late = w.ctx("late", "coder", None, None);
    assert!(w.db.join_instance(late.context_id, instance.instance_id, 9).is_err(), "a seq with no binding");
    assert_eq!(w.db.context_instance(late.context_id).unwrap(), None);

    let again = w.db.start_instance(who, None, None, None, conv.context_id, who);
    assert!(again.is_err(), "a context already in an instance cannot be the conversation of another");
    assert_eq!(w.count("character_instances"), 1);
}

// ── Migration `instances-v1` ────────────────────────────────────────

#[test]
fn a_rotation_chain_becomes_one_instance() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let first = w.ctx("seat-1", "coder", Some(who), None);
    let second = w.ctx("seat-2", "coder", Some(who), None);
    let third = w.ctx("seat-3", "coder", Some(who), None);
    let unrelated = w.ctx("unrelated", "coder", Some(who), None);
    w.rotated_from(&second, &first.context_id.to_hex());
    w.rotated_from(&third, &second.context_id.to_hex());
    w.archive(&first);
    w.archive(&second);

    w.migrate().unwrap();

    let instance = w.instance(&first).expect("mapped");
    assert_eq!(w.instance(&second), Some(instance));
    assert_eq!(w.instance(&third), Some(instance));
    assert_ne!(w.instance(&unrelated), Some(instance), "no link, no join");
    assert_eq!(w.db.get_instance(instance).unwrap().unwrap().principal_id, who);
    assert_eq!(w.db.list_instance_bindings(instance).unwrap().len(), 1, "equal resolutions share a seq");
    assert_eq!(w.db.instance_conversation(instance).unwrap(), Some(third.context_id), "newest live context");
    assert_eq!(w.count("character_instances"), 2);
}

#[test]
fn a_fork_child_joins_its_source_and_a_fork_by_another_performer_starts_a_kohai() {
    let w = legacy_world();
    let house = w.house();
    let [lead, coder] = [w.character("lead"), w.character("coder")];
    w.default_cast(lead, house);
    w.default_cast(coder, house);
    let seat = w.ctx("seat", "coder", Some(lead), None);
    let fork = w.ctx("fork", "coder", Some(lead), Some(&seat));
    let lane = w.ctx("lane", "coder", Some(coder), Some(&seat));
    w.set_senpai(&lane, &seat);

    w.migrate().unwrap();

    assert_eq!(w.instance(&fork), w.instance(&seat));
    assert_eq!(w.seq(&fork), w.seq(&seat), "a fork runs on its source's binding");
    let seat_instance = w.instance(&seat).unwrap();
    assert_eq!(w.db.instance_conversation(seat_instance).unwrap(), Some(seat.context_id), "a newer fork child is not the conversation");
    let lane_instance = w.instance(&lane).unwrap();
    assert_ne!(lane_instance, seat_instance);
    let row = w.db.get_instance(lane_instance).unwrap().unwrap();
    assert_eq!(row.principal_id, coder);
    assert_eq!(row.senpai_instance, Some(seat_instance), "the senpai is the instance of the first context's senpai_ctx");
    assert_eq!(w.db.get_instance(seat_instance).unwrap().unwrap().senpai_instance, None);
}

#[test]
fn a_judge_shadow_joins_the_instance_it_judges() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let seat = w.ctx("seat", "coder", Some(who), None);
    let live_shadow = w.ctx("shadow", "judge", None, Some(&seat));
    let old_shadow = w.ctx("old-shadow", "judge", None, Some(&seat));
    w.set_cast(&live_shadow, house);
    w.set_cast(&old_shadow, house);
    w.archive(&old_shadow);

    w.migrate().unwrap();

    let instance = w.instance(&seat).unwrap();
    assert_eq!(w.instance(&live_shadow), Some(instance));
    assert_eq!(w.instance(&old_shadow), Some(instance), "archived shadows are found too");
    assert_eq!(w.seq(&live_shadow), w.seq(&seat), "the shadow runs on the seat's binding");
    assert_eq!(w.db.instance_conversation(instance).unwrap(), Some(seat.context_id));
    assert_eq!(w.count("character_instances"), 1);
}

#[test]
fn a_shared_cast_is_copied_into_each_binding() {
    let w = legacy_world();
    let house = w.house();
    let [one, two] = [w.character("one"), w.character("two")];
    w.default_cast(one, house);
    w.default_cast(two, house);
    let a = w.ctx("a", "coder", Some(one), None);
    let b = w.ctx("b", "coder", Some(two), None);

    w.migrate().unwrap();

    let (a, b) = (w.instance(&a).unwrap(), w.instance(&b).unwrap());
    assert_ne!(a, b);
    for instance in [a, b] {
        let roles: Vec<String> = w.db.list_binding_slots(instance, 1).unwrap().into_iter().map(|s| s.role).collect();
        assert_eq!(roles, vec!["coder", "gate", "judge"], "the cast's own slots, nothing more");
    }
    assert_eq!(w.count("instance_slots"), 6);
    let slot = w.db.binding_slot(a, 1, "coder").unwrap().unwrap();
    assert_eq!((slot.max_tokens, slot.top_p, slot.context_window), (Some(4096), Some(0.9), Some(64_000)));
    assert_eq!(w.db.list_instance_bindings(a).unwrap()[0].cast_label.as_deref(), Some("house"));
}

#[test]
fn a_context_with_an_override_keeps_its_model() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let seat = w.ctx("seat", "coder", Some(who), None);
    let overridden = w.ctx("overridden", "coder", Some(who), Some(&seat));
    w.set_override(&overridden, "anthropic", "claude-x");

    w.migrate().unwrap();

    let instance = w.instance(&seat).unwrap();
    assert_eq!(w.instance(&overridden), Some(instance));
    assert_eq!(w.seq(&overridden), Some(1));
    assert_eq!(w.seq(&seat), Some(2), "a different resolution gets its own seq; the conversation's is last");
    let slot = w.db.binding_slot(instance, 1, "coder").unwrap().unwrap();
    assert_eq!((slot.backend_id, slot.model.as_str()), (w.anthropic, "claude-x"));
    assert_eq!(slot.max_tokens, Some(64_000), "nothing set it: the old code sent 64000");
    let kept = w.db.binding_slot(instance, 2, "coder").unwrap().unwrap();
    assert_eq!(kept.model, "ds-flash");
    assert!(
        w.db.binding_slot(instance, 1, "judge").unwrap().is_some(),
        "the override replaces only the context's own role: the cast's other slots stay"
    );
}

#[test]
fn a_context_resolved_through_llm_defaults_keeps_that_model() {
    let w = legacy_world();
    w.exec(
        "INSERT INTO llm_defaults (id, default_backend, default_model, max_tokens, temperature)
         VALUES (1, 'deepseek', 'ds-lite', 1000, 0.5)",
        [],
    );
    let who = w.character("plain");
    let ctx = w.ctx("plain", "coder", Some(who), None);

    w.migrate().unwrap();

    let instance = w.instance(&ctx).unwrap();
    let slot = w.db.binding_slot(instance, 1, "coder").unwrap().expect("resolved through the default");
    assert_eq!((slot.backend_id, slot.model.as_str()), (w.deepseek, "ds-lite"));
    assert_eq!((slot.max_tokens, slot.temperature), (Some(1000), Some(0.5)), "the llm_defaults floor is what the old code sent");
    assert_eq!(w.db.list_instance_bindings(instance).unwrap()[0].cast_label, None);
    assert!(w.notes("unresolved").is_empty());
}

#[test]
fn max_tokens_is_pinned_slot_then_model_then_defaults_then_64000() {
    let w = legacy_world();
    w.model_row(w.deepseek, "with-row", ModelTunablesRow { max_tokens: Some(7000), ..Default::default() }, None);
    let cast = w.cast("pins");
    w.slot(cast, "coder", w.deepseek, "ds-slot", Some(1234));
    w.slot(cast, "judge", w.deepseek, "with-row", None);
    w.slot(cast, "gate", w.deepseek, "bare", None);
    w.slot(cast, "distill", w.deepseek, "bare", None);
    let who = w.character("pins");
    w.default_cast(who, cast);
    let ctx = w.ctx("pins", "coder", Some(who), None);

    w.migrate().unwrap();

    let instance = w.instance(&ctx).unwrap();
    let max = |role: &str| w.db.binding_slot(instance, 1, role).unwrap().unwrap().max_tokens;
    assert_eq!((max("coder"), max("judge")), (Some(1234), Some(7000)));
    assert_eq!(max("distill"), Some(64_000));
    assert_eq!(
        w.db.binding_slot(instance, 1, "gate").unwrap().unwrap().max_tokens,
        None,
        "a gate slot names a council server: no max_tokens is pinned"
    );
}

#[test]
fn an_unresolvable_context_gets_a_binding_with_no_slot_and_is_listed() {
    let w = legacy_world();
    let who = w.character("lost");
    let ctx = w.ctx("lost", "coder", Some(who), None);

    w.migrate().unwrap();

    let instance = w.instance(&ctx).expect("the instance exists");
    assert!(w.db.list_binding_slots(instance, 1).unwrap().is_empty());
    let listed = w.notes("unresolved");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].context_id, Some(ctx.context_id));
    assert!(listed[0].note.contains("coder"), "{}", listed[0].note);
}

#[test]
fn a_cycle_refuses_the_whole_migration_naming_the_contexts() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let fine = w.ctx("fine", "coder", Some(who), None);
    let a = w.ctx("loop-a", "coder", Some(who), None);
    let b = w.ctx("loop-b", "coder", Some(who), None);
    w.rotated_from(&a, &b.context_id.to_hex());
    w.rotated_from(&b, &a.context_id.to_hex());

    let error = w.migrate().unwrap_err().to_string();

    assert!(error.contains("loop-a") && error.contains("loop-b"), "{error}");
    assert_eq!(w.count("character_instances"), 0, "nothing was written");
    assert_eq!(w.count("instance_slots"), 0);
    assert_eq!(w.instance(&fine), None);
    assert_eq!(
        w.count("kernel_migrations WHERE name = 'instances-v1'"),
        0,
        "no marker: the next open tries again"
    );
}

#[test]
fn an_instance_whose_senpai_would_be_itself_refuses() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let first = w.ctx("first", "coder", Some(who), None);
    let fork = w.ctx("fork", "coder", Some(who), Some(&first));
    w.set_senpai(&first, &fork);

    let error = w.migrate().unwrap_err().to_string();

    assert!(error.contains("first") && error.contains("fork"), "{error}");
    assert_eq!(w.count("character_instances"), 0);
}

#[test]
fn a_senpai_cycle_between_instances_refuses() {
    let w = legacy_world();
    let house = w.house();
    let [one, two] = [w.character("one"), w.character("two")];
    w.default_cast(one, house);
    w.default_cast(two, house);
    let a = w.ctx("ring-a", "coder", Some(one), None);
    let b = w.ctx("ring-b", "coder", Some(two), None);
    w.set_senpai(&a, &b);
    w.set_senpai(&b, &a);

    let error = w.migrate().unwrap_err().to_string();

    assert!(error.contains("ring-a") && error.contains("ring-b"), "{error}");
    assert_eq!(w.count("character_instances"), 0);
}

#[test]
fn a_handoff_context_gets_no_instance_and_a_root_console_has_no_senpai() {
    let w = legacy_world();
    let house = w.house();
    let [coder, amy] = [w.character("coder"), w.character("amy")];
    w.default_cast(coder, house);
    let handoff = w.ctx("coder-handoff", "handoff", Some(coder), None);
    w.exec(
        "UPDATE characters SET handoff_ctx = ?1 WHERE principal_id = ?2",
        params![blob_param(handoff.context_id.as_bytes()), blob_param(coder.as_bytes())],
    );
    let console = w.ctx("amy", "root", Some(amy), None);
    w.exec(
        "UPDATE characters SET root = 1, root_ctx = ?1 WHERE principal_id = ?2",
        params![blob_param(console.context_id.as_bytes()), blob_param(amy.as_bytes())],
    );
    let seat = w.ctx("seat", "coder", Some(coder), None);
    w.set_senpai(&seat, &console);

    w.migrate().unwrap();

    assert_eq!(w.instance(&handoff), None, "the handoff log belongs to the character");
    let root = w.instance(&console).expect("the root console is an instance");
    let row = w.db.get_instance(root).unwrap().unwrap();
    assert_eq!((row.principal_id, row.senpai_instance), (amy, None));
    assert_eq!(w.db.get_instance(w.instance(&seat).unwrap()).unwrap().unwrap().senpai_instance, Some(root));
}

#[test]
fn values_the_migration_does_not_carry_are_reported() {
    let w = legacy_world();
    let house = w.house();
    let [lead, coder, other] = [w.character("lead"), w.character("coder"), w.character("other")];
    for who in [lead, coder, other] {
        w.default_cast(who, house);
    }
    let top = w.ctx("top", "coder", Some(lead), None);
    let rotated = w.ctx("rotated", "coder", Some(lead), None);
    w.rotated_from(&rotated, "an-older-seat");
    let first = w.ctx("kohai-first", "coder", Some(coder), None);
    w.set_senpai(&first, &top);
    let later = w.ctx("kohai-later", "coder", Some(coder), Some(&first));
    w.set_senpai(&later, &rotated);
    let stranger = w.ctx("stranger", "coder", Some(other), None);
    w.rotated_from(&stranger, &top.context_id.to_hex());

    w.migrate().unwrap();

    let dropped = w.notes("dropped");
    let about = |label: &str, needle: &str| {
        let id = [&top, &rotated, &first, &later, &stranger].into_iter().find(|c| c.label.as_deref() == Some(label)).unwrap().context_id;
        dropped.iter().any(|n| n.context_id == Some(id) && n.note.contains(needle))
    };
    assert!(about("rotated", "an-older-seat"), "an unparseable ROTATED_FROM is logged and ignored: {dropped:?}");
    assert_ne!(w.instance(&rotated), w.instance(&top), "the ignored link joined nothing");
    assert!(about("kohai-later", "senpai"), "a later context's different senpai_ctx is not carried: {dropped:?}");
    assert!(about("stranger", "performer"), "a rotation link to another performer is not followed: {dropped:?}");
    assert_eq!(w.instance(&stranger).map(|i| w.db.get_instance(i).unwrap().unwrap().principal_id), Some(other));
}

#[test]
fn live_instances_without_a_gate_or_distill_slot_are_listed() {
    let w = legacy_world();
    let house = w.house();
    let thin = w.cast("thin");
    w.slot(thin, "coder", w.deepseek, "ds-flash", None);
    let [full, bare, gone] = [w.character("full"), w.character("bare"), w.character("gone")];
    w.default_cast(full, house);
    w.default_cast(bare, thin);
    w.default_cast(gone, thin);
    let distill = w.slot(house, "distill", w.deepseek, "ds-flash", None);
    assert_eq!(distill.role, "distill");
    let a = w.ctx("full", "coder", Some(full), None);
    let b = w.ctx("bare", "coder", Some(bare), None);
    let c = w.ctx("gone", "coder", Some(gone), None);
    w.archive(&c);

    w.migrate().unwrap();

    let no_gate: Vec<_> = w.notes("no_gate_slot").into_iter().map(|n| n.instance_id).collect();
    let no_distill: Vec<_> = w.notes("no_distill_slot").into_iter().map(|n| n.instance_id).collect();
    assert_eq!(no_gate, vec![w.instance(&b)], "only the live instance whose cast has no gate slot");
    assert_eq!(no_distill, vec![w.instance(&b)], "the full cast has a distill slot; the archived instance is inactive");
    assert!(w.db.binding_slot(w.instance(&a).unwrap(), 1, "distill").unwrap().is_some(), "a distill slot is copied only from the cast's own");
    assert!(w.db.binding_slot(w.instance(&b).unwrap(), 1, "gate").unwrap().is_none(), "none is invented");
}

#[test]
fn an_archived_context_that_cannot_be_mapped_is_left_without_an_instance_and_listed() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let live = w.ctx("live", "coder", Some(who), None);
    let a = w.ctx("old-a", "coder", Some(who), None);
    let b = w.ctx("old-b", "coder", Some(who), None);
    w.rotated_from(&a, &b.context_id.to_hex());
    w.rotated_from(&b, &a.context_id.to_hex());
    w.archive(&a);
    w.archive(&b);

    w.migrate().unwrap();

    assert!(w.instance(&live).is_some());
    assert_eq!((w.instance(&a), w.instance(&b)), (None, None));
    let ids: Vec<_> = w.notes("unmapped").into_iter().filter_map(|n| n.context_id).collect();
    assert!(ids.contains(&a.context_id) && ids.contains(&b.context_id), "{ids:?}");
}

#[test]
fn an_overriding_fork_newer_than_the_conversation_does_not_displace_it() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let seat = w.ctx("seat", "coder", Some(who), None);
    let overridden = w.ctx("overridden", "coder", Some(who), Some(&seat));
    w.set_override(&overridden, "anthropic", "claude-x");

    w.migrate().unwrap();

    let instance = w.instance(&seat).unwrap();
    assert_eq!(w.db.instance_conversation(instance).unwrap(), Some(seat.context_id), "the newest binding is the conversation's");
    assert_eq!(w.seq(&seat), Some(2), "the conversation's resolution takes the highest seq");
    assert_eq!(w.seq(&overridden), Some(1));
    let fork_slot = w.db.binding_slot(instance, 1, "coder").unwrap().unwrap();
    assert_eq!(fork_slot.model, "claude-x", "the fork keeps its override");
    assert_eq!(w.db.binding_slot(instance, 2, "coder").unwrap().unwrap().model, "ds-flash");
    let bindings = w.db.list_instance_bindings(instance).unwrap();
    assert_eq!(bindings[0].conversation_ctx, overridden.context_id, "another binding names its own best context");
    assert!(w.notes("conversation").is_empty());
}

#[test]
fn a_gate_slot_needs_no_max_tokens_but_a_coder_slot_does() {
    let w = legacy_world();
    let council = w.db.upsert_backend(&BackendRow {
        backend_id: BackendId::new(),
        name: "council".into(),
        kind: "mk".into(),
        base_url: Some("http://zorak:8090".into()),
        api_key_env: None,
        api_key_file: None,
        key_optional: false,
        request_timeout_secs: None,
        idle_timeout_secs: None,
        max_concurrent: None,
        created_at: now_millis(),
        created_by: PrincipalId::system(),
    })
    .unwrap()
    .backend_id;
    let house = w.house();
    w.slot(house, "gate", council, "gemma-4", None);
    let who = w.character("coder");
    let conv = w.ctx("conv", "coder", Some(who), None);

    let instance = w.db.start_instance(who, None, Some(house), None, conv.context_id, who).unwrap();

    let gate = w.db.binding_slot(instance.instance_id, 1, "gate").unwrap().expect("the gate slot is copied");
    assert_eq!((gate.model.as_str(), gate.max_tokens), ("gemma-4", None));

    w.slot(house, "coder", council, "gemma-4", None);
    let next = w.ctx("next", "coder", Some(who), None);
    let error = w.db.append_binding(instance.instance_id, Some(house), None, next.context_id).unwrap_err().to_string();
    assert!(error.contains("coder") && error.contains("max_tokens"), "{error}");
}

#[test]
fn the_migration_runs_once() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let seat = w.ctx("seat", "coder", Some(who), None);
    w.migrate().unwrap();
    let later = w.ctx("later", "coder", Some(who), None);

    KernelDb::migrate_instances(&w.db.conn).unwrap();

    assert_eq!(w.count("character_instances"), 1);
    assert_eq!(w.instance(&later), None, "a marked database is left alone");
    assert!(w.instance(&seat).is_some());
}

// ── Dropping what the instance model replaced ───────────────────────

/// Every column and table the instance model replaced, as `(table, column)`; a
/// column of `None` is the whole table.
const DROPPED: [(&str, Option<&str>); 11] = [
    ("contexts", Some("senpai_ctx")),
    ("contexts", Some("played_by")),
    ("contexts", Some("cast_id")),
    ("contexts", Some("provider")),
    ("contexts", Some("model")),
    ("characters", Some("root_ctx")),
    ("presets", Some("cast_id")),
    ("presets", Some("system_prompt")),
    ("turns", Some("cast_label")),
    ("turns", Some("model_source")),
    ("llm_defaults", None),
];

fn exists(w: &World, (table, column): (&str, Option<&str>)) -> bool {
    match column {
        Some(column) => KernelDb::has_column(&w.db.conn, table, column).unwrap(),
        None => KernelDb::has_table(&w.db.conn, table).unwrap(),
    }
}

/// An open on a database at the previous schema runs `instances-v1` first,
/// which reads the old columns, and drops them after: the instances and
/// bindings carry what the columns held, the columns and `llm_defaults` are
/// gone, `characters.default_cast_id` is `characters.cast_id`, and rows in the
/// tables that lost columns survive. A second open does nothing.
#[test]
fn an_open_carries_the_old_columns_into_instances_then_drops_them() {
    let w = legacy_world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let lead = w.character("lead");
    w.default_cast(lead, house);
    let seat = w.ctx("seat", "coder", Some(lead), None);
    let overridden = w.ctx("overridden", "coder", Some(who), Some(&seat));
    w.set_override(&overridden, "anthropic", "claude-x");
    w.exec(
        "UPDATE characters SET root_ctx = ?1 WHERE principal_id = ?2",
        params![blob_param(seat.context_id.as_bytes()), blob_param(who.as_bytes())],
    );
    w.exec(
        "INSERT INTO presets (preset_id, label, cast_id, system_prompt, created_at, created_by)
         VALUES (?1, 'with-cast', ?2, 'be terse', 0, ?3)",
        params![PresetId::new().as_bytes().as_slice(), blob_param(house.as_bytes()), blob_param(who.as_bytes())],
    );
    w.exec(
        "INSERT INTO turns (turn_id, context_id, performer, cast_label, role, provider, model, model_source, started_at)
         VALUES (?1, ?2, ?3, 'house', 'coder', 'deepseek', 'ds-flash', 'cast', 1)",
        params![TurnId::new().as_bytes().as_slice(), blob_param(seat.context_id.as_bytes()), blob_param(who.as_bytes())],
    );
    w.exec(
        "INSERT INTO llm_defaults (id, default_backend, default_model) VALUES (1, 'deepseek', 'ds-flash')",
        [],
    );
    w.exec(
        "INSERT INTO senpai_migration_report (context_id, senpai_ctx, note, old_reviewer, recorded_at)
         VALUES (?1, NULL, 'kept for kj senpai show', NULL, 1)",
        params![blob_param(overridden.context_id.as_bytes())],
    );
    for dropped in DROPPED {
        assert!(exists(&w, dropped), "{dropped:?} is there before the open");
    }
    assert!(KernelDb::has_column(&w.db.conn, "characters", "default_cast_id").unwrap());

    KernelDb::apply_additive_migrations(&w.db.conn).unwrap();

    for dropped in DROPPED {
        assert!(!exists(&w, dropped), "{dropped:?} is gone after the open");
    }
    assert!(!KernelDb::has_column(&w.db.conn, "characters", "default_cast_id").unwrap());
    assert_eq!(w.db.character_cast(who).unwrap().map(|c| c.label), Some("house".to_string()), "characters.cast_id holds the old default cast");
    assert_eq!(w.db.get_context(seat.context_id).unwrap().unwrap().played_by, Some(lead), "the performer was carried into an instance before the column went");
    let (instance, seq) = w.db.context_instance(overridden.context_id).unwrap().expect("the override joined an instance");
    let slot = w.db.binding_slot(instance, seq, "coder").unwrap().expect("the override's slot");
    assert_eq!((slot.backend_id, slot.model.as_str()), (w.anthropic, "claude-x"), "the override was carried into the binding");
    assert_eq!(w.db.senpai_conversation_of(overridden.context_id).unwrap(), Some(seat.context_id), "the senpai edge was carried");
    assert_eq!(w.count("turns"), 1, "a turn survives the loss of its columns");
    assert_eq!(w.count("presets"), 1, "a preset survives the loss of its columns");
    assert_eq!(w.count("senpai_migration_report"), 1, "kj senpai show still reads the report");

    KernelDb::apply_additive_migrations(&w.db.conn).unwrap();
    assert_eq!(w.db.context_instance(overridden.context_id).unwrap(), Some((instance, seq)), "a second open changes nothing");
}

/// The drop refuses before `instances-v1` has recorded its mapping: the
/// columns it reads are still there for the next try.
#[test]
fn the_drop_refuses_before_the_instance_migration_has_run() {
    let w = legacy_world();
    let error = KernelDb::drop_pre_instance_columns(&w.db.conn).unwrap_err().to_string();
    assert!(error.contains("before the instance migration"), "{error}");
    for dropped in DROPPED {
        assert!(exists(&w, dropped), "{dropped:?} stays");
    }
}

/// A database on the current schema, which never had the columns, opens
/// without either migration and without the columns.
#[test]
fn a_new_database_has_none_of_the_dropped_columns_and_runs_neither_migration() {
    let w = world();
    for dropped in DROPPED {
        assert!(!exists(&w, dropped), "{dropped:?} is not in a new database");
    }
    assert!(KernelDb::has_column(&w.db.conn, "characters", "cast_id").unwrap());
    assert_eq!(w.count("kernel_migrations WHERE name IN ('senpai-edges-v1', 'instances-v1')"), 0);
    KernelDb::apply_additive_migrations(&w.db.conn).unwrap();
}

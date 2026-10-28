use rusqlite::params;

use kaijutsu_types::{DocKind, PresetId, TurnId, WorkspaceId};

use super::*;
use crate::kernel_db::{
    BackendModelRow, BackendRow, CastRow, CastSlotRow, CharacterRow, ContextRow, DocumentRow, ModelTunablesRow,
    blob_param, insert_context_with_doc, into_pre_context_character, make_context_row, now_millis,
    set_stored_played_by, setup_test_db,
};
use crate::kj::context::{CharacterPlacement, insert_new_context_rows};

struct World {
    db: KernelDb,
    ws: WorkspaceId,
    anthropic: BackendId,
    deepseek: BackendId,
    /// Next `created_at` a test context takes, so creation order is explicit.
    clock: std::cell::Cell<i64>,
    /// Whether the database has the columns a migration reads.
    legacy: bool,
}

/// A database on the current schema.
fn world() -> World {
    build_world(false)
}

/// A database with the schema `context-characters-v1` migrates from.
fn legacy_world() -> World {
    build_world(true)
}

fn build_world(legacy: bool) -> World {
    let db = KernelDb::temporary().unwrap();
    if legacy {
        into_pre_context_character(&db);
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

    /// A context row; a legacy database also stores `played_by`.
    fn ctx(&self, label: &str, ty: &str, played_by: Option<PrincipalId>, forked_from: Option<&ContextRow>) -> ContextRow {
        assert!(self.legacy || played_by.is_none(), "only a legacy database stores a performer; write a context character instead");
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

    /// Make `ctx` the console of the root character `who`.
    fn console(&self, who: PrincipalId, ctx: &ContextRow) {
        self.exec(
            "UPDATE characters SET root = 1, root_ctx = ?1 WHERE principal_id = ?2",
            params![blob_param(ctx.context_id.as_bytes()), blob_param(who.as_bytes())],
        );
    }

    /// Run `context-characters-v1` the way an upgrading kernel does: the marker is absent.
    fn migrate(&self) -> KernelDbResult<()> {
        self.exec("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::CONTEXT_CHARACTERS_MIGRATION]);
        KernelDb::migrate_context_characters(&self.db.conn)
    }

    fn cc(&self, ctx: &ContextRow) -> Option<ContextCharacterRow> {
        self.db.context_character(ctx.context_id).unwrap()
    }

    fn slots(&self, ctx: &ContextRow) -> Vec<ContextSlotRow> {
        self.db.list_context_slots(ctx.context_id).unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        self.db.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    fn notes(&self, kind: &str) -> Vec<ContextCharacterMigrationNote> {
        self.db.context_character_migration_report().unwrap().into_iter().filter(|n| n.kind == kind).collect()
    }
}

/// A slot's content without its context, for comparing two contexts' copies.
fn content(slots: Vec<ContextSlotRow>) -> Vec<ContextSlotRow> {
    let nil = ContextId::try_from_slice(&[0; 16]).expect("sixteen bytes");
    slots.into_iter().map(|s| ContextSlotRow { context_id: nil, ..s }).collect()
}

// ── API ─────────────────────────────────────────────────────────────

#[test]
fn write_copies_the_cast_and_fills_unset_parameters_from_the_model_row() {
    let w = world();
    let house = w.house();
    let mut coder = w.slot(house, "coder", w.deepseek, "ds-flash", None);
    coder.temperature = Some(0.2);
    w.db.set_cast_slot(&coder).unwrap();
    let who = w.character("coder");
    w.default_cast(who, house);
    let conv = w.ctx("conv", "coder", None, None);

    let row = w.db.write_context_character(conv.context_id, who, None, Some("be terse")).unwrap();

    let slot = w.db.context_slot(conv.context_id, "coder").unwrap().expect("coder slot");
    assert_eq!(slot.max_tokens, Some(4096), "the model row supplies the slot's unset max_tokens");
    assert_eq!(slot.temperature, Some(0.2), "a parameter the slot sets wins");
    assert_eq!(slot.top_p, Some(0.9), "an unset parameter takes the model row's value");
    assert_eq!(slot.context_window, Some(64_000));
    assert_eq!(slot.effort, None, "an unset parameter stays unset: no value of our own");
    let judge = w.db.context_slot(conv.context_id, "judge").unwrap().unwrap();
    assert_eq!(judge.max_tokens, Some(2048), "the slot's own max_tokens wins over the model row");
    assert_eq!(row.persona.as_deref(), Some("be terse"));
    assert_eq!((row.principal_id, row.senpai_ctx), (who, None));
    assert_eq!(w.cc(&conv), Some(row));
}

/// A cast edit after create changes no context's slots; a context written
/// after the edit copies the new cast.
#[test]
fn a_cast_edit_after_create_changes_no_slots_and_a_later_context_copies_it() {
    let w = world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let first = w.ctx("first", "coder", None, None);
    w.db.write_context_character(first.context_id, who, None, None).unwrap();
    let before = content(w.slots(&first));

    w.model_row(w.anthropic, "claude-x", ModelTunablesRow { max_tokens: Some(9000), ..Default::default() }, None);
    w.slot(house, "coder", w.anthropic, "claude-x", None);
    w.model_row(w.deepseek, "ds-flash", ModelTunablesRow { max_tokens: Some(1), ..Default::default() }, None);
    let second = w.ctx("second", "coder", None, None);
    w.db.write_context_character(second.context_id, who, None, None).unwrap();

    assert_eq!(content(w.slots(&first)), before, "the first context's slots are a copy");
    let old = w.db.context_slot(first.context_id, "coder").unwrap().unwrap();
    assert_eq!((old.model.as_str(), old.backend_id, old.max_tokens), ("ds-flash", w.deepseek, Some(4096)));
    let new = w.db.context_slot(second.context_id, "coder").unwrap().unwrap();
    assert_eq!((new.model.as_str(), new.backend_id, new.max_tokens), ("claude-x", w.anthropic, Some(9000)));
}

/// A slot row with the given thinking knobs and a `max_tokens` of 4096.
fn thinking_slot(w: &World, cast: CastId, role: &str, style: Option<&str>, budget: Option<i64>) {
    let mut row = w.slot(cast, role, w.deepseek, "think-model", Some(4096));
    row.thinking_style = style.map(str::to_string);
    row.thinking_budget = budget;
    w.db.set_cast_slot(&row).unwrap();
}

fn write_error(w: &World, cast: CastId) -> String {
    let name = format!("thinker-{}", cast.short());
    let who = w.character(&name);
    w.default_cast(who, cast);
    let conv = w.ctx(&name, "coder", None, None);
    w.db.write_context_character(conv.context_id, who, None, None).unwrap_err().to_string()
}

#[test]
fn an_unknown_thinking_style_is_refused_on_cast_slot_and_model_row_writes() {
    let w = world();
    let cast = w.cast("typo");
    let mut row = w.slot(cast, "coder", w.deepseek, "ds-flash", Some(4096));
    row.thinking_style = Some("budgett".into());
    let error = w.db.set_cast_slot(&row).unwrap_err().to_string();
    assert!(error.contains("thinking_style") && error.contains("budgett"), "{error}");

    let error = w
        .db
        .set_backend_model(&BackendModelRow {
            backend_id: w.deepseek,
            model_id: "typo-model".into(),
            context_window: None,
            extra: None,
            tunables: ModelTunablesRow { thinking_style: Some("on".into()), ..Default::default() },
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("thinking_style") && error.contains("on"), "{error}");

    for style in ["off", "auto", "adaptive", "budget"] {
        row.thinking_style = Some(style.into());
        w.db.set_cast_slot(&row).unwrap_or_else(|e| panic!("{style} must be accepted: {e}"));
    }
}

#[test]
fn a_budget_slot_with_no_thinking_budget_refuses_at_write_naming_cast_role_and_model() {
    let w = world();
    let cast = w.cast("nobudget");
    thinking_slot(&w, cast, "coder", Some("budget"), None);

    let error = write_error(&w, cast);

    for needle in ["nobudget", "coder", "think-model", "thinking_budget"] {
        assert!(error.contains(needle), "{needle} missing from: {error}");
    }
    assert_eq!(w.count("context_characters"), 0, "a refused write leaves nothing");
    assert_eq!(w.count("context_slots"), 0);
}

#[test]
fn a_budget_slot_takes_its_thinking_budget_from_the_model_row() {
    let w = world();
    w.model_row(
        w.deepseek,
        "think-model",
        ModelTunablesRow { thinking_budget: Some(1024), ..Default::default() },
        None,
    );
    let cast = w.cast("rowbudget");
    thinking_slot(&w, cast, "coder", Some("budget"), None);
    let who = w.character("thinker");
    w.default_cast(who, cast);
    let conv = w.ctx("conv", "coder", None, None);

    w.db.write_context_character(conv.context_id, who, None, None).unwrap();
}

#[test]
fn a_thinking_budget_not_below_max_tokens_refuses_at_write() {
    let w = world();
    for (label, budget) in [("equal", 4096), ("over", 5000)] {
        let cast = w.cast(label);
        thinking_slot(&w, cast, "coder", Some("budget"), Some(budget));
        let error = write_error(&w, cast);
        for needle in [label, "coder", "think-model", "thinking_budget", "max_tokens"] {
            assert!(error.contains(needle), "{needle} missing from: {error}");
        }
    }
    let cast = w.cast("below");
    thinking_slot(&w, cast, "coder", Some("budget"), Some(4095));
    let who = w.character("fine");
    w.default_cast(who, cast);
    let conv = w.ctx("conv2", "coder", None, None);
    w.db.write_context_character(conv.context_id, who, None, None).unwrap();
}

#[test]
fn a_council_server_slot_is_exempt_from_the_thinking_checks() {
    let w = world();
    let cast = w.cast("council");
    thinking_slot(&w, cast, "gate", Some("budget"), None);
    thinking_slot(&w, cast, "judge", Some("budget"), Some(9000));
    let who = w.character("thinker");
    w.default_cast(who, cast);
    let conv = w.ctx("conv", "coder", None, None);

    w.db.write_context_character(conv.context_id, who, None, None).unwrap();
}

#[test]
fn a_slot_with_no_max_tokens_anywhere_refuses_naming_cast_role_and_model() {
    let w = world();
    let bare = w.cast("bare");
    w.slot(bare, "coder", w.deepseek, "no-row-model", None);
    let who = w.character("coder");
    w.default_cast(who, bare);
    let conv = w.ctx("conv", "coder", None, None);

    let error = w.db.write_context_character(conv.context_id, who, None, None).unwrap_err().to_string();

    for needle in ["bare", "coder", "no-row-model", "max_tokens"] {
        assert!(error.contains(needle), "{needle} missing from: {error}");
    }
    assert_eq!(w.count("context_characters"), 0, "a refused write leaves nothing");
    assert_eq!(w.count("context_slots"), 0);
    assert_eq!(w.cc(&conv), None);

    let house = w.house();
    w.default_cast(who, house);
    w.db.write_context_character(conv.context_id, who, None, None).unwrap();
    let next = w.ctx("next", "coder", None, None);
    w.slot(house, "coder", w.deepseek, "no-row-model", None);
    let error = w.db.write_context_character(next.context_id, who, None, None).unwrap_err().to_string();
    assert!(error.contains("house") && error.contains("coder") && error.contains("no-row-model"), "{error}");
    assert_eq!(w.count("context_characters"), 1, "a refused write leaves nothing");
    assert_eq!(w.cc(&next), None);
}

#[test]
fn a_context_may_have_no_slot_for_a_role() {
    let w = world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let with_cast = w.ctx("with-cast", "coder", None, None);
    w.db.write_context_character(with_cast.context_id, who, None, None).unwrap();
    assert!(w.db.context_slot(with_cast.context_id, "musician").unwrap().is_none());

    let castless = w.character("castless");
    let bare = w.ctx("castless", "coder", None, None);
    w.db.write_context_character(bare.context_id, castless, None, None).unwrap();
    assert!(w.db.context_slot(bare.context_id, "coder").unwrap().is_none());
    assert!(w.slots(&bare).is_empty());
}

/// The triggers: a context's slots are never changed or deleted, its
/// principal and persona never change, and `senpai_ctx` stays movable.
#[test]
fn slots_principal_and_persona_are_never_changed_but_the_senpai_moves() {
    let w = world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let seat = w.ctx("seat", "coder", None, None);
    let other = w.ctx("other", "coder", None, None);
    let conv = w.ctx("conv", "coder", None, None);
    w.db.write_context_character(seat.context_id, who, None, None).unwrap();
    w.db.write_context_character(other.context_id, who, None, None).unwrap();
    w.db.write_context_character(conv.context_id, who, Some(seat.context_id), Some("persona")).unwrap();

    for (sql, what) in [
        ("UPDATE context_slots SET max_tokens = 1", "slot update"),
        ("DELETE FROM context_slots", "slot delete"),
        ("UPDATE context_characters SET principal_id = x'00000000000000000000000000000001'", "principal update"),
        ("UPDATE context_characters SET persona = 'x'", "persona update"),
        ("UPDATE context_characters SET persona = NULL", "persona clear"),
    ] {
        let error = w.db.conn.execute(sql, []).unwrap_err().to_string();
        assert!(error.contains("never changed"), "{what}: {error}");
    }
    assert_eq!(w.cc(&conv).unwrap().principal_id, who);
    assert_eq!(w.cc(&conv).unwrap().persona.as_deref(), Some("persona"));
    assert_eq!(w.slots(&conv).len(), 3);

    w.db.set_context_senpai(conv.context_id, Some(other.context_id)).unwrap();
    assert_eq!(w.cc(&conv).unwrap().senpai_ctx, Some(other.context_id), "senpai_ctx is the one column that moves");
    w.exec("UPDATE contexts SET label = 'renamed' WHERE context_id = ?1", params![blob_param(conv.context_id.as_bytes())]);
}

/// Every reader of a context's performer gets the principal of its context
/// character; a root's console is the pointer on its sheet.
#[test]
fn the_performer_and_the_root_console_are_read_from_the_context_character() {
    let w = world();
    let who = w.character("coder");
    let conv = w.ctx("conv", "coder", None, None);
    assert_eq!(w.db.get_context(conv.context_id).unwrap().unwrap().played_by, None, "a context with none performs nothing");
    w.db.write_context_character(conv.context_id, who, None, None).unwrap();
    for read in [
        w.db.get_context(conv.context_id).unwrap().unwrap(),
        w.db.find_context_by_label("conv").unwrap().unwrap(),
        w.db.list_active_contexts().unwrap().into_iter().find(|r| r.context_id == conv.context_id).unwrap(),
        w.db.list_all_contexts().unwrap().into_iter().find(|r| r.context_id == conv.context_id).unwrap(),
        w.db.fork_lineage(conv.context_id).unwrap().remove(0).0,
    ] {
        assert_eq!(read.played_by, Some(who), "the context character's principal");
    }
    assert_eq!(w.db.contexts_played_by(who).unwrap().len(), 1);

    let amy = w.character("amy");
    w.db.update_character_root(amy, true).unwrap();
    let console = w.ctx("amy-console", "root", None, None);
    assert_eq!(w.db.root_console(amy).unwrap(), None);
    w.db.write_context_character(console.context_id, amy, None, None).unwrap();
    w.db.set_character_root_ctx(amy, Some(console.context_id)).unwrap();
    for sheet in [
        w.db.get_character(amy).unwrap().unwrap(),
        w.db.get_character_by_name("amy").unwrap().unwrap(),
        w.db.list_characters(false).unwrap().into_iter().find(|c| c.principal_id == amy).unwrap(),
    ] {
        assert_eq!(sheet.root_ctx, Some(console.context_id));
    }
    assert_eq!(w.db.root_console(amy).unwrap(), Some(console.context_id));
    assert_eq!(w.db.root_console(who).unwrap(), None, "only a root character has a console");
}

/// A row that sets `played_by` is refused: the performer is the context
/// character's principal, and no column stores it.
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
    assert!(error.contains("played_by") && error.contains("context character"), "{error}");
    assert!(w.db.get_context(row.context_id).unwrap().is_none());
    row.played_by = None;
    w.db.insert_context(&row).expect("the same row without a performer is accepted");
}

#[test]
fn a_context_gets_one_context_character_once() {
    let w = world();
    let house = w.house();
    let who = w.character("coder");
    w.default_cast(who, house);
    let conv = w.ctx("conv", "coder", None, None);
    w.db.write_context_character(conv.context_id, who, None, None).unwrap();

    let again = w.db.write_context_character(conv.context_id, who, None, None);
    assert!(again.is_err(), "a context already has its context character");
    assert!(w.db.copy_context_character(conv.context_id, conv.context_id).is_err(), "a copy onto itself is a second write");
    assert_eq!(w.count("context_characters"), 1);
    assert_eq!(w.count("context_slots"), 3, "a refused write copies no slots");
    let missing = w.db.write_context_character(ContextId::new(), who, None, None);
    assert!(matches!(missing, Err(KernelDbError::NotFound(_))), "{missing:?}");
}

/// A fork copies its source's context character: the same principal and
/// persona, every slot verbatim even after the cast changed, and the source
/// as senpai. Written through the transaction a fork uses.
#[test]
fn a_fork_copies_the_slots_verbatim_and_takes_the_forker_as_senpai() {
    let mut w = world();
    let house = w.house();
    let lead = w.character("lead");
    let who = w.character("coder");
    w.default_cast(who, house);
    let top = w.ctx("top", "coder", None, None);
    let source = w.ctx("source", "coder", None, None);
    w.db.write_context_character(top.context_id, lead, None, None).unwrap();
    w.db.write_context_character(source.context_id, who, Some(top.context_id), Some("persona")).unwrap();
    // The cast changes after the source was made.
    w.model_row(w.anthropic, "claude-x", ModelTunablesRow { max_tokens: Some(9000), ..Default::default() }, None);
    w.slot(house, "coder", w.anthropic, "claude-x", None);

    let mut child = make_context_row(Some("child"));
    child.forked_from = Some(source.context_id);
    w.db.insert_forked_context(&child, w.ws, source.context_id, None).unwrap();

    let copy = w.db.context_character(child.context_id).unwrap().expect("the fork has a context character");
    assert_eq!((copy.principal_id, copy.persona.as_deref()), (who, Some("persona")));
    assert_eq!(copy.senpai_ctx, Some(source.context_id), "the forker, not the forker's senpai");
    assert_eq!(content(w.db.list_context_slots(child.context_id).unwrap()), content(w.slots(&source)), "slots are copied verbatim");
    assert_eq!(w.db.context_slot(child.context_id, "coder").unwrap().unwrap().model, "ds-flash", "not the edited cast");
    assert_eq!(w.db.get_context(child.context_id).unwrap().unwrap().played_by, Some(who));

    let bare = w.ctx("bare", "coder", None, None);
    let mut bare_child = make_context_row(Some("bare-child"));
    bare_child.forked_from = Some(bare.context_id);
    w.db.insert_forked_context(&bare_child, w.ws, bare.context_id, None).unwrap();
    assert_eq!(w.db.context_character(bare_child.context_id).unwrap(), None, "no performer is invented");
}

/// A rotation's successor is the same character with slots copied fresh from
/// the cast and the predecessor's senpai, through the placement `kj context
/// rotate` writes.
#[test]
fn a_rotation_copies_fresh_slots_and_keeps_the_predecessors_senpai() {
    let w = world();
    let house = w.house();
    let lead = w.character("lead");
    let who = w.character("coder");
    w.default_cast(who, house);
    let seat = w.ctx("seat", "coder", None, None);
    let lane = w.ctx("lane", "coder", None, None);
    w.db.write_context_character(seat.context_id, lead, None, None).unwrap();
    w.db.write_context_character(lane.context_id, who, Some(seat.context_id), None).unwrap();
    w.model_row(w.anthropic, "claude-x", ModelTunablesRow { max_tokens: Some(9000), ..Default::default() }, None);
    w.slot(house, "coder", w.anthropic, "claude-x", None);

    let mut successor = make_context_row(Some("successor"));
    successor.context_type = "coder".into();
    insert_new_context_rows(&w.db, &successor, CharacterPlacement::Rotate { predecessor: lane.context_id }).unwrap();

    let copy = w.db.context_character(successor.context_id).unwrap().unwrap();
    assert_eq!((copy.principal_id, copy.senpai_ctx), (who, Some(seat.context_id)), "the same character, the predecessor's senpai");
    let slot = w.db.context_slot(successor.context_id, "coder").unwrap().unwrap();
    assert_eq!((slot.model.as_str(), slot.backend_id), ("claude-x", w.anthropic), "slots are read fresh from the cast");
    let old = w.db.context_slot(lane.context_id, "coder").unwrap().unwrap();
    assert_eq!(old.model, "ds-flash", "the predecessor keeps its own");
}

/// Rotating a root character's console moves `characters.root_ctx` in the
/// commit, as it moves `handoff_ctx`, and not before; another character's
/// pointer stays.
#[test]
fn root_ctx_moves_at_commit_rotation() {
    let w = world();
    let [amy, bob] = [w.character("amy"), w.character("bob")];
    for root in [amy, bob] {
        w.db.update_character_root(root, true).unwrap();
    }
    let old = w.ctx("amy-console", "root", None, None);
    let bob_console = w.ctx("bob-console", "root", None, None);
    for (who, console) in [(amy, &old), (bob, &bob_console)] {
        w.db.write_context_character(console.context_id, who, None, None).unwrap();
        w.db.set_character_root_ctx(who, Some(console.context_id)).unwrap();
    }
    let mut successor = make_context_row(Some("amy-next"));
    successor.context_type = "root".into();
    insert_new_context_rows(&w.db, &successor, CharacterPlacement::Rotate { predecessor: old.context_id }).unwrap();
    assert_eq!(w.db.root_console(amy).unwrap(), Some(old.context_id), "the pointer waits for the commit");

    w.db.commit_rotation(old.context_id, successor.context_id).unwrap();

    assert_eq!(w.db.root_console(amy).unwrap(), Some(successor.context_id), "the commit moves the console");
    assert_eq!(w.db.root_console(bob).unwrap(), Some(bob_console.context_id), "another root's console stays");
    assert!(w.db.get_context(old.context_id).unwrap().unwrap().is_archived());
}

#[test]
fn the_kohai_and_the_live_contexts_of_a_character_are_read_from_the_edge() {
    let w = world();
    let [lead, coder] = [w.character("lead"), w.character("coder")];
    let seat = w.ctx("seat", "lead", None, None);
    let a = w.ctx("a", "coder", None, None);
    let b = w.ctx("b", "coder", None, None);
    let gone = w.ctx("gone", "coder", None, None);
    w.db.write_context_character(seat.context_id, lead, None, None).unwrap();
    for ctx in [&a, &b, &gone] {
        w.db.write_context_character(ctx.context_id, coder, Some(seat.context_id), None).unwrap();
    }
    w.db.archive_context(gone.context_id).unwrap();

    assert_eq!(w.db.kohai_of(seat.context_id).unwrap(), vec![a.context_id, b.context_id], "live kohai, oldest first");
    assert_eq!(w.db.character_live_contexts(coder).unwrap(), vec![a.context_id, b.context_id]);
    assert_eq!(w.db.character_live_contexts(lead).unwrap(), vec![seat.context_id]);
    assert!(w.db.kohai_of(a.context_id).unwrap().is_empty());
}

// ── Migration `context-characters-v1` ───────────────────────────────

/// Every performed context gets a context character of its performer with
/// the senpai it had; a root console has none and a handoff log gets none.
#[test]
fn performed_contexts_get_context_characters_with_their_senpai() {
    let w = legacy_world();
    let house = w.house();
    let [amy, lead, coder] = [w.character("amy"), w.character("lead"), w.character("coder")];
    for who in [lead, coder] {
        w.default_cast(who, house);
    }
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let handoff = w.ctx("lead-handoff", "handoff", Some(lead), None);
    w.exec(
        "UPDATE characters SET handoff_ctx = ?1 WHERE principal_id = ?2",
        params![blob_param(handoff.context_id.as_bytes()), blob_param(lead.as_bytes())],
    );
    let seat = w.ctx("seat", "coder", Some(lead), None);
    w.set_senpai(&seat, &console);
    let lane = w.ctx("lane", "coder", Some(coder), Some(&seat));
    w.set_senpai(&lane, &seat);
    let scratch = w.ctx("scratch", "default", None, None);

    w.migrate().unwrap();

    let row = |ctx: &ContextRow| w.cc(ctx).map(|r| (r.principal_id, r.senpai_ctx));
    assert_eq!(row(&console), Some((amy, None)), "a root console has no senpai");
    assert_eq!(row(&seat), Some((lead, Some(console.context_id))));
    assert_eq!(row(&lane), Some((coder, Some(seat.context_id))));
    assert_eq!(row(&handoff), None, "the handoff log belongs to the character");
    assert_eq!(row(&scratch), None, "a context nobody plays has none");
    assert_eq!(w.count("context_characters"), 3);
    assert_eq!(w.cc(&lane).unwrap().created_at, lane.created_at);
}

#[test]
fn a_judge_shadow_gets_the_character_of_the_context_it_judges() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who] = [w.character("amy"), w.character("coder")];
    w.default_cast(who, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let seat = w.ctx("seat", "coder", Some(who), None);
    w.set_senpai(&seat, &console);
    let live_shadow = w.ctx("shadow", "judge", None, Some(&seat));
    let old_shadow = w.ctx("old-shadow", "judge", None, Some(&seat));
    w.set_cast(&live_shadow, house);
    w.set_cast(&old_shadow, house);
    w.archive(&old_shadow);

    w.migrate().unwrap();

    for shadow in [&live_shadow, &old_shadow] {
        let row = w.cc(shadow).unwrap_or_else(|| panic!("{:?} has a context character", shadow.label));
        assert_eq!((row.principal_id, row.senpai_ctx), (who, Some(seat.context_id)), "archived shadows are found too");
    }
    assert_eq!(
        w.slots(&live_shadow).into_iter().map(|s| s.role).collect::<Vec<_>>(),
        vec!["coder", "gate", "judge"],
        "the shadow's slots are its own cast's, not the seat's"
    );
    assert_eq!(w.count("context_characters"), 4);
}

/// A shadow whose judged context has no character cannot be mapped: an
/// archived one is left without a context character and listed, a live one
/// refuses the migration naming it.
#[test]
fn an_unmappable_judge_shadow_is_listed_when_archived_and_refuses_when_live() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who] = [w.character("amy"), w.character("coder")];
    w.default_cast(who, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let unplayed = w.ctx("unplayed", "coder", None, None);
    let archived_shadow = w.ctx("archived-shadow", "judge", None, Some(&unplayed));
    w.archive(&archived_shadow);
    let live_shadow = w.ctx("live-shadow", "judge", None, Some(&unplayed));

    let error = w.migrate().unwrap_err().to_string();
    assert!(error.contains("live-shadow") && !error.contains("archived-shadow"), "{error}");
    assert_eq!(w.count("context_characters"), 0, "nothing was written");
    assert_eq!(w.count("kernel_migrations WHERE name = 'context-characters-v1'"), 0, "no marker: the next open tries again");

    w.archive(&live_shadow);
    w.migrate().unwrap();
    assert_eq!(w.cc(&archived_shadow), None);
    let listed: Vec<_> = w.notes("unmapped").into_iter().filter_map(|n| n.context_id).collect();
    assert!(listed.contains(&archived_shadow.context_id) && listed.contains(&live_shadow.context_id), "{listed:?}");
    assert_eq!(w.count("context_characters"), 1, "only the console");
}

#[test]
fn a_shared_cast_is_copied_into_each_context() {
    let w = legacy_world();
    let house = w.house();
    let [amy, one, two] = [w.character("amy"), w.character("one"), w.character("two")];
    w.default_cast(one, house);
    w.default_cast(two, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let a = w.ctx("a", "coder", Some(one), None);
    let b = w.ctx("b", "coder", Some(two), None);
    w.set_senpai(&a, &console);
    w.set_senpai(&b, &console);

    w.migrate().unwrap();

    for ctx in [&a, &b] {
        let roles: Vec<String> = w.slots(ctx).into_iter().map(|s| s.role).collect();
        assert_eq!(roles, vec!["coder", "gate", "judge"], "the cast's own slots, nothing more");
    }
    assert_eq!(w.count("context_slots"), 6);
    let slot = w.db.context_slot(a.context_id, "coder").unwrap().unwrap();
    assert_eq!((slot.max_tokens, slot.top_p, slot.context_window), (Some(4096), Some(0.9), Some(64_000)));
}

#[test]
fn a_context_with_an_override_keeps_its_model() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who] = [w.character("amy"), w.character("coder")];
    w.default_cast(who, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let seat = w.ctx("seat", "coder", Some(who), None);
    w.set_senpai(&seat, &console);
    let overridden = w.ctx("overridden", "coder", Some(who), Some(&seat));
    w.set_senpai(&overridden, &seat);
    w.set_override(&overridden, "anthropic", "claude-x");

    w.migrate().unwrap();

    let slot = w.db.context_slot(overridden.context_id, "coder").unwrap().unwrap();
    assert_eq!((slot.backend_id, slot.model.as_str()), (w.anthropic, "claude-x"));
    assert_eq!(slot.max_tokens, Some(64_000), "nothing set it: the old code sent 64000");
    assert!(
        w.db.context_slot(overridden.context_id, "judge").unwrap().is_some(),
        "the override replaces only the context's own role: the cast's other slots stay"
    );
    assert_eq!(w.db.context_slot(seat.context_id, "coder").unwrap().unwrap().model, "ds-flash", "a sibling keeps the cast's model");
}

#[test]
fn a_context_resolved_through_llm_defaults_keeps_that_model() {
    let w = legacy_world();
    w.exec(
        "INSERT INTO llm_defaults (id, default_backend, default_model, max_tokens, temperature)
         VALUES (1, 'deepseek', 'ds-lite', 1000, 0.5)",
        [],
    );
    let [amy, who] = [w.character("amy"), w.character("plain")];
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let ctx = w.ctx("plain", "coder", Some(who), None);
    w.set_senpai(&ctx, &console);

    w.migrate().unwrap();

    let slot = w.db.context_slot(ctx.context_id, "coder").unwrap().expect("resolved through the default");
    assert_eq!((slot.backend_id, slot.model.as_str()), (w.deepseek, "ds-lite"));
    assert_eq!((slot.max_tokens, slot.temperature), (Some(1000), Some(0.5)), "the llm_defaults floor is what the old code sent");
    assert!(w.notes("no_slot").is_empty());
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
    let [amy, who] = [w.character("amy"), w.character("pins")];
    w.default_cast(who, cast);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let ctx = w.ctx("pins", "coder", Some(who), None);
    w.set_senpai(&ctx, &console);

    w.migrate().unwrap();

    let max = |role: &str| w.db.context_slot(ctx.context_id, role).unwrap().unwrap().max_tokens;
    assert_eq!((max("coder"), max("judge")), (Some(1234), Some(7000)));
    assert_eq!(max("distill"), Some(64_000));
    assert_eq!(max("gate"), None, "a gate slot names a council server: no max_tokens is pinned");
}

#[test]
fn an_unresolvable_context_gets_no_slot_and_is_listed() {
    let w = legacy_world();
    let [amy, who] = [w.character("amy"), w.character("lost")];
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let ctx = w.ctx("lost", "coder", Some(who), None);
    w.set_senpai(&ctx, &console);

    w.migrate().unwrap();

    assert!(w.cc(&ctx).is_some(), "the context character exists");
    assert!(w.slots(&ctx).is_empty());
    let listed = w.notes("no_slot");
    assert_eq!(listed.len(), 2, "the console has no model either: {listed:?}");
    let lost = listed.iter().find(|n| n.context_id == Some(ctx.context_id)).expect("the context is listed");
    assert!(lost.note.contains("coder"), "{}", lost.note);
}

#[test]
fn a_self_edge_or_a_cycle_among_live_contexts_refuses_the_whole_migration_naming_them() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who] = [w.character("amy"), w.character("coder")];
    w.default_cast(who, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let fine = w.ctx("fine", "coder", Some(who), None);
    w.set_senpai(&fine, &console);
    let a = w.ctx("loop-a", "coder", Some(who), None);
    let b = w.ctx("loop-b", "coder", Some(who), None);
    w.set_senpai(&a, &b);
    w.set_senpai(&b, &a);
    let own = w.ctx("own-senpai", "coder", Some(who), None);
    w.set_senpai(&own, &own);

    let error = w.migrate().unwrap_err().to_string();

    assert!(error.contains("loop-a") && error.contains("loop-b") && error.contains("own-senpai"), "{error}");
    assert!(!error.contains("fine"), "{error}");
    assert_eq!(w.count("context_characters"), 0, "nothing was written");
    assert_eq!(w.count("context_slots"), 0);
    assert_eq!(w.count("kernel_migrations WHERE name = 'context-characters-v1'"), 0, "no marker: the next open tries again");
}

/// A cycle of archived contexts is broken at its newest member, which is listed.
#[test]
fn a_cycle_among_archived_contexts_is_broken_and_listed() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who] = [w.character("amy"), w.character("coder")];
    w.default_cast(who, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let a = w.ctx("old-a", "coder", Some(who), None);
    let b = w.ctx("old-b", "coder", Some(who), None);
    w.set_senpai(&a, &b);
    w.set_senpai(&b, &a);
    w.archive(&a);
    w.archive(&b);

    w.migrate().unwrap();

    assert_eq!(w.cc(&b).unwrap().senpai_ctx, None, "broken at the newest member");
    assert_eq!(w.cc(&a).unwrap().senpai_ctx, Some(b.context_id));
    let dropped: Vec<_> = w.notes("dropped").into_iter().filter_map(|n| n.context_id).collect();
    assert_eq!(dropped, vec![b.context_id]);
}

#[test]
fn a_live_context_with_no_senpai_that_is_not_a_root_console_refuses() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who] = [w.character("amy"), w.character("coder")];
    w.default_cast(who, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let stray = w.ctx("stray-top", "coder", Some(who), None);
    let archived = w.ctx("archived-top", "coder", Some(who), None);
    w.archive(&archived);

    let error = w.migrate().unwrap_err().to_string();

    assert!(error.contains("stray-top") && error.contains("root console"), "{error}");
    assert!(!error.contains("archived-top"), "only live contexts refuse: {error}");
    assert_eq!(w.cc(&stray), None, "nothing was written");

    w.set_senpai(&stray, &console);
    w.migrate().unwrap();
    assert_eq!(w.cc(&archived).unwrap().senpai_ctx, None, "an archived top is carried as it was");
}

#[test]
fn values_the_migration_does_not_carry_are_reported() {
    let w = legacy_world();
    let house = w.house();
    let [amy, lead, nobody] = [w.character("amy"), w.character("lead"), w.character("nobody")];
    w.default_cast(lead, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    // A root console's senpai_ctx.
    let elsewhere = w.ctx("elsewhere", "default", None, None);
    w.set_senpai(&console, &elsewhere);
    let seat = w.ctx("seat", "coder", Some(lead), None);
    w.set_senpai(&seat, &console);
    // A context nobody plays: its senpai_ctx, cast and override run nothing.
    let unplayed = w.ctx("unplayed", "coder", None, None);
    w.set_senpai(&unplayed, &seat);
    w.set_cast(&unplayed, house);
    w.set_override(&unplayed, "anthropic", "claude-x");
    // An archived context whose senpai has no context character.
    let archived = w.ctx("archived-lane", "coder", Some(nobody), None);
    w.set_senpai(&archived, &unplayed);
    w.archive(&archived);

    w.migrate().unwrap();

    let about = |kind: &str, ctx: &ContextRow, needle: &str| {
        w.notes(kind).iter().any(|n| n.context_id == Some(ctx.context_id) && n.note.contains(needle))
    };
    assert!(about("dropped", &console, "root console"), "{:?}", w.notes("dropped"));
    assert_eq!(w.cc(&console).unwrap().senpai_ctx, None);
    assert!(about("unmapped", &unplayed, "senpai_ctx"), "{:?}", w.notes("unmapped"));
    assert!(about("dropped", &unplayed, "cast"), "{:?}", w.notes("dropped"));
    assert!(about("dropped", &archived, "no context character"), "{:?}", w.notes("dropped"));
    assert_eq!(w.cc(&archived).unwrap().senpai_ctx, None);
    assert_eq!(w.cc(&unplayed), None);
}

/// A live context whose senpai has no context character refuses.
#[test]
fn a_live_context_under_a_senpai_without_a_context_character_refuses() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who] = [w.character("amy"), w.character("coder")];
    w.default_cast(who, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let unplayed = w.ctx("unplayed", "coder", None, None);
    let lane = w.ctx("lane", "coder", Some(who), None);
    w.set_senpai(&lane, &unplayed);

    let error = w.migrate().unwrap_err().to_string();
    assert!(error.contains("lane") && error.contains("unplayed") && error.contains("no context character"), "{error}");
}

#[test]
fn live_contexts_without_a_gate_or_distill_slot_are_listed() {
    let w = legacy_world();
    let house = w.house();
    let thin = w.cast("thin");
    w.slot(thin, "coder", w.deepseek, "ds-flash", None);
    let [amy, full, bare, gone] = [w.character("amy"), w.character("full"), w.character("bare"), w.character("gone")];
    w.default_cast(full, house);
    w.default_cast(bare, thin);
    w.default_cast(gone, thin);
    let distill = w.slot(house, "distill", w.deepseek, "ds-flash", None);
    assert_eq!(distill.role, "distill");
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let a = w.ctx("full", "coder", Some(full), None);
    let b = w.ctx("bare", "coder", Some(bare), None);
    let c = w.ctx("gone", "coder", Some(gone), None);
    let shadow = w.ctx("shadow", "judge", None, Some(&a));
    for ctx in [&a, &b, &c] {
        w.set_senpai(ctx, &console);
    }
    w.archive(&c);

    w.migrate().unwrap();

    let no_gate: Vec<_> = w.notes("no_gate_slot").into_iter().map(|n| n.context_id).collect();
    let no_distill: Vec<_> = w.notes("no_distill_slot").into_iter().map(|n| n.context_id).collect();
    assert_eq!(no_gate, vec![Some(b.context_id)], "only the live context whose cast has no gate slot, not a root, a shadow or an archived one");
    assert_eq!(no_distill, vec![Some(b.context_id)], "the full cast has a distill slot");
    assert!(w.db.context_slot(a.context_id, "distill").unwrap().is_some(), "a distill slot is copied only from the cast's own");
    assert!(w.db.context_slot(b.context_id, "gate").unwrap().is_none(), "none is invented");
    assert!(w.cc(&shadow).is_some());
}

#[test]
fn the_migration_runs_once() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who] = [w.character("amy"), w.character("coder")];
    w.default_cast(who, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    let seat = w.ctx("seat", "coder", Some(who), None);
    w.set_senpai(&seat, &console);
    w.migrate().unwrap();
    let later = w.ctx("later", "coder", Some(who), None);

    KernelDb::migrate_context_characters(&w.db.conn).unwrap();

    assert_eq!(w.count("context_characters"), 2);
    assert_eq!(w.cc(&later), None, "a marked database is left alone");
    assert!(w.cc(&seat).is_some());
}

// ── Dropping what the context character replaced ────────────────────

/// Every column and table the context character replaced, as `(table,
/// column)`; a column of `None` is the whole table.
const DROPPED: [(&str, Option<&str>); 8] = [
    ("contexts", Some("senpai_ctx")),
    ("contexts", Some("played_by")),
    ("contexts", Some("cast_id")),
    ("contexts", Some("provider")),
    ("contexts", Some("model")),
    ("presets", Some("cast_id")),
    ("presets", Some("system_prompt")),
    ("llm_defaults", None),
];

fn exists(w: &World, (table, column): (&str, Option<&str>)) -> bool {
    match column {
        Some(column) => KernelDb::has_column(&w.db.conn, table, column).unwrap(),
        None => KernelDb::has_table(&w.db.conn, table).unwrap(),
    }
}

/// An open on a database at the previous schema runs `context-characters-v1`
/// first, which reads the old columns, and drops them after: the context
/// characters carry what the columns held, the columns and `llm_defaults` are
/// gone, `characters.default_cast_id` is `characters.cast_id`, `root_ctx`
/// stays, and rows in the tables that lost columns survive. A second open does
/// nothing.
#[test]
fn an_open_carries_the_old_columns_into_context_characters_then_drops_them() {
    let w = legacy_world();
    let house = w.house();
    let [amy, who, lead] = [w.character("amy"), w.character("coder"), w.character("lead")];
    w.default_cast(who, house);
    w.default_cast(lead, house);
    let console = w.ctx("amy", "root", Some(amy), None);
    w.console(amy, &console);
    // The senpai migration maps each edge from `forked_from` first.
    let seat = w.ctx("seat", "coder", Some(lead), Some(&console));
    let overridden = w.ctx("overridden", "coder", Some(who), Some(&seat));
    w.set_override(&overridden, "anthropic", "claude-x");
    w.exec(
        "INSERT INTO presets (preset_id, label, cast_id, system_prompt, created_at, created_by)
         VALUES (?1, 'with-cast', ?2, 'be terse', 0, ?3)",
        params![PresetId::new().as_bytes().as_slice(), blob_param(house.as_bytes()), blob_param(who.as_bytes())],
    );
    w.exec(
        "INSERT INTO turns (turn_id, context_id, performer, role, provider, model, started_at)
         VALUES (?1, ?2, ?3, 'coder', 'deepseek', 'ds-flash', 1)",
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
    assert!(KernelDb::has_column(&w.db.conn, "characters", "root_ctx").unwrap(), "root_ctx is the console's pointer");
    assert_eq!(w.db.character_cast(who).unwrap().map(|c| c.label), Some("house".to_string()), "characters.cast_id holds the old default cast");
    assert_eq!(w.db.root_console(amy).unwrap(), Some(console.context_id));
    assert_eq!(w.db.get_context(seat.context_id).unwrap().unwrap().played_by, Some(lead), "the performer was carried before the column went");
    let slot = w.db.context_slot(overridden.context_id, "coder").unwrap().expect("the override's slot");
    assert_eq!((slot.backend_id, slot.model.as_str()), (w.anthropic, "claude-x"), "the override was carried into the slots");
    assert_eq!(w.db.senpai_of(overridden.context_id).unwrap(), Some(seat.context_id), "the senpai edge was carried");
    assert_eq!(w.count("turns"), 1, "a turn survives the open");
    assert_eq!(w.count("presets"), 1, "a preset survives the loss of its columns");
    assert_eq!(w.count("senpai_migration_report"), 1, "kj senpai show still reads the report");

    KernelDb::apply_additive_migrations(&w.db.conn).unwrap();
    assert_eq!(w.db.senpai_of(overridden.context_id).unwrap(), Some(seat.context_id), "a second open changes nothing");
}

/// The drop refuses before `context-characters-v1` has recorded its mapping:
/// the columns it reads are still there for the next try.
#[test]
fn the_drop_refuses_before_the_context_character_migration_has_run() {
    let w = legacy_world();
    let error = KernelDb::drop_pre_context_character_columns(&w.db.conn).unwrap_err().to_string();
    assert!(error.contains("before the context character migration"), "{error}");
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
    for table in ["character_instances", "instance_bindings", "instance_slots", "instance_migration_report"] {
        assert!(!KernelDb::has_table(&w.db.conn, table).unwrap(), "{table} is gone");
    }
    for (table, column) in [("contexts", "instance_id"), ("contexts", "binding_seq"), ("turns", "instance_id"), ("turns", "binding_seq")] {
        assert!(!KernelDb::has_column(&w.db.conn, table, column).unwrap(), "{table}.{column} is gone");
    }
    assert!(KernelDb::has_column(&w.db.conn, "characters", "cast_id").unwrap());
    assert!(KernelDb::has_column(&w.db.conn, "characters", "root_ctx").unwrap());
    assert_eq!(w.count("kernel_migrations WHERE name IN ('senpai-edges-v1', 'context-characters-v1')"), 0);
    KernelDb::apply_additive_migrations(&w.db.conn).unwrap();
}

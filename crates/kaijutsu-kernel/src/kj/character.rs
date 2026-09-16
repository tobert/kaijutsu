//! `kj character` — the persistent someone a name resolves to (slice 1,
//! "the sheet": `docs/character.md`). `create` mints a fresh principal id
//! and its sheet row, idempotent on the name; `list`/`show` read it;
//! `retire` stamps `retired_at` and, in the same act, concludes and
//! archives every live context the character plays.
//!
//! No attribution change in this slice: a context's `played_by` is metadata
//! only, and every block is still stamped exactly as it is today
//! (`docs/character.md`, "A context is played by a character").

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId, ContextState, PrincipalId, SessionId};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::control::ConsentMode;
use crate::kernel_db::{CharacterRow, ContextRow};

/// The rc bundle a root context runs.
pub const ROOT_CONTEXT_TYPE: &str = "root";

#[derive(Parser, Debug)]
#[command(
    name = "character",
    about = "The persistent someone a name resolves to",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct CharacterArgs {
    #[command(subcommand)]
    command: CharacterCommand,
}

#[derive(Subcommand, Debug)]
enum CharacterCommand {
    /// Create a character: mint a fresh principal id and write its sheet
    /// row. Idempotent on the name — creating an existing name returns the
    /// existing row rather than erroring or minting a second principal for
    /// the same name.
    Create {
        /// The character's kernel-owned given name.
        name: String,
        /// Create a root character: a place with hands on it, never a seat
        /// a model plays. A root cannot be cast as a context's performer
        /// and cannot take a model turn.
        #[arg(long)]
        root: bool,
    },
    /// List the live (non-retired) characters.
    #[command(alias = "ls")]
    List {
        /// Include retired characters too.
        #[arg(long)]
        all: bool,
        /// Emit each row as a JSON object (name, id, timestamps, root)
        /// instead of the default array of ids.
        #[arg(long)]
        json: bool,
    },
    /// Show one character's sheet.
    Show {
        /// The character's name.
        name: String,
    },
    /// Update a character's sheet. Currently only `root`.
    Set {
        /// The character to update.
        name: String,
        /// Make this character a root: no model, and it cannot be cast as
        /// a context's performer.
        #[arg(long, conflicts_with = "no_root")]
        root: bool,
        /// Make this character an ordinary one that a model can play.
        #[arg(long = "no-root", conflicts_with = "root")]
        no_root: bool,
    },
    /// Retire a character: stamp `retired_at`, and conclude + archive every
    /// live context it plays, in the same act. There is no reassignment and
    /// no verb to move a context to another character — a retired
    /// character's contexts are archived, not orphaned.
    Retire {
        /// The character's name.
        name: String,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_character(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<CharacterArgs>();
        }
        let parsed = match CharacterArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj character: {e}"));
            }
        };

        // Mutating verbs need the same admin-shaped gate `kj cast`'s writes
        // use — minting, re-rooting, or retiring an identity is global,
        // cross-context state, not something a context should be able to do
        // to itself.
        let mutating = matches!(
            parsed.command,
            CharacterCommand::Create { .. }
                | CharacterCommand::Set { .. }
                | CharacterCommand::Retire { .. }
        );
        if mutating
            && let Err(denied) =
                self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "character")
        {
            return denied;
        }

        match parsed.command {
            CharacterCommand::Create { name, root } => self.character_create(&name, root, caller).await,
            CharacterCommand::List { all, json } => self.character_list(all, json),
            CharacterCommand::Show { name } => self.character_show(&name),
            CharacterCommand::Set { name, root, no_root } => {
                self.character_set(&name, root, no_root, caller).await
            }
            CharacterCommand::Retire { name } => self.character_retire(&name),
        }
    }

    /// Idempotent on `name`: an existing row is returned unchanged rather
    /// than minting a second principal for the same name — the double-mint
    /// hazard `add-key` avoids by binding instead of minting
    /// (`docs/character.md`, "Adding a key binds; it never mints") applies
    /// just as much to a repeated `create`.
    async fn character_create(&self, name: &str, root: bool, caller: &KjCaller) -> KjResult {
        let row = {
            let db = self.kernel_db().lock();
            match db.get_character_by_name(name) {
                Ok(Some(existing)) => {
                    return KjResult::ok_with_data(
                        format!("{name} already exists ({})", existing.principal_id.short()),
                        character_to_json(&existing),
                    );
                }
                Ok(None) => {}
                Err(e) => return KjResult::Err(format!("kj character create: {e}")),
            }

            let row = CharacterRow {
                principal_id: PrincipalId::new(),
                name: name.to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                retired_at: None,
                handoff_ctx: None, root_ctx: None,
                root,
            };
            if let Err(e) = db.insert_character(&row) {
                return KjResult::Err(format!("kj character create: {e}"));
            }
            row
        };
        if !root {
            return KjResult::ok_with_data(
                format!("created {name} ({})", row.principal_id.short()),
                character_to_json(&row),
            );
        }
        let root_ctx = match self.ensure_root_context(row.principal_id, caller.principal_id).await {
            Ok(ctx) => ctx,
            Err(e) => return KjResult::Err(format!(
                "kj character create: created {name} as a root, but not its root context: {e}"
            )),
        };
        let row = CharacterRow { root_ctx: Some(root_ctx), ..row };
        KjResult::ok_with_data(
            format!(
                "created {name} ({}) as a root, with root context '{name}' ({})",
                row.principal_id.short(),
                root_ctx.short()
            ),
            character_to_json(&row),
        )
    }

    /// Create the root context of each live root character that has none.
    /// The kernel calls this at start. Returns the contexts it created.
    pub async fn ensure_root_contexts(&self, requester: PrincipalId) -> Result<Vec<ContextId>, String> {
        let missing: Vec<PrincipalId> = self
            .kernel_db()
            .lock()
            .list_characters(false)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|row| row.root && row.root_ctx.is_none())
            .map(|row| row.principal_id)
            .collect();
        let mut created = Vec::with_capacity(missing.len());
        for principal in missing {
            created.push(self.ensure_root_context(principal, requester).await?);
        }
        Ok(created)
    }

    /// Return a live root character's root context, creating it when the
    /// sheet has none: type `root`, labeled with the character's name,
    /// played by it, with no parent. `requester` becomes `created_by`.
    async fn ensure_root_context(&self, character: PrincipalId, requester: PrincipalId) -> Result<ContextId, String> {
        let new_id = ContextId::new();
        let name = {
            let db = self.kernel_db().lock();
            let sheet = db
                .get_character(character)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("character {} has no sheet", character.short()))?;
            if let Some(existing) = sheet.root_ctx {
                return Ok(existing);
            }
            if !sheet.root {
                return Err(format!("{} is not a root character", sheet.name));
            }
            if sheet.retired_at.is_some() {
                return Err(format!("{} is retired", sheet.name));
            }
            let row = ContextRow {
                context_id: new_id,
                label: Some(sheet.name.clone()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: ContextState::Live,
                context_type: ROOT_CONTEXT_TYPE.to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: requester,
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
                cast_id: None,
                origin_host: None,
                played_by: Some(character),
                reviewer_id: None,
                director_id: None,
            };
            db.in_transaction(|db| {
                let default_ws = db.get_or_create_default_workspace(requester)?;
                db.insert_context_with_document(&row, default_ws)?;
                db.set_character_root_ctx(character, Some(new_id))
            })
            .map_err(|e| format!("could not create the root context '{}': {e}", sheet.name))?;
            sheet.name
        };

        self.drift_router()
            .write()
            .register(new_id, Some(&name), None, requester)
            .map_err(|e| format!("could not register the root context '{name}': {e}"))?;

        let rc_caller = KjCaller {
            principal_id: requester,
            actor_id: character,
            reviewer_id: None,
            context_id: Some(new_id),
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
        };
        self.run_rc_lifecycle("create", new_id, None, None, None, &rc_caller)
            .await
            .map_err(|e| format!("root context '{name}' rc create lifecycle: {e}"))?;
        Ok(new_id)
    }

    /// `list` → an array of full-id strings by default
    /// (`project_kj_structured_data` convention): a caller iterating the
    /// result never has to re-derive a truncated display id. `--json` opts
    /// into the full row per entry, `root` included, for a caller
    /// that wants the sheet without a `show` per name.
    fn character_list(&self, all: bool, json: bool) -> KjResult {
        let db = self.kernel_db().lock();
        let rows = match db.list_characters(all) {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj character list: {e}")),
        };
        if json {
            let data = serde_json::Value::Array(rows.iter().map(character_to_json).collect());
            return KjResult::ok_with_data(data.to_string(), data);
        }
        let data = serde_json::Value::Array(
            rows.iter()
                .map(|r| serde_json::Value::String(r.principal_id.to_hex()))
                .collect(),
        );
        if rows.is_empty() {
            return KjResult::ok_with_data("(no characters yet)".to_string(), data);
        }
        let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        let lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let status = if r.retired_at.is_some() { "  (retired)" } else { "" };
                format!("  {:<name_w$}  {}{status}", r.name, r.principal_id.short())
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// `show` → an object carrying the whole row (`project_kj_structured_data`
    /// convention).
    fn character_show(&self, name: &str) -> KjResult {
        let db = self.kernel_db().lock();
        match db.get_character_by_name(name) {
            Ok(Some(row)) => {
                let text = format!(
                    "Character:       {}\nID:              {}\nRoot:            {}\n\
                     Created:         {}\nRetired:         {}",
                    row.name,
                    row.principal_id.to_hex(),
                    if row.root { "yes" } else { "no" },
                    super::format::format_timestamp(row.created_at),
                    row.retired_at
                        .map(super::format::format_timestamp)
                        .unwrap_or_else(|| "-".to_string()),
                );
                KjResult::ok_with_data(text, character_to_json(&row))
            }
            // A missing mapping is corruption, never a fallback to a
            // default identity — say so loudly rather than rendering
            // something that looks like an answer.
            Ok(None) => KjResult::Err(format!(
                "kj character show: no character named '{name}' — \
                 `kj character list` to see who exists"
            )),
            Err(e) => KjResult::Err(format!("kj character show: {e}")),
        }
    }

    /// Set or clear `root`. A root has no model: it cannot be cast as a
    /// context's performer and turn identity refuses it
    /// (`docs/character.md`, "Roots and rotation").
    async fn character_set(&self, name: &str, root: bool, no_root: bool, caller: &KjCaller) -> KjResult {
        if !root && !no_root {
            return KjResult::Err("kj character set: specify --root or --no-root".to_string());
        }
        let principal_id = {
            let db = self.kernel_db().lock();
            let row = match db.get_character_by_name(name) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj character set: no character named '{name}' — \
                         `kj character list` to see who exists"
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj character set: {e}")),
            };
            if no_root && let Some(ctx) = row.root_ctx {
                match db.get_context(ctx) {
                    Ok(Some(context)) if !context.is_archived() => {
                        return KjResult::Err(format!(
                            "kj character set: {name} plays its live root context '{}' ({}), \
                             and a root context is played by a root; archive it first",
                            context.label.as_deref().unwrap_or(name),
                            ctx.short()
                        ));
                    }
                    Ok(_) => {}
                    Err(e) => return KjResult::Err(format!("kj character set: {e}")),
                }
            }
            if let Err(e) = db.update_character_root(row.principal_id, root) {
                return KjResult::Err(format!("kj character set: {e}"));
            }
            row.principal_id
        };
        if root && let Err(e) = self.ensure_root_context(principal_id, caller.principal_id).await {
            return KjResult::Err(format!(
                "kj character set: {name} is now a root, but has no root context: {e}"
            ));
        }
        let updated = match self.kernel_db().lock().get_character(principal_id) {
            Ok(Some(r)) => r,
            Ok(None) => return KjResult::Err(format!("kj character set: {name} vanished")),
            Err(e) => return KjResult::Err(format!("kj character set: {e}")),
        };
        let msg = if root {
            format!("{name} is now a root")
        } else {
            format!("{name} is no longer a root")
        };
        KjResult::ok_with_data(msg, character_to_json(&updated))
    }

    /// Retire: stamp `retired_at`, and conclude + archive every live
    /// context the character plays, in the same act
    /// (`docs/character.md`, "Retire takes its contexts with it").
    /// `Destroy`-classed (`kj/effect.rs`); the dispatcher latches
    /// an unconfirmed call before this handler runs, whether or not the
    /// character has anything live to archive — the archival loop below is
    /// not separately confirmed, since that one latch already covers the
    /// whole batch as one act.
    fn character_retire(&self, name: &str) -> KjResult {
        let db = self.kernel_db().lock();
        let row = match db.get_character_by_name(name) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return KjResult::Err(format!(
                    "kj character retire: no character named '{name}' — \
                     `kj character list` to see who exists"
                ));
            }
            Err(e) => return KjResult::Err(format!("kj character retire: {e}")),
        };
        if row.retired_at.is_some() {
            return KjResult::ok(format!("{name} already retired"));
        }
        let live = match db.contexts_played_by(row.principal_id) {
            Ok(l) => l,
            Err(e) => return KjResult::Err(format!("kj character retire: {e}")),
        };

        // Reuse the archive path `kj context archive` calls
        // (`KernelDb::conclude_context` / `archive_context`) rather than a
        // second archival implementation — conclude first (gives up the
        // ring-0 seat, same ladder `kj context archive` walks), then
        // archive (soft delete + sweep the context's unresolved asks).
        for ctx in &live {
            if let Err(e) = db.conclude_context(ctx.context_id) {
                return KjResult::Err(format!(
                    "kj character retire: failed to conclude {}: {e}",
                    ctx.context_id.short()
                ));
            }
            if let Err(e) = db.archive_context(ctx.context_id) {
                return KjResult::Err(format!(
                    "kj character retire: failed to archive {}: {e}",
                    ctx.context_id.short()
                ));
            }
        }
        let now = kaijutsu_types::now_millis() as i64;
        if let Err(e) = db.retire_character(row.principal_id, now) {
            return KjResult::Err(format!("kj character retire: {e}"));
        }
        drop(db);

        // The in-memory drift router must agree with the row, or an active
        // session could still write a drift op into a context whose
        // character no longer plays it (mirrors `kj context archive`).
        {
            let mut drift = self.drift_router().write();
            for ctx in &live {
                let _ = drift.set_state(ctx.context_id, ContextState::Archived);
            }
        }

        KjResult::ok(format!("retired {name}: {} context(s) archived", live.len()))
    }
}

fn character_to_json(row: &CharacterRow) -> serde_json::Value {
    serde_json::json!({
        "principal_id": row.principal_id.to_hex(),
        "name": row.name,
        "created_at": row.created_at,
        "retired_at": row.retired_at,
        "root": row.root,
        "root_ctx": row.root_ctx.map(|ctx| ctx.to_hex()),
    })
}

// Verb class: kj/effect.rs
use super::effect::{Classify, Effect};

impl Classify for CharacterArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for CharacterCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::List { .. } | Self::Show { .. } => Effect::Read,
            Self::Create { .. } | Self::Set { .. } => Effect::Write,
            Self::Retire { .. } => Effect::Destroy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{caller_with_context, register_context, test_caller};
    use super::super::KjResult;
    use crate::mcp::ContextToolBinding;
    use kaijutsu_types::PrincipalId;

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// `create` mints a fresh principal and a sheet row.
    #[tokio::test]
    async fn create_mints_a_character() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let result = d.dispatch(&[s("character"), s("create"), s("hajime")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Object(obj)), .. } = result else {
            panic!("expected Ok with an object, got {result:?}");
        };
        assert_eq!(obj["name"], "hajime");
        assert!(obj["principal_id"].as_str().unwrap().len() == 32, "expected a full hex id");
        assert!(obj["retired_at"].is_null());

        let row = d.kernel_db().lock().get_character_by_name("hajime").unwrap();
        assert!(row.is_some(), "the row must actually be persisted");
    }

    /// `create` on an existing name returns the SAME principal id rather
    /// than minting a second one for the same name.
    #[tokio::test]
    async fn create_is_idempotent_on_name() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let first = d.dispatch(&[s("character"), s("create"), s("hajime")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Object(first_obj)), .. } = first else {
            panic!("expected Ok, got {first:?}");
        };
        let second = d.dispatch(&[s("character"), s("create"), s("hajime")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Object(second_obj)), .. } = second else {
            panic!("expected Ok, got {second:?}");
        };
        assert_eq!(
            first_obj["principal_id"], second_obj["principal_id"],
            "a repeated create must return the SAME principal, never mint a second one"
        );

        let all = d.kernel_db().lock().list_characters(true).unwrap();
        assert_eq!(all.iter().filter(|sheet| sheet.name == "hajime").count(), 1, "only one row must exist for the name");
    }

    /// `list` surfaces only live characters by default, as a full-id array.
    #[tokio::test]
    async fn list_shows_live_characters_as_full_ids() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("amy")], &caller).await;
        let result = d.dispatch(&[s("character"), s("list")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Array(rows)), .. } = result else {
            panic!("expected a JSON array, got {result:?}");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].as_str().unwrap().len(), 32, "list must carry full ids, never short forms");
    }

    /// `show` on an unknown name fails loudly — no silent fallback to a
    /// default identity or to `system`.
    #[tokio::test]
    async fn show_unknown_name_fails_loudly() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let result = d.dispatch(&[s("character"), s("show"), s("nobody")], &caller).await;
        assert!(matches!(result, KjResult::Err(_)), "expected Err, got {result:?}");
    }

    /// A non-privileged caller with no `ConfigWrite` grant cannot mint a
    /// character.
    #[tokio::test]
    async fn create_denied_without_config_write() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("no-config-write"), None, principal);
        // Narrow the loadout register_context grants by default.
        d.kernel_db()
            .lock()
            .upsert_context_binding(ctx, &ContextToolBinding::new())
            .unwrap();
        let caller = caller_with_context(ctx);
        let result = d.dispatch(&[s("character"), s("create"), s("intruder")], &caller).await;
        assert!(matches!(result, KjResult::Err(_)), "expected denial, got {result:?}");
        assert!(d.kernel_db().lock().get_character_by_name("intruder").unwrap().is_none());
    }

    /// `retire` latches an unconfirmed call even with no live contexts to
    /// archive — `Destroy` always latches (`kj/effect.rs`, slice
    /// 3), regardless of what the handler would find. Confirmed, it
    /// completes on the next call.
    #[tokio::test]
    async fn retire_with_no_contexts_still_latches_then_completes() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("hajime")], &caller).await;
        let latch = d.dispatch(&[s("character"), s("retire"), s("hajime")], &caller).await;
        assert!(latch.is_latch(), "expected a latch even with no live contexts, got {latch:?}");

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result =
            d.dispatch(&[s("character"), s("retire"), s("hajime")], &confirmed).await;
        assert!(matches!(result, KjResult::Ok { .. }), "{result:?}");
        let row = d.kernel_db().lock().get_character_by_name("hajime").unwrap().unwrap();
        assert!(row.retired_at.is_some());
    }

    /// Retiring a character with live contexts latches first, then — once
    /// confirmed — concludes and archives every one of them, leaving their
    /// blocks and block authors untouched (no attribution change in this
    /// slice).
    #[tokio::test]
    async fn retire_archives_live_contexts_and_leaves_blocks_untouched() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("kaijutsu-lead")], &caller).await;
        let principal_id = {
            let db = d.kernel_db().lock();
            db.get_character_by_name("kaijutsu-lead").unwrap().unwrap().principal_id
        };

        // A live context played by the character, with a block authored by
        // it — the thing retire must leave untouched.
        let ctx = register_context(&d, Some("lead-ctx"), None, principal_id);
        d.kernel_db().lock().update_played_by(ctx, Some(principal_id)).unwrap();
        d.block_store()
            .create_document(ctx, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "hello from the lead",
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
                Some(principal_id),
            )
            .unwrap();

        let latch =
            d.dispatch(&[s("character"), s("retire"), s("kaijutsu-lead")], &caller).await;
        assert!(latch.is_latch(), "expected a latch with live contexts pending, got {latch:?}");

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result =
            d.dispatch(&[s("character"), s("retire"), s("kaijutsu-lead")], &confirmed).await;
        assert!(matches!(result, KjResult::Ok { .. }), "{result:?}");

        let ctx_row = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert!(ctx_row.archived_at.is_some(), "the character's live context must be archived");
        assert!(ctx_row.concluded_at.is_some(), "and concluded on the way");

        let char_row = d.kernel_db().lock().get_character_by_name("kaijutsu-lead").unwrap().unwrap();
        assert!(char_row.retired_at.is_some());

        // The block and its author are untouched — no attribution change.
        let blocks = d.block_store().block_snapshots(ctx).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].author(), principal_id, "block authorship must not change");
    }

    /// A NULL `played_by` is not a marker some code path special-cases —
    /// `kj context info` on an ordinary context (nobody playing it) must
    /// behave exactly as it did before this column existed.
    #[tokio::test]
    async fn null_played_by_preserves_context_info() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("plain-ctx"), None, principal);
        let caller = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &caller).await;
        let KjResult::Ok { data: Some(data), .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert_eq!(data["played_by"], serde_json::Value::Null);
        assert_eq!(data["played_by_name"], serde_json::Value::Null);
    }

    /// The emitted help is a product surface: every subcommand and flag
    /// this module publishes is read by a player, so the text is pinned
    /// here rather than left to drift (`docs/writing.md`).
    #[tokio::test]
    async fn published_help_names_root_and_no_sheet_relation() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let KjResult::Ok { message, .. } = d.dispatch(&[s("character"), s("--help")], &caller).await
        else { panic!("help must render") };
        println!("{message}");
        assert!(!message.contains("accountable"), "the sheet chain is gone: {message}");
        for (argv, expected) in [
            (vec![s("character"), s("create"), s("--help")], "--root"),
            (vec![s("character"), s("set"), s("--help")], "--no-root"),
        ] {
            let KjResult::Ok { message, .. } = d.dispatch(&argv, &caller).await
            else { panic!("help must render for {argv:?}") };
            println!("{message}");
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("accountable"), "{message}");
        }
    }

    /// `create --root` writes the flag, and `show`/`list --json` both
    /// carry it. An ordinary create is not a root.
    #[tokio::test]
    async fn create_root_flag_reaches_the_sheet_and_every_reader() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller).await;
        d.dispatch(&[s("character"), s("create"), s("coder")], &caller).await;

        assert!(d.kernel_db().lock().get_character_by_name("sovereign").unwrap().unwrap().root);
        assert!(!d.kernel_db().lock().get_character_by_name("coder").unwrap().unwrap().root);

        let KjResult::Ok { message, .. } = d.dispatch(&[s("character"), s("show"), s("sovereign")], &caller).await
        else { panic!("show must succeed") };
        assert!(message.contains("Root:            yes"), "{message}");
        let KjResult::Ok { message, .. } = d.dispatch(&[s("character"), s("show"), s("coder")], &caller).await
        else { panic!("show must succeed") };
        assert!(message.contains("Root:            no"), "{message}");

        let result = d.dispatch(&[s("character"), s("list"), s("--json")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Array(rows)), .. } = result else {
            panic!("expected a JSON array, got {result:?}");
        };
        let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap_or_else(|| panic!("{name} present")).clone();
        assert_eq!(row("sovereign")["root"], serde_json::Value::Bool(true));
        assert_eq!(row("coder")["root"], serde_json::Value::Bool(false));
    }

    /// `create --root` creates the character's root context: type `root`,
    /// labeled with its name, played by it, with no parent, recorded on the
    /// sheet. `tests/rc_role_bindings.rs` checks that its rc bundle binds it.
    #[tokio::test]
    async fn create_root_makes_its_root_context() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let created = d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller).await;
        assert!(matches!(created, KjResult::Ok { .. }), "{created:?}");

        let sheet = d.kernel_db().lock().get_character_by_name("sovereign").unwrap().unwrap();
        let root_ctx = sheet.root_ctx.expect("a root character records its root context");
        let row = d.kernel_db().lock().get_context(root_ctx).unwrap().unwrap();
        assert_eq!(row.label.as_deref(), Some("sovereign"));
        assert_eq!(row.context_type, "root");
        assert_eq!(row.played_by, Some(sheet.principal_id));
        assert_eq!(row.forked_from, None);
        assert_eq!(row.created_by, caller.principal_id, "the requester stays the creator");
        assert!(row.archived_at.is_none());

        let KjResult::Ok { data: Some(data), .. } =
            d.dispatch(&[s("character"), s("show"), s("sovereign")], &caller).await
        else { panic!("show must succeed") };
        assert_eq!(data["root_ctx"], serde_json::Value::String(root_ctx.to_hex()));

        let again = d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller).await;
        assert!(matches!(again, KjResult::Ok { .. }), "{again:?}");
        let live_roots = d.kernel_db().lock().list_all_contexts().unwrap()
            .into_iter().filter(|c| c.context_type == "root" && c.archived_at.is_none()).count();
        assert_eq!(live_roots, 1, "a repeated create must not make a second root context");
    }

    /// An ordinary character gets no context at create.
    #[tokio::test]
    async fn ordinary_create_makes_no_context() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let before = d.kernel_db().lock().list_all_contexts().unwrap().len();
        d.dispatch(&[s("character"), s("create"), s("coder")], &caller).await;
        let sheet = d.kernel_db().lock().get_character_by_name("coder").unwrap().unwrap();
        assert_eq!(sheet.root_ctx, None);
        assert_eq!(d.kernel_db().lock().list_all_contexts().unwrap().len(), before);
    }

    /// `set --root` creates the root context the flag requires, and
    /// `set --no-root` refuses while that context is live, since a root
    /// context is played by a root.
    #[tokio::test]
    async fn set_root_makes_the_root_context_and_no_root_refuses_while_live() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("laptop")], &caller).await;
        let set = d.dispatch(&[s("character"), s("set"), s("laptop"), s("--root")], &caller).await;
        assert!(matches!(set, KjResult::Ok { .. }), "{set:?}");
        let sheet = d.kernel_db().lock().get_character_by_name("laptop").unwrap().unwrap();
        let root_ctx = sheet.root_ctx.expect("set --root creates the root context");
        assert_eq!(d.kernel_db().lock().get_context(root_ctx).unwrap().unwrap().label.as_deref(), Some("laptop"));

        let cleared = d.dispatch(&[s("character"), s("set"), s("laptop"), s("--no-root")], &caller).await;
        let KjResult::Err(message) = cleared else { panic!("expected a refusal, got {cleared:?}") };
        assert!(message.contains("root context"), "{message}");
        assert!(d.kernel_db().lock().get_character_by_name("laptop").unwrap().unwrap().root);
    }

    /// At start the kernel creates a missing root context for each live
    /// root character, and a second pass creates nothing.
    #[tokio::test]
    async fn ensure_root_contexts_fills_missing_and_is_idempotent() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let keeper = PrincipalId::new();
        let retired = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            for (principal_id, name, retired_at) in [(keeper, "keeper", None), (retired, "old", Some(1))] {
                db.insert_character(&crate::kernel_db::CharacterRow {
                    principal_id, name: name.into(), created_at: 0, retired_at,
                    handoff_ctx: None, root_ctx: None, root: true,
                }).unwrap();
            }
        }

        let created = d.ensure_root_contexts(PrincipalId::system()).await.unwrap();
        assert_eq!(created.len(), 1, "only the live root gets a context");
        let sheet = d.kernel_db().lock().get_character(keeper).unwrap().unwrap();
        assert_eq!(sheet.root_ctx, Some(created[0]));
        assert_eq!(d.kernel_db().lock().get_character(retired).unwrap().unwrap().root_ctx, None);

        assert!(d.ensure_root_contexts(PrincipalId::system()).await.unwrap().is_empty());
    }

    /// `set --root` and `set --no-root` round-trip, and `set` with neither
    /// is a usage error rather than a silent no-op.
    #[tokio::test]
    async fn set_root_and_no_root_round_trip() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("worker")], &caller).await;

        let set = d.dispatch(&[s("character"), s("set"), s("worker"), s("--root")], &caller).await;
        assert!(matches!(set, KjResult::Ok { .. }), "{set:?}");
        assert!(d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().root);

        let root_ctx = d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().root_ctx.unwrap();
        assert!(d.kernel_db().lock().archive_context(root_ctx).unwrap());
        let cleared = d.dispatch(&[s("character"), s("set"), s("worker"), s("--no-root")], &caller).await;
        assert!(matches!(cleared, KjResult::Ok { .. }), "{cleared:?}");
        assert!(!d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().root);

        let neither = d.dispatch(&[s("character"), s("set"), s("worker")], &caller).await;
        assert!(matches!(neither, KjResult::Err(_)), "expected Err, got {neither:?}");
        let both = d.dispatch(&[s("character"), s("set"), s("worker"), s("--root"), s("--no-root")], &caller).await;
        assert!(matches!(both, KjResult::Err(_)), "expected Err, got {both:?}");
    }

    /// `retire` no longer consults any sheet relation: a character with a
    /// live context still retires, taking that context with it.
    #[tokio::test]
    async fn retire_takes_its_contexts_with_it() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("mentor")], &caller).await;
        let mentor_id = {
            let db = d.kernel_db().lock();
            db.get_character_by_name("mentor").unwrap().unwrap().principal_id
        };
        let ctx = register_context(&d, Some("mentor-ctx"), None, mentor_id);
        d.kernel_db().lock().update_played_by(ctx, Some(mentor_id)).unwrap();

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result = d.dispatch(&[s("character"), s("retire"), s("mentor")], &confirmed).await;
        assert!(matches!(result, KjResult::Ok { .. }), "expected Ok, got {result:?}");
        let ctx_row = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert!(ctx_row.archived_at.is_some(), "retirement archives the character's live contexts");
        assert!(d.kernel_db().lock().get_character_by_name("mentor").unwrap().unwrap().retired_at.is_some());
    }
}

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
use kaijutsu_types::{ContentType, ContextState, PrincipalId};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::kernel_db::CharacterRow;

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
        /// The character this one is accountable to. Omit to create a root
        /// (a human, or a character deliberately left unrooted) —
        /// accountability is a chain, and every model character is meant to
        /// point at one (`docs/character.md`, "Accountability is a chain").
        #[arg(long = "accountable-to")]
        accountable_to: Option<String>,
    },
    /// List the live (non-retired) characters.
    #[command(alias = "ls")]
    List {
        /// Include retired characters too.
        #[arg(long)]
        all: bool,
        /// Emit each row as a JSON object (name, id, timestamps,
        /// accountable_to) instead of the default array of ids.
        #[arg(long)]
        json: bool,
    },
    /// Show one character's sheet.
    Show {
        /// The character's name.
        name: String,
    },
    /// Update a character's sheet. Currently only `accountable_to`.
    Set {
        /// The character to update.
        name: String,
        /// Point this character's accountability chain at another
        /// character. Refused on self, a cycle, an unknown name, or a
        /// retired target.
        #[arg(long = "accountable-to", conflicts_with = "root")]
        accountable_to: Option<String>,
        /// Clear `accountable_to` — this character becomes a root.
        #[arg(long, conflicts_with = "accountable_to")]
        root: bool,
    },
    /// Retire a character: stamp `retired_at`, and conclude + archive every
    /// live context it plays, in the same act. There is no reassignment and
    /// no verb to move a context to another character — a retired
    /// character's contexts are archived, not orphaned. Refuses while a
    /// live character is still accountable to this one.
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
            CharacterCommand::Create { name, accountable_to } => {
                self.character_create(&name, accountable_to.as_deref())
            }
            CharacterCommand::List { all, json } => self.character_list(all, json),
            CharacterCommand::Show { name } => self.character_show(&name),
            CharacterCommand::Set { name, accountable_to, root } => {
                self.character_set(&name, accountable_to.as_deref(), root)
            }
            CharacterCommand::Retire { name } => self.character_retire(&name),
        }
    }

    /// Idempotent on `name`: an existing row is returned unchanged rather
    /// than minting a second principal for the same name — the double-mint
    /// hazard `add-key` avoids by binding instead of minting
    /// (`docs/character.md`, "Adding a key binds; it never mints") applies
    /// just as much to a repeated `create`.
    fn character_create(&self, name: &str, accountable_to: Option<&str>) -> KjResult {
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

        // Resolve the accountable-to name before minting anything, so an
        // unknown or retired target never leaves an orphan character behind.
        // `update_character_accountable_to` checks liveness again under its
        // own write; by then the row would already exist.
        let accountable_to_id = match accountable_to {
            Some(target_name) => match db.get_character_by_name(target_name) {
                Ok(Some(t)) if t.retired_at.is_some() => {
                    return KjResult::Err(format!(
                        "kj character create: {target_name} is retired and cannot be an \
                         accountable-to target"
                    ));
                }
                Ok(Some(t)) => Some(t.principal_id),
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj character create: no character named '{target_name}' — \
                         `kj character list` to see who exists"
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj character create: {e}")),
            },
            None => None,
        };

        let row = CharacterRow {
            principal_id: PrincipalId::new(),
            name: name.to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
            handoff_ctx: None,
            accountable_to: None,
        };
        if let Err(e) = db.insert_character(&row) {
            return KjResult::Err(format!("kj character create: {e}"));
        }
        // Set through the validated path even though a brand-new principal
        // can never close a cycle — it is the one place liveness is
        // checked, and a fresh character is no exception to that rule.
        if let Some(target_id) = accountable_to_id
            && let Err(e) = db.update_character_accountable_to(row.principal_id, Some(target_id))
        {
            return KjResult::Err(format!("kj character create: {e}"));
        }
        let row = match db.get_character_by_name(name) {
            Ok(Some(r)) => r,
            Ok(None) | Err(_) => row,
        };
        KjResult::ok_with_data(
            format!("created {name} ({})", row.principal_id.short()),
            character_to_json(&row),
        )
    }

    /// `list` → an array of full-id strings by default
    /// (`project_kj_structured_data` convention): a caller iterating the
    /// result never has to re-derive a truncated display id. `--json` opts
    /// into the full row per entry, `accountable_to` included, for a caller
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
                let accountable_to_name = match row.accountable_to {
                    Some(pid) => db.name_for(pid),
                    None => "(root)".to_string(),
                };
                let text = format!(
                    "Character:       {}\nID:              {}\nAccountable to:  {}\n\
                     Created:         {}\nRetired:         {}",
                    row.name,
                    row.principal_id.to_hex(),
                    accountable_to_name,
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

    /// Update `accountable_to`: point it at another character, or clear it
    /// to a root with `--root`. All the validation (self, cycle, missing,
    /// retired) lives in `KernelDb::update_character_accountable_to`; this
    /// handler only resolves names to ids and relays whatever it says.
    fn character_set(&self, name: &str, accountable_to: Option<&str>, root: bool) -> KjResult {
        if accountable_to.is_none() && !root {
            return KjResult::Err(
                "kj character set: specify --accountable-to <character> or --root".to_string(),
            );
        }
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
        let target = if root {
            None
        } else {
            let target_name = accountable_to.expect("checked above");
            match db.get_character_by_name(target_name) {
                Ok(Some(t)) => Some(t.principal_id),
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj character set: no character named '{target_name}' — \
                         `kj character list` to see who exists"
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj character set: {e}")),
            }
        };
        if let Err(e) = db.update_character_accountable_to(row.principal_id, target) {
            return KjResult::Err(format!("kj character set: {e}"));
        }
        let updated = match db.get_character_by_name(name) {
            Ok(Some(r)) => r,
            Ok(None) | Err(_) => row,
        };
        let msg = match (target, accountable_to) {
            (Some(_), Some(target_name)) => format!("{name} is now accountable to {target_name}"),
            _ => format!("{name} is now a root"),
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
        // A live dependent refuses the whole retire, before any context is
        // concluded or archived: `conclude_context` and `archive_context`
        // each commit on their own, so a refusal after the loop would leave
        // the contexts archived and the character live.
        match db.live_accountable_dependents(row.principal_id) {
            Ok(dependents) if dependents.is_empty() => {}
            Ok(dependents) => {
                return KjResult::Err(format!(
                    "kj character retire: cannot retire {name}: {} character(s) are still \
                     accountable to it ({}) — re-root or retire them first",
                    dependents.len(),
                    dependents.join(", ")
                ));
            }
            Err(e) => return KjResult::Err(format!("kj character retire: {e}")),
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
        "accountable_to": row.accountable_to.map(|p| p.to_hex()),
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

    /// `create --accountable-to` resolves the name and sets the chain in
    /// the same act.
    #[tokio::test]
    async fn create_with_accountable_to_sets_the_chain() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("root-char")], &caller).await;
        let result = d
            .dispatch(
                &[s("character"), s("create"), s("dep-char"), s("--accountable-to"), s("root-char")],
                &caller,
            )
            .await;
        let KjResult::Ok { data: Some(serde_json::Value::Object(obj)), .. } = result else {
            panic!("expected Ok with an object, got {result:?}");
        };
        let root_id = d
            .kernel_db()
            .lock()
            .get_character_by_name("root-char")
            .unwrap()
            .unwrap()
            .principal_id
            .to_hex();
        assert_eq!(obj["accountable_to"], root_id);
    }

    /// An unknown `--accountable-to` target fails loudly and leaves no
    /// half-formed character behind.
    #[tokio::test]
    async fn create_with_unknown_accountable_to_fails_loudly() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let result = d
            .dispatch(
                &[s("character"), s("create"), s("orphan"), s("--accountable-to"), s("nobody")],
                &caller,
            )
            .await;
        assert!(matches!(result, KjResult::Err(_)), "expected Err, got {result:?}");
        assert!(
            d.kernel_db().lock().get_character_by_name("orphan").unwrap().is_none(),
            "an unresolvable target must not leave a half-formed character behind"
        );
    }

    /// `set --accountable-to` and `set --root` round-trip through the
    /// sheet.
    #[tokio::test]
    async fn set_accountable_to_and_root_round_trip() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("lead")], &caller).await;
        d.dispatch(&[s("character"), s("create"), s("worker")], &caller).await;

        let set = d
            .dispatch(&[s("character"), s("set"), s("worker"), s("--accountable-to"), s("lead")], &caller)
            .await;
        assert!(matches!(set, KjResult::Ok { .. }), "{set:?}");
        let lead_id = d.kernel_db().lock().get_character_by_name("lead").unwrap().unwrap().principal_id;
        let worker = d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap();
        assert_eq!(worker.accountable_to, Some(lead_id));

        let cleared = d.dispatch(&[s("character"), s("set"), s("worker"), s("--root")], &caller).await;
        assert!(matches!(cleared, KjResult::Ok { .. }), "{cleared:?}");
        let worker = d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap();
        assert_eq!(worker.accountable_to, None, "--root must clear accountable_to");
    }

    /// `set` with neither `--accountable-to` nor `--root` is a usage error,
    /// not a silent no-op.
    #[tokio::test]
    async fn set_requires_accountable_to_or_root() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("solo")], &caller).await;
        let result = d.dispatch(&[s("character"), s("set"), s("solo")], &caller).await;
        assert!(matches!(result, KjResult::Err(_)), "expected Err, got {result:?}");
    }

    /// A self target and a would-be cycle both refuse through `set`, the
    /// same way `update_character_accountable_to` refuses them at the DB
    /// layer.
    #[tokio::test]
    async fn set_rejects_self_and_cycle() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("a")], &caller).await;
        d.dispatch(&[s("character"), s("create"), s("b")], &caller).await;

        let self_result =
            d.dispatch(&[s("character"), s("set"), s("a"), s("--accountable-to"), s("a")], &caller).await;
        assert!(matches!(self_result, KjResult::Err(_)), "expected Err, got {self_result:?}");

        let b_to_a =
            d.dispatch(&[s("character"), s("set"), s("b"), s("--accountable-to"), s("a")], &caller).await;
        assert!(matches!(b_to_a, KjResult::Ok { .. }), "{b_to_a:?}");
        let cycle_result =
            d.dispatch(&[s("character"), s("set"), s("a"), s("--accountable-to"), s("b")], &caller).await;
        assert!(matches!(cycle_result, KjResult::Err(_)), "expected Err, got {cycle_result:?}");
    }

    /// `show`'s text form prints the accountable-to character's NAME, not
    /// its bare id.
    #[tokio::test]
    async fn show_text_prints_accountable_to_name() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("boss")], &caller).await;
        d.dispatch(
            &[s("character"), s("create"), s("staff"), s("--accountable-to"), s("boss")],
            &caller,
        )
        .await;
        let result = d.dispatch(&[s("character"), s("show"), s("staff")], &caller).await;
        let KjResult::Ok { message, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert!(message.contains("boss"), "{message}");
    }

    /// `show` on a root prints `(root)` rather than a blank line.
    #[tokio::test]
    async fn show_text_prints_root_for_no_accountable_to() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("standalone")], &caller).await;
        let result = d.dispatch(&[s("character"), s("show"), s("standalone")], &caller).await;
        let KjResult::Ok { message, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert!(message.contains("(root)"), "{message}");
    }

    /// `list --json` emits each row as an object carrying `accountable_to`
    /// as a full id string (or null for a root) — the default (no flag)
    /// array-of-ids form is unaffected.
    #[tokio::test]
    async fn list_json_includes_accountable_to() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("boss2")], &caller).await;
        d.dispatch(
            &[s("character"), s("create"), s("staff2"), s("--accountable-to"), s("boss2")],
            &caller,
        )
        .await;
        let result = d.dispatch(&[s("character"), s("list"), s("--json")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Array(rows)), .. } = result else {
            panic!("expected a JSON array, got {result:?}");
        };
        let staff_row = rows.iter().find(|r| r["name"] == "staff2").expect("staff2 present");
        assert!(staff_row["accountable_to"].is_string(), "{staff_row:?}");
        let boss_row = rows.iter().find(|r| r["name"] == "boss2").expect("boss2 present");
        assert!(boss_row["accountable_to"].is_null(), "a root's accountable_to must be null, {boss_row:?}");
    }

    /// `retire` refuses while a live character is still accountable to it.
    #[tokio::test]
    async fn retire_refuses_with_a_live_dependent() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("mentor")], &caller).await;
        d.dispatch(
            &[s("character"), s("create"), s("mentee"), s("--accountable-to"), s("mentor")],
            &caller,
        )
        .await;

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result = d.dispatch(&[s("character"), s("retire"), s("mentor")], &confirmed).await;
        assert!(matches!(result, KjResult::Err(_)), "expected Err, got {result:?}");
        let mentor = d.kernel_db().lock().get_character_by_name("mentor").unwrap().unwrap();
        assert!(mentor.retired_at.is_none(), "a refused retire must not have stamped retired_at");
    }

    /// A refused retire leaves the character's live contexts alone. The
    /// dependent check runs before any context is concluded or archived,
    /// so a character with both a live dependent and a live context comes
    /// out of a refusal exactly as it went in.
    #[tokio::test]
    async fn retire_refused_by_a_dependent_archives_nothing() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("mentor")], &caller).await;
        d.dispatch(
            &[s("character"), s("create"), s("mentee"), s("--accountable-to"), s("mentor")],
            &caller,
        )
        .await;
        let mentor_id = {
            let db = d.kernel_db().lock();
            db.get_character_by_name("mentor").unwrap().unwrap().principal_id
        };
        let ctx = register_context(&d, Some("mentor-ctx"), None, mentor_id);
        d.kernel_db().lock().update_played_by(ctx, Some(mentor_id)).unwrap();

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result = d.dispatch(&[s("character"), s("retire"), s("mentor")], &confirmed).await;
        assert!(matches!(result, KjResult::Err(_)), "expected Err, got {result:?}");

        let ctx_row = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert!(ctx_row.archived_at.is_none(), "a refused retire must not archive the context");
        assert!(ctx_row.concluded_at.is_none(), "nor conclude it");
        let mentor = d.kernel_db().lock().get_character_by_name("mentor").unwrap().unwrap();
        assert!(mentor.retired_at.is_none());
    }

    /// A retired `--accountable-to` target refuses before minting, the same
    /// as an unknown one: no root character is left behind for a retry to
    /// find "already exists".
    #[tokio::test]
    async fn create_with_retired_accountable_to_leaves_no_character() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("elder")], &caller).await;
        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let retired = d.dispatch(&[s("character"), s("retire"), s("elder")], &confirmed).await;
        assert!(matches!(retired, KjResult::Ok { .. }), "{retired:?}");

        let result = d
            .dispatch(
                &[s("character"), s("create"), s("orphan"), s("--accountable-to"), s("elder")],
                &caller,
            )
            .await;
        assert!(matches!(result, KjResult::Err(_)), "expected Err, got {result:?}");
        assert!(
            d.kernel_db().lock().get_character_by_name("orphan").unwrap().is_none(),
            "a retired target must not leave a root character behind"
        );
    }
}

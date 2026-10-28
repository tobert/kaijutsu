//! `kj senpai show|set|report` and `kj kohai create|list`: the senpai edge
//! (`docs/senpai-kohai.md`). A context's senpai is the context it answers to;
//! its kohai are the live contexts that answer to it.

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId};

use crate::kernel_db::{ContextCharacterRow, ContextRow, GovernedAct, KernelDb};

use super::context::ContextConfigArgs;
use super::effect::{Classify, Effect};
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "senpai",
    about = "Show or move the context a context answers to",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct SenpaiArgs {
    #[command(subcommand)]
    command: SenpaiCommand,
}

#[derive(Subcommand, Debug)]
enum SenpaiCommand {
    /// Show a context's senpai, the lineage root, and who reviews an ask its
    /// character raises (default: the current context).
    Show {
        /// A context. Defaults to the current context. Ids and labels come
        /// from `kj context list`.
        context: Option<String>,
    },
    /// Move a context under another senpai. The character of its senpai, or
    /// its lineage root, may move it; the caller must also be the new
    /// senpai's character or its lineage root. Refuses while the context has
    /// pending asks, a move that would close a cycle, an archived senpai, and
    /// a senpai whose character is retired.
    Set {
        /// The context to move. Ids and labels come from `kj context list`.
        context: String,
        /// The context it answers to from now on.
        senpai: String,
    },
    /// Report to this context's senpai: the text lands in the senpai context
    /// as a drift block from the current context, as `kj drift push` would
    /// put it there, with no context id to type. A drift along the senpai
    /// edge needs no ask. Refuses a context with no senpai, an archived
    /// senpai, and empty text.
    Report {
        /// The report (joined with spaces).
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
}

#[derive(Parser, Debug)]
#[command(
    name = "kohai",
    about = "Start and list the contexts that answer to a context",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct KohaiArgs {
    #[command(subcommand)]
    command: KohaiCommand,
}

#[derive(Subcommand, Debug)]
enum KohaiCommand {
    /// Make a new context performed by a character, that answers to this
    /// context. Label is positional or `--name`. The new context runs the
    /// model in the character's cast slot for its type, copied now; the
    /// output says when the cast has none. Needs the operator capability and
    /// a live character caller.
    #[command(alias = "new")]
    Create {
        /// Label (positional form; or use --name)
        label: Option<String>,
        /// Label (flag form; takes precedence over the positional label)
        #[arg(long, short = 'n')]
        name: Option<String>,
        /// Context the new one is created from, for provenance only.
        /// Defaults to the current context
        #[arg(long, short = 'p')]
        parent: Option<String>,
        /// The context the new one answers to. Defaults to the current
        /// context; another context needs the caller to be its character or
        /// its lineage root
        #[arg(long)]
        senpai: Option<String>,
        /// The live, non-root character that performs the new context. Its
        /// senpai's character reviews its asks
        #[arg(long = "as", value_name = "CHARACTER", required = true)]
        character: String,
        #[command(flatten)]
        config: Box<ContextConfigArgs>,
    },
    /// List the live kohai of a context, each with its character and type
    /// (default: the current context).
    #[command(alias = "ls")]
    List {
        /// A context. Defaults to the current context.
        context: Option<String>,
    },
}

impl Classify for SenpaiArgs {
    fn effect(&self) -> Effect {
        match self.command {
            SenpaiCommand::Show { .. } => Effect::Read,
            SenpaiCommand::Set { .. } | SenpaiCommand::Report { .. } => Effect::Write,
        }
    }
}

impl Classify for KohaiArgs {
    fn effect(&self) -> Effect {
        match self.command {
            KohaiCommand::Create { .. } => Effect::Write,
            KohaiCommand::List { .. } => Effect::Read,
        }
    }
}

/// Parse `argv` for one of the two verbs, rendering help as help.
fn parse<A: Parser + clap::CommandFactory>(argv: &[String], verb: &str) -> Result<A, KjResult> {
    if argv.is_empty() {
        return Err(clap_help_for::<A>());
    }
    A::try_parse_from(argv).map_err(|e| {
        if matches!(
            e.kind(),
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ) {
            KjResult::ok_ephemeral(e.to_string(), ContentType::Plain)
        } else {
            KjResult::Err(format!("kj {verb}: {e}"))
        }
    })
}

/// The context a verb argument names, with its context character.
fn resolve_character_arg(
    arg: Option<&str>,
    caller: &KjCaller,
    db: &KernelDb,
) -> Result<(ContextId, ContextCharacterRow), String> {
    let context = super::refs::resolve_context_arg(arg, caller, db)?;
    let character = db.context_character(context).map_err(|e| e.to_string())?.ok_or_else(|| {
        format!("{} has no context character, so it has no senpai and no kohai", db.context_name(context))
    })?;
    Ok((context, character))
}

/// `label (short-id)` of the context, and its character.
fn context_line(db: &KernelDb, character: &ContextCharacterRow) -> String {
    format!("{}, {}", db.context_name(character.context_id), db.name_for(character.principal_id))
}

fn context_json(db: &KernelDb, character: &ContextCharacterRow) -> serde_json::Value {
    let label = db.get_context(character.context_id).ok().flatten().and_then(|row| row.label);
    serde_json::json!({
        "context": character.context_id.to_hex(),
        "label": label,
        "character": db.name_for(character.principal_id),
        "character_id": character.principal_id.to_hex(),
    })
}

impl KjDispatcher {
    pub(crate) async fn dispatch_senpai(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let parsed = match parse::<SenpaiArgs>(argv, "senpai") {
            Ok(parsed) => parsed,
            Err(result) => return result,
        };
        match parsed.command {
            SenpaiCommand::Show { context } => self.senpai_show(context.as_deref(), caller).await,
            SenpaiCommand::Set { context, senpai } => {
                if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Operator, "senpai set") {
                    return denied;
                }
                self.senpai_set(&context, &senpai, caller)
            }
            SenpaiCommand::Report { text } => {
                if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Drift, "senpai report") {
                    return denied;
                }
                self.senpai_report(&text, caller).await
            }
        }
    }

    pub(crate) async fn dispatch_kohai(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let parsed = match parse::<KohaiArgs>(argv, "kohai") {
            Ok(parsed) => parsed,
            Err(result) => return result,
        };
        match parsed.command {
            KohaiCommand::Create { label, name, parent, senpai, character, config } => {
                if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Operator, "kohai create") {
                    return denied;
                }
                Box::pin(self.kohai_create(
                    name.or(label).as_deref(),
                    parent.as_deref(),
                    senpai.as_deref(),
                    &character,
                    *config,
                    caller,
                ))
                .await
            }
            KohaiCommand::List { context } => self.kohai_list(context.as_deref(), caller),
        }
    }

    /// `kj senpai show [<ctx>]`.
    async fn senpai_show(&self, context: Option<&str>, caller: &KjCaller) -> KjResult {
        let (context, row, senpai, root, note) = {
            let db = self.kernel_db().lock();
            let shown = (|| -> Result<_, String> {
                let (context, row) = resolve_character_arg(context, caller, &db)?;
                let senpai = match row.senpai_ctx {
                    Some(id) => Some(db.context_character(id).map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("senpai {} has no context character", db.context_name(id)))?),
                    None => None,
                };
                let root = db.lineage_root(context).map_err(|e| e.to_string());
                let note = db.senpai_migration_report().map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|(c, _, _)| *c == context)
                    .map(|(_, _, note)| note);
                Ok((context, row, senpai, root, note))
            })();
            match shown {
                Ok(shown) => shown,
                Err(e) => return KjResult::Err(format!("kj senpai show: {e}")),
            }
        };
        let name = |principal| self.kernel_db().lock().name_for(principal);
        let mut lines = vec![format!("context: {}", context_line(&self.kernel_db().lock(), &row))];
        lines.push(match &senpai {
            Some(senpai) => format!("senpai: {}", context_line(&self.kernel_db().lock(), senpai)),
            None => "senpai: none (nothing above this context)".to_string(),
        });
        lines.push(match &root {
            Ok(root) => format!("lineage root: {}", name(*root)),
            Err(reason) => format!("lineage root: none ({reason})"),
        });
        let character = name(row.principal_id);
        let review = self.kernel().resolve_context_review(context).await;
        lines.push(match &review {
            Ok(review) => format!("an ask by {character} goes to: {} ({})", review.reviewer.name, review.source.as_str()),
            Err(error) => format!("an ask by {character} goes to nobody: {error}"),
        });
        if let Some(note) = &note {
            lines.push(format!("note: {note}"));
        }
        let (context_data, senpai_data) = {
            let db = self.kernel_db().lock();
            (context_json(&db, &row), senpai.as_ref().map(|s| context_json(&db, s)))
        };
        let data = serde_json::json!({
            "context": context_data,
            "senpai": senpai_data,
            "lineage_root": root.as_ref().ok().map(|r| serde_json::json!({ "id": r.to_hex(), "name": name(*r) })),
            "lineage_root_error": root.as_ref().err(),
            "reviewer": review.as_ref().ok().map(|r| serde_json::json!({
                "id": r.reviewer.principal_id.to_hex(), "name": r.reviewer.name, "source": r.source.as_str(),
            })),
            "reviewer_error": review.as_ref().err().map(|e| e.to_string()),
            "migration_note": note,
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// `kj senpai set <ctx> <senpai-ctx>`: every check and the write run under
    /// one lock and one transaction.
    fn senpai_set(&self, context: &str, senpai: &str, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        let moved = (|| -> Result<(ContextCharacterRow, ContextCharacterRow), String> {
            let (target, _) = resolve_character_arg(Some(context), caller, &db)?;
            let (senpai, _) = resolve_character_arg(Some(senpai), caller, &db)?;
            db.in_transaction(|db| Ok(move_kohai(db, target, senpai, caller)))
                .map_err(|e| e.to_string())?
        })();
        match moved {
            Ok((row, senpai)) => KjResult::ok_with_data(
                format!("{} now answers to {}", context_line(&db, &row), context_line(&db, &senpai)),
                serde_json::json!({ "context": context_json(&db, &row), "senpai": context_json(&db, &senpai) }),
            ),
            Err(e) => KjResult::Err(format!("kj senpai set: {e}")),
        }
    }

    /// `kj senpai report <text>...`: `kj drift push` to the caller's senpai,
    /// by id, so the report can only reach the context this one answers to.
    async fn senpai_report(&self, text: &[String], caller: &KjCaller) -> KjResult {
        let here = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };
        let senpai = {
            let db = self.kernel_db().lock();
            let found = (|| -> Result<ContextCharacterRow, String> {
                let (_, row) = resolve_character_arg(Some(&here.to_hex()), caller, &db)?;
                let senpai_id = row.senpai_ctx.ok_or_else(|| {
                    format!("{} has no senpai, so there is nobody to report to", db.context_name(row.context_id))
                })?;
                let senpai = db.context_character(senpai_id).map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("senpai {} has no context character", db.context_name(senpai_id)))?;
                let archived = db.get_context(senpai_id).map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("senpai {} has no row", senpai_id.short()))?
                    .is_archived();
                if archived {
                    return Err(format!(
                        "{} is archived; `kj context promote` it to reach it again, or move this context with `kj senpai set`",
                        db.context_name(senpai_id)
                    ));
                }
                Ok(senpai)
            })();
            match found {
                Ok(found) => found,
                Err(e) => return KjResult::Err(format!("kj senpai report: {e}")),
            }
        };
        let pushed = self.drift_push(&senpai.context_id.to_hex(), false, false, text, caller).await;
        match pushed {
            KjResult::Ok { .. } => {
                let db = self.kernel_db().lock();
                KjResult::ok_with_data(
                    format!("reported → {}", context_line(&db, &senpai)),
                    serde_json::json!({ "senpai": context_json(&db, &senpai) }),
                )
            }
            KjResult::Err(e) => KjResult::Err(e.replacen("kj drift push", "kj senpai report", 1)),
            other => other,
        }
    }

    /// `kj kohai list [<ctx>]`.
    fn kohai_list(&self, context: Option<&str>, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        let listed = (|| -> Result<(ContextCharacterRow, Vec<ContextRow>), String> {
            let (context, row) = resolve_character_arg(context, caller, &db)?;
            Ok((row, db.list_kohai(context).map_err(|e| e.to_string())?))
        })();
        let (row, kohai) = match listed {
            Ok(listed) => listed,
            Err(e) => return KjResult::Err(format!("kj kohai list: {e}")),
        };
        let character = |k: &ContextRow| k.played_by.map(|p| db.name_for(p));
        let mut lines = Vec::with_capacity(kohai.len() + 1);
        if kohai.is_empty() {
            lines.push(format!("{} has no live kohai", db.context_name(row.context_id)));
        }
        for k in &kohai {
            lines.push(format!(
                "{}  {}  {}",
                db.context_name(k.context_id), character(k).unwrap_or_else(|| "-".to_string()), k.context_type
            ));
        }
        let data = serde_json::json!({
            "context": context_json(&db, &row),
            "kohai": kohai.iter().map(|k| serde_json::json!({
                "context": k.context_id.to_hex(), "label": k.label, "context_type": k.context_type,
                "character": character(k),
            })).collect::<Vec<_>>(),
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }
}

/// The checks and the write behind `kj senpai set`, inside the caller's
/// transaction. The inner `Err` is the refusal.
fn move_kohai(
    db: &KernelDb,
    target: ContextId,
    senpai: ContextId,
    caller: &KjCaller,
) -> Result<(ContextCharacterRow, ContextCharacterRow), String> {
    let row = db.context_character(target).map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{} has no context character", db.context_name(target)))?;
    let senpai_row = db.context_character(senpai).map_err(|e| e.to_string())?.ok_or_else(|| {
        format!(
            "{} has no context character, so nobody would answer for {}",
            db.context_name(senpai), db.context_name(target)
        )
    })?;
    db.context_authority(target, caller.actor_id, GovernedAct::Move).map_err(|e| e.to_string())??;
    db.may_place_under(senpai, caller.actor_id).map_err(|e| e.to_string())??;
    let archived = db.get_context(senpai).map_err(|e| e.to_string())?
        .ok_or_else(|| format!("senpai {} has no row", senpai.short()))?
        .is_archived();
    if archived {
        return Err(format!(
            "{} is archived; a context answers to a live senpai, or `kj context promote` it first",
            db.context_name(senpai)
        ));
    }
    let live = db.get_character(senpai_row.principal_id).map_err(|e| e.to_string())?.is_some_and(|c| c.retired_at.is_none());
    if !live {
        return Err(format!(
            "{} has no live holder: its character is retired, so nobody would answer for {}",
            db.context_name(senpai), db.context_name(target)
        ));
    }
    // An ask raised in the moved context, or in a context below it, may
    // climb through it, so it blocks the move.
    let mut below = vec![target];
    let mut next = 0;
    while next < below.len() {
        for kohai in db.kohai_of(below[next]).map_err(|e| e.to_string())? {
            if !below.contains(&kohai) {
                below.push(kohai);
            }
        }
        next += 1;
    }
    let pending = db.list_pending_asks().map_err(|e| e.to_string())?;
    if pending.iter().any(|ask| below.iter().any(|c| ask.context_id.as_slice() == c.as_bytes())) {
        return Err(format!("settle or cancel the pending asks of {} before moving it", db.context_name(target)));
    }
    db.set_context_senpai(target, Some(senpai)).map_err(|e| e.to_string())?;
    let moved = db.context_character(target).map_err(|e| e.to_string())?.unwrap_or(row);
    Ok((moved, senpai_row))
}

#[cfg(test)]
mod tests {
    use kaijutsu_types::{ContextId, PrincipalId};

    use crate::kj::test_helpers::*;
    use crate::kj::{KjCaller, KjDispatcher, KjResult};

    fn s(v: &str) -> String {
        v.to_string()
    }

    struct House {
        root: ContextId,
        seat: ContextId,
        judge_seat: ContextId,
        lane: ContextId,
        other: ContextId,
        amy: PrincipalId,
        banto: PrincipalId,
        coder: PrincipalId,
        sibling: PrincipalId,
    }

    /// amy's console → banto's seat → a lane (coder) and another lane
    /// (sibling); judge's seat under amy's console too.
    fn house(d: &KjDispatcher) -> House {
        let amy = test_reviewer_principal();
        let [banto, coder, sibling, judge] = [PrincipalId::new(), PrincipalId::new(), PrincipalId::new(), PrincipalId::new()];
        for (principal_id, name) in [(banto, "banto"), (coder, "coder"), (sibling, "sibling"), (judge, "judge")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        let root = register_root_context(d);
        let seat = register_context(d, Some("banto-seat"), Some(root), amy);
        let judge_seat = register_context(d, Some("judge-seat"), Some(root), amy);
        let lane = register_context(d, Some("lane"), Some(seat), amy);
        let other = register_context(d, Some("other-lane"), Some(seat), amy);
        let db = d.kernel_db().lock();
        for (ctx, who) in [(seat, banto), (judge_seat, judge), (lane, coder), (other, sibling)] {
            db.update_played_by(ctx, Some(who)).unwrap();
        }
        House { root, seat, judge_seat, lane, other, amy, banto, coder, sibling }
    }

    fn caller(actor: PrincipalId, context: ContextId) -> KjCaller {
        KjCaller { principal_id: actor, actor_id: actor, reviewer_id: None, context_id: Some(context), ..test_caller() }
    }

    fn data(result: &KjResult) -> serde_json::Value {
        match result {
            KjResult::Ok { data: Some(data), .. } => data.clone(),
            other => panic!("expected data: {other:?}"),
        }
    }

    #[tokio::test]
    async fn senpai_show_names_the_senpai_its_holder_the_root_and_the_reviewer() {
        let d = test_dispatcher().await;
        let h = house(&d);
        let shown = d.dispatch(&[s("senpai"), s("show")], &caller(h.coder, h.lane)).await;
        assert!(shown.is_ok(), "{}", shown.message());
        let text = shown.message();
        assert!(text.contains(&format!("context: lane ({}), coder", h.lane.short())), "{text}");
        assert!(text.contains(&format!("senpai: banto-seat ({}), banto", h.seat.short())), "{text}");
        assert!(text.contains("lineage root: amy"), "{text}");
        assert!(text.contains("an ask by coder goes to: banto (senpai)"), "{text}");
        let data = data(&shown);
        assert_eq!(data["senpai"]["context"], h.seat.to_hex());
        assert_eq!(data["senpai"]["label"], "banto-seat");
        assert_eq!(data["senpai"]["character"], "banto");
        assert_eq!(data["context"]["character"], "coder");
        assert_eq!(data["reviewer"]["name"], "banto");

        let top = d.dispatch(&[s("senpai"), s("show"), h.root.to_hex()], &caller(h.amy, h.root)).await;
        assert!(top.message().contains("senpai: none"), "{}", top.message());
        assert!(top.message().contains("an ask by amy goes to: amy (self_confirmation)"), "{}", top.message());
        assert!(top.message().contains("goes to: amy (self_confirmation)"), "{}", top.message());
    }

    /// The holder of the lane's senpai (banto) may move it under a context
    /// banto holds, but not under one it does not; the lineage root (amy)
    /// may move it anywhere in its lineage. The move reroutes the asks.
    #[tokio::test]
    async fn senpai_set_moves_a_kohai_under_the_rule_and_reroutes_its_asks() {
        let d = test_dispatcher().await;
        let h = house(&d);
        let lane = h.lane.to_hex();
        let refused = d.dispatch(&[s("senpai"), s("set"), lane.clone(), h.judge_seat.to_hex()], &caller(h.banto, h.seat)).await;
        assert!(!refused.is_ok());
        assert!(refused.message().contains(&format!(
            "only the holder of judge-seat ({}) (judge) or its lineage root (amy) may place a context under it", h.judge_seat.short()
        )), "{}", refused.message());
        let moved = d.dispatch(&[s("senpai"), s("set"), lane.clone(), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
        assert!(moved.is_ok(), "{}", moved.message());
        assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.judge_seat));
        assert_eq!(d.kernel().resolve_context_review(h.lane).await.unwrap().reviewer.name, "judge");
        let back = d.dispatch(&[s("senpai"), s("set"), h.other.to_hex(), h.seat.to_hex()], &caller(h.banto, h.seat)).await;
        assert!(back.is_ok(), "banto holds other-lane's senpai and the target seat: {}", back.message());
    }

    /// `kj senpai set` refuses the kohai itself and a sibling (the rule), a
    /// cycle, a senpai with no live holder, and a context with pending asks.
    #[tokio::test]
    async fn senpai_set_refuses_the_rule_a_cycle_a_holderless_senpai_and_pending_asks() {
        let d = test_dispatcher().await;
        let h = house(&d);
        let set = |ctx: ContextId, senpai: ContextId| vec![s("senpai"), s("set"), ctx.to_hex(), senpai.to_hex()];
        for refused in [h.coder, h.sibling] {
            let result = d.dispatch(&set(h.lane, h.root), &caller(refused, h.lane)).await;
            assert!(!result.is_ok());
            assert!(result.message().contains(&format!(
                "only lane ({})'s senpai (banto) or its lineage root (amy) may move it to another senpai", h.lane.short()
            )), "{}", result.message());
        }
        let cycle = d.dispatch(&set(h.seat, h.lane), &caller(h.amy, h.root)).await;
        assert!(!cycle.is_ok() && cycle.message().contains("cycle"), "{}", cycle.message());

        // A context with no context character has nobody to answer for the kohai.
        let empty = register_context(&d, Some("nobody-holds"), Some(h.root), h.amy);
        d.kernel_db().lock().conn_for_ledger().execute(
            "DELETE FROM context_characters WHERE context_id = ?1",
            rusqlite::params![empty.as_bytes().as_slice()],
        ).unwrap();
        let holderless = d.dispatch(&set(h.lane, empty), &caller(h.amy, h.root)).await;
        assert!(
            !holderless.is_ok() && holderless.message().contains("has no context character"),
            "{}", holderless.message()
        );

        let gate_caller = KjCaller { reviewer_id: Some(h.banto), ..caller(h.coder, h.lane) };
        let spec = crate::kj::gate::GateSpec { publishes_pair: false, tool_call: None, origin: approval_ledger::types::Origin::Hook, instance: "test".into(), tool: "test".into(), hook_id: None, description: "pending".into(), authorized_label: "pending".into(), statements: vec![crate::kj::gate::GatedStatement { rendered: "pending".into(), statement_kind: "test".into(), vars: vec![], source_index: None }], exec_source: None, exec_stdin: None, planned: vec![] };
        let outcome = crate::kj::gate::run_gate(d.kernel(), &gate_caller, spec, d.kernel().ledger_flows(), &crate::kj::gate_policy::no_config()).await;
        assert!(outcome.ask.is_some());
        let pending = d.dispatch(&set(h.lane, h.judge_seat), &caller(h.amy, h.root)).await;
        assert!(!pending.is_ok() && pending.message().contains("pending asks"), "{}", pending.message());
        assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.seat), "every refusal changed nothing");
    }

    /// Setting the senpai again clears the migration's note, which `kj
    /// senpai show` prints until then.
    #[tokio::test]
    async fn senpai_show_prints_the_migration_note_until_the_senpai_is_set_again() {
        let d = test_dispatcher().await;
        let h = house(&d);
        d.kernel_db().lock().conn_for_ledger().execute(
            "INSERT INTO senpai_migration_report (context_id, senpai_ctx, note, old_reviewer, recorded_at) VALUES (?1, ?2, 'mapped by hand; set its senpai again', NULL, 1)",
            rusqlite::params![h.lane.as_bytes().as_slice(), h.seat.as_bytes().as_slice()],
        ).unwrap();
        let shown = d.dispatch(&[s("senpai"), s("show"), h.lane.to_hex()], &caller(h.amy, h.root)).await;
        assert!(shown.message().contains("note: mapped by hand"), "{}", shown.message());
        let set = d.dispatch(&[s("senpai"), s("set"), h.lane.to_hex(), h.seat.to_hex()], &caller(h.amy, h.root)).await;
        assert!(set.is_ok(), "{}", set.message());
        let shown = d.dispatch(&[s("senpai"), s("show"), h.lane.to_hex()], &caller(h.amy, h.root)).await;
        assert!(!shown.message().contains("note:"), "{}", shown.message());
    }

    #[tokio::test]
    async fn kohai_list_lists_the_live_kohai_with_type_and_performer() {
        let d = test_dispatcher().await;
        let h = house(&d);
        d.kernel_db().lock().update_context_type(h.other, "coder").unwrap();
        let gone = register_context(&d, Some("gone-lane"), Some(h.seat), h.amy);
        d.kernel_db().lock().archive_context(gone).unwrap();
        let listed = d.dispatch(&[s("kohai"), s("list")], &caller(h.banto, h.seat)).await;
        assert!(listed.is_ok(), "{}", listed.message());
        let text = listed.message();
        assert!(text.contains(&format!("lane ({})  coder  default", h.lane.short())), "{text}");
        assert!(text.contains(&format!("other-lane ({})  sibling  coder", h.other.short())), "{text}");
        assert!(!text.contains("gone-lane"), "an archived kohai is not listed: {text}");
        let data = data(&listed);
        assert_eq!(data["kohai"].as_array().unwrap().len(), 2);
        assert_eq!(data["kohai"][0]["character"], "coder");
        let none = d.dispatch(&[s("kohai"), s("list"), h.lane.to_hex()], &caller(h.banto, h.seat)).await;
        assert!(none.message().contains("has no live kohai"), "{}", none.message());
    }

    /// `kj kohai create` makes a new context performed by the named
    /// character. Its senpai is the caller's context, and its slots copy the
    /// character's cast.
    #[tokio::test]
    async fn kohai_create_makes_a_context_character_under_the_callers_context() {
        let d = test_dispatcher().await;
        let h = house(&d);
        let admin = test_caller();
        assert!(d.dispatch(&[s("cast"), s("create"), s("band")], &admin).await.is_ok());
        let set = d.dispatch(&[s("character"), s("set"), s("coder"), s("--cast"), s("band")], &admin).await;
        assert!(set.is_ok(), "{}", set.message());

        let made = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("coder"), s("fresh")], &caller(h.banto, h.seat)).await;
        assert!(made.is_ok(), "{}", made.message());

        let db = d.kernel_db().lock();
        let fresh = db.find_context_by_label("fresh").unwrap().unwrap();
        let character = db.context_character(fresh.context_id).unwrap().expect("the new context has a context character");
        assert_eq!(character.principal_id, h.coder);
        assert_eq!(fresh.created_by, h.banto, "the requester made it");
        assert_eq!(character.senpai_ctx, Some(h.seat), "its senpai is the caller's context");
        assert!(db.list_context_slots(fresh.context_id).unwrap().is_empty(), "the cast has no slot to copy");
        assert_eq!(fresh.played_by, Some(h.coder));
        drop(db);
        assert_eq!(d.kernel().resolve_context_review(fresh.context_id).await.unwrap().reviewer.name, "banto");
    }

    /// `--senpai` names any context it is given, and only when the caller's
    /// character is that context's character or its lineage root; `--parent` is provenance only and may differ.
    #[tokio::test]
    async fn kohai_create_senpai_needs_the_caller_to_hold_it_or_be_its_root() {
        let d = test_dispatcher().await;
        let h = house(&d);
        let create = |label: &str, extra: &[&str]| {
            let mut argv = vec![s("kohai"), s("create"), s("--as"), s("kohai-tester"), s(label)];
            argv.extend(extra.iter().map(|a| s(a)));
            argv
        };
        let refused = d.dispatch(&create("x", &["--senpai", "judge-seat"]), &caller(h.banto, h.seat)).await;
        assert!(!refused.is_ok());
        assert!(refused.message().contains("only the holder of judge-seat"), "{}", refused.message());
        assert!(d.kernel_db().lock().find_context_by_label("x").unwrap().is_none(), "a refusal creates nothing");

        let made = d.dispatch(&create("by-root", &["--senpai", "judge-seat", "--parent", "lane"]), &caller(h.amy, h.root)).await;
        assert!(made.is_ok(), "{}", made.message());
        let row = d.kernel_db().lock().find_context_by_label("by-root").unwrap().unwrap();
        assert_eq!(row.forked_from, Some(h.lane), "--parent records provenance");
        {
            let db = d.kernel_db().lock();
            assert_eq!(db.senpai_of(row.context_id).unwrap(), Some(h.judge_seat), "--senpai routes");
        }

        let own = d.dispatch(&create("by-holder", &["--senpai", "banto-seat"]), &caller(h.banto, h.lane)).await;
        assert!(own.is_ok(), "banto holds banto-seat: {}", own.message());
        let row = d.kernel_db().lock().find_context_by_label("by-holder").unwrap().unwrap();
        assert_eq!(row.forked_from, Some(h.lane));
        assert_eq!(d.kernel_db().lock().senpai_of(row.context_id).unwrap(), Some(h.seat));
    }

    /// `kj senpai report` lands the text in the senpai's log as a drift
    /// block from the reporting context, with no context id to type. A
    /// context with no senpai, an archived senpai and an empty report are
    /// refused, and nothing lands.
    #[tokio::test]
    async fn senpai_report_drifts_the_text_to_the_senpai_and_needs_no_id() {
        let d = test_dispatcher().await;
        let h = house(&d);
        for ctx in [h.root, h.seat, h.lane] {
            d.block_store().create_document(ctx, crate::DocumentKind::Conversation, None).unwrap();
        }
        let drifts = |ctx: ContextId| -> Vec<kaijutsu_types::BlockSnapshot> {
            d.block_store().block_snapshots(ctx).unwrap().into_iter()
                .filter(|b| b.kind == kaijutsu_types::BlockKind::Drift).collect()
        };
        let reported = d.dispatch(&[s("senpai"), s("report"), s("the tests pass;"), s("ready for review")], &caller(h.coder, h.lane)).await;
        assert!(reported.is_ok(), "{}", reported.message());
        assert!(reported.message().contains("banto-seat"), "{}", reported.message());
        let landed = drifts(h.seat);
        assert_eq!(landed.len(), 1, "{landed:?}");
        assert!(landed[0].content.contains("the tests pass; ready for review"), "{}", landed[0].content);
        assert_eq!(landed[0].source_context, Some(h.lane), "the report is from the kohai");
        assert!(drifts(h.root).is_empty() && drifts(h.lane).is_empty(), "only the senpai's log gets it");

        let top = d.dispatch(&[s("senpai"), s("report"), s("hello?")], &caller(h.amy, h.root)).await;
        assert!(!top.is_ok());
        assert!(top.message().contains("has no senpai"), "{}", top.message());

        let empty = d.dispatch(&[s("senpai"), s("report")], &caller(h.coder, h.lane)).await;
        assert!(!empty.is_ok(), "{}", empty.message());

        d.kernel_db().lock().archive_context(h.seat).unwrap();
        let archived = d.dispatch(&[s("senpai"), s("report"), s("still there?")], &caller(h.coder, h.lane)).await;
        assert!(!archived.is_ok());
        assert!(archived.message().contains("archived"), "{}", archived.message());
        assert_eq!(drifts(h.seat).len(), 1, "a refused report lands nothing");
    }

    /// A new context answers to its creator, so a caller with no character
    /// sheet, or a retired one, cannot create one.
    #[tokio::test]
    async fn kohai_create_needs_a_live_character_caller() {
        let d = test_dispatcher().await;
        let h = house(&d);
        let nobody = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("orphan")], &caller(PrincipalId::new(), h.root)).await;
        assert!(!nobody.is_ok());
        assert!(nobody.message().contains("no character sheet"), "{}", nobody.message());
        d.kernel_db().lock().retire_character(h.sibling, 1).unwrap();
        let retired = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("orphan")], &caller(h.sibling, h.other)).await;
        assert!(!retired.is_ok() && retired.message().contains("is retired"), "{}", retired.message());
        assert!(d.kernel_db().lock().find_context_by_label("orphan").unwrap().is_none());
    }

    /// A fork is a kohai of the context it forked from: the verbs read the
    /// edge of the context they name, and moving a fork moves that fork only.
    #[tokio::test]
    async fn the_senpai_verbs_read_the_edge_of_the_context_they_name() {
        let d = test_dispatcher().await;
        let h = house(&d);
        for ctx in [h.root, h.seat, h.judge_seat, h.lane] {
            d.block_store().create_document(ctx, crate::DocumentKind::Conversation, None).unwrap();
        }
        let fork = register_context(&d, Some("lane-fork"), Some(h.lane), h.amy);
        d.block_store().create_document(fork, crate::DocumentKind::Conversation, None).unwrap();
        d.kernel_db().lock().copy_context_character(h.lane, fork).unwrap().expect("the lane has a context character");
        let drifts = |ctx: ContextId| -> usize {
            d.block_store().block_snapshots(ctx).unwrap().into_iter()
                .filter(|b| b.kind == kaijutsu_types::BlockKind::Drift).count()
        };

        let shown = d.dispatch(&[s("senpai"), s("show"), s("lane-fork")], &caller(h.coder, h.lane)).await;
        assert!(shown.message().contains(&format!("context: lane-fork ({}), coder", fork.short())), "{}", shown.message());
        assert!(shown.message().contains(&format!("senpai: lane ({}), coder", h.lane.short())), "{}", shown.message());
        let listed = d.dispatch(&[s("kohai"), s("list"), s("lane")], &caller(h.coder, h.lane)).await;
        assert!(listed.message().contains(&format!("lane-fork ({})  coder", fork.short())), "{}", listed.message());
        let none = d.dispatch(&[s("kohai"), s("list"), s("lane-fork")], &caller(h.coder, h.lane)).await;
        assert!(none.message().contains("has no live kohai"), "{}", none.message());

        let reported = d.dispatch(&[s("senpai"), s("report"), s("from the fork")], &caller(h.coder, fork)).await;
        assert!(reported.is_ok(), "{}", reported.message());
        assert_eq!((drifts(h.lane), drifts(h.seat)), (1, 0), "a fork reports to the context it forked from");

        // `set` moves the fork alone.
        let moved = d.dispatch(&[s("senpai"), s("set"), s("lane-fork"), s("judge-seat")], &caller(h.amy, h.root)).await;
        assert!(moved.is_ok(), "{}", moved.message());
        let db = d.kernel_db().lock();
        assert_eq!(db.senpai_of(fork).unwrap(), Some(h.judge_seat));
        assert_eq!(db.senpai_of(h.lane).unwrap(), Some(h.seat), "the lane stays where it was");
    }

    /// Archiving a context archives that context only. A report to it and a
    /// `kj senpai set` onto it refuse, and its kohai stay live. Promoting it
    /// makes both work again.
    #[tokio::test]
    async fn an_archived_senpai_refuses_reports_and_moves_until_promoted() {
        let d = test_dispatcher().await;
        let h = house(&d);
        for ctx in [h.root, h.seat, h.lane, h.other] {
            d.block_store().create_document(ctx, crate::DocumentKind::Conversation, None).unwrap();
        }
        let drifts = |ctx: ContextId| -> usize {
            d.block_store().block_snapshots(ctx).unwrap().into_iter()
                .filter(|b| b.kind == kaijutsu_types::BlockKind::Drift).count()
        };
        let confirmed = KjCaller { confirmed: true, ..caller(h.amy, h.root) };
        let archived = d.dispatch(&[s("context"), s("archive"), s("banto-seat")], &confirmed).await;
        assert!(archived.is_ok(), "{}", archived.message());
        {
            let db = d.kernel_db().lock();
            assert!(db.get_context(h.seat).unwrap().unwrap().is_archived());
            for kohai in [h.lane, h.other, h.root] {
                assert!(!db.get_context(kohai).unwrap().unwrap().is_archived(), "archive named only the seat");
            }
        }

        let report = d.dispatch(&[s("senpai"), s("report"), s("anyone there?")], &caller(h.coder, h.lane)).await;
        assert!(!report.is_ok() && report.message().contains("is archived"), "{}", report.message());
        assert_eq!(drifts(h.seat), 0, "a refused report lands nothing");
        let onto = d.dispatch(&[s("senpai"), s("set"), s("other-lane"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
        assert!(!onto.is_ok() && onto.message().contains("is archived"), "{}", onto.message());
        let kohai = d.dispatch(&[s("kohai"), s("list"), s("lane")], &caller(h.coder, h.lane)).await;
        assert!(kohai.is_ok(), "the kohai's own verbs still work: {}", kohai.message());

        let promoted = d.dispatch(&[s("context"), s("promote"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
        assert!(promoted.is_ok(), "{}", promoted.message());
        let report = d.dispatch(&[s("senpai"), s("report"), s("back again")], &caller(h.coder, h.lane)).await;
        assert!(report.is_ok(), "{}", report.message());
        assert_eq!(drifts(h.seat), 1);
        let onto = d.dispatch(&[s("senpai"), s("set"), s("other-lane"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
        assert!(onto.is_ok(), "{}", onto.message());
    }

    /// The emitted help speaks of contexts, never of instances.
    #[tokio::test]
    async fn senpai_and_kohai_help_speak_of_contexts() {
        let d = test_dispatcher().await;
        for argv in [
            vec![s("senpai"), s("--help")],
            vec![s("senpai"), s("show"), s("--help")],
            vec![s("senpai"), s("set"), s("--help")],
            vec![s("senpai"), s("report"), s("--help")],
            vec![s("kohai"), s("--help")],
            vec![s("kohai"), s("list"), s("--help")],
            vec![s("kohai"), s("create"), s("--help")],
        ] {
            let help = d.dispatch(&argv, &test_caller()).await;
            assert!(help.is_ok(), "{argv:?}: {}", help.message());
            println!("== {argv:?}\n{}", help.message());
            assert!(!help.message().to_lowercase().contains("instance"), "{argv:?}: {}", help.message());
            assert!(!help.message().contains("holder"), "{argv:?}: {}", help.message());
        }
    }

    /// A pending ask raised in a context below the moved one, a fork
    /// included, blocks the move.
    #[tokio::test]
    async fn senpai_set_refuses_while_a_fork_of_the_context_has_a_pending_ask() {
        let d = test_dispatcher().await;
        let h = house(&d);
        let fork = register_context(&d, Some("lane-fork"), Some(h.lane), h.amy);
        d.kernel_db().lock().copy_context_character(h.lane, fork).unwrap().expect("the lane has a context character");
        let gate_caller = KjCaller { reviewer_id: Some(h.banto), ..caller(h.coder, fork) };
        let spec = crate::kj::gate::GateSpec { publishes_pair: false, tool_call: None, origin: approval_ledger::types::Origin::Hook, instance: "test".into(), tool: "test".into(), hook_id: None, description: "pending".into(), authorized_label: "pending".into(), statements: vec![crate::kj::gate::GatedStatement { rendered: "pending".into(), statement_kind: "test".into(), vars: vec![], source_index: None }], exec_source: None, exec_stdin: None, planned: vec![] };
        let outcome = crate::kj::gate::run_gate(d.kernel(), &gate_caller, spec, d.kernel().ledger_flows(), &crate::kj::gate_policy::no_config()).await;
        assert!(outcome.ask.is_some());
        let pending = d.dispatch(&[s("senpai"), s("set"), h.lane.to_hex(), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
        assert!(!pending.is_ok() && pending.message().contains("pending asks of lane"), "{}", pending.message());
    }
}

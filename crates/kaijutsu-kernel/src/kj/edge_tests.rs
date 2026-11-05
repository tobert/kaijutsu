//! Tests for the senpai edge through the general verbs: `kj context
//! info|list --senpai|set --senpai`, `kj drift push .senpai`, the `.senpai`
//! context reference, and `kj context create`'s senpai placement.

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
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow { rc_dir: Some("coder".into()), console_ctx: None, mcp_hub_ctx: None,
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
        db.update_performer(ctx, Some(who)).unwrap();
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
async fn context_info_names_the_senpai_its_holder_the_root_and_the_reviewer() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let shown = d.dispatch(&[s("context"), s("info")], &caller(h.coder, h.lane)).await;
    assert!(shown.is_ok(), "{}", shown.message());
    let text = shown.message();
    assert!(text.contains("Performer: coder"), "{text}");
    assert!(text.contains(&format!("Senpai: banto-seat ({}), banto", h.seat.short())), "{text}");
    assert!(text.contains("Lineage root: amy"), "{text}");
    assert!(text.contains("Reviewer: banto (senpai)"), "{text}");
    assert_eq!(text.matches("Senpai:").count(), 1, "one senpai line: {text}");
    let data = data(&shown);
    assert_eq!(data["senpai"]["context"], h.seat.to_hex());
    assert_eq!(data["senpai"]["label"], "banto-seat");
    assert_eq!(data["senpai"]["character"], "banto");
    assert_eq!(data["senpai"]["character_id"], h.banto.to_hex());
    assert_eq!(data["performer_name"], "coder");
    assert_eq!(data["lineage_root"]["name"], "amy");
    assert_eq!(data["reviewer_name"], "banto");
    assert_eq!(data["review_source"], "senpai");
    assert!(data["migration_note"].is_null());

    let top = d.dispatch(&[s("context"), s("info"), h.root.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!top.message().contains("Senpai:"), "a root console has no senpai: {}", top.message());
    assert!(top.message().contains("Reviewer: amy (self_confirmation)"), "{}", top.message());
    assert!(self::data(&top)["senpai"].is_null());
}

/// The holder of the lane's senpai (banto) may move it under a context
/// banto holds, but not under one it does not; the lineage root (amy)
/// may move it anywhere in its lineage. The move reroutes the asks.
#[tokio::test]
async fn context_move_moves_a_kohai_under_the_rule_and_reroutes_its_asks() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let lane = h.lane.to_hex();
    let refused = d.dispatch(&[s("context"), s("set"), lane.clone(), s("--senpai"), h.judge_seat.to_hex()], &caller(h.banto, h.seat)).await;
    assert!(!refused.is_ok());
    assert!(refused.message().contains(&format!(
        "only the holder of judge-seat ({}) (judge) or its lineage root (amy) may place a context under it", h.judge_seat.short()
    )), "{}", refused.message());
    let moved = d.dispatch(&[s("context"), s("set"), lane.clone(), s("--senpai"), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(moved.is_ok(), "{}", moved.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.judge_seat));
    assert_eq!(d.kernel().resolve_context_review(h.lane).await.unwrap().reviewer.name, "judge");
    let back = d.dispatch(&[s("context"), s("set"), h.other.to_hex(), s("--senpai"), h.seat.to_hex()], &caller(h.banto, h.seat)).await;
    assert!(back.is_ok(), "banto holds other-lane's senpai and the target seat: {}", back.message());
}

/// A root character's console is the top of its lineage, and a judge
/// shadow answers to the context it shadows: `kj context set --senpai` moves
/// neither, even for the lineage root. A judge shadow is a context performed
/// by a character a council block names as judge, whose parent is its senpai.
///
/// Falsified by telling a shadow by its label: a context another character
/// performs moves.
#[tokio::test]
async fn context_move_refuses_a_root_console_and_a_judge_shadow() {
    use crate::vfs::VfsOps;
    let d = test_dispatcher().await;
    let h = house(&d);
    let refused = d.dispatch(&[s("context"), s("set"), h.root.to_hex(), s("--senpai"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!refused.is_ok(), "a root console has no senpai");
    assert!(refused.message().contains("root console"), "{}", refused.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.root).unwrap(), None);

    let gate = "[council]\ncontexts = [\"reviewer\"]\npool = { method = \"loglinear\", weights = \"mass\" }\ndeadline_ms = 700\n\n\
                [[council.spec]]\nname = \"shell-gate\"\ncase = \"shell\"\n\n\
                [rc.coder.council]\nenabled = true\njudge = \"judge\"\n";
    let path = kaijutsu_types::paths::config_path(crate::kj::gate_policy::GATE_CONFIG_FILE);
    d.kernel().vfs().write_all(std::path::Path::new(&path), gate.as_bytes()).await.expect("write gate.toml");
    let judge = d.kernel_db().lock().get_character_by_name("judge").unwrap().unwrap().principal_id;
    let shadow = register_context(&d, Some("judge-lane"), Some(h.lane), h.amy);
    d.kernel_db().lock().update_performer(shadow, Some(judge)).unwrap();
    assert_eq!(d.kernel_db().lock().senpai_of(shadow).unwrap(), Some(h.lane));
    let refused = d.dispatch(&[s("context"), s("set"), shadow.to_hex(), s("--senpai"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!refused.is_ok(), "a judge shadow stays under the context it shadows");
    assert!(refused.message().contains("judge shadow"), "{}", refused.message());
    assert_eq!(d.kernel_db().lock().senpai_of(shadow).unwrap(), Some(h.lane));

    let moved = d.dispatch(&[s("context"), s("set"), h.other.to_hex(), s("--senpai"), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(moved.is_ok(), "a context no judge performs moves: {}", moved.message());
}

/// The judge names that tell a judge shadow come from every section's
/// council block that enables a council: `[rc.<dir>]`, `[character.<name>]`
/// and `[root]` alike.
///
/// Falsified by reading the judge names from one kind of section only.
#[tokio::test]
async fn the_council_judges_come_from_every_layer_s_council_block() {
    let d = test_dispatcher().await;
    write_gate_toml(&d, "[council]\ncontexts = [\"reviewer\"]\npool = { method = \"loglinear\", weights = \"mass\" }\ndeadline_ms = 700\n\n\
                         [[council.spec]]\nname = \"shell-gate\"\ncase = \"shell\"\n\n\
                         [rc.coder.council]\nenabled = true\njudge = \"judge\"\n\
                         [character.banto.council]\nenabled = true\njudge = \"banto-judge\"\n\
                         [root.council]\nenabled = true\njudge = \"root-judge\"\n\
                         [character.quiet.council]\nenabled = false\n").await;
    let mut judges = d.council_judges().await.expect("gate.toml loads");
    judges.sort();
    assert_eq!(judges, ["banto-judge", "judge", "root-judge"]);
}

/// Write `text` as the kernel's gate.toml.
async fn write_gate_toml(d: &KjDispatcher, text: &str) {
    use crate::vfs::VfsOps;
    let path = kaijutsu_types::paths::config_path(crate::kj::gate_policy::GATE_CONFIG_FILE);
    d.kernel().vfs().write_all(std::path::Path::new(&path), text.as_bytes()).await.expect("write gate.toml");
}

/// Give the character `judge` a model on a council server (a backend of kind `mk`).
fn give_judge_a_council_model(d: &KjDispatcher) -> PrincipalId {
    let db = d.kernel_db().lock();
    let mk = db.upsert_backend(&crate::kernel_db::BackendRow {
        backend_id: kaijutsu_types::BackendId::new(), name: s("mk-zorak"), kind: s("mk"), base_url: None,
        api_key_env: None, api_key_file: None, key_optional: true, request_timeout_secs: None,
        idle_timeout_secs: None, max_concurrent: None, created_at: 0, created_by: PrincipalId::system(),
    }).unwrap();
    let judge = db.get_character_by_name("judge").unwrap().unwrap().principal_id;
    db.set_character_model(&crate::kernel_db::CharacterModelRow {
        principal_id: judge, backend_id: mk.backend_id, model: s("gemma"), max_tokens: None,
        temperature: None, top_p: None, effort: None, thinking_budget: None, thinking_style: None,
    }).unwrap();
    judge
}

/// A gate.toml that does not load leaves the judge's model to tell a judge
/// shadow, and refuses nothing for it: an ordinary fork, whose parent is its
/// senpai, still moves.
///
/// Falsified by refusing every context shaped like a shadow when gate.toml
/// does not load.
#[tokio::test]
async fn context_move_moves_an_ordinary_fork_when_gate_toml_does_not_load() {
    let d = test_dispatcher().await;
    let h = house(&d);
    write_gate_toml(&d, "[council\nthis is not toml").await;
    let moved = d.dispatch(&[s("context"), s("set"), h.lane.to_hex(), s("--senpai"), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(moved.is_ok(), "{}", moved.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.judge_seat));
}

/// A character whose model is on a council server is a judge, so its fork
/// child that answers to its parent is a judge shadow, whether or not a
/// council block names it, and whether or not gate.toml loads.
///
/// Falsified by telling a judge shadow only by gate.toml's `judge` names.
#[tokio::test]
async fn context_move_refuses_a_shadow_whose_judge_has_a_council_model() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let judge = give_judge_a_council_model(&d);
    let shadow = register_context(&d, Some("judge-lane"), Some(h.lane), h.amy);
    d.kernel_db().lock().update_performer(shadow, Some(judge)).unwrap();
    for gate in ["[global]\n", "[council\nthis is not toml"] {
        write_gate_toml(&d, gate).await;
        let refused = d.dispatch(&[s("context"), s("set"), shadow.to_hex(), s("--senpai"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
        assert!(!refused.is_ok(), "{gate:?}: a judge shadow stays under the context it shadows");
        assert!(refused.message().contains("is a judge shadow"), "{gate:?}: {}", refused.message());
        assert_eq!(d.kernel_db().lock().senpai_of(shadow).unwrap(), Some(h.lane));
    }
}

/// `kj context archive` of a context archives its judge shadow in the same
/// transaction, says so, and marks the shadow archived in the drift router,
/// which frees its label there.
///
/// Falsified by archiving the shadow's row and leaving the router holding it
/// live.
#[tokio::test]
async fn archiving_a_context_archives_its_judge_shadow_in_the_row_and_the_router() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let judge = give_judge_a_council_model(&d);
    let shadow = register_context(&d, Some("judge-lane"), Some(h.lane), h.amy);
    d.kernel_db().lock().update_performer(shadow, Some(judge)).unwrap();
    let confirmed = KjCaller { confirmed: true, ..caller(h.amy, h.root) };
    let archived = d.dispatch(&[s("context"), s("archive"), h.lane.to_hex()], &confirmed).await;
    assert!(archived.is_ok(), "{}", archived.message());
    assert!(archived.message().contains("judge shadow judge-lane"), "{}", archived.message());
    assert!(d.kernel_db().lock().get_context(shadow).unwrap().unwrap().archived_at.is_some());
    let router = d.drift_router().read();
    assert_eq!(router.context_state(shadow), Some(kaijutsu_types::ContextState::Archived));
    assert!(router.resolve_context("judge-lane").is_err(), "the router frees the label");
}

/// `kj context set --senpai` refuses the context itself and a sibling (the rule), a
/// cycle, a senpai with no live holder, and a context with pending asks.
#[tokio::test]
async fn context_move_refuses_the_rule_a_cycle_a_holderless_senpai_and_pending_asks() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let set = |ctx: ContextId, senpai: ContextId| vec![s("context"), s("set"), ctx.to_hex(), s("--senpai"), senpai.to_hex()];
    for refused in [h.coder, h.sibling] {
        let result = d.dispatch(&set(h.lane, h.root), &caller(refused, h.lane)).await;
        assert!(!result.is_ok());
        assert!(result.message().contains(&format!(
            "only lane ({})'s senpai (banto) or its lineage root (amy) may move it to another senpai", h.lane.short()
        )), "{}", result.message());
    }
    let cycle = d.dispatch(&set(h.seat, h.lane), &caller(h.amy, h.root)).await;
    assert!(!cycle.is_ok() && cycle.message().contains("cycle"), "{}", cycle.message());

    // A context with no performer and no senpai has nobody to answer for the kohai.
    let empty = register_context(&d, Some("nobody-holds"), Some(h.root), h.amy);
    d.kernel_db().lock().conn_for_ledger().execute_batch(&format!(
        "DROP TRIGGER contexts_performer_fixed; DROP TRIGGER contexts_senpai_required;
         UPDATE contexts SET performer = NULL, senpai_ctx = NULL WHERE context_id = x'{}';",
        empty.to_hex()
    )).unwrap();
    let holderless = d.dispatch(&set(h.lane, empty), &caller(h.amy, h.root)).await;
    assert!(
        !holderless.is_ok() && holderless.message().contains("nobody-holds"),
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

/// Setting the senpai again clears the migration's note, which `kj context
/// info` prints until then.
#[tokio::test]
async fn context_info_prints_the_migration_note_until_the_senpai_is_set_again() {
    let d = test_dispatcher().await;
    let h = house(&d);
    d.kernel_db().lock().conn_for_ledger().execute(
        "INSERT INTO senpai_migration_report (context_id, senpai_ctx, note, old_reviewer, recorded_at) VALUES (?1, ?2, 'mapped by hand; set its senpai again', NULL, 1)",
        rusqlite::params![h.lane.as_bytes().as_slice(), h.seat.as_bytes().as_slice()],
    ).unwrap();
    let shown = d.dispatch(&[s("context"), s("info"), h.lane.to_hex()], &caller(h.amy, h.root)).await;
    assert!(shown.message().contains("Migration note: mapped by hand"), "{}", shown.message());
    let set = d.dispatch(&[s("context"), s("set"), h.lane.to_hex(), s("--senpai"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(set.is_ok(), "{}", set.message());
    let shown = d.dispatch(&[s("context"), s("info"), h.lane.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!shown.message().contains("Migration note:"), "{}", shown.message());
    assert!(self::data(&shown)["migration_note"].is_null());
}

/// The context-character migration's notes for a context print with the
/// senpai migration's, and only that context's.
#[tokio::test]
async fn context_info_prints_the_context_character_migration_notes() {
    let d = test_dispatcher().await;
    let h = house(&d);
    {
        let db = d.kernel_db().lock();
        for (kind, context, note) in [
            ("no_slot", Some(h.lane), "has no slot for its type"),
            ("dropped", Some(h.lane), "dropped a model override"),
            ("unmapped", Some(h.seat), "belongs to another context"),
            ("dropped", None, "belongs to no context"),
        ] {
            db.conn_for_ledger().execute(
                "INSERT INTO context_character_migration_report (kind, context_id, note, recorded_at) VALUES (?1, ?2, ?3, 1)",
                rusqlite::params![kind, context.map(|c| c.as_bytes().to_vec()), note],
            ).unwrap();
        }
    }
    let shown = d.dispatch(&[s("context"), s("info"), h.lane.to_hex()], &caller(h.amy, h.root)).await;
    assert!(shown.is_ok(), "{}", shown.message());
    let text = shown.message();
    assert!(text.contains("Character migration note (no_slot): has no slot for its type"), "{text}");
    assert!(text.contains("Character migration note (dropped): dropped a model override"), "{text}");
    assert!(!text.contains("another context") && !text.contains("no context"), "{text}");
    let notes = self::data(&shown)["character_migration_notes"].clone();
    assert_eq!(notes, serde_json::json!([
        { "kind": "no_slot", "note": "has no slot for its type" },
        { "kind": "dropped", "note": "dropped a model override" },
    ]));
    assert!(self::data(&shown)["migration_note"].is_null(), "the senpai note is its own key");

    let other = d.dispatch(&[s("context"), s("info"), h.root.to_hex()], &caller(h.amy, h.root)).await;
    assert_eq!(self::data(&other)["character_migration_notes"], serde_json::json!([]));
}

#[tokio::test]
async fn list_senpai_lists_the_live_contexts_that_answer_to_a_context() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let gone = register_context(&d, Some("gone-lane"), Some(h.seat), h.amy);
    d.kernel_db().lock().archive_context(gone).unwrap();
    let listed = d.dispatch(&[s("context"), s("list"), s("--senpai"), s(".")], &caller(h.banto, h.seat)).await;
    assert!(listed.is_ok(), "{}", listed.message());
    let text = listed.message();
    assert!(text.contains("lane") && text.contains(&h.lane.short()), "{text}");
    assert!(text.contains("other-lane") && text.contains(&h.other.short()), "{text}");
    assert!(!text.contains("gone-lane"), "an archived context is not listed: {text}");
    assert!(!text.contains("banto-seat") && !text.contains("judge-seat"), "only the contexts that answer to the seat: {text}");
    // `.data` keeps the list's shape: handle strings, labels first.
    assert_eq!(data(&listed), serde_json::json!(["lane", "other-lane"]));
    let none = d.dispatch(&[s("context"), s("list"), s("--senpai"), h.lane.to_hex()], &caller(h.banto, h.seat)).await;
    assert!(none.is_ok(), "{}", none.message());
    assert_eq!(none.message(), "(no contexts)");
    assert_eq!(data(&none), serde_json::json!([]));
}

/// `kj context create` makes a new context performed by the named
/// character. Its senpai is the caller's context, and its slots copy the
/// character's slots.
#[tokio::test]
async fn context_create_makes_a_context_character_under_the_callers_context() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let made = d.dispatch(&[s("context"), s("create"), s("coder"), s("--name"), s("fresh")], &caller(h.banto, h.seat)).await;
    assert!(made.is_ok(), "{}", made.message());

    let db = d.kernel_db().lock();
    let fresh = db.find_context_by_label("fresh").unwrap().unwrap();
    let character = db.context_character(fresh.context_id).unwrap().expect("the new context has a context character");
    assert_eq!(character.principal_id, h.coder);
    assert_eq!(fresh.created_by, h.banto, "the requester made it");
    assert_eq!(character.senpai_ctx, Some(h.seat), "its senpai is the caller's context");
    assert!(db.list_context_slots(fresh.context_id).unwrap().is_empty(), "the character has no slot to copy");
    assert_eq!(fresh.performer, Some(h.coder));
    drop(db);
    assert_eq!(d.kernel().resolve_context_review(fresh.context_id).await.unwrap().reviewer.name, "banto");
}

/// `--senpai` names any context it is given, and only when the caller's
/// character is that context's character or its lineage root; `--parent` is provenance only and may differ.
#[tokio::test]
async fn context_create_senpai_needs_the_caller_to_hold_it_or_be_its_root() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let create = |label: &str, extra: &[&str]| {
        let mut argv = vec![s("context"), s("create"), s("kohai-tester"), s("--name"), s(label)];
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

/// `kj drift push .senpai` lands the text in the senpai's log as a drift
/// block from the reporting context, with no context id to type. A
/// context with no senpai, an archived senpai and an empty report are
/// refused, and nothing lands.
#[tokio::test]
async fn drift_push_senpai_drifts_the_text_to_the_senpai_and_needs_no_id() {
    let d = test_dispatcher().await;
    let h = house(&d);
    for ctx in [h.root, h.seat, h.lane] {
        d.block_store().create_document(ctx, crate::DocumentKind::Conversation, None).unwrap();
    }
    let drifts = |ctx: ContextId| -> Vec<kaijutsu_types::BlockSnapshot> {
        d.block_store().block_snapshots(ctx).unwrap().into_iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Drift).collect()
    };
    let reported = d.dispatch(&[s("drift"), s("push"), s(".senpai"), s("the tests pass;"), s("ready for review")], &caller(h.coder, h.lane)).await;
    assert!(reported.is_ok(), "{}", reported.message());
    assert!(reported.message().contains("banto-seat"), "{}", reported.message());
    let landed = drifts(h.seat);
    assert_eq!(landed.len(), 1, "{landed:?}");
    assert!(landed[0].content.contains("the tests pass; ready for review"), "{}", landed[0].content);
    assert_eq!(landed[0].source_context, Some(h.lane), "the report is from the kohai");
    assert!(drifts(h.root).is_empty() && drifts(h.lane).is_empty(), "only the senpai's log gets it");

    let top = d.dispatch(&[s("drift"), s("push"), s(".senpai"), s("hello?")], &caller(h.amy, h.root)).await;
    assert!(!top.is_ok());
    assert!(top.message().contains("has no senpai"), "{}", top.message());

    let empty = d.dispatch(&[s("drift"), s("push"), s(".senpai")], &caller(h.coder, h.lane)).await;
    assert!(!empty.is_ok(), "{}", empty.message());

    d.kernel_db().lock().archive_context(h.seat).unwrap();
    let archived = d.dispatch(&[s("drift"), s("push"), s(".senpai"), s("still there?")], &caller(h.coder, h.lane)).await;
    assert!(!archived.is_ok());
    assert!(archived.message().contains("archived"), "{}", archived.message());
    assert!(
        archived.message().contains("kj context promote") && archived.message().contains("kj context set <ctx> --senpai"),
        "names both ways out: {}", archived.message()
    );
    assert_eq!(drifts(h.seat).len(), 1, "a refused push lands nothing");
}

/// A new context answers to its creator, so a caller with no character
/// sheet, or a retired one, cannot create one.
#[tokio::test]
async fn context_create_needs_a_live_character_caller() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let nobody = d.dispatch(&[s("context"), s("create"), s("kohai-tester"), s("--name"), s("orphan")], &caller(PrincipalId::new(), h.root)).await;
    assert!(!nobody.is_ok());
    assert!(nobody.message().contains("no character sheet"), "{}", nobody.message());
    d.kernel_db().lock().retire_character(h.sibling, 1).unwrap();
    let retired = d.dispatch(&[s("context"), s("create"), s("kohai-tester"), s("--name"), s("orphan")], &caller(h.sibling, h.other)).await;
    assert!(!retired.is_ok() && retired.message().contains("is retired"), "{}", retired.message());
    assert!(d.kernel_db().lock().find_context_by_label("orphan").unwrap().is_none());
}

/// A fork is a kohai of the context it forked from: the verbs read the
/// edge of the context they name, and moving a fork moves that fork only.
#[tokio::test]
async fn the_edge_verbs_read_the_edge_of_the_context_they_name() {
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

    let shown = d.dispatch(&[s("context"), s("info"), s("lane-fork")], &caller(h.coder, h.lane)).await;
    assert!(shown.message().contains(&format!("Senpai: lane ({}), coder", h.lane.short())), "{}", shown.message());
    let listed = d.dispatch(&[s("context"), s("list"), s("--senpai"), s("lane")], &caller(h.coder, h.lane)).await;
    assert_eq!(data(&listed), serde_json::json!(["lane-fork"]), "{}", listed.message());
    let none = d.dispatch(&[s("context"), s("list"), s("--senpai"), s("lane-fork")], &caller(h.coder, h.lane)).await;
    assert_eq!(data(&none), serde_json::json!([]), "{}", none.message());

    let reported = d.dispatch(&[s("drift"), s("push"), s(".senpai"), s("from the fork")], &caller(h.coder, fork)).await;
    assert!(reported.is_ok(), "{}", reported.message());
    assert_eq!((drifts(h.lane), drifts(h.seat)), (1, 0), "a fork reports to the context it forked from");

    // `set` moves the fork alone.
    let moved = d.dispatch(&[s("context"), s("set"), s("lane-fork"), s("--senpai"), s("judge-seat")], &caller(h.amy, h.root)).await;
    assert!(moved.is_ok(), "{}", moved.message());
    let db = d.kernel_db().lock();
    assert_eq!(db.senpai_of(fork).unwrap(), Some(h.judge_seat));
    assert_eq!(db.senpai_of(h.lane).unwrap(), Some(h.seat), "the lane stays where it was");
}

/// Archiving a context archives that context only. A push to it and a
/// `kj context set --senpai` onto it refuse, and the contexts that answer to it stay live. Promoting it
/// makes both work again.
#[tokio::test]
async fn an_archived_senpai_refuses_pushes_and_sets_until_promoted() {
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

    let report = d.dispatch(&[s("drift"), s("push"), s(".senpai"), s("anyone there?")], &caller(h.coder, h.lane)).await;
    assert!(!report.is_ok() && report.message().contains("is archived"), "{}", report.message());
    assert!(report.message().contains("kj context promote") && report.message().contains("kj context set <ctx> --senpai"), "{}", report.message());
    assert_eq!(drifts(h.seat), 0, "a refused report lands nothing");
    let onto = d.dispatch(&[s("context"), s("set"), s("other-lane"), s("--senpai"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!onto.is_ok() && onto.message().contains("is archived"), "{}", onto.message());
    // The contexts stranded under the archived senpai are found by it.
    let stranded = d.dispatch(&[s("context"), s("list"), s("--senpai"), h.seat.to_hex()], &caller(h.coder, h.lane)).await;
    assert_eq!(data(&stranded), serde_json::json!(["lane", "other-lane"]), "{}", stranded.message());

    let promoted = d.dispatch(&[s("context"), s("promote"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(promoted.is_ok(), "{}", promoted.message());
    let report = d.dispatch(&[s("drift"), s("push"), s(".senpai"), s("back again")], &caller(h.coder, h.lane)).await;
    assert!(report.is_ok(), "{}", report.message());
    assert_eq!(drifts(h.seat), 1);
    let onto = d.dispatch(&[s("context"), s("set"), s("other-lane"), s("--senpai"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(onto.is_ok(), "{}", onto.message());
}

/// The emitted help speaks of contexts, never of instances.
#[tokio::test]
async fn moved_verbs_help_speak_of_contexts() {
    let d = test_dispatcher().await;
    for argv in [
                vec![s("context"), s("info"), s("--help")],
        vec![s("context"), s("set"), s("--help")],
        vec![s("context"), s("list"), s("--help")],
        vec![s("drift"), s("push"), s("--help")],
        vec![s("context"), s("create"), s("--help")],
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
async fn context_move_refuses_while_a_fork_of_the_context_has_a_pending_ask() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let fork = register_context(&d, Some("lane-fork"), Some(h.lane), h.amy);
    d.kernel_db().lock().copy_context_character(h.lane, fork).unwrap().expect("the lane has a context character");
    let gate_caller = KjCaller { reviewer_id: Some(h.banto), ..caller(h.coder, fork) };
    let spec = crate::kj::gate::GateSpec { publishes_pair: false, tool_call: None, origin: approval_ledger::types::Origin::Hook, instance: "test".into(), tool: "test".into(), hook_id: None, description: "pending".into(), authorized_label: "pending".into(), statements: vec![crate::kj::gate::GatedStatement { rendered: "pending".into(), statement_kind: "test".into(), vars: vec![], source_index: None }], exec_source: None, exec_stdin: None, planned: vec![] };
    let outcome = crate::kj::gate::run_gate(d.kernel(), &gate_caller, spec, d.kernel().ledger_flows(), &crate::kj::gate_policy::no_config()).await;
    assert!(outcome.ask.is_some());
    let pending = d.dispatch(&[s("context"), s("set"), h.lane.to_hex(), s("--senpai"), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!pending.is_ok() && pending.message().contains("pending asks of lane"), "{}", pending.message());
}

/// `.senpai` follows `senpai_ctx` and `.parent` follows `forked_from`, in any
/// order. A step with nothing to follow names the context and the step.
#[tokio::test]
async fn the_senpai_ref_chain_follows_senpai_and_parent_steps_in_any_order() {
    let d = test_dispatcher().await;
    let h = house(&d);
    // lane: forked from banto-seat, but now answers to judge-seat (forked from amy).
    let moved = d.dispatch(&[s("context"), s("set"), s("lane"), s("--senpai"), s("judge-seat")], &caller(h.amy, h.root)).await;
    assert!(moved.is_ok(), "{}", moved.message());
    let resolve = |from: ContextId, reference: &str| {
        let db = d.kernel_db().lock();
        crate::kj::refs::resolve_context_arg_from(Some(reference), Some(from), &db)
    };
    assert_eq!(resolve(h.lane, ".senpai").unwrap(), h.judge_seat);
    assert_eq!(resolve(h.lane, ".parent").unwrap(), h.seat, "parent is provenance, not the senpai");
    assert_eq!(resolve(h.lane, ".senpai.senpai").unwrap(), h.root);
    assert_eq!(resolve(h.lane, ".parent.senpai").unwrap(), h.root);
    assert_eq!(resolve(h.lane, ".senpai.parent").unwrap(), h.root);
    assert_eq!(resolve(h.other, ".parent.parent").unwrap(), h.root);

    let error = resolve(h.root, ".senpai").unwrap_err();
    assert!(error.contains("amy") && error.contains("has no senpai") && error.contains(".senpai"), "{error}");
    let error = resolve(h.lane, ".parent.senpai.senpai").unwrap_err();
    assert!(error.contains("amy") && error.contains("has no senpai"), "the root is where the chain ends: {error}");
    let error = resolve(h.root, ".parent").unwrap_err();
    assert!(error.contains("amy") && error.contains("has no parent"), "{error}");

    // Not exactly a chain of those segments: a label or prefix query, which finds nothing.
    for query in [".senpaifoo", ".senpai.", ".senpai..parent", ".parent.foo", "senpai"] {
        let error = resolve(h.lane, query).unwrap_err();
        assert!(!error.contains("has no senpai") && !error.contains("has no parent"), "{query}: a query, not a chain: {error}");
    }
}

/// `kj context set --senpai` is one act per call: it names no other option.
#[tokio::test]
async fn context_set_senpai_conflicts_with_the_other_set_options() {
    let d = test_dispatcher().await;
    let h = house(&d);
    for extra in [
        vec!["--cwd", "/"],
        vec!["--env", "A=B"],
        vec!["--system-prompt", "x"],
        vec!["--egress-allow", "localhost"],
        vec!["--egress-deny", "localhost"],
    ] {
        let mut argv = vec![s("context"), s("set"), s("lane"), s("--senpai"), s("judge-seat")];
        argv.extend(extra.iter().map(|a| s(a)));
        let refused = d.dispatch(&argv, &caller(h.amy, h.root)).await;
        assert!(!refused.is_ok(), "{argv:?}: {}", refused.message());
        assert!(refused.message().contains("cannot be used with"), "{argv:?}: {}", refused.message());
    }
    assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.seat), "a conflict changed nothing");
}

/// `kj context set --senpai <ctx>` with no context names the current one, and
/// `kj context list --senpai` refuses `--tree`.
#[tokio::test]
async fn context_set_senpai_defaults_to_the_current_context_and_list_senpai_refuses_tree() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let moved = d.dispatch(&[s("context"), s("set"), s("--senpai"), s("judge-seat")], &caller(h.amy, h.lane)).await;
    assert!(moved.is_ok(), "{}", moved.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.judge_seat));
    let tree = d.dispatch(&[s("context"), s("list"), s("--senpai"), s("."), s("--tree")], &caller(h.amy, h.root)).await;
    assert!(!tree.is_ok() && tree.message().contains("cannot be used with"), "{}", tree.message());
}

/// `kj senpai` is gone, not aliased: every old subcommand is unknown.
#[tokio::test]
async fn kj_senpai_is_gone() {
    let d = test_dispatcher().await;
    let h = house(&d);
    for argv in [
        vec![s("senpai")],
        vec![s("senpai"), s("show")],
        vec![s("senpai"), s("kohai")],
        vec![s("senpai"), s("set"), s("lane"), s("banto-seat")],
        vec![s("senpai"), s("report"), s("hello")],
    ] {
        let r = d.dispatch(&argv, &caller(h.amy, h.root)).await;
        assert!(!r.is_ok(), "`kj senpai` must be unknown: {argv:?}: {}", r.message());
    }
}

/// The bespoke senpai verbs are gone: the general verbs do their work.
#[tokio::test]
async fn the_bespoke_senpai_verbs_are_gone() {
    let d = test_dispatcher().await;
    let h = house(&d);
    for argv in [
        vec![s("context"), s("kohai")],
        vec![s("context"), s("move"), s("lane"), s("judge-seat")],
        vec![s("drift"), s("report"), s("hello")],
    ] {
        let r = d.dispatch(&argv, &caller(h.amy, h.root)).await;
        assert!(!r.is_ok(), "{argv:?} must be unknown: {}", r.message());
    }
}

/// `kj context list --senpai` names an archived senpai by label or by the
/// short id `kj context info` prints, and still lists its live contexts.
#[tokio::test]
async fn list_senpai_finds_the_contexts_stranded_under_an_archived_senpai() {
    let d = test_dispatcher().await;
    let h = house(&d);
    d.kernel_db().lock().archive_context(h.seat).unwrap();
    for name in [s("banto-seat"), h.seat.short()] {
        let listed = d.dispatch(&[s("context"), s("list"), s("--senpai"), name.clone()], &caller(h.amy, h.root)).await;
        assert!(listed.is_ok(), "{name}: {}", listed.message());
        assert_eq!(data(&listed), serde_json::json!(["lane", "other-lane"]), "{name}");
    }
    let missing = d.dispatch(&[s("context"), s("list"), s("--senpai"), s("no-such-context")], &caller(h.amy, h.root)).await;
    assert!(!missing.is_ok(), "{}", missing.message());
}

/// Two archived contexts that match one prefix refuse and name the candidates.
#[tokio::test]
async fn list_senpai_refuses_an_ambiguous_archived_senpai() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let (a, b) = (register_context(&d, Some("old-a"), Some(h.root), h.amy), register_context(&d, Some("old-b"), Some(h.root), h.amy));
    for ctx in [a, b] {
        d.kernel_db().lock().archive_context(ctx).unwrap();
    }
    let result = d.dispatch(&[s("context"), s("list"), s("--senpai"), s("old-")], &caller(h.amy, h.root)).await;
    assert!(!result.is_ok());
    assert!(result.message().contains("old-a") && result.message().contains("old-b"), "{}", result.message());
}

/// An empty `--senpai` is a refusal, not the current context.
#[tokio::test]
async fn an_empty_senpai_value_refuses_on_set_and_create() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let set = d.dispatch(&[s("context"), s("set"), h.lane.to_hex(), s("--senpai"), s("")], &caller(h.amy, h.root)).await;
    assert!(!set.is_ok() && set.message().contains("--senpai needs a context"), "{}", set.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.seat));
    let create = d.dispatch(&[s("context"), s("create"), s("kohai-tester"), s("--name"), s("empty-senpai"), s("--senpai"), s("")], &caller(h.banto, h.seat)).await;
    assert!(!create.is_ok() && create.message().contains("--senpai needs a context"), "{}", create.message());
    assert!(d.kernel_db().lock().find_context_by_label("empty-senpai").unwrap().is_none());
}

/// A claimed ask is unresolved too, so it blocks the move like a pending one.
#[tokio::test]
async fn context_set_senpai_refuses_while_an_ask_is_claimed() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let gate_caller = KjCaller { reviewer_id: Some(h.banto), ..caller(h.coder, h.lane) };
    let spec = crate::kj::gate::GateSpec { publishes_pair: false, tool_call: None, origin: approval_ledger::types::Origin::Hook, instance: "test".into(), tool: "test".into(), hook_id: None, description: "pending".into(), authorized_label: "pending".into(), statements: vec![crate::kj::gate::GatedStatement { rendered: "pending".into(), statement_kind: "test".into(), vars: vec![], source_index: None }], exec_source: None, exec_stdin: None, planned: vec![] };
    let outcome = crate::kj::gate::run_gate(d.kernel(), &gate_caller, spec, d.kernel().ledger_flows(), &crate::kj::gate_policy::no_config()).await;
    let ask = outcome.ask.expect("the gate raised an ask");
    approval_ledger::claim::claim(d.kernel_db().lock().conn_for_ledger(), &ask.request_id, h.banto.as_bytes()).unwrap();
    let result = d.dispatch(&[s("context"), s("set"), h.lane.to_hex(), s("--senpai"), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!result.is_ok() && result.message().contains("pending asks"), "{}", result.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.seat));
}

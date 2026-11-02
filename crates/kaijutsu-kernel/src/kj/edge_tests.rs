//! Tests for the senpai edge verbs: `kj context info|kohai|move`,
//! `kj drift report`, and `kj context create`'s senpai placement.

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
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow { context_type: None,
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
async fn context_info_names_the_senpai_its_holder_the_root_and_the_reviewer() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let shown = d.dispatch(&[s("context"), s("info")], &caller(h.coder, h.lane)).await;
    assert!(shown.is_ok(), "{}", shown.message());
    let text = shown.message();
    assert!(text.contains("Played by: coder"), "{text}");
    assert!(text.contains(&format!("Senpai: banto-seat ({}), banto", h.seat.short())), "{text}");
    assert!(text.contains("Lineage root: amy"), "{text}");
    assert!(text.contains("Reviewer: banto (senpai)"), "{text}");
    assert_eq!(text.matches("Senpai:").count(), 1, "one senpai line: {text}");
    let data = data(&shown);
    assert_eq!(data["senpai"]["context"], h.seat.to_hex());
    assert_eq!(data["senpai"]["label"], "banto-seat");
    assert_eq!(data["senpai"]["character"], "banto");
    assert_eq!(data["senpai"]["character_id"], h.banto.to_hex());
    assert_eq!(data["played_by_name"], "coder");
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
    let refused = d.dispatch(&[s("context"), s("move"), lane.clone(), h.judge_seat.to_hex()], &caller(h.banto, h.seat)).await;
    assert!(!refused.is_ok());
    assert!(refused.message().contains(&format!(
        "only the holder of judge-seat ({}) (judge) or its lineage root (amy) may place a context under it", h.judge_seat.short()
    )), "{}", refused.message());
    let moved = d.dispatch(&[s("context"), s("move"), lane.clone(), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(moved.is_ok(), "{}", moved.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.lane).unwrap(), Some(h.judge_seat));
    assert_eq!(d.kernel().resolve_context_review(h.lane).await.unwrap().reviewer.name, "judge");
    let back = d.dispatch(&[s("context"), s("move"), h.other.to_hex(), h.seat.to_hex()], &caller(h.banto, h.seat)).await;
    assert!(back.is_ok(), "banto holds other-lane's senpai and the target seat: {}", back.message());
}

/// A root character's console is the top of its lineage, and a judge
/// shadow answers to its coder: `kj context move` moves neither, even for
/// the lineage root.
#[tokio::test]
async fn context_move_refuses_a_root_console_and_a_judge_shadow() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let refused = d.dispatch(&[s("context"), s("move"), h.root.to_hex(), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!refused.is_ok(), "a root console has no senpai");
    assert!(refused.message().contains("root console"), "{}", refused.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.root).unwrap(), None);

    d.kernel_db().lock().update_context_type(h.other, "judge").unwrap();
    let refused = d.dispatch(&[s("context"), s("move"), h.other.to_hex(), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!refused.is_ok(), "a judge shadow stays under its coder");
    assert!(refused.message().contains("judge shadow"), "{}", refused.message());
    assert_eq!(d.kernel_db().lock().senpai_of(h.other).unwrap(), Some(h.seat));
}

/// `kj context move` refuses the kohai itself and a sibling (the rule), a
/// cycle, a senpai with no live holder, and a context with pending asks.
#[tokio::test]
async fn context_move_refuses_the_rule_a_cycle_a_holderless_senpai_and_pending_asks() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let set = |ctx: ContextId, senpai: ContextId| vec![s("context"), s("move"), ctx.to_hex(), senpai.to_hex()];
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
    let set = d.dispatch(&[s("context"), s("move"), h.lane.to_hex(), h.seat.to_hex()], &caller(h.amy, h.root)).await;
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
async fn kohai_list_lists_the_live_kohai_with_type_and_performer() {
    let d = test_dispatcher().await;
    let h = house(&d);
    d.kernel_db().lock().update_context_type(h.other, "coder").unwrap();
    let gone = register_context(&d, Some("gone-lane"), Some(h.seat), h.amy);
    d.kernel_db().lock().archive_context(gone).unwrap();
    let listed = d.dispatch(&[s("context"), s("kohai")], &caller(h.banto, h.seat)).await;
    assert!(listed.is_ok(), "{}", listed.message());
    let text = listed.message();
    assert!(text.contains(&format!("lane ({})  coder  default", h.lane.short())), "{text}");
    assert!(text.contains(&format!("other-lane ({})  sibling  coder", h.other.short())), "{text}");
    assert!(!text.contains("gone-lane"), "an archived kohai is not listed: {text}");
    let data = data(&listed);
    assert_eq!(data["kohai"].as_array().unwrap().len(), 2);
    assert_eq!(data["kohai"][0]["character"], "coder");
    let none = d.dispatch(&[s("context"), s("kohai"), h.lane.to_hex()], &caller(h.banto, h.seat)).await;
    assert!(none.message().contains("has no live kohai"), "{}", none.message());
}

/// `kj context create` makes a new context performed by the named
/// character. Its senpai is the caller's context, and its slots copy the
/// character's slots.
#[tokio::test]
async fn context_create_makes_a_context_character_under_the_callers_context() {
    let d = test_dispatcher().await;
    let h = house(&d);
    let made = d.dispatch(&[s("context"), s("create"), s("coder"), s("--name"), s("fresh"), s("--type"), s("default")], &caller(h.banto, h.seat)).await;
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

/// `kj drift report` lands the text in the senpai's log as a drift
/// block from the reporting context, with no context id to type. A
/// context with no senpai, an archived senpai and an empty report are
/// refused, and nothing lands.
#[tokio::test]
async fn drift_report_drifts_the_text_to_the_senpai_and_needs_no_id() {
    let d = test_dispatcher().await;
    let h = house(&d);
    for ctx in [h.root, h.seat, h.lane] {
        d.block_store().create_document(ctx, crate::DocumentKind::Conversation, None).unwrap();
    }
    let drifts = |ctx: ContextId| -> Vec<kaijutsu_types::BlockSnapshot> {
        d.block_store().block_snapshots(ctx).unwrap().into_iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Drift).collect()
    };
    let reported = d.dispatch(&[s("drift"), s("report"), s("the tests pass;"), s("ready for review")], &caller(h.coder, h.lane)).await;
    assert!(reported.is_ok(), "{}", reported.message());
    assert!(reported.message().contains("banto-seat"), "{}", reported.message());
    let landed = drifts(h.seat);
    assert_eq!(landed.len(), 1, "{landed:?}");
    assert!(landed[0].content.contains("the tests pass; ready for review"), "{}", landed[0].content);
    assert_eq!(landed[0].source_context, Some(h.lane), "the report is from the kohai");
    assert!(drifts(h.root).is_empty() && drifts(h.lane).is_empty(), "only the senpai's log gets it");

    let top = d.dispatch(&[s("drift"), s("report"), s("hello?")], &caller(h.amy, h.root)).await;
    assert!(!top.is_ok());
    assert!(top.message().contains("has no senpai"), "{}", top.message());

    let empty = d.dispatch(&[s("drift"), s("report")], &caller(h.coder, h.lane)).await;
    assert!(!empty.is_ok(), "{}", empty.message());

    d.kernel_db().lock().archive_context(h.seat).unwrap();
    let archived = d.dispatch(&[s("drift"), s("report"), s("still there?")], &caller(h.coder, h.lane)).await;
    assert!(!archived.is_ok());
    assert!(archived.message().contains("archived"), "{}", archived.message());
    assert_eq!(drifts(h.seat).len(), 1, "a refused report lands nothing");
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
    let listed = d.dispatch(&[s("context"), s("kohai"), s("lane")], &caller(h.coder, h.lane)).await;
    assert!(listed.message().contains(&format!("lane-fork ({})  coder", fork.short())), "{}", listed.message());
    let none = d.dispatch(&[s("context"), s("kohai"), s("lane-fork")], &caller(h.coder, h.lane)).await;
    assert!(none.message().contains("has no live kohai"), "{}", none.message());

    let reported = d.dispatch(&[s("drift"), s("report"), s("from the fork")], &caller(h.coder, fork)).await;
    assert!(reported.is_ok(), "{}", reported.message());
    assert_eq!((drifts(h.lane), drifts(h.seat)), (1, 0), "a fork reports to the context it forked from");

    // `set` moves the fork alone.
    let moved = d.dispatch(&[s("context"), s("move"), s("lane-fork"), s("judge-seat")], &caller(h.amy, h.root)).await;
    assert!(moved.is_ok(), "{}", moved.message());
    let db = d.kernel_db().lock();
    assert_eq!(db.senpai_of(fork).unwrap(), Some(h.judge_seat));
    assert_eq!(db.senpai_of(h.lane).unwrap(), Some(h.seat), "the lane stays where it was");
}

/// Archiving a context archives that context only. A report to it and a
/// `kj context move` onto it refuse, and its kohai stay live. Promoting it
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

    let report = d.dispatch(&[s("drift"), s("report"), s("anyone there?")], &caller(h.coder, h.lane)).await;
    assert!(!report.is_ok() && report.message().contains("is archived"), "{}", report.message());
    assert_eq!(drifts(h.seat), 0, "a refused report lands nothing");
    let onto = d.dispatch(&[s("context"), s("move"), s("other-lane"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!onto.is_ok() && onto.message().contains("is archived"), "{}", onto.message());
    let kohai = d.dispatch(&[s("context"), s("kohai"), s("lane")], &caller(h.coder, h.lane)).await;
    assert!(kohai.is_ok(), "the kohai's own verbs still work: {}", kohai.message());

    let promoted = d.dispatch(&[s("context"), s("promote"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(promoted.is_ok(), "{}", promoted.message());
    let report = d.dispatch(&[s("drift"), s("report"), s("back again")], &caller(h.coder, h.lane)).await;
    assert!(report.is_ok(), "{}", report.message());
    assert_eq!(drifts(h.seat), 1);
    let onto = d.dispatch(&[s("context"), s("move"), s("other-lane"), h.seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(onto.is_ok(), "{}", onto.message());
}

/// The emitted help speaks of contexts, never of instances.
#[tokio::test]
async fn moved_verbs_help_speak_of_contexts() {
    let d = test_dispatcher().await;
    for argv in [
                vec![s("context"), s("info"), s("--help")],
        vec![s("context"), s("move"), s("--help")],
        vec![s("drift"), s("report"), s("--help")],
        vec![s("context"), s("kohai"), s("--help")],
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
    let pending = d.dispatch(&[s("context"), s("move"), h.lane.to_hex(), h.judge_seat.to_hex()], &caller(h.amy, h.root)).await;
    assert!(!pending.is_ok() && pending.message().contains("pending asks of lane"), "{}", pending.message());
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

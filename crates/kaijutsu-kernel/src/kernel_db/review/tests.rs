use super::*;

// ── 22d. review routing climbs the senpai chain ─────────────────────

/// amy's root console (played by amy, a root) → banto's seat → a lane
/// played by coder: the coder's ask resolves to banto, banto's to amy,
/// and amy's own statement in her console exhausts the climb and becomes
/// a self-confirmation rather than an unanswerable ask.
#[test]
fn route_review_goes_to_the_senpais_holder_and_a_root_console_confirms_itself() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();

    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&root), Some(banto));
    let lane = senpai_ctx_row(&db, ws_id, "lane-a", Some(&seat), Some(coder));

    assert_eq!(
        db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(),
        EffectiveReviewer::Assigned(banto),
        "the lane's senpai is banto's seat, held by banto",
    );
    assert_eq!(
        db.route_review(ReviewQuery::ask(seat.context_id, banto)).unwrap(),
        EffectiveReviewer::Assigned(amy),
        "the seat's senpai is amy's console",
    );
    assert_eq!(
        db.route_review(ReviewQuery::ask(root.context_id, amy)).unwrap(),
        EffectiveReviewer::SelfConfirmation(amy),
        "a root console has no senpai and its root confirms itself",
    );
    for context in [root.context_id, seat.context_id, lane.context_id] {
        assert_eq!(db.lineage_root(context).unwrap(), amy, "amy tops the senpai chain of every context");
    }
}

/// A senpai's holder is the character of its instance; a root console is
/// the conversation of its root's instance with no senpai, and the root's
/// sheet reads it back as `root_ctx` whatever the stored column says.
#[test]
fn a_root_console_is_the_conversation_of_its_roots_instance_with_no_senpai() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, amy] = seed_characters(&db, &["coder", "amy"])[..] else { unreachable!() };
    assert_eq!(db.get_character(amy).unwrap().unwrap().root_ctx, None, "a character that is not a root has no console");
    db.update_character_root(amy, true).unwrap();
    let console = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    assert_eq!(db.get_character(amy).unwrap().unwrap().root_ctx, Some(console.context_id), "the root instance's conversation is the console");
    let lane = senpai_ctx_row(&db, ws_id, "lane", Some(&console), Some(coder));

    assert_eq!(db.context_holder(console.context_id).unwrap(), Some(amy));
    assert_eq!(db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(), EffectiveReviewer::Assigned(amy));
    assert_eq!(db.lineage_root(lane.context_id).unwrap(), amy);
}

/// A banto that forks itself answers to the banto's own senpai: the
/// fork's senpai is banto's seat, held by the actor, so the climb goes on.
#[test]
fn route_review_climbs_past_a_senpai_the_actor_holds() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [banto, amy] = seed_characters(&db, &["banto", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&root), Some(banto));
    let fork = senpai_ctx_row(&db, ws_id, "banto-fork", Some(&seat), Some(banto));

    assert_eq!(
        db.route_review(ReviewQuery::ask(fork.context_id, banto)).unwrap(),
        EffectiveReviewer::Assigned(amy),
    );
    // A human at the top of the chain, acting in the fork, confirms themself.
    assert_eq!(
        db.route_review(ReviewQuery::ask(fork.context_id, amy)).unwrap(),
        EffectiveReviewer::SelfConfirmation(amy),
        "a fork's performer is its source's holder, so the climb passes banto and then amy herself",
    );
}

/// A root confirms its own statements only in the console it holds: in
/// another root's console it is refused, naming the actor and the context.
#[test]
fn a_root_confirms_only_the_console_it_holds() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [amy, bob] = seed_characters(&db, &["amy", "bob"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    db.update_character_root(bob, true).unwrap();
    let amy_console = senpai_ctx_row(&db, ws_id, "amy-console", None, Some(amy));
    let bob_console = senpai_ctx_row(&db, ws_id, "bob-console", None, Some(bob));
    assert_eq!(db.route_review(ReviewQuery::ask(amy_console.context_id, amy)).unwrap(), EffectiveReviewer::SelfConfirmation(amy));
    assert_eq!(db.route_review(ReviewQuery::ask(bob_console.context_id, bob)).unwrap(), EffectiveReviewer::SelfConfirmation(bob));
    let error = db.route_review(ReviewQuery::ask(bob_console.context_id, amy)).unwrap_err();
    assert!(matches!(error, KernelDbError::Validation(_)), "{error}");
    assert!(error.to_string().contains("amy") && error.to_string().contains("bob-console"), "{error}");
}

/// Only a root character confirms its own statements. A non-root actor
/// at a context with no senpai has no reviewer, and routing refuses
/// naming the actor and the context.
#[test]
fn route_review_refuses_a_non_root_actor_at_a_top_context() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [bob, coder] = seed_characters(&db, &["bob", "coder"])[..] else { unreachable!() };

    let orphan = senpai_ctx_row(&db, ws_id, "orphan-top", None, Some(bob));
    let error = db.route_review(ReviewQuery::ask(orphan.context_id, bob)).unwrap_err();
    assert!(matches!(error, KernelDbError::Validation(_)), "{error}");
    let text = error.to_string();
    assert!(text.contains("bob") && text.contains("orphan-top") && text.contains("root"), "{text}");
    assert!(text.contains("kj context set <ctx> --senpai <ctx>"), "names the fix: {text}");

    // A lane under a top held by the actor itself climbs into the same refusal.
    let lane = senpai_ctx_row(&db, ws_id, "under-bob", Some(&orphan), Some(coder));
    assert_eq!(db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(), EffectiveReviewer::Assigned(bob));
    let same = senpai_ctx_row(&db, ws_id, "bob-again", Some(&orphan), Some(bob));
    let error = db.route_review(ReviewQuery::ask(same.context_id, bob)).unwrap_err().to_string();
    assert!(error.contains("bob") && error.contains("orphan-top"), "{error}");

    let retired_root = seed_characters(&db, &["gone"])[0];
    db.update_character_root(retired_root, true).unwrap();
    assert!(db.retire_character(retired_root, 1).unwrap());
    let abandoned = senpai_ctx_row(&db, ws_id, "abandoned-root", None, Some(retired_root));
    let error = db.route_review(ReviewQuery::ask(abandoned.context_id, retired_root)).unwrap_err();
    assert!(error.to_string().contains("gone"), "a retired root does not confirm itself: {error}");
}

/// A non-root character acting in a root context it does not hold is
/// reviewed by the live root that holds it: the climb starts at the
/// context itself. A holder that is not a live root does not review, so a
/// non-root top still refuses, and a root in another root's console is
/// still refused.
#[test]
fn route_review_makes_a_live_root_review_a_model_acting_in_its_context() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [claude, amy, bob] = seed_characters(&db, &["claude", "amy", "bob"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    assert_eq!(
        db.route_review(ReviewQuery::ask(root.context_id, claude)).unwrap(),
        EffectiveReviewer::TopHolder(amy),
        "amy holds the top context, so she reviews what claude does in it",
    );
    let orphan = senpai_ctx_row(&db, ws_id, "orphan-top", None, Some(bob));
    assert!(db.route_review(ReviewQuery::ask(orphan.context_id, claude)).is_err(), "bob is no live root");
    assert!(
        db.route_review(ReviewQuery::ask(root.context_id, claude).above(amy)).is_err(),
        "an escalation past amy has nobody above her",
    );
}

/// A retired holder refuses by name; it never substitutes another character.
#[test]
fn route_review_retired_holder_refuses_by_name() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let seat = senpai_ctx_row(&db, ws_id, "retired-seat", Some(&root), Some(banto));
    let lane = senpai_ctx_row(&db, ws_id, "retired-seat-lane", Some(&seat), Some(coder));
    assert!(db.retire_character(banto, 1).unwrap());

    let error = db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap_err().to_string();
    assert!(error.contains("banto") && error.contains("retired"), "{error}");
    assert!(error.contains("retired-seat"), "names the senpai context: {error}");
    assert!(!error.contains("amy"), "never substitutes the next holder up: {error}");
}

/// A context with no performer routes like any other: an ask raised in it
/// climbs to the performer of its senpai, past senpais with no performer.
#[test]
fn route_review_in_a_context_with_no_performer_climbs_past_performer_less_senpais() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, amy] = seed_characters(&db, &["coder", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let console = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let hub = senpai_ctx_row(&db, ws_id, "hub", Some(&console), None);
    let log = senpai_ctx_row(&db, ws_id, "log", Some(&hub), None);
    assert_eq!(
        db.route_review(ReviewQuery::ask(log.context_id, coder)).unwrap(),
        EffectiveReviewer::Assigned(amy),
        "the log answers to the hub, which has no performer, so the ask reaches the console's performer",
    );
    assert_eq!(
        db.route_review(ReviewQuery::ask(hub.context_id, coder)).unwrap(),
        EffectiveReviewer::Assigned(amy),
    );
    assert_eq!(
        db.route_review(ReviewQuery::ask(log.context_id, amy)).unwrap(),
        EffectiveReviewer::SelfConfirmation(amy),
        "the root's own statement in a performer-less context under its console confirms itself",
    );
    assert_eq!(db.senpai_holder(log.context_id).unwrap(), Some((console.context_id, amy)));
    assert_eq!(db.lineage_root(log.context_id).unwrap(), amy);
    // A top with no performer and no senpai has nobody above it.
    let bare = senpai_ctx_row(&db, ws_id, "bare", None, None);
    let text = db.route_review(ReviewQuery::ask(bare.context_id, coder)).unwrap_err().to_string();
    assert!(text.contains("has no senpai above it") || text.contains("nobody reviews"), "{text}");
}

/// An archived senpai is climbed through (it still answers for what was
/// done under it); a missing one, or a missing context, is corruption.
#[test]
fn route_review_walks_through_archived_and_refuses_a_missing_senpai() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
    let seat = senpai_ctx_row(&db, ws_id, "archived-seat", None, Some(banto));
    let lane = senpai_ctx_row(&db, ws_id, "archived-seat-lane", Some(&seat), Some(coder));
    db.archive_context(seat.context_id).unwrap();
    assert_eq!(
        db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(),
        EffectiveReviewer::Assigned(banto),
        "an archived senpai is still the one who answers for what was done under it",
    );

    let orphan_senpai = ContextId::new();
    db.conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    db.conn.execute(
        "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
        params![blob_param(orphan_senpai.as_bytes()), blob_param(lane.context_id.as_bytes())],
    ).unwrap();
    db.conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    let error = db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap_err();
    assert!(matches!(error, KernelDbError::NotFound(_)), "{error}");

    let missing = db.route_review(ReviewQuery::ask(ContextId::new(), amy)).unwrap_err();
    assert!(matches!(missing, KernelDbError::NotFound(_)), "{missing}");
}

/// A senpai chain that loops is corruption; the climb stops with a named
/// error instead of looping.
#[test]
fn route_review_refuses_a_senpai_cycle() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder] = seed_characters(&db, &["coder"])[..] else { unreachable!() };
    let a = senpai_ctx_row(&db, ws_id, "cycle-a", None, Some(coder));
    let b = senpai_ctx_row(&db, ws_id, "cycle-b", Some(&a), Some(coder));
    db.conn.execute(
        "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
        params![blob_param(b.context_id.as_bytes()), blob_param(a.context_id.as_bytes())],
    ).unwrap();
    let error = db.route_review(ReviewQuery::ask(a.context_id, coder)).unwrap_err();
    assert!(matches!(error, KernelDbError::Corrupt(_)), "a cycle is corruption, not a refusal: {error}");
    assert!(error.to_string().contains("cycle"), "{error}");
}

/// The senpai edge is the only route. `forked_from`, the director and the
/// reviewer override on the row no longer decide anything.
#[test]
fn route_review_ignores_forked_from_and_the_old_routing_columns() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, judge, amy] = seed_characters(&db, &["coder", "banto", "judge", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let seat = senpai_ctx_row(&db, ws_id, "banto-seat", Some(&root), Some(banto));
    let judge_seat = senpai_ctx_row(&db, ws_id, "judge-seat", Some(&root), Some(judge));

    let mut lane = make_context_row(Some("routed-lane"));
    lane.forked_from = Some(seat.context_id);
    insert_context_with_doc(&db, &lane, ws_id);
    db.update_performer(lane.context_id, Some(coder)).unwrap();
    db.set_context_senpai(lane.context_id, Some(judge_seat.context_id)).unwrap();

    assert_eq!(
        db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(),
        EffectiveReviewer::Assigned(judge),
        "the senpai's holder, not the fork parent's",
    );
}

/// A create routes before its context exists: `ReviewFrom::Senpai`
/// starts at the senpai the new context will name.
#[test]
fn route_review_can_start_at_the_senpai_of_a_context_not_yet_written() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&root), Some(banto));

    assert_eq!(db.route_review(ReviewQuery::under(seat.context_id, coder)).unwrap(), EffectiveReviewer::Assigned(banto));
    assert_eq!(db.route_review(ReviewQuery::under(seat.context_id, banto)).unwrap(), EffectiveReviewer::Assigned(amy));
    assert_eq!(db.route_review(ReviewQuery::under(root.context_id, amy)).unwrap(), EffectiveReviewer::SelfConfirmation(amy));
    assert!(matches!(
        db.route_review(ReviewQuery::under(ContextId::new(), coder)).unwrap_err(),
        KernelDbError::NotFound(_)
    ));
}

/// Escalation runs the same climb past the current reviewer, one step at
/// a time, and refuses at a context with no senpai, naming it.
#[test]
fn route_review_above_climbs_one_step_and_refuses_at_the_top() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "escalate-root", None, Some(amy));
    let seat = senpai_ctx_row(&db, ws_id, "escalate-seat", Some(&root), Some(banto));
    let lane = senpai_ctx_row(&db, ws_id, "escalate-lane", Some(&seat), Some(coder));

    let ask = ReviewQuery::ask(lane.context_id, coder);
    assert_eq!(db.route_review(ask).unwrap(), EffectiveReviewer::Assigned(banto));
    assert_eq!(
        db.route_review(ask.above(banto)).unwrap(),
        EffectiveReviewer::Assigned(amy),
        "escalation climbs one senpai further",
    );
    let error = db.route_review(ask.above(amy)).unwrap_err();
    assert!(matches!(error, KernelDbError::NoReviewerAbove(_)), "{error}");
    assert!(error.to_string().contains("escalate-root"), "names the top context: {error}");
    // Even a root actor does not confirm itself on the way up.
    let error = db.route_review(ReviewQuery::ask(root.context_id, amy).above(amy)).unwrap_err();
    assert!(matches!(error, KernelDbError::NoReviewerAbove(_)), "{error}");
}

/// The lineage root is the root character holding the top of a context's
/// senpai chain. A chain that tops out anywhere else has no authority to
/// change routing, and says so.
#[test]
fn lineage_root_requires_a_live_root_character_at_the_top() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [banto, coder, amy] = seed_characters(&db, &["banto", "coder", "amy"])[..] else { unreachable!() };

    let seat = senpai_ctx_row(&db, ws_id, "unrooted-seat", None, Some(banto));
    let lane = senpai_ctx_row(&db, ws_id, "unrooted-lane", Some(&seat), Some(coder));
    let error = db.lineage_root(lane.context_id).unwrap_err();
    assert!(error.to_string().contains("banto"), "names the character at the top: {error}");
    assert!(error.to_string().contains("not a root"), "{error}");

    let bare = senpai_ctx_row(&db, ws_id, "nobody-top", None, None);
    assert!(db.lineage_root(bare.context_id).unwrap_err().to_string().contains("not a root console"));

    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    assert!(db.retire_character(amy, 1).unwrap());
    assert!(db.lineage_root(root.context_id).unwrap_err().to_string().contains("retired"));

    assert!(matches!(db.lineage_root(ContextId::new()).unwrap_err(), KernelDbError::NotFound(_)));
}

/// The lineage root follows the senpai chain, not `forked_from`.
#[test]
fn lineage_root_follows_senpai_not_forked_from() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [banto, coder, amy, bob] = seed_characters(&db, &["banto", "coder", "amy", "bob"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    db.update_character_root(bob, true).unwrap();
    let amy_root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let bob_root = senpai_ctx_row(&db, ws_id, "bob", None, Some(bob));
    let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&amy_root), Some(banto));
    let mut lane = make_context_row(Some("forked-from-amy-answers-to-bob"));
    lane.forked_from = Some(seat.context_id);
    insert_context_with_doc(&db, &lane, ws_id);
    db.update_performer(lane.context_id, Some(coder)).unwrap();
    db.set_context_senpai(lane.context_id, Some(bob_root.context_id)).unwrap();

    assert_eq!(db.lineage_root(lane.context_id).unwrap(), bob);
    assert_eq!(db.lineage_top(lane.context_id).unwrap().0, bob_root.context_id);
    assert_eq!(db.lineage_root(seat.context_id).unwrap(), amy);
}

/// `lineage_top` names the top context itself and asks nothing of its
/// character: a top held by a non-root still names a top.
#[test]
fn lineage_top_names_the_top_regardless_of_its_character() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [banto, coder] = seed_characters(&db, &["banto", "coder"])[..] else { unreachable!() };

    let seat = senpai_ctx_row(&db, ws_id, "unrooted-seat", None, Some(banto));
    let lane = senpai_ctx_row(&db, ws_id, "unrooted-lane", Some(&seat), Some(coder));
    assert!(db.lineage_root(lane.context_id).is_err());
    assert_eq!(db.lineage_top(lane.context_id).unwrap().0, seat.context_id);
    assert_eq!(db.lineage_top(seat.context_id).unwrap().0, seat.context_id, "a top names itself");
    assert!(matches!(db.lineage_top(ContextId::new()).unwrap_err(), KernelDbError::NotFound(_)), "a context with no row has no lineage");
}

/// A senpai cycle is corruption; the lineage walk stops with a named
/// error instead of looping, and so does the live variant.
#[test]
fn lineage_root_refuses_a_senpai_cycle() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [filler] = seed_characters(&db, &["filler"])[..] else { unreachable!() };
    let a = senpai_ctx_row(&db, ws_id, "cycle-a", None, Some(filler));
    let b = senpai_ctx_row(&db, ws_id, "cycle-b", Some(&a), Some(filler));
    db.conn.execute(
        "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
        params![blob_param(b.context_id.as_bytes()), blob_param(a.context_id.as_bytes())],
    ).unwrap();
    for error in [
        db.lineage_top(a.context_id).unwrap_err(),
        db.live_lineage_root(a.context_id).unwrap_err(),
    ] {
        assert!(matches!(error, KernelDbError::Validation(_)), "{error}");
        assert!(error.to_string().contains("cycle"), "{error}");
    }
}

/// `live_lineage_root` separates a chain with no live root (`None`) from a fault.
#[test]
fn live_lineage_root_is_none_without_a_root_and_an_error_on_a_fault() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [banto, amy] = seed_characters(&db, &["banto", "amy"])[..] else { unreachable!() };

    let bare = senpai_ctx_row(&db, ws_id, "nobody-top", None, None);
    assert_eq!(db.live_lineage_root(bare.context_id).unwrap(), None, "no holder");
    let seat = senpai_ctx_row(&db, ws_id, "not-root", None, Some(banto));
    assert_eq!(db.live_lineage_root(seat.context_id).unwrap(), None, "a non-root at the top");

    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    assert_eq!(db.live_lineage_root(root.context_id).unwrap(), Some(amy));
    assert!(db.retire_character(amy, 1).unwrap());
    assert_eq!(db.live_lineage_root(root.context_id).unwrap(), None, "a retired root");
    assert_eq!(db.live_lineage_root(ContextId::new()).unwrap(), None, "an ask may outlive its context");
}

/// A senpai pointer to a row that is not there is corruption, not a
/// reason to stop climbing.
#[test]
fn lineage_top_refuses_a_dangling_senpai() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [filler] = seed_characters(&db, &["filler"])[..] else { unreachable!() };
    let lane = senpai_ctx_row(&db, ws_id, "dangling-lane", None, Some(filler));
    db.conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    db.conn.execute(
        "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
        params![blob_param(ContextId::new().as_bytes()), blob_param(lane.context_id.as_bytes())],
    ).unwrap();
    db.conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    let error = db.lineage_top(lane.context_id).unwrap_err();
    assert!(matches!(error, KernelDbError::NotFound(_)), "{error}");
}

/// Moving a context under a new senpai refuses a self edge, a cycle, a
/// missing senpai and a context with no context character; `None` makes
/// it a top.
#[test]
fn set_context_senpai_moves_a_kohai_and_refuses_cycles_and_missing_rows() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [filler] = seed_characters(&db, &["filler"])[..] else { unreachable!() };
    let a = senpai_ctx_row(&db, ws_id, "mv-a", None, Some(filler));
    let b = senpai_ctx_row(&db, ws_id, "mv-b", Some(&a), Some(filler));
    let c = senpai_ctx_row(&db, ws_id, "mv-c", Some(&b), Some(filler));
    let d = senpai_ctx_row(&db, ws_id, "mv-d", None, Some(filler));
    let edge = |ctx: &ContextRow| db.senpai_of(ctx.context_id).unwrap();

    db.set_context_senpai(c.context_id, Some(d.context_id)).unwrap();
    assert_eq!(edge(&c), Some(d.context_id));
    assert_eq!(db.senpai_of(c.context_id).unwrap(), Some(d.context_id));
    let top = db.set_context_senpai(c.context_id, None).unwrap_err().to_string();
    assert!(top.contains("root console"), "only a root console has no senpai: {top}");
    assert_eq!(edge(&c), Some(d.context_id), "the refusal wrote nothing");
    db.set_context_senpai(c.context_id, Some(b.context_id)).unwrap();

    let own = db.set_context_senpai(a.context_id, Some(a.context_id)).unwrap_err().to_string();
    assert!(own.contains("mv-a") && own.contains("own senpai"), "{own}");
    let cycle = db.set_context_senpai(a.context_id, Some(c.context_id)).unwrap_err().to_string();
    assert!(cycle.contains("cycle") && cycle.contains("mv-a") && cycle.contains("mv-c"), "{cycle}");
    assert_eq!(edge(&a), None, "a refused move changes nothing");
    let missing = db.set_context_senpai(a.context_id, Some(ContextId::new())).unwrap_err();
    assert!(matches!(missing, KernelDbError::NotFound(_)), "{missing}");
    let no_row = db.set_context_senpai(ContextId::new(), Some(a.context_id)).unwrap_err();
    assert!(matches!(no_row, KernelDbError::NotFound(_)), "{no_row}");
    let bare = senpai_ctx_row(&db, ws_id, "mv-bare", None, None);
    db.set_context_senpai(bare.context_id, Some(a.context_id)).expect("a context with no performer answers to a senpai too");
    assert_eq!(edge(&bare), Some(a.context_id));
}

/// A rotation moves no kohai: they keep pointing at the archived
/// predecessor. It repoints a handoff log's pointer, and only that
/// character's.
#[test]
fn commit_rotation_leaves_the_kohai_and_repoints_a_handoff_log() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [owner, other, kohai_character] = seed_characters(&db, &["owner", "other", "kohai"])[..] else { unreachable!() };
    let top = senpai_ctx_row(&db, ws_id, "rot-top", None, Some(owner));
    let old = senpai_ctx_row(&db, ws_id, "rot-old", Some(&top), Some(other));
    let kohai = senpai_ctx_row(&db, ws_id, "rot-kohai", Some(&old), Some(kohai_character));
    let new = senpai_ctx_row(&db, ws_id, "rot-new", None, None);
    db.write_context_character(new.context_id, other, Some(top.context_id), None).unwrap();

    let handoff = senpai_ctx_row(&db, ws_id, "rot-handoff", None, None);
    let handoff_next = senpai_ctx_row(&db, ws_id, "rot-handoff-next", None, None);
    db.set_character_handoff_ctx(owner, Some(handoff.context_id)).unwrap();

    assert_eq!(db.commit_rotation(old.context_id, new.context_id, None).unwrap().handoffs_moved, 0, "no handoff log pointed at it");
    assert_eq!(db.senpai_of(kohai.context_id).unwrap(), Some(old.context_id), "the kohai still answer to the predecessor");
    assert!(db.list_kohai(new.context_id).unwrap().is_empty(), "the successor has none until it takes them");
    assert!(db.get_context(old.context_id).unwrap().unwrap().is_archived());

    assert_eq!(db.commit_rotation(handoff.context_id, handoff_next.context_id, None).unwrap().handoffs_moved, 1);
    assert_eq!(db.get_character(owner).unwrap().unwrap().handoff_ctx, Some(handoff_next.context_id));
}


// ── 22d2. the one authority rule ────────────────────────────────────

/// amy's console → banto's seat → two lanes. Over a lane, banto (who
/// holds its senpai) and amy (its lineage root) have authority for every
/// act the rule governs; the sibling lane's performer and the lane's own
/// performer do not, and the refusal names the rule, the holder and the
/// root.
#[test]
fn authority_rule_allows_the_senpai_holder_and_the_lineage_root_only() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, sibling, banto, amy] = seed_characters(&db, &["coder", "sibling", "banto", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&root), Some(banto));
    let lane = senpai_ctx_row(&db, ws_id, "lane", Some(&seat), Some(coder));
    let _other = senpai_ctx_row(&db, ws_id, "other-lane", Some(&seat), Some(sibling));

    for act in GovernedAct::ALL {
        assert_eq!(db.context_authority(lane.context_id, banto, act).unwrap(), Ok(Authority::SenpaiHolder), "{act:?}");
        assert_eq!(db.context_authority(lane.context_id, amy, act).unwrap(), Ok(Authority::LineageRoot), "{act:?}");
        // The lane's own performer archives it (`a_contexts_own_performer_may_archive_it_and_nothing_else`).
        let refused_actors: Vec<PrincipalId> = if act == GovernedAct::Archive { vec![sibling] } else { vec![sibling, coder] };
        for refused in refused_actors {
            let why = db.context_authority(lane.context_id, refused, act).unwrap().unwrap_err();
            let own = if act == GovernedAct::Archive { ", its own performer (coder)," } else { "" };
            assert_eq!(why, format!(
                "only lane ({})'s senpai (banto){own} or its lineage root (amy) may {}",
                lane.context_id.short(), act.as_str()
            ));
        }
    }
    // Over the seat, amy holds the senpai and is also the root.
    assert_eq!(db.context_authority(seat.context_id, amy, GovernedAct::Move).unwrap(), Ok(Authority::SenpaiHolder));
    assert!(db.context_authority(seat.context_id, banto, GovernedAct::Move).unwrap().is_err(), "a seat has no authority over itself");
}

/// A context with no performer answers to a senpai like any other: the
/// performer of its senpai (past senpais with no performer) or the lineage
/// root archives it by the one rule, and nobody else does.
#[test]
fn a_context_with_no_performer_is_archived_by_its_senpai_or_the_lineage_root() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [amy, banto, coder] = seed_characters(&db, &["amy", "banto", "coder"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let console = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let seat = senpai_ctx_row(&db, ws_id, "seat", Some(&console), Some(banto));
    let hub = senpai_ctx_row(&db, ws_id, "hub", Some(&seat), None);
    let log = senpai_ctx_row(&db, ws_id, "log", Some(&hub), None);
    for act in GovernedAct::ALL {
        assert_eq!(db.context_authority(log.context_id, banto, act).unwrap(), Ok(Authority::SenpaiHolder), "{act:?}: climbs past the hub to banto");
        assert_eq!(db.context_authority(log.context_id, amy, act).unwrap(), Ok(Authority::LineageRoot), "{act:?}");
        assert!(db.context_authority(log.context_id, coder, act).unwrap().is_err(), "{act:?}");
    }
}

/// A context's own performer archives it, and no other governed act is
/// open to it; the senpai's performer and the lineage root keep every act.
#[test]
fn a_contexts_own_performer_may_archive_it_and_nothing_else() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [amy, coder, sibling] = seed_characters(&db, &["amy", "coder", "sibling"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let console = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    let lane = senpai_ctx_row(&db, ws_id, "lane", Some(&console), Some(coder));
    assert_eq!(db.context_authority(lane.context_id, coder, GovernedAct::Archive).unwrap(), Ok(Authority::OwnPerformer));
    for act in GovernedAct::ALL.into_iter().filter(|act| *act != GovernedAct::Archive) {
        assert!(db.context_authority(lane.context_id, coder, act).unwrap().is_err(), "{act:?}");
    }
    assert!(db.context_authority(lane.context_id, sibling, GovernedAct::Archive).unwrap().is_err());
}

/// A senpai is written through one check: a NULL outside a root console is
/// refused by the Rust guard and, past it, by the database.
#[test]
fn a_null_senpai_outside_a_root_console_is_refused() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [amy, coder] = seed_characters(&db, &["amy", "coder"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let console = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
    db.set_character_root_ctx(amy, Some(console.context_id)).unwrap();
    let lane = senpai_ctx_row(&db, ws_id, "lane", Some(&console), Some(coder));

    // The Rust guard.
    let error = db.set_context_senpai(lane.context_id, None).unwrap_err().to_string();
    assert!(error.contains("only a root console has no senpai"), "{error}");
    let error = db.check_senpai_edge(lane.context_id, Some(coder), None).unwrap_err().to_string();
    assert!(error.contains("needs a senpai"), "{error}");
    let error = db.check_senpai_edge(lane.context_id, None, None).unwrap_err().to_string();
    assert!(error.contains("needs a senpai"), "a context with no performer has one: {error}");
    assert!(db.check_senpai_edge(console.context_id, Some(amy), None).is_ok(), "a root's console has none");
    let mut bare = make_context_row(Some("bare"));
    bare.performer = Some(coder);
    let error = db.insert_placed_context(&bare, ws_id).unwrap_err().to_string();
    assert!(error.contains("needs a senpai"), "{error}");
    // The rule reads the performer: a context a root character performs may
    // be written with no senpai, to become its root console when the same
    // transaction sets `root_ctx`.
    let mut person = make_context_row(Some("person-top"));
    person.performer = Some(amy);
    db.insert_placed_context(&person, ws_id).expect("a root character's context may have no senpai");
    // The trigger, under the guard.
    let error = db
        .conn
        .execute("UPDATE contexts SET senpai_ctx = NULL WHERE context_id = ?1", params![blob_param(lane.context_id.as_bytes())])
        .unwrap_err()
        .to_string();
    assert!(error.contains("only a root console has no senpai"), "{error}");
    assert_eq!(db.senpai_of(lane.context_id).unwrap(), Some(console.context_id), "the refusals changed nothing");
    // The console itself may be written to NULL again.
    db.conn
        .execute("UPDATE contexts SET senpai_ctx = NULL WHERE context_id = ?1", params![blob_param(console.context_id.as_bytes())])
        .unwrap();
}

/// A context with no senpai and a lineage with no live root still answer
/// with the rule's text, naming what is missing; neither is a fault.
#[test]
fn authority_rule_names_a_missing_senpai_and_a_missing_root() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto] = seed_characters(&db, &["coder", "banto"])[..] else { unreachable!() };
    let top = senpai_ctx_row(&db, ws_id, "top", None, Some(banto));
    let lane = senpai_ctx_row(&db, ws_id, "lane", Some(&top), Some(coder));

    let why = db.context_authority(top.context_id, coder, GovernedAct::Archive).unwrap().unwrap_err();
    assert!(
        why.starts_with(&format!("only top ({})'s senpai (none), its own performer (banto), or its lineage root (none:", top.context_id.short())),
        "{why}"
    );
    assert!(why.ends_with("may archive it"), "{why}");
    assert_eq!(db.context_authority(lane.context_id, banto, GovernedAct::Archive).unwrap(), Ok(Authority::SenpaiHolder),
        "holding the senpai is enough when the lineage has no root");
}


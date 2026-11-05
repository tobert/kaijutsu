use super::*;

/// Insert `row` answering to the kernel's root console, as every live
/// context but a root console does.
fn insert_kohai_with_doc(db: &KernelDb, row: &ContextRow, ws_id: WorkspaceId) {
    insert_context_with_doc(db, row, ws_id);
    let (_, console) = db.seed_test_root_console().unwrap();
    db.set_context_senpai(row.context_id, Some(console)).unwrap();
}

#[test]
fn conclude_context_sets_state_timestamp_and_is_idempotent() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("worky"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    // Fresh context: open (no concluded_at, Live).
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.concluded_at, None);
    assert_eq!(loaded.context_state, ContextState::Live);

    // First conclude: newly concluded → true, stamps state + timestamp.
    assert!(db.conclude_context(cid).unwrap());
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.context_state, ContextState::Concluded);
    let stamp = loaded.concluded_at.expect("concluded_at set");
    assert!(stamp > 0);

    // Idempotent: re-conclude → false, original timestamp preserved.
    assert!(!db.conclude_context(cid).unwrap());
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.concluded_at, Some(stamp));

    // Concluded is NOT archived — still visible to list_active_contexts.
    let active = db.list_active_contexts().unwrap();
    assert_eq!(active.len(), 1, "concluded context stays active (not hidden)");
}

#[test]
fn conclude_context_rejects_archived() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("gone"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    assert!(db.archive_context(cid).unwrap());
    // Archived contexts can't be concluded (guard in the UPDATE).
    assert!(!db.conclude_context(cid).unwrap());
}

#[test]
fn promote_context_stamps_and_is_append_stable() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("candidate"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.promoted_at, None);

    assert_eq!(
        db.promote_context(cid).unwrap(),
        PromoteOutcome::Promoted,
        "first promote is new"
    );
    let loaded = db.get_context(cid).unwrap().unwrap();
    let stamp = loaded.promoted_at.expect("promoted_at set");
    assert!(stamp > 0);

    // Re-promote: no-op, original timestamp preserved (append-stable).
    assert_eq!(
        db.promote_context(cid).unwrap(),
        PromoteOutcome::AlreadyPromoted,
        "re-promote is a no-op"
    );
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.promoted_at, Some(stamp));
}

#[test]
fn promote_clears_demoted_and_demote_clears_promoted() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("seesaw"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    // Promote → demoted_at stays clear.
    assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert!(loaded.promoted_at.is_some());
    assert_eq!(loaded.demoted_at, None);

    // Demote a promoted context: one step in from ring 0 → unpromoted,
    // NOT straight to demoted (each demote call is one step outward).
    assert_eq!(
        db.demote_context(cid).unwrap(),
        DemoteOutcome::Unpromoted
    );
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.promoted_at, None);
    assert_eq!(loaded.demoted_at, None);

    // Demote again from neutral → demoted.
    assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Demoted);
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert!(loaded.demoted_at.is_some());

    // Promote a demoted context → clears demoted_at.
    assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert!(loaded.promoted_at.is_some());
    assert_eq!(loaded.demoted_at, None);
}

#[test]
fn demote_ladder_ends_archived() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("ladder"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);

    // promoted → unpromoted (back to automatic placement)
    assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Unpromoted);
    // neither → demoted
    assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Demoted);
    // already demoted → archived (one step further outward)
    assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Archived);

    let loaded = db.get_context(cid).unwrap().unwrap();
    assert!(loaded.archived_at.is_some());

    // Further demote on an archived context is a loud error, not a
    // silent no-op — nothing further out to step to.
    let err = db.demote_context(cid).unwrap_err();
    assert!(matches!(err, KernelDbError::Validation(_)));
}

#[test]
fn conclude_context_clears_promoted_seat() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("mux-exit"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
    assert!(db.conclude_context(cid).unwrap());

    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.promoted_at, None, "conclude gives up the ring-0 seat");
}

#[test]
fn archive_context_clears_promoted_and_demoted() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("archived-seat"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
    assert!(db.archive_context(cid).unwrap());

    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.promoted_at, None);
    assert_eq!(loaded.demoted_at, None);
}

#[test]
fn set_context_paused_roundtrip() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("pausable"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.paused_at, None);

    db.set_context_paused(cid, true).unwrap();
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert!(loaded.paused_at.is_some());

    db.set_context_paused(cid, false).unwrap();
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.paused_at, None);
}

#[test]
fn active_ring_cap_refuses_an_eleventh_seat_until_a_demote() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let mut ids = Vec::new();
    for i in 0..ACTIVE_RING_CAPACITY {
        let row = make_context_row(Some(&format!("seat-{i}")));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);
        assert_eq!(
            db.promote_context(cid).unwrap(),
            PromoteOutcome::Promoted,
            "seat {i} should promote"
        );
        ids.push(cid);
    }

    // 11th promote: ring full, fails loudly.
    let eleventh = make_context_row(Some("overflow"));
    let eleventh_id = eleventh.context_id;
    insert_context_with_doc(&db, &eleventh, ws_id);
    let err = db.promote_context(eleventh_id).unwrap_err();
    assert!(matches!(err, KernelDbError::Validation(_)), "err: {err:?}");

    // Demote one seat, freeing capacity for the 11th.
    assert_eq!(
        db.demote_context(ids[0]).unwrap(),
        DemoteOutcome::Unpromoted
    );
    assert_eq!(
        db.promote_context(eleventh_id).unwrap(),
        PromoteOutcome::Promoted,
        "11th promote should now succeed"
    );
}

#[test]
fn promote_resurrects_an_archived_context() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("lazarus"));
    let cid = row.context_id;
    insert_kohai_with_doc(&db, &row, ws_id);

    assert!(db.archive_context(cid).unwrap());

    // Promote is the resurrection door: unarchives + seats in one act.
    assert_eq!(
        db.promote_context(cid).unwrap(),
        PromoteOutcome::Resurrected { concluded: false }
    );
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.archived_at, None, "resurrection clears archived_at");
    assert!(loaded.promoted_at.is_some(), "resurrection takes a seat");
    assert_eq!(loaded.demoted_at, None);

    // Back among the living: visible to list_active_contexts again.
    let active = db.list_active_contexts().unwrap();
    assert!(active.iter().any(|c| c.context_id == cid));
}

#[test]
fn promote_resurrects_a_concluded_context_without_unconcluding() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("done-but-back"));
    let cid = row.context_id;
    insert_kohai_with_doc(&db, &row, ws_id);

    assert!(db.conclude_context(cid).unwrap());
    let concluded_stamp = db.get_context(cid).unwrap().unwrap().concluded_at.unwrap();
    assert!(db.archive_context(cid).unwrap());

    // Placement is orthogonal to conclusion: "concluded but seated for
    // review" is a legit state — resurrection must NOT clear concluded_at.
    assert_eq!(
        db.promote_context(cid).unwrap(),
        PromoteOutcome::Resurrected { concluded: true }
    );
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(loaded.archived_at, None);
    assert_eq!(loaded.concluded_at, Some(concluded_stamp));
    assert!(loaded.promoted_at.is_some());
}

#[test]
fn ring_full_resurrection_errors_and_stays_archived() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    for i in 0..ACTIVE_RING_CAPACITY {
        let row = make_context_row(Some(&format!("full-seat-{i}")));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);
        assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
    }

    let buried = make_context_row(Some("stays-buried"));
    let buried_id = buried.context_id;
    insert_context_with_doc(&db, &buried, ws_id);
    assert!(db.archive_context(buried_id).unwrap());

    // Resurrection needs a free seat exactly like any promote: full ring
    // → the same loud error, and NO partial write — the context stays
    // archived.
    let err = db.promote_context(buried_id).unwrap_err();
    assert!(matches!(err, KernelDbError::Validation(_)), "err: {err:?}");
    let loaded = db.get_context(buried_id).unwrap().unwrap();
    assert!(loaded.archived_at.is_some(), "failed resurrection must not unarchive");
    assert_eq!(loaded.promoted_at, None);
}

#[test]
fn resurrection_stamps_a_fresh_promoted_at() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("stale-seat"));
    let cid = row.context_id;
    insert_kohai_with_doc(&db, &row, ws_id);

    // Defensive shape `archive_context` can't produce (it clears
    // promoted_at): an archived row still carrying an old seat stamp.
    // Resurrection must stamp FRESH — the old seat was surrendered at
    // archive time; a stale stamp would jump the append order.
    let stale_stamp: i64 = 12_345;
    db.conn
        .execute(
            "UPDATE contexts SET archived_at = ?1, promoted_at = ?2 WHERE context_id = ?3",
            params![99_999_i64, stale_stamp, blob_param(cid.as_bytes())],
        )
        .unwrap();

    assert_eq!(
        db.promote_context(cid).unwrap(),
        PromoteOutcome::Resurrected { concluded: false }
    );
    let loaded = db.get_context(cid).unwrap().unwrap();
    let fresh = loaded.promoted_at.expect("seated");
    assert!(
        fresh > stale_stamp,
        "resurrection must restamp promoted_at fresh (got {fresh}, stale was {stale_stamp})"
    );
}

/// The ladder checks `demoted_at` FIRST, so a corrupt both-stamps row
/// (a shape the kernel's mutual-exclusion writes can't produce) agrees
/// with the app layout's demoted-wins rule: one demote archives it,
/// instead of no-op-looping through unpromote while the stale demoted
/// stamp keeps it visually in ring 3.
#[test]
fn demote_on_a_corrupt_both_stamps_row_archives() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("both-stamps"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    db.conn
        .execute(
            "UPDATE contexts SET promoted_at = ?1, demoted_at = ?2 WHERE context_id = ?3",
            params![1_000_i64, 2_000_i64, blob_param(cid.as_bytes())],
        )
        .unwrap();

    assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Archived);
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert!(loaded.archived_at.is_some());
}

#[test]
fn set_context_paused_rejects_archived_and_unknown() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("buried-napper"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);
    assert!(db.archive_context(cid).unwrap());

    // Archived: loud Validation, symmetric with demote's guard.
    let err = db.set_context_paused(cid, true).unwrap_err();
    assert!(matches!(err, KernelDbError::Validation(_)), "err: {err:?}");

    // Unknown: still NotFound, not conflated with the archived case.
    let err = db.set_context_paused(ContextId::new(), true).unwrap_err();
    assert!(matches!(err, KernelDbError::NotFound(_)), "err: {err:?}");
}

/// `paused_at` is orthogonal to ring placement and lifecycle: promote,
/// demote, and conclude must all leave it alone.
#[test]
fn paused_at_survives_promote_demote_conclude() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("sleeper"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    db.set_context_paused(cid, true).unwrap();
    let stamp = db.get_context(cid).unwrap().unwrap().paused_at.unwrap();

    assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
    assert_eq!(db.get_context(cid).unwrap().unwrap().paused_at, Some(stamp));

    assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Unpromoted);
    assert_eq!(db.get_context(cid).unwrap().unwrap().paused_at, Some(stamp));

    assert!(db.conclude_context(cid).unwrap());
    assert_eq!(db.get_context(cid).unwrap().unwrap().paused_at, Some(stamp));
}

/// Conclude clears the ring-0 seat but deliberately preserves
/// `demoted_at`: explicit demotion is sticky through conclusion, so a
/// concluded-then-demoted (or demoted-then-concluded) context stays in
/// the demoted ring instead of floating back to the auto pool.
#[test]
fn conclude_preserves_demoted_at() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("demoted-then-done"));
    let cid = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);

    assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Demoted);
    let stamp = db.get_context(cid).unwrap().unwrap().demoted_at.unwrap();

    assert!(db.conclude_context(cid).unwrap());
    let loaded = db.get_context(cid).unwrap().unwrap();
    assert_eq!(
        loaded.demoted_at,
        Some(stamp),
        "conclude must not clear demoted_at — explicit demotion is sticky"
    );
}

#[test]
fn archiving_sets_the_state_as_well_as_the_timestamp() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("to-archive"));
    insert_context_with_doc(&db, &row, ws_id);

    assert!(db.archive_context(row.context_id).unwrap());

    let loaded = db.get_context(row.context_id).unwrap().unwrap();
    assert_eq!(loaded.context_state, ContextState::Archived);
    assert!(loaded.archived_at.is_some());
    assert!(loaded.is_archived());
}

/// Resurrection derives the restored state from the timestamps that
/// survived the archive, rather than remembering one.
#[test]
fn resurrecting_restores_live_or_concluded_from_the_timestamps() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let plain = make_context_row(Some("plain"));
    insert_kohai_with_doc(&db, &plain, ws_id);
    db.archive_context(plain.context_id).unwrap();
    db.promote_context(plain.context_id).unwrap();
    let loaded = db.get_context(plain.context_id).unwrap().unwrap();
    assert_eq!(loaded.context_state, ContextState::Live);
    assert!(loaded.archived_at.is_none());
    assert!(!loaded.is_archived());

    let done = make_context_row(Some("done"));
    insert_kohai_with_doc(&db, &done, ws_id);
    db.conclude_context(done.context_id).unwrap();
    db.archive_context(done.context_id).unwrap();
    assert_eq!(
        db.get_context(done.context_id).unwrap().unwrap().context_state,
        ContextState::Archived,
        "archiving a concluded context reads as archived — the outer state wins"
    );
    db.promote_context(done.context_id).unwrap();
    let loaded = db.get_context(done.context_id).unwrap().unwrap();
    assert_eq!(
        loaded.context_state,
        ContextState::Concluded,
        "concluded_at survived the archive, so the restored state is concluded"
    );
}

/// An archived context with no senpai stays archived: promoting it would make
/// a live context that answers to no one. A root console resurrects.
#[test]
fn promoting_an_archived_context_with_no_senpai_refuses() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let (_, console) = db.seed_test_root_console().unwrap();
    let bare = make_context_row(Some("bare"));
    insert_context_with_doc(&db, &bare, ws_id);
    db.archive_context(bare.context_id).unwrap();

    let error = db.promote_context(bare.context_id).unwrap_err().to_string();
    assert!(error.contains("has no senpai"), "{error}");
    assert!(db.get_context(bare.context_id).unwrap().unwrap().is_archived(), "it stays archived");

    // A root console whose `root_ctx` was cleared, as retirement clears it,
    // is no root console once archived, so it too has no senpai to come
    // back under.
    let (amy, _) = db.seed_test_root_console().unwrap();
    db.set_character_root_ctx(amy, None).unwrap();
    db.archive_context(console).unwrap();
    let error = db.promote_context(console).unwrap_err().to_string();
    assert!(error.contains("has no senpai"), "{error}");
}

/// No archive path archives a root console: `archive_context` and the
/// demote ladder's last step refuse it inside the write, naming `kj context
/// rotate`, and the console stays live with its `root_ctx`. Falsified by
/// dropping the root console check from `archive_one_context`.
#[test]
fn archiving_a_root_console_refuses_inside_the_write() {
    let db = KernelDb::temporary().unwrap();
    setup_test_db(&db);
    let (amy, console) = db.seed_test_root_console().unwrap();
    let error = db.archive_context(console).unwrap_err().to_string();
    assert!(error.contains("root console") && error.contains("kj context rotate"), "{error}");
    assert_eq!(db.get_character(amy).unwrap().unwrap().root_ctx, Some(console), "the refusal keeps root_ctx");
    assert!(!db.get_context(console).unwrap().unwrap().is_archived());

    assert_eq!(db.demote_context(console).unwrap(), DemoteOutcome::Demoted);
    let error = db.demote_context(console).unwrap_err().to_string();
    assert!(error.contains("root console"), "the demote ladder refuses too: {error}");
    let row = db.get_context(console).unwrap().unwrap();
    assert!(!row.is_archived() && row.demoted_at.is_some(), "the refused step changes nothing");
    assert_eq!(db.get_character(amy).unwrap().unwrap().root_ctx, Some(console));
}

/// The demote write takes the step its caller decided. A step decided as a
/// demote does not archive a context another demote reached first, and a
/// step decided as the archive does not demote a context that was not
/// demoted. Falsified by deciding the step from the row alone.
#[test]
fn a_demote_refuses_a_step_that_changed_after_its_caller_decided_it() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("raced"));
    let cid = row.context_id;
    insert_kohai_with_doc(&db, &row, ws_id);

    let error = db.demote_context_with_shadows(cid, None, true).unwrap_err().to_string();
    assert!(error.contains("kj context demote"), "{error}");
    assert_eq!(db.get_context(cid).unwrap().unwrap().demoted_at, None, "a stale archive decision demotes nothing");

    assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Demoted);
    let error = db.demote_context_with_shadows(cid, None, false).unwrap_err().to_string();
    assert!(error.contains("kj context demote") && error.contains("--confirm"), "{error}");
    assert!(!db.get_context(cid).unwrap().unwrap().is_archived(), "a stale demote decision archives nothing");

    let (outcome, _) = db.demote_context_with_shadows(cid, None, true).unwrap();
    assert_eq!(outcome, DemoteOutcome::Archived);
}

/// An MCP hub with a live session refuses its archive inside the write,
/// naming the session, so a session made after a verb's check is not left
/// without its hub. Once the session is archived, the hub archives and its
/// pointer clears. Falsified by dropping the hub check from
/// `archive_one_context`.
#[test]
fn archiving_a_hub_with_a_live_session_refuses_inside_the_write() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let (amy, _) = db.seed_test_root_console().unwrap();
    let hub = make_context_row(Some("mcp/amy"));
    insert_kohai_with_doc(&db, &hub, ws_id);
    db.set_character_mcp_hub_ctx(amy, Some(hub.context_id)).unwrap();
    let mut session = make_context_row(Some("session"));
    session.forked_from = Some(hub.context_id);
    insert_kohai_with_doc(&db, &session, ws_id);

    let error = db.archive_context(hub.context_id).unwrap_err().to_string();
    assert!(error.contains("MCP hub") && error.contains("session"), "{error}");
    assert!(!db.get_context(hub.context_id).unwrap().unwrap().is_archived());
    assert_eq!(db.get_character(amy).unwrap().unwrap().mcp_hub_ctx, Some(hub.context_id));

    assert!(db.archive_context(session.context_id).unwrap());
    assert!(db.archive_context(hub.context_id).unwrap());
    assert_eq!(db.get_character(amy).unwrap().unwrap().mcp_hub_ctx, None, "the archive clears the pointer");
}

use super::*;

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
    insert_context_with_doc(&db, &row, ws_id);

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
    insert_context_with_doc(&db, &row, ws_id);

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
    insert_context_with_doc(&db, &row, ws_id);

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
    insert_context_with_doc(&db, &plain, ws_id);
    db.archive_context(plain.context_id).unwrap();
    db.promote_context(plain.context_id).unwrap();
    let loaded = db.get_context(plain.context_id).unwrap().unwrap();
    assert_eq!(loaded.context_state, ContextState::Live);
    assert!(loaded.archived_at.is_none());
    assert!(!loaded.is_archived());

    let done = make_context_row(Some("done"));
    insert_context_with_doc(&db, &done, ws_id);
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

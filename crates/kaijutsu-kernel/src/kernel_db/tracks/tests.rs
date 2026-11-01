use super::*;

// ── Tracks CRUD ───────────────────────────────────────────────────

#[test]
fn track_upsert_get_round_trip() {
    let db = KernelDb::temporary().unwrap();
    let t = make_track("bass", 250);
    db.upsert_track(&t).unwrap();
    assert_eq!(db.get_track("bass").unwrap(), Some(t), "track survives write→read");
}

#[test]
fn track_absent_is_none() {
    let db = KernelDb::temporary().unwrap();
    assert!(
        db.get_track("no-such-track").unwrap().is_none(),
        "absent track → None"
    );
}

#[test]
fn get_track_empty_id_is_loud_error() {
    let db = KernelDb::temporary().unwrap();
    assert!(
        matches!(db.get_track(""), Err(KernelDbError::Validation(_))),
        "empty track_id is corrupt → Validation error, not a silent None"
    );
}

#[test]
fn get_track_zero_period_is_loud_error() {
    // A zero period is a beat that never advances — corrupt stored state.
    let db = KernelDb::temporary().unwrap();
    db.conn
        .execute(
            "INSERT INTO tracks (track_id, period_ms, beats_per_phrase)
             VALUES ('bad', 0, 16)",
            [],
        )
        .unwrap();
    assert!(
        matches!(db.get_track("bad"), Err(KernelDbError::Validation(_))),
        "a 0 period_ms is corrupt → loud Validation error"
    );
}

#[test]
fn get_track_negative_field_is_loud_error() {
    // A negative INTEGER written into SQLite (tampering / bit rot) must not
    // silently cast to a huge u64. Reject it loudly.
    let db = KernelDb::temporary().unwrap();
    db.conn
        .execute(
            "INSERT INTO tracks (track_id, period_ms, beats_per_phrase)
             VALUES ('neg', -1, 16)",
            [],
        )
        .unwrap();
    assert!(
        matches!(db.get_track("neg"), Err(KernelDbError::Validation(_))),
        "a negative period_ms is corrupt → loud Validation error"
    );
}

#[test]
fn list_tracks_rejects_a_corrupt_row_like_get_track() {
    // `list_tracks` must run the SAME numeric validation as `get_track` — a
    // corrupt row (here a negative period) must surface as a loud Validation
    // error, never a nonsense `u64::MAX` cadence cast silently into the roster.
    let db = KernelDb::temporary().unwrap();
    db.upsert_track(&make_track("good", 500)).unwrap();
    db.conn
        .execute(
            "INSERT INTO tracks (track_id, period_ms, beats_per_phrase)
             VALUES ('rotten', -1, 16)",
            [],
        )
        .unwrap();
    assert!(
        matches!(db.list_tracks(), Err(KernelDbError::Validation(_))),
        "a corrupt row must make list_tracks refuse loudly, not wrap to u64::MAX"
    );
}

#[test]
fn track_upsert_updates_in_place() {
    // A tempo change updates one row, not a second — PK is track_id.
    let db = KernelDb::temporary().unwrap();
    db.upsert_track(&make_track("bass", 500)).unwrap();
    db.upsert_track(&make_track("bass", 250)).unwrap(); // tempo up
    assert_eq!(
        db.get_track("bass").unwrap().unwrap().period_ms,
        250,
        "the second upsert overwrites the first"
    );
    assert_eq!(
        db.list_tracks().unwrap().len(),
        1,
        "one upsert, not two rows"
    );
}

#[test]
fn track_list_and_delete() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    db.upsert_track(&make_track("bass", 250)).unwrap();
    db.upsert_track(&make_track("lead", 500)).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 2, "two tracks");

    // Attach a context to bass so we can verify cascade.
    let ctx = make_context_row(Some("ctx-cascade"));
    insert_context_with_doc(&db, &ctx, ws_id);
    db.upsert_attachment(&make_attachment("bass", ctx.context_id)).unwrap();
    assert_eq!(
        db.list_attachments_for_track("bass").unwrap().len(),
        1,
        "attachment present before delete"
    );

    // Delete bass — should cascade to the attachment.
    assert_eq!(db.delete_track("bass").unwrap(), 1, "one row deleted");
    assert_eq!(db.list_tracks().unwrap().len(), 1, "lead still present");
    assert_eq!(
        db.list_attachments_for_track("bass").unwrap().len(),
        0,
        "attachment cascade-deleted with the track"
    );
}


// ── tombstone (kj transport delete) ─────────────────────────────────

#[test]
fn tombstone_track_renames_aside_and_hides_from_reads() {
    // The round trip `kj transport delete` depends on: the row is RENAMED
    // (not deleted), `deleted_at` is stamped, `score_context_id` survives
    // untouched (the durable recovery payload), and the tombstoned row is
    // invisible to both `list_tracks` and a `get_track` lookup by the
    // ORIGINAL name (so re-attaching that name starts fresh).
    let mut db = KernelDb::temporary().unwrap();
    let score = ContextId::new();
    db.upsert_track(&PersistedTrack { score_context_id: Some(score), ..make_track("bass", 250) })
        .unwrap();
    db.upsert_track(&make_track("lead", 500)).unwrap();

    let tombstone_name = db.tombstone_track("bass").unwrap();
    assert!(
        tombstone_name.starts_with("bass~tombstone-"),
        "tombstone name carries the original id: {tombstone_name}"
    );

    // Hidden from both read paths under the original name.
    assert!(
        db.get_track("bass").unwrap().is_none(),
        "get_track(original name) sees nothing — the PK moved"
    );
    let live = db.list_tracks().unwrap();
    assert_eq!(live.len(), 1, "list_tracks shows only the untouched sibling");
    assert_eq!(live[0].track_id, "lead");

    // The row survives under its new name, with score_context_id intact —
    // the durable recovery payload — and deleted_at stamped.
    let deleted_at: i64 = db
        .conn
        .query_row(
            "SELECT deleted_at FROM tracks WHERE track_id = ?1",
            params![tombstone_name],
            |row| row.get(0),
        )
        .unwrap();
    assert!(deleted_at > 0, "deleted_at is stamped");
    assert!(
        tombstone_name.ends_with(&deleted_at.to_string()),
        "the suffix and deleted_at come from the SAME clock read: {tombstone_name} vs {deleted_at}"
    );
    let score_blob: Vec<u8> = db
        .conn
        .query_row(
            "SELECT score_context_id FROM tracks WHERE track_id = ?1",
            params![tombstone_name],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        ContextId::try_from_slice(&score_blob),
        Some(score),
        "score_context_id survives the rename untouched — the recovery payload"
    );
}

#[test]
fn tombstone_track_refuses_with_dangling_attachments() {
    // The scheduler's delete path must detach + delete every attachment row
    // BEFORE tombstoning; if it didn't, renaming the track_id out from under
    // a live attachment row would leave that row pointing at a name that no
    // longer resolves. Refuse loudly rather than silently orphan it.
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.upsert_track(&make_track("bass", 250)).unwrap();
    let ctx = make_context_row(Some("dangling"));
    insert_context_with_doc(&db, &ctx, ws_id);
    db.upsert_attachment(&make_attachment("bass", ctx.context_id)).unwrap();

    let err = db.tombstone_track("bass").unwrap_err();
    assert!(
        matches!(err, KernelDbError::Validation(_)),
        "a dangling attachment refuses the tombstone: {err:?}"
    );
    // Untouched: still live under its original name.
    assert!(db.get_track("bass").unwrap().is_some(), "the track was NOT renamed");
}

#[test]
fn tombstone_track_unknown_track_is_not_found() {
    let mut db = KernelDb::temporary().unwrap();
    assert!(matches!(
        db.tombstone_track("no-such-track"),
        Err(KernelDbError::NotFound(_))
    ));
}

#[test]
fn track_playhead_and_playing_round_trip() {
    let db = KernelDb::temporary().unwrap();
    let t = PersistedTrack {
        playhead_tick: Some(1024),
        playing: true,
        ..make_track("bass", 250)
    };
    db.upsert_track(&t).unwrap();
    let got = db.get_track("bass").unwrap().unwrap();
    assert_eq!(got.playhead_tick, Some(1024));
    assert!(got.playing);
}

#[test]
fn track_score_context_round_trips_and_is_set_once() {
    let db = KernelDb::temporary().unwrap();
    let score = ContextId::new();
    // Created with a score context.
    db.upsert_track(&PersistedTrack {
        score_context_id: Some(score),
        ..make_track("bass", 250)
    })
    .unwrap();
    assert_eq!(db.get_track("bass").unwrap().unwrap().score_context_id, Some(score));

    // A later upsert that does NOT carry the score context (a tempo/playhead
    // change) must not wipe it — set-once.
    db.upsert_track(&PersistedTrack {
        playhead_tick: Some(512),
        score_context_id: None,
        ..make_track("bass", 250)
    })
    .unwrap();
    let got = db.get_track("bass").unwrap().unwrap();
    assert_eq!(got.playhead_tick, Some(512), "the upsert still updates mutable fields");
    assert_eq!(got.score_context_id, Some(score), "set-once: score context survived");

    // A pre-Stage-2 row (created without a score context) reads back as None.
    db.upsert_track(&make_track("drums", 250)).unwrap();
    assert_eq!(db.get_track("drums").unwrap().unwrap().score_context_id, None);
}

#[test]
fn track_clock_kind_round_trips_and_is_mutable() {
    // Unlike `score_context_id` (set-once), `clock_kind` is MUTABLE: a track
    // sketched on the system clock can be slaved to a MIDI master later
    // (docs/tracks.md Stage 3). The driver swap must survive an upsert.
    let db = KernelDb::temporary().unwrap();
    db.upsert_track(&make_track("bass", 250)).unwrap();
    assert_eq!(
        db.get_track("bass").unwrap().unwrap().clock_kind,
        "system",
        "default clock_kind round-trips"
    );

    // A tempo upsert that keeps the same kind preserves it.
    db.upsert_track(&PersistedTrack {
        period_ms: 300,
        ..make_track("bass", 0) // period_ms overridden above; 0 unused
    })
    .unwrap();
    assert_eq!(
        db.get_track("bass").unwrap().unwrap().clock_kind,
        "system",
        "a tempo upsert preserves the clock kind"
    );

    // A driver swap (system → modeled) is persisted — clock_kind is in the
    // ON CONFLICT UPDATE SET, the opposite of set-once score_context_id.
    db.upsert_track(&PersistedTrack {
        clock_kind: "modeled".to_string(),
        ..make_track("bass", 250)
    })
    .unwrap();
    assert_eq!(
        db.get_track("bass").unwrap().unwrap().clock_kind,
        "modeled",
        "a driver swap updates clock_kind (MUTABLE, not set-once)"
    );
}

#[test]
fn track_clock_kind_defaults_to_system_on_a_pre_stage3_row() {
    // A row inserted without the `clock_kind` column (the pre-Stage-3 shape an
    // additive ALTER backfills) reads back as 'system' via the column DEFAULT,
    // never NULL — `get_track`'s `String` read would fail loud on NULL.
    let db = KernelDb::temporary().unwrap();
    db.conn
        .execute(
            "INSERT INTO tracks (track_id, period_ms, beats_per_phrase)
             VALUES ('legacy', 500, 16)",
            [],
        )
        .unwrap();
    assert_eq!(
        db.get_track("legacy").unwrap().unwrap().clock_kind,
        "system",
        "a pre-Stage-3 row backfills to the 'system' default"
    );
}


// ── Attachments CRUD ──────────────────────────────────────────────

#[test]
fn attachment_upsert_get_round_trip() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.upsert_track(&make_track("bass", 250)).unwrap();
    let ctx = make_context_row(Some("att-rt"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let a = PersistedAttachment {
        track_id: "bass".into(),
        context_id: ctx.context_id,
        wakeup_every: 8,
        rotate_every_phrases: Some(4),
        ooda_armed: true,
    };
    db.upsert_attachment(&a).unwrap();
    assert_eq!(
        db.get_attachment("bass", ctx.context_id).unwrap(),
        Some(a),
        "all fields survive write→read"
    );
}

#[test]
fn attachment_absent_is_none() {
    let db = KernelDb::temporary().unwrap();
    db.upsert_track(&make_track("bass", 250)).unwrap();
    assert!(
        db.get_attachment("bass", ContextId::new()).unwrap().is_none(),
        "absent attachment → None"
    );
}

#[test]
fn attachment_upsert_updates_in_place() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.upsert_track(&make_track("bass", 250)).unwrap();
    let ctx = make_context_row(Some("att-up"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.upsert_attachment(&PersistedAttachment {
        wakeup_every: 1,
        ..make_attachment("bass", ctx.context_id)
    })
    .unwrap();
    db.upsert_attachment(&PersistedAttachment {
        wakeup_every: 8,
        ..make_attachment("bass", ctx.context_id)
    })
    .unwrap();
    assert_eq!(
        db.get_attachment("bass", ctx.context_id).unwrap().unwrap().wakeup_every,
        8,
        "the second upsert overwrites the first (in-place update)"
    );
}

#[test]
fn list_attachments_for_track() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.upsert_track(&make_track("bass", 250)).unwrap();
    db.upsert_track(&make_track("lead", 500)).unwrap();

    let ctx1 = make_context_row(Some("att-list-1"));
    let ctx2 = make_context_row(Some("att-list-2"));
    let ctx3 = make_context_row(Some("att-list-3"));
    insert_context_with_doc(&db, &ctx1, ws_id);
    insert_context_with_doc(&db, &ctx2, ws_id);
    insert_context_with_doc(&db, &ctx3, ws_id);

    db.upsert_attachment(&make_attachment("bass", ctx1.context_id)).unwrap();
    db.upsert_attachment(&make_attachment("bass", ctx2.context_id)).unwrap();
    db.upsert_attachment(&make_attachment("lead", ctx3.context_id)).unwrap();

    let bass_atts = db.list_attachments_for_track("bass").unwrap();
    assert_eq!(bass_atts.len(), 2, "two bass attachments");
    assert!(bass_atts.iter().all(|a| a.track_id == "bass"));

    let lead_atts = db.list_attachments_for_track("lead").unwrap();
    assert_eq!(lead_atts.len(), 1, "one lead attachment");
}

#[test]
fn list_attachments_for_context() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.upsert_track(&make_track("bass", 250)).unwrap();
    db.upsert_track(&make_track("lead", 500)).unwrap();

    let ctx = make_context_row(Some("att-ctx-list"));
    let other = make_context_row(Some("att-ctx-other"));
    insert_context_with_doc(&db, &ctx, ws_id);
    insert_context_with_doc(&db, &other, ws_id);

    db.upsert_attachment(&make_attachment("bass", ctx.context_id)).unwrap();
    db.upsert_attachment(&make_attachment("lead", ctx.context_id)).unwrap();
    db.upsert_attachment(&make_attachment("bass", other.context_id)).unwrap();

    let ctx_atts = db.list_attachments_for_context(ctx.context_id).unwrap();
    assert_eq!(ctx_atts.len(), 2, "ctx attached to two tracks");
    assert!(ctx_atts.iter().all(|a| a.context_id == ctx.context_id));
}

#[test]
fn delete_attachment() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.upsert_track(&make_track("bass", 250)).unwrap();
    let ctx = make_context_row(Some("att-del"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.upsert_attachment(&make_attachment("bass", ctx.context_id)).unwrap();
    assert_eq!(
        db.delete_attachment("bass", ctx.context_id).unwrap(),
        1,
        "one row deleted"
    );
    assert!(
        db.get_attachment("bass", ctx.context_id).unwrap().is_none(),
        "attachment gone after delete"
    );
    assert_eq!(
        db.delete_attachment("bass", ctx.context_id).unwrap(),
        0,
        "second delete is a no-op"
    );
}


#[test]
fn attachments_travel_with_a_fork() {
    // A forked musician must join the SAME tracks as the parent — the track
    // is the durable clock domain identity across the fork-lineage (the
    // rotation page-turn: docs/tracks.md §3). A thin spawn-fork has no label,
    // so without this copy the child would have no track to re-bind on.
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.upsert_track(&make_track("bass", 250)).unwrap();
    db.upsert_track(&make_track("lead", 500)).unwrap();

    let parent = make_context_row(Some("musician-parent"));
    insert_context_with_doc(&db, &parent, ws_id);

    // Parent is attached to two tracks with different wakeup divisors.
    db.upsert_attachment(&PersistedAttachment {
        wakeup_every: 8,
        rotate_every_phrases: Some(4),
        ooda_armed: true,
        ..make_attachment("bass", parent.context_id)
    })
    .unwrap();
    db.upsert_attachment(&PersistedAttachment {
        wakeup_every: 64,
        ..make_attachment("lead", parent.context_id)
    })
    .unwrap();

    // Fork: labelless thin child (spawn shape).
    let mut child = make_context_row(None);
    child.forked_from = Some(parent.context_id);
    db.insert_forked_context(&child, ws_id, parent.context_id, None).unwrap();

    // Child inherits both attachments with unchanged track_ids.
    let bass_att = db.get_attachment("bass", child.context_id).unwrap().unwrap();
    assert_eq!(bass_att.track_id, "bass");
    assert_eq!(bass_att.wakeup_every, 8);
    assert_eq!(bass_att.rotate_every_phrases, Some(4));
    assert!(bass_att.ooda_armed, "ooda_armed copied from parent");

    let lead_att = db.get_attachment("lead", child.context_id).unwrap().unwrap();
    assert_eq!(lead_att.track_id, "lead");
    assert_eq!(lead_att.wakeup_every, 64);
}

#[test]
fn fork_of_a_non_musician_copies_no_attachments() {
    // A non-musician parent has no attachments; the fork copy is a clean no-op
    // (does not error, does not leave stale rows behind).
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let parent = make_context_row(Some("coder"));
    insert_context_with_doc(&db, &parent, ws_id);

    let mut child = make_context_row(Some("coder-child"));
    child.forked_from = Some(parent.context_id);
    db.insert_forked_context(&child, ws_id, parent.context_id, None).unwrap();

    assert!(
        db.list_attachments_for_context(child.context_id).unwrap().is_empty(),
        "no parent attachments → none copied (forks of non-musicians are unaffected)"
    );
}

#[test]
fn copy_attachments_for_fork_in_transaction() {
    // Directly tests the tx-level primitive with two source attachments.
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.upsert_track(&make_track("bass", 250)).unwrap();
    db.upsert_track(&make_track("lead", 500)).unwrap();

    let source = make_context_row(Some("src-ctx"));
    let child_row = make_context_row(Some("child-ctx"));
    insert_context_with_doc(&db, &source, ws_id);
    insert_context_with_doc(&db, &child_row, ws_id);

    db.upsert_attachment(&PersistedAttachment {
        wakeup_every: 4,
        ..make_attachment("bass", source.context_id)
    })
    .unwrap();
    db.upsert_attachment(&PersistedAttachment {
        wakeup_every: 16,
        rotate_every_phrases: Some(2),
        ..make_attachment("lead", source.context_id)
    })
    .unwrap();

    // Run the copy inside a transaction, just like insert_forked_context does.
    let tx = db.conn.transaction().unwrap();
    KernelDb::copy_attachments_for_fork(&tx, source.context_id, child_row.context_id).unwrap();
    tx.commit().unwrap();

    // Child now has the same two attachments, keyed on child's context_id.
    let bass = db.get_attachment("bass", child_row.context_id).unwrap().unwrap();
    assert_eq!(bass.wakeup_every, 4);
    let lead = db.get_attachment("lead", child_row.context_id).unwrap().unwrap();
    assert_eq!(lead.wakeup_every, 16);
    assert_eq!(lead.rotate_every_phrases, Some(2));

    // Source rows are untouched.
    assert!(db.get_attachment("bass", source.context_id).unwrap().is_some());
    assert!(db.get_attachment("lead", source.context_id).unwrap().is_some());
}

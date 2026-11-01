use super::*;

// ── Claude cache breakpoints ──────────────────────────────────────

#[test]
fn cache_breakpoints_empty_for_unknown_context() {
    let db = KernelDb::temporary().unwrap();
    let bps = db.list_cache_breakpoints(ContextId::new()).unwrap();
    assert!(bps.is_empty());
}

#[test]
fn cache_breakpoints_add_and_list_round_trip_all_variants() {
    // Round-trip every CacheTarget variant plus both CacheTtl values
    // — the table schema is right only if every shape survives a
    // write+read.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-rt"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Extended))
        .unwrap();
    db.add_cache_breakpoint(ctx.context_id, &CacheTarget::System(CacheTtl::Ephemeral))
        .unwrap();
    db.add_cache_breakpoint(
        ctx.context_id,
        &CacheTarget::MessageIndex(7, CacheTtl::Extended),
    )
    .unwrap();

    let bps = db.list_cache_breakpoints(ctx.context_id).unwrap();
    assert_eq!(bps.len(), 3);
    assert_eq!(bps[0], CacheTarget::Tools(CacheTtl::Extended));
    assert_eq!(bps[1], CacheTarget::System(CacheTtl::Ephemeral));
    assert_eq!(bps[2], CacheTarget::MessageIndex(7, CacheTtl::Extended));
}

#[test]
fn hydration_policy_unset_is_none() {
    // No row → None → hydrate everything (the default for every context).
    let db = KernelDb::temporary().unwrap();
    assert!(db.get_hydration_policy(ContextId::new()).unwrap().is_none());
}

#[test]
fn hydration_policy_set_get_round_trip() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("hydra-rt"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let marker = BlockId::new(ctx.context_id, PrincipalId::new(), 4);
    db.set_hydration_policy(ctx.context_id, marker, 12).unwrap();

    let got = db.get_hydration_policy(ctx.context_id).unwrap();
    assert_eq!(got, Some((marker, 12)), "marker + window survive write→read");
}

#[test]
fn hydration_policy_upsert_advances_marker_in_place() {
    // Advancing the marker (a durable revision) is a single in-place upsert,
    // not a second row — the PK is context_id.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("hydra-up"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let p1 = BlockId::new(ctx.context_id, PrincipalId::new(), 2);
    let p2 = BlockId::new(ctx.context_id, PrincipalId::new(), 9);
    db.set_hydration_policy(ctx.context_id, p1, 8).unwrap();
    db.set_hydration_policy(ctx.context_id, p2, 16).unwrap();

    assert_eq!(
        db.get_hydration_policy(ctx.context_id).unwrap(),
        Some((p2, 16)),
        "the second set overwrites the first (in-place advance)"
    );
}

#[test]
fn hydration_policy_zero_window_is_loud_error() {
    // A window of 0 (corrupt row, or a hand-written DB edit) would window to
    // prefix-only and drop the current turn from the wire. Corrupt config is
    // a LOUD failure (the caller fails the turn), not a silent degrade to
    // hydrate-everything that disables the cost guard.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("hydra-zero"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let marker = BlockId::new(ctx.context_id, PrincipalId::new(), 3);
    db.set_hydration_policy(ctx.context_id, marker, 0).unwrap();
    assert!(
        matches!(
            db.get_hydration_policy(ctx.context_id),
            Err(KernelDbError::Validation(_))
        ),
        "a 0 window is corrupt → loud Validation error, not silent None"
    );
}

#[test]
fn hydration_policy_unparseable_marker_is_loud_error() {
    // A stored marker that no longer parses is corruption in this one row.
    // Same stance as a bad window: refuse loudly, don't silently hydrate all.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("hydra-badmark"));
    insert_context_with_doc(&db, &ctx, ws_id);

    // Write a malformed marker straight into the row (bypass the typed setter).
    db.conn
        .execute(
            "INSERT INTO context_hydration (context_id, marker, window_size)
             VALUES (?1, ?2, ?3)",
            params![blob_param(ctx.context_id.as_bytes()), "not-a-block-id", 8],
        )
        .unwrap();
    assert!(
        matches!(
            db.get_hydration_policy(ctx.context_id),
            Err(KernelDbError::Validation(_))
        ),
        "an unparseable marker is corrupt → loud Validation error"
    );
}

#[test]
fn hydration_policy_clear_reverts_to_none() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("hydra-clear"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let marker = BlockId::new(ctx.context_id, PrincipalId::new(), 1);
    db.set_hydration_policy(ctx.context_id, marker, 4).unwrap();
    assert_eq!(db.clear_hydration_policy(ctx.context_id).unwrap(), 1);
    assert!(
        db.get_hydration_policy(ctx.context_id).unwrap().is_none(),
        "cleared policy reverts to hydrate-everything"
    );
}


#[test]
fn cache_breakpoints_preserve_declaration_order_via_seq() {
    // Insertion order must be preserved — populators (rc scripts)
    // rely on this for the wire-layer's first-write-wins dedupe to
    // produce predictable results.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-order"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let seq0 = db
        .add_cache_breakpoint(
            ctx.context_id,
            &CacheTarget::MessageIndex(0, CacheTtl::Ephemeral),
        )
        .unwrap();
    let seq1 = db
        .add_cache_breakpoint(
            ctx.context_id,
            &CacheTarget::MessageIndex(5, CacheTtl::Ephemeral),
        )
        .unwrap();
    let seq2 = db
        .add_cache_breakpoint(
            ctx.context_id,
            &CacheTarget::MessageIndex(2, CacheTtl::Ephemeral),
        )
        .unwrap();
    assert_eq!(seq0, 0);
    assert_eq!(seq1, 1);
    assert_eq!(seq2, 2);

    let bps = db.list_cache_breakpoints(ctx.context_id).unwrap();
    assert_eq!(
        bps,
        vec![
            CacheTarget::MessageIndex(0, CacheTtl::Ephemeral),
            CacheTarget::MessageIndex(5, CacheTtl::Ephemeral),
            CacheTarget::MessageIndex(2, CacheTtl::Ephemeral),
        ],
        "list must return breakpoints in insertion order, not sorted by index"
    );
}

#[test]
fn cache_breakpoints_clear_returns_count_and_empties_list() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-clear"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
        .unwrap();
    db.add_cache_breakpoint(ctx.context_id, &CacheTarget::System(CacheTtl::Ephemeral))
        .unwrap();

    assert_eq!(db.clear_cache_breakpoints(ctx.context_id).unwrap(), 2);
    assert!(db.list_cache_breakpoints(ctx.context_id).unwrap().is_empty());

    // Idempotent — clearing again returns 0.
    assert_eq!(db.clear_cache_breakpoints(ctx.context_id).unwrap(), 0);
}

#[test]
fn cache_breakpoints_resume_seq_after_clear() {
    // After clearing, the next add should start back at seq=0.
    // This matters for rc-on-drift scripts that clear-then-rebuild
    // — we don't want sequence numbers to grow unboundedly.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-resume"));
    insert_context_with_doc(&db, &ctx, ws_id);

    for _ in 0..3 {
        db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
            .unwrap();
    }
    db.clear_cache_breakpoints(ctx.context_id).unwrap();

    let new_seq = db
        .add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
        .unwrap();
    assert_eq!(new_seq, 0, "seq restarts at 0 after a full clear");
}

#[test]
fn cache_breakpoints_cascade_delete_on_context() {
    // Context deletion must remove all of its breakpoints (via the
    // FK ON DELETE CASCADE through contexts -> documents).
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-cascade"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
        .unwrap();
    db.add_cache_breakpoint(
        ctx.context_id,
        &CacheTarget::MessageIndex(3, CacheTtl::Extended),
    )
    .unwrap();

    db.conn.execute("DELETE FROM contexts WHERE context_id = ?1",
        params![blob_param(ctx.context_id.as_bytes())]).unwrap();

    assert!(
        db.list_cache_breakpoints(ctx.context_id).unwrap().is_empty(),
        "context delete must cascade to cache_breakpoints"
    );
}

#[test]
fn cache_breakpoints_storage_does_not_enforce_4_cap() {
    // Storage is liberal — populators may add more than 4. The wire
    // layer applies the cap. This test pins the policy choice.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-nocap"));
    insert_context_with_doc(&db, &ctx, ws_id);

    for i in 0..6 {
        db.add_cache_breakpoint(
            ctx.context_id,
            &CacheTarget::MessageIndex(i, CacheTtl::Ephemeral),
        )
        .unwrap();
    }
    assert_eq!(db.list_cache_breakpoints(ctx.context_id).unwrap().len(), 6);
}

#[test]
fn cache_breakpoints_decode_drops_unrecognized_kind() {
    // Forwards-compat: an unknown target_kind in the DB (e.g. from a
    // future schema variant downgraded to this binary) must be
    // silently skipped, not panic the read path.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-future"));
    insert_context_with_doc(&db, &ctx, ws_id);

    // Insert one good row and one row with a kind this binary doesn't know.
    db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO cache_breakpoints
                 (context_id, seq, target_kind, target_index, ttl)
             VALUES (?1, 999, 'image_blocks', 4, 'ephemeral')",
            params![blob_param(ctx.context_id.as_bytes())],
        )
        .unwrap();

    let bps = db.list_cache_breakpoints(ctx.context_id).unwrap();
    assert_eq!(bps.len(), 1, "unknown kind silently dropped");
    assert_eq!(bps[0], CacheTarget::Tools(CacheTtl::Ephemeral));
}

#[test]
fn cache_breakpoints_decode_drops_unrecognized_ttl() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-future-ttl"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.conn
        .execute(
            "INSERT INTO cache_breakpoints
                 (context_id, seq, target_kind, target_index, ttl)
             VALUES (?1, 0, 'tools', NULL, 'eternal')",
            params![blob_param(ctx.context_id.as_bytes())],
        )
        .unwrap();

    assert!(db.list_cache_breakpoints(ctx.context_id).unwrap().is_empty());
}

#[test]
fn cache_breakpoints_message_index_zero_round_trips() {
    // Edge case the doc explicitly cares about: rc-on-fork drops
    // MessageIndex(fork_at - 1); when fork_at is 1, that's
    // MessageIndex(0). Make sure index 0 doesn't get confused with
    // SQL NULL or default fallback.
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-idx0"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.add_cache_breakpoint(
        ctx.context_id,
        &CacheTarget::MessageIndex(0, CacheTtl::Extended),
    )
    .unwrap();
    let bps = db.list_cache_breakpoints(ctx.context_id).unwrap();
    assert_eq!(bps, vec![CacheTarget::MessageIndex(0, CacheTtl::Extended)]);
}

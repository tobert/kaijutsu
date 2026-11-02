use super::*;

#[test]
fn publication_handoffs_gain_abandonment_on_existing_databases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        db.conn.execute_batch("ALTER TABLE approval_pair_handoffs DROP COLUMN abandoned_reason").unwrap();
    }
    let db = KernelDb::open(&path).unwrap();
    assert!(db.approval_pair_abandoned_reason("missing").unwrap().is_none());
    drop(db);
    assert!(KernelDb::open(&path).unwrap().unpublished_approval_pairs().unwrap().is_empty());
}

fn pending_ask_for_context(db: &KernelDb, context: ContextId) -> String {
    let principal = PrincipalId::new();
    db.create_approval_ask(&approval_ledger::types::NewAsk {
        context_id: context.as_bytes().to_vec(),
        actor_id: principal.as_bytes().to_vec(),
        reviewer_id: PrincipalId::new().as_bytes().to_vec(),
        principal_id: principal.as_bytes().to_vec(),
        origin: approval_ledger::types::Origin::KjVerb,
        instance: None,
        tool: None,
        hook_id: None,
        description: "archive sweep test".into(),
        statements: vec![],
        authorized_label: None,
        rc_run_id: None,
        expires_at: None,
        options: vec![],
        signals: vec![],
        cwd: None,
        exec_source: None,
        exec_stdin: None,
        continuation_epoch: None,
        env: vec![],
    }, false).unwrap()
}

fn approval_status(db: &KernelDb, request: &str) -> approval_ledger::types::ApprovalStatus {
    db.get_approval(request).unwrap().expect("ask exists").status
}

#[test]
fn archive_context_rolls_back_when_its_ask_sweep_fails() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("archive-sweep-rollback"));
    let context = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);
    let request = pending_ask_for_context(&db, context);
    db.conn.execute_batch(
        "CREATE TRIGGER reject_archive_ask_sweep BEFORE UPDATE OF status ON approvals
         WHEN NEW.status = 'abandoned'
         BEGIN SELECT RAISE(ABORT, 'injected archive ask sweep failure'); END;",
    ).unwrap();

    let error = db.archive_context(context).expect_err("the failed sweep must roll back archive");
    assert!(error.to_string().contains("injected archive ask sweep failure"), "{error}");
    let loaded = db.get_context(context).unwrap().unwrap();
    assert!(!loaded.is_archived(), "state change must roll back with its failed sweep");
    assert_eq!(approval_status(&db, &request), approval_ledger::types::ApprovalStatus::Pending);

    db.conn.execute("DROP TRIGGER reject_archive_ask_sweep", []).unwrap();
    assert!(db.archive_context(context).unwrap(), "retry archives after the sweep fault clears");
    assert_eq!(approval_status(&db, &request), approval_ledger::types::ApprovalStatus::Abandoned);
}

#[test]
fn archive_context_rolls_back_inside_a_committed_outer_transaction() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("archive-sweep-nested-rollback"));
    let context = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);
    let request = pending_ask_for_context(&db, context);
    db.conn.execute_batch(
        "CREATE TRIGGER reject_nested_archive_ask_sweep BEFORE UPDATE OF status ON approvals
         WHEN NEW.status = 'abandoned'
         BEGIN SELECT RAISE(ABORT, 'injected nested archive ask sweep failure'); END;",
    ).unwrap();

    db.in_transaction(|db| {
        assert!(db.archive_context(context).is_err(), "the nested archive must report its failed sweep");
        Ok(())
    }).unwrap();

    let loaded = db.get_context(context).unwrap().unwrap();
    assert!(!loaded.is_archived(), "the caught archive error must not leak its state change into the outer commit");
    assert_eq!(approval_status(&db, &request), approval_ledger::types::ApprovalStatus::Pending);
}

#[test]
fn archive_context_retries_the_sweep_for_an_already_archived_context() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("archive-sweep-heal"));
    let context = row.context_id;
    insert_context_with_doc(&db, &row, ws_id);
    let request = pending_ask_for_context(&db, context);
    db.conn.execute(
        "UPDATE contexts SET archived_at = ?1, context_state = 'archived' WHERE context_id = ?2",
        params![now_millis(), blob_param(context.as_bytes())],
    ).unwrap();

    assert!(!db.archive_context(context).unwrap(), "the historical state transition was already applied");
    assert_eq!(approval_status(&db, &request), approval_ledger::types::ApprovalStatus::Abandoned);
}

#[test]
fn continuation_invalidation_migrates_without_closing_existing_work() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("legacy-continuation"));
    insert_context_with_doc(&db, &row, ws_id);
    let origin = db.begin_continuation(row.context_id, 1_000).unwrap();
    db.conn.execute_batch("ALTER TABLE context_continuations DROP COLUMN invalidated_through").unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    assert_eq!(db.continuation_epoch(row.context_id).unwrap(), Some(origin.epoch));
    assert!(db.admit_inference_request(row.context_id, origin.epoch, 1_100).unwrap());
    assert!(!db.admit_inference_request(row.context_id, origin.epoch + 1, 1_100).unwrap());
}

/// A context's character is fixed by its context character, so a second
/// assignment of the same character keeps the continuation and another
/// character is refused without touching it.
#[test]
fn a_performer_that_cannot_change_keeps_the_continuation() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("reassigned-continuation"));
    insert_context_with_doc(&db, &row, ws_id);
    let actor = PrincipalId::new();
    let other = PrincipalId::new();
    db.update_played_by(row.context_id, Some(actor)).unwrap();
    let origin = db.begin_continuation(row.context_id, 1_000).unwrap();
    db.record_continuation_request(row.context_id, origin.epoch, 1_100).unwrap();
    db.record_continuation_yield(row.context_id, origin.epoch, 1_200).unwrap();
    db.update_played_by(row.context_id, Some(actor)).unwrap();
    assert!(db.automatic_resume_allowed(row.context_id, origin.epoch, 1_300, 600).unwrap());
    assert!(db.update_played_by(row.context_id, Some(other)).is_err());
    assert_eq!(db.get_context(row.context_id).unwrap().unwrap().performer, Some(actor));
    assert!(db.automatic_resume_allowed(row.context_id, origin.epoch, 1_300, 600).unwrap());
}

#[test]
fn continuation_window_requires_a_matching_yielded_unsigned_epoch() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("continuation"));
    insert_context_with_doc(&db, &row, ws_id);

    let origin = db.begin_continuation(row.context_id, 1_000).unwrap();
    assert_eq!(origin.epoch, 1);
    assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 1_100, 600).unwrap());

    db.record_continuation_request(row.context_id, origin.epoch, 1_500).unwrap();
    db.record_continuation_yield(row.context_id, origin.epoch, 1_200).unwrap();
    assert!(db.automatic_resume_allowed(row.context_id, origin.epoch, 2_100, 600).unwrap());
    assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 2_101, 600).unwrap());

    db.sign_off_continuation(row.context_id, 1_300).unwrap();
    assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 1_400, 600).unwrap());

    let next = db.begin_continuation(row.context_id, 2_000).unwrap();
    assert_eq!(next.epoch, 2);
    db.record_continuation_request(row.context_id, next.epoch, 2_150).unwrap();
    db.record_continuation_yield(row.context_id, next.epoch, 2_100).unwrap();
    assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 2_200, 600).unwrap());
    assert!(db.automatic_resume_allowed(row.context_id, next.epoch, 2_200, 600).unwrap());
    assert!(db.claim_automatic_resume(row.context_id, next.epoch, 2_200, 600).unwrap());
    assert!(!db.claim_automatic_resume(row.context_id, next.epoch, 2_200, 600).unwrap());
    db.record_continuation_yield(row.context_id, next.epoch, 2_300).unwrap();
    db.sign_off_continuation(row.context_id, 2_400).unwrap();
    assert!(!db.claim_automatic_resume(row.context_id, next.epoch, 2_500, 600).unwrap());
}

#[test]
fn interrupt_closes_a_yielded_continuation_independently_of_signoff() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("interrupted"));
    insert_context_with_doc(&db, &row, ws_id);
    let interrupter = PrincipalId::new();

    // No continuation open yet: nothing to interrupt.
    assert!(!db.record_continuation_interrupt(row.context_id, interrupter, 900).unwrap());
    assert_eq!(db.continuation_interrupt(row.context_id).unwrap(), None);

    let origin = db.begin_continuation(row.context_id, 1_000).unwrap();
    db.record_continuation_request(row.context_id, origin.epoch, 1_100).unwrap();
    db.record_continuation_yield(row.context_id, origin.epoch, 1_200).unwrap();
    assert!(db.automatic_resume_allowed(row.context_id, origin.epoch, 1_300, 600).unwrap());

    // Interrupt records who and when, and is idempotent.
    assert!(db.record_continuation_interrupt(row.context_id, interrupter, 1_250).unwrap());
    assert_eq!(db.continuation_interrupt(row.context_id).unwrap(), Some((1_250, interrupter)));
    assert!(!db.record_continuation_interrupt(row.context_id, PrincipalId::new(), 1_260).unwrap(),
        "a second interrupt on an already-interrupted epoch records nothing new");
    assert_eq!(db.continuation_interrupt(row.context_id).unwrap(), Some((1_250, interrupter)),
        "the second call must not overwrite who/when");

    // Interrupting closes automatic resume even though the epoch is
    // still yielded and within its window; signoff is untouched.
    assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 1_300, 600).unwrap());
    assert!(!db.claim_automatic_resume(row.context_id, origin.epoch, 1_300, 600).unwrap());
    assert!(db.sign_off_continuation(row.context_id, 1_400).unwrap(),
        "interrupting a continuation does not set signed_off_at");

    // Opening a new epoch (an explicit `kj drive`) clears the interrupt.
    let next = db.begin_continuation(row.context_id, 2_000).unwrap();
    assert_eq!(db.continuation_interrupt(row.context_id).unwrap(), None);
    db.record_continuation_request(row.context_id, next.epoch, 2_100).unwrap();
    db.record_continuation_yield(row.context_id, next.epoch, 2_200).unwrap();
    assert!(db.automatic_resume_allowed(row.context_id, next.epoch, 2_300, 600).unwrap());
}

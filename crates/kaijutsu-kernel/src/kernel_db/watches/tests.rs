use super::*;

fn live_context(db: &KernelDb) -> ContextId {
    let ws = setup_test_db(db);
    let row = make_context_row(None);
    insert_context_with_doc(db, &row, ws);
    row.context_id
}

/// Events coalesce into bursts: three events in one burst count once, and a
/// second burst counts again. Falsified by counting every raw event.
#[test]
fn events_in_one_burst_count_once() {
    let db = KernelDb::temporary().unwrap();
    let ctx = live_context(&db);
    db.add_path_watch(ctx, "/work/AGENTS.md", "/host/work/AGENTS.md", PrincipalId::new()).unwrap();
    db.record_path_event("/host/work/AGENTS.md", true, 10, true).unwrap();
    db.record_path_event("/host/work/AGENTS.md", true, 11, false).unwrap();
    db.record_path_event("/host/work/AGENTS.md", false, 12, false).unwrap();
    let pending = db.pending_watches(ctx).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!((pending[0].changes, pending[0].first_change_at, pending[0].last_change_at, pending[0].present),
        (1, Some(10), Some(12), Some(false)));
    db.record_path_event("/host/work/AGENTS.md", true, 900, true).unwrap();
    assert_eq!(db.pending_watches(ctx).unwrap()[0].changes, 2);
}

/// Delivery resets the status, and an event inside an old burst after a
/// delivery counts again. Falsified by a reset that keeps the count, or by an
/// in-burst event that a delivered watch drops.
#[test]
fn delivery_resets_and_a_later_event_counts_again() {
    let db = KernelDb::temporary().unwrap();
    let ctx = live_context(&db);
    let w = db.add_path_watch(ctx, "/w/a", "/h/a", PrincipalId::new()).unwrap();
    db.record_path_event("/h/a", true, 10, true).unwrap();
    db.mark_watches_delivered(&[(w.watch_id, 1)], 20).unwrap();
    assert!(db.pending_watches(ctx).unwrap().is_empty());
    let after = db.get_watch(ctx, w.watch_id).unwrap().unwrap();
    assert_eq!((after.changes, after.first_change_at, after.delivered_at), (0, None, Some(20)));
    db.record_path_event("/h/a", true, 21, false).unwrap();
    assert_eq!(db.pending_watches(ctx).unwrap()[0].changes, 1);
}

/// A delivery that rendered a stale count fails loudly instead of dropping
/// the newer change.
#[test]
fn a_stale_delivery_fails() {
    let db = KernelDb::temporary().unwrap();
    let ctx = live_context(&db);
    let w = db.add_path_watch(ctx, "/w/a", "/h/a", PrincipalId::new()).unwrap();
    db.record_path_event("/h/a", true, 10, true).unwrap();
    db.record_path_event("/h/a", true, 500, true).unwrap();
    assert!(db.mark_watches_delivered(&[(w.watch_id, 1)], 20).is_err());
    assert_eq!(db.pending_watches(ctx).unwrap()[0].changes, 2);
}

/// An archived context's watch neither accrues nor delivers, and its host
/// path leaves the watched set.
#[test]
fn an_archived_context_watch_is_inert() {
    let db = KernelDb::temporary().unwrap();
    let ctx = live_context(&db);
    db.add_path_watch(ctx, "/w/a", "/h/a", PrincipalId::new()).unwrap();
    assert_eq!(db.watched_host_paths().unwrap(), ["/h/a"]);
    db.archive_context(ctx).unwrap();
    assert_eq!(db.record_path_event("/h/a", true, 10, true).unwrap(), 0);
    assert!(db.pending_watches(ctx).unwrap().is_empty());
    assert!(db.watched_host_paths().unwrap().is_empty());
}

/// The successor gets the predecessor's targets with an empty status, and an
/// event after that reaches both live rows separately.
#[test]
fn copy_gives_the_successor_a_fresh_watch() {
    let db = KernelDb::temporary().unwrap();
    let old = live_context(&db);
    let new = live_context(&db);
    db.add_path_watch(old, "/w/a", "/h/a", PrincipalId::new()).unwrap();
    db.record_path_event("/h/a", true, 10, true).unwrap();
    db.copy_watches(old, new).unwrap();
    let copied = db.list_watches(new).unwrap();
    assert_eq!(copied.len(), 1);
    assert_eq!((copied[0].target.as_str(), copied[0].changes, copied[0].first_change_at), ("/w/a", 0, None));
    assert_eq!(db.list_watches(old).unwrap()[0].changes, 1, "the predecessor keeps its own row");
}

/// One target per context; another context's watch is not found by id.
#[test]
fn a_target_is_watched_once_per_context() {
    let db = KernelDb::temporary().unwrap();
    let a = live_context(&db);
    let b = live_context(&db);
    let w = db.add_path_watch(a, "/w/a", "/h/a", PrincipalId::new()).unwrap();
    let err = db.add_path_watch(a, "/w/a", "/h/a", PrincipalId::new()).unwrap_err().to_string();
    assert!(err.contains(&format!("watch {}", w.watch_id)), "{err}");
    db.add_path_watch(b, "/w/a", "/h/a", PrincipalId::new()).unwrap();
    assert!(db.get_watch(b, w.watch_id).unwrap().is_none());
    assert!(!db.remove_watch(b, w.watch_id).unwrap());
    assert!(db.remove_watch(a, w.watch_id).unwrap());
}

/// Promoting an archived context does not re-arm its watches: they stay
/// stopped until the path is added again, which starts the same watch with
/// an empty status.
#[test]
fn promote_leaves_watches_stopped_until_added_again() {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let senpai = make_context_row(None);
    insert_context_with_doc(&db, &senpai, ws);
    let mut row = make_context_row(None);
    row.senpai_ctx = Some(senpai.context_id);
    insert_context_with_doc(&db, &row, ws);
    let ctx = row.context_id;
    db.set_context_senpai(ctx, Some(senpai.context_id)).unwrap();
    let w = db.add_path_watch(ctx, "/w/a", "/h/a", PrincipalId::new()).unwrap();
    db.record_path_event("/h/a", true, 10, true).unwrap();
    db.archive_context(ctx).unwrap();
    db.promote_context(ctx).unwrap();

    assert!(db.get_watch(ctx, w.watch_id).unwrap().unwrap().stopped_at.is_some());
    assert_eq!(db.record_path_event("/h/a", true, 900, true).unwrap(), 0, "a stopped watch accrues nothing");
    assert!(db.pending_watches(ctx).unwrap().is_empty(), "a stopped watch delivers nothing");
    assert!(db.watched_host_paths().unwrap().is_empty());

    let again = db.add_path_watch(ctx, "/w/a", "/h/a", PrincipalId::new()).unwrap();
    assert_eq!((again.watch_id, again.stopped_at, again.changes), (w.watch_id, None, 0));
    db.record_path_event("/h/a", true, 2_000, true).unwrap();
    assert_eq!(db.pending_watches(ctx).unwrap().len(), 1);
}

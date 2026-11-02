use super::*;

/// A database from before the director column has `reviewer_id` but no
/// `director_id`. The open that added the director column cleared every
/// such reviewer, so the senpai migration maps none of them as an
/// override: the context takes its nearest ancestor and is not reported.
/// The drop then removes `reviewer_id`.
#[test]
fn a_reviewer_from_before_the_director_column_is_not_an_override() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, judge, amy] = seed_characters(&db, &["coder", "banto", "judge", "amy"])[..] else { unreachable!() };
    into_pre_context_character(&db);
    db.conn.execute("ALTER TABLE contexts ADD COLUMN reviewer_id BLOB", []).unwrap();
    let root = make_context_row(Some("amy"));
    insert_context_with_doc(&db, &root, ws_id);
    set_stored_played_by(&db, root.context_id, Some(amy));
    set_stored_root_ctx(&db, amy, root.context_id);
    let mut seat = make_context_row(Some("seat"));
    seat.forked_from = Some(root.context_id);
    insert_context_with_doc(&db, &seat, ws_id);
    set_stored_played_by(&db, seat.context_id, Some(banto));
    let mut judge_seat = make_context_row(Some("judge-seat"));
    judge_seat.forked_from = Some(root.context_id);
    insert_context_with_doc(&db, &judge_seat, ws_id);
    set_stored_played_by(&db, judge_seat.context_id, Some(judge));
    let mut lane = make_context_row(Some("lane"));
    lane.forked_from = Some(seat.context_id);
    insert_context_with_doc(&db, &lane, ws_id);
    set_stored_played_by(&db, lane.context_id, Some(coder));
    db.conn.execute(
        "UPDATE contexts SET reviewer_id = ?1 WHERE context_id = ?2",
        params![blob_param(judge.as_bytes()), blob_param(lane.context_id.as_bytes())],
    ).unwrap();
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SENPAI_MIGRATION]).unwrap();

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    assert_eq!(db.senpai_of(lane.context_id).unwrap(), Some(seat.context_id));
    assert!(db.senpai_migration_report().unwrap().is_empty());
    let has_reviewer: bool = db.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('contexts') WHERE name = 'reviewer_id')", [], |r| r.get(0),
    ).unwrap();
    assert!(!has_reviewer);
}

// ── Dropping the structural edge rows ──────────────────────────────

/// Give `context_edges` the shape it had while it carried structural
/// rows: the `kind` column and its partial unique index.
fn restore_edge_kind_column(db: &KernelDb) {
    db.conn
        .execute_batch(
            "ALTER TABLE context_edges ADD COLUMN kind TEXT NOT NULL DEFAULT 'structural';
             CREATE UNIQUE INDEX idx_edges_structural_unique
                 ON context_edges(source_id, target_id) WHERE kind = 'structural';",
        )
        .unwrap();
}

fn insert_legacy_edge(db: &KernelDb, source: ContextId, target: ContextId, kind: &str) {
    db.conn
        .execute(
            "INSERT INTO context_edges (edge_id, source_id, target_id, kind, created_at)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![
                uuid::Uuid::now_v7().as_bytes().as_slice(),
                blob_param(source.as_bytes()),
                blob_param(target.as_bytes()),
                kind
            ],
        )
        .unwrap();
}

fn edge_count(db: &KernelDb) -> i64 {
    db.conn
        .query_row("SELECT COUNT(*) FROM context_edges", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn dropping_structural_edges_keeps_drift_and_forked_from() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let parent = make_context_row(Some("parent"));
    let mut child = make_context_row(Some("child"));
    child.forked_from = Some(parent.context_id);
    insert_context_with_doc(&db, &parent, ws_id);
    insert_context_with_doc(&db, &child, ws_id);
    restore_edge_kind_column(&db);
    insert_legacy_edge(&db, parent.context_id, child.context_id, "structural");
    insert_legacy_edge(&db, child.context_id, parent.context_id, "drift");

    KernelDb::drop_structural_edges(&db.conn).unwrap();

    assert!(!KernelDb::has_column(&db.conn, "context_edges", "kind").unwrap());
    assert_eq!(edge_count(&db), 1, "only the drift row remains");
    let edges = db.edges_from(child.context_id).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].target_id, parent.context_id);
    assert_eq!(
        db.get_context(child.context_id).unwrap().unwrap().forked_from,
        Some(parent.context_id)
    );
    // A second run finds the column gone and changes nothing.
    KernelDb::drop_structural_edges(&db.conn).unwrap();
    assert_eq!(edge_count(&db), 1);
}

#[test]
fn dropping_structural_edges_refuses_a_parent_that_is_not_forked_from() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let edge_parent = make_context_row(Some("edge-parent"));
    let fork_parent = make_context_row(Some("fork-parent"));
    let mut child = make_context_row(Some("child"));
    child.forked_from = Some(fork_parent.context_id);
    let mut orphan = make_context_row(Some("orphan"));
    orphan.forked_from = None;
    for ctx in [&edge_parent, &fork_parent, &child, &orphan] {
        insert_context_with_doc(&db, ctx, ws_id);
    }
    restore_edge_kind_column(&db);
    insert_legacy_edge(&db, edge_parent.context_id, child.context_id, "structural");
    insert_legacy_edge(&db, edge_parent.context_id, orphan.context_id, "structural");

    let err = KernelDb::drop_structural_edges(&db.conn).unwrap_err().to_string();

    assert!(err.contains(&child.context_id.short()), "names the child: {err}");
    assert!(err.contains(&orphan.context_id.short()), "names the orphan: {err}");
    assert!(err.contains("forked_from"), "{err}");
    assert!(KernelDb::has_column(&db.conn, "context_edges", "kind").unwrap(), "nothing was dropped");
    assert_eq!(edge_count(&db), 2, "nothing was deleted");
}

#[test]
fn dropping_structural_edges_refuses_two_structural_parents() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let one = make_context_row(Some("one"));
    let two = make_context_row(Some("two"));
    let mut child = make_context_row(Some("child"));
    child.forked_from = Some(one.context_id);
    for ctx in [&one, &two, &child] {
        insert_context_with_doc(&db, ctx, ws_id);
    }
    restore_edge_kind_column(&db);
    insert_legacy_edge(&db, one.context_id, child.context_id, "structural");
    insert_legacy_edge(&db, two.context_id, child.context_id, "structural");

    let err = KernelDb::drop_structural_edges(&db.conn).unwrap_err().to_string();

    assert!(err.contains("2 structural parents"), "{err}");
    assert!(err.contains(&child.context_id.short()), "{err}");
    assert_eq!(edge_count(&db), 2, "nothing was deleted");
}

#[test]
fn dropping_structural_edges_refuses_an_unknown_kind() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let a = make_context_row(Some("a"));
    let b = make_context_row(Some("b"));
    insert_context_with_doc(&db, &a, ws_id);
    insert_context_with_doc(&db, &b, ws_id);
    restore_edge_kind_column(&db);
    insert_legacy_edge(&db, a.context_id, b.context_id, "mystery");

    let err = KernelDb::drop_structural_edges(&db.conn).unwrap_err().to_string();

    assert!(err.contains("unknown kind 'mystery'"), "{err}");
    assert_eq!(edge_count(&db), 1);
}


/// Give the database the routing schema the pre-senpai kernel had:
/// `contexts.reviewer_id`, `contexts.director_id` and the delegation
/// tables. Idempotent.
fn ensure_old_routing_schema(db: &KernelDb) {
    into_pre_context_character(db);
    for sql in ["ALTER TABLE contexts ADD COLUMN reviewer_id BLOB", "ALTER TABLE contexts ADD COLUMN director_id BLOB"] {
        match db.conn.execute(sql, []) {
            Ok(_) => {}
            Err(e) if is_duplicate_column_error(&e) => {}
            Err(e) => panic!("{sql}: {e}"),
        }
    }
    db.conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS approval_reviewer_delegations (
            director_id BLOB NOT NULL PRIMARY KEY,
            reviewer_id BLOB NOT NULL,
            granted_by BLOB NOT NULL,
            granted_at INTEGER NOT NULL,
            revoked_at INTEGER
        );
        CREATE TABLE IF NOT EXISTS approval_reviewer_delegation_events (
            event_id BLOB NOT NULL PRIMARY KEY,
            director_id BLOB NOT NULL,
            reviewer_id BLOB,
            actor_id BLOB NOT NULL,
            operation TEXT NOT NULL CHECK (operation IN ('grant', 'revoke')),
            occurred_at INTEGER NOT NULL
        );",
    ).unwrap();
}

/// An active delegation row as the pre-senpai kernel wrote it.
fn old_delegation(db: &KernelDb, director: PrincipalId, reviewer: PrincipalId, granted_by: PrincipalId) {
    ensure_old_routing_schema(db);
    db.conn.execute(
        "INSERT INTO approval_reviewer_delegations (director_id, reviewer_id, granted_by, granted_at, revoked_at)
         VALUES (?1, ?2, ?3, 1, NULL)",
        params![blob_param(director.as_bytes()), blob_param(reviewer.as_bytes()), blob_param(granted_by.as_bytes())],
    ).unwrap();
    db.conn.execute(
        "INSERT INTO approval_reviewer_delegation_events (event_id, director_id, reviewer_id, actor_id, operation, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'grant', 1)",
        params![uuid::Uuid::now_v7().as_bytes(), blob_param(director.as_bytes()), blob_param(reviewer.as_bytes()), blob_param(granted_by.as_bytes())],
    ).unwrap();
}

fn has_column(db: &KernelDb, table: &str, column: &str) -> bool {
    db.conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"), [column], |r| r.get(0),
    ).unwrap()
}

fn has_table(db: &KernelDb, table: &str) -> bool {
    db.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)", [table], |r| r.get(0),
    ).unwrap()
}

/// A context as the pre-senpai kernel wrote it: routed by `forked_from`,
/// `played_by`, `director_id` and `reviewer_id`, with no senpai.
fn old_context(
    db: &KernelDb,
    ws_id: WorkspaceId,
    label: &str,
    forked_from: Option<&ContextRow>,
    played_by: Option<PrincipalId>,
    created_at: i64,
) -> ContextRow {
    ensure_old_routing_schema(db);
    let mut row = make_context_row(Some(label));
    row.forked_from = forked_from.map(|p| p.context_id);
    row.created_at = created_at;
    insert_context_with_doc(db, &row, ws_id);
    set_stored_played_by(db, row.context_id, played_by);
    row.performer = played_by;
    row
}

fn set_old_routing(db: &KernelDb, ctx: &ContextRow, director: Option<PrincipalId>, reviewer: Option<PrincipalId>) {
    db.conn.execute(
        "UPDATE contexts SET director_id = ?1, reviewer_id = ?2 WHERE context_id = ?3",
        params![
            director.as_ref().map(|p| p.as_bytes().to_vec()),
            reviewer.as_ref().map(|p| p.as_bytes().to_vec()),
            blob_param(ctx.context_id.as_bytes()),
        ],
    ).unwrap();
}

/// The open steps from the senpai mapping through the routing drop.
fn senpai_open_steps(conn: &Connection) -> KernelDbResult<()> {
    KernelDb::migrate_senpai_edges(conn)?;
    KernelDb::drop_old_routing(conn)
}

/// Run the senpai migration the way an upgrading kernel does: the marker is absent.
fn run_senpai_migration(db: &KernelDb) -> KernelDbResult<()> {
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SENPAI_MIGRATION]).unwrap();
    KernelDb::migrate_senpai_edges(&db.conn)
}

/// Write the stored `senpai_ctx` of a context that predates instances.
fn set_stored_senpai(db: &KernelDb, ctx: &ContextRow, senpai: &ContextRow) {
    db.conn.execute(
        "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
        params![blob_param(senpai.context_id.as_bytes()), blob_param(ctx.context_id.as_bytes())],
    ).unwrap();
}

fn senpai_of(db: &KernelDb, ctx: &ContextRow) -> Option<ContextId> {
    db.conn.query_row(
        "SELECT senpai_ctx FROM contexts WHERE context_id = ?1",
        [blob_param(ctx.context_id.as_bytes())], |r| read_opt_context_id(r, 0),
    ).unwrap()
}

fn report_of(db: &KernelDb) -> Vec<(ContextId, Option<ContextId>, String)> {
    db.senpai_migration_report().unwrap()
}

/// The nearest-ancestor case maps exactly: each context's senpai is the
/// nearest context above it whose holder the old resolver named, and
/// nothing is reported. A self-held fork skips its own seat, a
/// holderless parent is walked past, and a root console stays a top.
#[test]
fn senpai_migration_maps_the_nearest_ancestor_case_exactly() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();

    let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
    set_stored_root_ctx(&db, amy, root.context_id);
    let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
    let lane = old_context(&db, ws_id, "lane", Some(&seat), Some(coder), 3);
    let self_fork = old_context(&db, ws_id, "banto-fork", Some(&seat), Some(banto), 4);
    let holderless = old_context(&db, ws_id, "holderless", Some(&seat), None, 5);
    let under_holderless = old_context(&db, ws_id, "under-holderless", Some(&holderless), Some(coder), 6);
    let client = old_context(&db, ws_id, "client", Some(&root), None, 7);
    let scratch = old_context(&db, ws_id, "scratch", None, None, 8);
    let [gone] = seed_characters(&db, &["gone"])[..] else { unreachable!() };
    let retired_seat = old_context(&db, ws_id, "retired-seat", Some(&root), Some(gone), 9);
    let under_retired = old_context(&db, ws_id, "under-retired", Some(&retired_seat), Some(coder), 10);
    assert!(db.retire_character(gone, 1).unwrap());

    run_senpai_migration(&db).unwrap();
    run_context_character_migration(&db).unwrap();

    assert_eq!(senpai_of(&db, &root), None, "a root console is a top");
    assert_eq!(senpai_of(&db, &seat), Some(root.context_id));
    assert_eq!(senpai_of(&db, &lane), Some(seat.context_id), "the lane's reviewer was banto, who plays the seat");
    assert_eq!(senpai_of(&db, &self_fork), Some(root.context_id), "banto cannot review itself: the nearest holder that is not banto is amy's console");
    assert_eq!(senpai_of(&db, &holderless), Some(seat.context_id));
    assert_eq!(senpai_of(&db, &under_holderless), Some(seat.context_id), "walked past the parent nobody holds, as the old walk did");
    assert_eq!(senpai_of(&db, &client), Some(root.context_id), "a context nobody performs answers to the nearest holder above it");
    assert_eq!(senpai_of(&db, &scratch), None, "a parentless system context is a top");
    assert_eq!(senpai_of(&db, &under_retired), Some(retired_seat.context_id), "the old walk stopped at the retired holder and refused by name");
    let error = db.route_review(ReviewQuery::ask(under_retired.context_id, coder)).unwrap_err().to_string();
    assert!(error.contains("gone") && error.contains("retired"), "the new resolver refuses the same way: {error}");
    assert_eq!(report_of(&db), vec![], "every one of these was the nearest-ancestor case");
    for ctx in [&root, &seat, &lane, &self_fork, &under_holderless, &retired_seat] {
        assert_eq!(db.lineage_root(ctx.context_id).unwrap(), amy);
    }
    assert_eq!(db.context_character(client.context_id).unwrap(), None, "a context nobody performs gets no context character");
    let listed: Vec<ContextId> = db.context_character_migration_report().unwrap().into_iter()
        .filter(|n| n.kind == "unmapped").filter_map(|n| n.context_id).collect();
    assert!(listed.contains(&client.context_id), "and is listed because it had a senpai_ctx: {listed:?}");
}

/// The migration routes every context to the reviewer the old resolver
/// would have named for its performer: same answer before and after.
#[test]
fn senpai_migration_preserves_the_reviewer_the_old_resolver_named() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, judge, amy] = seed_characters(&db, &["coder", "banto", "judge", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
    set_stored_root_ctx(&db, amy, root.context_id);
    let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
    let lane = old_context(&db, ws_id, "lane", Some(&seat), Some(coder), 3);
    let judged = old_context(&db, ws_id, "judged", Some(&seat), Some(coder), 4);
    set_old_routing(&db, &judged, Some(banto), Some(amy));
    let judge_seat = old_context(&db, ws_id, "judge-seat", Some(&root), Some(judge), 5);
    let _ = judge_seat;

    run_senpai_migration(&db).unwrap();
    run_context_character_migration(&db).unwrap();

    for (ctx, performer, reviewer) in [(&lane, coder, banto), (&seat, banto, amy), (&judged, coder, amy)] {
        assert_eq!(db.route_review(ReviewQuery::ask(ctx.context_id, performer)).unwrap().reviewer(), reviewer, "{:?}", ctx.label);
    }
    assert_eq!(senpai_of(&db, &judged), Some(root.context_id), "the override named amy, who holds an ancestor: nearest-ancestor case");
    assert_eq!(report_of(&db), vec![]);
}

/// An override or delegation that names a character outside the lineage
/// maps to that character's root console when it is a root, else to its
/// most recent live context, and the context is reported with the reason.
#[test]
fn senpai_migration_reports_what_it_cannot_map_to_an_ancestor() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, judge, bob, amy] = seed_characters(&db, &["coder", "banto", "judge", "bob", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    db.update_character_root(bob, true).unwrap();
    let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
    set_stored_root_ctx(&db, amy, root.context_id);
    let bob_console = old_context(&db, ws_id, "bob", None, Some(bob), 2);
    set_stored_root_ctx(&db, bob, bob_console.context_id);
    let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 3);
    let older = old_context(&db, ws_id, "judge-old", Some(&root), Some(judge), 4);
    let newer = old_context(&db, ws_id, "judge-new", Some(&root), Some(judge), 5);
    let archived = old_context(&db, ws_id, "judge-archived", Some(&root), Some(judge), 9);
    db.archive_context(archived.context_id).unwrap();
    let _ = older;

    let to_bob = old_context(&db, ws_id, "to-bob", Some(&seat), Some(coder), 6);
    set_old_routing(&db, &to_bob, Some(banto), Some(bob));
    let to_judge = old_context(&db, ws_id, "to-judge", Some(&seat), Some(coder), 7);
    set_old_routing(&db, &to_judge, Some(banto), Some(judge));
    let delegated = old_context(&db, ws_id, "delegated", Some(&seat), Some(coder), 8);
    set_old_routing(&db, &delegated, Some(banto), None);
    old_delegation(&db, banto, judge, amy);

    run_senpai_migration(&db).unwrap();

    assert_eq!(senpai_of(&db, &to_bob), Some(bob_console.context_id), "a root reviewer outside the lineage: its root console");
    assert_eq!(senpai_of(&db, &to_judge), Some(newer.context_id), "else its most recent live context, never an archived one");
    assert_eq!(senpai_of(&db, &delegated), Some(newer.context_id));
    assert_eq!(senpai_of(&db, &seat), Some(root.context_id), "the seat itself was the nearest-ancestor case");
    // The delegation also named the seat's other contexts of banto's direction.
    let report = report_of(&db);
    let reported: std::collections::BTreeMap<_, _> = report.iter().map(|(c, s, why)| (*c, (*s, why.clone()))).collect();
    assert_eq!(reported.len(), 3, "{report:?}");
    assert_eq!(reported[&to_bob.context_id].0, Some(bob_console.context_id));
    assert!(reported[&to_bob.context_id].1.contains("root console"), "{report:?}");
    assert_eq!(reported[&to_judge.context_id].0, Some(newer.context_id));
    assert!(reported[&to_judge.context_id].1.contains("most recent live context"), "{report:?}");
    assert!(reported[&delegated.context_id].1.contains("delegation"), "{report:?}");
    for why in reported.values().map(|(_, why)| why) {
        assert!(why.contains("senpai"), "the note says what to do: {why}");
    }
}

/// The migration runs once: a later call does not recompute senpai edges
/// that were moved since.
#[test]
fn senpai_migration_runs_once() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [banto, coder] = seed_characters(&db, &["banto", "coder"])[..] else { unreachable!() };
    let top = old_context(&db, ws_id, "once-top", None, Some(banto), 1);
    let other = old_context(&db, ws_id, "once-other", None, Some(banto), 2);
    let kid = old_context(&db, ws_id, "once-kid", Some(&top), Some(coder), 3);
    run_senpai_migration(&db).unwrap();
    assert_eq!(senpai_of(&db, &kid), Some(top.context_id));

    set_stored_senpai(&db, &kid, &other);
    KernelDb::migrate_senpai_edges(&db.conn).unwrap();
    assert_eq!(senpai_of(&db, &kid), Some(other.context_id), "the marker keeps the moved edge");
}

/// On a database with the old routing (an override, a delegation, the
/// two tables), opening maps every context to its senpai first, then
/// drops `contexts.reviewer_id`, `contexts.director_id` and both
/// delegation tables. Each context whose old route was not its nearest
/// ancestor stays in the report, which keeps the old reviewer. A second
/// open does nothing.
#[test]
fn opening_an_old_database_maps_the_senpai_then_drops_the_old_routing() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, judge, amy] = seed_characters(&db, &["coder", "banto", "judge", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
    set_stored_root_ctx(&db, amy, root.context_id);
    let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
    let lane = old_context(&db, ws_id, "lane", Some(&seat), Some(coder), 3);
    let judge_seat = old_context(&db, ws_id, "judge-seat", Some(&root), Some(judge), 4);
    let overridden = old_context(&db, ws_id, "overridden", Some(&seat), Some(coder), 5);
    set_old_routing(&db, &overridden, Some(banto), Some(judge));
    let delegated = old_context(&db, ws_id, "delegated", Some(&seat), Some(coder), 6);
    set_old_routing(&db, &delegated, Some(banto), None);
    old_delegation(&db, banto, judge, amy);
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SENPAI_MIGRATION]).unwrap();

    senpai_open_steps(&db.conn).unwrap();
    run_context_character_migration(&db).unwrap();

    assert_eq!(senpai_of(&db, &lane), Some(seat.context_id));
    assert_eq!(senpai_of(&db, &overridden), Some(judge_seat.context_id), "the override named judge: its live context");
    assert_eq!(senpai_of(&db, &delegated), Some(judge_seat.context_id), "the delegation named judge too");
    for column in ["reviewer_id", "director_id"] {
        assert!(!has_column(&db, "contexts", column), "contexts.{column} is dropped");
    }
    for table in ["approval_reviewer_delegations", "approval_reviewer_delegation_events"] {
        assert!(!has_table(&db, table), "{table} is dropped");
    }
    let reported: Vec<ContextId> = report_of(&db).into_iter().map(|(c, _, _)| c).collect();
    assert_eq!(reported, vec![overridden.context_id, delegated.context_id]);
    let old_reviewers: Vec<Option<Vec<u8>>> = db.conn.prepare("SELECT old_reviewer FROM senpai_migration_report ORDER BY recorded_at, context_id").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<SqliteResult<_>>().unwrap();
    assert!(old_reviewers.iter().all(|r| r.as_deref() == Some(judge.as_bytes().as_slice())), "the report keeps who the old route named");
    for (ctx, reviewer) in [(&lane, banto), (&overridden, judge), (&delegated, judge)] {
        assert_eq!(db.route_review(ReviewQuery::ask(ctx.context_id, coder)).unwrap().reviewer(), reviewer, "{:?}", ctx.label);
    }

    senpai_open_steps(&db.conn).unwrap();
    assert_eq!(report_of(&db).len(), 2, "a second open changes nothing");
}

/// A database the step-1 kernel already mapped (the marker is set) still
/// carries the columns and tables; opening drops them and leaves every
/// senpai as it was. A migration that refuses drops nothing, so the old
/// routing is still there to map on the next try.
#[test]
fn the_drop_follows_a_recorded_mapping_and_never_a_refused_one() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, nobody, amy] = seed_characters(&db, &["coder", "banto", "nobody", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
    set_stored_root_ctx(&db, amy, root.context_id);
    let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
    let bad = old_context(&db, ws_id, "bad", Some(&seat), Some(coder), 3);
    set_old_routing(&db, &bad, Some(banto), Some(nobody));
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SENPAI_MIGRATION]).unwrap();

    let error = senpai_open_steps(&db.conn).unwrap_err().to_string();
    assert!(error.contains("bad") && error.contains("nobody"), "{error}");
    assert!(has_column(&db, "contexts", "reviewer_id") && has_column(&db, "contexts", "director_id"), "a refused mapping drops nothing");
    assert!(has_table(&db, "approval_reviewer_delegations"));

    set_old_routing(&db, &bad, Some(banto), None);
    set_stored_senpai(&db, &bad, &seat);
    db.conn.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [KernelDb::SENPAI_MIGRATION]).unwrap();
    senpai_open_steps(&db.conn).unwrap();
    assert!(!has_column(&db, "contexts", "reviewer_id") && !has_column(&db, "contexts", "director_id"));
    assert!(!has_table(&db, "approval_reviewer_delegations") && !has_table(&db, "approval_reviewer_delegation_events"));
    assert_eq!(senpai_of(&db, &bad), Some(seat.context_id), "the recorded mapping is kept");
}

/// A context the migration cannot map refuses the whole migration, naming
/// every such context. Nothing is written: no senpai, no report, no marker.
#[test]
fn senpai_migration_refuses_an_unmappable_context_and_names_each() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, nobody, gone, amy] = seed_characters(&db, &["coder", "banto", "nobody", "gone", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
    set_stored_root_ctx(&db, amy, root.context_id);
    let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
    let fine = old_context(&db, ws_id, "fine", Some(&seat), Some(coder), 3);
    // An override naming a character with no console and no live context.
    let bad_override = old_context(&db, ws_id, "bad-override", Some(&seat), Some(coder), 4);
    set_old_routing(&db, &bad_override, Some(banto), Some(nobody));
    // An override naming a retired character: the old resolver refused every ask.
    let retired_override = old_context(&db, ws_id, "retired-override", Some(&seat), Some(coder), 5);
    set_old_routing(&db, &retired_override, Some(banto), Some(gone));
    assert!(db.retire_character(gone, 1).unwrap());
    // An archived context is never a reason to refuse.
    let archived_bad = old_context(&db, ws_id, "archived-bad", Some(&seat), Some(coder), 7);
    set_old_routing(&db, &archived_bad, Some(banto), Some(nobody));
    db.archive_context(archived_bad.context_id).unwrap();

    let error = run_senpai_migration(&db).unwrap_err().to_string();
    assert!(error.contains("bad-override") && error.contains("nobody"), "{error}");
    assert!(error.contains("retired-override") && error.contains("gone"), "{error}");
    assert!(!error.contains("archived-bad") && !error.contains("fine"), "{error}");
    assert!(error.contains("kj context move") || error.contains("senpai"), "{error}");

    assert_eq!(senpai_of(&db, &fine), None, "a refused migration writes nothing");
    assert_eq!(report_of(&db), vec![]);
    let marker: bool = db.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)", [KernelDb::SENPAI_MIGRATION], |r| r.get(0),
    ).unwrap();
    assert!(!marker, "so the next open tries again");
}

/// An archived context that cannot be mapped is left a top and reported;
/// it never blocks the migration.
#[test]
fn senpai_migration_leaves_an_unmappable_archived_context_a_top_and_reports_it() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let [coder, banto, nobody, amy] = seed_characters(&db, &["coder", "banto", "nobody", "amy"])[..] else { unreachable!() };
    db.update_character_root(amy, true).unwrap();
    let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
    set_stored_root_ctx(&db, amy, root.context_id);
    let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
    let archived = old_context(&db, ws_id, "archived-bad", Some(&seat), Some(coder), 3);
    set_old_routing(&db, &archived, Some(banto), Some(nobody));
    db.archive_context(archived.context_id).unwrap();
    let archived_ok = old_context(&db, ws_id, "archived-ok", Some(&seat), Some(coder), 4);
    db.archive_context(archived_ok.context_id).unwrap();

    run_senpai_migration(&db).unwrap();
    assert_eq!(senpai_of(&db, &archived), None);
    assert_eq!(senpai_of(&db, &archived_ok), Some(seat.context_id), "archived contexts map like live ones");
    let report = report_of(&db);
    assert_eq!(report.len(), 1, "{report:?}");
    assert_eq!((report[0].0, report[0].1), (archived.context_id, None));
    assert!(report[0].2.contains("archived") && report[0].2.contains("nobody"), "{report:?}");
}

/// A kernel.db opened with no senpai marker and no report table maps its edges on `open`.
#[test]
fn opening_a_pre_senpai_database_adds_the_column_and_maps_the_edges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (lane, seat, root, amy) = {
        let db = KernelDb::open(&path).unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
    set_stored_root_ctx(&db, amy, root.context_id);
        let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
        let lane = old_context(&db, ws_id, "lane", Some(&seat), Some(coder), 3);
        db.conn.execute_batch(
            "DROP TABLE senpai_migration_report;
             DELETE FROM kernel_migrations WHERE name IN ('senpai-edges-v1', 'instances-v1');",
        ).unwrap();
        (lane, seat, root, amy)
    };
    let db = KernelDb::open(&path).unwrap();
    assert_eq!(db.senpai_of(lane.context_id).unwrap(), Some(seat.context_id));
    assert_eq!(db.senpai_of(seat.context_id).unwrap(), Some(root.context_id));
    assert_eq!(db.lineage_root(lane.context_id).unwrap(), amy);
}

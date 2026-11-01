use super::*;

#[test]
fn fork_lineage_3_deep() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let root = make_context_row(Some("root"));
    insert_context_with_doc(&db, &root, ws_id);

    let mut child = make_context_row(Some("child"));
    child.forked_from = Some(root.context_id);
    child.fork_kind = Some(ForkKind::Full);
    insert_context_with_doc(&db, &child, ws_id);

    let mut grandchild = make_context_row(Some("grandchild"));
    grandchild.forked_from = Some(child.context_id);
    grandchild.fork_kind = Some(ForkKind::Filtered);
    insert_context_with_doc(&db, &grandchild, ws_id);

    let lineage = db.fork_lineage(grandchild.context_id).unwrap();
    assert_eq!(lineage.len(), 3);
    assert_eq!(lineage[0].0.context_id, grandchild.context_id);
    assert_eq!(lineage[0].1, 0);
    assert_eq!(lineage[1].0.context_id, child.context_id);
    assert_eq!(lineage[1].1, 1);
    assert_eq!(lineage[2].0.context_id, root.context_id);
    assert_eq!(lineage[2].1, 2);
}

#[test]
fn drift_edge_allows_dupes() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let a = make_context_row(None);
    let b = make_context_row(None);
    insert_context_with_doc(&db, &a, ws_id);
    insert_context_with_doc(&db, &b, ws_id);

    // Multiple drift edges same pair → all stored
    for _ in 0..3 {
        db.insert_edge(&make_edge(a.context_id, b.context_id))
            .unwrap();
    }

    let edges = db.edges_from(a.context_id).unwrap();
    assert_eq!(edges.len(), 3);
}

#[test]
fn drift_push_creates_edge() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let a = make_context_row(Some("source"));
    let b = make_context_row(Some("target"));
    insert_context_with_doc(&db, &a, ws_id);
    insert_context_with_doc(&db, &b, ws_id);

    // Simulate drift push → create drift edge
    let edge1 = make_edge(a.context_id, b.context_id);
    db.insert_edge(&edge1).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1));

    let edge2 = make_edge(a.context_id, b.context_id);
    db.insert_edge(&edge2).unwrap();

    // edges_from returns both
    let edges = db.edges_from(a.context_id).unwrap();
    assert_eq!(edges.len(), 2);

    // drift_provenance returns newest first
    let prov = db.drift_provenance(a.context_id).unwrap();
    assert_eq!(prov.len(), 2);
    assert!(prov[0].created_at >= prov[1].created_at);
}

#[test]
fn context_dag_recursive() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    // Create a 5-node tree: root → [a, b], a → [c, d]
    let root = make_context_row(Some("root"));
    let mut a = make_context_row(Some("a"));
    let mut b = make_context_row(Some("b"));
    let mut c = make_context_row(Some("c"));
    let mut d = make_context_row(Some("d"));
    a.forked_from = Some(root.context_id);
    b.forked_from = Some(root.context_id);
    c.forked_from = Some(a.context_id);
    d.forked_from = Some(a.context_id);

    for ctx in [&root, &a, &b, &c, &d] {
        insert_context_with_doc(&db, ctx, ws_id);
    }

    let dag = db.context_dag().unwrap();
    assert_eq!(dag.len(), 5);

    // Root at depth 0
    assert_eq!(dag[0].0.context_id, root.context_id);
    assert_eq!(dag[0].1, 0);

    // a, b at depth 1
    let depth1: Vec<_> = dag.iter().filter(|(_, d)| *d == 1).collect();
    assert_eq!(depth1.len(), 2);

    // c, d at depth 2
    let depth2: Vec<_> = dag.iter().filter(|(_, d)| *d == 2).collect();
    assert_eq!(depth2.len(), 2);
}

#[test]
fn context_dag_lists_every_child_under_its_own_parent() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    // root has two children; the earlier one has a child of its own that
    // is newer than the later sibling.
    let base = now_millis();
    let mut root = make_context_row(Some("root"));
    let mut first = make_context_row(Some("first"));
    let mut later = make_context_row(Some("later"));
    let mut grandchild = make_context_row(Some("grandchild"));
    root.created_at = base;
    first.created_at = base + 1;
    later.created_at = base + 2;
    grandchild.created_at = base + 3;
    first.forked_from = Some(root.context_id);
    later.forked_from = Some(root.context_id);
    grandchild.forked_from = Some(first.context_id);
    for ctx in [&root, &first, &later, &grandchild] {
        insert_context_with_doc(&db, ctx, ws_id);
    }

    let dag = db.context_dag().unwrap();
    let shape: Vec<(String, i64)> = dag
        .iter()
        .map(|(c, depth)| (c.label.clone().unwrap(), *depth))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("root".to_string(), 0),
            ("first".to_string(), 1),
            ("grandchild".to_string(), 2),
            ("later".to_string(), 1),
        ],
        "each child follows its own parent, depth-first"
    );
}

#[test]
fn context_edge_cascade() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    let a = make_context_row(Some("edge-a"));
    let b = make_context_row(Some("edge-b"));
    insert_context_with_doc(&db, &a, ws_id);
    insert_context_with_doc(&db, &b, ws_id);

    db.insert_edge(&make_edge(a.context_id, b.context_id))
        .unwrap();

    // The edge exists
    let all = db.edges_from(a.context_id).unwrap();
    assert_eq!(all.len(), 1);

    // Delete context a → edges cascade
    db.conn
        .execute(
            "DELETE FROM contexts WHERE context_id = ?1",
            params![blob_param(a.context_id.as_bytes())],
        )
        .unwrap();

    // Edges gone
    let all = db.edges_from(a.context_id).unwrap();
    assert!(all.is_empty());
}

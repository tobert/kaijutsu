use super::*;
use gogi_proto::control::ControlOp;
use std::collections::BTreeMap;

fn vector(byte: char) -> ContentId {
    ContentId::parse(format!("blake3:{}", byte.to_string().repeat(64))).unwrap()
}

fn control(byte: char, op: ControlOp, strength: f64, layers: Option<&[(u32, f64)]>) -> Control {
    Control {
        vector: vector(byte),
        op,
        strength,
        layers: layers.map(|l| l.iter().copied().collect::<BTreeMap<_, _>>()),
    }
}

fn fixture() -> (KernelDb, ContextId) {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let ctx = make_context_row(Some("controls"));
    insert_context_with_doc(&db, &ctx, ws);
    (db, ctx.context_id)
}

#[test]
fn controls_are_empty_for_a_context_that_has_none() {
    let (db, ctx) = fixture();
    assert!(db.list_context_controls(ctx).unwrap().is_empty());
}

#[test]
fn controls_round_trip_in_order_with_layers() {
    let (db, ctx) = fixture();
    let items = [
        control('a', ControlOp::Ablate, 1.0, None),
        control('b', ControlOp::Add, -0.25, Some(&[(12, 1.0), (3, 0.5)])),
        control('a', ControlOp::Clamp, 0.0, None),
    ];
    for (i, item) in items.iter().enumerate() {
        assert_eq!(db.add_context_control(ctx, item).unwrap(), i, "the position the item took");
    }
    assert_eq!(db.list_context_controls(ctx).unwrap(), items, "order, op, strength, and layers survive");
}

#[test]
fn removing_a_control_closes_the_positions_and_drops_its_layers() {
    let (db, ctx) = fixture();
    let first = control('a', ControlOp::Ablate, 1.0, Some(&[(1, 2.0)]));
    let second = control('b', ControlOp::Add, 3.0, Some(&[(4, 5.0)]));
    let third = control('c', ControlOp::Clamp, 6.0, None);
    for item in [&first, &second, &third] {
        db.add_context_control(ctx, item).unwrap();
    }
    assert_eq!(db.remove_context_control(ctx, 1).unwrap(), second);
    assert_eq!(db.list_context_controls(ctx).unwrap(), [first.clone(), third.clone()]);
    let stray: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM context_control_layers WHERE layer = 4", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stray, 0, "the removed item's layer rows go with it");
    // A later add lands after the last item, not in the gap.
    assert_eq!(db.add_context_control(ctx, &second).unwrap(), 2);
    assert_eq!(db.list_context_controls(ctx).unwrap(), [first, third, second]);
}

#[test]
fn removing_past_the_end_is_not_found() {
    let (db, ctx) = fixture();
    db.add_context_control(ctx, &control('a', ControlOp::Add, 1.0, None)).unwrap();
    let error = db.remove_context_control(ctx, 1).unwrap_err();
    assert!(matches!(error, KernelDbError::NotFound(_)), "{error}");
    assert_eq!(db.list_context_controls(ctx).unwrap().len(), 1, "nothing was removed");
}

#[test]
fn clearing_counts_the_items_and_leaves_other_contexts_alone() {
    let (db, ctx) = fixture();
    let other = make_context_row(Some("other"));
    insert_context_with_doc(&db, &other, setup_test_db(&db));
    db.add_context_control(ctx, &control('a', ControlOp::Add, 1.0, Some(&[(1, 1.0)]))).unwrap();
    db.add_context_control(ctx, &control('b', ControlOp::Add, 1.0, None)).unwrap();
    db.add_context_control(other.context_id, &control('c', ControlOp::Add, 1.0, None)).unwrap();
    assert_eq!(db.clear_context_controls(ctx).unwrap(), 2);
    assert!(db.list_context_controls(ctx).unwrap().is_empty());
    assert_eq!(db.list_context_controls(other.context_id).unwrap().len(), 1);
    assert_eq!(db.clear_context_controls(ctx).unwrap(), 0);
    let layer_rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM context_control_layers WHERE context_id = ?1", params![blob_param(ctx.as_bytes())], |r| r.get(0))
        .unwrap();
    assert_eq!(layer_rows, 0, "clearing takes the layer rows too");
    // The next item reuses position 0; it must not inherit the cleared item's layers.
    let plain = control('d', ControlOp::Ablate, 1.0, None);
    db.add_context_control(ctx, &plain).unwrap();
    assert_eq!(db.list_context_controls(ctx).unwrap(), [plain]);
}

/// An empty `layers` names no layer, which the contract refuses. Stored, it
/// would read back as no layer rows, which means every layer.
#[test]
fn an_item_whose_layers_name_no_layer_is_refused() {
    let (db, ctx) = fixture();
    let e = db.add_context_control(ctx, &control('a', ControlOp::Ablate, 1.0, Some(&[]))).unwrap_err();
    assert!(e.to_string().contains("names no layer"), "{e}");
    assert!(db.list_context_controls(ctx).unwrap().is_empty());
}

#[test]
fn a_non_finite_strength_or_weight_is_refused_at_the_write() {
    let (db, ctx) = fixture();
    for bad in [f64::NAN, f64::INFINITY] {
        let e = db.add_context_control(ctx, &control('a', ControlOp::Add, bad, None)).unwrap_err();
        assert!(e.to_string().contains("finite"), "{e}");
        let e = db.add_context_control(ctx, &control('a', ControlOp::Add, 1.0, Some(&[(1, bad)]))).unwrap_err();
        assert!(e.to_string().contains("finite"), "{e}");
    }
    assert!(db.list_context_controls(ctx).unwrap().is_empty());
}

/// A stored row that does not decode is corruption: dropping it would change
/// what the model does without anyone being told.
#[test]
fn a_corrupt_row_is_an_error_not_a_skipped_row() {
    for (column, value) in [("op", "'erase'"), ("vector", "'blake3:abc'"), ("strength", "1e999")] {
        let (mut db, ctx) = fixture();
        db.add_context_control(ctx, &control('a', ControlOp::Add, 1.0, None)).unwrap();
        db.add_context_control(ctx, &control('b', ControlOp::Ablate, 1.0, None)).unwrap();
        db.conn_for_test_mut()
            .execute(&format!("UPDATE context_controls SET {column} = {value} WHERE seq = 0"), [])
            .unwrap();
        let error = db.list_context_controls(ctx).unwrap_err();
        assert!(matches!(error, KernelDbError::Corrupt(_)), "{column}: {error}");
        assert!(error.to_string().contains("position 0"), "{column}: {error}");
    }
}

#[test]
fn a_corrupt_layer_row_is_an_error() {
    let (mut db, ctx) = fixture();
    db.add_context_control(ctx, &control('a', ControlOp::Add, 1.0, Some(&[(2, 1.0)]))).unwrap();
    db.conn_for_test_mut().execute("UPDATE context_control_layers SET layer = -1", []).unwrap();
    let error = db.list_context_controls(ctx).unwrap_err();
    assert!(matches!(error, KernelDbError::Corrupt(_)), "{error}");
}

#[test]
fn a_fork_copies_the_list_and_the_copy_is_independent() {
    let mut db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let src = make_context_row(Some("ctl-src"));
    insert_context_with_doc(&db, &src, ws);
    let items = [
        control('a', ControlOp::Ablate, 1.0, None),
        control('b', ControlOp::Clamp, 2.5, Some(&[(7, 0.5)])),
    ];
    for item in &items {
        db.add_context_control(src.context_id, item).unwrap();
    }
    let child = make_context_row(Some("ctl-child"));
    db.insert_forked_context(&child, ws, src.context_id, None).unwrap();
    assert_eq!(db.list_context_controls(child.context_id).unwrap(), items);

    db.clear_context_controls(child.context_id).unwrap();
    assert_eq!(db.list_context_controls(src.context_id).unwrap(), items, "the parent keeps its list");
}

//! `kaijutsu-server blocks repair-order` against a real kernel.db: the dry
//! run changes nothing, and applied moves survive reopening the database.
use std::sync::Arc;

use kaijutsu_kernel::block_store::{shared_block_store_with_db, DocumentKind};
use kaijutsu_kernel::kernel_db::KernelDb;
use kaijutsu_server::offline::repair_order;
use kaijutsu_types::{BlockId, BlockKind, ContentType, ContextId, PrincipalId, Role, Status};

fn order_after_reopen(dir: &std::path::Path, ctx: ContextId) -> Vec<BlockId> {
    let db = Arc::new(parking_lot::Mutex::new(KernelDb::open(dir.join("kernel.db")).unwrap()));
    let ws = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
    let store = shared_block_store_with_db(db, ws, PrincipalId::system());
    assert!(store.load_one_from_db(ctx).unwrap());
    store.get(ctx).unwrap().doc.blocks_ordered().iter().map(|b| b.id).collect()
}

#[test]
fn applied_moves_survive_a_reopen_and_the_dry_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ContextId::new();
    let (intended, scrambled) = {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::open(dir.path().join("kernel.db")).unwrap()));
        let ws = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let store = shared_block_store_with_db(db, ws, PrincipalId::system());
        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let u1 = store.insert_block_as(ctx, None, None, Role::User, BlockKind::Text, "q",
            Status::Done, ContentType::Plain, None).unwrap();
        let held = store.insert_block_as(ctx, None, Some(&u1), Role::User, BlockKind::Text, "held",
            Status::Done, ContentType::Plain, None).unwrap();
        let c1 = store.insert_tool_call(ctx, None, Some(&u1), "shell", serde_json::json!({"command": "a"}), None).unwrap();
        let r1 = store.insert_tool_result(ctx, &c1, Some(&c1), "out", false, Some(0), None).unwrap();
        store.move_block(ctx, &c1, Some(&r1)).unwrap();
        (vec![u1, c1, r1, held], vec![u1, r1, c1, held])
    };
    assert_eq!(order_after_reopen(dir.path(), ctx), scrambled);

    assert_eq!(repair_order(dir.path(), None, false).unwrap(), 1, "the dry run reports the damage");
    assert_eq!(order_after_reopen(dir.path(), ctx), scrambled, "the dry run changes nothing");

    assert_eq!(repair_order(dir.path(), None, true).unwrap(), 0, "nothing left after --apply");
    assert_eq!(order_after_reopen(dir.path(), ctx), intended, "the moves are durable");
    assert_eq!(repair_order(dir.path(), None, false).unwrap(), 0, "a second look finds nothing");
}

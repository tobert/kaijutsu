use super::*;

// ── Client Views (docs/shared-state.md "Retiring KV") ──────────────

#[test]
fn client_view_unset_is_none() {
    let db = KernelDb::temporary().unwrap();
    assert!(db.get_client_view("client-a").unwrap().is_none());
}

#[test]
fn client_view_set_get_round_trip() {
    let db = KernelDb::temporary().unwrap();
    let ctx = ContextId::new();
    db.set_client_view("client-a", ctx).unwrap();
    assert_eq!(db.get_client_view("client-a").unwrap(), Some(ctx));
}

#[test]
fn client_view_overwrite_returns_latest() {
    // Set twice: the second write replaces the first in place (PK is
    // client_id), matching the "one row per installation" design — not a
    // history of every context this client ever looked at.
    let db = KernelDb::temporary().unwrap();
    let first = ContextId::new();
    let second = ContextId::new();
    db.set_client_view("client-a", first).unwrap();
    db.set_client_view("client-a", second).unwrap();
    assert_eq!(
        db.get_client_view("client-a").unwrap(),
        Some(second),
        "second set overwrites the first"
    );
}

#[test]
fn client_view_is_namespaced_per_client() {
    // Two installations don't clobber each other's view — the whole point
    // of keying by client_id instead of a single global row.
    let db = KernelDb::temporary().unwrap();
    let ctx_a = ContextId::new();
    let ctx_b = ContextId::new();
    db.set_client_view("client-a", ctx_a).unwrap();
    db.set_client_view("client-b", ctx_b).unwrap();
    assert_eq!(db.get_client_view("client-a").unwrap(), Some(ctx_a));
    assert_eq!(db.get_client_view("client-b").unwrap(), Some(ctx_b));
}


// ── Dirty File Buffers (docs/file-buffers.md) ───────────────────────

#[test]
fn dirty_file_buffer_unset_is_none() {
    let db = KernelDb::temporary().unwrap();
    assert!(db.get_dirty_file_buffer("/tmp/never-touched.txt").unwrap().is_none());
}

#[test]
fn dirty_file_buffer_round_trips_with_generation() {
    let db = KernelDb::temporary().unwrap();
    let ctx = ContextId::new();
    db.record_dirty_file_buffer("/tmp/a.txt", ctx, Some(7)).unwrap();
    let row = db.get_dirty_file_buffer("/tmp/a.txt").unwrap().expect("row recorded");
    assert_eq!(row.path, "/tmp/a.txt");
    assert_eq!(row.context_id, ctx);
    assert_eq!(row.loaded_generation, Some(7));
}

#[test]
fn dirty_file_buffer_round_trips_with_none_generation() {
    // Nullable because the cache already models "couldn't read the VFS
    // attr" as None — the row must preserve that, not coerce to 0.
    let db = KernelDb::temporary().unwrap();
    let ctx = ContextId::new();
    db.record_dirty_file_buffer("/tmp/b.txt", ctx, None).unwrap();
    let row = db.get_dirty_file_buffer("/tmp/b.txt").unwrap().expect("row recorded");
    assert_eq!(row.context_id, ctx);
    assert_eq!(row.loaded_generation, None);
}

#[test]
fn dirty_file_buffer_upsert_replaces_not_duplicates() {
    let db = KernelDb::temporary().unwrap();
    let first_ctx = ContextId::new();
    let second_ctx = ContextId::new();
    db.record_dirty_file_buffer("/tmp/c.txt", first_ctx, Some(1)).unwrap();
    let first_dirtied_at = db.get_dirty_file_buffer("/tmp/c.txt").unwrap().unwrap().dirtied_at;

    db.record_dirty_file_buffer("/tmp/c.txt", second_ctx, Some(2)).unwrap();
    let row = db.get_dirty_file_buffer("/tmp/c.txt").unwrap().expect("row still present");
    assert_eq!(row.context_id, second_ctx, "second record wins on context_id/generation");
    assert_eq!(row.loaded_generation, Some(2));
    assert_eq!(
        row.dirtied_at, first_dirtied_at,
        "a re-edit within the same dirty spell must not reset dirtied_at"
    );

    let all = db.list_dirty_file_buffers().unwrap();
    assert_eq!(all.len(), 1, "upsert must not create a second row for the same path");
}

#[test]
fn dirty_file_buffer_clear_removes_it() {
    let db = KernelDb::temporary().unwrap();
    let ctx = ContextId::new();
    db.record_dirty_file_buffer("/tmp/d.txt", ctx, Some(1)).unwrap();
    assert!(db.clear_dirty_file_buffer("/tmp/d.txt").unwrap(), "a row existed to clear");
    assert!(db.get_dirty_file_buffer("/tmp/d.txt").unwrap().is_none());
}

#[test]
fn dirty_file_buffer_clear_unrecorded_path_is_noop() {
    let db = KernelDb::temporary().unwrap();
    assert!(
        !db.clear_dirty_file_buffer("/tmp/never-recorded.txt").unwrap(),
        "clearing a path with no row is a no-op, not an error"
    );
}

#[test]
fn dirty_file_buffer_list_returns_multiple_paths() {
    let db = KernelDb::temporary().unwrap();
    db.record_dirty_file_buffer("/tmp/e1.txt", ContextId::new(), Some(1)).unwrap();
    db.record_dirty_file_buffer("/tmp/e2.txt", ContextId::new(), None).unwrap();
    db.record_dirty_file_buffer("/tmp/e3.txt", ContextId::new(), Some(3)).unwrap();

    let mut paths: Vec<String> = db.list_dirty_file_buffers().unwrap().into_iter().map(|r| r.path).collect();
    paths.sort();
    assert_eq!(paths, vec!["/tmp/e1.txt", "/tmp/e2.txt", "/tmp/e3.txt"]);
}


use super::*;

#[test]
fn workspace_lifecycle() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let ws = WorkspaceRow {
        workspace_id: WorkspaceId::new(),
                    label: "kaijutsu".into(),
        description: Some("Main project".into()),
        created_at: now,
        created_by: creator,
        archived_at: None,
    };

    db.insert_workspace(&ws).unwrap();

    // Add paths
    let p1 = WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/src/kaijutsu".into(),
        read_only: false,
        created_at: now,
    };
    let p2 = WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/src/kaish".into(),
        read_only: false,
        created_at: now,
    };
    db.insert_workspace_path(&p1).unwrap();
    db.insert_workspace_path(&p2).unwrap();

    // List paths
    let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
    assert_eq!(paths.len(), 2);

    // Delete one path
    assert!(
        db.delete_workspace_path(ws.workspace_id, "/home/user/src/kaish")
            .unwrap()
    );
    let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
    assert_eq!(paths.len(), 1);

    // Archive workspace
    assert!(db.archive_workspace(ws.workspace_id).unwrap());
    let active = db.list_workspaces().unwrap();
    assert!(active.is_empty());
}

#[test]
fn workspace_soft_delete() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let ws = WorkspaceRow {
        workspace_id: WorkspaceId::new(),
                    label: "project".into(),
        description: None,
        created_at: now,
        created_by: creator,
        archived_at: None,
    };
    db.insert_workspace(&ws).unwrap();

    // Create context referencing this workspace
    let mut ctx = make_context_row(Some("ctx-ws"));
    ctx.workspace_id = Some(ws.workspace_id);
    insert_context_with_doc(&db, &ctx, ws.workspace_id);

    // Archive workspace
    db.archive_workspace(ws.workspace_id).unwrap();

    // Context still references the workspace
    let loaded = db.get_context(ctx.context_id).unwrap().unwrap();
    assert_eq!(loaded.workspace_id, Some(ws.workspace_id));

    // get_workspace still returns it (it's archived, not deleted)
    let loaded_ws = db.get_workspace(ws.workspace_id).unwrap().unwrap();
    assert!(loaded_ws.archived_at.is_some());
}

#[test]
fn workspace_path_read_only_roundtrip() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let ws = WorkspaceRow {
        workspace_id: WorkspaceId::new(),
                    label: "ro-test".into(),
        description: None,
        created_at: now,
        created_by: creator,
        archived_at: None,
    };
    db.insert_workspace(&ws).unwrap();

    let rw = WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/src".into(),
        read_only: false,
        created_at: now,
    };
    let ro = WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/docs".into(),
        read_only: true,
        created_at: now,
    };
    db.insert_workspace_path(&rw).unwrap();
    db.insert_workspace_path(&ro).unwrap();

    let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
    assert_eq!(paths.len(), 2);
    // Ordered by path
    assert_eq!(paths[0].path, "/home/user/docs");
    assert!(paths[0].read_only);
    assert_eq!(paths[1].path, "/home/user/src");
    assert!(!paths[1].read_only);
}

#[test]
fn context_workspace_paths_bound() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let ws = WorkspaceRow {
        workspace_id: WorkspaceId::new(),
                    label: "project".into(),
        description: None,
        created_at: now,
        created_by: creator,
        archived_at: None,
    };
    db.insert_workspace(&ws).unwrap();

    db.insert_workspace_path(&WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/src/project".into(),
        read_only: false,
        created_at: now,
    })
    .unwrap();
    db.insert_workspace_path(&WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/docs".into(),
        read_only: true,
        created_at: now,
    })
    .unwrap();

    // Create context with workspace bound
    let mut ctx = make_context_row(Some("bound"));
    ctx.workspace_id = Some(ws.workspace_id);
    insert_context_with_doc(&db, &ctx, ws.workspace_id);

    let paths = db.context_workspace_paths(ctx.context_id).unwrap();
    let paths = paths.unwrap();
    assert_eq!(paths.len(), 2);
}

#[test]
fn context_workspace_paths_unbound() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("unbound"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let paths = db.context_workspace_paths(ctx.context_id).unwrap();
    assert!(paths.is_none());
}

#[test]
fn check_workspace_path_unbound_context() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("unbound"));
    insert_context_with_doc(&db, &ctx, ws_id);

    // Unbound context → None (no restriction)
    let result = db
        .check_workspace_path(ctx.context_id, "/anywhere")
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn check_workspace_path_allowed_rw() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let ws = WorkspaceRow {
        workspace_id: WorkspaceId::new(),
                    label: "proj".into(),
        description: None,
        created_at: now,
        created_by: creator,
        archived_at: None,
    };
    db.insert_workspace(&ws).unwrap();
    db.insert_workspace_path(&WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/src/kaijutsu".into(),
        read_only: false,
        created_at: now,
    })
    .unwrap();

    let mut ctx = make_context_row(Some("bound"));
    ctx.workspace_id = Some(ws.workspace_id);
    insert_context_with_doc(&db, &ctx, ws.workspace_id);

    // Path inside workspace rw path → Some(false)
    let result = db
        .check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/src/main.rs")
        .unwrap();
    assert_eq!(result, Some(false));
}

#[test]
fn check_workspace_path_allowed_ro() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let ws = WorkspaceRow {
        workspace_id: WorkspaceId::new(),
                    label: "proj".into(),
        description: None,
        created_at: now,
        created_by: creator,
        archived_at: None,
    };
    db.insert_workspace(&ws).unwrap();
    db.insert_workspace_path(&WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/docs".into(),
        read_only: true,
        created_at: now,
    })
    .unwrap();

    let mut ctx = make_context_row(Some("bound"));
    ctx.workspace_id = Some(ws.workspace_id);
    insert_context_with_doc(&db, &ctx, ws.workspace_id);

    // Path inside workspace ro path → Some(true)
    let result = db
        .check_workspace_path(ctx.context_id, "/home/user/docs/README.md")
        .unwrap();
    assert_eq!(result, Some(true));
}

#[test]
fn check_workspace_path_outside_scope() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let ws = WorkspaceRow {
        workspace_id: WorkspaceId::new(),
                    label: "proj".into(),
        description: None,
        created_at: now,
        created_by: creator,
        archived_at: None,
    };
    db.insert_workspace(&ws).unwrap();
    db.insert_workspace_path(&WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/src/kaijutsu".into(),
        read_only: false,
        created_at: now,
    })
    .unwrap();

    let mut ctx = make_context_row(Some("bound"));
    ctx.workspace_id = Some(ws.workspace_id);
    insert_context_with_doc(&db, &ctx, ws.workspace_id);

    // Path outside all workspace paths → Validation error
    let err = db
        .check_workspace_path(ctx.context_id, "/etc/passwd")
        .unwrap_err();
    assert!(matches!(err, KernelDbError::Validation(_)));
}

#[test]
fn check_workspace_path_longest_prefix() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let ws = WorkspaceRow {
        workspace_id: WorkspaceId::new(),
                    label: "proj".into(),
        description: None,
        created_at: now,
        created_by: creator,
        archived_at: None,
    };
    db.insert_workspace(&ws).unwrap();
    db.insert_workspace_path(&WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/src".into(),
        read_only: false,
        created_at: now,
    })
    .unwrap();
    db.insert_workspace_path(&WorkspacePathRow {
        workspace_id: ws.workspace_id,
        path: "/home/user/src/kaijutsu/docs".into(),
        read_only: true,
        created_at: now,
    })
    .unwrap();

    let mut ctx = make_context_row(Some("bound"));
    ctx.workspace_id = Some(ws.workspace_id);
    insert_context_with_doc(&db, &ctx, ws.workspace_id);

    // /home/user/src/kaijutsu/src → matches /home/user/src (rw)
    assert_eq!(
        db.check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/src/main.rs")
            .unwrap(),
        Some(false),
    );

    // /home/user/src/kaijutsu/docs/README.md → matches /home/user/src/kaijutsu/docs (ro, longer prefix)
    assert_eq!(
        db.check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/docs/README.md")
            .unwrap(),
        Some(true),
    );
}

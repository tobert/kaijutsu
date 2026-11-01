use super::*;

// ── Roster: dropping the legacy value-enum CHECKs ───────────────────
// Rust-API coverage (upsert/reconcile/restart-shaped/generation/clock
// skew) lives in `roster.rs`'s own test module. These exercise the
// schema directly, the same split approval-ledger's `schema.rs` uses —
// but where that split used to mean "the CHECK rejects what the Rust
// API never sends," these four tables no longer carry that CHECK at
// all (`drop_legacy_value_enum_checks`'s doc comment states the
// ruling). `RosterEntity::from_parts`, `LivenessKind::parse`, and
// `Availability::parse` (`roster.rs`) are the only gates left.

fn put_roster_entity_back_on_its_legacy_shape(conn: &Connection) {
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE roster_entity_legacy (
             entity_kind   TEXT NOT NULL CHECK (entity_kind IN ('principal', 'context')),
             entity_id     BLOB NOT NULL,
             label         TEXT,
             first_seen_at INTEGER NOT NULL
                 DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
             PRIMARY KEY (entity_kind, entity_id)
         );
         INSERT INTO roster_entity_legacy (entity_kind, entity_id, label, first_seen_at)
             SELECT entity_kind, entity_id, label, first_seen_at FROM roster_entity;
         DROP TABLE roster_entity;
         ALTER TABLE roster_entity_legacy RENAME TO roster_entity;
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();
}

fn put_roster_presence_back_on_its_legacy_shape(conn: &Connection) {
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE roster_presence_legacy (
             source          TEXT NOT NULL,
             source_local_id TEXT NOT NULL,
             entity_kind     TEXT NOT NULL,
             entity_id       BLOB NOT NULL,
             liveness_kind   TEXT NOT NULL
                 CHECK (liveness_kind IN ('bound', 'recent', 'attested')),
             host            TEXT,
             pid             INTEGER,
             proc_start      INTEGER,
             first_seen_at   INTEGER NOT NULL,
             PRIMARY KEY (source, source_local_id),
             FOREIGN KEY (entity_kind, entity_id)
                 REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE
         );
         INSERT INTO roster_presence_legacy
             (source, source_local_id, entity_kind, entity_id, liveness_kind,
              host, pid, proc_start, first_seen_at)
             SELECT source, source_local_id, entity_kind, entity_id, liveness_kind,
                    host, pid, proc_start, first_seen_at
               FROM roster_presence;
         DROP TABLE roster_presence;
         ALTER TABLE roster_presence_legacy RENAME TO roster_presence;
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();
}

fn put_roster_activity_back_on_its_legacy_shape(conn: &Connection) {
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE roster_activity_legacy (
             source          TEXT    NOT NULL,
             source_local_id TEXT    NOT NULL,
             live            INTEGER NOT NULL CHECK (live IN (0, 1)),
             observed_at     INTEGER,
             recorded_at     INTEGER NOT NULL,
             PRIMARY KEY (source, source_local_id),
             FOREIGN KEY (source, source_local_id)
                 REFERENCES roster_presence(source, source_local_id) ON DELETE CASCADE
         );
         INSERT INTO roster_activity_legacy (source, source_local_id, live, observed_at, recorded_at)
             SELECT source, source_local_id, live, observed_at, recorded_at FROM roster_activity;
         DROP TABLE roster_activity;
         ALTER TABLE roster_activity_legacy RENAME TO roster_activity;
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();
}

fn put_roster_status_back_on_its_legacy_shape(conn: &Connection) {
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE roster_status_legacy (
             entity_kind  TEXT NOT NULL,
             entity_id    BLOB NOT NULL,
             status_text  TEXT,
             availability TEXT NOT NULL CHECK (availability IN ('active', 'idle', 'away', 'dnd')),
             observed_at  INTEGER,
             recorded_at  INTEGER NOT NULL,
             PRIMARY KEY (entity_kind, entity_id),
             FOREIGN KEY (entity_kind, entity_id)
                 REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE
         );
         INSERT INTO roster_status_legacy
             (entity_kind, entity_id, status_text, availability, observed_at, recorded_at)
             SELECT entity_kind, entity_id, status_text, availability, observed_at, recorded_at
               FROM roster_status;
         DROP TABLE roster_status;
         ALTER TABLE roster_status_legacy RENAME TO roster_status;
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();
}

/// A database carrying `roster_entity`'s legacy `entity_kind` CHECK must
/// lose it on the next open (`apply_additive_migrations`), keep the row
/// it already had, and accept a kind the old CHECK rejected. Falsified
/// by reverting `drop_legacy_value_enum_checks` to a no-op: the final
/// INSERT below then failed with a CHECK violation.
#[test]
fn drop_legacy_value_enum_checks_drops_the_roster_entity_check_and_keeps_rows() {
    let db = KernelDb::temporary().unwrap();
    db.conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, label, first_seen_at)
             VALUES ('principal', ?1, 'kept', 1)",
            params![blob_param(PrincipalId::new().as_bytes())],
        )
        .unwrap();
    put_roster_entity_back_on_its_legacy_shape(&db.conn);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let sql: String = db
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_entity'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!sql.contains("CHECK (entity_kind IN ("), "the value-enum CHECK must be gone, got: {sql}");
    let label: String = db
        .conn
        .query_row("SELECT label FROM roster_entity WHERE entity_kind = 'principal'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(label, "kept");

    // Neither 'principal' nor 'context' — never admitted by the retired
    // CHECK. `RosterEntity::from_parts` (roster.rs) is the sole gate now.
    db.conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
             VALUES ('service', ?1, 1)",
            params![blob_param(&[7u8; 16])],
        )
        .unwrap();
}

/// Same contract as the `roster_entity` test above, for
/// `roster_presence.liveness_kind`.
#[test]
fn drop_legacy_value_enum_checks_drops_the_roster_presence_check_and_keeps_rows() {
    let db = KernelDb::temporary().unwrap();
    let principal = PrincipalId::new();
    db.conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
             VALUES ('principal', ?1, 1)",
            params![blob_param(principal.as_bytes())],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO roster_presence
                (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
             VALUES ('kept-source', 'kept-id', 'principal', ?1, 'bound', 1)",
            params![blob_param(principal.as_bytes())],
        )
        .unwrap();
    put_roster_presence_back_on_its_legacy_shape(&db.conn);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let sql: String = db
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_presence'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!sql.contains("CHECK (liveness_kind IN ("), "the value-enum CHECK must be gone, got: {sql}");
    let kept: String = db
        .conn
        .query_row("SELECT liveness_kind FROM roster_presence WHERE source = 'kept-source'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(kept, "bound");

    // 'heard' is the design table's fourth kind, deliberately never
    // admitted by the retired CHECK (no v1 producer). It now inserts
    // cleanly — `LivenessKind::parse` (roster.rs) is the sole gate.
    db.conn
        .execute(
            "INSERT INTO roster_presence
                (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
             VALUES ('new-source', 'new-id', 'principal', ?1, 'heard', 1)",
            params![blob_param(principal.as_bytes())],
        )
        .unwrap();
}

/// Same contract as the `roster_entity` test above, for
/// `roster_activity.live`.
#[test]
fn drop_legacy_value_enum_checks_drops_the_roster_activity_check_and_keeps_rows() {
    let db = KernelDb::temporary().unwrap();
    let principal = PrincipalId::new();
    db.conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
             VALUES ('principal', ?1, 1)",
            params![blob_param(principal.as_bytes())],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO roster_presence
                (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
             VALUES ('kept-source', 'kept-id', 'principal', ?1, 'bound', 1)",
            params![blob_param(principal.as_bytes())],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO roster_activity (source, source_local_id, live, recorded_at)
             VALUES ('kept-source', 'kept-id', 1, 1)",
            [],
        )
        .unwrap();
    put_roster_activity_back_on_its_legacy_shape(&db.conn);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let sql: String = db
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_activity'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!sql.contains("CHECK (live IN ("), "the value-enum CHECK must be gone, got: {sql}");
    let kept: i64 = db
        .conn
        .query_row("SELECT live FROM roster_activity WHERE source = 'kept-source'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(kept, 1);

    // `live` was a strict 0/1 boolean under the retired CHECK. The only
    // writer (`roster_reconcile_presence`) always writes 0 or 1; this
    // pins that the schema itself no longer enforces it.
    db.conn
        .execute(
            "INSERT INTO roster_presence
                (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
             VALUES ('new-source', 'new-id', 'principal', ?1, 'bound', 1)",
            params![blob_param(principal.as_bytes())],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO roster_activity (source, source_local_id, live, recorded_at)
             VALUES ('new-source', 'new-id', 2, 1)",
            [],
        )
        .unwrap();
}

/// Same contract as the `roster_entity` test above, for
/// `roster_status.availability`.
#[test]
fn drop_legacy_value_enum_checks_drops_the_roster_status_check_and_keeps_rows() {
    let db = KernelDb::temporary().unwrap();
    let principal = PrincipalId::new();
    db.conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
             VALUES ('principal', ?1, 1)",
            params![blob_param(principal.as_bytes())],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO roster_status (entity_kind, entity_id, availability, recorded_at)
             VALUES ('principal', ?1, 'active', 1)",
            params![blob_param(principal.as_bytes())],
        )
        .unwrap();
    put_roster_status_back_on_its_legacy_shape(&db.conn);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let sql: String = db
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_status'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!sql.contains("CHECK (availability IN ("), "the value-enum CHECK must be gone, got: {sql}");
    let kept: String = db
        .conn
        .query_row("SELECT availability FROM roster_status WHERE entity_kind = 'principal'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(kept, "active");

    // 'sideways' was never one of the retired CHECK's four values. It
    // now inserts cleanly — `Availability::parse` (roster.rs) is the
    // sole gate.
    let second = PrincipalId::new();
    db.conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
             VALUES ('principal', ?1, 1)",
            params![blob_param(second.as_bytes())],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO roster_status (entity_kind, entity_id, availability, recorded_at)
             VALUES ('principal', ?1, 'sideways', 1)",
            params![blob_param(second.as_bytes())],
        )
        .unwrap();
}

/// The hazard `drop_legacy_value_enum_checks` exists to avoid:
/// `roster_entity` is the FK parent of `roster_presence` and
/// `roster_status` (`ON DELETE CASCADE`), and `roster_presence` is in
/// turn the parent of `roster_activity`. With foreign keys enforced —
/// the connection's normal state, set by `init_connection` — a naive
/// rebuild of `roster_entity` performs an implicit `DELETE FROM` under
/// `DROP TABLE` and fires every one of those cascades, emptying the
/// live roster. Falsified by removing the `PRAGMA foreign_keys = OFF`
/// window from `drop_legacy_value_enum_checks`: every child row seeded
/// below was gone after `apply_additive_migrations`.
#[test]
fn drop_legacy_value_enum_checks_does_not_cascade_delete_the_live_roster() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("roster-cascade"));
    insert_context_with_doc(&db, &row, ws_id);
    let entity = RosterEntity::Context(row.context_id);

    db.roster_reconcile_presence(
        "cascade-source",
        LivenessKind::Bound,
        &[PresenceSnapshotRow {
            source_local_id: "cascade-peer".into(),
            entity,
            entity_label: Some("cascade".into()),
            host: Some("localhost".into()),
            pid: Some(4321),
            proc_start: Some(1000),
            observed_at: Some(1000),
            live: true,
        }],
        1000,
    )
    .unwrap();
    db.roster_write_status(entity, Some("cascade"), Some("hi"), Availability::Active, Some(1000), 1000)
        .unwrap();

    let count = |table: &str| -> i64 {
        db.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    };
    // Confirm the fixture actually populated every child table before
    // touching anything, so a later count of 0 unambiguously means the
    // migration deleted rows rather than never having seen any.
    assert_eq!(count("roster_presence"), 1);
    assert_eq!(count("roster_activity"), 1);
    assert_eq!(count("roster_status"), 1);

    put_roster_entity_back_on_its_legacy_shape(&db.conn);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    assert_eq!(count("roster_entity"), 1, "the entity row must survive its own rebuild");
    assert_eq!(
        count("roster_presence"),
        1,
        "roster_entity's rebuild must not cascade into roster_presence"
    );
    assert_eq!(
        count("roster_activity"),
        1,
        "the cascade must not reach two levels down into roster_activity"
    );
    assert_eq!(count("roster_status"), 1, "roster_entity's rebuild must not cascade into roster_status");
}

/// `DROP TABLE` takes a table's triggers with it. After a
/// `roster_entity` rebuild, `roster_entity_context_must_exist` must
/// both still be listed in `sqlite_master` and still fire — checking
/// only the listing would miss a trigger body silently changed by a
/// bad `SCHEMA` edit. Falsified by skipping the post-rebuild `SCHEMA`
/// re-run in `drop_legacy_value_enum_checks`: the trigger disappeared
/// and the phantom insert below succeeded instead of erroring.
#[test]
fn drop_legacy_value_enum_checks_restores_the_roster_entity_trigger() {
    let db = KernelDb::temporary().unwrap();
    put_roster_entity_back_on_its_legacy_shape(&db.conn);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let trigger_count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'trigger' AND name = 'roster_entity_context_must_exist'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trigger_count, 1, "the joinability trigger must survive the rebuild");

    let phantom = ContextId::new();
    let err = db
        .conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
             VALUES ('context', ?1, 1)",
            params![blob_param(phantom.as_bytes())],
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("never minted"),
        "expected the joinability trigger's message, got: {err}"
    );
}

/// `DROP TABLE` takes a table's indexes with it too. After a
/// `roster_presence` rebuild, `idx_roster_presence_entity` must still
/// exist. Falsified the same way as the trigger test above.
#[test]
fn drop_legacy_value_enum_checks_restores_the_roster_presence_index() {
    let db = KernelDb::temporary().unwrap();
    put_roster_presence_back_on_its_legacy_shape(&db.conn);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let index_count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'index' AND name = 'idx_roster_presence_entity'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(index_count, 1, "idx_roster_presence_entity must survive the rebuild");
}

/// A real `documents` row, because `doc_snapshots.document_id` is a
/// foreign key and a snapshot for a document that does not exist would
/// not be a case this migration ever meets.
fn setup_snapshot_document(db: &KernelDb) -> ContextId {
    let ws_id = setup_test_db(db);
    let doc_id = ContextId::new();
    db.insert_document(&DocumentRow {
        document_id: doc_id,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: now_millis(),
        created_by: PrincipalId::system(),
    })
    .unwrap();
    doc_id
}

/// Bring `doc_snapshots` back to the shape that carried `content`, and
/// seed a row through it, so the next open has something real to migrate.
fn put_doc_snapshots_back_with_its_content_column(conn: &Connection, doc: ContextId) {
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         DROP TABLE doc_snapshots;
         CREATE TABLE doc_snapshots (
             document_id BLOB    NOT NULL PRIMARY KEY,
             seq         INTEGER NOT NULL,
             version     INTEGER NOT NULL,
             state       BLOB    NOT NULL,
             content     TEXT    NOT NULL,
             created_at  INTEGER NOT NULL
                 DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
             FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
         );
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO doc_snapshots (document_id, seq, version, state, content, created_at)
         VALUES (?1, 7, 3, X'DEADBEEF', 'the derived copy', 1)",
        params![blob_param(doc.as_bytes())],
    )
    .unwrap();
}

/// A database still carrying `doc_snapshots.content` loses the column on
/// the next open, and the row it held keeps every other value. The text
/// was recoverable from `state` all along, so dropping it loses nothing
/// a reader could have wanted.
#[test]
fn the_doc_snapshots_content_column_is_dropped_and_the_row_survives() {
    let db = KernelDb::temporary().unwrap();
    let doc = setup_snapshot_document(&db);
    put_doc_snapshots_back_with_its_content_column(&db.conn, doc);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let sql: String = db
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'doc_snapshots'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!sql.contains("content"), "the content column must be gone, got: {sql}");

    let row = db.load_latest_snapshot(doc).unwrap().expect("the row must survive the drop");
    assert_eq!(row.seq, 7, "seq must survive");
    assert_eq!(row.version, 3, "version must survive");
    assert_eq!(row.state, vec![0xDE, 0xAD, 0xBE, 0xEF], "state must survive");
    assert_eq!(row.created_at, 1, "created_at must survive, and must not shift a column left");
}

/// The drop is guarded, so a second open does not re-run it — an
/// unguarded `ALTER TABLE ... DROP COLUMN` on a column that is already
/// gone is an error, not a no-op.
#[test]
fn dropping_the_doc_snapshots_content_column_is_idempotent() {
    let db = KernelDb::temporary().unwrap();
    let doc = setup_snapshot_document(&db);
    put_doc_snapshots_back_with_its_content_column(&db.conn, doc);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
}

/// Bring `contexts.consent_mode` and `presets.consent_mode` back, with a
/// non-default value on each row, so the next open has something real to
/// migrate.
fn put_consent_mode_columns_back(conn: &Connection, ctx: ContextId, preset: PresetId) {
    conn.execute(
        "ALTER TABLE contexts ADD COLUMN consent_mode TEXT NOT NULL DEFAULT 'collaborative'",
        [],
    )
    .unwrap();
    conn.execute(
        "ALTER TABLE presets ADD COLUMN consent_mode TEXT NOT NULL DEFAULT 'collaborative'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE contexts SET consent_mode = 'autonomous' WHERE context_id = ?1",
        params![blob_param(ctx.as_bytes())],
    )
    .unwrap();
    conn.execute(
        "UPDATE presets SET consent_mode = 'autonomous' WHERE preset_id = ?1",
        params![blob_param(preset.as_bytes())],
    )
    .unwrap();
}

/// A database still carrying `contexts.consent_mode` and
/// `presets.consent_mode` loses both columns on the next open, and the
/// rows they held keep every other value. Nothing reads the field any
/// more — the migration exists only to leave an old database loadable.
#[test]
fn the_consent_mode_columns_are_dropped_and_rows_survive() {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let row = make_context_row(Some("consent-drop-ctx"));
    let ctx = row.context_id;
    insert_context_with_doc(&db, &row, ws);
    let preset = insert_test_preset(&db, "consent-drop-preset");

    put_consent_mode_columns_back(&db.conn, ctx, preset);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let contexts_sql: String = db
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'contexts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !contexts_sql.contains("consent_mode"),
        "contexts.consent_mode must be gone, got: {contexts_sql}"
    );
    let presets_sql: String = db
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'presets'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !presets_sql.contains("consent_mode"),
        "presets.consent_mode must be gone, got: {presets_sql}"
    );

    let loaded = db.get_context(ctx).unwrap().expect("the context row must survive the drop");
    assert_eq!(loaded.label, Some("consent-drop-ctx".to_string()));
    let loaded_preset =
        db.get_preset(preset).unwrap().expect("the preset row must survive the drop");
    assert_eq!(loaded_preset.label, "consent-drop-preset");
}

/// The drop is guarded, so a second open does not re-run it — an
/// unguarded `ALTER TABLE ... DROP COLUMN` on a column that is already
/// gone is an error, not a no-op.
#[test]
fn dropping_the_consent_mode_columns_is_idempotent() {
    let db = KernelDb::temporary().unwrap();
    let ws = setup_test_db(&db);
    let row = make_context_row(Some("consent-drop-idempotent"));
    let ctx = row.context_id;
    insert_context_with_doc(&db, &row, ws);
    let preset = insert_test_preset(&db, "consent-drop-idempotent-preset");
    put_consent_mode_columns_back(&db.conn, ctx, preset);

    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
}

/// Every rebuild in `rebuild_value_enum_tables` carries its table's
/// shape a second time, beside `SCHEMA`'s own copy of it. A rebuilt
/// table must therefore end up with the shape a fresh database gets:
/// the same columns, types, nullability, defaults and primary key, and
/// the same foreign keys. An edit to `SCHEMA` that forgets its rebuild
/// spec fails here, instead of silently dropping a column the next time
/// an older kernel opens its database.
#[test]
fn a_rebuilt_table_has_the_shape_a_fresh_database_gets() {
    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}|{}|{}",
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    row.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .collect::<SqliteResult<Vec<_>>>()
            .unwrap()
    }
    fn foreign_keys(conn: &Connection, table: &str) -> Vec<String> {
        conn.prepare(&format!("PRAGMA foreign_key_list({table})"))
            .unwrap()
            .query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}|{}",
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    row.get::<_, String>(6)?,
                ))
            })
            .unwrap()
            .collect::<SqliteResult<Vec<_>>>()
            .unwrap()
    }

    let fresh = KernelDb::temporary().unwrap();
    let rebuilt = KernelDb::temporary().unwrap();
    put_backends_back_on_its_pre_codex_shape(&rebuilt.conn);
    put_roster_entity_back_on_its_legacy_shape(&rebuilt.conn);
    put_roster_presence_back_on_its_legacy_shape(&rebuilt.conn);
    put_roster_activity_back_on_its_legacy_shape(&rebuilt.conn);
    put_roster_status_back_on_its_legacy_shape(&rebuilt.conn);

    KernelDb::apply_additive_migrations(&rebuilt.conn).unwrap();

    for table in
        ["backends", "roster_entity", "roster_presence", "roster_activity", "roster_status"]
    {
        assert_eq!(
            columns(&rebuilt.conn, table),
            columns(&fresh.conn, table),
            "{table}'s rebuild spec has drifted from SCHEMA"
        );
        assert_eq!(
            foreign_keys(&rebuilt.conn, table),
            foreign_keys(&fresh.conn, table),
            "{table}'s rebuild spec lost or changed a foreign key"
        );
    }
}

/// The joinability guarantee at the schema level: an `entity_kind =
/// 'context'` row must name a context this kernel actually minted. A
/// phantom id (never inserted into `contexts`) is refused outright.
#[test]
fn roster_entity_context_must_exist_trigger_refuses_a_phantom_context() {
    let db = KernelDb::temporary().unwrap();
    let _ws_id = setup_test_db(&db);
    let phantom = ContextId::new();
    let err = db
        .conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
             VALUES ('context', ?1, 1)",
            params![blob_param(phantom.as_bytes())],
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("never minted"),
        "expected the joinability trigger's message, got: {err}"
    );
}

/// A `principal` entity has no backing table to check against — the
/// trigger's `WHEN` is scoped to `entity_kind = 'context'` only, so an
/// arbitrary principal id must insert cleanly (documented in
/// `roster.rs`'s module doc as Rust-API-only discipline for that half).
#[test]
fn roster_entity_principal_kind_is_not_checked_against_any_table() {
    let db = KernelDb::temporary().unwrap();
    db.conn
        .execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
             VALUES ('principal', ?1, 1)",
            params![blob_param(PrincipalId::new().as_bytes())],
        )
        .unwrap();
}


//! The additive migration ladder and its generic table rebuilds.

use super::*;

/// The retired risk classifier's global pre_call hook: its id and the body
/// path its rc installed. Opening a DB removes a row matching both.
pub(crate) const RETIRED_CLASSIFIER_HOOK: (&str, &str) = ("lfm2d-advisory", "/config/rc/lib/hooks/lfm2d.kai");

/// Per-context env keys the retired classifier's rc exported. Opening a DB
/// removes them; nothing reads them.
pub(crate) const RETIRED_CLASSIFIER_ENV: &[&str] = &["LFM2D_MODE", "LFM2D_BENIGN_LABEL", "LFM2D_URL"];

impl KernelDb {
    /// Under WAL, `synchronous = NORMAL` fsyncs at checkpoint instead of on
    /// every commit, and stays consistent through a process crash; only a
    /// power loss or kernel panic can drop the most recent commits. The
    /// default `FULL` fsyncs every commit, and a streaming turn commits once
    /// per delta.
    ///
    /// `journal_size_limit` caps how large the `-wal` file is allowed to grow
    /// before a checkpoint truncates it back down; SQLite's default is -1
    /// (no limit), which lets the WAL's high-water mark become permanent disk
    /// cost with no correctness benefit.
    pub(super) fn init_connection(conn: &Connection) -> SqliteResult<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;
             PRAGMA journal_size_limit = 67108864;",
        )?;
        Ok(())
    }

    /// Column backfills and guarded configuration migrations for existing DBs.
    /// An `embedding_config` in an older shape (a builtin model or a service
    /// endpoint) is replaced by an empty table: the kernel ships no backend
    /// to put in its place.
    /// The project's stance is "schema is truth, bump = wipe", but a single
    /// `ADD COLUMN ... DEFAULT 0` is cheap and spares a live kernel a wipe.
    /// Each ALTER is guarded: a "duplicate column" error on a fresh DB (the
    /// column is already in `SCHEMA`) is expected and ignored. Anything else
    /// — a locked file, a genuinely malformed ALTER — fails loud instead of
    /// leaving the DB silently short a column that later code assumes exists.
    pub(super) fn apply_additive_migrations(conn: &Connection) -> KernelDbResult<()> {
        let legacy_embedding: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('embedding_config'))
                AND NOT EXISTS(SELECT 1 FROM pragma_table_info('embedding_config') WHERE name = 'backend')",
            [], |row| row.get(0),
        )?;
        if legacy_embedding {
            // The builtin-model and service-endpoint rows name no backend and
            // there is no default to supply, so the rebuilt table starts empty.
            // The warning names what the row held, so the operator can
            // choose a backend and model for it.
            let held_column = ["endpoint", "model_dir"].into_iter().find(|column| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('embedding_config') WHERE name = ?1)",
                    [column], |row| row.get::<_, bool>(0),
                ).unwrap_or(false)
            });
            let held: Option<String> = match held_column {
                Some(column) => conn
                    .query_row(&format!("SELECT {column} FROM embedding_config WHERE id = 1"), [], |row| row.get(0))
                    .optional()?,
                None => None,
            };
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("DROP TABLE embedding_config;
                CREATE TABLE embedding_config (
                    id INTEGER NOT NULL PRIMARY KEY DEFAULT 1 CHECK (id = 1),
                    enabled INTEGER NOT NULL DEFAULT 1,
                    backend TEXT NOT NULL CHECK (backend != ''),
                    model TEXT NOT NULL CHECK (model != ''),
                    dimensions INTEGER NOT NULL CHECK (dimensions > 0),
                    profile TEXT NOT NULL DEFAULT 'plain',
                    timeout_ms INTEGER NOT NULL CHECK (timeout_ms > 0),
                    max_in_flight INTEGER NOT NULL CHECK (max_in_flight > 0),
                    max_context_bytes INTEGER NOT NULL CHECK (max_context_bytes > 0)
                );")?;
            tx.commit()?;
            tracing::warn!(held = held.as_deref().unwrap_or("nothing"), "dropped the old embedding configuration; \
                the semantic index is off until embedding_config names a backend and model (docs/synthesis.md)");
        }
        // A database written before context characters still has the columns
        // they replaced; `contexts.provider` is in every such database and in
        // no other. Decided before anything below changes it.
        let pre_context_character = Self::has_column(conn, "contexts", "provider")?;
        let alters = [
            "ALTER TABLE context_continuations ADD COLUMN invalidated_through INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE approval_pair_handoffs ADD COLUMN abandoned_reason TEXT",
            "ALTER TABLE contexts ADD COLUMN concluded_at INTEGER",
            "ALTER TABLE tracks ADD COLUMN score_context_id BLOB",
            "ALTER TABLE tracks ADD COLUMN clock_kind TEXT NOT NULL DEFAULT 'system'",
            "ALTER TABLE contexts ADD COLUMN last_activity_at INTEGER",
            "ALTER TABLE contexts ADD COLUMN promoted_at INTEGER",
            "ALTER TABLE contexts ADD COLUMN demoted_at INTEGER",
            "ALTER TABLE contexts ADD COLUMN paused_at INTEGER",
            "ALTER TABLE tracks ADD COLUMN deleted_at INTEGER",
            "ALTER TABLE hooks ADD COLUMN action_ask_description TEXT",
            "ALTER TABLE contexts ADD COLUMN origin_host TEXT",
            "ALTER TABLE hooks ADD COLUMN action_kaish_path TEXT",
            "ALTER TABLE context_usage ADD COLUMN cache_ttl_secs INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE turns ADD COLUMN cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE backends ADD COLUMN idle_timeout_secs INTEGER \
                 CHECK (idle_timeout_secs IS NULL OR idle_timeout_secs > 0)",
            "ALTER TABLE backends ADD COLUMN max_concurrent INTEGER \
                 CHECK (max_concurrent IS NULL OR max_concurrent > 0)",
            "ALTER TABLE characters ADD COLUMN handoff_ctx BLOB REFERENCES contexts(context_id) ON DELETE SET NULL",
            "ALTER TABLE characters ADD COLUMN root INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE characters ADD COLUMN root_ctx BLOB REFERENCES contexts(context_id) ON DELETE SET NULL",
            "ALTER TABLE characters ADD COLUMN context_type TEXT",
            "ALTER TABLE characters ADD COLUMN rc_dir TEXT",
            "ALTER TABLE characters ADD COLUMN console_ctx BLOB REFERENCES contexts(context_id) ON DELETE SET NULL",
            "ALTER TABLE characters ADD COLUMN mcp_hub_ctx BLOB REFERENCES contexts(context_id) ON DELETE SET NULL",
            "ALTER TABLE backend_models ADD COLUMN max_tokens INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0)",
            "ALTER TABLE backend_models ADD COLUMN temperature REAL \
                 CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0))",
            "ALTER TABLE backend_models ADD COLUMN top_p REAL CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0))",
            "ALTER TABLE backend_models ADD COLUMN effort TEXT",
            "ALTER TABLE backend_models ADD COLUMN thinking_budget INTEGER \
                 CHECK (thinking_budget IS NULL OR thinking_budget > 0)",
            "ALTER TABLE backend_models ADD COLUMN thinking_style TEXT",
        ];
        for sql in alters {
            match conn.execute(sql, []) {
                Ok(_) => {}
                Err(e) if is_duplicate_column_error(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        // Accountability moved from the sheet to the context forest, so the
        // column that held it goes. A drop is the one shape the additive
        // ladder above cannot express, and it is guarded on the column
        // still being there: a fresh database never had it.
        let has_accountable_to: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('characters') WHERE name = 'accountable_to')",
            [], |row| row.get(0),
        )?;
        if has_accountable_to {
            conn.execute("ALTER TABLE characters DROP COLUMN accountable_to", [])?;
            tracing::info!("dropped characters.accountable_to; accountability is a relation between contexts");
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS context_continuations (
                context_id BLOB NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
                epoch INTEGER NOT NULL,
                opened_at INTEGER NOT NULL,
                last_request_at INTEGER,
                yielded_at INTEGER,
                signed_off_at INTEGER,
                interrupted_at INTEGER,
                interrupted_by BLOB
            )",
        )?;
        for sql in [
            "ALTER TABLE context_continuations ADD COLUMN last_request_at INTEGER",
            "ALTER TABLE context_continuations ADD COLUMN interrupted_at INTEGER",
            "ALTER TABLE context_continuations ADD COLUMN interrupted_by BLOB",
        ] {
            match conn.execute(sql, []) {
                Ok(_) => {}
                Err(error) if is_duplicate_column_error(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Self::migrate_preset_cast_narrowing(conn)?;
        Self::drop_legacy_value_enum_checks(conn)?;
        Self::migrate_well_known_lost_found(conn)?;
        Self::migrate_doc_kind_file_collapse(conn)?;
        Self::migrate_archived_context_state(conn)?;
        Self::migrate_label_index_live_only(conn)?;
        Self::migrate_subtree_fork_kind_clear(conn)?;
        Self::drop_doc_snapshots_content_column(conn)?;
        Self::drop_consent_mode_columns(conn)?;
        // Review has no configured default; the lineage root holds that
        // authority (`docs/approval-identity.md`).
        conn.execute_batch("DROP TABLE IF EXISTS approval_identity_config")?;
        Self::remove_retired_classifier(conn)?;
        Self::migrate_house_capability(conn)?;
        if pre_context_character {
            Self::ensure_legacy_cast_tables(conn)?;
            Self::ensure_pre_context_character_columns(conn)?;
            Self::migrate_senpai_edges(conn)?;
            Self::drop_old_routing(conn)?;
        }
        Self::drop_structural_edges(conn)?;
        if pre_context_character {
            Self::migrate_context_characters(conn)?;
            Self::drop_pre_context_character_columns(conn)?;
        }
        Self::migrate_character_slots(conn)?;
        Self::migrate_context_fold(conn)?;
        Self::migrate_console_label(conn)?;
        Self::ensure_context_guards(conn)?;
        Ok(())
    }

    /// Remove the retired risk classifier's global pre_call hook row and the
    /// per-context env its rc exported. Runs after the column backfills,
    /// because the hook match names `action_kaish_path`.
    pub(super) fn remove_retired_classifier(conn: &Connection) -> KernelDbResult<()> {
        let (hook_id, hook_path) = RETIRED_CLASSIFIER_HOOK;
        let removed = conn.execute(
            "DELETE FROM hooks WHERE hook_id = ?1 AND action_kind = 'kaish_path' AND action_kaish_path = ?2",
            [hook_id, hook_path],
        )?;
        if removed > 0 {
            tracing::warn!(hook_id, "removed the retired classifier pre_call hook");
        }
        for key in RETIRED_CLASSIFIER_ENV {
            let removed = conn.execute("DELETE FROM context_env WHERE key = ?1", [key])?;
            if removed > 0 {
                tracing::info!(key, contexts = removed, "removed a retired classifier env var");
            }
        }
        Ok(())
    }

    /// Drops the unread `contexts.consent_mode` and `presets.consent_mode`
    /// columns so an older database keeps loading; guarded on
    /// `PRAGMA table_info` so a fresh database, whose `CREATE TABLE` never
    /// had the column, does nothing and this stays safe to call on every
    /// open.
    pub(super) fn drop_consent_mode_columns(conn: &Connection) -> KernelDbResult<()> {
        for table in ["contexts", "presets"] {
            let has_column = conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<SqliteResult<Vec<String>>>()?
                .iter()
                .any(|name| name == "consent_mode");
            if has_column {
                conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN consent_mode;"))?;
            }
        }
        Ok(())
    }

    /// `doc_snapshots.content` held a document's plain text beside `state`,
    /// which is the encoded whole document and already contains it. Nothing
    /// read the column: the single reader returns the row whole and every
    /// caller uses `state`, and a `state` that fails to decode skips the
    /// document rather than falling back to the text — so it was never the
    /// only readable copy. It cost about a fifth of the database.
    ///
    /// Guarded on `PRAGMA table_info` so a fresh database, which never had
    /// the column, does nothing and this stays safe to call on every open.
    /// `doc_snapshots` carries no trigger, no index and no child table, so
    /// dropping a column needs none of the rebuild machinery the value-enum
    /// `CHECK`s did.
    ///
    /// Dropping a column rewrites the table's rows but does not return pages
    /// to the filesystem. A `VACUUM` does, and that is an operator's call:
    /// running one at boot against a database this size would hold the kernel
    /// off its own startup for as long as the rewrite takes.
    pub(super) fn drop_doc_snapshots_content_column(conn: &Connection) -> KernelDbResult<()> {
        let has_column = conn
            .prepare("PRAGMA table_info(doc_snapshots)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<SqliteResult<Vec<String>>>()?
            .iter()
            .any(|name| name == "content");
        if has_column {
            conn.execute_batch("ALTER TABLE doc_snapshots DROP COLUMN content;")?;
        }
        Ok(())
    }

    /// Drop a value-enum `CHECK` SQLite cannot widen in place, rather than
    /// grow it — extends the ruling against `approval_events.kind`'s CHECK
    /// (`approval-ledger/src/schema.rs`) to every closed-enum column in this
    /// file. A value-enum CHECK gates one point in the type's history; the
    /// next value always needs one more open-ended migration, and the type
    /// that already owns the enum (`BackendKind`, `RosterEntity`,
    /// `LivenessKind`, `Availability`) is the durable place to keep gating
    /// it. `SCHEMA`'s `CREATE TABLE IF NOT EXISTS` only ever reaches a fresh
    /// database, so every one of these needs the same rebuild a fresh
    /// database's schema already reflects.
    ///
    /// Drops:
    /// - `backends.kind`
    /// - `roster_entity.entity_kind`
    /// - `roster_presence.liveness_kind`
    /// - `roster_activity.live`
    /// - `roster_status.availability`
    ///
    /// `roster_entity` is the FK parent of `roster_presence` and
    /// `roster_status` (`ON DELETE CASCADE`), and `roster_presence` is in
    /// turn the parent of `roster_activity`, also cascade. With foreign keys
    /// enforced, `DROP TABLE roster_entity` performs an implicit `DELETE
    /// FROM` that fires every one of those edges and empties the live
    /// roster. `PRAGMA foreign_keys` is a no-op inside a transaction, so it
    /// is turned off before the rebuilds' shared transaction starts and
    /// restored to whatever it was on entry once that transaction has
    /// committed or rolled back — on every return path, including the error
    /// one. `PRAGMA foreign_key_check` runs before the commit as a second
    /// line of defense: any row it reports fails the whole rebuild loudly
    /// rather than committing a database missing rows it once had.
    ///
    /// Each rebuild is independently guarded (see
    /// [`Self::rebuild_table_dropping_check`]), so a database already on the
    /// checkless shape does none of them and this is a no-op safe to call on
    /// every open. `DROP TABLE` takes a table's indexes and triggers with
    /// it, so a rebuild re-runs `SCHEMA` afterward — every statement there
    /// is `IF NOT EXISTS`, so this restores `idx_roster_presence_entity` and
    /// `roster_entity_context_must_exist` and no-ops on everything else.
    pub(super) fn drop_legacy_value_enum_checks(conn: &Connection) -> KernelDbResult<()> {
        let foreign_keys_were_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        let rebuilt = Self::rebuild_value_enum_tables(conn);
        let restore_sql =
            if foreign_keys_were_on != 0 { "PRAGMA foreign_keys = ON;" } else { "PRAGMA foreign_keys = OFF;" };
        let restored = conn.execute_batch(restore_sql);

        let rebuilt = match (rebuilt, restored) {
            (Ok(rebuilt), Ok(())) => rebuilt,
            (Err(e), _) => return Err(e),
            (Ok(_), Err(e)) => return Err(e.into()),
        };
        if rebuilt {
            conn.execute_batch(SCHEMA)?;
        }
        Ok(())
    }

    /// Run every value-enum `CHECK` rebuild inside one transaction, so a
    /// mid-list failure leaves every table on whichever shape it already
    /// had rather than half-migrated. Returns whether any table actually
    /// rebuilt.
    pub(super) fn rebuild_value_enum_tables(conn: &Connection) -> KernelDbResult<bool> {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let outcome = (|| -> KernelDbResult<bool> {
            let mut rebuilt = false;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "backends",
                "CHECK (kind IN (",
                "backend_id           BLOB NOT NULL PRIMARY KEY,
                 name                 TEXT NOT NULL UNIQUE,
                 kind                 TEXT NOT NULL,
                 base_url             TEXT,
                 api_key_env          TEXT,
                 api_key_file         TEXT,
                 key_optional         INTEGER NOT NULL DEFAULT 0,
                 request_timeout_secs INTEGER
                     CHECK (request_timeout_secs IS NULL OR request_timeout_secs > 0),
                 idle_timeout_secs    INTEGER
                     CHECK (idle_timeout_secs IS NULL OR idle_timeout_secs > 0),
                 max_concurrent       INTEGER
                     CHECK (max_concurrent IS NULL OR max_concurrent > 0),
                 created_at           INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 created_by           BLOB NOT NULL",
                "backend_id, name, kind, base_url, api_key_env, api_key_file, \
                 key_optional, request_timeout_secs, idle_timeout_secs, max_concurrent, created_at, created_by",
            )?;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "roster_entity",
                "CHECK (entity_kind IN (",
                "entity_kind   TEXT NOT NULL,
                 entity_id     BLOB NOT NULL,
                 label         TEXT,
                 first_seen_at INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 PRIMARY KEY (entity_kind, entity_id)",
                "entity_kind, entity_id, label, first_seen_at",
            )?;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "roster_presence",
                "CHECK (liveness_kind IN (",
                "source          TEXT NOT NULL,
                 source_local_id TEXT NOT NULL,
                 entity_kind     TEXT NOT NULL,
                 entity_id       BLOB NOT NULL,
                 liveness_kind   TEXT NOT NULL,
                 host            TEXT,
                 pid             INTEGER,
                 proc_start      INTEGER,
                 first_seen_at   INTEGER NOT NULL,
                 PRIMARY KEY (source, source_local_id),
                 FOREIGN KEY (entity_kind, entity_id)
                     REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE",
                "source, source_local_id, entity_kind, entity_id, liveness_kind, \
                 host, pid, proc_start, first_seen_at",
            )?;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "roster_activity",
                "CHECK (live IN (",
                "source          TEXT    NOT NULL,
                 source_local_id TEXT    NOT NULL,
                 live            INTEGER NOT NULL,
                 observed_at     INTEGER,
                 recorded_at     INTEGER NOT NULL,
                 PRIMARY KEY (source, source_local_id),
                 FOREIGN KEY (source, source_local_id)
                     REFERENCES roster_presence(source, source_local_id) ON DELETE CASCADE",
                "source, source_local_id, live, observed_at, recorded_at",
            )?;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "roster_status",
                "CHECK (availability IN (",
                "entity_kind  TEXT NOT NULL,
                 entity_id    BLOB NOT NULL,
                 status_text  TEXT,
                 availability TEXT NOT NULL,
                 observed_at  INTEGER,
                 recorded_at  INTEGER NOT NULL,
                 PRIMARY KEY (entity_kind, entity_id),
                 FOREIGN KEY (entity_kind, entity_id)
                     REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE",
                "entity_kind, entity_id, status_text, availability, observed_at, recorded_at",
            )?;

            let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                let table: String = row.get(0)?;
                return Err(KernelDbError::Validation(format!(
                    "dropping the value-enum CHECK left a dangling foreign key on {table}; \
                     refusing to commit the rebuild"
                )));
            }
            Ok(rebuilt)
        })();

        match &outcome {
            Ok(_) => conn.execute_batch("COMMIT;")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK;");
            }
        }
        outcome
    }

    /// Rebuild `table` to drop a legacy value-enum `CHECK` SQLite cannot
    /// alter in place: copy every row into a same-shaped table without the
    /// CHECK, drop the original, and rename the copy into its place.
    ///
    /// Guarded on `check_marker`, a substring unique to the retired CHECK
    /// clause — never the bare word `"CHECK"`, since a table can carry
    /// other CHECKs (`backends.request_timeout_secs`) that must survive
    /// this rebuild untouched. A table already on the checkless shape has
    /// no occurrence of `check_marker` in its stored SQL, so this is a
    /// no-op and safe to call on every open.
    ///
    /// `column_defs` is the rebuilt table's full column/key/FK list
    /// (everything that would sit between the parens of a `CREATE TABLE`);
    /// `shared_columns` is the comma-separated column list common to both
    /// shapes, used for the `INSERT ... SELECT` that carries every row
    /// across. Foreign key enforcement and the surrounding transaction are
    /// the caller's responsibility.
    ///
    /// Returns whether it rebuilt. A caller running this over several
    /// tables uses that to decide whether `DROP TABLE` took an index or
    /// trigger with it that now needs restoring.
    pub(super) fn rebuild_table_dropping_check(
        conn: &Connection,
        table: &str,
        check_marker: &str,
        column_defs: &str,
        shared_columns: &str,
    ) -> KernelDbResult<bool> {
        let sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .optional()?;
        let Some(sql) = sql else { return Ok(false) };
        if !sql.contains(check_marker) {
            return Ok(false);
        }
        let staging = format!("{table}__check_drop");
        conn.execute_batch(&format!(
            "CREATE TABLE {staging} (\n{column_defs}\n);
             INSERT INTO {staging} ({shared_columns})
                 SELECT {shared_columns} FROM {table};
             DROP TABLE {table};
             ALTER TABLE {staging} RENAME TO {table};",
        ))?;
        Ok(true)
    }

    /// One-time adoption of the pre-registry lost+found scheme
    /// (docs/drifting-dead-letters.md, slice 1): a database that predates
    /// `well_known_contexts` identified lost+found purely by the magic label
    /// `'lost+found'`. This backfills the registry row from whatever context
    /// currently holds that label, so `WellKnownRole::LostFound` lookups work
    /// on an upgraded database without anyone having to notice or act.
    ///
    /// `INSERT OR IGNORE` makes this idempotent across every `open()` — once
    /// a row exists for the role (whether from this backfill or a later
    /// rotation), the label stops being meaningful and this is a no-op
    /// forever after, even if the label is later reused or moved.
    pub(super) fn migrate_well_known_lost_found(conn: &Connection) -> KernelDbResult<()> {
        conn.execute(
            "INSERT OR IGNORE INTO well_known_contexts (role, context_id, assigned_at)
             SELECT 'lost_found', context_id, ?1
               FROM contexts WHERE label = 'lost+found'
               ORDER BY created_at LIMIT 1",
            params![now_millis()],
        )?;
        Ok(())
    }

    /// Collapse the retired `code`, `text`, and `config` document kinds into
    /// the one `file` kind. The three tags described a single-block,
    /// path-addressed document whose content is the file body; the `language`
    /// column already carried whatever distinguished them.
    ///
    /// Idempotent by construction — after the first run the `WHERE` clause
    /// matches zero rows, so this needs no `kernel_migrations` marker and is
    /// safe on every `open()`.
    pub(super) fn migrate_doc_kind_file_collapse(conn: &Connection) -> KernelDbResult<()> {
        conn.execute(
            "UPDATE documents SET doc_kind = 'file'
              WHERE doc_kind IN ('code', 'text', 'config')",
            [],
        )?;
        Ok(())
    }

    /// Bring `context_state` into agreement with `archived_at` on rows written
    /// before archiving set the state. Until then `archive_context` stamped
    /// only the timestamp, so every archived context read back as `live` and a
    /// caller switching on the enum alone saw a dead context as a running one.
    ///
    /// Idempotent by construction — after the first run the `WHERE` clause
    /// matches zero rows — so this needs no `kernel_migrations` marker and is
    /// safe on every `open()`.
    /// The label index covers live contexts only (`SCHEMA`,
    /// `idx_contexts_label`). A DB created when it also covered archived
    /// rows keeps that index — `CREATE INDEX IF NOT EXISTS` never replaces
    /// one — so it is rebuilt here when its SQL lacks the archived filter.
    /// Idempotent; a fresh DB matches and nothing runs.
    pub(super) fn migrate_label_index_live_only(conn: &Connection) -> KernelDbResult<()> {
        let current: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_contexts_label'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let live_only = current
            .as_deref()
            .is_some_and(|sql| sql.contains("archived_at IS NULL"));
        if live_only {
            return Ok(());
        }
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_contexts_label;
             CREATE UNIQUE INDEX idx_contexts_label
                 ON contexts(label) WHERE label IS NOT NULL AND archived_at IS NULL;",
        )?;
        Ok(())
    }

    pub(super) fn migrate_archived_context_state(conn: &Connection) -> KernelDbResult<()> {
        conn.execute(
            "UPDATE contexts SET context_state = 'archived'
              WHERE archived_at IS NOT NULL AND context_state <> 'archived'",
            [],
        )?;
        Ok(())
    }

    /// Clear the retired `subtree` fork kind from stored rows. The
    /// template-subtree fork variant is gone from `ForkKind`
    /// (`docs/fork-and-create.md`), and `fork_kind_from_sql` treats any
    /// unrecognized value as corruption rather than degrading silently —
    /// so a row a deleted template-subtree fork left behind must lose the
    /// tag before `get_context` (or any other reader) can read it back.
    ///
    /// Idempotent by construction — after the first run the `WHERE` clause
    /// matches zero rows — so this needs no `kernel_migrations` marker and
    /// is safe on every `open()`.
    pub(super) fn migrate_subtree_fork_kind_clear(conn: &Connection) -> KernelDbResult<()> {
        let cleared = conn.execute("UPDATE contexts SET fork_kind = NULL WHERE fork_kind = 'subtree'", [])?;
        if cleared > 0 {
            info!(cleared, "cleared retired 'subtree' fork_kind from stored contexts");
        }
        Ok(())
    }

    pub(super) fn has_column(conn: &Connection, table: &str, column: &str) -> KernelDbResult<bool> {
        Ok(conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"),
            [column], |row| row.get(0),
        )?)
    }

    pub(super) fn has_table(conn: &Connection, table: &str) -> KernelDbResult<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table], |row| row.get(0),
        )?)
    }

    pub(super) const HOUSE_MIGRATION: &'static str = "grant-house-capability-v1";

    /// Grant `house` once to every binding that existed before the
    /// capability did, so an upgraded kernel keeps the administration verbs
    /// on its contexts. Worker types (coder, toolie, musician) are the seats
    /// the capability narrows, so they do not get it; a binding that grants
    /// nothing stays empty, because `house` alone would make it look usable.
    ///
    /// Recorded in `kernel_migrations`: a seat that later drops `house` must
    /// not get it back on the next open. A fresh database records the marker
    /// with nothing to grant.
    pub(super) fn migrate_house_capability(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::HOUSE_MIGRATION], |row| row.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let granted = tx.execute(
            "INSERT OR IGNORE INTO context_binding_authorities (context_id, authority)
             SELECT b.context_id, 'house'
             FROM context_bindings b JOIN contexts c ON c.context_id = b.context_id
             WHERE c.context_type NOT IN ('coder', 'toolie', 'musician')
               AND (b.all_instances OR b.all_facades OR b.binding_admin
                    OR EXISTS (SELECT 1 FROM context_binding_instances WHERE context_id = b.context_id)
                    OR EXISTS (SELECT 1 FROM context_binding_tools WHERE context_id = b.context_id)
                    OR EXISTS (SELECT 1 FROM context_binding_facades WHERE context_id = b.context_id)
                    OR EXISTS (SELECT 1 FROM context_binding_authorities WHERE context_id = b.context_id))",
            [],
        )?;
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::HOUSE_MIGRATION])?;
        tx.commit()?;
        if granted > 0 {
            info!(granted, "granted the house capability to existing non-worker bindings");
        }
        Ok(())
    }

    /// Migrate the approval-ledger schema into the kernel DB. The ledger is
    /// DB-injected (`approval_ledger::migrate` borrows whatever connection
    /// the caller owns), so the kernel owns the single connection and the
    /// ledger's tables live beside the kernel's own — one DB, one backup,
    /// one WAL. `execute_batch`'s `CREATE TABLE IF NOT EXISTS` makes this
    /// idempotent across every open.
    pub(super) fn migrate_ledger(conn: &Connection) -> KernelDbResult<()> {
        approval_ledger::migrate(conn)?;
        // Jump the ledger's change generation forward by a fixed gap, once
        // per DB open — which for the live kernel is once per process start,
        // i.e. exactly the restart boundary this is meant to mark.
        //
        // The counter is durable, so it is already monotonic without this;
        // the gap is insurance for the cases that break that promise anyway
        // (a crash mid-transaction, a database restored from a backup), where
        // a client could otherwise be handed a generation it already held
        // with different facts sitting behind it. It also makes a restart
        // legible to a subscriber as a discontinuity rather than a silent
        // resume.
        //
        // Deliberately here rather than inside `approval_ledger::migrate`:
        // that function is idempotent and documented as safe to call on every
        // process start, so folding the jump into it would make the jump size
        // depend on how many times it happened to run. A second `KernelDb`
        // open in one process would over-gap, and that is fine — an extra
        // discontinuity costs a subscriber one poll, while a *missing* one
        // could cost it a stale view it has no way to notice.
        //
        // Failing the open is the right response to a failure here, rather
        // than logging and continuing: the counter is the only thing telling
        // subscribers a restart happened, so a kernel that came up unable to
        // advance it would hand out change notifications indistinguishable
        // from the previous boot's.
        approval_ledger::generation::bump_for_restart(conn)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod shape_tests;

# kernel_db.rs split plan

Working plan, deleted when the split lands. Target: `crates/kaijutsu-kernel/src/kernel_db.rs` (about 18,400
lines). The split is move-only, with no behavior change. Drafted by a read-only planner on 2026-11-01; nothing
here was compiled.

Line numbers are deliberately absent: every anchor is a function, type, const or `// ──` header name. Locate each
with `grep -n "fn NAME" crates/kaijutsu-kernel/src/kernel_db.rs`. **The dead-code sweep runs first and may delete
some named items** (`played_by` helpers such as `set_stored_played_by` and `update_played_by`, `read_opt_cast_id`,
the old migration-report reader). A name that no longer exists is skipped, not recreated.

Paths below are relative to `crates/kaijutsu-kernel/src/`. `kernel_db/X.rs` is a new module `X`. Its tests go in
`kernel_db/X/tests.rs`.

## 0. Conventions already in the repo (follow them)

- **Existing children.** `kernel_db.rs` declares `mod context_characters;` and `mod model_prices;`. It re-exports
  their public types with `pub use model_prices::{...}` and `pub use context_characters::{...}`.
- **Child shape.** A child file starts with a `//!` doc, then `use` lines. It pulls parent helpers with
  `use super::{KernelDb, KernelDbError, KernelDbResult, blob_param, now_millis, read_context_id, ...}`. Its methods
  are `impl KernelDb { ... }`. Items the parent or siblings need are `pub(super)`. Cross-crate items are
  `pub(crate)` or `pub`.
- **Child tests.** `context_characters.rs` ends with `#[cfg(test)]` then `mod tests;`. The file
  `kernel_db/context_characters/tests.rs` is flat, with no `mod tests { }` wrapper and no indentation. It starts with
  `use super::*;` followed by `use crate::kernel_db::{...};`. A descendant module can see its ancestors' private
  items.
- **Privacy rules.** A private item in `kernel_db.rs` is visible to every descendant. A private `fn` in a child's
  `impl KernelDb` is visible only inside that child; if another module calls it, it must become `pub(super)`. A
  `pub(super)` item of a child is visible in `kernel_db` and all its descendants, siblings included.
- **Style.** Never run `cargo fmt`, do not reformat by hand, match the surrounding style (`AGENTS.md`).

### Global mechanical rules

1. **Visibility.** Every moved item that is not `pub` and not already `pub(crate)` becomes `pub(super)`. Do not
   analyze who calls it.
2. **Imports.** Each new module starts with a `//!` line, then `use super::*;`. That one line is the only import
   line a step should need.
3. **Re-exports.** Every moved `pub` type, const or fn gets `pub use NAME::{...};` in `kernel_db.rs`, next to the
   existing `pub use` lines. Every moved `pub(crate)` item gets `pub(crate) use NAME::{...};`. External users
   include `kj/gate.rs` (`record_approval_tool_call`), `kj/drift.rs` (`record_approval_drift_block`), `kj/fork.rs`
   (`DEFAULT_HYDRATION_WINDOW`), `runtime/llm_stream.rs` (`HeldAskEnd`), `kj/context.rs` (`context_used_pct`,
   `validate_egress_host`, `ACTIVE_RING_CAPACITY`), and `kaijutsu-server` `beat.rs` (`PersistedTrack`,
   `PersistedAttachment`, `DEFAULT_HYDRATION_WINDOW`).
4. **Private struct fields** stay as they are.
5. **`#[cfg]` attributes.** Keep every `#[cfg(test)]` and `#[cfg(any(test, feature = "test-util"))]` on its item.
   Attributes and `///` docs travel with their item.
6. **impl blocks.** Collect a domain's methods into ONE `impl KernelDb { ... }` per new file, in original relative
   order. Keep the `// ── name ──` headers that precede them.
7. **Tests.** A test moves to `kernel_db/X/tests.rs`; append `#[cfg(test)]` then `mod tests;` at the bottom of
   `X.rs`. Dedent the moved tests by exactly 4 spaces (`sed 's/^    //'` on the extracted block). The file starts
   with `use super::*;`; add `use crate::kernel_db::*;` only if a helper is still missing. Test-local helpers that
   only that domain uses move with it; shared ones are in `test_support` (section 3).
8. **Test names** change path (`kernel_db::tests::foo` becomes `kernel_db::approvals::tests::foo`). The count does
   not change.

## 1. Target module map

### Parent: `kernel_db.rs` (stays)

Everything not named below stays. See section 2.

### `kernel_db/test_support.rs` (`#[cfg(test)] mod test_support;`)

The shared test helpers. See section 3.

### `kernel_db/schema.rs`

- `const SCHEMA`, moved whole as `pub(super) const SCHEMA: &str = r#"..."#;` with its doc comment.
- Parent: add `use schema::SCHEMA;`. Tests none.

### `kernel_db/migrate.rs` (the `apply_additive_migrations` ladder)

- Methods: `init_connection`, `apply_additive_migrations`, `has_column`, `has_table`, `remove_retired_classifier`,
  `drop_consent_mode_columns`, `drop_doc_snapshots_content_column`, `drop_legacy_value_enum_checks`,
  `rebuild_value_enum_tables`, `rebuild_table_dropping_check`, `migrate_well_known_lost_found`,
  `migrate_doc_kind_file_collapse`, `migrate_label_index_live_only`, `migrate_archived_context_state`,
  `migrate_subtree_fork_kind_clear`, `migrate_house_capability`, `migrate_ledger`.
- Const: associated `const HOUSE_MIGRATION`. Free consts `RETIRED_CLASSIFIER_HOOK`, `RETIRED_CLASSIFIER_ENV` (stay
  `pub(crate)`, no re-export).
- Stays in the parent: `is_duplicate_column_error`.
- Tests, `migrate/tests.rs`: `retired_gate_action_tables_are_dropped_on_open`,
  `drop_legacy_value_enum_checks_drops_the_pre_codex_backends_check_and_keeps_rows`,
  `rebuild_table_dropping_check_is_a_true_no_op_on_its_second_call`,
  `last_activity_at_migration_is_idempotent_no_op_on_fresh_db`,
  `origin_host_migration_is_idempotent_no_op_on_fresh_db`, `apply_additive_migrations_propagates_real_errors`,
  `house_capability_migrates_onto_pre_existing_bindings_once`,
  `cache_ttl_secs_migrates_onto_a_pre_existing_context_usage_table`,
  `cache_ttl_secs_migration_is_idempotent_and_preserves_existing_value`,
  `open_migrates_the_ledger_schema_and_reopen_is_idempotent`,
  `retired_classifier_cleanup_opens_a_hooks_table_without_kaish_path`,
  `retired_classifier_hook_and_env_are_removed`, `open_normalizes_a_stored_subtree_fork_kind_to_none`,
  `root_migration_adds_the_flag_and_drops_accountable_to`, helpers `insert_raw_kind_docs` and `raw_doc_kind`, the
  three `migrate_doc_kind_file_collapse_*` tests, `migrate_well_known_lost_found_backfills_and_is_idempotent`,
  `migrate_archived_context_state_backfills_a_disagreeing_row`, and from `mod label_index_tests` the helper
  `index_sql` plus `the_old_index_shape_is_rebuilt_at_open`.
- Second tests file `migrate/shape_tests.rs` (`#[cfg(test)] mod shape_tests;` in `migrate.rs`): everything from
  the header `// ── Roster: dropping the legacy value-enum CHECKs ──` through
  `roster_entity_principal_kind_is_not_checked_against_any_table`, kept intact (the `put_roster_*_back` helpers,
  roster `drop_legacy_value_enum_checks_*`, `setup_snapshot_document`,
  `put_doc_snapshots_back_with_its_content_column` and the `doc_snapshots` tests,
  `put_consent_mode_columns_back` and the consent tests, `a_rebuilt_table_has_the_shape_a_fresh_database_gets`,
  the roster trigger tests).

### `kernel_db/migrate_cast.rs`

- Methods: `ensure_legacy_cast_tables`, `migrate_character_slots`, `migrate_preset_cast_narrowing`,
  `pre_character_slots_backup_path`, `back_up_before_character_slots`.
- Tests, `migrate_cast/tests.rs`: everything under `// ── Context model pins + Track D preset cast narrowing ──`
  except `touch_context_activity_stamps_and_advances` (goes to `contexts`): helpers `with_cast_tables`,
  `legacy_cast`; `a_database_on_casts_is_copied_aside_before_it_migrates`, `a_failed_backup_refuses_naming_the_path`,
  `character_slots_migration_copies_each_characters_cast_and_drops_the_casts`,
  `character_slots_migration_refuses_before_the_context_character_migration`,
  `character_slots_migration_is_a_no_op_on_a_fresh_database`, `preset_cast_narrowing_drops_the_legacy_columns`,
  `preset_cast_narrowing_is_a_no_op_on_a_fresh_db`.

### `kernel_db/migrate_senpai.rs`

- Types and consts: `struct OldContext`, `const PRE_CONTEXT_CHARACTER_COLUMNS`, associated `const SENPAI_MIGRATION`.
- Methods: `ensure_pre_context_character_columns`, `migrate_senpai_edges`, `drop_old_routing`,
  `drop_pre_context_character_columns`, `drop_structural_edges`, `senpai_migration_report`.
- Tests, `migrate_senpai/tests.rs`: `a_reviewer_from_before_the_director_column_is_not_an_override`; the whole
  `// ── Dropping the structural edge rows ──` section (helpers `restore_edge_kind_column`, `insert_legacy_edge`,
  `edge_count`, the four `dropping_structural_edges_*` tests); the whole `// ── 22e. the senpai migration ──`
  section from `ensure_old_routing_schema` through
  `opening_a_pre_senpai_database_adds_the_column_and_maps_the_edges`, with its local helpers (`old_delegation`,
  `has_column`, `has_table`, `old_context`, `set_old_routing`, `senpai_open_steps`, `run_senpai_migration`,
  `set_stored_senpai`, `senpai_of`, `report_of`). Not in this file: `contexts_played_by_excludes_archived`,
  `name_for_resolves_sentinels_then_sheet_then_short_id` (to `llm_characters`), `open_read_only_*` (parent tests).
  The test-local `has_column`/`senpai_of` are plain fns, not the `KernelDb` methods; they move as is.

### `kernel_db/kernel_meta.rs` (kernel identity and quiesce)

- Type `QuiesceState`. Methods `ensure_singleton_kernel`, `kernel_id`, `set_kernel_label`, `legacy_rc_scripts`,
  `migration_applied`, `quiesce_state`, `set_quiesced`, `clear_quiesced`, `poison_quiesce_table_for_test` (keeps
  `#[cfg(test)]` and `pub(crate)`).
- Re-export `pub use kernel_meta::QuiesceState;`.
- Tests: `quiesce_flag_defaults_clear_and_roundtrips`, `quiesce_flag_survives_reopen`,
  `quiesce_read_failure_is_an_error_not_a_running_kernel`, `get_or_create_kernel_id_stable_across_calls`,
  `get_or_create_kernel_id_fresh_on_empty_db`.

### `kernel_db/approvals.rs` (asks, continuations, approval pairs)

- Types and fns: `HeldAskEnd` (the `pub(crate) enum` at the bottom of `kernel_db.rs`), `ContinuationOrigin`, free
  fns `record_approval_tool_call`, `record_approval_drift_block`.
- Methods: `abandon_unresolved_asks_on_restart`, `link_ask_blocks`, `ask_env`, `begin_continuation`,
  `record_continuation_request`, `admit_inference_request`, `continuation_epoch`, `record_continuation_yield`,
  `sign_off_continuation`, `record_continuation_interrupt`, `continuation_interrupt`, `automatic_resume_allowed`,
  `claim_automatic_resume`, `list_pending_asks`, `undelivered_answers`, `create_approval_ask`,
  `create_approval_ask_recorded`, `approval_tool_call`, `approval_for_drift_block`, `approval_drift_block`,
  `approval_pair_expected`, `approval_pair_ready`, `release_approval_pair`, `approval_pair_abandoned_reason`,
  `abandon_approval_pair`, `unpublished_approval_pairs`, `get_approval`, `redeem_ask`, `ask_redeemed`,
  `end_held_ask`.
- Stay in the parent: `conn_for_ledger`, `conn_for_test_mut`.
- Re-exports: `pub use approvals::ContinuationOrigin;` and
  `pub(crate) use approvals::{HeldAskEnd, record_approval_tool_call, record_approval_drift_block};`.
- Tests: `publication_handoffs_gain_abandonment_on_existing_databases`, helpers `pending_ask_for_context` and
  `approval_status`, `archive_context_rolls_back_when_its_ask_sweep_fails`,
  `archive_context_rolls_back_inside_a_committed_outer_transaction`,
  `archive_context_retries_the_sweep_for_an_already_archived_context`; from `label_index_tests`:
  `continuation_invalidation_migrates_without_closing_existing_work`,
  `a_performer_that_cannot_change_keeps_the_continuation`,
  `continuation_window_requires_a_matching_yielded_unsigned_epoch`,
  `interrupt_closes_a_yielded_continuation_independently_of_signoff`.

### `kernel_db/review.rs` (senpai chain, routing, authority)

- Types: `ReviewFrom`, `GovernedAct` + `impl GovernedAct`, `Authority`, `ReviewQuery` + `impl ReviewQuery`,
  `EffectiveReviewer` + `impl EffectiveReviewer`.
- Methods: `context_holder`, `context_display` (becomes `pub(super)`; `context_characters.rs` calls it),
  `senpai_of`, `list_kohai`, `route_review`, `at_top`, `lineage_root_of_context`, `lineage_root_or_reason`,
  `lineage_root`, `live_lineage_root`, `context_authority`, `may_place_under`.
- Re-export `pub use review::{ReviewFrom, GovernedAct, Authority, ReviewQuery, EffectiveReviewer};`.
- Tests: the `// ── 22d. review routing climbs the senpai chain ──` and `// ── 22d2. the one authority rule ──`
  sections (their setup helpers are in `test_support`), including
  `set_context_senpai_moves_a_kohai_and_refuses_cycles_and_missing_rows` and
  `commit_rotation_leaves_the_kohai_and_repoints_a_handoff_log`.

### `kernel_db/contexts.rs` (the contexts table)

- Types `ContextRow` + `impl ContextRow`.
- Methods: `insert_context_with_document`, `insert_context`, `write_context`, `get_context`, `update_label`,
  `update_settings`, `update_context_type`, `set_origin_host`, `update_workspace`, `touch_context_activity`,
  `list_active_contexts`, `list_all_contexts`, `resolve_context`, `find_context_by_label`. Uses `performer_sql!`
  (section 5).
- Re-export `pub use contexts::ContextRow;`.
- Tests: `context_lifecycle`, `label_uniqueness`, `tag_resolution`, `resolve_context_basic`, `null_labels_coexist`,
  `archive_excludes_from_active`, `context_row_to_context`, `roundtrip_create_and_recover`,
  `set_origin_host_round_trips_through_get_context`, `set_origin_host_unknown_context_is_not_found`,
  `origin_host_survives_the_context_dag_cte_read`, `touch_context_activity_stamps_and_advances`; from
  `// ── 29. Time-well ring placement ──`: `resolve_context_full_id_reaches_an_archived_context`,
  `insert_context_round_trips_placement_stamps`, `list_orders_by_last_activity_over_created_at`; from
  `label_index_tests`: `an_archived_context_keeps_its_label_and_frees_it`,
  `two_live_contexts_still_cannot_share_a_label`.

### `kernel_db/context_ring.rs` (state, archive, promote/demote, rotation)

- Consts and types: `ACTIVE_RING_CAPACITY`, `DemoteOutcome`, `PromoteOutcome`.
- Methods: `archive_context`, `held_drift_refusal` (stays `pub(crate)`), `archive_context_writes`,
  `commit_rotation`, `update_context_state`, `conclude_context`, `promote_context`, `demote_context`,
  `set_context_paused`.
- Re-export `pub use context_ring::{ACTIVE_RING_CAPACITY, DemoteOutcome, PromoteOutcome};`.
- Tests: `conclude_context_sets_state_timestamp_and_is_idempotent`, `conclude_context_rejects_archived`; the
  `// ── 29. Time-well ring placement ──` section minus the three tests listed under `contexts` and minus
  `block_provenance_roundtrip_and_cascade` (to `documents`); `archiving_sets_the_state_as_well_as_the_timestamp`,
  `resurrecting_restores_live_or_concluded_from_the_timestamps`.

### `kernel_db/context_fork.rs`

- Methods: `insert_forked_context`, `fork_context_config`, `copy_context_binding`.
- Tests: `fork_copies_binding`; the `// ── 26. Fork context config (composite) ──` section
  (`fork_context_config_full`, `fork_context_config_empty_source`,
  `fork_context_config_rolls_back_on_partial_failure`, `insert_forked_context_creates_rows_and_copies_config`,
  `insert_forked_context_rolls_back_rows_on_config_failure`,
  `insert_forked_context_drops_context_scoped_mcp_grants`).

### `kernel_db/lineage.rs` (forked_from tree and drift edges)

- Type `ContextEdgeRow`. Methods `fork_children`, `context_dag`, `fork_lineage`, `insert_edge`, `edges_from`,
  `edges_to`, `drift_provenance`, `delete_drift_edge`.
- Re-export `pub use lineage::ContextEdgeRow;`.
- Tests: `fork_lineage_3_deep`, `drift_edge_allows_dupes`, `drift_push_creates_edge`, `context_dag_recursive`,
  `context_dag_lists_every_child_under_its_own_parent`, `context_edge_cascade`.

### `kernel_db/fixtures.rs` (test-util context-character fixtures)

- Declared as `#[cfg(any(test, feature = "test-util"))] mod fixtures;`; keep per-fn cfg attrs.
- Methods: `note_senpai_intent`, `has_senpai_intent_table`, `senpai_intent`, `update_played_by`,
  `recast_test_character`, `assign_test_review_recasting`, `seed_test_slots`, `seed_test_slot_entries`,
  `assign_stand_in_character`, `is_stand_in_character`, `assign_context_character`, `assign_test_review`.
- Tests none.

### `kernel_db/context_bindings.rs`

- Methods: `upsert_context_binding`, `write_binding`, `get_context_binding`, `delete_context_binding`,
  `poison_context_binding_detail_table_for_test` (stays `pub(crate)`).
- Tests: everything under `// ── 23b. Context tool bindings CRUD` except `binding_with` (in `test_support`) and
  `fork_copies_binding` (in `context_fork`), plus `get_context_binding_reports_a_database_fault`.

### `kernel_db/context_env.rs` (shell, env, egress)

- Types `ContextShellRow`, `ContextEnvRow`.
- Methods: `write_context_shell`, `upsert_context_shell`, `get_context_shell`, `copy_context_shell`,
  `write_context_env`, `set_context_env`, `get_context_env`, `delete_context_env`, `clear_context_env`,
  `copy_context_env`, `write_context_egress`, `add_context_egress`, `remove_context_egress`,
  `list_context_egress`, `copy_context_egress`.
- Re-export `pub use context_env::{ContextShellRow, ContextEnvRow};`.
- Tests: `context_shell_upsert_and_get`, `context_shell_get_unknown`, `context_shell_copy`,
  `context_shell_copy_empty`, `context_shell_cascade_delete`; the whole `// ── 24. Context env CRUD ──` section;
  the `// ── Context egress ──` section except the two `validate_egress_host_*` tests (they stay with the parent):
  `context_egress_add_list_remove_round_trip`, `context_egress_copy`, `context_egress_cascade_delete`,
  `context_egress_add_rejects_an_invalid_host`, `context_egress_table_appears_on_a_db_opened_without_it`.

### `kernel_db/cache_hydration.rs`

- Const and free fns: `DEFAULT_HYDRATION_WINDOW`, `encode_cache_target`, `decode_cache_target`.
- Methods: `add_cache_breakpoint`, `list_cache_breakpoints`, `clear_cache_breakpoints`, `set_hydration_policy`,
  `get_hydration_policy`, `clear_hydration_policy`.
- Re-export `pub use cache_hydration::DEFAULT_HYDRATION_WINDOW;`.
- Tests: the `// ── Claude cache breakpoints ──` section (`cache_breakpoints_empty_for_unknown_context`,
  `cache_breakpoints_add_and_list_round_trip_all_variants`, the six `hydration_policy_*`); from
  `// ── Fork inheritance ──` the `cache_breakpoints_*` tests (`..._preserve_declaration_order_via_seq`,
  `..._clear_returns_count_and_empties_list`, `..._resume_seq_after_clear`, `..._cascade_delete_on_context`,
  `..._storage_does_not_enforce_4_cap`, `..._decode_drops_unrecognized_kind`, `..._decode_drops_unrecognized_ttl`,
  `..._message_index_zero_round_trips`).

### `kernel_db/tracks.rs` (tracks and attachments)

- Types and free fns: `PersistedTrack`, `PersistedAttachment`, `parse_optional_context_id`, `validated_track`
  (keep its `#[allow(clippy::too_many_arguments)]`).
- Methods: `upsert_track`, `write_track`, `get_track`, `list_tracks`, `delete_track`, `tombstone_track`,
  `upsert_attachment`, `write_attachment`, `get_attachment`, `list_attachments_for_track`,
  `list_all_attachments`, `list_attachments_for_context`, `delete_attachment`, `copy_attachments_for_fork`.
- Re-export `pub use tracks::{PersistedTrack, PersistedAttachment};`.
- Tests: the `// ── Tracks CRUD ──`, `// ── tombstone (kj transport delete) ──` and `// ── Attachments CRUD ──`
  sections; from `// ── Fork inheritance ──`: `attachments_travel_with_a_fork`,
  `fork_of_a_non_musician_copies_no_attachments`, `copy_attachments_for_fork_in_transaction`.

### `kernel_db/client_state.rs` (client views and dirty file buffers)

- Type and free fn `DirtyFileBuffer`, `read_dirty_file_buffer`. Methods `set_client_view`, `get_client_view`,
  `record_dirty_file_buffer`, `clear_dirty_file_buffer`, `get_dirty_file_buffer`, `list_dirty_file_buffers`.
- Re-export `pub use client_state::DirtyFileBuffer;`.
- Tests: the `// ── Client Views ──` and `// ── Dirty File Buffers ──` sections.

### `kernel_db/documents.rs` (documents, oplog, provenance, snapshots)

- Types `DocumentRow`, `DocSnapshotRow`.
- Methods: `insert_document`, `write_document`, `insert_document_with_op`, `classify_document_insert_error`,
  `insert_document_or_ignore`, `write_document_or_ignore`, `get_document`, `list_documents`,
  `list_documents_by_kind`, `delete_document`, `append_op`, `write_op`, `insert_block_provenance`,
  `get_block_provenance`, `list_block_provenance`, `load_oplog_since`, `load_latest_snapshot`,
  `write_snapshot_and_truncate`, `delete_oplog_row_for_test` (stays `pub(crate)` with `#[cfg(test)]`).
- Re-export `pub use documents::{DocumentRow, DocSnapshotRow};`.
- Tests: `failed_well_known_publication_leaves_only_a_discardable_document`,
  `document_deletion_refuses_live_and_archived_contexts`, `block_provenance_roundtrip_and_cascade`, the
  `// ── 30. insert_document constraint classification, by read-back ──` section, the helper `file_doc_row`,
  `insert_document_with_op_commits_the_row_and_the_op`,
  `insert_document_with_op_rolls_the_row_back_when_the_op_fails`,
  `insert_document_with_op_writes_no_op_when_the_row_is_a_duplicate`.

### `kernel_db/workspaces.rs`

- Types `WorkspaceRow`, `WorkspacePathRow`.
- Methods: `get_or_create_system_workspace`, `get_or_create_default_workspace`, `get_or_create_builtin_workspace`,
  `insert_workspace`, `get_workspace`, `get_workspace_by_label`, `list_workspaces`, `list_all_workspaces`,
  `archive_workspace`, `insert_workspace_path`, `list_workspace_paths`, `delete_workspace_path`,
  `context_workspace_paths`, `check_workspace_path`, `contexts_using_workspace`.
- Re-export `pub use workspaces::{WorkspaceRow, WorkspacePathRow};`.
- Tests: `workspace_lifecycle`, `workspace_soft_delete`, `workspace_path_read_only_roundtrip`,
  `context_workspace_paths_bound`, `context_workspace_paths_unbound`, the five `check_workspace_path_*`.

### `kernel_db/presets.rs`

- Types `PresetRow`, `PresetArg`. Methods `insert_preset`, `get_preset`, `get_preset_by_label`, `list_presets`,
  `update_preset`, `delete_preset`, `set_preset_args`, `get_preset_args`, `contexts_using_preset`.
- Re-export `pub use presets::{PresetRow, PresetArg};`.
- Tests: helper `arg`, `preset_args_roundtrip_verb_scoped_and_dedup`, `preset_args_cascade_on_preset_delete`,
  `preset_lifecycle`. (`insert_test_preset` is in `test_support`.)

### `kernel_db/hooks.rs`

- Types `HookRow`, `HookScriptRow`. Methods `insert_hook`, `upsert_hook`, `delete_hook`, `insert_hook_script`,
  `update_hook_script`, `get_hook_script`, `list_all_hook_scripts`, `count_hooks_referencing_script`,
  `delete_hook_script`, `load_all_hooks`. `row_to_hook_script_row` stays in the parent.
- Re-export `pub use hooks::{HookRow, HookScriptRow};`.
- Tests: the `// ── 23b. Hook persistence ──` section (`minimal_hook_row` is in `test_support`).

### `kernel_db/usage.rs` (context usage and turns)

- Types and fns: `ContextUsageRow` + `impl`, `TurnStart`, `RawUsage`, `TurnRow`, `TurnTotals`, free fn
  `context_used_pct`, associated `const TURN_COLUMNS`.
- Methods: `set_context_usage`, `get_context_usage`, `list_all_context_usage`, `begin_turn`, `add_turn_usage`,
  `finish_turn`, `read_turn_row`, `get_turn`, `list_turns`, `context_turn_totals`.
- Re-export `pub use usage::{ContextUsageRow, TurnStart, RawUsage, TurnRow, TurnTotals, context_used_pct};`.
- Tests: `context_usage_absent_by_default`, `turn_usage_sums_every_call_and_finish_closes_the_row`,
  `turn_writes_fail_loudly_without_a_running_row`, `context_turn_totals_sum_the_context_s_turns_only`,
  `context_usage_set_and_get_round_trips`, `context_usage_upsert_overwrites_not_sums`,
  `list_all_context_usage_returns_every_row`, the four `context_used_pct_*`. (`turn_start` is in `test_support`.)

### `kernel_db/model_prices.rs` (existing; add tests only)

- Append `#[cfg(test)]` then `mod tests;`. Tests, `model_prices/tests.rs`: helper `new_price`,
  `the_effective_price_is_the_latest_at_or_before_the_instant`,
  `prices_cannot_be_rewritten_or_removed_or_negative`,
  `cost_groups_split_turns_by_the_price_in_force_at_their_start`,
  `a_contexts_cost_prices_each_turn_by_the_price_in_force`.

### `kernel_db/well_known.rs`

- Type `WellKnownRole` + `impl`. Methods `well_known_context`, `insert_well_known_context`,
  `set_well_known_context`, `clear_well_known_context`.
- Re-export `pub use well_known::WellKnownRole;`.
- Tests: `well_known_context_set_get_clear_roundtrip`,
  `set_well_known_context_twice_rotates_rather_than_conflicts`.

### `kernel_db/roster.rs` (storage half of `crate::roster`)

- Free fns `read_roster_entity`, `read_opt_liveness_kind`, `row_to_roster_row` (the last three row parsers).
- Methods (the last `impl KernelDb` block, under the `// Roster (crate::roster)` banner): `roster_reconcile_presence`,
  `roster_write_status`, `roster_snapshot`, `roster_get_availability`, `roster_presence_source_local_ids`.
- Tests none.

### `kernel_db/llm_backends.rs` (backends, models, aliases, embedding)

- Types and consts: `BackendRow`, `BackendModelRow`, `ModelTunablesRow`, `ModelAliasRow`, `EmbeddingConfigRow`,
  `BACKEND_COLS`, `row_to_backend`.
- Methods under the `// ── backends ──`, `// ── backend_models ──`, `// ── model_aliases ──` and
  `// ── embedding_config ──` headers: `upsert_backend`, `get_backend_by_name`, `list_backends`, `delete_backend`,
  `set_backend_model`, `delete_backend_model`, `list_backend_models`, `set_model_alias`, `delete_model_alias`,
  `list_model_aliases`, `get_embedding_config`, `set_embedding_config`.
- Stay in the parent: `read_backend_id`, `validate_tunables`, `THINKING_STYLES`.
- Re-export `pub use llm_backends::{BackendRow, BackendModelRow, ModelTunablesRow, ModelAliasRow, EmbeddingConfigRow};`.
- Tests: `backend_models_gain_tunables_on_existing_databases`, helper `embedding_row`,
  `legacy_embedding_config_migrates_to_an_unconfigured_service`, `a_table_with_the_new_columns_keeps_its_row`,
  `an_embedding_row_is_refused_when_it_cannot_work`.

### `kernel_db/llm_characters.rs` (characters, slots, models)

- Types and consts: `CharacterRow` + `impl`, `CharacterSlotRow`, `CharacterModelRow`, `CHARACTER_SLOT_COLS`,
  `CHARACTER_MODEL_COLS`, `row_to_character_slot`, `row_to_character_model`.
- Methods under `// ── character slots ──`, `// ── character models ──`, `// ── characters ──`:
  `set_character_slot`, `delete_character_slot`, `list_character_slots`, `get_character_slot`,
  `set_character_model`, `delete_character_model`, `get_character_model`, `insert_character`,
  `get_character_by_name`, `get_character`, `list_characters`, `set_character_handoff_ctx`,
  `update_character_root`, `update_character_context_type`, `retire_character`, `name_for`, `contexts_played_by`.
- Stays in the parent: `row_to_character_row` (also used by `migrate_senpai_edges`).
- Re-export `pub use llm_characters::{CharacterRow, CharacterSlotRow, CharacterModelRow};`.
- Tests: `character_crud_roundtrip`, `character_duplicate_name_is_rejected`,
  `character_root_flag_round_trips_in_both_directions`, `contexts_played_by_excludes_archived`,
  `name_for_resolves_sentinels_then_sheet_then_short_id`.

### Parent tests: `kernel_db/tests.rs`

Everything left: `schema_idempotent`, `init_connection_sets_synchronous_normal`,
`init_connection_sets_journal_size_limit`, `checkpoint_truncates_wal`, the four `vacuum_into_*`,
`duplicate_column_error_detection_is_specific`, `fork_kind_from_sql_fails_loud_on_unknown`,
`label_validation_colon`, `fk_violation_is_validation_error`, the two `validate_egress_host_*`,
`open_read_only_reads_an_existing_db_without_writing`, `open_read_only_fails_loudly_on_a_missing_file`,
`temporary_writes_a_real_file_and_removes_it_on_drop`.

## 2. What stays in `kernel_db.rs`

- Header and imports: the `//!` doc, all current `use` lines (children depend on them through `use super::*`), the
  `mod` and `pub use` lists.
- `KernelDbError`, `KernelDbResult`, `macro_rules! performer_sql` (moved up, section 5), `struct KernelDb`.
- Core `impl KernelDb`: `conn_for_ledger`, `conn_for_test_mut`, `open`, `open_read_only`, `temporary`,
  `in_transaction`, `checkpoint`, `vacuum_into`, `set_query_only_for_test`.
- Shared helpers: `blob_param`, `read_context_id`, `read_opt_context_id`, `read_principal_id`,
  `read_opt_principal_id`, `read_workspace_id`, `read_opt_workspace_id`, `read_preset_id`, `read_opt_preset_id`,
  `read_opt_cast_id`, `read_edge_id`, `read_backend_id`, `now_millis`, `validate_label`, `validate_env_key`,
  `validate_egress_host` (`pub(crate)`), `map_unique_violation`, `is_duplicate_column_error`,
  `context_state_from_sql`, `fork_kind_from_sql`, `doc_kind_from_sql`, `validate_tunables`, `THINKING_STYLES`.
- Row mappers used across domains: `row_to_document_row`, `row_to_context_row`, `row_to_edge_row`,
  `row_to_preset_row`, `row_to_character_row`, `row_to_hook_script_row`, `row_to_workspace_row`.
- Test wiring: `#[cfg(test)] mod test_support;`, `#[cfg(test)] use test_support::*;` (so
  `crate::kernel_db::make_context_row` keeps working for `context_characters/tests.rs`), `#[cfg(test)] mod tests;`.

No visibility changes in the parent: children already see its private items.

## 3. Shared test helpers (`kernel_db/test_support.rs`) — the FIRST step

The file begins `//! Test fixtures shared by the kernel_db tests.` and `use super::*;`. Every item is `pub(super)`.
Declared in the parent as `#[cfg(test)] mod test_support;` then `#[cfg(test)] use test_support::*;`.

**Step 1** moves the helpers at top level of `kernel_db.rs` under the "Test helpers" banner:
`into_pre_context_character`, `set_stored_root_ctx`, `set_stored_played_by`, `make_context_row`,
`insert_context_with_doc`, `setup_test_db`, `make_edge`. (`into_pre_context_character` uses
`PRE_CONTEXT_CHARACTER_COLUMNS`; at step 28 write it as `super::migrate_senpai::PRE_CONTEXT_CHARACTER_COLUMNS`.)

**Step 2** hoists helpers now inside `mod tests` but used by more than one domain (add `pub(super)`, dedent by 4):
`seed_characters`, `senpai_ctx_row`, `run_context_character_migration`, `minimal_hook_row`, `insert_test_preset`,
`turn_start`, `binding_with`, `make_track`, `make_attachment`, `BACKENDS_LEGACY_COLS`,
`put_backends_back_on_its_pre_codex_shape`.

Every other helper inside `mod tests` is used by one domain and moves with it.

## 4. Ordered steps

Each step: ONE domain, code plus its tests together, with the template in section 6. N is the baseline test count;
every step must report the same N. For a finer check, save leaf names before step 1 with
`cargo test -q -p kaijutsu-kernel --lib -- --list 2>/dev/null | grep ': test$' | sed 's/.*:://; s/: test$//' | sort`
and diff after each step.

| Step | Name | What moves | Notes |
|---|---|---|---|
| 0 | Baseline | none | record N and the name list |
| 1 | `test_support` (top-level helpers) | section 3 step 1 | add the two `#[cfg(test)]` lines |
| 2 | Hoist shared test-local helpers | section 3 step 2 | delete them from `mod tests` |
| 3 | Macro position | `performer_sql!` and its doc, to just above the first `mod` line | pure reposition |
| 4 | `kernel_meta` | | |
| 5 | `presets` | | |
| 6 | `hooks` | | |
| 7 | `client_state` | | |
| 8 | `well_known` | | |
| 9 | `workspaces` | | |
| 10 | `roster` | | no tests file |
| 11 | `usage` | | |
| 12 | `model_prices` tests | | append `#[cfg(test)] mod tests;` |
| 13 | `context_bindings` | | |
| 14 | `context_env` | | |
| 15 | `cache_hydration` | | |
| 16 | `tracks` | | |
| 17 | `documents` | | `write_document*`, `write_op` become `pub(super)` |
| 18 | `lineage` | | |
| 19 | `llm_backends` | | |
| 20 | `llm_characters` | | |
| 21 | `approvals` | | includes `HeldAskEnd` |
| 22 | `review` | | `context_display` becomes `pub(super)` |
| 23 | `contexts` | | `write_context` becomes `pub(super)` |
| 24 | `context_ring` | | |
| 25 | `fixtures` | | gated mod; also `cargo check -p kaijutsu-kernel --features test-util` |
| 26 | `context_fork` | | `context_characters::copy_character_into` → `super::context_characters::copy_character_into` |
| 27 | `schema` | | |
| 28 | `migrate_senpai` | | update `test_support`'s const path |
| 29 | `migrate_cast` | | |
| 30 | `migrate` code | | tests stay in the parent for now |
| 31 | `migrate` tests | | skip the roster-shape block |
| 32 | `migrate` shape tests | | |
| 33 | Parent tests | all remaining tests in `mod tests` and `mod label_index_tests` | delete the empty wrappers |
| 34 | Cleanup (on request only) | header doc describes the module map | prose and imports only |

Leaf domains go first; the contexts cluster is last because everything else touches it. Per-step self-check:
`grep -c "#\[test\]"` summed over `kernel_db.rs kernel_db/*.rs kernel_db/*/*.rs` equals the baseline sum.

## 5. Risks and how to resolve each

1. **Cross-domain private items** (`write_context`, `write_document*`, `write_op`, `write_context_shell`,
   `write_binding`, `write_context_env`, `write_context_egress`, `copy_attachments_for_fork`, `context_display`,
   `assign_context_character`, `has_column`/`has_table`, `ensure_legacy_cast_tables`, `SENPAI_MIGRATION`,
   `HOUSE_MIGRATION`): the blanket `pub(super)` rule.
2. **impl blocks spanning domains.** Move whole functions by name into the domain's single `impl`; leave the rest
   in the original block until their own step.
3. **`performer_sql!` textual scope.** It is defined after the `mod context_characters;` lines; a `mod` declared
   before a `macro_rules!` cannot see it. Step 3 moves it above the first `mod` line, before step 23.
4. **`use` lists.** If a sibling's item is missing, add `use super::<module>::X;` or rely on the parent's re-export.
5. **Relative paths.** `context_characters::copy_character_into` becomes
   `super::context_characters::copy_character_into` after a move. Extern crates (`approval_ledger::`) need no change.
6. **Doc links** still resolve through `use super::*`; do not chase rustdoc warnings.
7. **`#[cfg(test)]`/`test-util` items** keep their attributes; `set_query_only_for_test` stays in the parent.
8. **`SCHEMA`** moves whole, keeps its name; tests reach it through the parent's `use schema::SCHEMA;`.
9. **The migration ladder** stays in `migrate.rs`, calls unchanged and unreordered.
10. **Dedenting tests** shifts lines inside multi-line string literals. Harmless for SQL. If an exact-text
    assertion fails, stop and report.
11. **Test names** change path; N must not change.
12. **Re-export visibility.** A `pub(crate)` item is re-exported with `pub(crate) use`.
13. **Name collisions.** Test-local `has_column`, `has_table`, `senpai_of` are free fns, distinct from the methods.
14. **Unused-import warnings**: do not delete the import; report it.
15. **Concurrent edits**: always locate by name.

## 6. Step template for each Haiku brief

```
You are executing ONE step of docs/kernel-db-split.md (read it first, sections 0, 3, 5).
Step: <NUMBER> — <MODULE NAME>. Items to move: <the module's section from the plan>.

Rules (move-only refactor, NO behavior change):
1. Move code and tests by copying text exactly. No edits beyond:
   (a) the path/`mod`/`use` lines,
   (b) visibility: every moved item that is not `pub` or `pub(crate)` becomes `pub(super)`,
   (c) the 4-space dedent of tests that leave `mod tests { }`,
   (d) `pub use`/`pub(crate) use` re-exports in kernel_db.rs.
2. No reformatting, no `cargo fmt`, no renames, no reordering inside an impl, no comment rewrites.
3. New file layout: `//!` doc line, `use super::*;`, the items, then (if the module has tests)
   `#[cfg(test)]` newline `mod tests;`. Tests go in `kernel_db/<module>/tests.rs`, flat, starting with
   `use super::*;`. Locate everything by NAME, never by line number. A named item that no longer exists is
   skipped and reported.
4. After moving, delete the originals. Leave other domains' code alone.
5. Edit only crates/kaijutsu-kernel/src/kernel_db.rs and files under crates/kaijutsu-kernel/src/kernel_db/.
6. No git mutations.

Verify (from the repo root):
  systemd-run --user --scope -q -p CPUWeight=20 --nice=19 -p MemoryHigh=40G -p MemoryMax=56G -p MemorySwapMax=4G cargo test -q -p kaijutsu-kernel --lib 2>&1 | tail -3
  systemd-run --user --scope -q -p CPUWeight=20 --nice=19 -p MemoryHigh=40G -p MemoryMax=56G -p MemorySwapMax=4G cargo check --workspace --tests 2>&1 | tail -3

Expected: exactly N passed (N given in this brief), 0 failed; cargo check has no errors and no new warnings.

STOP CONDITION: if the count changes, a test fails, or anything beyond imports/visibility/re-exports/dedent
needs editing to compile, STOP. Do not fix it. Report the step, the exact error or count, and the items involved.
```

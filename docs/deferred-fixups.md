# Deferred Fixups

Issues identified during code review that are out of scope for the current
fix pass. Each requires its own design session or PR.

## Schema / Protocol Changes

- **Kernel: TextOps sequence number** — Requires capnp schema change + client
  protocol update to detect out-of-order text operations.

- **Kernel: dead letter queue RPC** — Requires new capnp method for inspecting
  and replaying failed tool executions.

- **Kernel: config CRDT ops** — Config backend needs DTE integration so config
  changes replicate across peers.

## Design Work

- **Per-field LWW for header merge** — `merge_header()` uses whole-header LWW;
  concurrent field mutations race. Design doc: `docs/LWW-critical-todo.md`.
  Regression test: `test_lww_race_ephemeral_overwritten_by_status`.

## External Dependencies

- **Kernel: MCP tool cache refresh** — Requires rmcp notification handling to
  detect when remote MCP servers add/remove tools.

## Testing

- **MCP: remote mode tests** — The MCP server's remote (SSH-connected) code
  paths have no integration tests. Valuable but large scope.

- **Index: test isolation** — `kaijutsu-index` tests share temp directories and
  fail intermittently under parallel execution (`--test-threads=1` is a
  workaround). Each test should use an isolated temp dir.

## Performance

- **CRDT: order_index BTreeMap** — The O(N log N) sort in `blocks_ordered()`
  works correctly but scales poorly. Add a secondary sorted index when scale
  demands it.

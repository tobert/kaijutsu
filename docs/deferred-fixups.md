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

- **Index: hnsw_rs reverse-edge quirk** — hnsw_rs's
  `reverse_update_neighborhood_simple` writes reverse edges at the neighbour's
  own assigned layer rather than the current search layer. When a point lands
  at level > 0 (random, P = 1/max_nb_connection per point), later-inserted
  points may not appear in its layer-0 neighbour list, which silently degrades
  approximate-nearest-neighbor recall. Tests work around this by not asserting
  on HNSW ordering; production code should revisit whether we need to switch
  vector-index libraries, patch upstream, or accept reduced recall. (The old
  fixup entry claimed tests shared temp dirs — they don't; each test uses a
  unique `TempDir::new()`. The intermittent failures were this upstream quirk
  plus the reload-path bug fixed alongside this note.)

## Performance

- **CRDT: order_index BTreeMap** — The O(N log N) sort in `blocks_ordered()`
  works correctly but scales poorly. Add a secondary sorted index when scale
  demands it.

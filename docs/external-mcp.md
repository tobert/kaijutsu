# External MCP servers — `mcp.toml`

> **Status:** SHIPPED. `ExternalMcpServer` (`mcp/servers/external.rs`) was a
> complete, tested `McpServerLike` implementation with no caller since the
> old `mcp_pool`/`mcp_config` were deleted — this doc covers the five pieces
> that closed that gap: config loading, the reconciler that's the actual
> caller, per-server QoS, visibility, and the `kj mcp` operator surface.

`mcp.toml` (CRDT-owned, `docs/config-crdt-ownership.md`) declares MCP servers
the kernel spawns and registers on the broker alongside the in-process
builtins (`builtin.file`, `builtin.block`, …), under the `external.<name>`
instance-id namespace so a context binding can grant/deny the two
independently. `[servers.kaibo]` and `[servers.bevy_brp]` in
`assets/defaults/mcp.toml` are the shipping examples.

## Component 1 — the loader (`mcp/toml.rs`)

`load_mcp_config_toml` parses the TOML into `Vec<McpServerConfig>` +
warnings. Two failure tiers, matching the project's "loud, not silent"
doctrine (CLAUDE.md):

- **Whole-file** parse failure (bad TOML syntax, or a field shape `serde`
  can't coerce at all) is the caller's problem: `Err`, and callers fall back
  to the embedded default, loudly logged — the same embedded-default fallback
  shape the kernel's other config files use.
- **Per-entry** semantic failure (stdio transport with no `command`,
  `streamable_http` with no `url`, an unrecognized `transport` string) does
  **not** fail the whole file — the entry is dropped and a human-readable
  reason lands in `warnings`. One malformed `[servers.X]` table must not
  take every other configured server down with it, but it must never vanish
  silently either.

An unrecognized `transport` is a warning, not a silent fallback to `stdio` —
the deleted `mcp_config.rs` used to default it, which meant a typo'd
`"streemable_http"` quietly spawned whatever `command` happened to be set
(or wasn't).

## Component 2 — visibility (loud, not a log grep)

A configured-but-unstartable server (wrong binary path, crashed handshake)
is recorded on `Broker::external_mcp_failures` (`(name, error)` pairs,
replaced wholesale on every reconcile pass) so it's queryable via
`kj mcp list` instead of requiring a log grep after the boot log has
scrolled by. Per-entry loader warnings ride the same "surfaced, not just
logged" principle through the reconcile report.

## Component 3 — per-server QoS override

`call_timeout_ms` in a `[servers.X]` table overrides
`InstancePolicy::call_timeout` for that instance; absent, the kernel-wide
`TimeoutPolicy::mcp_call_timeout_default` (120000ms) applies. A kaibo
consultation is a real model call and routinely runs 5–15 minutes — the
120s kernel default would cut every one of them off, hence
`[servers.kaibo]`'s `call_timeout_ms = 900000` in the shipped default.

The override is re-applied on **every** reconcile pass, not just at
first connect: `reconcile_with_toml` refreshes an already-running
instance's policy from the current config each time (cheap — an in-memory
`Duration` swap on `Broker.policies`, no subprocess involved). This means
a `call_timeout_ms` edit takes effect on the next `kj mcp reload` without
disturbing an in-flight call, and removing the override from `mcp.toml`
reverts the running instance to the kernel default rather than sticking at
the old value.

## Component 4 — the reconciler (`mcp/external_registry.rs`)

`reconcile_with_toml` (and its VFS-reading wrapper
`reconcile_external_mcp_servers`, called from server boot and from
`kj mcp reload`) diffs the desired config against the broker's live
`external.*` registrations:

- **New** in `mcp.toml` → connect + register.
- **Removed** (or `enabled = false`) → unregister.
- **Still present** → the *connection* is never touched — no reconnect, no
  respawn, even if `command`/`args`/`env` changed underneath it. Only the
  `InstancePolicy` is refreshed (component 3).

### The reload design fork

This is deliberately conservative: a kaibo consultation mid-flight when
someone edits `mcp.toml` is never interrupted by a reload, at the cost of a
`command`/`args`/`env` edit not taking effect until the server is
explicitly restarted. **There is no `kj mcp restart <name>` in this slice**
(`docs/issues.md`).

The alternative — diffing `McpServerConfig` and hot-swapping a changed
entry — is also defensible: `Broker::register` already replaces-by-id, and
an in-flight call holds its own `Arc` to the resolved instance (resolved
once per call, not re-resolved mid-flight), so a hot-swap costs nothing
*additional* over what a reload already risks. It was set aside because it
requires remembering each running instance's last-applied config somewhere
— `Arc<dyn McpServerLike>` carries no config accessor, and adding one means
either widening the shared trait (every builtin server pays for a concept
it doesn't have) or a parallel `InstanceId -> McpServerConfig` side table.
That's real new state for a benefit a name-scoped restart verb gets more
predictably and more visibly. The smaller, zero-new-state design is what
shipped; flagged here per this project's practice of recording a defensible
alternative that was set aside, not silently picking one of two reasonable
options.

## Component 5 — the operator surface (`kj mcp`)

```
kj mcp list [--json]     # alias: status — configured vs. running, with health
kj mcp reload            # re-read mcp.toml, reconcile (see component 4)
```

`list` is a read, ungated. `reload` materializes `mcp.toml` as running
processes — the same authority tier as editing the file
(`kj config set/edit mcp.toml`) — so it's gated on `Capability::ConfigWrite`
rather than a bespoke capability.

## Transport (`mcp/servers/external.rs`)

`ExternalMcpServer` wraps an `rmcp` subprocess (stdio) or `streamable_http`
connection behind `McpServerLike`: connect + handshake, `_meta` propagation
per the kernel's `io.kaijutsu.v1.*` namespace, and health flipping to
`Down` on a transport error. Reconnect-on-failure is intentionally not
automatic (component 4's reload semantics cover the "config changed"
half; a transport error mid-session is a separate, still-open concern).

## Testing

- `crates/kaijutsu-kernel/src/mcp/toml.rs` — loader unit tests (both failure
  tiers, ordering, the shipped default parses).
- `crates/kaijutsu-kernel/src/mcp/external_registry.rs` — reconcile-decision
  tests (add/remove/refresh/idempotent) using a no-op `FakeServer`, no real
  subprocess.
- `crates/kaijutsu-kernel/tests/external_mcp_stub.rs` +
  `crates/kaijutsu-kernel/src/bin/mcp_stub_server.rs` — a minimal real
  MCP-speaking subprocess (never kaibo or bevy_brp themselves) that proves
  the add path lands a genuinely callable instance.
- `crates/kaijutsu-server/tests/mcp_boot.rs` — boot-level: a malformed
  `mcp.toml` must never abort kernel startup, and a configured-but-unreachable
  server must be visibly failed, not silently absent.

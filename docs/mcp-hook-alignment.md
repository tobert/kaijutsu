# MCP Hook System — Alignment Pass

The **MCP socket hook** lets an external agent tool (Claude Code today) stream its
session into a kaijutsu context as it happens, and receive kaijutsu **drift** back
on the return path. It was built early, last saw real work **2026-06-13**, and has
drifted from current core in three places: the adapter's field/event mapping, the
context model it assumes, and the process-coupling that decides which context it
writes to. This note is the design pass before any fixes — it maps current state,
names the misalignments, frames the role question, and proposes a build order.

Status: **design pass, 2026-06-18** (with Amy). Role decision deferred to this doc;
one decision pre-made — hook-fed contexts get their **own context_type** (a passive
*mirror*), distinct from the `mcp` shell-driver bundle. No code yet.

---

## Two things both called "hook" — disambiguate first

1. **MCP socket hook** (this doc). Path: Claude Code fires a hook →
   `contrib/adapters/claude.sh` reshapes the JSON → pipes to `kaijutsu-mcp hook
   --socket …` → Unix socket → `HookListener` (`crates/kaijutsu-mcp/src/hook_listener.rs`)
   authors CRDT blocks into a kaijutsu context and injects pending drift back as
   `additionalContext`. Dormant right now (not wired into live `settings.json`).
2. **Kernel hook table** (`crates/kaijutsu-kernel/src/mcp/hook_table.rs`). A
   match-action policy engine around the broker's tool calls (PreCall / PostCall /
   OnError / OnNotification). Actively maintained, unrelated to this note. Mentioned
   only so the two are never conflated again.

Everything below is about #1.

---

## How the socket hook works today

- **Wire-in.** `contrib/claude-hooks.json` is a sample `settings.json` fragment
  wiring 5 events (PostToolUse, UserPromptSubmit, SessionStart, SessionEnd, Stop) to
  `contrib/adapters/claude.sh`. The adapter maps Claude event names → kaijutsu event
  names, reshapes the payload with `jq`, and shells out to `kaijutsu-mcp hook`.
- **Transport.** `kaijutsu-mcp hook` is a one-shot client: read JSON on stdin,
  connect to the socket, write one line, read one line, print it, exit. Fail-open:
  if the socket is absent it exits 0 and the agent proceeds (`main.rs::run_hook_client`).
- **Socket discovery.** `$XDG_RUNTIME_DIR/kaijutsu/hook-{ppid}.sock`
  (`hook_listener.rs::default_socket_path`), with a `discover_sockets()` scan as
  fallback. The listener side opens the same path in `run_serve`.
- **Authoring.** `HookListener::process_event` turns each event into CRDT blocks:
  session.start/end → System, prompt.submit → User, tool.after/error → ToolCall +
  ToolResult pair, agent.stop → Model, file.edit → Tool, subagent.* → System. It
  filters `KAIJUTSU_MCP_TOOLS` (hook_types.rs) so MCP tool calls don't double-write.
- **Return path.** After authoring, `maybe_inject_drift` polls the drift queue for
  `target_ctx == ctx_id`, builds a context string, cancels the consumed drifts, and
  returns it; the adapter relays it as `hookSpecificOutput.additionalContext`. Deny
  is wired (exit 2 → adapter relays reason → Claude blocks the action).
- **Which context?** The listener writes to `shared_context_id`, an
  `Arc<Mutex<Option<ContextId>>>` that is **`None` until `register_session` runs on
  the MCP stdio side** of the *same process* (`lib.rs::register_session` sets it).

---

## Misalignment 1 — adapter ↔ core field/event drift (silent data loss)

Concrete, verified against `hook_types.rs` and the current Claude Code hook spec
(code.claude.com/docs/en/hooks.md, 30 events):

- **`agent_id` → `principal_id` rename not propagated.** `claude.sh` emits
  `agent_id:` into the kaijutsu payload, but `HookEvent` was renamed to
  `principal_id` (commit `de860e2`, "agent is not a noun";
  `hook_types.rs:53`). serde drops the unknown field silently — a textbook
  silent-fallback we don't accept.
- **`.tool_response` → `.tool_output`.** `claude.sh` reads `.tool_response` for tool
  output, but current Claude Code emits **`.tool_output`** on PostToolUse (and a
  separate `.error` on the distinct `PostToolUseFailure` event). Every mirrored tool
  result currently has a null body.
- **Event coverage is thin, not wrong.** The adapter's `PostToolUseFailure`,
  `SubagentStart`, `PreCompact` names *are* real in current Claude Code. But
  `claude-hooks.json` only wires 5 events: no `PreToolUse` (so the deny path the
  adapter implements is never actually invoked), no subagent events, no compaction.
- **`.model` is SessionStart-only** in current Claude Code; the adapter pulls it on
  every event (harmless null elsewhere, but worth noting the assumption changed).

These are mechanical and low-risk to fix once the role (below) is settled, because
the role decides *which* events we even want to wire.

---

## Misalignment 2 — context model: mirror vs. driver share one stance

- `register_session` defaults `context_type = "mcp"` (`lib.rs:944`). An `mcp` rc
  bucket exists and is current (`assets/defaults/rc/mcp/{create,drift,fork}` —
  stance, binding, cache all present).
- **But the `mcp` stance is written for a *driver*:** *"You are driving a kaijutsu
  context from outside the kernel, over MCP. `shell` is your entry point…"*
  (`rc/mcp/create/S00-stance.md`). That fits an LLM **driving** kaijutsu through the
  `shell` tool. It does **not** fit a **passive mirror** of an external Claude Code
  session, where no LLM is reading that stance at all — blocks just accrete.
- **Decision (pre-made):** hook-fed contexts get their own context_type — a
  *mirror* bundle — separate from `mcp`. Candidate name: `mirror` (generic) or
  `claude-code` (source-specific). Recommend **`mirror`**: the same machinery serves
  gemini-cli/cursor later, and `source` already carries the tool name on each event.
  A mirror's rc stance is minimal/absent (no model drives it); its value is in
  `create` doing context setup and `drift` cache-reset only if the mirror is ever
  forked into a live conversation.

### Atomicity caveat (the load-bearing assumption)

The listener authors blocks via `flush_local_ops` straight to the server,
**bypassing the per-context mailbox** that is the atomicity gate for
tool_use+tool_result pairing (CLAUDE.md "Conversation vs Context"). It pairs
ToolCall+ToolResult under one local lock, which is safe **only while the mirror is
never a live conversation**. If a mirror is forked/attached and an LLM runs in it
with a concurrent sibling writer, a pair could be split. A dedicated `mirror`
context_type lets us state this invariant explicitly and gate forking accordingly.
The listener-internal sync hardening (sole-writer frontier, resync window) is
already triaged in `docs/issues.md` (the "hook authoring vs resync" cluster, HIGH
PARTIAL) — not re-derived here.

---

## Misalignment 3 — the registration coupling decides everything silently

The socket (`hook-{ppid}.sock`) and the MCP stdio channel are **two doors into one
process**. The hook only learns its target context via `shared_context_id`, which
stays `None` until `register_session` runs on the stdio side. Consequences:

- If Claude Code runs `kaijutsu-mcp` as an MCP server **and** the hook's
  `kaijutsu-mcp hook` client resolves to that same process's socket by ppid, and
  `register_session` has been called — it works. Three conditions, all implicit.
- If `register_session` hasn't run, every hook event **fails open and is dropped**.
  No error, no block, no signal. The mirror is silently empty.
- The ppid coupling assumes a specific launch topology that may no longer match how
  the kernel/app launches MCP. This is the part most likely to have rotted and the
  thing to validate against a live `kaijutsu-runner` session before anything else.

Open question for the design: should the hook **auto-register** a mirror context on
first event (so the socket is self-sufficient and doesn't depend on the stdio side
having run `register_session`)? That would decouple the two doors and make the
mirror robust, at the cost of the hook creating contexts on its own.

---

## The role question (deferred to here)

Three coherent roles; they differ in which direction is primary and therefore in how
much alignment work each demands.

1. **Mirror-in.** Faithfully shadow the external session into a `mirror` context;
   blocks accrete passively; never a live conversation. Smallest surface: fix the
   adapter fields, add the `mirror` context_type, validate registration. The return
   path (drift) is optional polish.
2. **Bridge-out.** Lead with the return path — pipe kaijutsu drift/notifications
   *into* the external tool via `additionalContext`, plus deny gating on `PreToolUse`.
   The mirror is just enough to position the context. Demands the most current,
   correct event wiring (PreToolUse, Stop, SubagentStop all support injection/block).
3. **Both, co-equal.** Full bidirectional bridge. Largest surface: faithful capture
   *and* the return path, which forces the mailbox-atomicity question to be answered
   for real (mirrors become forkable into live conversations).

**Recommendation:** sequence them. Land **mirror-in** first (it's the foundation and
the cheapest correctness win), wire **bridge-out** drift on top once the mirror is
trustworthy, and only take on **both/forkable-mirror** if a concrete use demands a
live conversation seeded from a mirror. That ordering also matches the dependency:
you can't trust drift-out until the context the drift targets is faithfully populated.

---

## Proposed build order (no code yet — for review)

1. **Validate the live path.** Wire `claude-hooks.json` into a throwaway
   `settings.json`, run under `kaijutsu-runner`, confirm whether the ppid socket +
   `register_session` coupling actually connects today. This decides whether
   Misalignment 3 needs the auto-register redesign or just a doc fix.
2. **Fix the adapter (mechanical).** ✅ DONE 2026-06-18. `agent_id`→`principal_id`
   and `.tool_response`→`.tool_output` in `claude.sh` (both were silent serde
   drops); `.error`/`PostToolUseFailure` already mapped. Contributing factor —
   the field map was trapped in an untestable bash heredoc — removed: each
   adapter's map now lives in a standalone filter (`contrib/adapters/*-to-kaijutsu.jq`,
   `jq -f`), single-sourced and tested. `crates/kaijutsu-mcp/tests/adapter_mapping.rs`
   round-trips real Claude payload fixtures through the actual filter into
   `HookEvent`; it was confirmed RED on the two bugs before the fix. Added a
   `KJ_HOOK_DRYRUN` transform-only hatch to both adapters for live validation
   (step 1). `claude-hooks.json` now also wires `PostToolUseFailure` + `SubagentStop`
   (mirror completeness; `PreToolUse`/`PreCompact` deferred with the injection work).
3. **Add the `mirror` context_type.** New `assets/defaults/rc/mirror/` bucket;
   `register_session` (or an auto-register path) uses it for hook-fed contexts.
   State the "never a live conversation" invariant in the bundle.
4. **Decide registration.** Either document the required launch topology, or add
   hook-side auto-register so the socket is self-sufficient.
5. **Bridge-out polish.** Only after 1–4: confirm `maybe_inject_drift` matches the
   current drift API (fork-lineage-down / drift-up grammar) and wire `PreToolUse` +
   `Stop` injection.
6. **Tests.** Close the e2e gap flagged in `issues.md` (no coverage for the
   hook-listener socket path) — the `e2e_shell.rs` harness generalizes.

Steps 1–2 are independent and safe to start the moment the role is confirmed. Steps
3–4 depend on what step 1 reveals about the coupling.

## Related

- `docs/issues.md` — listener-internal sync findings (hook authoring vs resync;
  Remote multi-context collapse; warn-then-allow on insert failure). Not duplicated
  here; those are correctness bugs *within* the chosen role, this note is the role
  and alignment frame around them.
- `crates/kaijutsu-mcp/src/{hook_listener.rs,hook_types.rs,lib.rs,main.rs}` — impl.
- `contrib/{claude-hooks.json,adapters/claude.sh,adapters/gemini.sh}` — wire-in.
- `assets/defaults/rc/mcp/` — current driver bundle (mirror bundle to be added).

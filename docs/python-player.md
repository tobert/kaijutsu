# The Code-Enabled Player — kaijutsu-py

Amy's framing (2026-08-09): this is a **code-enabled player**, not "the
Python integration" — a player that computes locally and reaches into the
shared world through kaish, which remains the one instrument surface.
*"Reverse ACP"*: the agent runtime is the host and kaijutsu is the
instrument it plays. Python is the first binding (a `kaijutsu` wheel; any
Python process becomes a first-class player); the concept admits other
languages later. Design reviewed by gemini-pro (batch deliberate) and
deepseek-v4-pro (consult) on 2026-08-09; both verdicts converged and are
melted into this doc.

## Why

- **Vendor harnesses as players, client-side.** Later: OpenAI/Gemini
  harnesses under their own plans, each a separate process with its own
  credentials and its own per-vendor policy read. The kernel never touches
  vendor auth — it just sees players.

  **CONTRADICTED 2026-08-14 — needs Amy's re-read before this lane is
  built.** This bullet used to read "the Claude Agent SDK runs under Amy's
  own subscription login (the sanctioned personal-use lane per the
  2026-08-09 policy read)". Anthropic's current docs say otherwise on both
  halves: the SDK authenticates from `ANTHROPIC_API_KEY` in the process
  environment (it does not spawn the `claude` CLI for auth, and does not
  inherit a logged-in CLI's credentials), and the overview carries an
  explicit gate — *"Unless previously approved, Anthropic does not allow
  third party developers to offer claude.ai login or rate limits for their
  products, including agents built on the Claude Agent SDK."* Managed
  Agents is the same story: tokens at API rates plus $0.08/session-hour,
  no subscription path. So the SDK lane is **metered spend, not seat
  spend**, and pyo3 buys nothing for the subscription question. The
  OAuth-extraction ban is undisturbed. What survives: the subscription
  surface is the vendor's *own harness under Amy's login* — a process, not
  a library (`claude` 2.1.232 and `gemini` 0.45.0 are installed here;
  `codex` is not) — which is lane (A) below and already works over
  kaijutsu-mcp. Judge the wheel on lanes 2 and 3.
- **Notebooks, science, MIDI.** A Jupyter cell that joins a context, reads
  the score, emits blocks.
- **Sandbox/venv experiment space** for agent-callable Python (see
  "Exec ownership" below).

## Doctrine

- **MCP for harness integration, wheel for native Python.** kaijutsu-mcp
  already is the "local daemon with a simpler protocol" — CC/Gemini-style
  harnesses keep using it. The wheel is the thinner path for Python-native
  players. Same ActorHandle underneath; they can coexist in one process.
- **Fat in capability, thin in derived state.** Players may carry vendor
  SDKs, local tools, venvs — never session logic or forked state. The
  kernel is the head.
- **Players are trusted peers** (shared trust boundary); capabilities
  remain ergonomic nudges.

## Two directions, and only one is solved (named 2026-08-14)

"Use my subscriptions" splits into two shapes the doc had been treating as
one. Keeping them apart is what stops the wheel from being justified by
work it doesn't do:

- **(A) The harness drives kaijutsu.** Vendor harness is host, kaijutsu is
  the instrument it plays — the "reverse ACP" framing above. **This works
  with zero new code**: `kaijutsu-mcp` + the Claude Code hooks pipeline,
  live since 2026-07-17. Gemini CLI is the cheap next seat (installed,
  speaks both MCP and ACP). No wheel required for any of it.
- **(B) Kaijutsu drives the harness.** Subscription-backed inference as a
  *backend* for kaijutsu's own contexts, so `kj`-driven turns spend seat
  instead of API credit. This is what "use my subscriptions" most naturally
  means for the budget, and it is **not designed**. It would want an LLM
  backend under `kernel/src/llm/` shelling out to a vendor CLI, which
  collides with kaish exec ownership (CLAUDE.md: a new exec site is a design
  conversation, not a patch) and resembles the extraction-into-our-own-harness
  shape the policy read ruled out, even without touching OAuth tokens.
  **Amy's policy read gates this; no code before it.**

The wheel is orthogonal to both. It is a *native Python handle* for lanes 2
and 3 (notebooks, science, MIDI, agent-callable venvs) — build it on that
case or not at all.

## Shape

- New crate `crates/kaijutsu-py`: `crate-type = ["cdylib", "rlib"]`, built
  by maturin, **pyo3 confined to this crate** — kernel/server/app never
  link libpython. Depends on `kaijutsu-client` (+ pyo3, pyo3-asyncio,
  tokio). Rust surface stays small (~Player class + runtime bootstrap +
  bridge); ergonomics (dataclasses, context managers, async generators)
  live in a pure-Python `kaijutsu/` package wrapping the `_core` module.
- **Seam: `ActorHandle`** (verified complete by review — Send+Sync+Clone
  over the !Send capnp internals, auto-reconnect FSM included).
- **Runtime**: one dedicated OS thread runs the actor's LocalSet
  (`runtime.block_on(local_set.run_until(...))`) beside a multi-thread
  tokio runtime — the same dual-executor shape as kaijutsu-mcp's main.
  Explicit `close()`; dropping the Player drops the actor cleanly so
  Python atexit never hangs on capnp callbacks.
- **API: asyncio-native core, explicit sync facade.** All methods async;
  a `Client` facade wraps with GIL-releasing block_on for sync/notebook
  flows. The facade must be explicit — a careless blocking wrapper starves
  the reconnect FSM.
- **GIL discipline**: never call Python from the actor thread. Events
  forward over a channel; Python consumes via async iterators (or
  `call_soon_threadsafe` dispatch). Rust background tasks never wait on
  the GIL.
- **abi3** wheels (one wheel per platform, Python ≥3.10).

## First slice

| Capability | ActorHandle basis | Serves |
|---|---|---|
| connect + auto-reconnect | `spawn_actor(config, …)` | both |
| register/join context | `join_context`, `create_context_typed`, `resolve_context_label` | both |
| `shell(cmd)` run + result | `shell_execute` + sync polling | agent, notebook |
| `events()` async generator | `subscribe_events` | agent (must) |
| block reads | `get_blocks_query`, `get_context_sync` | notebook (must) |
| drift queue/iter/cancel | `drift_queue`, `drift_cancel` | agent (must) |
| connection status | `subscribe_status` | both |

`scope_blocks_to_context=true` for single-context players (the MCP's
choice). `broadcast::Lagged` surfaces as a structured resync event —
never silently dropped (mirror the MCP's `EventsLagged` handling).

**Deferred past slice 1:** SyncedDocument replication (the sole-writer doc
task is ~1000 lines of hard-won concurrency control — do not port it until
the connection layer is proven), MIDI capture, VFS surfaces, input-document
compose, invoke_peer. `shell "kj …"` covers the gaps meanwhile.

## kaish data plumbing (audited 2026-08-09)

Player code calls through kaish for shared-world operations, so the value
of the whole design rides on structured data surviving the round trip.
Audit result: the promises hold, with one gap.

- **Wire**: `ShellExecResult` carries `data: ShellValue` — a typed union
  (null/bool/int/float/string/json/bytes) — plus `outputData` for
  tables/trees; the streaming path has a `structured` OutputEvent variant.
  `bytes` means binary (images) round-trips.
- **Client**: `read_shell_value` parses the `json` variant into
  `serde_json::Value`. Shell variables carry `ShellValue` both directions
  (`get/set/list_shell_vars`, durable per-context) — a code-enabled player
  and a kaish script share a typed variable namespace. The wheel maps
  these straight to Python objects; `player.vars["x"] = {...}` is visible
  to scripts as `$x`.
- **Producers**: `kj` is the structured producer (list verbs → arrays of
  full IDs, inspect → objects; full ids only). Unix-shaped commands remain
  text. The MCP `shell` tool already delivers `data` to CC agents today.
- **GAP (slice-1 item)**: `getLastResult` exists in the capnp schema but
  has NO client implementation — nothing in `KernelHandle` or
  `ActorHandle` calls it. The MCP reads `data` indirectly off block
  replication (hence its "null until replicated" caveat). The wheel gets
  the direct fetch: wrap `getLastResult` through KernelHandle + the actor
  so `sh()` returns lag-free structured results.

## Exec ownership (the kaish rule, applied)

- The player process runs whatever Python its owner wrote — that's inside
  the player's own trust envelope on its own machine. Client-side venvs
  (`uv`) are a Python-package concern, not a Rust-crate concern.
- Anything executed **by the kernel** still routes through kaish's
  `ExternalExec` policy. A player never ships Python for the kernel to
  blindly exec — it authors a tool-call block that kernel policy handles.
- Contained agent-callable Python execution reuses the isotest podman
  harness (PID-namespace hygiene, RO mounts) rather than growing a second
  containment story.

## Risks (ranked by review)

1. **Wheel↔kernel wire drift** (both reviewers' #1; same class as the
   kaijutsu-mcp binary pin). Mitigation is mandatory in slice 1: protocol
   version exchanged at connect; mismatch fails loudly with "wheel
   outdated — `pip install --upgrade kaijutsu` to match server vX". A
   capnp schema change now requires rebuilding FOUR clients.
2. **maturin-in-workspace packaging**: pyproject.toml localized to the
   crate; `maturin develop` in-workspace for dev; sdists must carry the
   workspace (the `diamond-types-extended` git pin is lost outside it);
   mold linker assumption on build machines.
3. **GIL/tokio deadlock** if events call inline into Python — forbidden by
   the channel rule above.
4. **Notebook version skew** — loud and actionable via the handshake.

## Rejected alternatives

- **pycapnp pure-Python client**: would re-implement the reconnect FSM,
  handshake, and event buffering kaijutsu-client owns — the same
  policy-copy drift the exec-unification work exists to kill.
- **New local daemon w/ simpler protocol**: already exists (kaijutsu-mcp);
  a second one adds IPC hops and lifecycle management for nothing.

## Open

- **First consumer to build (Amy's call; the actual blocker — nothing is
  coded).** Sharpened 2026-08-14 now that "Agent-SDK seat" is known to be a
  metered lane rather than a seat: is it a notebook/MIDI player (the wheel's
  real case, read-mostly ergonomics), or a second vendor seat over MCP/ACP
  (turn-driving, **needs no wheel at all** — Gemini CLI is installed and
  speaks both)? These are no longer two flavors of the same build.
- Direction (B) above — does it get a policy read, or is the lane closed?
- Watch the paused Agent-SDK-credits program (2026-06-15 pause) — if it
  resumes, the subscription seat gets its own metered lane.

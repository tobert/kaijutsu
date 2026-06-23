# In-App vi Editor

Goal: `vi /etc/rc/thing/whatever.kai` (and `kj rc edit <path>` by default) opens a
real vi-like editor on that file's CRDT block. The editor is a **kernel-owned
session** driven through a small tool-shaped surface; the Bevy app is one
*renderer* of it, a model is another *player* of it, and a headless test is a
third *driver* of it. Same surface for all three.

Status: **architecture reworked 2026-06-22 — kernel-owned editor sessions + a new
`kaijutsu-editor` crate. Not yet building the crate.** Slice 1's path resolver has
shipped (`crates/kaijutsu-kernel/src/editor.rs`, commit `110bd95`). The near-term
driver is making `kj rc edit <path>` open this editor by default (today it is a
non-interactive `--content` replace: `kj/rc.rs:402`).

---

## The core idea: the editor is the input-doc surface, generalized

The compose box is already a **kernel-owned editable buffer**: `input_doc.rs`,
fronted by a tool-shaped surface — `read_input` / `write_input` / `edit_input` /
`submit_input` (real MCP tools today) — that the Bevy app merely *renders*. A
model edits the compose box over the wire right now; the GUI is one view onto it.

The vi editor is that **exact pattern pointed at a file/config block** instead of
the input doc. So the testability win isn't scaffolding we throw away — it is the
editor's real spine:

- **Thin client, smart kernel.** Editing state and semantics live in the kernel;
  the 3D viewer is a *view onto* a kernel session, not the owner of it
  (`feedback_thin_client_smart_kernel`).
- **Playable by anyone.** Many hands on one keyboard: app, model, test all drive
  the same `editor_*` surface.
- **Testable before pixels.** The whole increment is exercised headless because
  the increment was never about rendering.

This is the "instrument you play, not a harness that drives you" stance made
concrete for editing.

---

## Three pieces

### 1. `kaijutsu-editor` crate — `EditorCore` (pure)

A new crate holding the vim *engine* as pure logic — **no Bevy, no kernel, no
RPC**. Built on modalkit's `VimMachine` + `TerminalKey` (already used by the app's
compose surface).

`EditorCore` owns one editor's session-local state:

- the text buffer (modalkit's rope),
- cursor + selection,
- mode (normal/insert/visual/replace + operator-pending),
- the rollback checkpoint (`saved_text` + CRDT frontier — see Rollback).

Contract (the seam everything else is tested against):

- `apply_keys(&str) -> Vec<EditOp>` — feed a key sequence in modalkit/vim
  notation (`"idata<Esc>"`, `"2dd"`, `"<C-w>"`); returns the **`(char_offset,
  insert, delete)` edit-ops** the keystrokes produced. (The app's current
  `dispatch.rs` already turns vim actions into insert/delete tuples — `EditorCore`
  formalizes that, returning the ops instead of calling an RPC inline.)
- `apply_remote_ops(&[EditOp])` — merge a peer's CRDT edits into the buffer and
  **transform this session's cursor** against them (vi.md's old risk #2, now a
  single owned, unit-testable method).
- state accessors: `text()`, `cursor()`, `mode()`, `selection_rects()`,
  `dirty()`.

> **Dependency note (the cost of kernel-side keys).** modalkit `0.0.25` pulls in
> `crossterm 0.29`. Putting the vim engine behind the kernel means modalkit —
> and crossterm — enter the **kernel/server dependency graph**. The key *types*
> don't need a live TTY, so this is sound. **Accepted (Amy, 2026-06-22):** the
> kernel was expected to grow these deps anyway; not worth fighting modalkit's
> features to trim crossterm. Move on.

### 2. Kernel editor sessions — the tool-shaped surface

The kernel holds a registry of open editor sessions. Each session = one
`EditorCore` bound to a `(context_id, block_id)`. Multi-user: many sessions
(each its own cursor/mode) can target one shared block; their edits merge through
the CRDT. The surface (kj verbs + MCP tools, mirroring `*_input`):

| Verb | Does |
|---|---|
| `editor_open(path)` | `resolve_editor_target(path)` (shipped) → load block text into a fresh `EditorCore` → return a session handle + initial state. Signals the app peer to open a renderer. |
| `editor_keys(session, keys)` | `EditorCore::apply_keys` → mirror the returned edit-ops onto the CRDT block (`block_store.edit_text`) → return new state. |
| `editor_state(session)` | read text/cursor/mode/selection/dirty (what a renderer draws). |
| `editor_save(session)` | `ZZ` — flush the CRDT doc to its owner; advance the checkpoint. |
| `editor_quit(session)` | `ZQ` — diff-rollback to checkpoint (see Rollback), drop the session. |

Remote CRDT ops on an open block flow into every session's
`EditorCore::apply_remote_ops`, so cursor reconciliation is **one kernel
responsibility**, not scattered per-client.

### 3. App renderer — `Screen::Editor` + MSDF panel

The app becomes a **renderer + key forwarder** for the editor surface (its
compose-surface VimMachine is untouched; unifying compose onto `EditorCore` +
the input doc is a *later* possibility, explicitly out of scope here):

- captures keystrokes → `editor_keys(session, …)`;
- renders `editor_state` onto an **in-scene MSDF panel** on a new `Screen::Editor`,
  reusing the time-well's panel primitive (`view/time_well/panel.rs`,
  `create_msdf_panel`/`commit_panel_glyphs`; `ReadingCard` is the template);
- draws the **cursor quad** from parley geometry
  (`Cursor::from_byte_index(layout, byte_idx, affinity).geometry(layout, w)`) and
  **selection rects** from `Selection::geometry(layout)` — both off-the-shelf in
  parley 0.7, from the same layout the MSDF path already builds.
- Latency: every keystroke round-trips to the kernel (as compose's `edit_input`
  already does). If it feels laggy, the app keeps a local *optimistic* cursor/mode
  mirror and reconciles on kernel ack — defer until measured.

The 3D viewer thus adds **no editing logic** — it renders kernel state and
forwards keys. That is why it can come last.

---

## Testability — two GPU-free layers (the whole point)

1. **Vim semantics (pure `kaijutsu-editor` unit tests).** Drive `EditorCore` with
   key sequences; assert resulting text + cursor + the emitted edit-op stream.
   *"Does `2dw` emit the right delete? Does `<Esc>` leave insert mode?"* No Bevy,
   no kernel.
2. **Editing lifecycle (kernel e2e, tool-shaped).** Drive `editor_open` /
   `editor_keys` / `editor_state` / `editor_save` / `editor_quit` against a live
   kernel; assert the CRDT block, flush-to-owner, `ZQ` rollback, and a concurrent
   second-session merge. Reuses the `e2e_shell` / live-eval harness. No GPU.

A thin driver composing the two (keys → `EditorCore` → `editor_keys` → kernel)
gives full headless e2e of the entire feature *before the renderer exists*. The
same surface is what a model plays.

---

## Decisions (carried + reframed)

1. **Dispatch: kaish builtin.** A real `vi`/`edit <path>` builtin (and `kj rc
   edit` with no `--content`) resolves the path's *owning* `(ctx, block)` (see
   Path resolution), opens a kernel session, and signals the app peer. Follows the
   `switch_context` peer-invocation precedent.
2. **Save model: bind the session buffer to the CRDT block** — edits are CRDT ops
   (a merge), never a whole-file replace, so concurrent edits are never clobbered.
3. **Binding tightness: live keystroke sync.** `editor_keys` mirrors each edit to
   the CRDT immediately; remote ops merge back via `apply_remote_ops`. Fully
   collaborative; the rollback story keeps a clean `ZQ` possible.
4. **Command line: keybind save/quit only** in pass 1 (`ZZ`/`ZQ`, Esc-Esc to
   close). The `:` command bar is a later pass. (Multi-line *selection-highlight*
   is **not** a punt — `Selection::geometry` gives it on the MSDF panel.)
5. **Rollback: inverse forward edit against a checkpoint** (see Rollback), now
   held in the session's `EditorCore`.
6. **Surface: MSDF panel on `Screen::Editor`** (decided 2026-06-22), reusing the
   time-well substrate. Rejected: a flat 2D vello overlay (doesn't share the 3D
   scene) and editing the time-well `ReadingCard` in place (couples vi to the
   well).

### Path resolution — bind to the owner, not a copy (SHIPPED)

`resolve_editor_target(path, blocks, file_cache)` in
`crates/kaijutsu-kernel/src/editor.rs` is **path-kind aware**, and that is
load-bearing:

- **config-owned** (`/etc/rc/*`, `/etc/config/*`): bind to
  `(config_doc::config_context_id(path), config_doc::first_block_id(blocks, ctx))`
  — the ConfigCrdtFs-owned block, the sole source of truth. Edits land in the
  rc/config doc directly; `kj rc edit` is correct and collaborative.
- **ordinary file**: `FileDocumentCache::get_or_load(path)`.

The CRDT-owned-config work made rc/config sole-owned single-block `DocKind::Config`
documents. Running a config path through `get_or_load` would mint a *separate*
`FileDocumentCache` copy shadowing that owner — reviving the dual-ownership
write-through bug class that work deleted (`docs/config-crdt-ownership.md`).
Missing config docs **fail loud** (no empty editor). Sharp edge to revisit: the
prefix check ideally belongs to the mount table ("what owns this path?"), not a
hardcoded prefix. 3 tests green.

---

## Rollback / checkpoint (diamond-types-extended)

We run `diamond-types-extended` 0.2 (eg-walker list CRDT). The checkpoint lives in
the session's `EditorCore`.

- **Checkpoint primitive: yes.** `Document::version() -> &Frontier`
  (`document.rs:107`) is a checkpoint token — captured at each `editor_save`/`ZZ`.
  `ops_since(&frontier)` (`document.rs:321`) returns *precisely* the op-set since
  that checkpoint. "What changed since last save" is a first-class query.
- **Op-truncation / history deletion: no — by design.** No public `truncate`/
  `revert`; deleting shared ops would corrupt merge for any peer who already
  pulled them. Append-only is load-bearing.
- **Trap:** `Branch::checkout_at_version(_frontier)` (`branch.rs:36`) *looks* like
  "materialize document as of version X" but **ignores its argument — it is a
  stub.** Do not build on it.

**Therefore rollback = inverse forward edit, not history erasure:**

```
editor_save / ZZ  →  checkpoint = (saved_text, version())   // session holds saved_text
…editor_keys…     →  live CRDT ops (shared, collaborative)
editor_quit / ZQ  →  diff(current, saved_text) → edit ops   // forward "undo" edit
```

Cheap, restart-safe, collaboration-safe (peers see an undo edit land). Open
detail: if collaborators edited *after* our last save, scope the inverse edit to
*our* delta (`ops_since(checkpoint)`), not a blind reset to `saved_text` — or
accept last-writer semantics for pass 1 and note it.

---

## Risks / things to confront in the plan

1. ~~**modalkit (→ crossterm) in the kernel graph.**~~ Resolved — accepted
   (Amy, 2026-06-22): the kernel was expected to grow these deps anyway. Not a
   risk, just a noted footprint.
2. **Cursor reconciliation on remote ops.** Now a single kernel-owned method
   (`EditorCore::apply_remote_ops`) — *better* than the old per-client scatter,
   and directly unit-testable. Still the genuinely fiddly part: a remote op
   shifts char offsets; transform the local cursor against incoming ops.
3. **Principal→peer addressing for the open signal.** Pass 1 targets the single
   well-known nick `"kaijutsu-app"` (`editor::APP_PEER_NICK`); fail loud if absent
   (no silent no-op). Multi-user submitting-peer addressing is deferred. Known
   related bugs: peer registry empties after kernel restart until `kj` restart
   (`tech_debt_peer_reattach_on_reconnect`); `switch_context` doesn't drive Screen
   state (`tech_debt_switch_context_screen_transition`) — the editor-open path may
   hit the same Screen-transition gap.
4. **`edit_block` RPC may be unnecessary now.** With sessions kernel-side, the
   kernel applies edits via `block_store.edit_text` directly; the app sends *keys*,
   not edits. A generic `edit_block` RPC is still reusable elsewhere but is **off
   the editor's critical path** — don't build it just for vi.
5. **rc-write capability.** `/etc/rc/*` needs the rc-write capability; the rest of
   `/etc` is denied flat (`file_tools/edit.rs`, `path.rs`). The open + save path
   must surface permission errors loudly — crash over corruption.
6. **Enter binding.** Compose uses `submit_on_enter()` (Enter submits). The editor
   needs Enter to insert a newline — a *separate* `VimBindings` set inside
   `EditorCore`, not the app.
7. ~~**A renderable editor surface, distinct from the conversation timeline.**~~
   Resolved by the substrate decision (MSDF panel + `Screen::Editor`) **and the
   render-path decision (Amy, 2026-06-23): Design A — the app renders the editor
   from a kernel-served `editor_state` channel and NEVER joins the editor's
   context into `DocumentCache`.** The feared `DocKind`-discriminator collision
   cannot occur: the app's cache only holds contexts it explicitly *joins*, and
   block ops for an un-joined context are dropped (`view/sync.rs` — the
   `BlockTextOps` handler only applies to an existing cache entry). So there is
   no editor cache entry to collide with a conversation, and **no `DocKind`
   discriminator is needed** (rejected Design B: joining the editor context into
   `DocumentCache`, which would have required `DocKind` on the wire + a cache
   discriminator). The editor is cleanly separated from the conversation cache.

---

## Build order

**Slice 1 — kernel, headless, test-first** (no GUI):

1. ✅ `resolve_editor_target` (shipped, `editor.rs`).
2. ✅ `kaijutsu-editor` crate: `EditorCore` + `EditOp`, built on modalkit. Pure
   unit tests (keys → ops + state). *Test layer 1.*
3. ✅ Kernel editor-session registry + the `editor_*` surface.
   - ✅ `EditorSessions` registry (`editor.rs`): open/keys/state/save/quit,
     keystrokes mirror onto the owning CRDT block, checkpoint-backed `ZQ`
     rollback. e2e lifecycle tests against a live block store. *Test layer 2.*
   - ✅ Kernel-wide registry behind a mutex (`SendSessions`; the `!Send`
     `EditorCore` stays inside sync critical sections), exposed as
     `Kernel::editor_open/keys/state/save/quit`.
   - ✅ `kj editor open/keys/state/save/quit` verbs (`kj/editor.rs`); e2e test
     proves `keys` mutates the real rc doc (read back via VFS) and `quit` rolls
     it back.
   - ⬜ MCP tools: deferred — a model can already drive via `kj editor` through
     the shell (the hybrid kj-rich/MCP-narrow stance). Add a narrow MCP wrapper
     only if a non-shell driver needs it.
4. ✅ `vi`/`edit` builtin + `kj rc edit <path>` (no `--content`) → `editor_open`.
   - ✅ `ViBuiltin` (`runtime/vi_builtin.rs`): a real kaish `Tool` registered
     under both `vi` and `edit` (`kj/context_shell.rs`), calling the shared
     `Kernel::editor_open` primitive. Session-id-only — opens, returns the
     handle + initial state; **no peer signal yet** (lands with slice 2).
   - ✅ Bare `kj rc edit <path>` (no `--content`) opens an editor session via
     the *same* primitive instead of erroring (`kj/rc.rs::rc_edit`); the
     `--content`/stdin replace path is unchanged.
   - ✅ One shared state shape: `EditorState::to_json` — `kj editor`, the
     `vi`/`edit` builtin, and `kj rc edit` all emit the identical record (no
     drift between front doors). 3 vi-builtin tests + the rewired rc test green.

**Slice 2 — app, on the runner** (verify visually). Render path = **Design A**
(decided 2026-06-23): the app gets editor state over a dedicated channel and
never joins the editor context into `DocumentCache` (see risk #7). **Channel =
push, not poll** (decided 2026-06-23, Amy): a `subscribeEditor` callback mirroring
`subscribeBlocks`, so remote CRDT merges into an open block reach every renderer
the instant they land — collaborative editing is the point, and poll would lag it.
The concrete wire/build plan is **"Slice 2 build plan" below**. Groundwork first:

0a. ✅ **General Screen-transition fix** (commit `116ed57`). `handle_context_switch`
    (`view/sync.rs`) now drives the `Screen` FSM on a *landed* switch:
    `screen_revealing_switched_context` keys on the screen being left, so every
    switch writer (peer `switch_context`, server-pushed `ContextSwitched` from
    fork / `kj context switch`, dock, …) reveals the conversation uniformly —
    closing the `tech_debt_switch_context_screen_transition` gap. Editor-open is
    the mirror: it drives `Screen::Editor` from its own landing handler, not here.
    Verified live on the runner (in the well → `kj context switch` → pops to
    Conversation). Decision logic unit-tested.
0b. ✅ **App-id addressing infrastructure** (multi-app). The peer registry no
    longer clobbers (it keyed by the shared nick, last-attach-wins); it's now
    keyed by a per-window `instance` with server-stamped `principal` and
    by-nick/principal/instance addressing — so the editor-open signal can target
    the requesting app (submitter-aware) or fan out to a principal's windows
    (fallback). Commits: `55d285e` (whoami `principalId` — the principal-population
    gap), `41e236c` (registry), `bdcc0b2a` (bridge-task self-detach on
    `conn_cancel` + `reap_closed` — fixes a latent lingering-task leak),
    `63ff4d7a` (capnp `instance` + app mints a per-process UUID + `same_channel`
    self-detach identity guard so a re-attach can't be clobbered). 11 registry +
    3 peer-e2e tests; single-window runner-verified (`kj context switch` reaches
    the instance-keyed app). **Remaining for the routing itself (slice-2, with
    `open_editor`):** capture the submitter at `editor_open` and pick the target;
    **verify the submitter-side `KjCaller.principal` is populated** (Amy's
    principal-population caveat — whoami + peer-principal are done). Live
    2-window coexistence check wants a second window.
5. `Screen::Editor` + MSDF panel rendering editor state; key forwarding to
   `editor_keys`; cursor quad + selection rects from parley.
6. Optimistic local mirror only if latency demands it.

---

## Slice 2 build plan (2026-06-23) — dependency-ordered, test-first where headless

Slice-2 groundwork (0a Screen-transition, 0b app-id addressing) is **done**. The five
steps below are the remaining work, ordered so each is testable before the next
depends on it. Steps 1–2 are headless (e2e harness, no GPU); steps 3–5 are the
runner-verified renderer. **Render channel = push** (`subscribeEditor`).

### Step 1 — editor wire surface (headless, TDD). *The gate for everything app-side.*

> **1a SHIPPED (2026-06-23).** The full request/response surface + the push
> subscription are wired and green over real SSH + Cap'n Proto. capnp:
> `EditorState` struct + `editorOpen/Keys/State/Save/Quit @84–88` + `EditorEvents`
> (`onEditorState`/`onEditorClosed`) + `subscribeEditor @89`. Kernel: `EditorFlow`
> on a new `editor_flows` bus; `editor_keys`/`editor_save` publish `StateChanged`,
> `editor_quit` publishes `Closed`. Server: 5 handlers + a `subscribe_editor`
> bridge (mirrors `subscribe_blocks` — cancel + callback-timeout + health-reap).
> Client: `KernelHandle::editor_*` + `subscribe_editor`; `EditorState` +
> `parse_editor_state`; `EditorEventsForwarder` + the public
> `editor_events_channel()` building block; `ServerEvent::EditorStateChanged`/
> `EditorClosed`. e2e: `crates/kaijutsu-server/tests/editor_wire.rs` (2 tests) —
> open/keys/state/quit + rollback over the wire, and keys/save/quit pushes
> received on the subscription. **Deferred to 1b** (its own task): the actor
> auto-resubscribe-on-connect (the renderer in step 4 consumes
> `editor_events_channel`), and the remote-merge push below.

There is **zero** capnp plumbing for the editor today; the `Kernel::editor_*`
methods (`kernel.rs:842–887`, return `editor::EditorState`) never cross the wire.
Add a request/response surface **plus** a push subscription, mirroring the input-doc
surface (`editInput`/`getInputState`, capnp `@44–48`) and the block subscription
(`subscribeBlocks @39` + `BlockEvents`).

- **`kaijutsu.capnp`** (next free method is `@84`; add an `EditorState` struct):
  - `struct EditorState { session @0 :UInt64; text @1 :Text; cursor @2 :UInt64; mode @3 :Text; dirty @4 :Bool; }`
  - `editorOpen  @84 (path :Text, trace :TraceContext) -> (state :EditorState)` —
    no `contextId`: the path resolves the owning block; session ids are global.
  - `editorKeys  @85 (sessionId :UInt64, keys :Text, trace :TraceContext) -> (state :EditorState)`
  - `editorState @86 (sessionId :UInt64, trace :TraceContext) -> (state :EditorState)`
  - `editorSave  @87 (sessionId :UInt64, trace :TraceContext) -> (state :EditorState)`
  - `editorQuit  @88 (sessionId :UInt64, trace :TraceContext) -> ()`
  - `interface EditorEvents { onEditorState @0 (sessionId :UInt64, state :EditorState); onEditorClosed @1 (sessionId :UInt64); }`
  - `subscribeEditor @89 (callback :EditorEvents)`
- **Kernel push source** (`flows.rs`): add `EditorFlow::StateChanged { session_id, state }` /
  `Closed { session_id }` with topics `editor.state_changed` / `editor.closed`, and a
  bus on the kernel (clone-able like `block_flows()`). Split into two passes:
  - **1a (local-edit push):** the kernel's `editor_keys/save/quit` publish after the
    registry mutates (`kernel.rs:855–887`). `EditorCore` is `!Send` and the registry
    is pure, so the kernel wrapper (which already holds the bus) is the publish site,
    not `EditorSessions`. The subscription goes live + e2e-testable here.
  - **1b SHIPPED (2026-06-23) — remote-merge push (risk #2, the reason push beats
    poll).** `EditorCore::apply_remote_text(new_text) -> bool` reconciles the buffer to
    the block's *merged* truth and transforms the leader cursor against the single-region
    diff (edit-before shifts, edit-after leaves, straddle lands at the replacement end);
    identical text returns `false` (the self-write-echo skip). `EditorSessions::reconcile_block`
    finds sessions bound to `(ctx, block)`, skips self-writes (their buffer already equals
    the block — the mirror is faithful), merges stale siblings, returns the changed states;
    `Kernel::editor_reconcile_block` publishes them. The server's `spawn_editor_reconciler`
    (one per kernel, dedicated thread + LocalSet, mirrors `spawn_turn_driver`) drains
    `block.text_ops` and drives it — so a sibling session / MCP edit / streaming turn that
    writes a bound block pushes the merged state. Tests: 4 in `kaijutsu-editor`
    (`apply_remote_text` + cursor transform), 2 in `kaijutsu-kernel` (`reconcile_block`
    self-skip + unbound no-op), 1 e2e in `editor_wire.rs` (two sessions on one block; A's
    edit pushes merged state to B — over the wire). **Pass-1 scope:** text-level reconcile
    (the block is the merged truth, so re-read is canonical) with a one-region cursor
    transform; `set_text` resets undo history (a remote merge is disruptive anyway), and
    richer multi-site op transforms remain future work. **Still deferred:** the app actor's
    auto-resubscribe-on-connect — that's step-4 renderer wiring (it consumes
    `editor_events_channel`), not a kernel concern.
- **Server** (`rpc.rs`): five request handlers (copy the `editInput`/`getInputState`
  shape, ~`4345–4426`; trace-extract; facade-gate the mutators `editorKeys/Save/Quit`,
  leave `editorState` ungated) + a `subscribe_editor` bridge task copying
  `subscribe_blocks` (`rpc.rs:2081+`): `spawn_local`, `editor_flows.subscribe("editor.*")`,
  match `EditorFlow` → `callback.on_editor_state_request()`.
- **Client** (`rpc.rs` + `actor.rs`): five façade methods (copy `edit_input`/
  `get_input_state`, `client/src/rpc.rs:1562–1652`); an `EditorEventsForwarder`
  implementing `editor_events::Server` (copy `BlockEventsForwarder`,
  `subscriptions.rs:165–203`) forwarding to a new `ServerEvent::EditorStateChanged`/
  `EditorClosed`; subscribe in the actor on connect/reconnect (mirror
  `resubscribe_blocks`, `actor.rs:1769–1799`).
- **Test (layer 2++):** e2e in `kaijutsu-server` (reuse `e2e_shell`/live-eval): open via
  the client RPC, send keys, assert pushed `EditorState` arrives on the subscription
  and matches `editorState`; assert a second session's remote edit pushes a
  `StateChanged` to the first. No GPU.

### Step 2 — `open_editor` peer signal (headless-testable kernel half).

Today `editor_open` is **session-id-only** — it returns a handle but fires no peer
signal (deferred from step 4 on purpose). Wire it now:

- **Kernel**: at the `vi`/`edit`/`kj rc edit` open path, after opening the session,
  `invoke_peer` the requesting app with action `"open_editor"` and params
  `{ session, context_id, path }`. **Submitter-aware:** target the submitting peer's
  `instance` (`peers.rs:172`), fall back to `senders_by_principal` (`peers.rs:190`)
  for that principal's other windows, then the single `APP_PEER_NICK` as last resort.
  **Confront the flagged gap first:** verify the submitter-side `KjCaller.principal`
  is actually populated at this path (whoami + peer-principal are fixed; this submitter
  side is unverified — Amy's caveat). If it is nil, fixing population is part of this step.
- **App**: new arm in `dispatch_peer_action` (`peers/systems.rs:39–106`,
  `"open_editor"`): deserialize params, write `EditorOpenRequested { session, context_id, path }`.

### Step 3 — `Screen::Editor` FSM + landing handler.

- Add `Editor` to `Screen` (`ui/screen.rs:18–31`); `OnEnter(Editor)` hides conversation
  chrome (mirror `TimeWell` enter/exit, `time_well/mod.rs:37–71`), `OnExit` restores.
- `screen_revealing_switched_context` (`view/sync.rs:18–34`) already treats any
  non-Conversation screen as "reveal Conversation on a context switch" — `Editor`
  inherits that for free (a `kj context switch` while editing pops to conversation).
- New landing system `handle_editor_open` (mirror `handle_context_switch`,
  `view/sync.rs:409–498`): reads `EditorOpenRequested`, stores the session in an
  `ActiveEditor` resource, `next_screen.set(Screen::Editor)`.

### Step 4 — MSDF panel renderer (on the runner).

- Reuse the panel primitive: `create_msdf_panel` + `commit_panel_glyphs`
  (`time_well/panel.rs:31–58`); spawn one editor panel on `OnEnter(Editor)`.
- An `EditorEventsForwarder` event drains (step 1) into the `ActiveEditor` resource;
  a render system (gated `run_if(in_state(Screen::Editor))`) rebuilds the parley
  `Layout` from the editor text (`text/shaping/font.rs:34–67` `layout`), collects
  glyphs (`collect_msdf_glyphs`, `time_well/text.rs:54–70`), and commits on change.
- **Cursor quad + selection rects:** reuse the compose precedent verbatim —
  `parley::editing::Cursor::from_byte_index(&layout, byte, Affinity::Upstream)
  .geometry(&layout, 2.0)` and the `OverlayCursorGeometry` → `BlockFxMaterial`
  `cursor_params`/`selection_params` shader uniforms (`view/overlay.rs:348–400`,
  `shaders/mod.rs:145–196`). Editor cursor byte = char-offset→byte over the text.

### Step 5 — key forwarding.

- `editor_dispatch_keys` system gated `run_if(in_state(Screen::Editor))`: drain
  `KeyboardInput`, translate to modalkit/vim key-notation strings, `IoTaskPool`-spawn
  `handle.editor_keys(ctx, session, keys)` (mirror `apply_insert`/`apply_delete`,
  `input/vim/dispatch.rs:127–195`). The push subscription returns the new state — no
  optimistic mirror until latency is measured (step 6, deferred).

**Wire-surface debt to retire at "done":** every new capnp method/struct re-justified
(the tech-debt sweep). `subscribeEditor` earns its keep via the remote-merge push;
the five request methods mirror the input-doc surface. Fold landed pieces out of
`docs/issues.md` as they ship.

---

## Picking up next session (state as of 2026-06-23)

**Slice 1 AND step 4 are done, headless and green.** The whole editing increment
plus its ergonomic front doors are built and test-driven with no GUI: resolver →
`kaijutsu-editor` `EditorCore` (pure modalkit vim) → kernel `EditorSessions`
(mounted, `Send`-wrapped) → `kj editor` verbs → `vi`/`edit` builtin + bare
`kj rc edit`. A model or a shell can open a real vi session against any rc/config
block with `vi <path>` *now*. **28 editor tests green** (14 in `kaijutsu-editor`,
14 in `kaijutsu-kernel`).

Slice-1 commits: `110bd95` resolver · `5383402` `EditorCore` · `e0b3ec3` session
registry · `8ca5674` kernel mount · `bc11e2c` `kj editor` · `cf5e663` coverage
battery · `4bf2c28` corruption + trailing-newline fixes. Step 4 (this pass):
`ViBuiltin` + `kj rc edit` editor-open + shared `EditorState::to_json`.

**Step-4 decisions (Amy, 2026-06-23):** `vi`/`edit` is a *real* kaish builtin
(not a `kj editor open` alias); `kj` reaches the editor through the same shared
`Kernel::editor_open` primitive when it needs one (so there's no duplicated
logic — the three front doors all funnel to one kernel method + one
`EditorState::to_json` shape). Opening is **session-id-only**: it returns a
handle for headless driving and does **not** fire the `open_editor` peer signal
yet — that wiring waits for the slice-2 renderer (avoids the `APP_PEER_NICK` /
`switch_context`-Screen gaps until there's something to render).

**Next is slice 2** — the app renderer (`Screen::Editor` + MSDF panel), on the
runner. The renderer adds *no editing logic*; it renders `editor_state` and
forwards keys. The `open_editor` peer signal (risk #3) gets wired here, where it
finally has a renderer to target. Deferred items live in the tech-debt sweep above.

---

## Tech debt — sweep before "done"

vi is not done until we circle back and clean up the scaffolding the build
accreted. A running list (add to it as we go); **none of this blocks shipping the
working feature, but all of it blocks calling it finished**:

- **Redundant wire surface.** Every RPC / MCP tool / capnp method added for the
  editor gets re-justified at the end. Prime suspect: a generic `edit_block` RPC —
  vi.md (risk #4) says the editor doesn't need it (the app sends *keys*, the
  kernel writes via `block_store.edit_text`). If one lands anyway, either find it
  a second consumer or pull it.
- **The path-resolution prefix check** (`config_owned` in `editor.rs`) is a
  hardcoded `/etc/rc` + `/etc/config` test; it should become a mount-table
  question ("what owns this path?"). Noted at the resolver.
- **Peer addressing** is pass-1 single-nick (`APP_PEER_NICK`); generalize to the
  submitting peer (risk #3) before multi-user.
- **Any optimistic-mirror / latency hack** in the app renderer (if we add one)
  gets revisited once measured.
- **Trailing-newline consistency — ✅ FIXED.** modalkit's `EditRope` is
  line-terminated; `EditorCore` strips one terminator, and the **session** now
  keeps the block's terminator aside (`EditorSession.terminator`) so dirty/
  rollback compare against the normalized view: a newline-terminated block opens
  clean and `ZQ` re-applies the terminator. Spec:
  `newline_terminated_block_opens_clean_and_quit_preserves_terminator`. Residual
  (deferred): full byte-fidelity for exotic/CRLF terminators ties into the
  hashline/CRLF preservation work.
- **`EditOp` granularity.** `apply_keys` emits one contiguous prefix/suffix diff
  per keystroke; a multi-site change (macro, multi-cursor) would need a richer
  diff. Fine for pass 1; revisit with multi-cursor.
- **Prompt-key buffer corruption — ✅ FIXED.** `:`/`/`/`?` focus modalkit's
  command-line/search bar (a separate buffer we don't implement); `EditorCore`
  now suppresses document edits while that bar is focused (set on
  `CommandBar(Focus)`, cleared on `Prompt(Submit|Abort)`), so an unwired
  `:`/`/` is a safe no-op and editing resumes after. Spec:
  `command_line_keys_must_not_corrupt_the_buffer`.
- **Dot-repeat (`.`) — deferred (not corruption).** Produces no edit action in
  our minimal modalkit setup (inert no-op). Wiring real `.` repeat needs
  modalkit's last-edit-sequence machinery; revisit when the `:`/search surface
  is built.

### Command surface the e2e covers (verified)

`kaijutsu-editor`'s `coverage` test battery is the executable map: the e2e drives
the full **normal-mode editing surface** headless — motions, operators, counts,
linewise ops, inserts (`i`/`a`/`A`/`o`), registers (yank→paste), **undo `u` +
redo `<C-r>`**, visual-mode + operator, find-char. *Not yet* wired (safe no-ops
now, not corruption): `:` ex-commands, `/`·`?` search, `.` dot-repeat.

Mirror the live items into `docs/issues.md` (the backlog/pressure-valve) as they
appear, and delete them here + there when they ship.

## Key file anchors

Paths are under `crates/` (workspace-crates layout). Line numbers drift — treat
as hints, grep the symbol.

| Concern | Location |
|---|---|
| **Editor target resolver** (shipped) | `crates/kaijutsu-kernel/src/editor.rs` (`resolve_editor_target`, `config_owned`, `APP_PEER_NICK`) |
| **Precedent: kernel-owned buffer + tool surface** | `crates/kaijutsu-kernel/src/input_doc.rs`; `edit_input`/`read_input`/`write_input`/`submit_input` (MCP + `block_store.rs`, `server/src/rpc.rs:4291`) |
| **Vim engine to extract** | `crates/kaijutsu-app/src/input/vim/mod.rs` (`VimMachine`, `TerminalKey`, `submit_on_enter`) |
| **Action→edit-op precedent** | `crates/kaijutsu-app/src/input/vim/dispatch.rs` (`apply_insert`/`apply_delete`; sink is `edit_input` — the seam to cut) |
| modalkit dep | `modalkit 0.0.25` → `crossterm 0.29` (footprint note above) |
| CRDT text edit | `crates/kaijutsu-kernel/src/block_store.rs:1469` (`edit_text`/`edit_text_as`) |
| Peer signal (precedent) | `crates/kaijutsu-kernel/src/kernel.rs:994` (`invoke_peer`); app nick `peers/mod.rs` |
| **MSDF panel primitive** (renderer) | `crates/kaijutsu-app/src/view/time_well/panel.rs`; template `view/time_well/text.rs` (`update_reading_card`), `scene.rs:63` (`ReadingCard`) |
| **Screen FSM** (add `Screen::Editor`) | `crates/kaijutsu-app/src/ui/screen.rs` |
| Cursor / selection geometry | parley 0.7 `Cursor::from_byte_index(..).geometry`, `Selection::geometry` (`editing/{cursor,selection}.rs`) |
| `kj rc edit` (entry point) | `crates/kaijutsu-kernel/src/kj/rc.rs` (`rc_edit` — no-`--content` → `editor_open`) |
| **`vi`/`edit` builtin** (front door) | `crates/kaijutsu-kernel/src/runtime/vi_builtin.rs` (`ViBuiltin`); registered in `kj/context_shell.rs` |
| Shared editor-state shape | `crates/kaijutsu-kernel/src/editor.rs` (`EditorState::to_json`) |
| File doc cache | `crates/kaijutsu-kernel/src/file_tools/cache.rs` (`get_or_load`, `file_context_id`) |
| Config doc owner | `crates/kaijutsu-kernel/src/config_doc.rs` (`config_context_id`, `first_block_id`) |
| CRDT version / ops_since | `diamond-types-extended` `document.rs:107,321` |
| CRDT checkout stub (do not use) | `diamond-types-extended` `branch.rs:36` |

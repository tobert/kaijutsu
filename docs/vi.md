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
7. **A renderable editor surface, distinct from the conversation timeline.**
   Resolved by the substrate decision (MSDF panel + `Screen::Editor`). Residual:
   the app routes block events to the conversation MainCell (`view/sync.rs`) and
   the doc cache is conversation-keyed (`view/document.rs`); the editor renders
   from `editor_state`, sidestepping that path — but a file-doc cache entry still
   wants a `DocKind` discriminator so the two don't collide.

---

## Build order

**Slice 1 — kernel, headless, test-first** (no GUI):

1. ✅ `resolve_editor_target` (shipped, `editor.rs`).
2. ✅ `kaijutsu-editor` crate: `EditorCore` + `EditOp`, built on modalkit. Pure
   unit tests (keys → ops + state). *Test layer 1.*
3. Kernel editor-session registry + the `editor_*` surface.
   - ✅ `EditorSessions` registry (`editor.rs`): open/keys/state/save/quit,
     keystrokes mirror onto the owning CRDT block, checkpoint-backed `ZQ`
     rollback. e2e lifecycle tests against a live block store. *Test layer 2.*
   - ⬜ Surface it: kj verbs (`kj editor open/keys/…`) + MCP tools + the
     kernel-wide registry behind a mutex (the `!Send` `EditorCore` stays inside
     sync critical sections — see the registry doc).
4. `vi`/`edit` builtin + `kj rc edit <path>` (no `--content`) → `editor_open`.

**Slice 2 — app, on the runner** (verify visually):

5. `Screen::Editor` + MSDF panel rendering `editor_state`; key forwarding to
   `editor_keys`; cursor quad + selection rects from parley.
6. Optimistic local mirror only if latency demands it.

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
- **Trailing-newline fidelity.** modalkit's `EditRope` is line-terminated, so
  `EditorCore` can't tell `"hello"` from `"hello\n"`; it strips one terminator at
  the boundary. The kernel binding must remember the loaded terminator and
  re-apply on save (ties into the CRLF/hashline preservation concerns).
- **`EditOp` granularity.** `apply_keys` emits one contiguous prefix/suffix diff
  per keystroke; a multi-site change (macro, multi-cursor) would need a richer
  diff. Fine for pass 1; revisit with multi-cursor.

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
| `kj rc edit` (entry point) | `crates/kaijutsu-kernel/src/kj/rc.rs:402` (`rc_edit`) |
| File doc cache | `crates/kaijutsu-kernel/src/file_tools/cache.rs` (`get_or_load`, `file_context_id`) |
| Config doc owner | `crates/kaijutsu-kernel/src/config_doc.rs` (`config_context_id`, `first_block_id`) |
| CRDT version / ops_since | `diamond-types-extended` `document.rs:107,321` |
| CRDT checkout stub (do not use) | `diamond-types-extended` `branch.rs:36` |

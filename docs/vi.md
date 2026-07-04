# In-App vi Editor

`vi /etc/rc/thing/whatever.kai` (and bare `kj rc edit <path>`) opens a real
vi-like editor on that file's CRDT block. The editor is a **kernel-owned
session** driven through a small tool-shaped surface; the Bevy app is one
*renderer* of it, a model is another *player* of it, and a headless test is a
third *driver* of it. Same surface for all three.

## Status

**Slices 1–3 shipped.** The full stack exists and is runner-verified: path
resolver → `kaijutsu-editor` `EditorCore` (pure modalkit vim) → kernel
`EditorSessions` → capnp wire surface (`editorOpen/Keys/State/Save/Quit @84–88`
+ push `subscribeEditor @89`) → `Screen::Editor` MSDF renderer with a real
cursor quad → key forwarding. Front doors: the `vi`/`edit` kaish builtin,
`kj editor` verbs, and bare `kj rc edit <path>` — all funneling to one
`Kernel::editor_open` and one `EditorState::to_json` shape. The `:` command
dialect shipped (core verbs, a hand-rolled `:s`, `:r <file>` / `:r !cmd`),
as did Ctrl+Z suspend / `fg` resume. The cache-coherence and restart-staleness
issues are fixed.

**Open** (only in this doc; the backlog proper is `docs/issues.md`):

- **Selection rects** on the editor panel — the cursor quad ships; selection is
  the editor specialization still to wire (`view/editor/render.rs` notes it).
  `Selection::geometry(layout)` is off-the-shelf in parley 0.7.
- **`/`·`?` search and `.` dot-repeat** — safe no-ops today (the command-bar
  suppression guard makes them inert, not corrupting). Real search needs the
  bar submit wired to a search action; real `.` needs modalkit's
  last-edit-sequence machinery.
- **`:s` refinements** — bare `:s` (repeat-last) errors; `.`/`$` symbolic
  ranges, `&`/`~` repeat, and finer-than-`set_text` undo granularity deferred.
- **`:w!` changed-under-us guard.** `:w!` == `:w` today (decided 2026-06-27).
  `CommandRequest::Write{force}` carries the bang for a future guard: when the
  kernel detects the block moved since open (a concurrent writer), a plain `:w`
  refuses and `:w!` overrides — vim's "file has changed since editing started".
  Not a permission gate.
- **Exact-window peer targeting.** The `open_editor` signal fans out to the
  submitter *principal's* windows; targeting the submitter's exact `instance`
  needs the instance threaded onto the execute path
  (`ConnectionState`→`ExecContext`).
- **Optimistic local cursor/mode mirror** — only if measured latency demands it.
- **`EditOp` granularity.** `apply_keys` emits one contiguous prefix/suffix
  diff per keystroke; a multi-site change (macro, multi-cursor) needs a richer
  diff. Fine until multi-cursor.
- **Terminator byte-fidelity.** The session preserves a single trailing
  newline across open/`ZQ`; exotic/CRLF terminators tie into the
  hashline/CRLF preservation work.
- **MCP tools** — deferred; a model already drives via `kj editor` through the
  shell (the hybrid kj-rich/MCP-narrow stance). Add a narrow wrapper only if a
  non-shell driver needs it.
- **Render polish** — a long document underlaps the floating `:` strip (vim
  reserves the last row; we don't shrink the doc layout yet), and the strip
  floats above the dock rather than integrating with its status row.

Tracked in `docs/issues.md` (not repeated here): the slice-3 polish
runner-verify, `:e <path>`, and the residual `config_owned` sync prefix on the
cache-invalidation path.

---

## The core idea: the editor is the input-doc surface, generalized

The compose box is already a **kernel-owned editable buffer**: `input_doc.rs`,
fronted by a tool-shaped surface — `read_input` / `write_input` / `edit_input` /
`submit_input` (real MCP tools) — that the Bevy app merely *renders*. A model
edits the compose box over the wire right now; the GUI is one view onto it.

The vi editor is that **exact pattern pointed at a file/config block** instead
of the input doc. So the testability win isn't scaffolding we throw away — it
is the editor's real spine:

- **Thin client, smart kernel.** Editing state and semantics live in the
  kernel; the 3D viewer is a *view onto* a kernel session, not the owner of it.
- **Playable by anyone.** Many hands on one keyboard: app, model, test all
  drive the same `editor_*` surface.
- **Testable before pixels.** The whole increment is exercised headless because
  the increment was never about rendering.

This is the "instrument you play, not a harness that drives you" stance made
concrete for editing.

---

## Three pieces

### 1. `kaijutsu-editor` crate — `EditorCore` (pure)

The vim *engine* as pure logic — **no Bevy, no kernel, no RPC**. Built on
modalkit's `VimMachine` + `TerminalKey` (also used by the app's compose
surface). `EditorCore` owns one editor's session-local state: the text buffer
(modalkit's rope), cursor + selection, mode (normal/insert/visual/replace +
operator-pending + command-line), and the rollback checkpoint.

The contract everything else is tested against:

- `apply_keys(&str) -> Vec<EditOp>` — feed a key sequence in modalkit/vim
  notation (`"idata<Esc>"`, `"2dd"`); returns the `(char_offset, insert,
  delete)` edit-ops the keystrokes produced.
- `apply_remote_text(new_text) -> bool` — reconcile the buffer to the block's
  *merged* truth and transform the local cursor against the single-region diff
  (edit-before shifts, edit-after leaves, straddle lands at the replacement
  end); identical text returns `false` (the self-write-echo skip).
- Intent drains: `take_close()` (`ZZ`/`ZQ`), `take_commands()` (`:` verbs),
  `take_io()` (`:r` fetches) — the editor surfaces *intent*; the kernel acts.
- State accessors: `text()`, `cursor()`, `mode()`, `command_line()`, `dirty()`.

**modalkit `0.0.25` pulls `crossterm 0.29` into the kernel/server dependency
graph.** The key *types* don't need a live TTY, so this is sound — accepted
(Amy, 2026-06-22); not worth fighting modalkit's features to trim crossterm.

### 2. Kernel editor sessions — the tool-shaped surface

The kernel holds a registry of open editor sessions
(`crates/kaijutsu-kernel/src/editor.rs`). Each session = one `EditorCore`
bound to a `(context_id, block_id)`. Multi-user: many sessions (each its own
cursor/mode) can target one shared block; their edits merge through the CRDT.
The registry is kernel-wide behind a mutex (`SendSessions`; the `!Send`
`EditorCore` stays inside sync critical sections), exposed as
`Kernel::editor_open/keys/state/save/quit`.

| Verb | Does |
|---|---|
| `editor_open(path)` | `resolve_editor_target(path)` → load block text into a fresh `EditorCore` → return a session handle + initial state. `editor_open_signaled` also fires the `open_editor` peer invoke at the submitter principal's app windows (falling back to `APP_PEER_NICK`; a missing renderer is a `warn`, never fatal — the session is already open). |
| `editor_keys(session, keys)` | `EditorCore::apply_keys` → mirror the edit-ops onto the CRDT block (`block_store.edit_text`) → drain intents (`take_close`/`take_commands`/`take_io`) and act → return new state. **Async** since `:r` — sync-lock, release, await the fetch, sync-lock again; `EditorCore` never crosses the await, only the fetched `String` does. |
| `editor_state(session)` | read text/cursor/mode/command-line/dirty (what a renderer draws). |
| `editor_save(session)` | `ZZ` / `:w` — flush the CRDT doc to its owner; advance the checkpoint. |
| `editor_quit(session)` | `ZQ` / `:q!` — diff-rollback to checkpoint (see Rollback), drop the session. |

**The wire surface mirrors the input-doc surface.** capnp: `EditorState`
struct (text, cursor, mode, dirty, `commandLine @5`, `message @6`) +
`editorOpen/Keys/State/Save/Quit @84–88` + `subscribeEditor @89` with
`EditorEvents` callbacks. **The render channel is push, not poll** (decided
2026-06-23): remote CRDT merges into an open block must reach every renderer
the instant they land — collaborative editing is the point, and poll would lag
it. `EditorFlow::StateChanged/Closed` ride an `editor_flows` bus; the kernel's
`editor_keys/save/quit` publish after the registry mutates.

Remote ops are **one kernel responsibility**, not scattered per-client: the
server's `spawn_editor_reconciler` (one per kernel, dedicated thread +
LocalSet) drains `block.text_ops`; `EditorSessions::reconcile_block` finds
sessions bound to `(ctx, block)`, skips self-writes (their buffer already
equals the block — the mirror is faithful), merges stale siblings via
`apply_remote_text`, and publishes the changed states. So a sibling session, an
MCP edit, or a streaming turn that writes a bound block pushes the merged state
to every open renderer.

### 3. App renderer — `Screen::Editor` + MSDF panel

The app is a **renderer + key forwarder** (its compose-surface VimMachine is
untouched; unifying compose onto `EditorCore` is a later possibility, out of
scope):

- `view::editor` (`EditorPlugin`): the `open_editor` peer signal (carrying the
  initial `EditorState` — no fetch, no race) lands as `EditorOpenRequested`;
  the landing handler stores `ActiveEditor` and drives `Screen::Editor`.
  `handle_editor_events` keeps `ActiveEditor.state` fresh off the
  `subscribeEditor` push (own keystrokes, peer merges) and pops to Conversation
  on close.
- The panel reuses the time-well MSDF primitive (`create_msdf_panel` +
  `commit_panel_glyphs`), mono font, with a `:`-strip appended at the bottom
  when `command_line` or `message` is set. The **cursor quad** derives from
  parley geometry (`Cursor::from_byte_index(..).geometry`) through the shared
  `cursor_selection_uniforms` shader path.
- `editor_dispatch_keys` (gated `in_state(Screen::Editor)`) drains
  `KeyboardInput`, translates to modalkit key notation, and ships *every* key
  to `editor_keys`. The push subscription returns the new state.

The 3D viewer adds **no editing logic** — it renders kernel state and forwards
keys. The app never detects mode: app-side mode detection races the mode push
(step 5 of the build proved it by corrupting a block).

---

## Testability — two GPU-free layers (the whole point)

1. **Vim semantics (pure `kaijutsu-editor` unit tests).** Drive `EditorCore`
   with key sequences; assert resulting text + cursor + the emitted edit-op
   stream. *"Does `2dw` emit the right delete? Does `<Esc>` leave insert
   mode?"* No Bevy, no kernel. The `coverage` battery is the executable map:
   motions, operators, counts, linewise ops, inserts, registers, undo/redo,
   visual-mode + operator, find-char.
2. **Editing lifecycle (kernel + wire e2e, tool-shaped).** Drive the `editor_*`
   surface against a live kernel; assert the CRDT block, flush-to-owner, `ZQ`
   rollback, and concurrent second-session merge — including over real SSH +
   Cap'n Proto (`crates/kaijutsu-server/tests/editor_wire.rs`). No GPU.

The same surface a test drives is what a model plays.

---

## Decisions

1. **Dispatch is a kaish builtin.** `vi`/`edit <path>` is a real kaish `Tool`
   (`runtime/vi_builtin.rs`, registered in `kj/context_shell.rs`) — not a
   `kj editor open` alias. `kj editor` and bare `kj rc edit` reach the editor
   through the same shared `Kernel::editor_open` primitive; three front doors,
   one kernel method, one `EditorState::to_json` shape.
2. **Save model: the session buffer binds to the CRDT block.** Edits are CRDT
   ops (a merge), never a whole-file replace, so concurrent edits are never
   clobbered.
3. **Binding tightness: live keystroke sync.** `editor_keys` mirrors each edit
   to the CRDT immediately; remote ops merge back via the reconciler. Fully
   collaborative; the rollback story keeps a clean `ZQ` possible.
4. **Render path: Design A** (decided 2026-06-23). The app renders the editor
   from the kernel-served editor subscription and **never joins the editor's
   context into `DocumentCache`** — the app's cache only holds contexts it
   explicitly joins, block ops for an un-joined context are dropped
   (`view/sync.rs`), so no `DocKind` discriminator is needed on the wire.
   (Rejected Design B: joining the editor context into the cache.)
5. **Surface: MSDF panel on `Screen::Editor`** (decided 2026-06-22), reusing
   the time-well substrate. Rejected: a flat 2D vello overlay (doesn't share
   the 3D scene) and editing the time-well `ReadingCard` in place (couples vi
   to the well).
6. **Mode lives kernel-side, period.** The app forwards every key, including
   `:`. `ZZ`/`ZQ` and command mode are recognized in `EditorCore`; an app-side
   input surface would race the mode push and split input ownership.
7. **Enter inserts a newline.** Compose uses `submit_on_enter()`; the editor
   runs a separate `VimBindings` set inside `EditorCore`.
8. **No generic `edit_block` RPC.** The app sends *keys*; the kernel writes via
   `block_store.edit_text`. A generic block-edit RPC is off the editor's
   critical path — don't build it for vi.
9. **rc-write capability applies.** `/etc/rc/*` needs rc-write; the rest of
   `/etc` is denied flat. Open + save surface permission errors loudly — crash
   over corruption.

### Path resolution — bind to the owner, not a copy

`resolve_editor_target(path, blocks, file_cache, mounts)` in
`crates/kaijutsu-kernel/src/editor.rs` is **ownership-aware**, and that is
load-bearing:

- **config-owned**: the mount table answers — `MountTable::owner_of(path)` +
  `VfsOps::owns_config_docs()` (the config-doc backends answer for themselves;
  `ConfigCrdtFs` returns `true`). Bind to
  `(config_doc::config_context_id(path), config_doc::first_block_id(..))` —
  the ConfigCrdtFs-owned block, the sole source of truth. A config path is
  config-owned only when its backend is actually *mounted* (you can't edit an
  unmounted tree).
- **ordinary file**: `FileDocumentCache::get_or_load(path)`.

The CRDT-owned-config work made rc/config sole-owned single-block
`DocKind::Config` documents. Running a config path through `get_or_load` would
mint a *separate* `FileDocumentCache` copy shadowing that owner — reviving the
dual-ownership write-through bug class that work deleted
(`docs/config-crdt-ownership.md`). Missing config docs **fail loud** (no empty
editor).

---

## Rollback / checkpoint (diamond-types-extended)

We run `diamond-types-extended` 0.2 (eg-walker list CRDT). The checkpoint
lives in the session.

- **Checkpoint primitive: yes.** `Document::version() -> &Frontier` is a
  checkpoint token — captured at each `editor_save`/`ZZ`.
  `ops_since(&frontier)` returns *precisely* the op-set since that checkpoint.
- **Op-truncation / history deletion: no — by design.** No public
  `truncate`/`revert`; deleting shared ops would corrupt merge for any peer who
  already pulled them. Append-only is load-bearing.
- **Trap:** `Branch::checkout_at_version(_frontier)` *looks* like "materialize
  document as of version X" but **ignores its argument — it is a stub.** Do
  not build on it.

**Therefore rollback = inverse forward edit, not history erasure:**

```
editor_save / ZZ  →  checkpoint = (saved_text, version())   // session holds saved_text
…editor_keys…     →  live CRDT ops (shared, collaborative)
editor_quit / ZQ  →  diff(current, saved_text) → edit ops   // forward "undo" edit
```

Cheap, restart-safe, collaboration-safe (peers see an undo edit land). Pass-1
semantics: the inverse edit resets to `saved_text` (last-writer); scoping it to
*our* delta via `ops_since(checkpoint)` when collaborators edited after our
save is a known refinement.

The session also keeps the block's trailing terminator aside
(`EditorSession.terminator`), so a newline-terminated block opens clean and
`ZQ` re-applies the terminator.

---

## Command mode — the editor's `:` dialect

Command mode extends the key-forwarder seam; it does **not** add an app input
surface. Two decisions (Amy, 2026-06-24):

1. **The surface stays kernel-owned.** `:` and command mode live in
   `EditorCore`/modalkit; the app keeps forwarding every key. Rejected: an
   app-side `:` input surface (a "vi-flavored shell dock") — it would
   reintroduce the mode-detection race, split input ownership, and add a third
   compose surface.
2. **The dialect is its own thing, not kaish.** `:` is a distinct
   editor-command dialect (ex-flavored), not kaish-scoped-to-the-session. The
   shell escape bridges *to* kaish; kaish never owns `:`.

**Mechanism — the intent pattern, not modalkit's command machine.** modalkit's
command machine was probed and **not adopted**: its default set lacks
`:wq`/`:x` and one `exec` emits one `Action`, so `:wq` can't compose
write+quit. Instead modalkit owns the command-mode *keystroke plumbing*
(`CommandBar(Focus)` → a real cmdline `EditBuffer` with backspace/cursor →
`Prompt(Submit)`) and we own the grammar (`parse_ex_command`). Each submitted
line becomes a **`CommandRequest`** intent drained via `take_commands()` — the
same `CloseRequest`/`take_close()` pattern as `ZZ`/`ZQ` — so the dialect is
headless-TDD-able: `apply_command(":wq")` asserts `[Write, Quit]`, no GPU, no
kernel. The in-progress `:`-line rides `EditorState.command_line` over the
push channel; the app renders it read-only.

### The dialect

- **Core verbs:** `:w :q :wq :q! :x :w!`. `:q` on a dirty buffer **refuses**
  (vim "No write since last change"); `:q!` discards and rolls the block back;
  `:wq`/`:x` save-then-close; `:w` saves and stays. `:w!` == `:w` (force is
  reserved — see Open).
- **Substitute:** `:s/old/new/`, `:%s/…/…/`, `:N,Ms/…/…/`. Hand-rolled
  (modalkit's `vim_cmd_substitute` is an explicit stub). **The dialect is Rust
  regex + Rust replacement syntax** (`$1` capture refs) — a deliberate choice
  over chasing vim's BRE flavor; the `:` line is its own dialect, not
  vim-exact. Flags: `g` (all-per-line), `i` (case-insensitive); unknown flag
  fails loud. Arbitrary delimiter (`:s#a#b#`). `:s` is an **edit**, not a
  kernel `CommandRequest`: it mutates the `EditorCore` buffer and rides the
  existing diff→`EditOp`→CRDT-mirror path.
- **Read:** `:r <file>` reads via `FileDocumentCache::read_content`; `:r !cmd`
  materializes a kaish in the **opener's** `(principal, context_id,
  session_id)` (the same `materialize_context_kaish` helper the model shell +
  rc lifecycle use) and splices the command's stdout — both **at the cursor**
  (not vim's linewise-below; simpler, refine later). These are the async
  intents (`EditorIo::{ReadFile, ReadShell}` via `take_io()`); a missing file,
  denied/failed command, or unfulfilled intent fails loud, never a silent
  no-op.
- **No `:!`, deliberately.** It was the entire source of complexity — nested
  editor sessions, a return stack, ephemeral-block lifecycle. The **shell is
  already a surface a keystroke away**: **Ctrl+Z** (a local app intercept —
  don't forward the key; leave to the shell but keep `ActiveEditor`; the
  kernel session is untouched, and local means it doubles as the hung-kernel
  escape hatch) and **`fg`** (a kaish builtin: `Kernel::resume_editor` finds
  the caller's session via the captured `EditorOpener` and re-fires the
  existing `open_editor` signal). vim only grew `:!` because it had nowhere
  else to go; kaijutsu does. (`:%!filter` is also out of scope; the shell
  filters.)
- **Errors report on the `:` line.** An unknown command or bad `:s` regex sets
  the transient `EditorState.message` (vim's E492) and keeps the session open,
  instead of erroring `editor_keys` out from under the renderer; it clears on
  the next keystroke batch.

Related separate thread (not part of this): the Ctrl+Z shell may become a
**shadow context** whose blocks are excluded from the conversation until
drifted — its own design pass.

---

## Key file anchors

Paths are under `crates/`. Line numbers drift — grep the symbol.

| Concern | Location |
|---|---|
| Editor sessions + resolver + state shape | `kaijutsu-kernel/src/editor.rs` (`resolve_editor_target`, `EditorSessions`, `EditorState::to_json`, `APP_PEER_NICK`) |
| Vim engine (pure) | `kaijutsu-editor/src/lib.rs` (`EditorCore`, `EditOp`, `CommandRequest`, `EditorIo`) |
| `vi`/`edit` builtin (front door) | `kaijutsu-kernel/src/runtime/vi_builtin.rs`; registered in `kj/context_shell.rs` |
| `kj editor` / `kj rc edit` | `kaijutsu-kernel/src/kj/editor.rs`, `kj/rc.rs` (`rc_edit`) |
| CRDT text edit | `kaijutsu-kernel/src/block_store.rs` (`edit_text`/`edit_text_as`) |
| Peer signal | `kaijutsu-kernel/src/kernel.rs` (`invoke_peer`, `signal_open_editor`, `editor_reconcile_block`) |
| Remote-merge reconciler | `kaijutsu-server/src/rpc.rs` (`spawn_editor_reconciler`) |
| Wire schema | `kaijutsu.capnp` (`EditorState`, `editorOpen @84` … `subscribeEditor @89`) |
| Wire e2e | `kaijutsu-server/tests/editor_wire.rs` |
| App renderer | `kaijutsu-app/src/view/editor/` (`mod.rs`, `render.rs`, `keys.rs`); screen FSM `ui/screen.rs` |
| MSDF panel primitive | `kaijutsu-app/src/view/time_well/panel.rs` |
| Precedent: input-doc surface | `kaijutsu-kernel/src/input_doc.rs` |
| Compose vim (untouched) | `kaijutsu-app/src/input/vim/` (`mod.rs`, `dispatch.rs`) |
| Config doc owner | `kaijutsu-kernel/src/config_doc.rs` (`config_context_id`, `first_block_id`) |
| File doc cache | `kaijutsu-kernel/src/file_tools/cache.rs` (`get_or_load`) |

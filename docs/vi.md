# In-App vi Editor

`vi /config/rc/thing/whatever.kai` opens a real vi-like editor on that file's
block. The editor is a **kernel-owned session** driven through a small
tool-shaped surface; the Bevy app is one
*renderer* of it, a model is another *player* of it, and a headless test is a
third *driver* of it. Same surface for all three.

Every input mutation carries the current player's identity. `kj vi` uses
its invoking performer; RPC keys, paste, and rollback use the authenticated
connection's principal. Opening a shared editor does not make its opener the
author of later players' edits. The original block author stays unchanged.
This is live mutation attribution; durable per-edit audit remains separate.

## Underneath: file buffers

The editor sits on the file-buffer layer — how kaijutsu holds file content, and
where `:w` actually lands. **`docs/file-buffers.md` is canonical for that**, and
it carries decisions that change this document: `write` and `grep` are being
removed, `edit` becomes hashline-only, and the `:w!` changed-under-us guard
below (the W12 guard, slice 3) shipped — a 2026-08-18 incident proved the
hazard real.

## Status

**Slices 1–3 shipped.** The full stack exists and is runner-verified: path
resolver → `kaijutsu-editor` `EditorCore` (pure modalkit vim) → kernel
`EditorSessions` → capnp wire surface (`editorOpen/Keys/State/Save/Quit @74–78`
+ push `subscribeEditor @79` + `editorInsert @105`) → `Screen::Editor` MSDF
renderer with a real cursor quad → key forwarding. Front doors: the `vi` kaish builtin
(in-app: the shell, the TUI's and app's `:vi`) and the `kj vi` verbs (agents and
scripts) — both funneling to one `Kernel::editor_open` and one
`EditorState::to_json` shape. The `:` command dialect shipped (core verbs, a
hand-rolled `:s`, `:r <file>` / `:r !cmd`),
as did Ctrl+Z suspend / `fg` resume. The cache-coherence and restart-staleness
issues are fixed. The editor now participates in the file-buffer layer it
sits on (`docs/file-buffers.md`): an edit to a file-backed session marks the
durable swap row, and `:w`/`:wq`/`:x`/`ZZ` flush to disk through the same
`FileDocumentCache` every other writer uses — previously the editor mirrored
edits onto the kernel block and never touched the cache at all, so `:w` on a
file never reached disk and an unsaved edit had no swap row to survive a
restart. The W12 changed-under-us guard shipped too
(`FileDocumentCache::flush_one_guarded`): a plain `:w` refuses when the disk
generation has moved past the buffer's load generation, reporting on the
status line and leaving the session open and dirty; `:w!` overrides.
`/`·`?` search with `n`/`N`, and `.` dot-repeat, also shipped (see "Search
and dot-repeat" under the `:` dialect).

**Open** (only in this doc; the backlog proper is `docs/issues.md`):

- **Selection rects: the app draw is not runner-verified.** The visual-mode
  selection travels end to end: `EditorCore::selection()` reads modalkit's
  anchor and cursor, the kernel's `EditorState.selection` and the capnp
  `EditorState.selection @7` carry it as an `EditorSelection` (shape plus
  half-open char spans, `kaijutsu-types/src/editor.rs`), and `kj vi`
  emits it in `to_json`. The tui draws it (`docs/tui.md`, "Editor and
  diff"). The app builds rects through `Selection::geometry` →
  `layout_rects` → `coalesce_selection_rects` → `pack_selection_rects` →
  `mat.selection_rects`; unit tests cover the span → rect mapping against a
  real parley layout, but nobody has looked at it in the running app yet.
  The diff viewer draws the *same* rects as `MsdfBlockGeometry` quads
  instead, because a diff selection has to sit on top of the `+`/`-` bands;
  the editor has no bands and wants the shader's over-the-text composite.
  Left: a runner check of the app highlight (including a selected empty
  line and a blockwise block), and a blockwise selection taller than 16
  rows, which overflows the material's rect uniform — `pack_selection_rects`
  drops the extra rows with a warning. The visual-mode cursor is hidden in
  the app, as before; the tui shows the terminal cursor.
- **Search and dot-repeat gaps** — search has no offsets (`/pat/e`), no
  wrap notice ("search hit BOTTOM"), no incremental highlight, and no `/` or
  `?` history recall. `.` after leaving Visual mode with `<Esc>` and no
  operator repeats nothing, because modalkit records the return-to-normal
  cursor reset as the new edit sequence; the `:`·`/`·`?` bar has the same
  modalkit behavior, and `EditorCore` works around it there only. `.` does
  not repeat an operator over a typed search (`d/pat<CR>`): `EditorCore`
  runs that motion itself at bar submit, outside modalkit's edit sequence
  (`dn` repeats).
- **`:s` refinements** — bare `:s` (repeat-last) errors; `.`/`$` symbolic
  ranges, `&`/`~` repeat, and finer-than-`set_text` undo granularity deferred.
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
- **MCP tools** — deferred; a model already drives via `kj vi` through the
  shell (the hybrid kj-rich/MCP-narrow stance). Add a narrow wrapper only if a
  non-shell driver needs it.
- **Render polish** — a long document underlaps the floating `:` strip (vim
  reserves the last row; we don't shrink the doc layout yet), and the strip
  floats above the dock rather than integrating with its status row.

Tracked in `docs/issues.md` (not repeated here): the slice-3 polish
runner-verify, and argument-list recovery (`vi -r`).

---

## The core idea: one vi engine, two placements

The compose draft is a **kernel-owned block**: the per-principal `Status::Draft`
block (`BlockStore::edit_draft`), written through the `editInput`/`submitInput`
RPC methods. Both clients run compose on a local `EditorCore`, the same
engine the kernel's editor sessions run. Each `EditOp` it emits is one
`editInput` call, queued in typed order on the shared draft outbox
(`kaijutsu_client::draft_outbox`); the draft block on the change feed comes
back through `kaijutsu_client::draft_ledger`, which decides when a mirror may
replace the local buffer. See `docs/tui.md`, "Compose".

For the draft the engine runs in the client. The draft is the player's
alone: it never gets a VFS path, a `kj` verb, or an editor session, because
the safety assumptions around who submits a turn rest on it
(`docs/issues.md`, "The compose draft is the player's alone").

The vi editor runs the same engine kernel-side, pointed at a file/config
block. So the testability win isn't scaffolding we throw away — it is the
editor's real spine:

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
modalkit's `VimMachine` + `TerminalKey`. Compose in the app and the tui runs
it too, client-side, over the draft. `EditorCore` owns one editor's session-local state: the text buffer
(modalkit's rope), cursor + selection, mode (normal/insert/visual/replace +
operator-pending + command-line), and the rollback checkpoint.

The contract everything else is tested against:

- `apply_keys(&str) -> Vec<EditOp>` — feed a key sequence in modalkit/vim
  notation (`"idata<Esc>"`, `"2dd"`); returns the `(char_offset, insert,
  delete)` edit-ops the keystrokes produced. Named keys: `<Esc>`, `<CR>`,
  `<BS>`, `<Tab>`, `<Space>`, `<Del>` (or `<Delete>`), `<Left>`, `<Right>`,
  `<Up>`, `<Down>`, `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, and `<C-x>`
  chords, case-insensitive. A literal `<` is `<lt>` (`<lt><lt>` is `<<`). A bare
  `<` opens a token that runs to the next `>`. An unknown `<foo>` is skipped.
  An unterminated `<` runs to the end of the input, so the rest of the batch
  is dropped, including a closing `<Esc>`. `<PageUp>`/`<PageDown>` (and `<C-b>`/`<C-f>` in
  normal and visual mode, `<C-u>`/`<C-d>` for half a page) move the cursor 24
  lines (12 for half a page), multiplied by a count. A page key keeps the column,
  clamped to the last character, or to the end of the line in insert mode. It
  extends a visual selection. After an operator it cancels the operator, as in
  vim. In insert mode `<C-d>` and `<C-u>` are not scrolls. The core has no
  window, so `H`, `M` and `L` see an empty one.
- `apply_remote_text(new_text) -> bool` — reconcile the buffer to the block's
  *merged* truth and transform the local cursor against the single-region diff
  (edit-before shifts, edit-after leaves, straddle lands at the replacement
  end); identical text returns `false` (the self-write-echo skip).
- Intent drains: `take_close()` (`ZZ`/`ZQ`), `take_commands()` (`:` verbs),
  `take_io()` (`:r` fetches) — the editor surfaces *intent*; the kernel acts.
- State accessors: `text()`, `cursor()`, `mode()`, `command_line()`,
  `selection()`. Dirtiness is computed kernel-side against the checkpoint. `selection()` is `None` outside visual mode.
  Charwise (`v`) and linewise (`V`) give one span; blockwise (`<C-v>`) gives
  one span per row that reaches the block's left column. A selected line
  break is part of a span. A peer merge (`apply_remote_text`) moves the
  anchor by the same rule as the cursor, so the selection keeps covering
  the same text and visual mode stays on.
- Registers: `with_registers(text, registers)`, `take_registers()`,
  `give_registers(registers)` move vi's registers from one core to the
  next, and nothing else (see "Registers").

**modalkit `0.0.25` pulls `crossterm 0.29` into the kernel/server dependency
graph.** The key *types* don't need a live TTY, so this is sound — accepted
(Amy, 2026-06-22); not worth fighting modalkit's features to trim crossterm.

### 2. Kernel editor sessions — the tool-shaped surface

The kernel holds a registry of open editor sessions
(`crates/kaijutsu-kernel/src/editor.rs`). Each session is an argument list
of files ("The argument list"); each loaded file is one `EditorCore` bound
to a `(context_id, block_id)`. Multi-user: many sessions (each its own
cursor/mode) can target one shared block; the kernel sequences every
session's edits onto it, and siblings reconcile via the mechanism below.
The registry is kernel-wide behind a mutex (`SendSessions`; the `!Send`
`EditorCore` stays inside sync critical sections), exposed as
`Kernel::editor_open/keys/insert/state/save/quit`.

| Verb | Does |
|---|---|
| `editor_open(path)` | `resolve_editor_target(path)` → load block text into a fresh `EditorCore` → return a session handle + initial state. `editor_open_list(paths)` opens one session over an argument list, or joins the opener's session that holds one of the paths. `editor_open_signaled` also fires the `open_editor` peer invoke at the submitter principal's app windows (falling back to `APP_PEER_NICK`; a missing renderer is a `warn`, never fatal — the session is already open). |
| `editor_keys(session, keys)` | `EditorCore::apply_keys` → mirror the edit-ops onto the block (`block_store.edit_text`) → drain intents (`take_close`/`take_commands`/`take_io`) and act → return new state. A dirty file-backed edit marks the swap row; a `:w`/`:wq`/`:x`/`ZZ` inside the batch flushes to disk too (`docs/file-buffers.md`). **Async** since `:r` (and now the flush) — sync-lock, release, await, sync-lock again; `EditorCore` never crosses the await, only the fetched `String`/flush result does. |
| `editor_insert(session, text)` | A paste, not keystrokes: `EditorCore::insert_at_cursor` splices `text` at the live cursor and mirrors the edit onto the block, the same as a key batch's plain edit — no write intent, so nothing to flush. It never touches the vim state machine, so the mode is exactly what it was before the call: insert stays insert, normal stays normal. Refuses with the state unchanged while the `:` command line is open, reporting on `EditorState.message` — the same dialect-level channel a bad `:s` regex uses. Synchronous. |
| `editor_state(session)` | read text/cursor/mode/command-line/dirty/selection (what a renderer draws). |
| `editor_save(session)` | `ZZ` / `:w` — flush the document to its owner; advance the checkpoint. |
| `editor_quit(session)` | `ZQ` / `:q!` — diff-rollback to checkpoint (see Rollback; skipped when entangled with peer work), drop the session. |

**The wire surface mirrors the input-doc surface.** capnp: `EditorState`
struct (text, cursor, mode, dirty, `commandLine @5`, `message @6`,
`selection @7`, `path @8`, `argIndex @9`, `argCount @10`) +
`editorOpen/Keys/State/Save/Quit @74–78` + `subscribeEditor @79` +
`editorInsert @105` with `EditorEvents` callbacks. **The render channel is
push, not poll** (decided
2026-06-23): a remote write landing in an open block must reach every renderer
the instant it lands — collaborative editing is the point, and poll would lag
it. `EditorFlow::StateChanged/Closed` ride an `editor_flows` bus; the kernel's
`editor_keys/insert/save/quit` publish after the registry mutates.

Remote ops are **one kernel responsibility**, not scattered per-client: the
server's `spawn_editor_reconciler` (one per kernel, dedicated thread +
LocalSet) drains `block.text_ops`; `EditorSessions::reconcile_block` finds
sessions bound to `(ctx, block)`, skips self-writes (their buffer already
equals the block — the mirror is faithful), merges stale siblings via
`apply_remote_text`, and publishes the changed states. So a sibling session, an
MCP edit, or a streaming turn that writes a bound block pushes the merged state
to every open renderer.

### 3. App renderer — `Screen::Editor` + MSDF panel

The app is a **renderer + key forwarder** for the editor:

- `view::editor` (`EditorPlugin`): the `open_editor` peer signal (carrying the
  initial `EditorState` — no fetch, no race) lands as `EditorOpenRequested`;
  the landing handler stores `ActiveEditor` and drives `Screen::Editor`.
  `handle_editor_events` keeps `ActiveEditor.state` fresh off the
  `subscribeEditor` push (own keystrokes, peer merges) and pops to Conversation
  on close.
- The editor gets its **own dedicated 2D full-screen surface**, superseding
  the earlier time-well reuse: `EditorSurfaceRoot`/
  `EditorSurface` in `view/editor/render.rs` are a full-window dark "page" node
  with an MSDF text child on `BlockFxMaterial` — the same material the
  conversation/compose surfaces use, so the cursor/selection shader path is
  shared via `shaders::cursor_selection_uniforms`.
- Text is laid out with the mono font at the conversation's `cell_font_size`,
  wrapped at the page width. The page is three bands: the body, the mode line
  (path and `[+]` on the left; mode and `row,col` on the right, as in the
  tui), and the `:`-strip (command line, else the kernel's message, else a
  local notice such as a lost key batch). All of it goes into one glyph
  buffer. The viewport follows the cursor (`view/editor/viewport.rs`): the
  window moves only when the cursor leaves it, lands on a line top, and keeps
  the cursor row clear of the mode line and strip. Rows outside the body band
  are not drawn, and a `~` marks each empty row below the text.
- Cursor geometry is computed via parley (`Cursor::from_byte_index(..).geometry`)
  into `OverlayCursorGeometry`, pushed to the material by `sync_editor_cursor`.
  Selection rects (`editor_selection_rects`) ride `EditorSelectionGeometry`
  on the same surface and reach the material in the same system, clipped to
  the body band. A blockwise selection merges stacked rows of one column range
  into one rect, and the selection uniform holds 64 rects.
- `editor_dispatch_keys` (gated `in_state(Screen::Editor)`) drains
  `GrabbedKey`, translates to the kernel's key notation (`keys.rs`: arrows,
  `<Home>`, `<End>`, page keys, `<Del>`, `<lt>`; a navigation key held with
  Ctrl, Alt or Shift is refused), and queues *every* key on the
  `kaijutsu_client::editor_outbox` outbox. The outbox keeps one call in
  flight and sends keys that queue behind it as one `editor_keys` batch, in
  press order, never resending a batch. The push subscription returns the new
  state. A failed answer is never silent: `no such session` pops back to the
  conversation, a stopped writer is replaced by a fresh outbox, and any other
  failure shows in the strip.
- A paste is `editor_insert`, not keys: Ctrl+Shift+V reads CLIPBOARD and
  middle-click reads PRIMARY. Plain Ctrl+V stays vi's visual-block key.

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
   surface against a live kernel; assert the block, flush-to-owner, `ZQ`
   rollback, and concurrent second-session merge — including over real SSH +
   Cap'n Proto (`crates/kaijutsu-server/tests/editor_wire.rs`). No GPU.

The same surface a test drives is what a model plays.

---

## Decisions

1. **The in-app door is the `vi` kaish builtin; `kj vi` is the agent door.**
   `vi <path>` is a real kaish `Tool` (`runtime/vi_builtin.rs`, registered in
   `runtime/context_shell.rs`). Players reach it through the shell: the shell
   surface, and the TUI's and app's `:vi <args>`, which run the statement
   `vi <args>` as `:!vi <args>` does, so quotes and `$vars` follow shell rules.
   `kj vi` is the programmatic door for agents and scripts. It takes structured
   argv, so a path is one word with no quoting, and it is not an in-app entry.
   Both reach the same `Kernel::editor_open`. `kj vi` is a clap verb named
   `vi`.
   `edit` is kaish's hashline editor, not a vi front door, and the builtin is
   not registered under that name.
   The builtin takes one or more paths, an argument list ("The argument
   list"). kaish splits on whitespace, so `vi my notes.md` is two files.
   Quote a path with a space: `vi "my notes.md"`, in the shell and in the
   bars alike.
2. **Save model: the session buffer binds to the block.** Typing lands as
   range edits (`block_store.edit_text`), never a whole-file replace. The
   kernel sequences every write, so concurrent merge into a block is
   structurally impossible — edits never clobber each other; a sibling
   session's buffer reconciles against the block's latest text instead.
3. **Binding tightness: live keystroke sync.** `editor_keys` mirrors each edit
   to the block immediately; a sibling session's buffer resyncs via the
   reconciler when its edits land. Fully collaborative; the rollback story
   keeps a clean `ZQ` possible.
4. **Render path: Design A** (decided 2026-06-23). The app renders the editor
   from the kernel-served editor subscription and **never joins the editor's
   context into `DocumentCache`** — the app's cache only holds contexts it
   explicitly joins, block ops for an un-joined context are dropped
   (`view/sync.rs`), so no `DocKind` discriminator is needed on the wire.
   (Rejected Design B: joining the editor context into the cache.)
5. **Surface: dedicated full-screen `EditorSurface`.** Originally decided to reuse the time-well's
   MSDF panel substrate; that didn't survive contact with the editor's actual
   needs — full-screen layout, the conversation-matched font, and a real
   cursor quad, none of which the time-well's 3D card could give it — so the
   editor got its own `Screen::Editor` surface (`view/editor/render.rs`). The
   originally-rejected alternatives still hold: a flat 2D vello overlay
   (doesn't share the 3D scene) and editing the time-well `ReadingCard` in
   place (couples vi to the well).
6. **Mode lives kernel-side, period.** The app forwards every key, including
   `:`. `ZZ`/`ZQ` and command mode are recognized in `EditorCore`; an app-side
   input surface would race the mode push and split input ownership.
7. **Enter inserts a newline.** `EditorCore` runs plain `VimBindings`.
   Compose takes Enter before the engine sees it only in normal mode, where
   it submits the draft; in insert mode it is the engine's newline. The
   shell line (the app's shell surface, the tui's `:` bar) runs on Enter in
   any mode.
8. **No generic `edit_block` RPC.** The app sends *keys*; the kernel writes via
   `block_store.edit_text`. A generic block-edit RPC is off the editor's
   critical path — don't build it for vi.
9. **No capability gates `/config/rc`.** rc melted to host files
   (`docs/rc-on-disk.md`) and `rc-write` is dropped with it — a script is
   protected the way any file is, through the file tools and the editor, not
   a loadout check. The host's real `/etc` is a plain read-only host path,
   like any other path under the read-only `/` mount — kaijutsu no longer
   squats it. Open + save surface permission errors loudly — crash over
   corruption.

### Path resolution — one branch, the file cache

`resolve_editor_target(path, file_cache)` in
`crates/kaijutsu-kernel/src/editor.rs` resolves every path the same way:
`FileDocumentCache::try_get_or_load(path)` mints or loads the working-copy
file-doc that owns the path's text, and the editor binds to its
`(context_id, block_id)`. Every `/config` tree — `rc`, `kernel`, `client`,
`midi` — mounts as an ordinary host directory (`docs/config-namespace.md`),
the same as any other path, so there is no second branch to route a config
path down.

**There is no ownership-aware branch.** `ConfigDocFs`,
`EditorTarget::config_owned`, `VfsOps::owns_config_docs`, `config_doc.rs`,
and a four-argument `resolve_editor_target(path, blocks, file_cache,
mounts)` signature that once routed config paths separately are all gone.
What that branch guarded against — a config path running through
`get_or_load` and minting a *separate* `FileDocumentCache` copy shadowing a
kernel-owned document, the dual-ownership write-through bug class the
kernel-owned design existed to prevent (`docs/devlog.md`, "Config: the
kernel owned it, then gave it back") — cannot happen once every config tree
is an ordinary host file: there is exactly one owner, the file cache, for
any path. A missing file still **fails loud** (no empty editor) —
`try_get_or_load` surfaces the open error rather than serving a phantom
block.

---

## Rollback / checkpoint

The checkpoint is a buffer's last-saved text (`Buffer.saved_content`), set
when the file loads and refreshed by `editor_save`. Each file of an
argument list has its own.

- **`editor_save`/`ZZ`** sets the checkpoint to the current buffer text and
  clears the peer-wrote flag (see the entanglement guard below) — the
  checkpoint now contains every merged peer edit.
- **`editor_quit`/`ZQ`** compares the checkpoint against the block's current
  text. If they differ, it writes the checkpoint back with one
  `block_store.edit_text` call — an inverse forward edit, not history
  erasure: concurrent merge into a block is structurally impossible, and
  there is no oplog to erase from.

```
editor_save / ZZ  →  checkpoint = current buffer text     // session holds saved_content
…editor_keys…     →  edits land on the block, kernel-sequenced
editor_quit / ZQ  →  restore = checkpoint (+ terminator)  // written back if it differs from current
```

Cheap, restart-safe, collaboration-safe (peers see the rollback edit land).

**Entanglement guard — detach, don't retract (Amy, 2026-07-07).** The rollback
runs only when the session was provably *alone* with the block since its
checkpoint. `EditorSessions::quit` skips it — dropping the session, leaving the
block's merged truth — when a sibling session is still bound to the block **or**
a peer's write merged into this session since the checkpoint (a sticky
`peer_wrote` flag: set by `reconcile_block` on any non-self merge, reset when
`save` advances the checkpoint, which folds the peer work in). So in a shared
session `ZQ`/`:q!` mean "quit *me* out of the doc, leave the others playing" —
never "undo everyone back to my checkpoint". Your own unsaved keystrokes stay in
the shared score; they were already part of the ensemble the moment they
mirrored. Residual race: a peer who writes and closes within the reconciler's
bus latency (flag not yet set, no sibling left to see) can still be clobbered.
`:w!`'s `force` is now spent on the disk-level W12 guard
(`docs/file-buffers.md`), which checks the VFS generation, not this
block-level peer-write race — closing this residual race still needs its own
detection.

Each buffer also keeps its block's trailing terminator aside
(`Buffer.terminator`), so a newline-terminated block opens clean and `ZQ`
re-applies the terminator.

---

## Registers

Registers belong to the player, not to one editor. In the kernel, every vi
session a player types in shares that player's registers, across sessions
and contexts. In a client, compose keeps one set for the client, across
drafts. `dd` in one, then `p` in another, puts the line below the cursor,
linewise, as one vim would. Amy, 2026-11-10: *"it doesn't have to be one
global, but like, I want to be able to dd while working in one context's
editor, then go to another and 'p' when I type in the wrong spot."*

- **Kernel.** `EditorSessions` keeps one `kaijutsu_editor::Registers` per
  principal that sends keys: the invoking performer for `kj vi keys`, the
  connection's principal for the `editorKeys` RPC — the same identity each
  edit is attributed to. They are lent to the session's `EditorCore` for one
  key batch and taken back after it, so two open sessions of one player see
  each other's yanks at once, and a model's session never reads or
  overwrites a person's registers. They are held in memory: a kernel restart
  empties them.
- **Compose.** The tui's `Compose::load_draft` moves the registers into each
  new core (a seat switch, a submit, a clear). The app's `ComposeState` holds
  them and lends them to the chat draft or the shell line for each key, so
  its two surfaces share them. See `docs/tui.md`, "Compose", and
  `docs/input.md`, "Compose keys".
- **What moves.** `Registers` is modalkit's register store: the unnamed
  register, `"0`, the delete ring `"1`–`"9`, `"-`, `"a`–`"z`, recorded
  macros, and the last `:` line and search pattern. Nothing else in
  modalkit's store moves: marks are keyed by a buffer id every
  `EditorCore` shares, so a moved store would carry one text's marks into
  another. The cursor, undo history, mode, and an open `:` line stay with
  their core. The API is `EditorCore::with_registers`, `take_registers`
  and `give_registers`.
- **Yanks from outside an editor.** `EditorCore::yank` puts text into the
  unnamed register and `"0` with a shape, as a `y` would. The tui's copy
  mode yanks whole rows this way and the app's diff viewer yanks a patch
  (whole lines) or plain text (characters), so `p` in compose puts either.
  `EditorCore::put` is `p` for a key that is not vi's own: the tui's
  `Ctrl+A ]`.
- **The system clipboard.** A compose `y` is also written to the system
  clipboard: OSC 52 in the tui, arboard in the app. A client learns that a
  key yanked by comparing `EditorCore::last_yank` (`"0`) across the key, so
  a delete never goes out, and neither does a second yank of the same
  text. Nothing reads the clipboard into a register: `"+` and `"*` read
  empty because modalkit's `clipboard` feature is off, and paste from the
  clipboard stays the terminal's bracketed paste and the app's `Ctrl+V`.
- **Not shared yet:** compose with the kernel's vi sessions, and the tui
  with the app. A yank in a kernel vi session does not reach the system
  clipboard. See `docs/issues.md`, "vi registers: what
  sharing leaves open".

---

### A kernel-owned register store (design, not built)

Amy, 2026-11-11, asked for this design after choosing that `p` always puts
the vi register and never the system clipboard. The goal: `dd` in the tui
and `p` in the app, and `dd` on the compose line and `p` in a `vi` session.

Amy's working pattern, same day: *"I often `vi *.rs` and :n and :N through
the files, and I can yank and put things across them pretty quickly."*
That pattern stays inside the kernel's vi sessions, whose registers one
person's sessions already share, and the argument list serves it ("The
argument list" below). This store serves a yank that crosses from one
client, or from the compose line, to another editor.

Three sets of registers exist per person today: the tui's compose, the
app's compose, and the kernel's vi sessions. Compose runs `EditorCore` in
the client, so its registers never reach the kernel.

**Constraints.**

- `p` never waits on the wire. A client keeps its own `Registers` and puts
  from them; the kernel's copy is what the clients converge on.
- The store is per character, keyed as `EditorSessions.registers` is
  already: the principal the edit is attributed to. A model's yank never
  lands in a person's registers.
- The kernel sequences writes. Two clients yanking at once resolve to the
  later accepted write per register; nothing merges register text.
- Only text registers move: the unnamed register, `"0`, `"1`–`"9`, `"-`
  and `"a`–`"z`, each as text and a shape (charwise, linewise, blockwise).
  Macros, the last `:` line and the search pattern stay with the client.

**Two ways to carry it.**

1. *Wire methods.* `registersPut(cells, since)` sends the cells a key
   changed; `subscribeRegisters` pushes accepted cells to the person's
   other connections with a kernel sequence number. `EditorSessions` lends
   from the same store, so a vi session needs no new code path. A client
   applies a pushed cell between keys, never inside a half-typed command,
   and ignores the echo of its own write by sequence number. Cost: two
   wire methods and a callback, and `EditorCore` must report which
   registers a key changed (today only `last_yank` is readable).
2. *VFS files.* Each register is a file under a per-character directory,
   with the shape in a sidecar or a one-line header. Clients write through
   the file RPCs and learn of changes through a watch. Scripts and models
   could read a register with `cat` and fill one with a redirect, which is
   new capability, not only plumbing. Cost: a watch path clients do not
   have today, a file format for the shape, and a yank on every `x` and
   `dw` becoming a file write.

**Recommendation: the wire methods, held in memory.** Compose input is
already an interaction-rate RPC path (`AGENTS.md`, "State and
interfaces"), and a register changes on most normal-mode edits. The VFS
form earns its cost only if scripts should reach registers, and the draft
is the player's alone today: a delete from a draft would become readable
at a path. If that reach is wanted later, a read-only `kj` verb over the
same store is a smaller step than files.

**Smallest useful slice.** Share the unnamed register and `"0` only. That
covers `dd` here and `p` there, and needs one cell per message. The delete
ring and the named registers follow once the push path has been played
with.

**Amy's answers, 2026-11-11.**

- *Durability:* "the state should survive in some recoverable form even
  if the vi session doesn't, basic power outage / kill -9 protection for
  ongoing work." The store is a table, written through on each accepted
  write.
- *Reconnect:* "on reconnect nobody gets it, they can recover it." No
  side wins by itself. A reconnecting client keeps its local registers and
  the kernel keeps its copy; a person asks for the kernel's copy when they
  want it. The verb for that is not chosen.
- *Models:* "might be cool I suppose I was thinking about some short term
  memory options." A model's character may get a store. As short-term
  memory it is a separate idea from yank and put and wants its own design.

**Still open.**

- The recover verb: a `:` command, a `kj` verb, or both, and whether it
  replaces the local registers or fills named ones.
- The system clipboard stays a separate, outbound path: a yank is written
  to it, and nothing here reads it.

### The argument list

Amy, 2026-11-11: *"I often `vi *.rs` and :n and :N through the files, and
I can yank and put things across them pretty quickly."* She chose this
ahead of the register store. One person's vi sessions already share
registers, so the list is what this adds.

`vi a.rs b.rs c.rs` opens one session over the three files. kaish expands
`vi *.rs` before the builtin sees it, and a path with a space is one word
only when quoted. `:n` moves to `b.rs`, `:N` comes back, and `dd` in one
file then `p` in another puts the line, because the registers belong to
the player, not the buffer.

**The session holds the list.** An `EditorSession` is an ordered list of
files and a current index (`crates/kaijutsu-kernel/src/editor.rs`). Each
loaded file is a buffer: an `EditorCore`, its document binding, its
checkpoint and terminator, and its `peer_wrote` flag. The session id does
not change as the index moves, so the renderer's screen and subscription
stay. A file is loaded the first time it is visited: `begin_move` reports
the path, the kernel resolves it outside the sessions lock (the async step
`:r` uses), and `finish_load` loads it. A visited buffer keeps its core, so
its undo history, marks and cursor are there on return, as vim's hidden
buffers are. Each loaded file holds a file-cache pin until the session
closes. The other design, one session per file with the client switching
between them, would put the list, the dirty refusal and the `:` commands in
every client; this project keeps derived logic in the kernel.

**The dialect.** `kaijutsu-editor` parses the verbs into
`CommandRequest::Go` and `CommandRequest::Args`; the kernel moves.

| Line | Does |
|---|---|
| `:n`, `:next` | Next file. `:2n` moves two. |
| `:N`, `:Next`, `:prev`, `:previous` | Previous file. `:2N` moves two. |
| `:first`, `:fir`, `:rew`, `:rewind` | First file. |
| `:last`, `:la` | Last file. |
| `:args`, `:ar` | Shows the list on `EditorState.message`, the current file in brackets: `a.rs [b.rs] c.rs`. |
| `:wn`, `:wnext`, `:wN`, `:wNext`, `:wp`, `:wprevious` | Write, then move. A count works: `:2wn`. A failed write does not move. |
| `:e path`, `:edit path` | Moves to `path`, appending it to the list when it is not in it (Amy: "add to the list"). A path already in the list adds nothing. |

Every verb but `:args` takes a `!` (`:n!`, `:e! path`); `:args!` is
refused. A count of 0 is refused ("Positive
count required"), and a count on a verb that takes none is refused ("No
range allowed"). vim keeps `:e` outside the argument list; one list is
simpler here. Every `:` line ends its key batch, as do `ZZ`, `ZQ` and a
failed search: the kernel acts on the line, then runs the keys after it as
the next batch (`EditorCore::take_rest`). So `x:w<CR>:n<CR>dd` writes this
file, moves, and deletes a line of the next file; no line's intent is
overwritten by a later one in the same batch.

**Leaving a dirty buffer.** As vim, `:n`, `:N`, `:first`, `:last` and `:e`
refuse with "No write since last change (add ! to override)" when the
current buffer differs from its checkpoint. The `!` form leaves it dirty
and hidden (Amy, 2026-11-11: "keep dirty"), where vim would discard: the
text is already on the document and durable, and `ZQ`'s rollback stays the
one way to discard. Moving to the current file is a no-op and refuses
nothing. Past either end of the list: "Cannot go beyond last file" or
"Cannot go before first file". `:wn` on the last file writes, then refuses.
A file that cannot be opened reports on the status line and nothing moves;
`:e` of such a path adds nothing to the list.

**Closing.** `:q` with a dirty hidden buffer refuses and names the first
one in list order: `No write since last change for buffer "a.rs"`.
`:q!` and `ZQ` apply the rollback rule, with its entanglement guard, to
every loaded file, then close. `ZZ`, `:wq` and `:x` save the current file
and close only when no other file is dirty; otherwise they save and refuse
to close with the same message.

**One session per file, per person.** `vi` on a path that is already in one
of the opener principal's sessions joins that session on that file (Amy:
"join existing should be the default"). Any other paths in the same command
are appended to that session's list, and the open signal names the joined
session. Joining moves off a dirty buffer as `:n!` does, keeping it.
Another person's or a model's session over the same file is never joined,
and neither is a headless one (no opener): each keeps its own, and both see
each edit as they do today. `:e` and `:n` inside a session do not join: a
file another of the person's sessions holds opens in this one too, with
the "already open in 1 other session" notice.

**Peer edits reach hidden buffers.** `reconcile_block` merges a peer's
write into every loaded buffer bound to the block, current or hidden, and
sets that buffer's `peer_wrote`. A renderer is pushed a state only when the
current buffer moved.

**The wire.** `EditorState` carries the current `path`, `argIndex` (0-based)
and `argCount` (`kaijutsu.capnp`), and `EditorState::to_json` carries
`path`, `arg_index` and `arg_count`. Both renderers draw the path and, when
the list holds more than one file, `(2 of 5)` on the mode line
(`kaijutsu_types::editor::arg_position_label`). No new method: `:n` is keys.

**`kj vi` takes the list too.** `kj vi open a.rs b.rs` opens one session
over both, with the same join rule for the caller's principal. `kj vi list`
shows each session's current file, its place in the list, and every file
with unsaved changes (`dirty_files`; `dirty` is true when any file is).
`kj vi quit` names the files it closed and the ones it rolled back.

**Not built.**

- Recovery; see the design below.
- vim's "N more files to edit" refusal of `:q` while unvisited files
  remain. `:q` here closes once nothing loaded is dirty.

### Recovering an argument list (design, not built)

Amy, 2026-11-11: *"the state should survive in some recoverable form even
if the vi session doesn't, basic power outage / kill -9 protection for
ongoing work. on reconnect nobody gets it, they can recover it."*

**What survives a restart today.** The text does. A file buffer with
unsaved changes has a `dirty_file_buffers` row and comes back as a
recovered swap (`docs/file-buffers.md`, rule 2); a block's text is on its
document. The session does not: its list, its index, each buffer's cursor
and each buffer's checkpoint are memory. After a restart `vi a.rs` on a
swapped path opens the swap's text as a clean buffer, and `:w` refuses
until `kj swap ack a.rs`.

**The record.** Two tables in `kernel.db`, one row per session and one row
per file of its list:

| Table | Columns |
|---|---|
| `editor_recovery_sessions` | `recovery_id` (UUIDv7), the opener's principal and context, `current_index`, `opened_at`, `updated_at` |
| `editor_recovery_files` | `recovery_id`, `position`, `path`, `visited`, `cursor_line`, `cursor_col` |

The kernel writes them when the session opens, when the list grows (`:e`, a
join that appends), when the index moves, and on `:w`. A move and a `:w`
record the cursor of the buffer they leave or save; keys between them do
not write, so a crash loses the cursor's travel since the last one and
nothing else. A close by any route (`:q`, `ZZ`, `ZQ`, `kj vi quit`) deletes
the session's rows. A headless session (no opener) is not recorded: there
is nobody to recover it. A write failure is reported on the status line and
logged; the session keeps working, because the text's durability does not
depend on this record.

**After a restart.** Rows that remain belong to sessions that did not
close. The kernel opens nothing and tells no client. `vi -r` lists the
caller's recoverable lists, newest first: an index for picking, the age,
the files with the current one in brackets, and a `+` on each file that has
a swap. `vi -r 2` reopens the second: one session over the same list at the
same index, each visited buffer's cursor restored when it is next loaded.
`kj vi recover` and `kj vi recover <n>` do the same for a model or a
script, and `kj vi recover --discard <n>` deletes a record without touching
any swap. Reopening deletes the old record; the new session writes its own.
A file that no longer resolves is dropped from the list with a message that
names it, and a list with no file left is refused.

**A recovered file buffer is dirty.** This is vim's `-r`: the buffer holds
the swap's text and the file on disk is unchanged, so the checkpoint is the
disk text, `:w` writes the buffer, and `ZQ` rolls back to disk. `vi -r`
acknowledges the swap of each buffer it loads, because the person asked for
the recovery by name, and the buffer stays dirty with its
`dirty_file_buffers` row. The disk-moved check still applies to `:w`. A
plain `vi a.rs` on a swapped path is unchanged, and it gains one line when
a recovery record holds the path: `a.rs is in a recoverable list (vi -r)`.

**What recovery does not restore.**

- Undo history and marks. They live in the `EditorCore`.
- The checkpoint of a block that is not a file. Its text is the document's
  text, the recovered buffer opens clean, and `ZQ` has nothing to roll
  back to.
- Registers. They wait for the register store above.

**Open.**

- Whether `vi -r` acknowledging a swap is right, or whether it should keep
  refusing `:w` until `kj swap ack`.
- Whether a person sees only their own records. One trust boundary says a
  `--all` flag would be honest; the default is the caller's own.
- How long an unrecovered record stays. Nothing deletes one today except
  recovery and `--discard`.

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
  on the status line (vim's E37 "No write since last change"); `:q!` discards
  and rolls the block back (when provably alone — see Rollback's entanglement
  guard); `:wq`/`:x` save-then-close; `:w` saves and stays. For a file-backed
  session "saves" means both the kernel checkpoint *and* a
  `FileDocumentCache::flush_one_guarded` to disk (`docs/file-buffers.md`); a
  config/rc session has no host file, so only the checkpoint applies. The
  guard is W12: a plain `:w` refuses when disk moved past the buffer's load
  generation, reporting on the status line and leaving the session open and
  dirty; `:w!` sets `force` and overrides it.
- **Argument list:** `:n :N :first :last :args :wn :wN :e path`, with
  counts (`:2n`) and `!` forms. `:q` also refuses while another file of the
  list is dirty, and `ZZ`/`:wq`/`:x` then save without closing. See "The
  argument list" above for the table and the rules.
- **Substitute:** `:s/old/new/`, `:%s/…/…/`, `:N,Ms/…/…/`. Hand-rolled
  (modalkit's `vim_cmd_substitute` is an explicit stub). **The dialect is Rust
  regex + Rust replacement syntax** (`$1` capture refs) — a deliberate choice
  over chasing vim's BRE flavor; the `:` line is its own dialect, not
  vim-exact. Flags: `g` (all-per-line), `i` (case-insensitive); unknown flag
  fails loud. Arbitrary delimiter (`:s#a#b#`). `:s` is an **edit**, not a
  kernel `CommandRequest`: it mutates the `EditorCore` buffer and rides the
  existing diff→`EditOp`→block-mirror path.
- **Read:** `:r <file>` reads through `FileDocumentCache::try_read_content`.
  `:r !cmd` runs on the kernel runtime with the requester, performer, reviewer,
  session, and context captured when the editor opened. Later shell navigation
  does not retarget the read. The player who submits the keys owns the resulting
  text insertion, even when another player opened the editor. Both splice
  **at the cursor**, rather than below its line. Accepted spellings also include
  `:read <file>`, `:read !cmd`, and the adjacent-bang `:r!cmd`.
  A missing file, denied or failed command, invalid UTF-8, or truncated output
  reports on the `:` status line and leaves the buffer unchanged. Shell output
  uses the internal 4 MiB limit; redirect larger output to a file, then use
  `:r <file>`. Caller cancellation and kernel shutdown stop the shell read.
  Local cwd/export changes in that shell are not written back. Read-only model
  shells refuse editor opens and input; use `shell_write` to drive an editor.
- **No `:!` in the app, deliberately.** The app's `:` bar runs one thing,
  `:vi <args>`, as the shell statement `vi <args>`; every other `:` line is
  refused. The TUI's bar also takes `:!<statement>` (`docs/tui.md`, "The `:`
  line"). Nested editor sessions, a return stack, and ephemeral-block lifecycle
  are still out of scope. The **shell is already a surface a keystroke away**: **Ctrl+Z** (a local app intercept —
  don't forward the key; leave to the shell but keep `ActiveEditor`; the
  kernel session is untouched, and local means it doubles as the hung-kernel
  escape hatch) and **`fg`** (a kaish builtin: `Kernel::resume_editor` finds
  the caller's session via the captured `EditorOpener` and re-fires the
  existing `open_editor` signal). vim only grew `:!` because it had nowhere
  else to go; kaijutsu does. (`:%!filter` is also out of scope; the shell
  filters.)
- **Search and dot-repeat.** `/pat<CR>` moves forward to the next match and
  `?pat<CR>` moves backward, wrapping past either end; `n` repeats in the
  same direction and `N` in the opposite one, and both take a count. An empty
  pattern (`/<CR>`) reuses the last one. The pattern is Rust regex, like
  `:s`. A search moves the cursor and never edits the document, unless an
  operator is pending (`d/pat<CR>` deletes up to the match). No match leaves
  the cursor and reports `Pattern not found: <pat>`; `n` with no earlier
  pattern reports `No previous regular expression`. modalkit owns the
  pieces: the bar submit stores the pattern in its last-search register and
  runs the search motion the bar was opened with. `.` replays modalkit's
  tracked edit sequence (`dw`, `x`, `ciwfoo<Esc>`, `A!<Esc>`, ...) through the
  same action loop as typed keys, so it emits ordinary `EditOp`s. A count
  replaces the change's count and keeps replacing it for later `.`s (`x`
  then `3.` is `3x`). `EditorCore` keeps its own copy of the last change,
  taken before the bar opens and on each `.`, because leaving the bar makes
  modalkit record a cursor reset as a new edit sequence; without the copy,
  `.` after `/pat` or `:w` would do nothing.
- **Errors report on the `:` line.** Every **dialect-level** failure — an
  unknown command (E492), a bad `:s` regex, a dirty-buffer `:q` refusal (E37),
  a failed `:r` (missing file, denied/failed command, no opener), a failed or
  invalid search — sets the
  transient `EditorState.message` and keeps the session open; it clears on the
  next keystroke batch. Hard `editor_keys` errors are **reserved for
  session/infrastructure failures** (no such session, a block-mirror failure) —
  the app's session-lost detection keys on exactly that distinction, and a
  dialect failure surfaced as an RPC error never reached the GUI at all (the
  app only logs it) while the un-pushed state left the `:`-strip showing the
  stale submitted line (unified 2026-07-07).

Related separate thread (not part of this): the Ctrl+Z shell may become a
**shadow context** whose blocks are excluded from the conversation until
drifted — its own design pass.

---

## Key file anchors

Paths are under `crates/`. Line numbers drift — grep the symbol.

| Concern | Location |
|---|---|
| Editor sessions + resolver + state shape | `kaijutsu-kernel/src/editor.rs` (`resolve_editor_target`, `EditorSessions`, `EditorState::to_json`, `APP_PEER_NICK`) |
| Vim engine (pure) | `kaijutsu-editor/src/lib.rs` (`EditorCore`, `EditOp`, `CommandRequest`, `EditorIo`, `selection`) |
| Selection value | `kaijutsu-types/src/editor.rs` (`EditorSelection`, `SelectionShape`) |
| `vi` builtin (front door) | `kaijutsu-kernel/src/runtime/vi_builtin.rs`; registered in `runtime/context_shell.rs` |
| `kj vi` (clap verb `vi`) | `kaijutsu-kernel/src/kj/vi.rs` |
| Block text edit | `kaijutsu-kernel/src/block_store.rs` (`edit_text`/`edit_text_as`) |
| Peer signal | `kaijutsu-kernel/src/kernel.rs` (`invoke_peer`, `signal_open_editor`, `editor_reconcile_block`) |
| Remote-merge reconciler | `kaijutsu-server/src/rpc.rs` (`spawn_editor_reconciler`) |
| Wire schema | `kaijutsu.capnp` (`EditorState`, `EditorSelection`, `editorOpen @74` … `subscribeEditor @79`, `editorInsert @105`) |
| Wire e2e | `kaijutsu-server/tests/editor_wire.rs` |
| App renderer | `kaijutsu-app/src/view/editor/` (`mod.rs`, `render.rs`, `keys.rs`); screen FSM `ui/screen.rs` |
| Editor surface renderer | `kaijutsu-app/src/view/editor/render.rs` (`EditorSurface`, `build_editor_surface`, `editor_selection_rects`, `sync_editor_cursor`) |
| Compose draft block | `kaijutsu-kernel/src/block_store.rs` (`get_or_create_draft`, `edit_draft`) |
| Compose (app) | `kaijutsu-app/src/input/compose/` (`mod.rs` `ComposeState`, `systems.rs`, `outbox.rs`) |
| Compose (tui) | `kaijutsu-tui/src/compose.rs` (`Compose`) |
| Draft outbox and ledger | `kaijutsu-client/src/draft_outbox.rs`, `draft_ledger.rs` |
| File doc cache | `kaijutsu-kernel/src/file_tools/cache.rs` (`get_or_load`, `try_get_or_load`) |

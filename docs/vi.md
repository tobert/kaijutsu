# In-App vi Editor

`vi /config/rc/thing/whatever.kai` opens a real vi-like editor on that file's block. The editor is a **kernel-owned session** driven through a small tool-shaped surface; the Bevy app is one *renderer* of it, a model is another *player* of it, and a headless test is a third *driver* of it. Same surface for all three.

Every input mutation carries the current player's identity. `kj vi` uses its invoking performer; RPC keys, paste, and rollback use the authenticated connection's principal. Opening a shared editor does not make its opener the author of later players' edits. The original block author stays unchanged. This is live mutation attribution; durable per-edit audit remains separate.

## Underneath: file buffers

The editor sits on the file-buffer layer — how kaijutsu holds file content, and where `:w` actually lands. **`docs/file-buffers.md` is canonical for that**, and it carries decisions that change this document: `write` and `grep` are removed, `edit` is hashline-only, and the `:w!` changed-under-us guard (the W12 guard) is enforced.

## The core idea: one vi engine, two placements

The compose draft is a **kernel-owned block**: the per-principal `Status::Draft` block (`BlockStore::edit_draft`), written through the `editInput`/`submitInput` RPC methods. Both clients run compose on a local `EditorCore`, the same engine the kernel's editor sessions run. Each `EditOp` it emits is one `editInput` call, queued in typed order on the shared draft outbox (`kaijutsu_client::draft_outbox`); the draft block on the change feed comes back through `kaijutsu_client::draft_ledger`, which decides when a mirror may replace the local buffer. See `docs/tui.md`, "Compose".

For the draft the engine runs in the client. The draft is the player's alone: it never gets a VFS path, a `kj` verb, or an editor session.

The vi editor runs the same engine kernel-side, pointed at a file/config block:
- **Thin client, smart kernel.** Editing state and semantics live in the kernel; the 3D viewer is a *view onto* a kernel session, not the owner of it.
- **Playable by anyone.** Many hands on one keyboard: app, model, test all drive the same `editor_*` surface.
- **Testable before pixels.** The whole increment is exercised headless because the increment was never about rendering.

## Three pieces

### 1. `kaijutsu-editor` crate — `EditorCore` (pure)

The vim *engine* as pure logic — **no Bevy, no kernel, no RPC**. Built on modalkit's `VimMachine` + `TerminalKey`. Compose in the app and the tui runs it too, client-side, over the draft. `EditorCore` owns one editor's session-local state: the text buffer (modalkit's rope), cursor + selection, mode (normal/insert/visual/replace + operator-pending + command-line), and the rollback checkpoint.

Contract:
- `apply_keys(&str) -> Vec<EditOp>` — feed a key sequence in modalkit/vim notation (`"idata<Esc>"`, `"2dd"`); returns the `(char_offset, insert, delete)` edit-ops. Named keys: `<Esc>`, `<CR>`, `<BS>`, `<Tab>`, `<Space>`, `<Del>` (or `<Delete>`), `<Left>`, `<Right>`, `<Up>`, `<Down>`, `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, and `<C-x>` chords, case-insensitive. Literal `<` is `<lt>` (`<lt><lt>` is `<<`).
- `apply_remote_text(new_text) -> bool` — reconcile the buffer to the block's *merged* truth and transform the local cursor against the single-region diff; identical text returns `false`.
- Intent drains: `take_close()` (`ZZ`/`ZQ`), `take_commands()` (`:` verbs), `take_io()` (`:r` fetches).
- State accessors: `text()`, `cursor()`, `mode()`, `command_line()`, `selection()`. Dirtiness is computed kernel-side against the checkpoint.
- Registers: `with_registers(text, registers)`, `take_registers()`, `give_registers(registers)` move vi's registers from one core to another.

### 2. Kernel editor sessions — the tool-shaped surface

The kernel holds a registry of open editor sessions (`crates/kaijutsu-kernel/src/editor.rs`). Each session is an argument list of files ("The argument list"); each loaded file is one `EditorCore` bound to a `(context_id, block_id)`. Multi-user: many sessions can target one shared block; the kernel sequences every session's edits onto it, and siblings reconcile via the mechanism below.

| Verb | Does |
|---|---|
| `editor_open(path)` | Resolve target, load block text into a fresh `EditorCore`, return session handle + initial state. `editor_open_list` opens an argument list or joins the opener's session holding one of the paths. |
| `editor_keys(session, keys)` | `EditorCore::apply_keys`, mirror edit-ops onto block (`block_store.edit_text`), drain intents and act, return new state and yank. A dirty file-backed edit marks the swap row; `:w`/`:wq`/`:x`/`ZZ` flushes to disk. Async since `:r` and flush. |
| `editor_insert(session, text)` | Paste at cursor, mirror edit onto block. Refuses while `:` command line is open. Synchronous. |
| `editor_state(session)` | Read text, cursor, mode, command-line, dirty, selection. |
| `editor_save(session)` | `ZZ` / `:w` — flush document to owner, advance checkpoint. |
| `editor_quit(session)` | `ZQ` / `:q!` — diff-rollback to checkpoint, drop session. Closes through `Kernel::editor_close`. |

The wire surface mirrors the input-doc surface. capnp: `EditorState` struct + `editorOpen/Keys/State/Save/Quit` + `subscribeEditor` + `editorInsert`. Render channel is **push, not poll**. Remote ops are handled by `spawn_editor_reconciler` (one per kernel), which reconciles sibling sessions and pushes merged states to open renderers.

### 3. App renderer — `Screen::Editor` + MSDF panel

The app is a **renderer + key forwarder** for the editor, providing a dedicated full-screen surface (`EditorSurfaceRoot` / `EditorSurface`) with MSDF text child on `BlockFxMaterial`. Text layout, parley-computed cursor geometry (`OverlayCursorGeometry`), selection rects (`EditorSelectionGeometry`), and outbox-mediated key dispatch (`editor_dispatch_keys`) drive the editor without detecting mode app-side.

## Rollback / checkpoint

The checkpoint is a buffer's last-saved text (`Buffer.saved_content`), set when the file loads and refreshed by `editor_save`.
- **`editor_save`/`ZZ`** sets checkpoint to current buffer text and clears the peer-wrote flag.
- **`editor_quit`/`ZQ`** compares checkpoint against block's current text. If they differ, writes checkpoint back with one `block_store.edit_text` call.

**Entanglement guard — detach, don't retract.** The rollback runs only when the session was provably *alone* with the block since its checkpoint. `EditorSessions::quit` skips rollback — dropping the session, leaving the block's merged truth — when a sibling session is still bound or a peer's write merged into this session since checkpoint (sticky `peer_wrote` flag).

## Registers

Registers belong to the player, not to one editor. Every vi session a player types in, and the compose line of every client the player connects, share that player's text registers through the kernel's register store.

- **Kernel.** `EditorSessions` keeps one `kaijutsu_editor::Registers` per principal that sends keys, lent to `EditorCore` for one key batch and taken back after.
- **Compose.** Client compose states hold and lend registers.
- **What moves.** Unnamed register, `"0`, delete ring `"1`–`"9`, `"-`, `"a`–`"z`, recorded macros, last `:` line, and search pattern. Marks and undo history stay with their core.
- **A kernel-owned register store.** Registers are written through to the `editor_registers` table and loaded on first use after a start. Client changes are pushed via `registersPut` writes, replayed through modalkit's `RegisterStore::put`.

## Command mode — the editor's `:` dialect

Command mode extends the key-forwarder seam. `:` and command mode live in `EditorCore`/modalkit; the app forwards every key. Each submitted line becomes a `CommandRequest` intent drained via `take_commands()`.

- **Core verbs:** `:w :q :wq :q! :x :w!`. `:q` on a dirty buffer refuses; `:q!` discards and rolls back; `:wq`/`:x` save and close; `:w` saves and stays. W12 guard enforces disk generation checks on `:w`.
- **Argument list:** `:n :N :first :last :args :wn :wN :e path`, with counts and `!` forms.
- **Substitute:** `:s/old/new/`, `:%s/…/…/`, `:N,Ms/…/…/` using Rust regex + replacement syntax.
- **Read:** `:r <file>` and `:r !cmd` run on the kernel runtime with captured identity. Inserts at cursor.
- **Search and dot-repeat:** `/pat<CR>`, `?pat<CR>`, `n`, `N`, and `.` dot-repeat replay through ordinary `EditOp`s.
- **Errors:** Dialect-level failures set transient `EditorState.message` on the `:` line; hard errors are reserved for session/infrastructure failures.

## A kernel-owned register store

Registers persist across sessions and restarts via the `editor_registers` table. Changes (`RegisterWrite`) are published via the editor flow bus and applied by clients between key batches. `:regpull` replaces client text registers with the kernel's copy.

## The argument list

`vi a.rs b.rs c.rs` opens one session over multiple files. `:n`, `:N`, `:first`, `:last`, `:args`, `:wn`, and `:e path` navigate the argument list. The session holds the list and current index; each loaded file keeps its `EditorCore`, undo history, and marks. Dirty buffer checks refuse movement unless forced with `!`.

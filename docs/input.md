# Input routing, vi editing, and file buffers

## App input routing and focus rules

App input flows through a central action table that maps key combinations to `ActionFired` events handled by specific input contexts (`InputContext`). 
- **Vi editor:** Only the vi editor grabs raw keyboard input directly. Vi owns `Esc` where live; elsewhere, an `Esc` triggers a `PopLevel` action.
- **Muscle memory:** Standard screen/tmux `Ctrl+A` bindings and standard vi keybindings are preserved.

## The vi editor and registers

`vi` opens a real vi-like editor on a file or configuration block. The editor is a kernel-owned session driven through a tool-shaped surface; the Bevy app, a model, and headless tests all act as drivers of the same `EditorCore` engine (`kaijutsu-editor` crate).

- **Editor sessions and rollback:** Kernel sessions hold argument lists and manage `EditorCore` instances. Rollback and checkpoints use `Buffer.saved_content`. The entanglement guard (`peer_wrote`) ensures cleanup drops sessions without overwriting concurrent sibling writes.
- **Shared register store (`editor_registers`):** Text registers (unnamed, numbered, named `"a`–`"z`, macros, last `:` line, search pattern) are shared across sessions and client compose buffers via a kernel-owned register store.
- **Command dialect (`:`):** Core verbs (`:w`, `:q`, `:wq`, `:q!`, `:x`, `:w!`), argument lists (`:n`, `:N`, `:first`, `:last`, `:args`, `:wn`), substitute (`:s/old/new/`, `:%s/…/…/`), read (`:r <file>`, `:r !cmd`), and search/dot-repeat (`/pat`, `.`) are fully supported.
- **Argument list recovery:** `:n`, `:N`, `:args`, and `:e path` navigate multi-file sessions. Swap recovery (`vi -r` / `kj vi recover`) loads recovered edits into dirty buffers pending explicit acknowledgment (`kj swap ack` / `kj swap discard`).

## File buffers and swap recovery

**Disk is the source of truth.** File buffers are caches, never replacements for disk.
- **`dirty_file_buffers` swap table:** Unsaved work is persisted in durable file documents as swap files. On reopen, the kernel announces recovered swaps rather than serving them silently.
- **W12 changed-on-disk guard:** `:w` refuses to overwrite a file when disk generation has advanced past the buffer's load generation, unless overridden with `:w!`.
- **`kj swap` management:** Use `kj swap list`, `kj swap ack <path>`, and `kj swap discard <path>` to inspect and resolve recovered swaps explicitly.

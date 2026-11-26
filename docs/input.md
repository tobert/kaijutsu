# Input routing, vi editing, and file buffers

## App input routing and focus rules

App input flows through a central action table that maps key combinations to `ActionFired` events handled by specific input contexts (`InputContext`). 
- **Vi editor:** Only the vi editor grabs raw keyboard input directly. Vi owns `Esc` where live; elsewhere, an `Esc` triggers a `PopLevel` action.
- **Muscle memory:** Standard screen/tmux `Ctrl+A` bindings and standard vi keybindings are preserved. `Ctrl+A k` enters the heroic view (`Screen::Heroic` in the app; "not built yet" notice in the tui).

In Heroic, select a performer and press `Ctrl+G` to write a board note.
Closing the editor returns to the selected performer.

On a first visit, Conversation follows the newest blocks. Returning to a
conversation restores its saved reading position, including the top.

### Conversation panels and Neo

Conversation opens with Workbench above Event History to the right of the
transcript. In narrow or short windows the two panels sit side by side below
the transcript. Press `Ctrl+A` then `Shift+E` to hide or show both panels.
Press `Ctrl+A` then lowercase `e` to enter Neo. The same chord returns to
Conversation with the previous panel layout.

Neo puts recent dialogue near the reader's position on the left. Patches
and ABC scores stack in the upper right. Selecting a score shows its
notation, and `Shift+Enter` opens that phrase in Conversation. Events stack below.
Corner brackets mark the stack that receives keyboard input. Press `\`
to switch stacks and `[` or `]` to browse. Selecting a call or
receipt with a linked patch brings that patch forward. Unrelated events
leave the current patch in place. `Shift+Enter` returns to the transcript
at the selected source. Press `v` on a revealed diff to open the full viewer.

A thread joins the selected event to the call's position between dialogue
turns. If the result contains a patch or the creation receipt names its
block ID, the thread also reaches that patch. “Earlier call” and “Later
call” mark calls outside the visible passage. A patch with no recorded
link stands alone. A glint runs once along a newly selected thread.
The Heroic reduced-motion setting keeps the thread still.

In Conversation, the highlighted heading marks the panel that receives
`[` and `]`. Press either key to browse from the displayed record. Selecting
a patch shows its linked result in Event History. When a short patch
leaves space in Workbench, the filenames beside `[` and `]` show where
each key will take you. Switching panels with
`\` lets you browse from that result. A selected command shows its exit
status and output when available. Its linked artifact appears in Workbench.
Browsing leaves the transcript in place until you press `Shift+Enter`
to reveal the active record.
When a shell call writes a workspace text file, its patch appears while the
call runs. Later writes in that call update the same card. If the call
touches several files, selecting one keeps that file's patch in view when
switching to its event and back. See [Kaish execution and hooks](rc.md#kaish-execution-and-hooks)
for the mounts and write operations that produce patches.

Workbench includes declared diffs, parsed unified patches from text and
tool output, and typed SVG, score, media, file, and resource blocks. A
patch may appear in a leading diff fence or before a printed `[exit N]`.
History names shell calls and results by command. Its markers show the
selected record and linked call or result. Each panel indexes up to 48
records from the latest 1,024 blocks in the active context.

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

## Agent control over BRP

An agent can fire actions in the app only while agent control is on. It is off
at startup. `kaijutsu-app --agent-control` starts it on, and `Ctrl+A g`
(`ToggleAgentControl`) turns it on or off while the app runs. While it is on,
the dock's connection text ends with `AGENT CONTROL`.

`kaijutsu/act` takes `{"action": "<name>"}` (or `"<name>:<payload>"`, such as
`JumpSeat:3`) and fires that action as the dispatcher would for a key press. It
stamps the action with the active context that binds it, and with `Global` when
none does (the `Ctrl+A` chords and wheel scroll fire as `Global` too). While
control is off it returns error code -23100. It refuses `ToggleAgentControl`:
the latch belongs to the player. It has no text parameter.

`kaijutsu/request_control` takes `{"reason": "<why>"}` and works with control
off or on. It returns `on`, `pending`, or `requested`, and records the request
without opening control. While a request waits, the hints line starts with
`^A g allow | ^A G refuse | agent asks: <reason>` (the reason is clipped to 80
characters). `Ctrl+A g` allows it and `Ctrl+A G` (`DenyAgentControl`) refuses
it; an unanswered request expires after 60 seconds. `kaijutsu/act` refuses both
actions: the answer is the player's.

`kaijutsu/state` is read-only and answers while control is off. It returns the
screen, the focus area, the active contexts, the active context id, whether
control is on, and `control_request` (`pending`, `denied`, or `timed_out` with
its reason; null when there is none or it was allowed). In Heroic, the
`heroic` field lists performer context IDs in seat order and the selected ID.
Use the zero-based seat index with `JumpSeat`.

To tour a live score, run `python3 contrib/demo/music_tour.py <score> <musician> ...`
with agent control enabled. The arguments take context labels or IDs.
The tour opens the score at full width, visits each musician, then holds
each one's lane in Heroic. Use `--neo-context <context>` to include Neo.
When the tour ends, it restores the starting view.

The latch covers only these two methods. BRP's built-in methods and
`bevy_brp_extras` (`send_keys`, `type_text`, ECS writes) are outside it, so the
latch is consent, not a boundary. `contrib/headless-app.sh start --agent-control`,
`act`, and `state` drive it for tests.


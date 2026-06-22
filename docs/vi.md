# In-App vi Editor

Goal: type `vi /etc/rc/thing/whatever.kai` and get a vi-like editor inside
kaijutsu-app. First pass is basic vi behavior. Much of the machinery already
exists (modalkit, the compose surfaces) — this is mostly connective work.

Status: **design locked, not yet implemented.** Surface substrate chosen
2026-06-22: the editor is an **in-scene MSDF panel** on a new `Screen::Editor`,
reusing the time-well's panel primitive (see "Surface substrate" below). The
near-term driver is making `kj rc edit <path>` open this editor by default
(today it is a non-interactive `--content` replace: `kj/rc.rs:402`).

---

## What already works

The vim engine is fully wired and live in the compose surfaces:

- `input/vim/` — modalkit `VimMachine` drives normal/insert/visual/replace,
  motions (`hjkl w b e gg G 0 $ ^`), operators (`d y`), registers, paste.
  Operates on a plain buffer: `InputOverlay { text: String, cursor: usize,
  selection_anchor }` (`view/components.rs:324`).
- Key path: Bevy `KeyboardInput` → `keyconv.rs` → `TerminalKey` →
  `VimMachine` → `dispatch.rs` translates actions into buffer mutations.
  Cursor shape follows mode (block/beam/underline).
- Vim edits already dual-write to CRDT on every keystroke for the chat
  surface — `dispatch.rs:127-189` (`apply_insert`/`apply_delete`) call
  `edit_input(ctx_id, pos, insert, delete)`. The file editor reuses this exact
  shape, pointed at the file block instead.
- Multi-line is partially there: the overlay grows its node height per
  laid-out line (`overlay.rs:313-347`), motions understand lines
  (`textutil.rs`). Multi-line *visual-selection highlight* is a graceful-
  degradation TODO (`overlay.rs:392`) — cursor + motions work, the cross-line
  highlight just isn't drawn.

Roughly 70% of "basic vi" already exists.

## Architecture is on-grain (not greenfield)

- **File docs are already first-class CRDT docs.** `FileDocumentCache`
  (`kaijutsu-kernel/src/file_tools/cache.rs:116`) creates them with
  `create_document(ctx, DocKind::Code, …)` + one Text block, in the same
  BlockStore, emitting to the same FlowBus the app already subscribes to.
  Context id is derived deterministically from the path (`file_context_id`).
- **The subscription is already generic.** The app subscribes with an *empty*
  `BlockEventFilter` (`actor.rs:1822`) = all contexts. It is already
  *receiving* file-block events; it just never *opens* a file doc to render
  them.
- **Peer signaling exists.** `switch_context` already goes kernel→app via
  `invoke_peer` → BRP custom method (`view/brp_methods.rs:60`,
  `peers/systems.rs:47`). The `vi` builtin follows that path.
- **Write path exists.** `edit_input` proves per-keystroke CRDT write; the
  kernel already has `store.edit_text(ctx, block, offset, new, len)` (used by
  the file `EditEngine`). We wrap that in a new generic RPC.
- **An in-scene text surface now exists** (shipped *after* this doc's first
  draft, by the time-well work). `view/time_well/panel.rs` —
  `create_msdf_panel(images, w, h) -> (Handle<Image>, impl Bundle)` and
  `commit_panel_glyphs(msdf, glyphs)` — is *"the shared shape behind every
  in-scene text surface"*: a `Mesh3d` quad sampling an RTT texture the MSDF pass
  rasterizes glyphs into. `ReadingCard` (`scene.rs:63`) already uses it to
  render *one selected context's content at readable size*. That is the editor
  panel, minus a cursor and a relayout-on-edit trigger. This is what made the
  MSDF-panel substrate the choice over the original 2D-pane sketch.

---

## Decisions (locked 2026-06-12)

1. **Dispatch: kaish builtin.** A real `vi`/`edit <path>` builtin in the
   kernel (not app-side string interception). It resolves the path to its
   *owning* CRDT `(ctx_id, block_id)` (see "Path resolution" below), then signals
   the app peer to open an editor. Rationale: keeps "everything through kaish";
   follows the `switch_context` peer-invocation precedent.

   **Path resolution — bind to the owner, not a copy (refined 2026-06-22).** The
   original draft said `FileDocumentCache::get_or_load(path)` unconditionally.
   That is correct for ordinary files but *wrong for rc/config paths*, which the
   CRDT-owned-config work (landed after this doc's first draft) made sole-owned
   single-block `DocKind::Config` documents under `/etc/rc` + `/etc/config`
   (`runtime/config_crdt_fs.rs`). For those, `get_or_load` would mint a *separate*
   `FileDocumentCache` working copy — reintroducing the dual-ownership
   write-through bug class that work deliberately deleted. So resolution is
   path-kind aware:
   - **config-owned** (`/etc/rc/*`, `/etc/config/*`): bind to
     `(config_doc::config_context_id(path), config_doc::first_block_id(blocks, ctx))`
     — the ConfigCrdtFs-owned block, the single source of truth. Edits land in
     the rc/config doc directly; `kj rc edit` is correct and collaborative.
   - **ordinary file**: `FileDocumentCache::get_or_load(path)` as before.
   The prefix check has a precedent in `file_tools/path.rs` (`is_rc_path`,
   `deny_etc_write`). Sharp edge to revisit: this ideally belongs to the mount
   table ("what (ctx, block) owns this path?"), not a hardcoded prefix — both
   backends are CRDT-backed, only the mount routing differs.

2. **Save model: bind buffer to the CRDT block** (not whole-buffer write-back).
   Edits write CRDT ops (a merge), never a whole-file replace — so concurrent
   edits are never clobbered.

3. **Binding tightness: live keystroke sync.** Every vim edit emits a CRDT op
   immediately (like the chat surface via `edit_input` today); remote ops merge
   into the buffer as you type. Fully collaborative. This was chosen *over*
   "ops-on-save" because the rollback story below makes a clean `:q!` possible
   without giving up live collaboration.

4. **Command line: keybind save/quit only** (no `:` command bar in pass 1).
   `ZZ` = save + quit, `ZQ` = quit without save, Esc-Esc to close. The modalkit
   `:` command mode (`KaijutsuAction` is currently an empty enum) comes in a
   later pass.

5. **Rollback: inverse forward edit against a checkpoint** (see below).

6. **Punt for pass 1:** the `:` command bar. (Multi-line *selection-highlight*
   rendering is no longer a punt — `Selection::geometry` gives it cheaply on the
   MSDF panel; see "Surface substrate".)

### Concrete shape

1. **kaish `vi`/`edit <path>` builtin** (kernel) — `get_or_load(path)` →
   `(file_ctx_id, block_id)`, then signal the *submitting* peer:
   `open_editor{ctx_id, block_id, path}`.
2. **App `open_editor` BRP handler** — open a `SyncedDocument` for
   `file_ctx_id`, fetch initial block text, enter `Screen::Editor`, and spawn
   the **editor panel** (an MSDF panel — see "Surface substrate" — not a 2D
   pane and not the conversation MainCell timeline).
3. **New generic RPC `edit_block(ctx_id, block_id, pos, insert, delete)`**
   wrapping the kernel's existing `store.edit_text(...)`. This is the file-doc
   analog of `edit_input`, and is reusable beyond the editor.
4. **Vim dispatch reuse** — `apply_insert`/`apply_delete` branch on surface
   kind: editor → `edit_block`, chat → `edit_input`, shell → local.
5. **Save = flush** — `ZZ` calls `flush_one(path)` (CRDT doc → disk,
   write-through already exists); `ZQ` reverts to checkpoint then closes.

---

## Surface substrate — MSDF panel on `Screen::Editor` (decided 2026-06-22)

The editor renders as an **in-scene MSDF panel**, reusing the time-well
substrate, on a **new `Screen::Editor`** state. Rejected: a flat 2D vello
overlay (the original sketch — doesn't share the 3D scene) and editing the
time-well `ReadingCard` in place (couples vi to the well). Chosen because the
panel primitive already exists and this is the cleanest path to "bring the
time-well design back" into everyday editing.

What we **reuse as-is**:

- `create_msdf_panel` / `commit_panel_glyphs` (`panel.rs`) — texture + bundle.
- The MSDF glyph path: `font.layout(text, style, align, max_advance)` →
  `parley::Layout` → `collect_msdf_glyphs(...)` → `Vec<PositionedGlyph>`
  (`text.rs:54,74`; glyph carries block-local `x`/`y`). `update_reading_card`
  (`text.rs:203`) is the template — it already lays out one doc's text onto a
  panel; the editor does the same but keyed on the *file block*, not selection.
- `Screen` FSM (`ui/screen.rs`) — built explicitly so *"future screens can be
  reintroduced without rewiring `run_if`."* Add `Screen::Editor` beside
  `Conversation`/`TimeWell`; `OnEnter`/`OnExit` spawn/despawn the panel.
- The well camera / RTT / billboarding machinery if we want the panel framed in
  3D; a head-on static framing is the pass-1 default.

What is **genuinely new on the MSDF path** (the only net-new rendering work):

1. **Cursor quad.** parley 0.7 gives the caret rect directly from a byte offset:
   `Cursor::from_byte_index(layout, byte_idx, affinity).geometry(layout, w)`
   returns a `BoundingBox`. Spawn a small `Mesh3d` quad in panel-local space at
   that rect; mode-aware shape (block/beam/underline) = scale the quad, mirroring
   the compose cursor shapes. The buffer offset → caret geometry mapping is thus
   off-the-shelf, not bespoke.
2. **Live relayout.** `update_reading_card` rebuilds glyphs only on *selection
   change* (gated by a `Local<Option<ContextId>>`). The editor must relayout on
   *every block-text change* — re-run the layout when the file block's text or
   CRDT version moves (cheap: one parley pass over a small rc file). This is the
   editor's per-keystroke (and per-remote-op) refresh.

Bonus the substrate hands us for free: **multi-line selection highlight**, the
old "punt, degrades gracefully" item (risk #6 / decision 6 below). parley's
`Selection::geometry(layout) -> Vec<(BoundingBox, usize)>` yields per-line rects;
draw them as quads behind the glyphs. No longer a graceful-degradation TODO.

---

## Rollback / checkpoint (diamond-types-extended)

We run `diamond-types-extended` 0.2 (eg-walker list CRDT, fork of Seph's
diamond-types). Investigated for checkpoint/rollback support:

- **Checkpoint primitive: yes.** `Document::version() -> &Frontier`
  (`document.rs:107`) is a checkpoint token — capture it at each `:w`/`ZZ`.
  `ops_since(&frontier)` (`document.rs:321`) returns *precisely* the op-set
  since that checkpoint. "What changed since last save" is a first-class query.
- **Op-truncation / history deletion: no — by design.** No public
  `truncate`/`revert`; deleting shared ops would corrupt merge for any peer who
  already pulled them. Append-only is load-bearing.
- **Trap:** `Branch::checkout_at_version(_frontier)` (`branch.rs:36`) *looks*
  like "materialize document as of version X" but the parameter is `_frontier`
  — **it ignores its argument. It is a stub.** Do not build on it. We cannot
  lean on the CRDT to reconstruct old-version text.

**Therefore rollback = inverse forward edit, not history erasure:**

```
:w  / ZZ  →  checkpoint = (saved_text, version())     // we already hold saved_text
…edits…   →  live CRDT ops (shared, collaborative)
:q! / ZQ  →  diff(current_text, saved_text) → edit_block ops   // forward "undo" edit
```

Cheap, restart-safe, collaboration-safe (peers see an undo edit land), reuses
`edit_block`. We already hold `saved_text` from open / last save, so we do not
depend on the stubbed checkout. The frontier is still useful as a precise
checkpoint token (dirty indicator, detect external merges after save).

This is why decision (3) live-sync and decision (4) clean `:q!` are *not* in
tension: the frontier is the checkpoint, the diff is the rollback.

---

## Lingering questions / risks to confront in the plan

1. **Principal→peer addressing for `open_editor`.** Does the `vi` builtin know
   *which* app peer ran the shell command? Verify against the `switch_context`
   path — it must already resolve principal→peer to drive the right app. Two
   known related bugs in memory: peer registry stays empty after kernel restart
   until `kj` restart (`tech_debt_peer_reattach_on_reconnect`), and
   `switch_context` updates `active_id` but doesn't drive Screen state
   (`tech_debt_switch_context_screen_transition`). The editor-open path may hit
   the same Screen-transition gap.

2. **Offset reconciliation on remote ops mid-edit.** The genuinely fiddly part
   of live sync: the buffer cursor is a byte offset; a remote op landing while
   you edit shifts offsets. Need a deliberate strategy (transform local cursor
   against incoming ops). The chat surface is mostly single-author so it hasn't
   had to solve this hard.

3. **A real editor surface, distinct from the conversation timeline.**
   *Largely resolved by the substrate decision* — the MSDF panel + `Screen::Editor`
   is that surface (see "Surface substrate"). Residual: the app still routes block
   events to the active-conversation MainCell (`view/sync.rs`), and the doc cache
   is conversation-keyed (`view/document.rs`). The editor opens its file doc as a
   `SyncedDocument` outside that path and feeds its text straight to the panel
   layout, so it sidesteps MainCell routing rather than extending it — but the
   doc-cache entry for a file doc still wants a `DocKind` discriminator so the two
   don't collide.

4. **`ZQ`/`:q!` semantics with live sync.** Decided: diff-to-checkpoint. Open
   detail — if collaborators made edits *after* our last `:w`, a naive
   "restore saved_text" would also stomp *their* post-save edits. Revert should
   target *our* delta, not blindly reset to `saved_text`. May need
   `ops_since(checkpoint)` to scope the inverse edit to our own ops, or accept
   last-writer semantics for pass 1 (note it).

5. **rc-write capability.** `/etc/rc/*` requires the rc-write capability; the
   rest of `/etc` is denied flat (`file_tools/edit.rs`). The editor open + save
   path must surface permission errors loudly (no silent fallback), per the
   crash-over-corruption stance.

6. **Enter binding.** Compose uses `submit_on_enter()` (Enter submits). The
   editor needs the opposite: Enter inserts a newline. That means a *separate*
   VimMachine binding set for the editor surface.

---

## Key file anchors

Paths are under `crates/` (the repo moved to a workspace-crates layout after this
doc's first draft). Line numbers drift — treat them as hints, grep the symbol.

| Concern | Location |
|---|---|
| **MSDF panel primitive** (reuse) | `crates/kaijutsu-app/src/view/time_well/panel.rs` (`create_msdf_panel`, `commit_panel_glyphs`) |
| **Glyph layout template** (reuse) | `crates/kaijutsu-app/src/view/time_well/text.rs` (`card_text_glyphs`, `update_reading_card`) |
| **ReadingCard panel** (template) | `crates/kaijutsu-app/src/view/time_well/scene.rs:63` |
| **Screen FSM** (add `Screen::Editor`) | `crates/kaijutsu-app/src/ui/screen.rs` (`enum Screen`) |
| Positioned glyph (block-local x/y) | `crates/kaijutsu-app/src/text/msdf/glyph.rs:43` (`PositionedGlyph`) |
| **Cursor caret geometry** (new) | parley 0.7 `Cursor::from_byte_index(..).geometry(layout, w)` (`editing/cursor.rs`) |
| **Selection rects** (free win) | parley 0.7 `Selection::geometry(layout)` (`editing/selection.rs`) |
| Vim machine setup | `crates/kaijutsu-app/src/input/vim/mod.rs` (`submit_on_enter`, `KaijutsuAction` empty enum) |
| Vim dispatch + CRDT dual-write | `crates/kaijutsu-app/src/input/vim/dispatch.rs` (`apply_insert`/`apply_delete`) |
| `kj rc edit` (the entry point to default) | `crates/kaijutsu-kernel/src/kj/rc.rs:402` (`rc_edit`) |
| File doc cache | `crates/kaijutsu-kernel/src/file_tools/cache.rs` (`FileDocumentCache`, `file_context_id`) |
| Kernel block edit (`edit_text`) | used by `crates/kaijutsu-kernel/src/file_tools/edit.rs` |
| Peer method dispatch (precedent) | `crates/kaijutsu-app/src/view/brp_methods.rs`, `peers/systems.rs` |
| Conversation-only routing (residual) | `crates/kaijutsu-app/src/view/sync.rs` |
| Conversation-keyed doc cache (residual) | `crates/kaijutsu-app/src/view/document.rs` |
| CRDT version / ops_since | `diamond-types-extended` `document.rs` (`version`, `ops_since`) |
| CRDT checkout stub (do not use) | `diamond-types-extended` `branch.rs` (`checkout_at_version`) |

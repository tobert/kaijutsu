# Input — one grammar for keyboard, gamepad, and everything after

*Seeded 2026-07-16 from a full survey of the live code; the unification
slices SHIPPED the same day and were verified in the running app (contexts,
grabs, prefix chords, scene actions, clipboard paste). The keymap below is
the as-built truth, modulo the items marked **deferred**. "The mess we
started from" is kept as the record of why the architecture looks like
this.*

## The mess we started from: two regimes (historical, fixed same day)

**Regime 1 — the central dispatcher** (`crates/kaijutsu-app/src/input/`),
Conversation screen only. `FocusArea` → derived `InputContext` set → one
`dispatch_input` system → binding-table match → `ActionFired` → domain
handlers. It already carries everything the rest of the app is missing:
user config (`~/.config/kaijutsu/bindings.toml`), context priority for
conflicts, and complete gamepad support (dpad, triggers, analog sticks,
face buttons). Compose is a sub-regime: the modalkit VimMachine owns all
keys while compose is focused, with hardcoded bypasses for Ctrl+C
(interrupt) and Ctrl+Z (surface toggle), plus vim-mode-aware double-Esc
dismiss (`vim/dismiss.rs`).

**Regime 2 — five raw-key readers** that grew during the scene work and see
none of the above: `room_keyboard` (the octagon carousel,
`view/room/mod.rs`), `well_keyboard` + `toggle_legend` (the zoomed well,
`view/time_well/`), `patch_bay_keyboard`, `plain_zoom_keyboard`,
`fsn_camera_fly`/`fsn_keyboard` (`view/fsn/scene.rs`), and
`editor_dispatch_keys` (`view/editor/mod.rs`, the vi forwarder). Isolation
is held together by the suppression hack at `input/context.rs:67` (on
`Screen::Editor | Room` only `Global` context survives — **Fsn was
forgotten**, a latent Esc double-fire), `run_if` state gates, and
`.after(room_keyboard)` ordering contracts every new station must know
about.

**Strays**: `Ctrl+1/2/3` MRU switching (`commands/conversation.rs`) and the
`Ctrl+W` well toggle (`time_well/scene.rs:815`) gate only on "not typing" —
both fire on every screen, including under the fullscreen vi editor (whose
focus parks on `Conversation`), while the editor simultaneously forwards
`<C-1>`/`<C-w>` to kernel vi. The dock click handler has no gating at all.

## As built: one table, explicit grabs, a prefix in front

```text
Raw input
  │
  ▼
Ctrl+A prefix machine        — sees every key first, on every surface
  │ (unclaimed keys fall through)
  ▼
Keyboard grab?               — vi editor session, compose VimMachine
  │ yes → whole stream to the grab owner (vi notation / modalkit)
  │ no ↓
  ▼
dispatch_input               — ONE binding table, contexts per surface
  ▼
ActionFired → domain handlers (scenes consume actions, never raw keys)
```

1. **Contexts per surface.** New `InputContext` variants — `RoomNav`,
   `WellZoomed`, `PatchBayZoomed`, `StationZoomed`, `FsnFly` — derived by
   `sync_input_context` from `Screen` + `RoomState` instead of bailing.
   Scenes consume `ActionFired` (`StepNext/StepPrev`, `LevelUp/LevelDown`,
   `Dive`, `PopLevel`, well verbs, fly axes). Gamepad, `bindings.toml`,
   and the `?` legend (rendered from `InputMap` labels) then cover the
   scenes for free. The suppression list and the `.after` ordering
   contracts die; that also fixes the Fsn double-fire structurally.
2. **Keyboard grabs are explicit.** The vi editor is the one sanctioned raw
   consumer — a declared exclusive grab, not a suppression side effect. The
   compose VimMachine becomes the second grab, which retires the
   `vim_owns_keyboard` skip-dance inside `dispatch_input`.
3. **The Ctrl+A prefix machine** (`input/prefix.rs`) sits in front of
   everything, tmux-style: prefix wins even over vi; `Ctrl+A a` is the
   literal-passthrough escape hatch (kernel vim's own Ctrl+A increment
   stays reachable — the literal travels as its own `LiteralPrefix` message
   because modifier state is long released by resolve time). ~1s timeout;
   an unbound second key is swallowed with a log flash, never leaked to
   the surface below. The chord table is deliberately hardcoded (one
   screenful; per-user prefix chords would defeat the muscle memory).
   While armed, the footer hint line shows the chord table
   (`ui/dock.rs::update_hints`).

## The prefix table

The working set is GNU screen's, on purpose (Amy: "Ctrl-A [aq'"A0-9] is
most of my working set"). Ring 0 of the well — ACTIVE, the rank
(`docs/timewell.md` "Ring membership becomes explicit") — is the window
list.

| Chord | Action | Screen ancestry |
|---|---|---|
| `Ctrl+A 0–9` | Switch to ring-0 seat *n*, from anywhere | window n |
| `Ctrl+A Ctrl+A` | Toggle to previous context | other window |
| `Ctrl+A a` | Send literal Ctrl+A to the focused vi surface | meta |
| `Ctrl+A q` | Close-and-demote: demote the current context one step on the ring ladder (kernel `demote_context`), then switch to the MRU-previous context; nowhere to land → demote and stay | (repurposed) |
| `Ctrl+A "` | Open the well (zoomed) — the interactive picker | windowlist |
| `Ctrl+A w` | Synonym for `"` — the well | — |
| `Ctrl+A '` | Switch-by-prompt: prefilled-`kj` prompt, `kj context switch ` (see pattern below) | select |
| `Ctrl+A A` | Rename current context: prefilled-`kj` prompt, `kj context rename ` (verb added 2026-07-16; label-stealing stays `retag`'s latched job) | title |
| `Ctrl+A n` / `p` | Next / previous ring-0 seat | next/prev |
| `Ctrl+A d` | Detach to Conversation view from any scene/editor | detach |
| *(armed)* | The footer hint line shows the whole chord table while a prefix is pending — the legend appears exactly when you need it, so there is no separate `?` overlay | help |

**The prefilled-`kj` prompt pattern** (Amy, 2026-07-16: "pop a kj so the
user can type and hit enter — we might use that pattern elsewhere"): summon
the shell surface with a command line already typed up to the argument,
cursor at end; Enter runs it, Esc abandons it. `Ctrl+A A` and `Ctrl+A '`
are the first two users; any verb that needs one free-text argument can
ride it.

## Escape — two meanings total

Esc belongs to vi wherever a vi surface is live; everywhere else it is
exactly one action.

| Surface | Esc does |
|---|---|
| Vi editor (`Screen::Editor`) | Forwarded to kernel vi, never stolen |
| Compose overlay | To the VimMachine (mode switch); double-Esc in Normal mode dismisses (kept — works in practice) |
| Everywhere else | `PopLevel`, one resolver walking the level ladder: well focus → overview → room; patch bay → room; fsn → room; room → conversation; dialog → cancel |

## Scene contexts (same keys, now table-driven and rebindable)

| Context | Bindings |
|---|---|
| RoomNav (octagon) | `←/→/Tab` cycle stations · `Enter/↓` dive · `Esc` pop |
| WellZoomed | `0–9` seat of *focused* ring · `←/→/Tab` spin · `↑/↓` ring (Up at mouth → hero pose) · `Enter` focus/commit · `p d c z a` verbs · `?` legend · `Esc` pop |
| PatchBayZoomed | `←/→/Tab` wires · `r` rescan · `↑/Esc` pop |
| StationZoomed (plain) | `↑/Esc` pop |
| FsnFly | arrows + WASD fly (WASD kept until the keys are needed elsewhere) · `PgUp/PgDn` altitude · `Esc` pop |

## Ctrl chords and clipboard (xterm-style)

| Input | Action | Note |
|---|---|---|
| `Ctrl+C` | Interrupt ladder (soft → hard → hard+clear) | never copy |
| `Ctrl+V` | Paste CLIPBOARD into compose | compose only — in the editor it forwards to vi (visual block) |
| middle-click | Paste PRIMARY | |
| selection | Auto-copies to PRIMARY (mouse or vi visual) | |
| `Ctrl+Z` | Chat↔shell toggle; in the editor: suspend to shell | unix suspend metaphor |
| `Ctrl+D/U` | Half-page scroll | unchanged |
| `Ctrl+6` | Previous-pane toggle | unchanged |
| `Alt+hjkl/v/s/q/[/]` | Tiling | unchanged |

## Gamepad

The scene should be fully navigable by pad once the contexts land.

| Input | Action |
|---|---|
| Start (`\|\|\|`) | Go to the well, from anywhere — pad-side `Ctrl+A w` |
| DPad | Context-sensitive arrows: blocks / carousel / rings / seats |
| South | Activate / dive / commit |
| East | `PopLevel` |
| Left stick | Scroll (conversation) · fly (fsn) · ring spin (well) |
| Triggers | Page up / down |
| North | Cycle focus |

## Retired by this design

- `Ctrl+1/2/3` MRU shortcuts — superseded by `Ctrl+A digit` + `Ctrl+A Ctrl+A`.
- `Ctrl+W` as the direct well toggle (it was never sacred, just first) —
  replaced by `Ctrl+A w`/`Ctrl+A "` and gamepad Start; inside the editor
  `<C-w>` now reaches kernel vi cleanly.
- The dead clipboard actions (`Copy/Cut/SelectAll/Undo/Redo`) and the
  `action.rs` comment claiming Ctrl+A means SelectAll — unreachable since
  the VimMachine took compose; undo/redo belong to vi.
- The implicit `Editor | Room` context-suppression list — replaced by
  explicit grabs + per-surface contexts.
- Three separate 500ms double-tap machines (`interrupt.rs`,
  `vim/dismiss.rs`, the dead `nav.rs::DoubleTap`) — consolidated on one
  helper.

## Seeds parked elsewhere

- "Done for now" context marker + auto-summarize-when-quiet — no existing
  `ContextState` carries that intent (`Concluded` is *done*, demotion is
  *placement*); see `docs/issues.md`.

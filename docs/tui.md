# The terminal client — `kaijutsu-tui`

**Status:** built; first cut shipped 2026-09-02, and Amy plays it. The
shapes are the ASCII figures below; the direction is `AGENTS.md`,
"Proprioception". Amy's statements here are guidance, not rulings — there
is room to trip on a new combination.

`kaijutsu-tui` is a standalone binary that dials the kernel over the existing
`kaijutsu-rpc` SSH subsystem and renders a terminal UI with ratatui. It is the
ssh seat: `ssh -t zorak kaijutsu-tui` and play. It is **not** a kernel
subsystem, and it is **not** a Bevy frontend — see "Roads not taken" for both.

```text
terminal (wezterm, Blink, a plain xterm)
   │  keys + cells
kaijutsu-tui            ratatui inline viewport; modalkit for the vi surfaces
   │  kaijutsu-client   ActorHandle, DocumentStore/ContextMirror, ServerEvent
kaijutsu-server / kernel
```

## Guidance (Amy, 2026-08-30)

1. **Inline viewport** — superseded 2026-09-13 by "The owned screen".
   The first cut followed *"I tend not to like the fullscreen modes"*: the
   transcript flowed into the terminal's own scrollback and the live UI
   was a viewport at the bottom, the codex-rs / Claude Code shape, growing
   for a dashboard and shrinking back. Two weeks of use found the costs
   (a shrink leaves blank rows, a slow hop stalls on the cursor query,
   nothing printed can be redrawn, contexts interleave in one history)
   and the tui now takes the alternate screen for the whole session.
2. **One process is the mux.** *"What if I don't really need the mux anymore
   and I can just connect on ssh and run the tui and explore contexts and even
   have some dashboard views ala the app."* Contexts are windows; the GNU
   screen prefix table from `docs/input.md` is the switching surface.
   Persistence is the kernel's, not the client's — a dropped ssh loses nothing,
   because the TUI holds no state a reconnect cannot rebuild.
3. **Beat timing lives in `kaijutsu-audio`.** Already true: `LocalBeat`,
   `BeatRef`, the stale ladder and the deadband are in
   `crates/kaijutsu-audio/src/timebase.rs`. Only the per-track map
   (`WellBeats`) was app-side and now lives in `kaijutsu-present`
   (`beats.rs`), with the `Resource` derive replaced by an app-side newtype.
4. **One `bindings.toml` for both clients**, keyed by vim key notation. The
   TUI ships it first; subagents convert the app after. *"I'll want the
   commentary in that toml to be clinical and reviewed by a couple flash tier
   kaibo agents for clarity and long-term support."*
5. **`docs/ssh-shell.md` is retired.** Its two durable paragraphs are melted
   into this document ("Melted from the ssh shell design").
6. **codex's look is fine.** Lean into modalkit; the key pidgin is
   screen/tmux/vi/vim, the one the app already speaks.

7. **Match the tool the hand already knows** (2026-09-03; `AGENTS.md`,
   "Proprioception"). The `:` line draws as vim's does: no gap, `:!` as
   typed. There is no state past normal mode. **`Ctrl+A [` is tmux's copy
   mode** — Amy's `.tmux.conf` has `mode-keys vi`, `bind [ copy-mode`,
   `bind ] paste-buffer` — and it means the same thing here: from the
   conversation, switch so that movement goes up and down the transcript.
   The transcript becomes a buffer on the alternate screen under vi
   motions (`j`/`k`, `Ctrl+U`/`Ctrl+D`, `gg`/`G`, `/` and `?` search),
   `Space` marks and `Enter` copies to the clipboard (OSC 52 over ssh; `v`
   and `y` are the vim spelling of the same two acts), `q` or `Esc`
   leaves. (Decision record: since slice 3, `Space` snaps the view to the
   tail instead of marking, and `v` is the one key that marks — "Scrolling
   is copy mode".) Fullscreen exists only where vim and tmux themselves go
   fullscreen: the editor, the diff, copy mode (since 2026-09-13 copy mode is
   the scrolled transcript itself, not a screen of its own). Amy: *"I almost hit ctrl-a
   [ to start scrolling up in this window so what if we put that in?"*
   Tool output is never collapsed in the tui; a block is read whole, and
   reading it better is a print-time problem (a formatter over kaish
   `.data`, longer-term), never a redraw — copy mode is how a long result
   is read, the way `less` would be. Two things differ from tmux, named:
   the buffer is the tui's own (the context mirror, so it holds the whole
   context and search crosses all of it, not only what was printed), and
   the mouse wheel stays the terminal's — since 2026-09-13 it arrives as
   arrow keys and scrolls the tui's own transcript ("The owned screen"). The tui is not run under tmux; the targets are
   iTerm2 and the Linux terminals (Amy: *"my loyalty to wezterm is the
   scrolling with mux + claude code is so damn good, otherwise I like
   trying new terms"*), which is also the OSC 52 clipboard's target set.

## The owned screen (Amy, 2026-09-13)

The buffer question ("Open", below) is decided: the tui **is the mux** and
takes the alternate screen for the whole session, owning its transcript,
scroll and search the way vim and tmux do. Amy: *"I think I'm going to
change my mind about the terminal history, and let it be trashed. We can
build up what I want from scrolling even better I think. Like, some
minimal mouse integration to grab scroll wheel and go into copy mode
automatically, and so on."* And on timing: *"We're still pretty fresh so
let's try to do the big moves now."* This supersedes guidance 1's inline
viewport and the "exactly one viewport claim" rule under "Surfaces"; both
are annotated below and their sections are rewritten as the slices land.

What this buys, in the order it was felt: no cursor-position query, so no
two-second stall on a slow hop; no band grow and shrink, so no blank rows
after `Esc`; the transcript re-wraps on resize; a thinking fold, an
exclusion or an edit redraws in place; copy mode stops being a snapshot;
one transcript per context, so switching seats switches buffers (the
per-context copy mode shelved on 2026-09-08 falls out for free); search
and jump over the whole conversation. What it costs: after `:q` the
conversation is not in the terminal's history. The kernel holds every
block, so nothing is lost, but "scroll up after quitting" is gone.

### The mouse stays the terminal's

The tui **never enables mouse reporting** (no DECSET 1000/1002/1003).
Amy: *"I use select with autocopy, xterm/x style, all the time in
wezterm."* With reporting off, the terminal keeps every mouse gesture:
select, autocopy to clipboard and PRIMARY, right-click and middle-click
paste (which arrive as a bracketed paste), OSC 8 link clicks. None of it
needs a modifier. With reporting on, all of that would need `Shift`
(`bypass_mouse_reporting_modifiers`), which is Crush's model: it captures
the mouse every frame, drag-selects inside its own widgets and copies over
OSC 52 plus a native clipboard library (`~/src/research/crush`,
`internal/ui/model/chat.go`, `internal/ui/common/common.go`), with no
PRIMARY selection at all. Vim with `mouse=` off is the precedent we follow
instead (`~/src/research/vim/src/os_unix.c`, `mch_setmouse`).

**The wheel arrives as arrow keys.** On the alternate screen with
reporting off, wezterm turns each wheel tick into arrow-key presses
(`alternate_buffer_wheel_scroll_speed`, default 3, automatic; xterm needs
DECSET 1007, which the tui sends; kitty and foot do it by default). Vim
cannot tell such an `Up` from a typed one and neither can the tui, so one
rule covers both: **`Up` at the top edge of the draft scrolls the
transcript, `Down` at the live tail does nothing.** A one-line draft is
the common case, so every `Up` scrolls. Inside a taller draft `Up` moves
the cursor first, as vim does, and scrolls once it is on the first line.
The `:` bar keeps `Up`/`Down` for its history. `Ctrl+A [` still enters
copy mode outright, and `PageUp`/`PageDown` scroll by a screen.

A later opt-in mouse mode, tmux's `mouse on`, could add click-to-select-
block and drag-select over OSC 52. It is never the default, because it
takes the terminal's own selection away.

### Scrolling is copy mode

Leaving the live tail is entering copy mode; reaching it again is leaving.
The transcript area follows new blocks while the view is at the tail
(Crush calls this `follow`), and stops following the moment it is
scrolled. While scrolled, the transcript owns copy mode's own keys (vi
motions, `/` and `?`, `n`/`N`, `v` marks and `y` or `Enter` copies, `q`,
`Esc`, `G`). **Every other key snaps the view back to the tail and goes
to the draft.** The `Ctrl+A` prefix and its chord stand aside and do not
snap, so a seat switch keeps the scrolled place and `Ctrl+A [` while
scrolled is a no-op (`Keys::claims`). Amy: *"live typing should snap back to the tail, I often
hit space just to do that"* — the habit is wezterm's
`scroll_to_bottom_on_input`, so `Space` is the snap key here and no longer
starts a mark; `v` does, the vim spelling copy mode already carried. The
draft stays live while scrolled, drawn as it is. There is no frozen
snapshot: a still-streaming block grows under the reader, and the view
keeps its place by block and line, not by row.

### The buffer

Amy: *"whole context, lazy seems smart, though we may have a lot of
candidates so it should probably only keep the top ring / hot set
loaded."* One transcript per context, rendered from that context's block
mirror, wrapped at the current width on demand and re-wrapped on resize.
The ACTIVE ring's contexts stay resident; the current context is always
resident; a context outside the ring is hydrated on switch and released
when it leaves the ring and the screen. A resident transcript holds the
whole context, so search crosses all of it; nothing is paged from the
kernel by scroll position in the first cut, and a cap on rendered lines
per transcript is the first thing to add if memory says so.

**Landed 2026-09-13.** The scrolled state (`TranscriptView`) lives on each
`ContextView`, not on the screen, so switching seats keeps each context's
own place and mark. The hot set — `App::hot_set` — is the current context,
the previous context (`Ctrl+A Ctrl+A`), and every ACTIVE-ring seat; those
stay resident (feed watched, mirror hydrated, wrap entries kept). After
every refresh round the loop releases contexts that left the hot set
(forwarder aborted, `ContextView` dropped, `WrapCache::forget_context`) and
hydrates every missing hot context in one spawned task per round, each
landing through `adopt`; a switch releases too. `Feeds::hydrating` is the
in-flight guard, so a switch mid-hydrate waits rather than subscribing
twice. Dropping the receiver ends the feed on the wire, and the release
also tells the actor to forget the context (`unsubscribe_context`) so a
reconnect does not re-issue it.
No cap landed on rendered lines: a warm frame over 5,000 blocks measured
2.4 ms in release, 12.9 ms in debug (`render.rs`'s
`a_warm_frame_over_five_thousand_blocks_costs_a_screenful_not_a_context`),
so the plan pass stays cheap enough without one.

### What owning the screen lets us use

Each of these is a terminal feature the inline design could not touch.
None is a commitment; the ones marked *now* ride the first slice.

- *now* **Alternate screen for the session** (`?1049`), taken after the
  connection is up so a failure is a plain line, left by every exit path
  (`restore_terminal`). Suspend leaves it and resume retakes it, vim's
  `stoptermcap`/`starttermcap` order.
- *now* **Alternate scroll** (`?1007`) for terminals that need it.
- *now* **Synchronized output** (`?2026`): one `BSU`…`ESU` around each
  frame so a full redraw never tears. Vim probes support with `DECRQM`
  and remembers the answer; wezterm, kitty, foot and iTerm2 support it.
- *landed 2026-09-13* **Focus reporting** (`?1004`): enabled with
  `EnableFocusChange` when the screen is taken and disabled with
  `DisableFocusChange` on every exit path (`enter_terminal`,
  `restore_terminal`, and around a suspend). `App::focused` defaults to
  `true` and follows `Event::FocusGained`/`FocusLost`; while unfocused
  `render::strip_animating` holds the in-flight strip's spinner still and
  the beat wake is disarmed (`run.rs`'s `FocusLost` arm sets `beat_wake =
  None`), so the track pulse stops too. Gaining focus re-arms the beat
  timer (`rearm_beat_wake`) and marks the frame dirty for a redraw.
- *landed 2026-09-13* **Window title** (OSC 0): `<label> — kaijutsu` for
  the context on screen (`run::wanted_title`), sent once per change under
  the terminal lock (`run::title_to_send`/`set_title`). xterm's title
  stack is pushed (`CSI 22;0t`, `run::TITLE_PUSH`) when the screen is
  taken and popped (`CSI 23;0t`, `run::TITLE_POP`) on every exit path and
  around a suspend, so the shell's own title comes back as found.
- *landed 2026-09-13* **Desktop notification** (OSC 9 / OSC 777) when an
  ask lands unfocused: `asks::ask_notification` fires once, from the
  refresh round's first new ask, only while `!app.focused` — an ask that
  lands in front of the player is already the card on screen, not a toast.
- **OSC 8 hyperlinks**: detection landed (`present::links` — absolute
  paths with at least two segments at word boundaries, `http(s)` URLs,
  trailing sentence punctuation trimmed, `file://<host>/<path>` targets);
  emission is blocked because ratatui 0.30 has no hyperlink attribute on
  spans or styles and the backend diffs cells (`docs/issues.md`).
- *landed 2026-09-14* **Kitty keyboard protocol**
  (`DISAMBIGUATE_ESCAPE_CODES`), on by request only: `--kitty-keyboard`
  pushes it with the screen (`CSI > 1 u`, after `?1049h`) and pops it on
  every exit path (`CSI < 1 u`, before `?1049l` — kitty keeps a separate
  flag stack per screen buffer). A lone `Esc` then arrives unambiguous,
  `Ctrl+I` stops reading as bare `Tab` (`keys::Keys::interpret`'s
  `KeyCode::Tab if !ctrl`), and `Shift+Enter` arrives apart from `Enter`
  but is reserved, bound to nothing (below, "Open"). Wezterm ships the
  protocol off by default (`enable_kitty_keyboard`); vim requests it through
  `'keyprotocol'`. The same disambiguation takes `Ctrl+M` apart from
  `Enter` and `Ctrl+[` apart from `Esc`: under the protocol `Ctrl+M` is a
  control chord for the draft, not a submit.
- **Images** per terminal: iTerm2 inline images in wezterm and iTerm2,
  the kitty protocol in kitty; sixel is preliminary in wezterm. "Images"
  below stays additive.
- **In-band resize** (`?2048`) instead of `SIGWINCH`, where offered.

### What went

Slice 2 deleted `run::set_viewport_height`, `enter_terminal`'s inline
viewport, `render::insert_before`, `print_scrollback` and
`take_settled_prints`, `ContextView::printed` and `last_printed`, the
resize-refusal fallback, `AltScreen` as a second `Terminal`, and the
harness's `mute_cursor_queries`. The pty harness stays: it already parses
the alternate screen, and it now counts `ESC [ 6 n` instead of answering
it, so a probe can assert the client sends none.

Slice 3 deleted the frozen `CopyScreen` and `render::copy_buffer_lines`
that built it, and `ScreenMode::Copy`, the full-screen mode that drew it:
copy mode is now the transcript itself, scrolled, addressed by block and
line (`copy::Anchor`) rather than snapshotted at open. The tool-result cap
went with it — the scrollback-only cut (`render::cap_tool_result`) that
kept a live-tail print to one screenful and relied on copy mode's separate,
uncapped render of the same block to make the rest reachable; with one
view for both, a long result renders whole and the transcript scrolls
("Conversation").

### Slices

1. **Docs.** This chapter; guidance 1, the viewport claim and the buffer
   question annotated. (This commit.)
2. **The owned transcript.** Landed 2026-09-13. Alternate screen for the
   session, synchronized frames, the transcript as a scrolling view over
   the current context's mirror with the band below it, every surface an
   overlay, no viewport rebuilds, no cursor queries. Probes: exit
   restores the main screen; a completed block appears above the band;
   a resize re-wraps.
3. **Scroll is copy mode.** Landed 2026-09-13. `Up` at the draft's edge,
   `PageUp`, and `Ctrl+A [` all enter it; the tail returns on `q`/`Esc`/`G`.
   `?1007` sent. Probe: wheel-as-arrows scrolls and a typed key at the tail
   still edits the draft.
4. **Per-context buffers and the hot set.** Landed 2026-09-13. Switching
   seats switches transcripts; the ACTIVE ring stays resident.
5. **Terminal features** from the list above, one at a time, focus and
   title first. Focus reporting, the window title and the desktop
   notification landed 2026-09-13; OSC 8 link detection landed the same
   day (emission still blocked on ratatui); the kitty keyboard protocol
   landed 2026-09-14, on by request. Images and in-band resize are open.

### Open

Decided 2026-09-14: the kitty keyboard protocol is on by request
(`--kitty-keyboard`), never by probe. `supports_keyboard_enhancement` asks
the terminal with `CSI ? u`, and this client's pinned invariant of zero
terminal queries
(`tests/terminal_fit.rs`'s `the_client_never_asks_where_the_cursor_is`)
covers that query too — a probe-on-startup would have broken it. No config
file exists yet for the client; a config key comes later with
bindings.toml. `Shift+Enter` is reserved under the protocol: a lane built
it as a submit from insert mode and Amy set it aside ("escape-enter works
well and is vim-y-er"), so it stays unbound until something else claims
it. Probe: `shift_enter_is_reserved_under_the_kitty_protocol`.

Decided 2026-09-13: the draft stays live while scrolled and typing snaps
to the tail (above); `:q` is quiet — nothing is printed onto the primary
screen (Amy: *":q can be quiet"*).

## Shape: the ACP bridge minus the protocol

`crates/kaijutsu-acp` is the skeleton, and its three-part split is the rule:

| Part | In `kaijutsu-acp` | In `kaijutsu-tui` |
|---|---|---|
| Kernel side | `bridge.rs` — connect, `spawn_actor`, one pump per context into a `ContextMirror` | same, unchanged in shape |
| Pure mapper | `update.rs` — "no RPC, no I/O, no clock"; unit-tested without a kernel | `present.rs` — `BlockSnapshot` → styled lines, same discipline |
| Edge | ACP JSON-RPC over stdio | ratatui over crossterm |

Two ACP files are client-generic and move **down into `kaijutsu-client`**
rather than being copied: `rank.rs` (ring seats via
`kaijutsu_viz::layout::assign_ring_seats`, the same pure function the well
uses) and the ledger round trip in `permission.rs`. After that, app, ACP and
TUI share one rank and one ask poll.

**Write the renderer generic over `ratatui::Backend`**, with input as a
`KeyEvent` stream and the kernel reached only through `ActorHandle`. That is
what keeps the standalone binary honest, and it is what would let a
kernel-served transport exist later without a rewrite — see "Roads not
taken".

## Surfaces

**Nothing you would paste is inside a box.** Tool output, code, paths,
commands, ask statements and picker rows render flush-left with no border
glyphs and no column bars, so a terminal selection of any of them pastes
clean. Role dividers and the status line are the only ruled lines, and
neither is something you paste. (Amy, 2026-08-30: *"copy/paste can just
work, there's no borders around stuff I commonly want to grab."*)

**How a surface is specified.** Every surface section below follows one
grammar, and a new surface arrives in the same shape:

- **Entry gesture in the heading** (`Ctrl+Z`, `Ctrl+A "`). Ambient surfaces
  (conversation, status line) have none.
- **One figure, and the figure is the spec** — "the example is the rule"
  applied to UI. Because every overlay renders its own key line ("Keys"),
  the figure's last line documents the surface's keys for free.
- **"Rules the figure carries"** — bullets for the semantics the picture
  cannot show.
- **The machinery, named** — the wire or `kj` path behind the surface
  (`shell_execute`, `subscribeLedgerEvents`, `edit_input`). A surface that
  cannot name its kernel path is not designed yet.
- **Every surface is drawn on the owned screen.** The conversation is the
  transcript area: a scrolling view over the current context's mirror,
  bottom-aligned above the band. An overlay (picker, ledger, an ask card,
  the thinking pane) takes rows at the transcript area's foot, between the
  transcript and the band, and gives them back on dismiss. A full-screen
  surface (vi, diff) takes the whole screen. The band — the in-flight
  strip, a blank row, the draft, the status line — is always drawn; only a
  full-screen surface replaces it. Copy mode is neither: it is the
  transcript area itself, scrolled, with the band's status row swapped for
  its own hint line ("Copy mode (scroll, or `Ctrl+A [`)").

Three sanctioned deviations: compose's figure is the `❯` line inside the
conversation figure — it is part of that frame, not an overlay; the
in-flight strip is one fixed row of that same frame ("The in-flight
strip"); and editor/diff has no figure because its look is vim's,
specified by `EditorState` rather than by this document. Cache health and Images are
rendering concerns that ride other surfaces, not surfaces of their own.

### Conversation

The transcript is redrawn every frame from the current context's block
mirror (`render::transcript_window`): every block, wrapped at the current
width, re-wrapped whenever the width changes. Nothing is printed once and
left; a fold, an exclude or an edit shows on the next frame the way any
other change does.

```text
  ╭ claude · coder ─────────────────────────────────────────────── 14:02:11 ╮
  │ The unlink bug was in resolve(): it canonicalized the final component,   │
  │ so the symlink's target was removed instead of the link. resolve_nofollow │
  │ fixes unlink; rename and getattr share the cause and are deliberately ▍  │
  ▸ shell  cargo test -p kaijutsu-kernel vfs::                     running 4s
  ╰──────────────────────────────────────────────────────────────────────────╯

  ❯ and getattr? _                                                  -- INSERT --
  0 kaijutsu*  1 kaish@  2 lfm2d  3 exo        │  -- NORMAL --  17.3/128k  91%  4m
```

Rules the figure carries:

- The role divider names principal, `context_type` and the block's wallclock.
  One blank row sits above it when the speaker changes (none above the first
  speaker), and one blank row sits above the `❯` line: air between what is
  read and what is typed, never a border.
- **A tool call and its result are one unit under one header:**
  `─ deepseek-v4-flash · shell ─ kj ledger list ─────── 07:27:12`, then the
  result's body with no second divider and no gap. The header is who
  called (the cast for a model's call, you for a `:!` or `:kj` line), the
  tool, and the call's one-line argument when its input amounts to one
  string (`inflight::one_line_arg`: a bare string or a one-member object
  such as `{"command": …}`); the argument clips with `…` to keep the
  stamp at the edge. A call whose whole one-line argument is in its header
  prints no body — the body would repeat the header as JSON; a clipped
  argument, a heredoc, or an input with more shape than one string prints
  the body whole under the header. A result joins the call printed
  directly before it when it names that call through `tool_call_id`, or
  names none (`present::continues_pair`); any other result carries its own
  divider. Tool names print as the kernel sends them (`shell`,
  `shell_write`, `read`); a shortening table is a later, one-place change.
  (Amy, 2026-09-04: *"could `tool` be the actual tool name? maybe
  principal name far left … possibly some args inline?"* — option 2 of
  three.)
- **A long tool result renders whole, and the transcript scrolls.** A
  coder turn is mostly tool output and a `cargo build` runs to thousands of
  lines; there is no print-time cap and no cut — the tool-result cap that
  once capped a result to one screenful on the live tail is deleted
  ("What went"). Scrolling up (`Up`, `PageUp`, or `Ctrl+A [`) is how the
  rest of a long result is read, the way `less` would. The divider still
  survives however far the result runs — a result you cannot attribute is
  worse than a long one.
- `▸` is a collapsed block; only `Error` collapses by default (tool output
  prints whole, guidance 7) and
  `Error` is a one-line stub, per the app's error-render policy. Collapse is
  kernel state (`CollapsedChanged`), so a sibling's expand is yours too. A
  completed `Thinking` block is the one block that prints as a `▸` stub
  regardless ("The thinking pane").
- `Thinking` streams dim and italic in the thinking pane and leaves a
  `▸ thinking · N lines · …` stub in the transcript when it completes, a
  stub whether the view is scrolled or not; `kj block read` is what reads
  the whole text once the pane has closed ("The thinking pane").
- A `ToolResult` with structured output (`OutputData` — headers, a flat list,
  a tree, `rich_json`) lays out at the terminal's real width in the tui: a
  table, `ls -C` columns, or an indented tree (`layout::layout_output`).
  kaish owns the data, the tui owns the layout; `kaijutsu_present::format`'s
  one-name-per-line string is the fallback for plain text and for any client
  that has no width to lay out against.

### Compose

```text
  ❯ and getattr? the symlink case too, and whether rename shares the
    cause — run vfs::unlink_symlink before touching rename_

  0 kaijutsu*  1 kaish@  2 lfm2d  3 exo        │  -- INSERT --  17.3/128k  91%  4m
```

Compose is a modalkit `VimMachine` over the kernel-owned input block
(`edit_input` / `submit_input`), as the app's compose overlay is. The draft is
a shared block: a sibling's typing shows. `Enter` in normal mode submits;
`Esc Enter` is the way out of insert mode and into a submit
(`compose::Compose::press`). A second `Esc` is
harmless: compose always holds the keyboard. On a submit the
tui sends the newest block the transcript showed as the player's edge, with
its character count as rendered (`ContextView::edge`, `docs/prompts.md`,
"The submit verb"). A context that has shown nothing at all sends no edge.

Rules the figure carries:

- The vi engine is `kaijutsu-editor`'s `EditorCore`, the same pure modalkit
  core the kernel's vi sessions run on. Its `EditOp`s are char-indexed, which
  is `edit_input`'s `(pos, insert, delete)` addressing exactly, so one
  keystroke is one `edit_input` and nothing translates between the two. Keys
  reach it as `crossterm::event::KeyEvent` through
  `EditorCore::apply_key_event` — the vim-notation string cannot carry a
  literal `<`.
- A fresh draft rests in normal mode, as vim opens a buffer; `i`, `a` or
  `o` start typing, and a submit returns to normal mode. A resumed draft
  opens with the cursor on its last character, so `a` continues it. The
  mode is the status line's first figure on the right, vim's own spelling,
  normal included: vim leaves normal mode blank, and a blank was the one
  mode a player could not tell apart (Amy: *"I can't tell which mode I'm
  in besides INSERT"*). It took the model name's place there (Amy,
  2026-09-04: *"drop the model name in the status bar for now and put the
  vi mode there instead"*); `kj context info` still names the model.
- **The sibling's edit wins only when it is newer.** The draft is redrawn from
  the change feed, and `edit_input` acknowledges the same context version the
  feed speaks, so a mirror older than this client's last ack is refused rather
  than applied — otherwise our own echo, arriving one keystroke behind, would
  delete what was just typed.
- **A long line wraps, and the band grows for the draft.** A logical line
  wraps at the width by character, as vim wraps, and continuation rows
  indent under the prompt; Enter in insert mode is a newline. Every row
  past the first grows the band by one, taking that row from the
  transcript area, up to a third of the screen (`render::third_of_screen`,
  the same ceiling the thinking band takes): the transcript is redrawn one
  row shorter, never rebuilt or scrolled to make room, and a grow tied to
  a keystroke reads as the line editor growing, not as a jump. Past the
  cap the draft scrolls around the cursor, as vim's command line does. The
  band shrinks once, when the draft is submitted or cleared — never
  gradually — and the transcript simply gets its rows back on the next
  frame (Amy, 2026-09-04: *"the next turn or tool that scrolls would have
  it shrink back maybe gradually or would that get jittery?"* — it would).
- **The cursor is the terminal's own.** The tui puts the real cursor on the
  draft's vi cursor — past the `❯` prompt, past the matching indent on a
  continuation row — or after the `:` bar's text, and shapes it by mode as
  vim does in a terminal (`DECSCUSR`, `t_SI`/`t_EI`): a steady block in
  normal mode, a bar while inserting and on the `:` bar, an underline while
  replacing; the editor's alternate screen shapes it by its own buffer's
  mode. No painted cell stands in for it, so the terminal's own cursor
  color applies. The armed legend, the picker, an ask card and the ledger
  hide it. The shape goes back to the terminal's default on `:q` and on
  `Ctrl+Z`.
- **The terminal cursor hides while scrolled.** The draft keeps drawing
  live off the tail — typing snaps back, so it never changes underneath —
  but the keys belong to the transcript there, not the draft, so the real
  cursor is hidden for as long as the view is scrolled ("Copy mode (scroll,
  or `Ctrl+A [`)").
- **`Up` at the draft's top line leaves the tail; `Down` at the live tail
  does nothing.** A one-line draft is the common case, so every `Up`
  scrolls the transcript one line; inside a taller draft `Up` moves the
  draft's own cursor first, as vim does, and only scrolls once the cursor
  is on the first line. `PageUp` always leaves and scrolls a screen; the
  `:` bar keeps `Up`/`Down` for its own history throughout. One rule covers
  both a typed arrow and the mouse wheel, which the terminal sends as
  arrow-key presses (`docs/tui.md`, "The mouse stays the terminal's").
- **A paste is text, not keystrokes.** Bracketed paste is on while the
  viewport is up, so the terminal delivers a paste as one event: the draft
  takes it as one edit at the cursor, the way `Ctrl+A ]` does, and a
  newline inside it is a newline in the draft, never an Enter that submits
  the first line. The `:` bar takes it flattened onto one line. An open
  editor session takes it as one `editorInsert` call at the cursor, leaving
  the session's mode untouched — the editor's `editor_keys` notation cannot
  carry a literal `<`, so a paste there rides its own wire method instead
  of forwarding as keys. While the editor's own `:` line is open the tui
  refuses with a notice, since the strip draws that line in preference to
  the kernel's refusal message. The picker, the ledger, an ask card and a
  frozen diff refuse it with a notice — none of them is wired for one. Line
  endings are normalized, since terminals differ on what a pasted newline
  is. Probe: `a_bracketed_paste_lands_in_the_draft_without_submitting`.
- There is no state past normal mode. The app's `Esc Esc` hands the
  keyboard to its block list; the tui redraws its transcript every frame
  from the mirror rather than printing it once into scrollback, and
  reading it from the keyboard is scrolling, which is copy mode, not a
  block cursor — the unfocused state that reserved room for one was
  deleted (Amy: *"I don't think we need the unfocused mode at all"*). Vi
  motions in normal mode act on the draft. `:` opens the bar from normal
  mode, and the bar
  closes into normal mode.

### The thinking pane

```text
  ─ claude · coder ─────────────────────────────────────────────── 14:02:11
  The unlink bug was in resolve(): it canonicalized the final ▍

  The unlink bug: resolve() canonicalizes the final component, so the
  symlink's target is what gets removed. Check resolve_nofollow first,
  then whether rename and getattr share the cause. The test that would
  show it is vfs::unlink_symlink; run that before touching rename.

  ❯                                                                -- NORMAL --
  0 kaijutsu*  1 kaish@  2 lfm2d  3 exo        │  -- NORMAL --  17.3/128k  91%  4m
```

…and once the block completes, scrollback holds one line in its place:
the kernel's summary, computed once at completion. The pane wraps the block
for the screen; the summarizer reads the block's own text, whose first
sentence runs to "removed":

```text
  ▸ thinking · 14 lines · The unlink bug: resolve() canonicalizes the final component, so the symlink's target is what gets removed
```

Reasoning pops up while the turn runs and gets out of the way when the
turn is done, without ceasing to be findable (Amy: *"those would pop up
while thinking runs then get out of the way, but still be findable"*).

Rules the figure carries:

- **The pane is the turn's, not the block's.** It opens at the first
  `Thinking` block of a turn this client knows is running (`turns_running`,
  the partial signal under "The `:` line") and holds until that turn ends.
  A fast model thinks for 400 ms; a pane that closed with the block was a
  flap that redrew the whole screen twice per block, measured on
  deepseek-v4-flash (`App::observe_thinking` is the latch, cleared by
  `mark_turn_ended`; a block that completed inside one delivery latches
  too, as long as its stub has not printed).
- **The pane is an overlay of its own rows, a third of the screen at most**
  (`render::thinking_pane`, `render::thinking_band_lines`,
  `render::third_of_screen`: 8 rows on a 24-row terminal, 20 on a 60-row
  one). It sits at the foot of the transcript area, below the streaming
  answer and above the band, and the latest reasoning's tail fills it, dim
  and italic. Because the pane is its own region rather than part of the
  transcript, the answer streaming in above it never scrolls the reasoning
  away, and a later thinking block in the same turn replaces the earlier
  one in place. One size, taken once and given back once.
- The turn-liveness half is what closes a pane a lost turn would otherwise
  hold open: a block left `Running` by a dropped stream shows in the
  ordinary band, never in a stuck pane (`forget_turn_liveness` clears the
  latch with the flags).
- **The block completing prints the stub**, in document order, so
  scrollback reads in sequence while the pane still holds the text: one
  `▸ thinking · N lines · <summary>` line (`present::thinking_stub_line`),
  where `<summary>` is the kernel's extractive summary of the block
  (`kaijutsu_types::summarize_thinking`, stamped on the block when it
  settles), with the block's own first line as the fallback when no
  summary exists. Thinking blocks leave the stream while the pane holds
  them, so nothing draws twice.
- **The reasoning is not readable in a copy buffer.** There is no second
  buffer to render it whole into: the scrolled view is the same transcript
  the live tail draws, and a completed `Thinking` block is one `▸` stub
  there regardless of scroll position, collapse state ignored for
  `Thinking`. `kj block read` is the way to read the settled reasoning
  whole once the pane closes.
- No tui-side truncation, no kernel budget: what prints is what the
  provider sent (Claude's adaptive summarized thinking is the API's own
  summary; DeepSeek gets `reasoning_effort`).

### The in-flight strip

```text
  ─ deepseek-v4-flash · coder ─────────────────────────────── 07:27:16
  Now that is the interesting rule — I hit a wall with a clean ▍

   ◐ shell cargo test -p kaijutsu-kernel · 4s   ⏳ shell_write · waiting on ask 01a0686d · 17h

  ❯                                                          -- NORMAL --
  0 ROOT  1 cc-exomemory  2 0de73794  3 tui-testing   │  -- NORMAL --  17.3/128k  99%  0m
```

One row, always present, directly above the blank row over `❯`: every
tool call of the current context that has not settled, as one region
each — `◐ <tool> <arg> · <elapsed>` while it runs, `⏳ <tool> · waiting on
ask <id> · <age>` while a gate holds it. The row is a faint ground so it
reads as a row when empty; each region is a second tint, cyan for running
and yellow for waiting, so its extent is visible.

Rules the figure carries:

- **The band is a static height.** `render::BAND_ROWS` is 4: the strip's
  row, the blank row, one draft row, the status line — a wrapped draft
  takes more, up to a third of the screen ("Compose"). A tool call coming
  or going changes the strip's text and nothing else — never the band's
  height, so the transcript never shifts for it. The thinking pane is the
  one overlay that takes rows from the transcript area for its own reason,
  once per turn (`render::band_frame` builds the band; `render::overlay_lines`
  is separate). (Amy, 2026-09-04: *"the screen seems to be jumpy with the
  tool call pinning like that"* — every unsettled body used to sit in the
  band with its divider, and wrapped or completed at its own pace.)
- **What leaves the stream:** an unsettled `ToolCall`, and a `ToolResult`
  a gate holds (`waiting`/`pending`). A `Running` result stays in the
  stream, because that is the tool's output streaming in and Amy wants to
  watch it. Bodies print to scrollback when they settle, as before.
- **The entry is the call's**; its result refines it through
  `tool_call_id`. A held result whose call is gone is an entry on its own.
  The ask id is read from the gate's own text (`ask <uuid>`); the seat's
  `!` and the ask card carry the rest.
- Entries are clipped with `…` to the width, never wrapped: the row is one
  row at any width.
- **A running entry moves.** Its spinner turns (`◐ ◓ ◑ ◒`) and its ground
  breathes one shade, one step every 250 ms (`inflight::PHASE_MILLIS`),
  and its elapsed time counts. The phase is a pure function of the clock
  (`inflight::phase`), and the event loop redraws on that step only while
  a running entry exists — a held entry is still, and an empty strip costs
  no redraws at all.
- The machinery: `inflight::entries` over the mirror's unprinted blocks,
  `inflight::strip_line` for the row, `render::band_frame` places it.

### The `:` line and the `Ctrl+C` ladder

Amy, after an evening on the first cut: *"let's think through MVP for :,
reclaim ctrl-c for interrupts."* Her guidance the same evening: *"all three rulings
yes"* (`/` retires from compose, `:!` shares the shell history, `:q` is the
only quit); *":q should warn if there's still active turns, :q! exits and
leaves them going in the kernel"*; *"Ctrl-Z can probably go away in favor of
the :!"*; and on the turn-liveness signal, *"fine to do simple :q if we
don't have a clear signal."* Built the same lane.

**What exists.** Compose runs `kaijutsu_editor::EditorCore`, and the core
already has the command bar: `:` in normal mode focuses it, `command_line()`
returns the text to draw (`":wq"`), Enter parses into `CommandRequest`s that
`take_commands()` hands back, and an unknown command comes back as `Err`
(vim's "Not an editor command"). The core's own dialect is the editor's:
`:w :q :wq :x` with `!`, `:s`, `:r` — that dialect answers the
alternate-screen editor's own `:` bar; this parse is a different one, over
compose's bar.

```text
  :kj fork --name alt              run kj, blocks land like a player's own
  :!git status                     one kaish statement, the gated human path
  :q                               quit, unless a turn is known running
  :kj con█                         the bar draws on the compose row
  :!git status█                    a shell line, drawn as typed
```

- `:` in compose normal mode draws the bar on the compose row from
  `command_line()`; `Esc` aborts, `Enter` submits, discarding what was
  typed. The bar draws the line as vim draws its own: the `:` and then the
  text, no gap, `:!` as typed, no shell glyph. The `❯` prompt leaves with
  the draft row. (A first cut drew `: kj con` and `$ git status`; Amy: *"go
  with pure vim, no space and no $"*, and *"I'd like to mostly match it
  where it lines up."*) The seam: `Compose::press` peeks `command_line()` **before**
  feeding the key to the core's own `apply_key_event` on `Enter` — the raw
  line as typed, ahead of the core's own ex-command dialect parsing it. The
  core still runs its own parse on the same keystroke (closing the bar the
  way it always did); its `Ok`/`Err` is drained via `take_commands()` and
  never surfaced — the core's `Err("Not an editor command")` for a line like
  `:kj fork` is not this parse's failure.
- Three verbs, whitespace-split (no shell-style quoting — MVP):
  `:kj <argv>` runs through `execute_kj` (a player's command, so it authors
  its block pair); `:!<statement>` runs through `shell_execute`, the gated
  path a key press already took on the old shell surface, and joins its
  history; `:q` quits unless this client knows of a turn still running,
  which warns instead (`:q!` quits regardless — the turn keeps running in
  the kernel). Anything else is a status-line notice naming the line
  verbatim, and the draft is untouched.
- The bar keeps a local history (`compose.rs`'s `ColonHistory` — the core
  surfaces no history hook of its own, so this is hand-rolled the way
  `shell.rs`'s used to be), walked by `Up`/`Down`. `:kj` and `:!` lines
  share it, and it survives a chat submit and a context switch — a session
  fact, not a per-draft one.
- Completion moved from `/` to `:kj `, over the same catalog, driven by the
  same `Tab`. `/` retired from compose: a draft is always chat, and there is
  one command syntax.

**`Ctrl+C` reclaimed.** `interrupt::Ladder` ports the app's escalation
(`kaijutsu-app/src/input/interrupt.rs`'s `TapCounter`), reimplemented rather
than pulled in as a dependency — the terminal client does not otherwise carry
the app's Bevy stack. One ladder per client, 500 ms window:

| presses within 500 ms | call | notice |
|---|---|---|
| 1 | `interrupt_context(ctx, immediate = false)` | `interrupting after this tool call — Ctrl+C again to abort` |
| 2 | `interrupt_context(ctx, immediate = true)` | `aborted` |
| 3 | the above, then `edit_input` clears the draft | `aborted, draft cleared` |

With no turn known running in the context on screen, the first press posts
`nothing to interrupt — :q quits` and calls nothing. `Ctrl+C Ctrl+C` no
longer quits — `:q` is the only quit. `Ctrl+A q` stays close-and-demote
(`docs/input.md`, "The prefix table"), so it is not a quit either.

**Turn liveness is a partial signal.** `App::turns_running` is set when this
client submits (`compose_key`'s own submit) or when
`ServerEvent::TurnStarted` names a context, and cleared on
`TurnCompleted`/`TurnFailed` for that context. An interactive submit from
*another* client or peer announces no start this client can see unless it is
already watching that context, so `nothing to interrupt` and a clean `:q`
can both be wrong about a turn someone else started. That gap is what Amy's
"fine to do simple `:q`" guidance accepted rather than building a
cross-client turn ledger for it. The flag is forgotten wholesale when the
event stream is known broken — a broadcast lag (`RecvError::Lagged`) or the
connection leaving `Connected` — with a notice saying so, because the
stream is the only thing that clears it and a flag nothing clears would
make `:q` refuse forever.

**The `Ctrl+Z` shell surface retired in favor of `:!`.** `shell.rs`,
`Intent::ShellToggle`, `CtrlZ::Toggled` and `app.shell` are gone — deleted,
not deprecated (`CLAUDE.md`, "Kaijutsu is the learning space"). `:kj` and
`:!` both always act on the context on screen, the same context the old
shell surface always acted on too, so there is no separate "acting context"
cursor left to show — the bar draws no context label at all. `Ctrl+Z` is a
single-press suspend now, not a toggle: leave raw mode, raise `SIGTSTP` on
ourselves; on `SIGCONT` re-enter raw mode and redraw the viewport. Nothing
else needs restoring — the inline viewport leaves the transcript in
scrollback and the host shell's prompt appears under it; `fg` brings the
instrument back. Over `ssh -t zorak kaijutsu-tui` that puts zorak's login
shell one keystroke and one `fg` away.

**Every way out restores the terminal.** `run::restore_terminal` is the one
place the exit sequences are written: give the session screen back if the
session took it (`editor::abandon`, guarded by the `ENTERED` flag
`editor::take_screen` sets, because xterm restores a saved cursor on
`?1049l` even when the alternate buffer was never in use), turn focus
reporting off (`DisableFocusChange`), pop the xterm title stack (`CSI
23;0t`), reset the cursor shape, turn bracketed paste off, leave raw mode.
`suspend` does the same pair — `DisableFocusChange` and the title-stack pop
— around the `SIGTSTP`, then re-enables focus reporting and pushes the
title stack again on the way back, vim's `stoptermcap`/`starttermcap`
order. `:q` reaches it
through `leave_terminal`; a panic on the loop thread, outside any task,
reaches it through the hook `run` installs before the screen is taken, so
the message prints on a cooked main screen; `SIGTERM` and `SIGHUP` reach it
by ending the loop the way `:q` does. A panic inside a `spawn_local` task
nobody joins — `Feeds::pump`'s forwarder, a hydrate round — does not unwind
the loop, so the hook must not restore in place there
(`run::panic_unwinds_the_loop`); it records the panic instead and the loop
ends on it the next iteration, reaching `leave_terminal` the normal way.
The key reader thread stops before raw mode
goes, so keys typed at the prompt while the connection tears down reach
the shell. Diagnostics never touch the screen: when stderr is the terminal
they go to `kaijutsu-tui/tui.log` under the state directory, a redirected
stderr is used as given, and `--log` names the file. Probes:
`tests/terminal_fit.rs`, "Every way out restores the terminal" — `:q`,
`SIGTERM` from copy mode, a panic from copy mode
(`KAIJUTSU_TUI_PROBE_PANIC` makes `F12` panic), and a task panic
(`KAIJUTSU_TUI_PROBE_PANIC=task`) each leave the pty on the
main screen with a cooked line discipline.

**Probes** (`tests/terminal_fit.rs`): `:` draws the bar visibly while
typing, and `Esc` discards it without reaching the draft (the receipt for
the pre-lane "a bar nobody can see and every key after it goes there" bug);
`:q` and `:q!` exit 0; `:kj context list` and `:!echo hi` land real blocks,
and `:kj context list` lands after a second `Esc` too (where the first live
test stalled, when a second `Esc` still meant something);
a partial `:` line never repeats into scrollback; one `Ctrl+C` posts
`nothing to interrupt` and does not quit, two within the window still do
not quit; `Ctrl+Z` suspends and `SIGCONT` leaves a responsive client (the
stopped state itself is not observable in every sandbox — the probe's own
doc comment says why). Slice 5's own probes name the same file:
`focus_reporting_is_taken_with_the_screen_and_its_reports_are_not_keys`,
`the_window_title_follows_the_context_and_is_given_back`, and
`an_ask_notifies_the_desktop_only_while_unfocused`.

### Copy mode (scroll, or `Ctrl+A [`)

tmux's own copy-mode chord (`.tmux.conf`'s `mode-keys vi`, `bind [
copy-mode`), and it means the same thing here: leaving the live tail is
entering copy mode, and reaching the tail again is leaving it
(`docs/tui.md`, "Scrolling is copy mode"). `Ctrl+A [` leaves without
moving; `Up` at the draft's edge, `PageUp`, and the wheel arriving as
arrow keys leave and scroll the same way. The transcript is not frozen and
not full-screen: it is the same scrolling view over `ContextView`'s mirror
that the live tail draws, now under vi motions — how a long tool result is
read whole now that tool output no longer collapses by default
(`present::collapses_by_default`), and how the conversation is scrolled
from the keyboard.

```text
  ─ claude · coder ──────────────────────────────────────────────  14:02:11
  The unlink bug was in resolve(): it canonicalized the final component,
  so the symlink's target was removed instead of the link. resolve_nofollow
  fixes unlink; rename and getattr share the cause and are deliberately ▍
  ▸ shell  cargo test -p kaijutsu-kernel vfs::                     running 4s

  ❯ and getattr? _                                                  -- INSERT --
  line 1204/1207   j/k  ^D/^U  gg/G  / search  v mark  y copy  q leave
```

The reader's line and a marked range paint over the transcript rows
(`copy_cursor`, `copy_selection` in the palette); the position and the key
hint take the band's own status row, in place of the ordinary status line,
and a `/` or `?` prompt takes that same row while it is being typed
(`copy_search` highlights a match). The draft stays drawn live above it —
typing snaps the view back to the tail, so it never changes underneath —
but the terminal's own cursor is hidden while scrolled, because the keys
belong to the transcript, not the draft.

Rules the figure carries:

- **Entry position is the bottom** — the newest block, the reader on its
  last row — the way tmux enters copy mode at the current screen. Leaving
  the tail never prints anything and never moves the view: `Ctrl+A [`
  anchors exactly where the following view already sat; `Up` and `PageUp`
  anchor there too and then scroll one line or one screen.
- **There is no frozen snapshot.** A block still streaming grows under the
  reader while they scroll, and the view keeps its place by block and line
  rather than by row (`copy::Anchor`), so growth above or below never moves
  what is being read. The buffer being the tui's own — holding the whole
  context, so search crosses all of it, not only what was printed since
  attach — is one of the two things guidance 7 names as different from
  tmux; the other is that the mouse wheel stays the terminal's, arriving as
  arrow keys under alternate scroll (`?1007`) rather than a reported wheel
  event.
- **`Up`/`Down` scroll the view one line, the reader's line pinned to its
  screen row** — one wheel tick is three of them, and moves the screen
  three lines. At an edge the view cannot move, so the reader's line moves
  instead: the first and the last row stay reachable with the arrows
  alone. `j`/`k` are vim's cursor motions instead: they walk the reader
  and scroll only once it is on the top or bottom row.
  `Ctrl+U`/`Ctrl+D` move half a screen, `Ctrl+F`/`Ctrl+B`/`PageUp`/
  `PageDown` a full screen; `gg`/`Home` go to the top; `G`/`End`, or any
  downward move past the last row, return to the live tail.
- `/` and `?` open a search prompt on the band's status row —
  case-insensitive substring is enough, and it takes every key while open.
  `Enter` commits and jumps to the nearest match in that direction; `n`/`N`
  step to the next/previous match, wrapping around the whole transcript
  once it runs out. Every row holding a match is highlighted; the reader's
  own row is painted as the reader's row first.
- `q` and `Esc` leave copy mode and return to the live tail with a redraw.
  Leaving never prints anything — the transcript is read-only there, and
  returning to the tail is a view change, not a transcript event.
- `v` marks the reader's line, the vim spelling copy mode carries; a second
  `v` clears it. `y` or `Enter` yanks the marked range — or the reader's
  own line when nothing is marked — to the clipboard over OSC 52
  (`ESC ] 52 ; c ; <base64> BEL`, written straight to stdout under the same
  `term_lock` every other terminal write takes) and into `app.paste_buffer`
  for `Ctrl+A ]`, then returns to the tail. `Esc` leaves whether or not a
  mark is active; a second `v` is what clears a mark.
- **Every other key snaps the view to the tail and is handled as if typed
  there** — `Space` most of all (Amy: *"I often hit space just to do
  that"*, the habit wezterm calls `scroll_to_bottom_on_input`); `v` marks
  now, so `Space` no longer does. See "Scrolling is copy mode" for the
  decision. The `Ctrl+A` prefix is the exception: both halves of the chord
  are claimed away from the scrolled transcript (`Keys::claims`), so a
  seat switch never snaps.
- **Each context keeps its own scrolled place across a switch** — the
  scrolled state lives on that context's `ContextView`, not on the screen,
  so a context left scrolled is still scrolled on return and one left at
  its tail is still at its tail (`docs/tui.md`, "The buffer").
- **`Ctrl+A ]` pastes the last yank into the draft** at the cursor, as one
  edit, in whatever mode the draft is in — tmux's `paste-buffer`. The
  buffer is the tui's own, so it never needs aligning with vim's registers
  or the OS clipboard (Amy: *"I always disliked trying to align copy
  buffers between vim/tmux/os"*). The same yank also goes out over OSC 52,
  one way; the tui never reads the OS clipboard back.
- **The machinery, named.** Nothing on the wire opens copy mode: it is a
  local decision over the `ContextMirror` this client already holds, the
  same source `render::transcript_window` reads — there is no RPC round
  trip to enter it. `copy::Scrolled` holds the reader's place, the mark and
  the search state; `render::row_index` maps a rendered row to the block
  and line it belongs to (`copy::RowIndex`) and back; OSC 52 is the
  clipboard write, and `app.paste_buffer` is where the last yank lives for
  `Ctrl+A ]`.

### The picker (`Ctrl+A "`)

The well, flattened. An overlay at the foot of the transcript area, taking
the rows it needs and giving them back on dismiss — no viewport growth,
the transcript simply gets fewer or more rows on the next frame.

```text
  ACTIVE
  0 kaijutsu   ● coder    running  4s    cargo test -p kaijutsu-kernel vfs::
  1 kaish      @ coder    idle     2m    arithmetic: `test` is back, `[` stays banned
  2 lfm2d        toolie   idle    41m    export plan waits on Amy's go
  3 exo          coder    idle     3h    daily written, 2026-08-30
  RECENT
    kaibo-batch  mcp      idle     1d    OpenAI Files→Batch still 404s
    scratch      coder    idle     3d
  +47 beyond the horizon                                         / filter  h dive
  TRACKS
  ▮ bass         120 bpm   17.3  ●○○○      ▯ click       120 bpm   17.3  ○●○○
```

Rules the figure carries:

- `j`/`k` move, `Tab` hops sections, `Enter` switches, `p d z a c` are the
  placement verbs unchanged, `/` filters, `h` opens the horizon as a filtered
  list. Digits address the ACTIVE seats, as in the well.
- A switch through the picker keeps each context's own scrolled place, like
  every switch (`docs/tui.md`, "The buffer").
- The live tail is the context's own last line (`live.rs` tail buffers,
  fed by the kernel-wide `ServerEvent` stream). `●` is chatter now, `@` is
  activity since you last looked — screen's monitor flags.
- TRACKS lists `listTracks` with a bar.beat counter and a pulse glyph driven by
  the per-track phasor.
- The list follows the kernel while it is open. A placement verb or a
  `:kj` line that ran (`App::roster_changed`) starts a refresh round now
  instead of on the next tick, and every round
  rebuilds an open picker over the new roster (`PickerModel::refreshed`):
  the filter, the open horizon and the archive confirm latch stay, and the
  cursor follows the context it was on — after `p` it sits on the same row
  in ACTIVE. A row that left the roster clamps the cursor into what remains.

### Asks

An ask arrives through `subscribeLedgerEvents` and is answered through
`kj ledger allow|deny`. It renders as an overlay, never as a modal that
steals the transcript. Same overlay treatment as the picker: it takes the
rows it needs at the transcript area's foot, key line included, so a long
statement never pushes `[a]llow once ...` off the bottom — the key-hints
line is always on screen because the overlay takes exactly the rows it
needs rather than being cropped into a fixed region.

```text
  ⚠ ask 01a04eb6  shell_write  from kaijutsu (coder)  asker coder  reviewer amy
    rm -rf ~/src/wt/kaish-arith
    [a]llow once  [A]llow always  [d]eny  [v]iew ledger  Esc aside
```

While the card is up, `a`/`A`/`d`/`v` answer it and typed text is held:
the draft never changes under a card, so a decision key and a typed
letter are never confused. A `Ctrl+A` chord, `Ctrl+C` and `Ctrl+Z` act
exactly as they do with no card up — `Ctrl+A 4` still switches seats, and
the card goes aside with the switch, since it is always the current
context's ask; the next refresh raises it again on return. With the
prefix armed, `Ctrl+A d` is the chord `d`, never a deny. (Until
2026-09-06 the card took every key, which is why `Ctrl+A <digit>` and the
picker "did not always work": the advisory gate raises a card often.)
`Esc` puts the card aside with the ask still pending: the seat keeps its
`!`, the status line its `!n`, and `Ctrl+A l` reaches it. The card also comes down by itself when its ask
leaves the pending set — answered from another surface (`kj ledger allow`
in a shell, the app, a sibling session), expired, or abandoned — with a
status-line notice saying what became of it: `ask 01a04eb6 allow once by
you`, `by 2b1ffa32e069` (a principal's short id), or `expired` — the ask
by its first id segment, the one `kj ledger list` keys on. A key
pressed on an already-answered ask reports the lost race on the status
line and nothing else happens.

- An ask landing while the terminal is unfocused notifies the desktop
  once — OSC 777 and OSC 9 (`asks::ask_notification`); one already on
  screen notifies nothing. Testing note: the ephemeral kernel this
  probes against starts with no reviewer character sheet, so raising an
  ask needs `arrange_a_reviewer` first — `kj binding allow config-write`
  from ROOT, then `kj character create amy` — before any ask can be
  raised at all.

**The assigned reviewer answers in the context on screen.** An ask records
its requester, actor, and reviewer. The actor cannot approve it from any
context, unless it is the assigned reviewer too — a self-confirmation,
which shows its approval keys to that actor (`docs/approval-identity.md`).
The reviewer can approve it in the work context, including the context that
raised it. A card for another player shows its asker and
reviewer and offers cancellation or escalation guidance instead of approval
keys.

The card's line count is measured at the terminal's own width — the
statement wraps by width, so a wider count would say fewer lines than a
narrower terminal actually needs. The ledger's row count does not need
this: its rows truncate rather than wrap, so it is measured once at
`u16::MAX`, the picker's own pattern. Only a terminal shorter than the
overlay — smaller than the whole card or ledger needs — falls back to
cropping, and it crops from the top: the key line is the last thing
either view renders, so it is the last thing to disappear.

### The ledger (`Ctrl+A l`)

The ask above is one row of a view you work from between sessions: every
pending ask across every context, then the recent decisions with their
redemption. Same overlay treatment as the picker and the ask card, same
single-key answers, backed by `kj ledger list` / `show` / `allow` / `deny`
and the `redeemed:` field.

```text
  LEDGER                                                    pending 2   answered today 7
  PENDING
  ! 01a04eb6   12s   kaijutsu   coder    shell_write   git worktree remove --force ~/src/wt/kaish-arith
  ! 01a04ec1    4m   lfm2d      toolie   file:write    ~/exomemory/lfm2d/work-machine-export.md
  ANSWERED
    01a04eaa  13:58   kaijutsu   allow once     amy   redeemed 13:58   git worktree remove …
    01a04e91  11:20   exo        deny           amy   —                cat /config/kernel/backends.toml
    01a04e77  09:02   kaish      allow always   amy   redeemed ×3      cargo test -p kaish-kernel
  a allow once  A allow always  d deny  Enter show  j/k move  / filter  Esc back
```

Rules the figure carries:

- The status line carries the pending count as `!n` next to the rank, so an
  ask in a context you are not looking at is visible from anywhere; the seat
  it belongs to carries `!`.
- `Enter` shows one ask in full (`kj ledger show`), including the hook that
  raised it and the statement as the gate saw it.
- Answered rows keep the answering principal and redemption. Redemption means
  the answer was consumed; it does not prove execution. An invocation retired
  before publication keeps its reviewer's decision. The TUI's full ask detail
  and the app's recent ledger rows show `publication abandoned` and the kernel's
  reason, including that source did not run. The TUI offers only
  `Esc back` for a terminal ask.
- `Ctrl+A l` is in the app's prefix table (`input/prefix.rs`; the Bevy
  surfaces shipped 2026-09-12 as `ui/ask_sheet.rs` and
  `ui/ledger_ribbon.rs`). The app's ribbon shows `recent n` rather than
  "answered today": its `LedgerMirror` keeps the last three asks that left
  the pending set, and nothing on the wire tells a client how many were
  answered today. Its rows carry ages (`12s`, `4m`) rather than wall-clock
  times, which is the app's convention everywhere else.

### Status line

Screen's window list, with kaijutsu's facts on the right:

```text
  0 kaijutsu*  1 kaish@  2 lfm2d  3 exo  │  -- NORMAL --  17.3/128k  91%  4m  17.3 ●
```

Left to right: the rank (ring 0, seat digits, `*` current, `@` activity, `!`
an ask waiting in that seat, `!n` the pending count across all contexts);
`│`, the one separator, always left of the mode so the eye finds it in
the same place; the vi mode (`-- NORMAL --`, `-- INSERT --`, vim's
spelling); the last completed call's tokens over the model's window,
`17.3/128k` — both in thousands, the unit once, trailing by one call and
never an estimate (the kernel does not tokenize locally), warning at 75%
of the window and alarm at 90%; cache health (next); bar.beat and pulse
for the playing track; and connection state from `ConnectionStatus` only
while it is not `Connected` — a healthy connection says nothing,
`◐ connecting`, `◐ retrying` and `○ offline` do (Amy: *"where 'ok' is,
that's not super useful"*). No icons on the right half (Amy, 2026-09-04:
*"let's try without for now"*); the `▮ 42%` occupancy figure is gone,
the token count says the same thing in the units we think in.

- The pulse and the spinner stop while the terminal is unfocused
  (`render::strip_animating`, `docs/tui.md`, "What owning the screen lets
  us use").

### Cache health

Two figures, always shown, from the last completed LLM call of the current
context (Amy, 2026-08-30: *"how long since the last api turn; a proxy for KV
health when we have no other info … for now let's focus on exposing the data
we have"*):

- **`4m`** — age of the last completed call, in whole minutes (Amy,
  2026-09-04: seconds ticking beside the prompt were *"a lil
  distracting"*). When the cache TTL of that call is known the segment
  turns warning color past 80% of it and reads `6m ✗` once past it. With
  no TTL known (DeepSeek, a local model) the age stands alone — an age is
  never dressed up as an expiry. No icon; the position says what it is.
- **`91%`** — the cached share of the last call,
  `cacheReadTokens / contextUsedTokens`. `—` when either is unknown. The
  denominator is the last call's whole fill (input + output) because the wire
  carries no separate input-token count; an `inputTokens` field beside
  `cacheReadTokens` is what would make it the prompt share exactly.

The wire carries all of it on `ContextHandleInfo`, beside `contextWindow` /
`contextUsedTokens`: `lastCallAt` (unix milliseconds of the last completed
call), `cacheReadTokens`, `cacheWriteTokens` and `cacheTtlSecs`. The TTL is
the longest `CacheTtl` among the request's cache breakpoints
(`llm/stream.rs`: `Ephemeral` = 300, `Extended` = 3600), recorded on the
usage row at call completion; it is `0` when the provider path never emits
a `cache_control` (DeepSeek, a local model) or the request had no
breakpoints. `0` on any field means unknown, and `kaijutsu_client::ContextInfo`
decodes it as `None`. The TUI reads `list_contexts`; nothing is stamped
locally.

Estimators — a per-provider model of when a cache actually expires, learned
from `cache_read` falling to zero — come later and build on these fields;
this slice exposes, it does not guess.

### Editor and diff

`vi <path>` and `kj editor` open the kernel-owned editor session; the TUI
forwards keys through `editor_keys` and draws the pushed `EditorState`
(`kaijutsu.capnp`, `EditorState`). Both take the alternate screen and return
on `:q`. `kaijutsu-editor` drives modalkit on
`modalkit::crossterm::event::KeyEvent`, so the app's `keyconv.rs` translation
is the identity here.

**An open is a peer signal, not a push.** `subscribeEditor @79` carries
`onEditorState` and `onEditorClosed` only — `Kernel::editor_open_as` publishes
neither — so a client that watches that stream alone never learns a session
exists. `signal_open_editor` sends the `open_editor` peer invoke to the
*submitter principal's* attached peers, so the TUI attaches as a peer under
the nick `kaijutsu-tui` and serves that one action. A `vi` submitted by
another principal does not arrive: it goes to the `kaijutsu-app` fallback.
Exact-window targeting is `docs/vi.md`'s open item, and it is what would let a
terminal and an app window stop both popping.

**The wire carries a cursor, no selection anchor.** `EditorState` has
`cursor @2` and nothing else positional, so no client can draw a visual-mode
highlight — the TUI renders the mode label and no band. Adding a selection
range to the kernel's `EditorState` and to the capnp struct is step 1 of
`docs/vi.md`'s "Selection rects", and it is what unblocks both renderers at
once.

**Keys bypass the prefix.** While the alternate screen is up every key goes to
`editor_keys` verbatim, so `Ctrl+A` is vim's increment and `Ctrl+C` is vim's
interrupt — the editor is the sanctioned raw reader (`docs/input.md`). The
notation `kaijutsu-editor`'s `parse_keys` accepts is a literal char, `<Esc>`,
`<CR>`, `<BS>`, `<Tab>`, a space, and `<C-x>`; an arrow, a function key and a
literal `<` have no token, and are refused rather than sent and silently
dropped. The loop awaits one `editor_keys` call per key, so keystrokes cannot
reorder in flight and no ordering pipe is needed.

**Restart staleness gives the viewport back.** A kernel restart drops the
in-memory sessions while the persisted kernel id is unchanged, so the
reconnect looks ordinary and only `editor: no such session N` reports it. The
TUI leaves the alternate screen with a status-line notice on that verdict and
on nothing else, probes with an empty key batch when `ServerEvent::Reconnected`
arrives, and leaves on a `ConnectionStatus::Terminal`.

**Nothing on the wire opens a diff view.** `kj diff` authors a
`ContentType::Diff` block and each client decides for itself; the app's gesture
is `v` on a focused block. The TUI's are `Ctrl+A v`, which opens on the newest
openable block in the current context, and `kaijutsu-tui --diff <A> [B]`, which
runs `kj diff` through `execute_kj` and opens on its output. The open rule is
the app's `openable_diff`, ported: a declared diff always opens (an error state
when it will not parse), and `Plain` opens only when its text — or a leading
` ```diff ` fence — really parses. `q`/`Esc` closes; `j`/`k`, `Ctrl+D`/`Ctrl+U`
and `g`/`G` move.

### Images

Blocks already carry everything the wire needs: `ContentType::Svg` is inline
SVG text, `ContentType::Abc` is ABC notation, and a `ContentType::Image`
block's text is a CAS hash whose real MIME type lives in CAS sidecar metadata
(`kaijutsu-types/src/block.rs`, `ContentType`). Images are a client rendering
lane; nothing new rides the wire.

- **Symbolic on the wire, raster at the edge.** The kernel ships SVG, ABC, or
  a CAS hash — never pixels it rendered itself. The TUI rasterizes with
  resvg + tiny-skia (already workspace dependencies through the app) at a
  pixel size derived from the terminal's cell size, so the receiver renders
  the symbolic form at its own resolution — the same doctrine as
  tempo-not-pulses (`docs/midi.md`, "The one timebase").
- **Protocol ladder.** v1 emits the iTerm2 inline-image protocol (OSC 1337
  `File=`, base64 PNG); wezterm and iTerm2 both speak it, which covers both
  seats in use today. The fallback is unicode half-blocks (`▀` with fg/bg
  colors), which renders in any true-color terminal. Kitty's graphics
  protocol is a later rung for kitty/ghostty; sixel is not planned.
- **Detection is in-band, never environment.** Over `ssh -t zorak
  kaijutsu-tui` the process runs on zorak and `TERM_PROGRAM` does not
  propagate. Cell pixel size comes from `TIOCGWINSZ` (ssh forwards the pixel
  fields in pty-req and window-change), with a `CSI 16 t` query as the
  fallback; protocol support is probed with terminal queries at startup. When
  no protocol answers, half-blocks render.
- **An image redraws like any other block.** The transcript is rebuilt every
  frame from the mirror ("The owned screen"), so an image is emitted sized
  in cell units wherever the block sits in that frame's window — no
  write-once reservation, and a late edit or a resize redraws it exactly as
  it would redraw text. There are no streaming images — a still-streaming
  block renders as text until it completes.
- **The presentation crate stays pure text.** Rasterization and protocol
  emission live in the ratatui edge, beside the transcript printer. The
  `ratatui-image` crate covers detection and encoding for every rung and is
  worth an evaluation pass for those parts; emission stays in our printer
  either way, because the printer owns the terminal writes.

`Abc` blocks reach the staff through the same rasterizer, and need no new
emitter: `engrave::engrave_to_svg` (`engrave/svg.rs`) already renders a tune
to a self-contained SVG string, and `engrave/font.rs` caches a `path_d`
string per glyph, so `kurbo::BezPath` is not on this path at all. Its only
callers today are tests — a `ContentType::Abc` block reaching the terminal is
wiring, not new engraving. Lane in `docs/issues.md`.

## Keys

The prefix table in `docs/input.md`, "The prefix table", ports verbatim:
`Ctrl+A 0–9`, `Ctrl+A Ctrl+A`, `a` (a literal `Ctrl+A` into the draft,
screen's own `C-a a`), `q`, `"`, `w`, `'`, `A`, `n`/`p`, `d`, `h`, and the
armed-prefix legend line. `q`, `'`, `A`, and `d` are chords the prefix
already claims; the notice on the status line names what each will do —
switch by prompt, rename, close-and-demote, detach — until this client
builds them. `h` is not this client's yet either and has no notice of its
own: an unbound chord, named the plain way (`Ctrl+A h is not bound`). Three
chords are this client's own, with no app equivalent: `Ctrl+A v` opens the
diff viewer, `Ctrl+A l` opens the ledger, `Ctrl+A ]` pastes the copy-mode
yank buffer, and `Ctrl+A [` leaves the live tail without moving ("Copy mode
(scroll, or `Ctrl+A [`)"). The legend takes the
compose row while a prefix is pending — never the status line, whose seat digits are
what the player is about to press (Amy: *"by the time I read that, the
number was gone"*); there is no separate `?` overlay. `Ctrl+C` is the
interrupt ladder ("The `:` line and the `Ctrl+C` ladder"); it never quits.
`Ctrl+Z` is a single-press suspend (`raise SIGTSTP`; `fg` or `SIGCONT`
brings it back), not a toggle. `:q` (warns first if a turn is known
running) and `:q!` (quits regardless) are the only quits. Amy, on the
mockup: *"the legend in the status line is awesome, that'll help me a lot, I
tend to forget keys outside the core stuff I use."* Every overlay (picker,
ledger) ends with its own key line for the same reason.

**Off the live tail, the transcript owns its own keys** ("Copy mode
(scroll, or `Ctrl+A [`)"):

| Key | Does |
|---|---|
| `Up`/`Down`, `j`/`k` | scroll or move the reader a line |
| `Ctrl+U`/`Ctrl+D` | half a screen |
| `Ctrl+F`/`Ctrl+B`, `PageUp`/`PageDown` | a full screen |
| `gg`/`Home` | top |
| `G`/`End`, or a downward move past the last row | back to the live tail |
| `/`, `?`, `n`/`N` | search, step to the next/previous match |
| `v` | mark the reader's line |
| `y`, `Enter` | copy the marked range (or the reader's line), then the tail |
| `q`, `Esc` | back to the live tail |
| any other key | snap to the tail and handle it there (except `Ctrl+A` and its chord, which switch seats without snapping) |

**A key never waits on the kernel.** The rank, the pending asks and the
tracks are fetched on their own task (`refresh.rs`) and folded into the
app when the result lands; the event loop itself awaits no kernel call for
a refresh, so a kernel busy with a coder turn cannot queue keys behind it.
A key that asks the kernel for something (`:` verbs, a submit, a seat
switch) still waits for that one answer.

**Shared `bindings.toml`.** The TUI's bindings file is keyed by vim key
notation (`<C-a>`, `<Esc>`, `<S-Tab>`), which `modalkit::key::TerminalKey`
parses. The app's file today is keyed by Bevy `KeyCode` names
(`kaijutsu-app/src/input/bindings_config.rs`, `parse_key_code`). The TUI ships
the vim-notation schema first; converting the app is a follow-up lane. The
file's commentary is technical and reviewed by two flash-tier kaibo casts for
clarity before it lands (guidance 4).

## Time features

| App | Wire | TUI |
|---|---|---|
| The rank (ring 0) | `promotedAt`/`demotedAt` on `ContextInfo` | status-line window list; `Ctrl+A 0–9` |
| The rings | `list_contexts` + `assign_ring_seats` | picker sections ACTIVE / RECENT, `+N` horizon |
| Live-state layer: tails, chatter energy | kernel-wide `ServerEvent` stream | picker tails; `●`/`@` flags |
| Track rays, per-track phasors | `BeatSync` (`subscriptions.rs`), `listTracks` | TRACKS section; bar.beat + pulse in the status line |
| Room (3D stations) | — | not carried |

**Timing to music.** The ratatui loop is event-driven; the beat is one more
wake source. Arm a timer at the phasor's *predicted* next onset
(`LocalBeat::position` extrapolated at the current tempo), redraw when it
fires, re-arm at `scheduled + period` — never `actual_wake + period`. A missed
onset is missed; nothing is replayed. `docs/midi.md`, "The one timebase" is
the doctrine, and every rule there applies unchanged. A terminal over ssh
refreshes in 10–30 ms, which is fine for visual sync and for emitting cues and
is not a place for audio; if a TUI is ever a sink, that is the DJ thread's
extraction and a flag, not this document.

## What is reused, what is new

Reused unchanged: `kaijutsu-client` (22k lines, no Bevy dependency; ACP is the
proof it is toolkit-agnostic), `kaijutsu-editor`, `kaijutsu-diff`,
`kaijutsu-viz::layout`, `kaijutsu-audio::timebase`.

Lifted with a small de-Bevy into `kaijutsu-present`, the presentation crate
both clients consume — the thin-client move that falls out of this work:
`view/format.rs` (block → text), `text/markdown.rs` (pulldown-cmark →
`RichSpan`), `kaish/mod.rs` (syntax validation, already Bevy-free), the
`Action` enum, `WellBeats`.

**Color is the seam, and it is semantic.** A block resolves to a `BlockTone`
and a markdown span to a `SpanTone` — one variant per color a theme has a name
for, `BlockTone::User` through `BlockTone::Dim` — and each client turns a tone
into its own color type (`Theme::color_for` in the app, a ratatui `Style` in
the TUI). The resolver is a total function with no default arm, so a tone
added to the crate fails the client's match instead of painting a placeholder
nobody notices. The other three leaks were smaller: `Action` keeps `Reflect`
behind an off-by-default `bevy` feature (the app's `Binding` and `ActionFired`
nest it in their own reflected types), and `WellBeats` sheds its `Resource`
derive for an app-side newtype.

New, in ratatui: event loop and actor wiring, the transcript printer with a
per-block wrap cache keyed `(block, version, width)`, compose, shell surface,
picker, asks, status line, slash completion over the `KjCommandInfo` catalog,
editor and diff screens, keymap, `theme.toml` → ratatui styles. About 8k lines
for a usable first cut; five parallel lanes once the skeleton (event loop, app
state, the `Backend`-generic renderer, `present.rs`) lands.

Version note: `modalkit-ratatui 0.0.25` matches the workspace's modalkit pin
but wants `ratatui ^0.29`; ratatui is at 0.30. It is not a dependency and does
not need to become one — compose and the editor both render from state the
client already holds, so the ratatui widget crate buys nothing here.

## Roads not taken

**Kernel-served over an SSH PTY.** Feasible — russh 0.61.1 ships
`examples/ratatui_app.rs` doing exactly this — and declined. A TUI inside the
kernel process is a client with in-process reach into the block store, and it
would reach around "clients read over the wire and write through kaish" the
first time the wire was inconvenient; then app, ACP and TUI would show three
projections. It also puts frame rendering and widget panics inside the process
that must not play the turn. Today `pty_request`, `shell_request`,
`exec_request` and `window_change_request` are unimplemented on
`ConnectionHandler` (`kaijutsu-server/src/ssh.rs`) and stay that way. The
`Backend`-generic renderer keeps the door unlocked; the loopback `ActorHandle`
is what would keep a kernel-served variant honest.

**Bevy with a terminal frontend.** `bevy_ratatui` exists. Of the app's ~90k
lines, ~65k are vello/MSDF/Bevy-UI/3D and cannot render in a terminal; the
reusable logic is the ~3k listed above and is cheaper to extract than to host.
The sharing that matters is already done: the model lives in the kernel and
`kaijutsu-client`, and two thin renderers over one kernel is the instrument's
intended shape.

## Melted from the ssh shell design

`docs/ssh-shell.md` designed a `kaijutsu-shell` SSH subsystem: a line-mode
kaish loop starting in a lobby context. The TUI absorbs it — `:!` is that
loop, one statement at a time, over the RPC path that already exists, and
its picker needs no lobby because `list_contexts` is contextless. One
paragraph survives.

The design's other paragraph, *two cursors never mixed* (an acting context
set by `kj attach`, a cwd set by `cd`, each resolved live per statement),
retired with the `Ctrl+Z` shell surface: `:kj` and `:!` always act on the
context on screen, so there is no acting-context cursor to keep separate from
the cwd, and nothing for a prompt to render ("The `:` line and the `Ctrl+C`
ladder"). The cwd half still holds — `cd` inside `:!` moves the context's
durable cwd, the same one every other kaish path in that context sees.

**Principal is the Unix model.** The authorship lane (`BlockId.principal_id`)
is the authenticated user's principal. Two logins by one user are two ttys for
one uid — same lane, no per-session principal. The connection's `instance` /
`session_id` distinguishes `/v/session` rows and rides traces, and never enters
authorship.

If a raw kaish-over-ssh REPL is ever wanted again, its home is
`kaijutsu-tui --shell` — a readline mode of this binary — not a kernel
subsystem.

## Open

- Whether `RichSpan` should ride on `BlockSnapshot` next to `style_spans`,
  kernel-side, the way ANSI already does — consistent with "derived logic
  lives in the kernel", at the cost of a wire change. Not needed for v1.
- Scrollback staleness: the skeleton posts `block #12 changed after print`
  in the status line and never redraws. Whether a change also earns its own
  line in the transcript is still open.
- Internal splits. Set aside for v1; wezterm splits with
  `kaijutsu-tui --context <id>` cover it. Revisit when the itch is real.
- **The buffer question** (Amy, 2026-09-08). The inline model prints every
  context into one terminal scrollback that no terminal reports the depth
  of, and switching contexts interleaves them with no boundary. A printed
  marker row per switch and a per-context copy-mode view were tried and
  shelved: "marker and copy mode aren't gonna work." Two shapes remain,
  and the choice is a session of its own: (a) the tui is a
  **single-context app** that composes with a mux — tmux or wezterm owns
  windows, one tui per context, the terminal's scrollback is that
  context's alone; or (b) the tui **is the mux** and goes alternate-screen
  all the way, owning its buffer, scroll and search the way vim and tmux
  do, "but all modern like in rust". The switch path itself is one
  function now (`switch_seat`) whichever shape wins. **Decided
  2026-09-13: (b).** See "The owned screen".

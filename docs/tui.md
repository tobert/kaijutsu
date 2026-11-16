# The terminal client — `kaijutsu-tui`

`kaijutsu-tui` is a standalone binary that dials the kernel over the existing `kaijutsu-rpc` SSH subsystem and renders a terminal UI with ratatui. Run `ssh -t zorak kaijutsu-tui` and play. It is **not** a kernel subsystem, and it is **not** a Bevy frontend.

`--context <label>` joins that context or creates it, as a child of `--parent` or of the kernel's only live root context. A context is played by a character whose rc directory and model shape it. The default is the session's own character; a person's context runs no model, so a person's session names another with `--character <name>`. The `Ctrl+Z` context is the exception: the person performs it.

```text
terminal (wezterm, Blink, a plain xterm)
   │  keys + cells
kaijutsu-tui            ratatui inline viewport; modalkit for the vi surfaces
   │  kaijutsu-client   ActorHandle, DocumentStore/ContextMirror, ServerEvent
kaijutsu-server / kernel
```

## The owned screen

The TUI takes the alternate screen for the whole session, owning its transcript, scroll, and search.

### The mouse stays the terminal's
The TUI **never enables mouse reporting** (no DECSET 1000/1002/1003). With reporting off, the terminal keeps every mouse gesture: select, autocopy to clipboard and PRIMARY, right-click and middle-click paste, and OSC 8 link clicks. None of it needs a modifier.

**The wheel arrives as arrow keys.** On the alternate screen with reporting off, wezterm turns each wheel tick into arrow-key presses. One rule covers both: **`Up` at the top edge of the draft scrolls the transcript, `Down` at the live tail does nothing.** Inside a taller draft `Up` moves the cursor first, and scrolls once it is on the first line. The `:` bar keeps `Up`/`Down` for its history. `Ctrl+A [` enters copy mode outright, and `PageUp`/`PageDown` scroll by a screen.

### Scrolling is copy mode
Leaving the live tail is entering copy mode; reaching it again is leaving. The transcript area follows new blocks while the view is at the tail (`follow`), and stops following the moment it is scrolled. While scrolled, the transcript owns copy mode's own keys (vi motions, `/` and `?`, `n`/`N`, `v` marks and `y` or `Enter` copies, `q`, `Esc`, `G`). **Every other key snaps the view back to the tail and goes to the draft.** `Space` is the snap key (`scroll_to_bottom_on_input`). The draft stays live while scrolled, drawn as it is.

### The buffer
One transcript per context, rendered from that context's block mirror, wrapped at the current width on demand and re-wrapped on resize. The active ring's contexts stay resident; the current context is always resident; a context outside the ring is hydrated on switch and released when it leaves the ring and the screen. A resident transcript holds the whole context, so search crosses all of it.

### Terminal features
- **Alternate screen** (`?1049`), taken after connection up and left on exit (`restore_terminal`).
- **Alternate scroll** (`?1007`) for terminals that need it.
- **Synchronized output** (`?2026`): one `BSU`…`ESU` around each frame so full redraws never tear.
- **Focus reporting** (`?1004`): `App::focused` defaults to `true` and follows `Event::FocusGained`/`FocusLost`.
- **Window title** (OSC 0): `<label> — kaijutsu` for the context on screen.
- **Desktop notification** (OSC 9 / OSC 777) when an ask lands unfocused.
- **Kitty keyboard protocol** (`DISAMBIGUATE_ESCAPE_CODES`), enabled via `--kitty-keyboard`.

---

## Surfaces

### Conversation
The transcript is redrawn every frame from the current context's block mirror (`render::transcript_window`): every block, wrapped at current width, re-wrapped on resize.
- **Role divider** names principal, context performer, and wallclock time. One blank row sits above it when the speaker changes, and one blank row sits above the `❯` line.
- **Tool call and result form one unit under one header:** `─ speaker · tool ─ arg ─ time`, with result body directly below.
- **Long tool results stream whole, then fold once something follows them.** Settled results over 12 lines fold to `▸ N lines · <last line>` under the call's header. `Ctrl+O` opens it in place.
- **Reasoning streams dim and italic**, folding to `▸ thinking · N lines · …` when the answer begins.
- **Structured output** (`OutputData`) lays out at real terminal width (tables, columns, indented trees).

### Compose
Compose runs `kaijutsu-editor`'s `EditorCore` over the kernel-owned draft block (`edit_input` / `submit_input`), as the app's compose overlay does.
- `Enter` in normal mode submits; `Esc Enter` exits insert mode and submits.
- **The key path never waits on the kernel.** Local buffer draws immediately. Each `edit_input` goes on the draft outbox.
- **Vi mode** (`-- NORMAL --`, `-- INSERT --`) displays on the status line.
- **The sibling's edit wins only when newer.** Refuses mirror older than last ack.
- **Registers outlive drafts.** `dd`, `yy`, and `p` share registers across context switches through kernel outboxes (`register_outbox`, `registersPut`, `registersGet`).
- **Long lines wrap, and the band grows for the draft** up to a third of the screen.

### Folds
- **Reasoning and long tool results fold automatically** when followed by subsequent blocks.
- **`Ctrl+O` toggles the fold at the reader's block.**
- History arrives folded; `Error` blocks arrive folded (`✗`).
- Folds are local to the viewer and never sent to the kernel.

### Colon commands
`:` in compose normal mode draws the bar on the compose row from `command_line()`.
- `:kj <argv>`: whitespace-split and runs through `execute_kj`.
- `:!<statement>`: runs through `shell_execute`.
- `:vi <args>`: opens kernel-owned vi editor in alternate screen.
- `:q`: quits unless a turn is known running.
- `:regpull`: replaces registers with kernel copy (`registersGet`).

### Copy mode (scroll, or `Ctrl+A [`)
- **Entry position is the bottom** (newest block).
- **`Up`/`Down` scroll the view one line**; `j`/`k` walk the reader.
- `/` and `?` open search prompts on the status row.
- `v` marks lines; `y` or `Enter` yanks marked range to clipboard (OSC 52) and unnamed vi register.

### The picker (`Ctrl+A "`)
- Overlay at the foot of transcript taking required rows.
- Renders ACTIVE and RECENT ring contexts, horizon, and TRACKS.
- `j`/`k` move, `Tab` hops sections, `Enter` switches contexts, `p d z a c` handle placements.

### Asks
- Overlay card for pending asks (`ActorHandle::ledger`).
- Keys: `a` (allow once), `A` (allow globally), `c` (allow for character), `C` (command family), `d` (deny), `e` (escalate), `t` (take over).
- **Arms 100 ms after drawing** to prevent accidental keystrokes.

### The ledger (`Ctrl+A l`)
- Full view of pending asks and recent decisions (`list_asks`, `get_ask`).
- Same single-key answers as ask cards.

### Status line
- Window list and ring rank on the left.
- Vi mode, token counts (`17.3/128k`), cost (`$1.24`), cache health (`4m`, `91%`), track phasor, and connection status on the right.

### Cache health
- Age of last completed call in minutes (`4m`).
- Cached share of last prompt (`91%`).

### Editor and diff
- `vi <path>` and `kj vi` open kernel-owned editor sessions via `subscribeEditor` and `editor_keys`.
- `Ctrl+A v` or `--diff` opens diff viewer. `y` yanks hunks.

### Images
- Not built. Blocks carry `ContentType::Svg`, `ContentType::Abc`, or CAS hashes (`ContentType::Media`).

---

## The reader's block

The reader's block is the target (`kaijutsu_present::reader::target`) that an act applies to: the last block that fits the act at or above the reader's line, or the newest that fits on the live tail. Used by folds, `Ctrl+O`, and diff opening.

---

## Keys

- **`Ctrl+A` prefix table**: context switching (`Ctrl+A 0-9`), picker (`Ctrl+A "` ), ledger (`Ctrl+A l`), copy mode (`Ctrl+A [`), paste (`Ctrl+A ]`), diff (`Ctrl+A v`), quit/close (`Ctrl+A q`).
- **`Ctrl+C` interrupt ladder**:
  - 1 press: graceful interrupt (`interrupt_context(ctx, false)`).
  - 2 presses: immediate abort (`immediate = true`).
  - 3 presses: abort and clear draft.
- **`Ctrl+Z` context toggle and suspend**:
  - First press: toggles between root/console and the person's shell context (`kaijutsu_client::shell_toggle`).
  - Double tap within 500 ms: suspends (`SIGTSTP`).

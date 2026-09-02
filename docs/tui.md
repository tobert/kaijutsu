# The terminal client — `kaijutsu-tui`

**Status:** design, ruled 2026-08-30 (Amy + Fable), unbuilt. Mockups and
two flash-cast takes on them are in the design artifact linked from
`signoff.md` while it is live; the decided shapes are the ASCII figures
below.

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

## Rulings (Amy, 2026-08-30)

1. **Inline viewport.** *"I tend not to like the fullscreen modes."* The
   transcript flows into the terminal's own scrollback; the live UI is a
   viewport at the bottom, the codex-rs / Claude Code shape. The viewport
   grows for a dashboard and shrinks back. Only the vi editor and the diff
   viewer take the alternate screen, the way `vim` itself does.
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
  applied to UI. Because every grown view renders its own key line ("Keys"),
  the figure's last line documents the surface's keys for free.
- **"Rules the figure carries"** — bullets for the semantics the picture
  cannot show.
- **The machinery, named** — the wire or `kj` path behind the surface
  (`shell_execute`, `subscribeLedgerEvents`, `edit_input`). A surface that
  cannot name its kernel path is not designed yet.
- **Exactly one viewport claim**, from a closed set of three: *flows to
  scrollback* (conversation), *grows the viewport* and shrinks on dismiss
  (picker, ledger, asks), or *takes the alternate screen* (vi and diff only,
  ruling 1). There is no fourth mode.

Two sanctioned deviations: compose's figure is the `❯` line inside the
conversation figure — it is part of that frame, not a grown view — and
editor/diff has no figure because its look is vim's, specified by
`EditorState` rather than by this document. Cache health and Images are
rendering concerns that ride other surfaces, not surfaces of their own.

### Conversation

The transcript is `insert_before` output: a block that completes is printed
into scrollback as styled lines and never touched again. The viewport holds
what is live — streaming blocks, the compose line, the status line. Scrolling
is the terminal's; search, copy and split are the terminal's.

```text
  ╭ claude · coder ─────────────────────────────────────────────── 14:02:11 ╮
  │ The unlink bug was in resolve(): it canonicalized the final component,   │
  │ so the symlink's target was removed instead of the link. resolve_nofollow │
  │ fixes unlink; rename and getattr share the cause and are deliberately ▍  │
  ▸ shell  cargo test -p kaijutsu-kernel vfs::                     running 4s
  ╰──────────────────────────────────────────────────────────────────────────╯
  ❯ and getattr? _                                                  -- INSERT --
  0 kaijutsu*  1 kaish@  2 lfm2d  3 exo        coder/deepseek-v4  ▮ 42%  ● ok
```

Rules the figure carries:

- The role divider names principal, `context_type` and the block's wallclock.
- `▸` is a collapsed block; `ToolCall`/`ToolResult` collapse by default and
  `Error` is a one-line stub, per the app's error-render policy. Collapse is
  kernel state (`CollapsedChanged`), so a sibling's expand is yours too.
- A block that completes leaves the viewport for scrollback. A late edit,
  exclude or collapse of a block already in scrollback **cannot redraw it**;
  the change is real in the kernel and the next hydrate, and the TUI says so
  in the status line rather than pretending. This is the price of ruling 1,
  paid knowingly.
- `Thinking` renders dim and collapses when its turn completes.

### Compose

Compose is a modalkit `VimMachine` over the kernel-owned input block
(`edit_input` / `submit_input`), as the app's compose overlay is. The draft is
a shared block: a sibling's typing shows. `Enter` in normal mode submits;
`Esc Esc` in normal mode clears focus (`docs/input.md`, "Escape").

### Shell (`Ctrl+Z`)

The shell surface runs kaish through `shell_execute` — the gated path a human's
shell already takes (`docs/gate-and-shell-split.md`). Its prompt renders both
cursors:

```text
  kaijutsu ▸ /v/ctx/7f/kaish-arith $ kj stage exclude 019c…#12 && kj fork
```

`kaijutsu` is the acting context; the path is cwd. They move independently —
see "Melted from the ssh shell design".

**`Ctrl+Z` once toggles the shell surface; `Ctrl+Z Ctrl+Z` suspends the
process.** The second press inside the 500 ms double-tap window (the app's
`Esc Esc` / `Ctrl+A Ctrl+A` pattern, `docs/input.md`) undoes the toggle and
suspends for real: leave raw mode, raise `SIGTSTP` on ourselves; on `SIGCONT`
re-enter raw mode and redraw the viewport. Nothing else needs restoring —
the inline viewport leaves the transcript in scrollback and the host shell's
prompt appears under it; `fg` brings the instrument back. Over
`ssh -t zorak kaijutsu-tui` that puts zorak's login shell two keystrokes away
and one `fg` back. (Amy, 2026-08-30: *"could the tui catch ctrl-z and drop to
a kaish repl? ctrl-z twice to background it?"*)

### The picker (`Ctrl+A "`)

The well, flattened. The viewport grows to hold it and shrinks on dismiss.

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
- The live tail is the context's own last line (`live.rs` tail buffers,
  fed by the kernel-wide `ServerEvent` stream). `●` is chatter now, `@` is
  activity since you last looked — screen's monitor flags.
- TRACKS lists `listTracks` with a bar.beat counter and a pulse glyph driven by
  the per-track phasor.

### Asks

An ask arrives through `subscribeLedgerEvents` and is answered through
`kj ledger allow|deny`. It renders in the viewport, never as a modal that
steals the transcript:

```text
  ⚠ ask 01a04eb6  shell_write  from kaijutsu (coder)
    rm -rf ~/src/wt/kaish-arith
    [a]llow once  [A]llow always  [d]eny  [v]iew ledger
```

### The ledger (`Ctrl+A l`, proposed chord)

The ask above is one row of a view you work from between sessions: every
pending ask across every context, then the recent decisions with their
redemption. Same grown-viewport treatment as the picker, same single-key
answers, backed by `kj ledger list` / `show` / `allow` / `deny` and the
`redeemed:` field.

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
- Answered rows keep the answering principal and the redemption, because
  "was this consumed" is the question the redemption incident taught us to
  ask (`docs/issues.md`, the resolved redemption entry).
- `Ctrl+A l` is not in the app's table today; it is proposed here and lands
  in the shared `bindings.toml` (ruling 4), where the app inherits it.

### Status line

Screen's window list, with kaijutsu's facts on the right:

```text
  0 kaijutsu*  1 kaish@  2 lfm2d  3 exo  coder/deepseek-v4  ▮ 42%  ⟳ 91%  ⏱ 4m12s/5m  17.3 ●  ● ok
```

Left to right: the rank (ring 0, seat digits, `*` current, `@` activity, `!`
an ask waiting in that seat, `!n` the pending count across all contexts);
cast and model; context-window occupancy; cache health (next); bar.beat and
pulse for the playing track; connection state from `ConnectionStatus`.

### Cache health

Two figures, always shown, from the last completed LLM call of the current
context (Amy, 2026-08-30: *"how long since the last api turn; a proxy for KV
health when we have no other info … for now let's focus on exposing the data
we have"*):

- **`⏱ 4m12s`** — age of the last completed call, ticking. When the cache
  TTL of that call is known it follows as `/5m` or `/1h`; the segment turns
  warning color past 80% of the TTL and reads `⏱ 6m01s ✗5m` once past it.
  With no TTL known (DeepSeek, a local model) the age stands alone — an age
  is never dressed up as an expiry.
- **`⟳ 91%`** — the cached share of the last call,
  `cacheReadTokens / contextUsedTokens`. `⟳ —` when either is unknown. The
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
- **Write-once emission, riding ruling 1.** An image renders when its block
  completes and prints into scrollback: reserve N lines in the
  `insert_before`, emit the image sized in cell units (so N is exact), and
  never touch it again. Both target terminals keep inline images in
  scrollback and scroll them with the text. A late edit of a printed image
  block gets the scrollback-staleness treatment: status-line notice, no
  redraw. There are no streaming images — a still-streaming block renders as
  text until it completes.
- **The presentation crate stays pure text.** Rasterization and protocol
  emission live in the ratatui edge, beside the transcript printer. The
  `ratatui-image` crate covers detection and encoding for every rung and is
  worth an evaluation pass for those parts; emission stays in our printer
  either way, because the printer owns `insert_before`.

`Abc` blocks reach the staff through the same rasterizer, and need no new
emitter: `engrave::engrave_to_svg` (`engrave/svg.rs`) already renders a tune
to a self-contained SVG string, and `engrave/font.rs` caches a `path_d`
string per glyph, so `kurbo::BezPath` is not on this path at all. Its only
callers today are tests — a `ContentType::Abc` block reaching the terminal is
wiring, not new engraving. Lane in `docs/issues.md`.

## Keys

The prefix table in `docs/input.md`, "The prefix table", ports verbatim:
`Ctrl+A 0–9`, `Ctrl+A Ctrl+A`, `a`, `q`, `"`, `w`, `'`, `A`, `n`/`p`, `d`,
`h`, and the armed-prefix legend line. The legend replaces the status line
while a prefix is pending; there is no separate `?` overlay. Amy, on the
mockup: *"the legend in the status line is awesome, that'll help me a lot, I
tend to forget keys outside the core stuff I use."* Every grown view (picker,
ledger) ends with its own key line for the same reason.

**Shared `bindings.toml`.** The TUI's bindings file is keyed by vim key
notation (`<C-a>`, `<Esc>`, `<S-Tab>`), which `modalkit::key::TerminalKey`
parses. The app's file today is keyed by Bevy `KeyCode` names
(`kaijutsu-app/src/input/bindings_config.rs`, `parse_key_code`). The TUI ships
the vim-notation schema first; converting the app is a follow-up lane. The
file's commentary is technical and reviewed by two flash-tier kaibo casts for
clarity before it lands (ruling 4).

## Time features

| App | Wire | TUI |
|---|---|---|
| The rank (ring 0) | `promotedAt`/`demotedAt` on `ContextInfo` | status-line window list; `Ctrl+A 0–9` |
| The rings | `list_contexts` + `assign_ring_seats` | picker sections ACTIVE / RECENT, `+N` horizon |
| Live-state layer: tails, chatter energy | kernel-wide `ServerEvent` stream | picker tails; `●`/`@` flags |
| Track rays, per-track phasors | `BeatSync` (`subscriptions.rs`), `listTracks` | TRACKS section; bar.beat + pulse in the status line |
| Room, FSN, patch bay, tracker | — | not carried |

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
but wants `ratatui ^0.29`; ratatui is at 0.30. The editor renders from wire
state so the widget crate is optional; if wanted, pin 0.29.

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
kaish loop starting in a lobby context. The TUI absorbs it — its shell surface
is that loop over the RPC path that already exists, and its picker needs no
lobby because `list_contexts` is contextless. Two paragraphs survive:

**Two cursors, never mixed.** The shell has an *acting context* (set by
`kj context switch` / `kj attach`, drives capabilities and what `/v/docs`
points at) and a *cwd* (set by `cd`, where you are looking). They move
independently: `cd /v/ctx/<shard>/<ctx>` browses another context read-only
without acting as it. The invariant: resolve both live per statement, or bake
both per line — never one of each. Live is the target, so `kj attach X ; mv …`
runs `mv` as `X`, the way `cd /foo ; ls` lists `/foo`. The prompt renders the
acting context because it must be legible before you act.

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
- Internal splits. Ruled out for v1; wezterm splits with
  `kaijutsu-tui --context <id>` cover it. Revisit when the itch is real.

# kaijutsu devlog

How kaijutsu and its ideas took shape — an evolving narrative, not a standup
log. `git log` is canonical for what landed and when; the design docs under
`docs/` hold current designs; `docs/issues.md` holds unbuilt work. This file
keeps the story: arcs, decisions, and enduring lessons. It reads oldest to
newest.

## Prologue — the first five months (January–May 2026)

Kaijutsu started January 15, 2026, as an agent environment with a Bevy frontend
and an embedded shell (`kaish`). Early development established the SSH and
Cap'n Proto RPC connection layer, Lamport clocks and fork semantics in the block
store, and an MCP broker routing all tools in-process. Demolitions defined the
toolchain: the Rhai scripting engine went once kaish carried scripting alone, and
rig-core was dropped for hand-rolled providers to retain direct wire control
for cache breakpoints and CAS memoization.

## The stance arrived mid-flight

The framing ideas crystallized during building. **Instrument, not harness:**
Kaijutsu is played by people and models together; the kernel is the instrument
body supplying turn requirements without playing the turn. **Shared trust:**
All players sit inside one trust boundary running under a single Unix user;
loadouts and capabilities are ergonomic nudges for focus, never security.
**Context vs. conversation:** The durable block log is the context; the
live session shipped to the LLM is the conversation, hydrated at boundary events.
**No first-class agent:** An actor is always a principal; agent-ness emerges from
forks and drifts, later generalized as **characters** with sheets.

## Config: the kernel owned it, then gave it back (June → August 29)

The kernel initially owned `/etc/rc` and `/etc/config` directly to eliminate
silent fallback bugs, but this isolated configuration from git and host tools.
August policy restored files to host directories under a unified mount
registry (`/config/rc`, `/config/kernel`, `/config/client`, `/config/midi`),
reaching them through `LocalBackend`. Git became the choice about a directory,
preserving a single truth source without special-casing config paths.

## The music stack — from one loop to a band on the wire (June 13 → July 3)

The music stack forced real-time constraints: models play to a beat, and turn
text is the score. Tracks outlive players on durable per-track score contexts.
Audio rendering converged on the wire rule: bytes never ride the track; MIDI
and audio samples converge on a mime-keyed wire cue (`RenderCue`) backed by CAS
and pre-fetched out-of-band. The kernel links no audio FFI; headless kernels
make no sound, but scores remain preserved and replayable.

## ABC grows up (May, then June 30)

Auditing against the verbatim ABC v2.1 spec fixed fourteen bugs TDD-first.
A robustness net ensuring parse-midi-abc-parse never panics eliminated drift.
The engraver shares `Key::signature()` with the MIDI converter so musical
notations cannot disagree.

## The app — text, wells, and carousels (June–August)

The vi editor runs as a kernel-owned modal session behind `EditorSessions`; the
Bevy app renders from a kernel-served editor channel without caching the
editor's document. The time well evolved into fixed, hand-curated and
automatic rings on the room floor, making spatial position deliberate.

## Wires and surfaces (June–July)

RPC transport consolidated onto a single channel requesting subsystems by
name. The SFTP adapter serves `kernel.vfs()` directly with generation counters
as coherence primitives. Subprocess execution in containers uses deny-by-default
loadout rules.

## How we work — the ritual and its lessons

Surviving practices: **House review:** independent models read whole files with
no diff to evaluate holistically. **TDD and fail-loud:** tests must fail against
old code; crash over corrupting. **Demolition:** parity first, then delete
legacy code whole rather than maintaining transitional shims. **Living docs:**
chronology stays in git and devlog; design docs state the present.

## The instrument gets kinder to its players (July 4)

CLI sweeps refined `kj` verbs for agent ergonomics. `kj fork` learned JSON
arguments, datetime seeds provide daily context blocks, and `kj block cat
--latest` retrieves turn artifacts in one call.

## The kernel gets an interior (July 7–10)

Navigation grew a spatial grammar (Up/Down for detail levels, Left/Right for
siblings, Esc to walk up). Wall screens replaced separate dive views: with
fullscreen as a camera pose and zoom inside one Room state, dedicated UI states
dissolved, deleting entire bug classes.

## The app learns to mean its colors (July 12)

UI and 3D scene rendering unified under a kernel-owned `theme.toml` carrying
sRGB UI and linear-HDR scene palettes. Live A/B testing selected ACES
tonemapping, eliminating muted retro palettes.

## The index learns to keep itself honest (July 12)

The semantic index gained safe slot eviction where slots are never renumbered
or reused. Embedding inference moved out of the kernel to lfm2d with explicit
purpose and normalization parameters.

## The filesystem becomes a world, then ambient (July 12–13)

The FSN world was re-scoped from an active file browser to an ambient 3D view
rendering live host activity, using the `MountTable` chokepoint for file heat.

## The filesystem joins the band — /r client shares (July)

Clients share local directories into the kernel via a reverse SFTP channel with
in-band manifests, supported by stateless `open_read_stream` primitives.

## The beat learns to carry its own clock (July 15)

Timing artifacts carry emission wall-clocks; sinks back-date, and stale events
are dropped. The metronome runs scheduled-periodic on deadlines, and the phasor
includes a deadband where it acts as the local clock, achieving sub-millisecond
sync.

## The stolen bridge (August 2–3)

Client connection deduplication fixed multi-process instance collisions.
`kj db backup` added live SQLite backups via `VACUUM INTO`.

## Contexts join a band — SQL-native model config and casts (August 3)

Model config migrated from TOML to normalized SQLite tables edited via
`kj backend|cast|alias`. Casts define who plays; presets store patch args over
verb arguments.

## Errors that were only strings (August 3–4)

Database errors were typed: `create_document` reads database state directly on
conflicts rather than parsing error message strings.

## Tasks join the block model — the household-agent arc (August 4)

Tasks landed as first-class blocks using LWW registers, allowing household
agents to coordinate task state through the block log without schema surgery.

## The day toad played kaijutsu (August 5)

`kaijutsu-acp` bridged ACP clients. FlowBus broadcast rings gained per-subscription
bounded queues: systems must be lossless or terminated, never silently lossy.

## The instrument changes its strings (August 12)

Bevy 0.19 migration validated dependencies by inspecting manifest files rather
than version numbers.

## The denial that pointed at a locked door (August 12)

Gate refusals distinguish diagnosis from action: read verbs remain uninhibited
so players can inspect why an action was denied.

## The mirror that stopped being a mirror (August 13)

`kaijutsu-mcp` shed its CRDT replica stack. Polling became the guarantee; events
became hints. Hook consolidation replaced custom shell wrappers.

## The instrument could not say who was in the room (August 15)

Roster freshness relies on recent activity stamps. Roots form a forest of
directed contexts, preserving immutable lineage.

## The melt begins, and finds two armed fields (August 15)

Dormant wire fields no client read were audited and removed; dead fields are
armed traps awaiting migration.

## The day the wire stopped being a storage engine (August 15–16)

Client-side sync protocols and text CRDTs were removed in favor of an ordered
per-context change feed (`docs/contexts.md`). Document content became plain
`String` with amortized `push_str` appends.

## Two doorbells, one ledger (August 18)

Hook approval prompts unified with the approval ledger. Gate outcomes explicitly
distinguish refusals from absent controls.

## The conversation stops being a widget tree (August 16–18)

Conversations moved from Bevy UI flex columns to cached shaped glyph runs
drawn in a ±1-screen window with scroll as a uniform.

## The file cache learns what vim already knew (August 18–20)

Disk is the source of truth; file buffers operate like vim swaps. Dirty buffers
write durable rows, and `:w` refuses overwriting changed disk generations
without explicit overrides.

## The scripts that shipped and never arrived (August 20)

rc seeding checks installation status visibly; uninstalled scripts report an
explicit missing state rather than failing silently.

## The turn that said it had finished (August 22)

A turn-in-flight registry tracks active inference runs, avoiding race conditions
between stream completion and next-block submission.

## The seat you ssh into (August 30 → September 8)

The TUI client landed as a standalone ratatui binary communicating over ACP,
providing terminal-native interaction with vim bindings.

## The answer that travelled as an error (September 1–2)

RPC protocol errors distinguish transport faults from logical semantic refusals.
Approval execution triggers automatically upon ledger authorization.

## The name that had no home (September 5–8)

Characters acquired sheets and keyrings. Handoff notes live in ordinary
performer-attributed blocks, replacing ephemeral signoff files.

## The hardware gets its own body (September 7–8)

`audiod` daemon manages real-time audio hardware independently of the GUI app.
Nodes track clock offsets relative to the kernel timebase to tolerate network
skew.

## The file that answered a question the code already knew (September 9)

CLI verbs declare their side effects (`Effect::Read | Write | Destroy`) via
exhaustive pattern matches on parsed enums, replacing error-prone sidecar lists.

## Context types choose their prompts (September 10)

Prompts separated into character-specific rc files rather than forced
universal bases.

## The kernel that fsynced every word (September 11)

SQLite WAL mode configuration switched from FULL to NORMAL durability,
eliminating redundant fsync storms and reducing write overhead.

## The approval that resumed its coder (September 12)

Ledger approvals automatically wake and resume waiting agent turns.
Invocations track requester, performer, and reviewer identities.

## The message that knew where the player was looking (September 13)

Async messages submitted during active turns anchor to conversation edges,
ensuring models receive inputs in correct conversational context.

## The tui takes the alternate screen (2026-09-13)

The TUI adopted the terminal alternate screen for reliable buffer rendering and
clean scrollback handling.

## Retiring duplicated state (September 16)

A major architectural cleanup removed legacy adapter mounts, duplicate client/server
state caches, and redundant synchronization code. Mutation acceptance unified
under single-owner transactions across all wire surfaces, establishing strict
atomicity between journal commits and prompt projections.

## The kernel with no one to answer to (September 16)

`kaijutsu-server init` provisions root characters and keys before startup,
eliminating bootstrap anonymous states.

## The benchmark that measured the harness (September 18)

Harbor meta-harness integration and `kaijutsu-solo-acp` enabled automated agent
evaluation against standard benchmarks (Terminal-Bench) to monitor token costs
and behavior under constraint.

## Burning down the well-defined half (September 20)

Parallel subagent passes burned down accumulated backlog issues, tightening
invariants and removing deprecated code paths.

## The morning rotation (September 21)

Context rotation received a dedicated UI shortcut (`Ctrl+A r`), archiving old
seats and spinning up clean successors from handoff notes.

## The session that answered for its neighbor (September 25)

Session ID isolation (`CLAUDE_CODE_SESSION_ID`) prevented concurrent agent
instances from cross-updating or archiving each other's transcripts.

## Kaijutsu stops trusting the whole host (October 2)

Host file walks exclude sensitive paths (`/proc`, `/sys`), and memory caps
prevent unbounded recursion during glob searches.

## Council guidance becomes chat (October 6)

Council contexts evolved into persistent chat sessions, allowing character
guidance to update dynamically through conversation.

## The ledger stops borrowing a context (October 8)

Ledger RPC methods became context-free, allowing clients to query and manage
approvals without attaching to specific conversation documents.

## Reading the bumps back (October 10)

Council interaction history records reversibility metrics, allowing automated
escalation when agents stall repeatedly.

## The app catches up (October 11)

The Bevy app migrated to Bevy 0.20, integrating WESL shaders and updating its
compose outbox to match tui editing parity.

## The nudge that read as the user (October 16)

Kernel notices and completion events deliver via structured system messages
rather than disguised user turns, preventing model confusion.

## The server that kept no records (October 17)

Council backend integration (gogi) shifted to stateless request payloads,
removing server-side context mirroring.

## The root takes a seat (October 25)

Root console sessions gained model support alongside dedicated non-model
shell child contexts accessible via Ctrl-Z.

## Bootstrap, and the review comes due (October 27)

`kaijutsu-server bootstrap` streamlined initial offline provisioning, supported by
rigorous architectural audit cherry-picks.

## A drift into ROOT waits for the human (October 28)

Incoming drifts into root contexts route through the approval ledger as
`Pending` asks, requiring explicit human release.

## A context ready to roll (November 1–5)

Context types melted into characters. Every context gained a strict senpai edge
and inheritance rules, simplifying authorization and lifecycle management.

## Carrying the conversation without the bulk (November 5)

`kj fork --drop` and conversation stubs compact large historical logs into
summaries, preserving execution facts while reducing token overhead.

## A context learns what changed while it waited (November 8)

File watchers allow contexts to track host file modifications asynchronously,
injecting updates as system blocks before the next prompt.

## The council server becomes a backend like any other (November 8)

`kaijutsu-council` consolidated chat turns, embeddings, and gate decisions under
one unified contract (gogi).

## The pane that held a third of the screen (November 10)

Reasoning and long tool outputs fold into collapsible summary lines in both UI
and TUI, keeping conversation flow readable.

## Signing in to a ChatGPT plan (November 18)

OpenAI's "Sign in with ChatGPT" plan usage lets an open-source app call the
Responses API on a Plus or Pro subscriber's plan, so the `codex-app` bridge,
which existed to reach the plan through Codex, was deleted along with its
inline tool seam. A `chatgpt` backend is a preset over the OpenAI client that
speaks `/v1/responses` under the plan's narrower contract. Sign-in is
paste-back rather than a loopback listener, so a kernel on another host needs
no tunnel. Grants are kernel state, not config: they live in a 0700
`credentials/` directory, not the 0644 `kernel.db`, and rotating refresh tokens
are written before use under a per-backend lock. Kaibo's #253 was the
template.

Plan prompt caching warms slowly over HTTP, and a probe showed that a
Responses WebSocket under `store: false` continues a response on the same
connection and hits the cache at once. So a `chatgpt` request inside a turn
now goes on its context's WebSocket, and sends `previous_response_id` with
only the new items when it extends the last exchange there. The comparison
is on a normalized view of the input rather than the rendered JSON, because
the builder drops reasoning from earlier turns and the server sends ids and
annotations the builder never writes. Anything else sends in full, and a
failed upgrade or a lost connection falls back to a full request, so the
rule can be strict without losing a turn. HTTP stays as the fallback and as
the off switch.

## Give the reason with the denial (November 19)

A reviewer's denial reason belongs to the accepted decision. The ledger writes
it on the decision event in the same transaction. The asking model reads it
in the tool result, so a reviewer does not have to send a second block that
may arrive after the turn ends.

## Working directories during a tool turn (November 19)

A removed working directory is a tool error, not an ask. The failed call
runs nothing even if the kernel found a parent to use for the next call.
Model tool batches read the saved directory once so dispatch and file write
ordering agree after a shell `cd`.

## A judge bumps before it asks (November 19)

Model judges use the gate's durable bump history, not an in-memory breaker.
The third would-be bump within 30 minutes asks; the judge keeps judging.
The same policy keys apply to Council v2 when set under `[judge]`.
Existing host council policies still load until the host is migrated.

## The embedding model is a model on a backend (November 19)

`embedding_config` named its backend by text and held the prompt profile
itself, so `kj backend remove` could delete the backend under the semantic
index and the next start turned the index off with a log line. The setting
now points at a `backend_models` row by `(backend_id, model)`, the row holds
the profile, and both removal paths refuse while the index uses them. It
stays one setting for the kernel, not a character slot: every context is
searched in one vector space. `kj embedding set` runs the startup checks
before it writes. The kernel also builds on macOS now: `rustix` 1.x has the
no-replace rename there.
## A model reads its output once (November 19)

A shell result's `[data]` line repeated the output for every `--json` call
and for `seq`, `find`, and `glob`, so the model read a list twice. The line
now comes only when stdout does not already show the data; a `kj` verb whose
table abbreviates still sends its handles. The decision is by content, not
by who produced the data, because the same builtin prints either way. The
`[latch]` line is gone: stderr already names the confirming command.

The preview limit moved from constants to the character's model
(`kj character model set --preview-kib`), copied into a context's `model`
slot like `max_tokens`, so a small model can take less than Codex's 40 KiB.
The shell and its tool description read the same value. It stops at 48 KiB,
below the broker's 64 KiB shell budget, which would otherwise cut the
envelope with no `/v/cas` copy.

## The heroic view and the eight-bar council band (November 21)

The heroic view (`Screen::Heroic`, `Ctrl+A k`) renders every playing track's
anticipation and commitment along a perspective highway driven by wall time.
Its camera is solved from framing ratios rather than fixed world coordinates,
its identity hues share one golden-ratio OKLCH rule with the room and the time
well, and `kaijutsu-present` owns the pure 10-rule `WorkStatus` classification
and covered-phrase slack math. When no track is playing, the highway stays calm
and empty; no client-side fake driver generates synthetic traffic.

Live testing across `zorak` (TiMidity++ on `ff4sf2.sf2`) and `moltar`
(FluidSynth over PipeWire) turned up four practical lessons:

- `rays::apply_tracks` had been gated to `Screen::Room`, so entering
  `Screen::Heroic` from the conversation view left `WellTracks` and beat
  phasors unpopulated until `apply_tracks` moved ungated and `live.rs` seeded
  `WellBeatsRes` from live `TrackInfo`.
- Real FluidSynth advertises `"FLUID Synth (<pid>)"` with a space in ALSA
  sequencer client names, which `"fluidsynth"` missed until `"fluid synth"`
  joined `SYNTH_PATTERNS`.
- Metronome click config defaulted to `enabled = true` and loaded only at
  connect; defaulting it to `false` and polling `metronome.toml` every five
  seconds lets clients mute or toggle clicks without restarting `audiod`.
- Musician decision programs must match the track's 32-beat phrase (`PHRASE`):
  4-bar programs leave eight seconds of silence per phrase at 120 BPM, whereas
  8-bar programs (`bass-bars.json`, `pad-bars.json`, `drum-bars.json` with
  `shuffle = 2`) play continuously across the 3-track `lfm3-omni` band.

## The rotation signoff (November 23)

A rotation asked for from inside a context's own turn used to archive the
predecessor in the middle of that turn. The turn then spent a model call on
tools its archived context refused, and its last words landed after the
successor was live. Interrupting the turn is the other fix, and it throws away
the part worth keeping: the leaving context's own account of what it finished.

So `kj context rotate` now stops before the archive when the context's own turn
holds its conversation lock. It creates the successor, runs `create`, writes
the handoff brief, and registers a `PendingRotation` on that turn; nothing else
moves, so the label, ring seat, and handoff pointer never name a successor that
does not answer yet. The turn takes exactly one more inference. `builtin.turn`'s
`done` is the whole surface of that call — built for it on demand, offered
whether or not the loadout opted into `builtin.turn`, and answered from the
registered server so a loadout's own `done` cannot intercept it. Anything else
the model names is refused with an error result rather than left to fail
against an archived context. The turn ends after that call whether or not the
model calls `done`; nothing schedules it, and no retry or continuation spends a
second signoff call.

The turn's end then settles the rotation: the predecessor's `archive` lifecycle
runs while it is still live, and the same one transaction as an idle rotation
archives it and hands its place to the successor. The `done` verb is admitted
first, so the report still drifts to the senpai — for a signoff report it runs
even in a turn a person prompted, which the ordinary end suppresses. The
settlement holds a reservation in the turn registry until it finishes, so a
second rotation refuses rather than racing a commit, and a turn that ends
holding a rotation it never settled says so in the kernel log.

Idle rotation is unchanged, and so is a rotation aimed at another context. The
trade is that a rotation is no longer atomic with its command: it is atomic at
the turn's end instead, and a kernel that dies between the two leaves the
predecessor live and the successor unlabeled for inspection, as a rotation that
stops before its commit always did.

## Boards — shared topic rooms and exomemory (November 22–23)

Contexts hydrate into LLM conversations and drifts push direct messages into a
context's live mailbox; neither is a pull-based room where multiple players post
working notes, coordinate kohai, or build a curated knowledge base.

Slices 1–3 of `docs/board.md` add **boards**, **topics**, and **posts** directly
on top of `BlockStore` documents (`DocKind::Topic`, no `contexts` row) so they
never hydrate into a conversation and never wake an autonomous turn:

- **Ownership without a global scope:** Characters own named durable boards
  (`banto/memory`, `librarian/kb`). Each context has at most one lazy working
  board (`contexts.board_id`), carried across `kj context rotate` and reachable
  by direct kohai as `.senpai` for team coordination. Kernel-wide exomemory
  belongs to an ordinary `librarian` character rather than a special global
  table.
- **Zulip-shaped flat topics with backrefs:** Posts are numbered `#1`, `#2`, …
  in log order; `kj board reply '<topic>' --to 3` stores post `#3`'s `BlockId`
  in `parent_id` and renders a one-line backref header while keeping topics flat.
- **Curation gradient (`open` → `curated` → `librarian`):** `curated` boards
  hold non-owner posts as `Status::Pending` + `excluded = true` with an
  `Origin::Board` ask in `approval-ledger`, settled via `kj ledger allow|deny`
  and overridable per character/rc in `gate.toml`. `kj board promote --summarize`
  distills a working topic into a curated board with `promoted_from` provenance.
- **Exact board-scoped semantic search:** `kaijutsu-index` skips `excluded`
  blocks in `extract_context_content`, subscribes to `"block.*"` in
  `FlowBusStatusReceiver` so terminal inserts and hide/unhide re-index, and
  exposes `SemanticIndex::search_in` over `KernelDb::list_topic_ids` rather than
  duplicating `board_id` into `index_meta.db`.

Two DeepSeek Kaibo reviews caught a critical legacy-DB migration bug
(`idx_contexts_board` in `SCHEMA` running before the `ALTER TABLE` ladder), a
high-severity `"block.status"` subscription filter that would have dropped board
inserts/excludes before reaching the embedder, a UTF-8 `split_at` panic in
`parse_duration_ms`, reversible ledger denials via `unhide`, and unthrottled
curated-board asks; a 7-mutant audit confirmed 6/7 killed and added direct test
coverage for `BlockStore::set_excluded`'s pending-held guard.


# kaijutsu devlog

How kaijutsu and its ideas took shape — an evolving narrative, not a standup
log. `git log` is canonical for what landed and when; the design docs under
`docs/` hold the current designs; `docs/issues.md` holds what isn't built yet.
This file keeps the story: the arcs, the decisions, and why they went the way
they did. It reads oldest → newest, like the story it is.

Maintenance: fold new work into the chapter it belongs to; open a new chapter
only for a genuinely new arc; compress chapters as they cool. Commit hashes,
test counts, and day-by-day detail live in git history — including this file's
own history, where the fine-grained entries this narrative was melted from
survive intact.

## Prologue — the first five months (January–May 2026)

Kaijutsu started 2026-01-15 as "what if my agent had a Bevy frontend and its
own shell." The first two days produced a UI shell, a Quake-style console, and
an SSH + Cap'n Proto connection layer; kaish was embedded by day three. The
ancestry is sshwarma — an SSH MUD that grew an equipment system for models and
nerdsniped its author into the context problem — and hootenanny, a retired pile
of music-model experiments. The README's developer note tells that part.

The months after built the body a layer at a time:

- **February** — the type system consolidated (`kaijutsu-types`, `ContextId`
  everywhere), contexts learned to survive server restarts, and a first
  constellation view drew contexts as a radial graph.
- **March** — CRDT correctness (Lamport clocks, fork semantics, order keys),
  DocumentDb + KernelDb unified into one database, and the app moved to MSDF
  text + per-block Vello textures — the rendering stack it still rides.
- **April** — the tool system was redesigned around the MCP broker (everything
  routes through it, builtins included, as a virtual in-process MCP), and the
  CAS crate landed.
- **May** — the ABC crate's first deep spec push (lyrics, repeats, endings), a
  Haiku-driven live-eval harness, kernel-wide timeout policy.

Two demolitions shaped the toolchain along the way: the Rhai engine was removed
outright once kaish could carry scripting alone, and rig-core was dropped for
hand-rolled LLM providers (Claude + OpenAI-compat + DeepSeek) — the "unrig" —
because owning the wire layer is what later made cache breakpoints, CAS image
memoization, and per-role model routing tractable. The sibling projects matured
alongside: kaish grew up rapidly inside kaibo, which is in many ways the
pragmatic take on what kaijutsu explores maximally.

## The stance arrived mid-flight

The framing ideas weren't written first and implemented after; they
crystallized while building, mostly once the music work made "players" stop
being a metaphor.

**Instrument, not harness.** Kaijutsu is something you play — you, a model,
anyone with a connected app; many hands on one keyboard. The kernel is the
instrument's body: it supplies what a turn needs without playing the turn.
That reframe (and the composer→musician, explorer→toolie renames that came
with it) lives in `docs/instrument-design.md`.

**Shared trust, crosstalk-as-feature** (settled late June). The
privilege-asymmetry question — should sibling contexts be defended from each
other? — resolved as won't-fix-by-design: every player is inside the trust
boundary; the kernel runs as one unix user and the real boundaries live
outside it. Capabilities and loadouts are ergonomic nudges for focus and
mistake-prevention, never security; your neighbor's wrong note is one you
cover.

**Context vs conversation** is the load-bearing invariant underneath
everything: the context is the durable, multi-writer CRDT side; the
conversation is the append-only live session hydrated from it at boundary
events. `block exclude`/`edit` land at the next hydrate — remediate a poisoned
conversation by excluding in context, then forking. The per-context mailbox is
the atomicity gate that keeps must-travel-together blocks from being split by
unrelated writers.

**No first-class "agent."** An actor is always a Principal; agent-ness emerges
from fork and drift, not from a noun in the schema.

## The kernel becomes sole owner of itself (mid-June)

A silent-fallback bug in rc loading turned into the biggest structural decision
of June: rather than patch the dual-ownership cluster (stale-bytes reads,
append file-wipes, mtime no-ops, stale rc seeds), we **deleted the class** —
the CRDT became the sole owner of `/etc/rc` and `/etc/config`, seeded once from
embedded defaults under `assets/defaults/`, with no host file and no
write-through. There is nothing to `vim`; `kj rc edit` / `kj config set` are
the surfaces, and `kj rc reset` restores an embedded default. The bespoke
debounced-flush/watcher backend was deleted rather than fixed. Design:
`docs/config-crdt-ownership.md`.

The same weeks put teeth in the fail-loud posture:

- **builtin.file corruption post-mortem.** The kernel's `edit` tool fed BYTE
  offsets into the CHARACTER-indexed CRDT — a silent splice on any file with
  multibyte UTF-8 before the edit site, while honestly reporting success.
  Fixed with byte→char conversion, fail-loud post-write verification (crash
  over corruption), and hashline addressing (`read` prints `LINE:hash→`,
  `edit` re-verifies anchors before writing).
- **The external MCP shell hang** was root-caused to executor starvation on
  the client's single-threaded RPC LocalSet, made *permanent* by a server reap
  that broke subscriptions on the first 5s stall. Fixes: tolerate transient
  stalls (reap only after consecutive failures), client re-subscribes on
  timeout, and the MCP's block subscription scoped to its joined context. A
  300s command dropped to 285ms against a busy 24-context kernel.
- **`FileAttr.generation`** split the cache-coherence stamp from display
  mtime: a monotonic per-backend counter is the coherence primitive; mtime is
  for humans. Two writes in one clock tick can no longer alias, `cp -p` stops
  silently losing mtime, and SFTP's future TOCTOU re-verify shares the same
  primitive.

June 24's cache/cost design session added the lens that still guides prompt
plumbing: the Anthropic prompt cache is a prefix match, so *where* a byte lands
matters more than whether. The per-turn hook seam splits mechanics (Rust) from
policy (data) from decisions (kaish hooks), and hook output is append-only so a
hook physically can't rewrite the cached prefix.

## The music stack — from one loop to a band on the wire (June 13 → July 3)

The longest arc, and the one that forced most of the system's ideas to get
real. Canonical designs: `docs/chameleon.md`, `docs/tracks.md`, `docs/midi.md`,
`docs/pcm.md`, `docs/clips.md`, `docs/hyoushigi.md`.

**The chameleon loop (June 13).** The first loop reached MIDI end to end:
models playing to a beat, a player's turn text *being* the score
(`on_turn_completed` eager-parses ABC). The hard-won constraint: players must
be tool-free — a small local model handed the full palette stalls the turn.
Players are rc programs; a musician is a context attached to a beat track.

**Tracks: the score outlives the players (June 28–30).** Three stages moved
the music substrate off contexts and onto a durable per-track model. Stage 1
moved the clock (playhead, transport, scheduler heap); Stage 2 moved the score
itself — its container is a real, app-viewable per-track **score context**
(minted the lost+found way), which reused the entire per-context block
machinery and embodies the thesis: *the track persists, the players come and
go*. N producers share one open future; failures route back per-`played_by` so
each player reads its own mistakes. Stage 3 generalized the clock behind
`ClockSourceKind` and made tempo mutable — and the landed-code review caught
three places that had quietly assumed tempo was constant for all time,
including a silent-fallback restart data-loss (exactly the class we crash
over). Along the way `context_type` decomposed into rc: musician-ness became
"your create/ rc arms you" rather than a string the kernel matches, and the
rotate page-turn became a five-line rc script (fork → arm → rotate → play)
riding beat-state that now travels with a fork.

**First sound (June 30).** A Haiku musician composed a line and it came out of
a synth: ABC turn → track timeline → materialize → ALSA seq → TiMidity →
speakers. The unit tests had been green for weeks; the acceptance test was
audible. Then a *local* model took the chair: a gemma4-e4b bass, dialed in by
making the prompt small-model-foolproof (`L:1/4`, one note per beat, no
duration numbers, low register) and having the tick rc precompute bar targets
in kaish — continuous bar-filling bass, "lovely harmony." The gig itself
(key, register, vamp) is still hardcoded in the tick prompt; the producer's
chart layer is future work.

**The docs learned to stay present-tense (July 1).** After three intense weeks
the music docs taught superseded mechanisms as current — "living" had come to
mean *stratified*: direction notes on top of superseded status on top of good
design. The fix wasn't more banners; it was moving chronology to the devlog and
git history and letting each doc state the present. `playback.md` was retired
outright (its surviving ideas moved to `pcm.md`, each marked with what
superseded the rest). A tri-model review of the harmonized suite then settled
the render question below.

**Render convergence: bytes never ride the track (July 1–2).** Two decisions,
named out loud in `docs/midi.md`: we take real time seriously by *refusing to
chase it* — micro-batch, promise only what we can hit, a speculation lead of
seconds; only the final sub-lead scheduling on the node that owns the gear is
hard-realtime. And MIDI + samples converge on one mime-keyed wire cue,
`RenderCue { mime, payload: Inline | Cas, lead }` — a placed sample is a *clip
cell* (CAS ref + placement); bytes prefetch out-of-band. The app became the
first MIDI sink (it already had the ABC crate, so it renders symbolic ABC at
the sink), the materialize crossing publishes cues, stop/pause publish a flush
cue — and once parity was proven by ear, the entire in-process render path was
demolished (~1000 lines: `RenderTarget`, `AlsaMidiOut`, the server's `alsa`
dep). The kernel binary links no audio FFI; a headless kernel makes no sound,
but the score is preserved and replayable — silence-now is never lost work.

**The metronome (July 2).** Built to settle a reviewer split about whether the
per-cue anchor and a continuous timebase compose (measure, don't assume). The
first cut sloshed — integrator wind-up in the slew — replaced by a
feedforward-tempo P-phase controller: run at the reference tempo directly,
correct only phase by a small bounded step. Inter-click stddev fell from 50ms
to 0.7ms. Clicks are pre-scheduled into the ALSA queue (not fired at frame
time), references are low-rate, and a flush cue silences the phasor on stop.

**Clips (July 1).** A seven-industry survey of cue systems
(`docs/cue-prior-art.md`) found every industry re-inventing the same six field
clusters — half already on hyoushigi's `Cell`. So **Cell does not expand**;
Shape A is a versioned `application/vnd.kaijutsu.clip+json` payload (media hash
+ mime + required human label + source range + gain + extension bag), tempo
default tick-anchored/no-stretch, trigger semantics in the transport, never
the committed record. No standalone format unless interchange knocks: OTIO won
model-first, AES31 stalled format-first.

**`/v/cas` — the CAS pool made reachable (July 2)** *(originally shipped as
`/v/blobs`; renamed `/v/cas` 2026-07-06 for naming consistency).* The clip
design needed "sync the CAS to the client," and track B went design → audible
demo in one arc: harden `kaijutsu-cas` first (atomic store, TOCTOU-free
retrieve, validating `ContentHash` deserialization — the client cache is
multi-process and a cache hit never re-hashes, so a torn object would be truth
forever); a read-only `CasFs` VFS backend at `/v/cas` where immutability makes
the hard problems trivial; a client `BlobResolver` over its own SFTP connection
(SFTP futures are Send, the capnp world is !Send — they must not mix) that
re-hashes every fetch and hard-errors on mismatch; and the app sink consuming
CAS cues off a dedicated runtime. The review earned its keep: gemini caught
two real concurrency bugs (a transport-error handler that could wipe a fresh
connection, a single-flight lock leaked on cancellation) that both the author
and deepseek missed. Verified by ear: `kj cas put` → `kj play --cas <hash>` →
SFTP fetch → hash-verified XDG cache → speakers. One scar worth the telling:
kaish's overlay *reserves* `/v/cas`, so `kaish ls` shows an empty shadow
while SFTP serves the real pool — an hour lost, a gotcha memory written.

**Music demo #1 post-mortem (July 3).** The first attempt to run the whole
band as a demo burned a director's turns on stale docs advertising the
demolished `kj transport render`, then found the app's ALSA port unwired, then
found kaish couldn't run `aconnect` at all. The docs got supersession banners;
the deeper fix was **subprocess exec** (below).

## ABC grows up (May, then June 30)

The notation crate got its second, harder conformance push as a kaibo
three-model audit with the verbatim ABC v2.1 spec in context — which paid off
twice over, once by finding bugs and once by *rejecting* a confident wrong
finding (the code's accidental propagation was already spec-correct). Fourteen
real bugs fell TDD-first: tempo beat-units, compound-meter rests, tuplets
dropping inner rests/chords, key-signature accidentals never reaching MIDI, a
tie carrying an accidental across a bar line leaving a hung note, and variant
endings not expanding at all. A robustness net followed (parse→midi→abc→parse
must never panic; NoteOns/NoteOffs must balance) and immediately caught a real
divide-by-zero (`L:1/0`) — a parser over untrusted ABC degrades, never panics.
Grace notes now sound (steal-from-next, beat grid preserved). The engraver
turned out to carry exact copies of two MIDI bugs, fixed at the root by
extracting one shared `Key::signature()` both call — so they can't drift again
— followed by a rendering sweep (augmentation dots, H-bar rests, tuplet
brackets, mid-staff `[K:]` clef changes).

## The app — text, wells, and carousels

**The vi editor (June 23).** Editing is a kernel-owned session — `EditorCore`
(pure modalkit vim) behind kernel `EditorSessions`, with the Bevy app one
renderer among many drivers. Three front doors (`vi` builtin, `kj rc edit`,
MCP) share one primitive. The feared render-path collision evaporated by
decision: the app renders from a kernel-served editor-state channel and never
joins the editor context into its document cache. The app-id addressing
infrastructure (per-window instance, server-stamped principal, identity-guarded
self-detach) landed as its groundwork.

**The time well.** The context browser went through more visible evolution
than anything else in the project — the constellation of February became a
compacting spiral, then a tilted vortex with an accretion-disk throat and
odometer navigation; cards moved off `StandardMaterial` onto a full-GPU SDF
card material with MSDF text crisp at any zoom; HDR + bloom collapsed onto one
shared always-on camera so only the card FX bloom. Kernel-derived live status
rides the existing poll (thin client, smart kernel); drift endpoints shimmer.

Then July 3 made it navigable in one long live-tuned day: idle-age **bands**
keyed on a new `last_activity_at` (stamped at the one journal chokepoint;
status reads became an O(1) cached bump instead of an every-5s full rescan) —
resurfacing proven live by drifting a probe into the second-oldest context and
watching it jump to the mouth. Terraces grew ornate counter-rotating
magic-circle rings ("it looks so cool"), cards stood up as slides radiating
from the funnel; and the terraces became a **Kodak-Carousel** the user drives:
one ring per band, left/right spins the focused ring so the selected card eases
face-on to a gate angle, up/down changes rings, non-focused rings dim
("fantastic… I'm delighted").

Two days later (July 5) the idle-age bands themselves were replaced: **placement
you can't control isn't an instrument.** Amy's model — two hand-curated rings
sandwiching two automatic ones, every ring exactly ten seats, digits addressing
the focused ring's seats — landed end-to-end in an afternoon of lead + two
sonnet lanes: ACTIVE (promote by keystroke or by visiting; the kernel
auto-promotes in `setLastContext`, which the app already called), RECENT and
BUMPED (pure recency competition for ten seats each — the age constants and the
running-forces-hot override died outright; liveness is *light*, never
placement), DEMOTED (an explicit push-away), and past all four, a real event
horizon: unseated cards get no entity, just a "+N" in the throat. The demote
ladder steps one ring outward per press and archives off the end; promote on an
archived context *resurrects* it (Amy: the archive is memory to drift back
from, not trash — the door Stage-5 search will feed). Pause landed as designed
state only — a `paused_at` stamp, a toggle, a dimmed card — with its real
meaning (suspend activity: no beat wakeups, refuse turn-starts) documented on
the column for a later slice. A legend HUD names the verbs in the well itself;
the keys are declared provisional. Ring 0 is the Stage-2 rank arriving in ring
clothing: append-ordered, kernel-owned, ten seats, digit-addressed.

**Conversation view hardening (July 3).** Two long-standing irritations fell
in one arc. Error blocks stuck to the bottom traced to Bevy child-ordering
choreography: three mutations changed order without bumping the re-sort gate,
and `replace_children` silently un-parented missing entries into leaked root
nodes — now fail-loud. The "text loads with holes" bug split into a benign
self-healing transient and two silent forever-failures: a full MSDF atlas
respawned generation tasks every frame — infinite CPU churn wearing a
missing-glyph costume — and missing font data retried unbounded. The atlas now
grows to a 4096 cap, terminal failures land loud, and kanji-heavy documents
(the motivating case — 日本語 conversations) keep their glyphs.

## Wires and surfaces

**One channel, named subsystems (June 26).** The RPC transport moved off a
positional three-channel scheme (two of which existed only to pad the ordinal)
onto a single channel requesting the `kaijutsu-rpc` subsystem by name — the
shared retention-and-dispatch scaffold SFTP and future subsystems hang off as
additional match arms. A flag-day cutover, no compat shim; early dev, single
user. The client actor also stopped lazy-connecting: it dials as soon as it
can, because the early connected/failed signal is worth more than deferring —
the first call after a cold start no longer bounces.

**SFTP + the VFS.** The full SFTP server adapter serves `kernel.vfs()`
directly; the generation counter (above) is its coherence primitive; `/v/cas`
(music chapter) is its first growing pool. Track V (`/v/ctx`, `/v/session`)
and adapter limits are the open follow-ups in `docs/slash-v.md` / issues.md.

**Subprocess exec (July 3).** The music-demo post-mortem's real fix: kaish's
`subprocess` feature turned on behind a new `exec` loadout authority
(deny-by-default at materialization; coder/mcp/default/director seeds carry
it, musician/toolie never), `MountBackend::resolve_real_path` made real, and
`$PATH` seeded from the kernel env. Verified live from inside a context shell
— including re-making the TiMidity wire with `aconnect`. The direction locked
with Amy inverts the mount posture entirely: an *opaque host* — drop the
read-only `/` mount, curate PATH-dir bin mounts per context_type, VFS-mediated
resolution upstream in kaish.

## How we work — the ritual and its lessons

The practices that survived contact, recorded because they're the real product
of six months:

- **The house review ritual:** two models outside our family read the *whole
  files*, no diff — typically a gemini-pro batch plus a deepseek agent over the
  same surface — so they evaluate holistically. Cross-model divergence is the
  point: each has caught real bugs the other and the author missed. Batch is
  the resilient path for gemini-pro; interactive 503s under load, batch
  capacity sails.
- **When two competent readers model the topology differently, that is the
  signal to go look.** The transport-ACK review's reviewers disagreed on
  threading; tracing it found a no-deadlock property that held only because
  one function stayed fully synchronous — now a documented invariant rather
  than an accident. Diagnose from the code, not a reviewer's summary.
- **Two voices at design time.** Big cuts (Tracks Stage 2, the render
  convergence) get stress-tested by independent models *before* code; the
  findings fold into the tracker, not a rewrite later.
- **TDD, red-first, and crash over corruption.** The recurring bug class is
  the silent fallback — restart tempo loss, byte/char splices, torn CAS
  objects — and the recurring fix is fail-loud verification plus a test that
  fails against the old code.
- **Demolition as practice.** Rhai, rig-core, the config flush backend, the
  in-process MIDI path, dead viz layout code, and the KV store (July 4: its
  one production caller moved to a typed per-client row first, then ~1,600
  lines deleted whole — the VFS namespace is the shared-state store) —
  parity first, then delete whole, never strand a transitional path. The
  score being durable is what makes deleting renderers cheap.
- **Docs are living, not stratified.** Chronology belongs here and in git;
  design docs state the present. `docs/issues.md` deletes entries when they
  ship. And the acceptance test for music is the ear.
- **Shared docs get edited, never re-emitted from model memory.** Twice now a
  stale in-context copy of issues.md has been whole-file-written over a
  groomed HEAD (hallucinated dates included). The reconcile ritual: title-diff
  forensics against HEAD, graft only the genuinely new entries, discard the
  ghost.

## The instrument gets kinder to its players (July 4)

A player's-eye sweep of the kj surface, picked by asking one question of the
backlog: what does a model hit mid-turn that a human wouldn't tolerate? Five
lanes ran as parallel worktree subagents (two Opus, three Sonnet) with the
lead context coordinating, merging, and keeping the docs honest — the first
real test of the fan-out-and-merge shape, and it held. What shipped, and what
the digging taught:

- **`kj fork` works from kaish again.** The `--include` range parser was
  never broken — the kaish→kj bridge had no arm for `Value::Json`, so every
  repeatable `Vec<String>` flag arrived Debug-formatted. One general fix
  repaired fork ranges and every other repeatable kj flag that rides through
  kaish. Label conflicts now fail *before* the billed distill and name the
  existing context; compact-fork distillation defaults to the caller's own
  provider+model, and `--distill-model` speaks `--model`'s grammar — that
  last one because the coordinator caught the new error message recommending
  syntax the parser rejected.
- **Contexts know what day it is.** Datetime rc seeds (kaish's `date` builtin
  → `kj block create --kind notification`) fire at create/fork for
  coder/director/mcp/default; musicians deliberately never — musical time is
  their only clock. The load-bearing choice was the block kind: Notification
  hydrates as an appended message, while System/Text would be swept into the
  cached system prefix and invalidate it daily, and `.kai` stdout is
  model-hidden Trace. Both mechanism halves already existed; zero kernel
  logic changed. Motivated by three hallucinated-date incidents in durable
  docs.
- **Config lies less.** Unknown provider types are rejected at `kj config
  set`/`edit` with the supported list (the boot-time drop was
  silent-until-a-turn-hung); "missing API key?" only appears on a real auth
  error; piped stdin works as the help always claimed (the gate was an
  rc-only hardcode); `kj config edit` mirrors `kj rc edit`.
- **Artifacts are one verb away.** `kj block cat` resolves a block's CAS
  content (binary refuses the terminal; `--out` for bytes), and `--latest
  <mime>` answers "give me this turn's rendered artifact" in one call. `kj
  rc list` marks every script in-sync/differs/no-seed against its embedded
  seed — detection for the stale-seed class without touching live-is-truth.
- **A "bug" that wasn't.** The unknown-command 300 s hang closed as a proof:
  the dispatch fall-through is bounded at every await (tests across all
  three shell flavors, a cross-model audit, a live probe). The observed hang
  was almost certainly the stale-FlowBus observation gap wearing a costume.
  And `$HOME` is now seeded in every shell — the dig found `~` was broken
  too; both read one scope var, so they agree by construction.
- **The awaited kaish release closed two of these loose ends.** The 0.10 → 0.11
  bump was zero-source — we ride the embedder API through low-level primitives,
  so all four of the release's breaking changes miss us — but it carried the
  rewrite we'd parked two papercuts against. The confirmation-latch nonce, an
  explicit machine protocol whose token was buried in human prose (a batch loop
  had to `2>&1` and regex-scrape it), now rides a typed `ExecResult.latch`; we
  emit it structurally on both the MCP shell envelope and `kj --json`, so
  automation reads `latch.nonce` and re-runs with `--confirm`. And kj's
  synthetic root `help` param — a crutch that existed only to stop kaish's outer
  help router from swallowing `kj <verb> --help` — retired the moment 0.11 gated
  that router on `owns_output` (an owned-output tool re-parses its own argv and
  is never intercepted). Same theme as the rest of the chapter: the surface a
  model hits mid-turn stops fighting it.

## The kernel gets an interior (July 7–9)

The time well had proven that kernel state could be a *place*; the scenes
charter (`docs/scenes/`) asked what building the rest of the place would
mean. Two days of design — 28 image-model mockups culled to one canonical
image per decided surface, every discarded lesson melted into prose — then
three days that took the first station from spec to a finished instrument.

- **The room exists, and the arrows just keep going.** Navigation grew one
  level up without a new grammar: Up/Down move between detail levels,
  Left/Right within one, Esc always walks up — and the well's mouth ring
  exits upward through a *speedbump* (double-tap, the app's existing 500ms
  pattern pointed at a new axis) so habitual ring nav never ejects you.
  Slice A made the blockout a chamber: vault, trace floor bowed around the
  console emblem, bearing pylons with engraved nameplates, violet radiator
  placeholders, and per-bearing activity glow fed by the same event stream
  the well already ingests — the shell adds renderers, not wire.
- **The camera taught us the room's first hard lesson.** The focused-station
  pose originally stood diametrically across the chamber — and cardinal
  bearings are colinear through the center, so the opposite pylon and the
  console stacked on the sight line and hid the very station being focused.
  The fix is an *approach* pose: stand on the focused station's side,
  looking outward. Same family: the reserved South marker shrank to a stub
  because the overview camera lives at South. In a radial room, every
  camera pose is a claim about what may stand between you and the center.
- **The patch bay went from black blob to instrument.** Slice 0 (observed
  ALSA graph on a round table, read-only) shipped with the nav skeleton;
  the visual wave made it parseable: etched gold guide rings and seat
  ticks, short ALL-CAPS port labels from a display heuristic that
  deliberately is *not* the symbolic-endpoint registry (that question
  stays open), nameplates receded to a supporting tier, and the
  inspection card blooming at the selected chord's apex with
  shrink-to-fit text, speaking the same label language as the pegs.
- **Slice 1 killed the oldest papercut.** The app auto-connects its render
  port to a name-matched GM synth on startup — deferential (any existing
  outbound wire means stand down) and one-shot with patient retry, so a
  human's later `aconnect -d` stays cut: the metronome click rides that
  port with no off-switch yet, and a continuously-reconciling ensure would
  have made the wire uncuttable. Continuous reconciliation stays slice 2's
  kernel-owned job. Names, never client numbers.
- **Live traffic is light.** The render port's send seams raise one message
  per frame-with-traffic; chords the app can observe carry a GPU-animated
  packet (one uniform write per pulse, `globals.time` does the rest).
  The two-hour hunt for the "missing" pulse ended in the best possible
  verdict: staged shader probes (stamp-arrival, age-window, UV paint)
  proved every layer correct — nothing was broken. The 0.42s default is
  just faster than screenshot sampling, and the only chord was always
  selected, masking the band in its own glow. Lesson: distinguish "the
  mechanism is broken" from "my observation can't see it" *before*
  touching the mechanism.
- **The fan-out held on a single file.** Two opus lanes built the
  instrument face and the live layer in the same `mod.rs` under explicit
  region ownership; the merge was three keep-both conflicts. Every lane
  got a kaibo round (gemini batch + deepseek, whole files, no diff) —
  which caught two real moderates (a cold retry timer; unmasked MIDI data
  bytes in the pre-existing click path) and one real HIGH (room nameplates
  blank on re-entry: a process-lifetime latch guarding per-visit
  entities — the same bug family as the patch bay's own re-entry fix that
  morning). It also produced confident "criticals" asserting pre-0.12
  Bevy folklore — non-recursive despawn, no sync points between chained
  systems. Bevy source is checked out locally; reviewer claims about
  engine scheduling get verified there before any code moves.
- **One scene graph, and the lifecycle bill for it (July 9–10).** Amy
  settled the shell's biggest open question — shared, not separate: the
  patch bay is room furniture behind one placement transform, and diving
  is a continuous camera descent inside the persistent room, with LOD
  (room chrome hides on dive, the label/card layer shows only dived)
  recovering the budget the scene-cut used to provide. The review round
  on that slice earned its keep once: with `OnExit(Room)` no longer
  firing on a dive-first exit, a context switch landing mid-dive leaked
  the whole room into the next screen. The fix made the dive's own exit
  share the room's teardown — and the *same* round re-asserted the same
  pre-0.12 scheduling folklore as last time, now formally a pattern:
  engine claims get checked against the local Bevy checkout first.
- **Furnishing day (July 10).** With the grammar proven, one sonnet lane
  moved the room from blockout toward the concept renders: a ~35-route
  deterministic circuit-board floor (pure generators, keepout locked by
  tests against the production route table — "the floor is the wiring"
  made literal), an inscribed gold ring the routes depart from, the well
  emblem grounded on a real table whose plinth physically fills the trace
  keep-out, framed radiators with thread-strips, pylons with plinths and
  caps. Amy's dials: boring labels (TRACKER, not RHYTHM GATE — plates
  should *recede* as real detail arrives), more solidness, aurora paused
  until the drift layer knows what information it carries. The lead's
  live tuning pass then earned two lessons worth keeping: **inhabitable
  is mostly camera height** (dropping the overview from bird's-eye to
  human-eye did more than any geometry), and **you cannot light a 1%
  albedo** — pixel-sampled screenshots proved no point-light intensity
  lifts a near-black metallic surface; the material's diffuse response,
  not the lamp, was the knob. The dived table stays gold-etch-on-black
  by choice.
- **The room closes over (July 10, afternoon).** Amy asked for enclosure
  and a camera cutaway, and both turned out to be one rendering rule:
  build the wall shell single-sided, facing inward, back-faces culled —
  near walls vanish from any outside camera, the dollhouse cut for free.
  Her shape call made the walls *mean* something: an octagon of eight
  content-surface panels ("the surface gets taken over by its content"),
  neon-trimmed in each bearing's hue, the free-floating radiators
  retiring into the diagonal panels. In the same wave the patch wheel
  stopped being a labeled exhibit and became the west station itself —
  sign and pylon deleted, the live circle seated on a dais at furniture
  scale, floor traces terminating at its foot — and the whole scene
  family unified on one palette module and the all-unlit discipline (the
  patch bay's point light and lit metals deleted outright; the albedo
  lesson made them dead weight). First light found the honest bugs taste
  can't: the wheel's tabletop seated exactly coplanar with its dais
  (full-surface z-fighting starburst) and a "one shade up" surface that
  washed the gold etch grey. Both are contract fixes now, not tweaks —
  the dais agreement lives in the palette module where neither file can
  drift from the other silently.

## Now

As of 2026-07-10: the kernel-interior scene family is live and furnished —
the Tardis room reads as a place (circuit-board floor, well table, framed
radiators, signage hung high), with the patch bay standing at W as
furniture inside one shared scene graph, dive-as-descent proven end to end
including its lifecycle edge (leaving the shell from a dive, live-verified).
The tracks bearing's breathe-on-jam acceptance and the metronome-click
chord pulse await the next live jam. Deferred by choice: the N archway
(returns with the fsn scene), the drift-layer air (paused until it knows
what information it carries — point cloud vs aurora arcs is open). Next
station candidates: the tracks highway (TRACKER, E) or patchbay slice 2
(declared wires, kernel-owned). Open work is in `docs/issues.md`; the live
handoff in `signoff.md` (ephemeral, repo root).

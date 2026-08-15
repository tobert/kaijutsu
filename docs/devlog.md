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
`docs/pcm.md` (which absorbed `docs/clips.md`, 2026-07-16), `docs/hyoushigi.md`.

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

**The HUD melts into the instrument (July 11–12).** Amy's first look at the
room's hero shot named the problem: the four camera-parented edge panels read
as floating flat UI over a diegetic scene. Four slices melted them into the
instrument — selection drapes down the bowl wall (mockup 27's silk threads,
finally built), a live-tail band on the selected card's own face, the reading
card absorbing specs + ancestry as pure shared text (`specs_text`/
`ancestry_text`, extracted so panel and card rendered byte-identical content
while both existed) — then `hud.rs` (851 lines) died whole. Live-driving the
reading card before the cut caught a real bug the panels had been masking: the
absorbed specs duplicated the card header's model/fork lines and pushed
ancestry + tail past the glyph budget, silently dropped by the overflow guard
— exactly the content the slice existed to show. The keyboard legend survived
as the one panel with no scene-native home, reborn as a transient `?` toggle
(dismissed by `?`, zoom-out, or leaving the room). Every readout now lives on
the thing that owns it; the well's mouth is open browser space again.

**Four rings become two and a floor (August 1).** The well moved into the
room's center as furniture, and Amy asked for the magic circles to lie
parallel to the floor. That one rotation quietly turned funnel depth into
plain height — `world_y = 160 + 0.5·depth` — and the arithmetic indicted the
design: BUMPED landed at world y −70 and DEMOTED at −185, two rings of cards
rendering in the room's basement, with the deck and the "+N" label deeper
still. The fix wasn't to lift them. Asked what those two rings were *for*, the
honest answer was: the same thing. Demoted, concluded and overflow contexts
all read as one category to the eye — work you have pushed away — and the
four-ring scheme had been paying two rings of prime real estate for a
distinction nobody navigates by. So ACTIVE stayed on top, RECENT rose to just
clear the tabletop, and the two lower rings collapsed into the destination
they had always been heading for. The ring deck moved onto the room floor and
stopped being a "throat floor": it became the **event horizon**, an
accretion disc lying on the room floor, encircling the plinth of the table the
rings hover over — a shader that had always looked like accretion finally put
where that reading is literal. The "+N" moved onto the disc it counts, which
is also why it stopped being hidden at room scale (it used to float mid-air as
an unreadable chip). None of it touched the kernel: `p`/`d`/`z`/`a`/`c`, the
demote ladder and the stamps are unchanged — `d` still means "pushed away," it
just stopped earning a ring. Card entities halved to ≤20, and `h` went in as
the front door to a horizon dive nobody has built yet. The reusable lesson: a
geometry change is a *proof obligation* against the design it renders, and the
test that catches it is the one composing the real placement math to assert a
card ring never sinks below the floor it hangs over.

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
- **0.12 (July 12) closed the third.** Zero-source again — `LatchRequest`
  picked up a `job_id` back-reference we don't construct, everything else
  landed on surface we don't touch — but it fixed the `/v/cas` scar from the
  CAS chapter above: kaish's `VirtualOverlayBackend` used to reserve the whole
  `/v` tree for itself regardless of what an embedder had actually mounted
  there, so `kaish ls`/`cat /v/cas/...` saw an empty shadow while SFTP and `kj
  cas` (which bypass the kaish VFS) saw the real pool. Routing is now purely
  mount-coverage based — an unclaimed `/v/*` path falls through to the
  embedder's backend — so kaish's view of `/v/cas` finally agrees with
  everyone else's. Pinned by a new regression test
  (`kaish_ls_and_cat_reach_the_real_cas_mount_at_v_cas`) so a future bump
  can't quietly reopen it.

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

- **Walls become screens, and a state dies (July 10, evening).** Amy kept
  pulling the same thread: mount the patch wheel ON the wall instead of in
  front of it (a transform edit — the placement seam's third re-placement,
  though typography taught us the one thing a similarity transform can't
  right is which way text reads); then "we could almost drop the dive if
  the walls were 16:9 and you could fullscreen them." She was right, and
  the payoff was structural: with fullscreen as a camera pose plus a zoom
  field inside the one Room state, `Screen::PatchBay` dissolved — and with
  it the entire dive-exit lifecycle machinery, including the leak fix
  built that same morning. The careful teardown special-casing lived one
  day, replaced by a design in which the bug cannot exist. Bounded
  stations are now panel content (the wheel owns the W panel at 82% of
  its height; the tracker's falling notes and the radiators' message
  walls are born screens); only worlds too big for the room — the fsn
  landscape — keep a true dive-through door. Deleting a state to delete
  a bug class is the day's best trade.

## The app learns to mean its colors

The terrace glyphs came first (2026-07-12): the placeholder dashed dial
became a per-ring variant family — barcode graduations, braided rosettes,
a Fibonacci moiré dial, orbiting motes — with hash-seeded gem glints
twinkling gold on every ring. Amy looked at it and named the real problem:
"I see it, but it's muted like the rest of the octagon… maybe the goal for
the vibe overall is more synthwave than anything." The mutedness turned
out to be structural, not aesthetic: palette.rs governed hues and the
glow-discipline caps, but *brightness* was thirty scattered per-site
constants, and the tonemapper had never been chosen — the app's look was
the accidental sum of local decisions.

The color pass made color a decision again. One CRDT theme.toml now
carries both color lanes — the sRGB post-tonemap UI lane (the old Tokyo
Night token system, kept) and a new linear-HDR scene lane (`[scene]`:
identity hues, a named brightness ladder, live-signal gains, and a
`[scene.post]` camera chain that hot-applies). App-side, a `ScenePalette`
resource absorbed every scene constant; palette.rs shrank to geometry
contracts. The synthwave skin shipped as the default across file, data
layer, and compiled fallback (Tokyo Night retired to contrib/themes/),
and a live tonemapper A/B over BRP picked ACES + raised bloom — the muted
look was literally TonyMcMapface. docs/color.md is the contract: one
identity, two lanes, threshold 1.0 stays the line between decoration and
live activity.

Two lessons worth the ink. The mirror test between compiled defaults and
file defaults caught a real sRGB-as-linear bug on its first run — the
palette had been quietly 13× off on one channel family. And when a
round-tripped `kj config show` poisoned the live theme with its own
decoration, the app's refuse-loudly parse path (toast + keep current
theme) turned what could have been a silent skin corruption into a
ten-minute diagnosis — the observable-write-failures discipline paying
for itself.

## The index learns to keep itself honest

The semantic index — bge-small over ONNX, an HNSW graph, a SQLite sidecar —
had grown real consumers (well-card gists, constellation clusters, kj
synth) on top of three quiet debts: HNSW can't delete points so eviction
left dead vectors forever (`rebuild()` was a TODO), nothing noticed when
the embedding model changed under an existing index, and every synthesized
gist evaporated at kernel restart because nothing re-warms a memory-only
cache when content hashes say "unchanged." One afternoon (2026-07-12)
retired all three, plus a live ABBA deadlock between search and indexing
that a stress test could summon on demand.

The design decision that made rebuild tractable: **slots are never
renumbered**. A rebuild re-inserts only live slots into a fresh graph at
their existing numbers, so SQLite is never touched and crash-consistency
collapses to atomic file publication (dump `.new` → fsync files, marker,
and directory → rename → recover idempotently at boot). The corollary took
a red test to believe: slot numbers must also never be *reused*, because
MAX+1 allocation regresses when the highest slot is evicted and the dead
vector still in the graph would answer for the new context. A monotonic
allocator table closed the class. First boot on the live kernel vindicated
the whole shape immediately — the real index was carrying 51 graph points
against 43 metadata rows, and startup auto-rebuild silently reclaimed all
eight dead slots.

Live verification earned its keep twice more. `kj synth all` on real data
blew ort's never-shrinking arena past 9 GB — one BatchLongest-padded
embed_batch of every block in a large context — fixed by chunking at the
embedder seam, where every call site inherits the bound. And the
whole-file kaibo ritual (deepseek consult + gemini-pro deliberate, no
diff) caught what unit tests hadn't: eviction cleared persisted synthesis
but left the memory cache serving ghosts, and the rename dance fsynced
files but not the directory. Sonnet lanes wrote the code; the lead's
review, the outside models, and the running kernel each found bugs the
other two missed. That triangle is the lesson.

## The filesystem becomes a world (July 12, evening)

The fsn landscape went from baked vocabulary to a rendering world in one
evening of three parallel Sonnet lanes with the lead reviewing seams:
pure layout math in kaijutsu-viz (CellId quadtree + relaxed-Voronoi with
fixed-k Lloyd — the blast-radius promise turned into a trajectory-compared
test), a `Vfs.snapshot` RPC with generation stamps and
gitignore-as-metadata, and the Bevy scene behind the N archway — a genuine
`Screen::Fsn` dive-through, wireframe prisms and vertex points in exactly
frame 45's grammar. Reviews earned their keep in both directions: the lead
caught a fetch-queue wedge, a truncated-dir refetch loop, and
guaranteed-overlapping subdir fields before merge; kaibo verified the
fixes but also *mis-blessed* one thing (Bevy messages don't wait for a
gated reader — they expire), which became the fourth fix.

The deepest lesson came from the live pass: the unit trees were too
polite. The real host tree killed the walker three ways in an hour —
root-only directories (one EACCES failed the whole walk), `/v` existing
only in the mount table (intermediate mount dirs had never had
getattr/readdir semantics), and `/proc` PIDs vanishing between readdir
and getattr. Each fix was a design decision, not a patch: denial is a
fact about the tree and renders as a seam (truth-seams rule), the mount
table now answers for its own synthetic namespace, and churn under the
walk is claim 4 made operational. Then the arch opened onto violet
districts over a dark plane, the basalt pattern plainly visible, and a
selection-ring pass over `/etc` pulled `/etc/iptables` out of the
unbuilt shell — enumeration-on-demand working exactly as designed.

## The world becomes ambient (July 13)

Slice 1 opened with a reframe from Amy that rewrote the roadmap: the fsn
world is **not a file browser** — agents work at the file level and the
shell covers the rest. It's the space the octagon vessel inhabits, and the
filesystem is a free source of ambient data that looks good in 3D. That
single sentence deprioritized the bloom grammar, dive-to-vi, and search,
and promoted three reaches: heat from the kernel's own hands, recency from
data already on the wire, and the vessel actually *inhabiting* the world.

The heat design fell out of a distinction worth keeping: the MountTable
chokepoint already sees every kaijutsu-mediated file op, so the kernel can
light the world where *it* is working with no new dependencies — inotify
and host weather (cargo-build storms) stay a later reach, and arguably a
different statement. The wire is the vfs.md digest design made real minus
depth-keying: absolute per-directory totals from per-connection timer
bridges, where the subscription is just parameters against rolling
counters. Absolute totals proved their worth three times in review — the
lead caught cap-dropped entries stranding on a quiet kernel, kaibo caught
a Relaxed-ordering torn read that could strand a bump behind its own
epoch, and both fixes were the same shape: never advance the cursor past
what was actually delivered, and the stream self-heals by construction.

On the app side, one composition law kept two ambient signals from
fighting over one material: recency bakes into vertex colors as a
relative tint (`tint × base = lerp(base, gold, recency)` exactly), heat
rides the material hue/gain, and `apply_fsn_lod` stays the sole writer.
The room got its long-promised N-archway churn glow (recorded from the
digest's global delta — the stateless `event_bearing` seam was wrong for
absolute counters, and saying so in its doc mattered), a gold ship
silhouette hangs overhead as the you-are-never-lost landmark, and the
walls opened: two panels flanking DATA HORIZON render a sparse world
impression from an off-screen orbiting camera — the app's first true
second-camera render-to-texture, which promptly taught the pre-existing
`single()` camera queries that a second `Camera3d` exists (the fix rode
the same lane). Live-verify closed the loop end to end: `kj vfs activity`
counted a kaish write storm exactly, the windows showed the world from
the room, and a parked-camera A/B caught the gold district cooling back
to violet as the heat decayed. The slip worth remembering: the first
live pass ran an app binary built *before* the ingest stitch — recency
gold masqueraded as heat until the log showed no subscription. Verify
against the binary you think you shipped.

As of 2026-07-13: the Tardis room is furnished, lit, AND windowed — the
fsn world renders the real host tree, warms where the kernel works, and
shows through the N wall without a dive. The kernel publishes its own
activity as lossy-safe digests; `kj vfs activity` reads the counters raw.
Ambient-reframe survivors for later: heat drama tuning (Amy's eyeball),
stage-2 inotify for host weather, the solid/materialized tier, bloom and
search if the browser reading ever returns. The tracks bearing's
breathe-on-jam acceptance and the metronome-click chord pulse still await
the next live jam; theme push-on-change and the remaining compiled-only
color families remain in issues.md. Open work is in `docs/issues.md`; the
live handoff in `signoff.md` (ephemeral, repo root).

## The filesystem joins the band — /r client shares (July 2026)

The idea arrived as one sentence from Amy: reverse the SFTP we already
have, so a client can share `~/Downloads` or `~/src` into the kernel the
way `code .` shares a directory with an editor — patch cables for
filesystems, to sit beside the MIDI ones. The design conversation settled
the load-bearing lines fast: heavy IO stays off capnp (control verbs and
light metadata are fine — the rule was never purity), file bytes ride
SFTP with the roles swapped (the client opens a `kaijutsu-share` channel
and speaks the *server* role; subsystem requests only travel one way, so
the swap is the whole trick), and the share session describes itself with
an in-band `index` TSV manifest instead of a capnp token handshake — the
slash-v "index is the resolver" ethos applied to negotiation, which
dissolved the pairing problem outright.

Two pre-build reviews earned their keep before a line of code existed.
DeepSeek confirmed the role swap and flagged session serialization;
Gemini Pro caught the finding that reshaped slice 0: `VfsOps::read` is
stateless, SFTP is stateful, so a naive pump over a share would pay
OPEN/READ/CLOSE per 256 KiB — ~1.7 MB/s at 50 ms RTT. The fix became the
first thing built: `open_read_stream` on the trait, loop-`read` by
default, held-handle when it matters. Gemini also pointed out that owning
both protocol ends means we can ship nanosecond generations in a vendor
extension rather than accepting SFTP v3's one-second mtime; the built
form landed as a sibling `SSH_FXP_EXTENDED` request (russh-sftp's attrs
have no extension slot) with the required-check riding INIT extension
advertisement — an accidental improvement, since INIT is where version
negotiation belongs anyway.

The build ran the FSN slice-1 playbook: two worktree lanes, Sonnet
subagents on the code, lead reviewing every diff and re-running every
suite. The pump lane landed clean. The share lane built the whole loop —
jailed client server (openat2 `RESOLVE_BENEATH`, ENOSYS-only fallback),
registration with token-guarded unregister, `ShareFs` behind the frozen
mount table — and caught two of its own bugs by running tests (a FIFO
open that blocks forever without `O_NONBLOCK`; an attrs builder ordering
clobber that made every share root a non-directory). A post-build
deepseek pass over the worktree found six more the tests missed, the
worst being a dead `readlink` stub that *lied* (getattr said symlink,
readlink said not-a-symlink) and an `index`-by-name attrs override that
clobbered any real file named `index`. The same agent fixed all six with
regression tests, then stitched the held-handle override — proven by a
counting harness asserting exactly ONE remote OPEN for a four-chunk
transfer, with per-chunk lock scoping so the keepalive and sibling ops
interleave with a long copy. One day, design to stitch: `ad4b212e`
(pump), `99d4e5cd` (share). Live verification against a real kernel is
the open loop; slices 2–4 (`kj share` verbs, `:rw`, notify) wait in
issues.md.

## The beat gets a face — the TRACKER station (July 15)

The East wall had worn a promissory nameplate since the octagon existed:
"TRACKER", a dim marker breathing with the well's loudest beat. Slice 0
of the tracker station replaced the promise with the instrument. Amy
picked the **pattern grid** over a DAW lane-wall and a staff-notation
score wall — the classic-tracker homage turned honest to kaijutsu's own
model: tracks are independent clock domains (`docs/tracks.md`), so each
column scrolls at *its own* tempo past one fixed playhead row, and no
shared row-grid pretends there's a band conductor. Slice 0 shows track
state only (transport, tempo, phrase lines, attached-context dots, the
per-column beat pulse); note cells wait for the score-sync plumbing
decision.

Two design facts carried the build. First, **zero new wire**: the roster
was already polled (`WellTracks`) and the beat phasors already ingested
(`WellBeats`) — the one new API is `beat_position()`, whose `None` *is*
the freeze signal, so a stopped track's rows hold exactly (through the
5-second poll rebuild via an entity carry, and through room re-entry via
a durable map kaibo's review demanded). Second, the render split: rows
move by Transform writes (per-frame-free), text is MSDF plates, the
pulse is quantized change-guarded material writes — Vello RTT was
rejected because a continuously-scrolling face would re-raster its whole
texture every frame.

The review ladder earned its keep in one afternoon: the lead's diff pass
caught a Bevy B0001 query-conflict panic the whole unit suite was
structurally blind to (schedules never initialize in tests — the app
would have died at first frame), plus rows drawing outside the grid band
and the freeze snapping to zero after the stop-poll rebuild. The live
pass caught the phrase-emphasis tiers *inverted* (boundaries read as
gaps in a wall of bright bars) and a header wrapping onto a clipped
third line. kaibo deepseek then confirmed all six design contracts and
found the room-re-entry freeze loss neither earlier pass had. Two tracks
at 120 and 60 BPM scrolling independently on the E wall closed the loop.

## The beat learns to carry its own clock (July 15, afternoon)

Amy's ear caught it during the first tracker-station jam: the metronome
"bumping a few times, not evenly spaced, like some midi is stuck." A
timestamped port tap made it concrete — bursts of ten simultaneous C6
note-ons, then five seconds of silence, cycling — while the bass on the
same wire stayed metronomic. The asymmetry was the whole diagnosis: bass
notes ride render cues with a phrase-length lead into the ALSA queue, so
delivery jitter vanishes; the click follows raw BeatSync references with
no lead at all.

The kernel was innocent — beats fired on time; ticks are fire-and-forget.
The references were stalling behind the musician turn's streamed-output
flood on the single per-connection callback stream, then arriving all at
once, and the receivers folded every buffered reference against one
frame-now, walking the phasor beats at a time. The click scheduler then
amplified the walk: replay-the-backlog on a forward lurch (the blob),
stranded monotonic next_beat on a backward one (the starve). The repo
already knew the answer in the other direction: the MIDI-clock-in path
ships `epoch_ns` with every estimate and back-dates at the consumer. The
forward path even latched the per-beat wallclock — and dropped it on the
floor while building the reference.

So the fix was symmetry: `BeatRef.epochNs` on the wire, each reference
re-anchored to its own emission instant before folding (stale ones
dropped, the phasor free-running on exact feedforward tempo), a liveness
split so a backlogged-but-alive track never gets pruned, and a click
policy worth stating as law — a metronome never stacks clicks and never
silences past a bounded slack; missed beats are missed. The burst
behavior had been *encoded in a unit test* as correct; the test was
rewritten, not preserved. Live verify: 149 consecutive intervals between
499 and 510 ms straight through the model's turns, where the morning's
trace showed zero-millisecond blobs and six-second holes.

The jam also surfaced the next lesson, filed for its own arc: the track's
score outlives every player by design, but injecting the *whole* committed
score into each wake means a long-lived track eventually drowns every
musician that sits down at it — a fresh chair at the morning-old track
opened at 190k tokens. The band view needs a window.

The phase story closed the same day. With the clicks honest, Amy heard
the next layer: click and bass drifting apart — the exact
boundary-jitter failure the 2026-07-02 timebase analysis had predicted
and posted a validator for. Three more fixes landed as one doctrine
(docs/midi.md, "The one timebase"): the kernel grid went
scheduled-periodic (re-arm on the deadline, not the wakeup — lateness
stopped compounding into the musical timeline), render cues got the
same emission stamp beat references had (a late cue now spends its
lateness out of its own lead instead of shifting the phrase), and the
phasor earned Amy's principle as a mechanism — a deadband inside which
it takes zero steps and simply IS the local clock, with stale references
demoted to liveness signals on a ladder. The measurement went from
zero-millisecond click blobs and six-second holes in the morning to,
by late afternoon, four hundred click-to-bass pairs across three-plus
continuous minutes holding a +0.2 ms mean offset with a slope
indistinguishable from zero — and a click grid averaging 500.00 ms
exactly. The day also kept teaching on the side: the track-delete verb's
four live uses found two real gaps in itself (cold tracks after restart,
persisted rows after manual detach), and the jam demonstrated that any
track played continuously for a few hours drowns every musician that
sits down at it — the windowed-band-view problem now filed as the next
real design arc.

## The stolen bridge (August 2–3)

For weeks the MCP shell carried an intermittent ~5-second tax that read
like a haunting: the kernel finished every command in milliseconds, the
reply just didn't arrive until a stall-fallback resubscribe went and
fetched it. The 2026-07-17 `SubscriberHealth` rewrite had already fixed
the *long* hangs, but this residue survived reboots and defied the usual
staleness stories. The diagnosis, when it landed, was one line reading
another: the server dedupes block-event subscriptions by (principal,
instance) — correct, so a reconnect replaces its own dead bridge — but
`kaijutsu-mcp` passed the literal `"mcp-server"` as its instance, so
every concurrent MCP process for one principal was *the same client*.
Whoever subscribed last silently evicted the rest, and an evicted client
was never told; its channel just went quiet until its own next call
stole the slot back. Five live processes, measured: 5419 ms for a
146 ms echo. The app had the identical bug spelled `"bevy-client"` —
two windows trampling each other — while the correct shape,
`app_peer_instance()`, sat one layer above it minting per-process UUIDs
for the peer registry. The fix was to let both clients use that shape
for the subscription too, and the lasting lesson rode along as
observability: the registry now remembers *which connection* registered
each bridge and warns when a different one displaces a live entry,
carrying the evicted subscription's age — the signal that turns this
class of theft from an afternoon of forensics into one journal line.
Truncation honesty shipped the same day: a kaish result that spilled its
output (exit remapped to 3) no longer reads as failure on the MCP path —
error-ness is judged by the command's real exit, with `[output
truncated]` and a structural `did_spill` keeping the cap unmissable, so
a model neither retries a command that succeeded nor reasons over a
head+tail excerpt as if it were whole. And kernel.db — 287 MB of every
conversation we've ever had, one WAL-mode file with no backup story —
got its first: `kj db backup` (SQLite's `VACUUM INTO`, consistent
against a live writer), `kj db checkpoint` for the snapshot-your-own
crowd, and restore deliberately left a documented procedure instead of a
verb, because a live file swap under a kernel full of in-memory state is
a lie waiting to be discovered.

## Contexts join a band — SQL-native model config and casts (August 3)

The afternoon's seed ("consider adopting kaibo's config and cast concept?")
became the evening's renovation. Reading kaibo's resolved config against our
`llm/` layer made the gaps obvious: the provider table's NAME was its TYPE, so
a second openai-compatible endpoint needed a blessed name; tunables (effort,
thinking budgets, sampling) existed nowhere; and the hosted-OpenAI path had
been quietly broken since GPT-5.x started rejecting `max_tokens` — a live
probe confirmed it in one curl.

The design conversation took two turns that mattered. First, Amy pushed past
"adopt kaibo's TOML": *"I'd like to see the cast data modeled in SQL directly
and that's the source."* models.toml — the CRDT doc, the embedded asset, the
whole TOML parse layer — was demolished, replaced by normalized tables
(backends with a name/kind split, casts, cast_slots, aliases, llm_defaults)
edited through `kj backend`/`kj cast`/`kj alias`, with the registry rebuilt
live on every write. No restart to change which model runs. Bootstrap is
agent-driven: read a colleague's config, run the verbs. Second, presets
survived on purpose. The near-miss was absorbing them into casts; reading the
actual code showed presets are patch recall over verb args (fork bases,
consent), of which model-pinning was only the underused corner. So the
concepts split cleanly — **cast = who plays, preset = the patch** — and a
preset now references a cast instead of pinning provider/model itself.

Roles are context_types, the same word rc dispatch already keys on, so `kj
context create --cast house --type coder` seats a context in the band and the
turn path resolves explicit override → cast slot → default through one pure
function. Existing contexts rolled over idempotently (keep the surviving
backend names, rename openai→gpt, toss the rest to deepseek-v4-flash). Three
lanes ran it: Opus built the schema/registry/verbs, two Sonnets in one tree
with file fences did tunables-to-the-wire and cast-on-context, and the lead
stitched the seam where they met. The first live proof was a `budget`-cast
probe context resolving `deepseek/deepseek-v4-pro via CastSlot` in the
journal on its first turn. Old aliases didn't make the trip — Amy: "they were
guesses" — the floor ships four backends, zero aliases, zero casts, and every
row above it is something someone chose.

## Errors that were only strings (August 3–4)

Amy's rule arrived as a one-liner — *DB errors are P1, we fix them now* —
and the kernel spent two days proving why. It started with a warn that had
been scrolling past every restart since mid-July: `UNIQUE constraint failed:
contexts.context_id`, once per archived context. Not a race, as everyone
assumed, but two different definitions of "KernelDb already knows this
context" sitting a few lines apart: the presence check asked the *active*
set while the primary key covered the whole table, so every archived
context looked missing forever and got re-offered on every cold start. The
PK had been quietly doing the real work — and quietly preventing an
archived context from being *resurrected* by a placeholder row, which is
why the obvious idempotent-insert "fix" would have been the dangerous one.

Pulling that thread surfaced the sibling one file over. `create_document`
decided whether a failed insert was benign by asking `e.to_string()`
whether it contained "UNIQUE constraint" or "already exists" — and the
string it was matching came from `map_unique_violation`, which flattens
*every* constraint violation into one message. Two failures wore that one
disguise: a primary-key conflict (the same document, genuinely benign) and
a partial-unique conflict on `(workspace_id, path)` — a *different*
document claiming a taken path, which is divergence. Both got the same
cheerful "recovering" warn. And the whole recovery hung on wording: reword
the message and every duplicate-document recovery becomes a hard error, no
test the wiser. The fix refuses to read messages at all. On a constraint
violation the DB layer *reads itself back* — is there a row at this id? is
there a row at this path? — and returns a typed answer, so classification
depends on the database's state rather than its prose. Above it, the benign
arm now compares the persisted row against the one it meant to write; kind,
workspace, or path differing is `DocumentDiverged`, never a recovery.

The same day's third strand was the drift router's lost+found. Its dead
letters — drifts that failed every retry — were written into a context the
router minted and registered itself, then persisted by a caller that logged
`tracing::error!` and carried on when the row wouldn't write. Three lines
above sat the comment explaining the invariant being broken: a registered
handle implies a KernelDb row. The router turned out to be the wrong place
to hold the pen, since it has no DB handle and can only ever produce a
rowless handle; it now *claims* an id whose row someone else has already
written, and `ensure_lost_found` is gone so the old shape can't be
rebuilt. The flush secures the sink before draining, returns an error
naming what it flushed instead of a log line nobody reads, and hands a
failed dead-letter write back to the queue — the one code path whose whole
purpose is not losing failed drifts had been dropping them on the floor.

Then the live run found the joke in it. Restart the kernel, orphan a staged
drift, ask for a flush: *nothing to flush*. The staged queue drains
per-caller; the dead-letter queue is kernel-global; and the early return for
an empty caller queue sat above the global half, so dead letters were only
ever written when the same caller happened to have something else staged —
never in the case the sink exists for, which is everything having failed. The
existing test had met this behavior and *accommodated* it, in a comment that
reads, now, like a confession: "the flush early-returns when the caller's
staging is empty, so we need both a deliverable item AND a dead letter
present at flush time." All three fixes were unit-green before that flush
ever ran, and none of them would have found it. What did was typing the
command on a live kernel and reading the answer.

## Tasks join the block model — the household-agent arc opens (August 4)

A gap-analysis session against two flagship agent harnesses — hermes-agent
and QwenPaw, both cloned and read cover to cover — turned up one gap kj
actually had to fix: no task state. Both harnesses lean on a todo tool
writing JSON to disk; Amy's read was immediate — *"Task BlockKind and tool
is a great idea"* — and the reason why fell out of the block model itself.
A task that's a block gets the CRDT for free: create it here, watch it
converge everywhere, no second store to keep honest.

The design took less debate than expected once the precedent was found.
`content_type` had already solved "one more mutable field on every block,
independently LWW-clocked, cheap to add" — a `Copy` field on `BlockHeader`
plus its own Lamport timestamp, merged by the same `field_wins` tiebreak
every other per-field register uses. `task_status` is that mechanism
again, verbatim, which meant the CRDT plumbing — `content.rs`'s
`set_task_status`, `block_store.rs`'s wrapper, the wire fields on
`BlockSnapshot` and the `MetadataChanged` bundle — was less a design
problem than a typing exercise. The one real decision was what status
even means: reusing the existing `Status` enum (Pending/Running/Done/
Error) was tempting and wrong, because `Error` means "the tool crashed"
and a cancelled task isn't a crash, it's a choice. `TaskStatus` got its
own four values and its own tie-break order — Cancelled outranks Done on
a timestamp tie, the same way Error outranks Done in the original —
documented rather than left to accident.

The nuance the task brief flagged in advance turned out to be the real
one: tasks get edited constantly, mid-conversation, and kaijutsu already
has a hard rule against exactly that shape of block — the daily
system-prompt-cache invalidation `BlockKind::Notification` was built to
dodge in July. The Notification precedent transferred clean once its
actual mechanism was understood, not just its name: it isn't that
Notification blocks are special, it's that `ConversationMailbox` only
ever translates a given `BlockId` once, so a block whose fields mutate in
place after that translation simply never gets re-rendered into the live
conversation — the already-sent bytes stay the already-sent bytes,
cache-safe by construction, no special hydration path required. Task
inherited that for free. What it does *not* get for free — and what got
written down rather than built — is the case of Amy completing a task
from the app while the model isn't looking; that one wants a companion
Notification block on the same pattern that already exists, deferred
because nothing forces it yet.

The MCP surface turned into a smaller decision than the doc comments
suggest it should have been: a fifth builtin server, `builtin.tasks`,
sitting next to `block.rs` rather than inside it, for the same reason
`builtin.shell`/`builtin.shell_readonly` already split — a household
agent should be grantable "groom tasks" without also getting arbitrary
block surgery. Reaching Claude Code needed nothing at all: the MCP
slim-down from a few weeks back had already turned the external server
into a generic `kaish_exec` dispatcher gated by broker capability, so a
new builtin instance is visible to every `*`-loadout role the moment it
registers. The tests found two real bugs on the way in — a Lamport-tie
test that wasn't actually testing a tie (the second peer's clock starts
one tick ahead after a one-way sync; the fix was an equalizing round trip
before the race, now called out by name in the test comment so nobody
"fixes" it back into a false pass), and a `task_list` filter that
validated the filter string per-candidate instead of up front, so an
invalid filter silently passed on an empty task list. Both are exactly
the shape of bug a fresh feature is supposed to shake out before anyone
depends on it.

Codex later joined this same instrument through two deliberately separate
channels: its MCP subprocess receives `CODEX_THREAD_ID` for stable identity,
while command hooks translate Codex lifecycle events into the existing generic
hook protocol. The split matters because MCP identifies the player but cannot
observe prompts, compaction, or session boundaries. A source-aware model map
keeps Codex models on `codex-app` and Claude models on `anthropic`; unknown
sources leave the provider alone instead of guessing. Codex hooks use a tight
fail-open budget so a slow kernel never makes the coding loop feel sticky.

## The day toad played kaijutsu (August 5)

The household-agent foundations were one night old when Amy opened the day
with a verdict and a wish: *"Using toad with kaibo was a delight. I'd like
to have some subagents work on the ACP adapter."* Four lanes launched
before lunch — the deferred cast toil, the deepseek review queue (where
soft-cancel crystallization got decided on purpose rather than left as an
accident of the variant), the Ask pathway for permissions, and the
headline: `kaijutsu-acp`, a thin ACP v1 bridge in the exact image of
kaijutsu-mcp. The adapter lane had prior art nobody had planned for — the
kaibo ACP worktree was sitting in ~/src/wt, the very adapter toad had been
talking to the day before — and it returned the day's most satisfying
finding: the session picker could serve the app's ring-0 rank with zero
schema changes, because the ring stamps already rode the wire and the
seating function was pure. One seating engine, two frontends; seat 2 on
the desk is seat 2 anywhere.

Then Amy bounced the kernel and the first toad flight died on its first
prompt — and pulling that thread found the day's real monster. The
task-blockkind merge had added a field to a struct that rides the at-rest
oplog CBOR, without a serde default. Nobody had restarted the kernel
across that merge, so the breakage sat latent until the bounce, then
detonated all at once: every pre-task document undecodable, rc scripts
unreadable, therefore no create-time capability bindings, therefore
deny-by-default locking every facade including the operator's own kj. A
missing attribute became a total lockout through four links of chain. The
skip-not-truncate durability design held — nothing lost, everything
replayed once decode was fixed — and the lessons went straight into the
backlog: new at-rest fields decode old bytes or they don't merge; forty
quiet per-document errors should be one loud aggregate; and an unbound
context that denies its own operator is mistake-prevention behaving like
an auth wall, which the instrument stance explicitly forbids.

The afternoon became the best shakedown this project has had. Toad flew;
things broke; every break was real. Four incidents traced to one kernel
defect — the FlowBus was a shared broadcast ring, so one slow subscriber's
overflow silently evicted events for everyone — and the adapter grew
defensive layers (a quiet-poll on the turn wait, catch-up resync, a
trailing-edge sweep) that each taught the true shape of the problem before
the real fix landed. Amy set the doctrine in one sentence: *"no lossy
solutions. I'd rather be disconnected."* The Opus rework built exactly
that: per-subscription bounded queues, lossless-or-terminated for ordered
topics, an explicit kick that drops the connection so even a version-blind
old client recovers through ordinary reconnect-resync. Musical time kept
its own law — timing topics stay latency-first and drop-oldest, missed
beats stay missed — and the token firehose got coalesced at the forwarder,
batching calls but never concatenating buffers. The quiet contract
underneath is the part worth keeping: sequence numbers mean a gap can now
only be "terminated or resubscribed," so a gap without a termination is by
definition a kernel bug, and the client logs it as one. Silent loss
stopped being a representable state.

Losslessness had one more lesson to teach: per-lane guarantees say nothing
about ordering *across* lanes, and the turn-completion event started
beating the final text chunks to the bridge — truncating exactly the
report the user was waiting for. The bridge learned to settle delivery
before answering the prompt. Then the arcs converged: Task blocks, one day
old, wired into ACP's plan updates — cancelled tasks omitted because a
plan is what the agent intends to do, subtasks flattened, identical
rebuilds silent — and Amy's flight report closed the loop: *"that task
tracker worked perfectly though."* Grooming a CRDT task block anywhere now
moves a checklist in every connected frontend.

What toad taught that wasn't ours to fix: it doesn't consume ACP's session
list (its picker is its own database), and its thought widget accumulates
upward where nobody is looking. What it taught that is ours: a fresh coder
context with a full toolbelt answers a simple question with a 63k-token
expedition, rediscovering kaish's parse rules the hard way every session —
the coder stance owes the model both proportionality and a primer, and
connecting clients want identity-keyed presets (ACP's initialize already
says who's calling). Review earned its keep all day — a hydration-skip
suggestion from an external reviewer that would have made the leak worse,
a chained-lock self-deadlock in a merged lane, a settled behavior change
pinned by test instead of comment. And the crosstalk stance stopped being
theory: while the model toured the repo in Amy's toad session, the lead
watched the same blocks from the kj side and Amy watched from the app —
three players reading one score, which is what the instrument was for.

## The instrument changes its strings (August 12)

Two upgrades landed in a day that a scouting report had said would take
several, and the interesting part is why the estimate was wrong in that
direction.

The Bevy 0.19 plan was written by reading all 104 official migration guides
against the app rather than from memory — deliberately, because model
training data predates even 0.18's own event-system rename, so any
recalled claim about 0.19 is suspect by construction. The verdict:
**seven of 104 guides touched us.** The framework's headline reworks —
the text stack onto parley, resources-as-components, render-graph-as-
systems, the extract refactor — were all near-misses, because we had never
used the APIs they changed. We own our shaping and our MSDF atlas, our
render-world code was already written as plain systems on `ExtractSchedule`,
our UI is hand-rolled, and our 687 `Res`/`ResMut` sites kept their sugar.
The upgrade was small; it had only been wearing a scary hat.

What was actually load-bearing was a dependency whose version number lied.
`bevy_brp_extras = "0.19"` reads perfectly current and requires
`bevy 0.18.1`; the release that wants Bevy 0.19 is 0.21. So the one crate
pinning us to the old engine was the one that looked most up to date — and
because BRP is how agents drive the live GUI, it was pinning the testing
loop too. The lesson generalizes past this crate: **read a dependency's
manifest, never its version number**, and prefer the cargo registry cache
as truth, because a working checkout of upstream can be five months stale
while you plan against it. That one nearly bit us too.

The second blocker was invisible to every guide, because it was ours:
`avian2d` had been declared and never used — no import, no symbol, one line
in a manifest — and it hard-pinned Bevy 0.18. A physics engine nobody
called was the thing standing between us and the upgrade. Amy ruled delete
over bump, which took 440 lines of lockfile with it. Dead dependencies are
not free; they vote on your version constraints.

Sequencing was the other real decision, and it was made before any code
moved. Bevy 0.19's `bevy_audio` wants rodio 0.22, which deleted
`OutputStream` and `Sink` outright — the exact API the 868-line scheduled
playback engine was built on. The tempting order was to bump Bevy and let a
second rodio copy sit in the tree. Amy ruled the opposite: **migrate rodio
first, on its own, then upgrade onto an already-converged audio stack.**
That kept the "one copy of rodio/cpal" invariant true throughout instead of
briefly false and then repaired, and it let the playback rewrite be
reviewed and bisected as itself rather than as noise inside a framework
bump. The manifest comment that had promised dedupe would return "when Bevy
0.19 brings bevy_audio onto rodio ^0.22" got to become simply true.

The upgrade found exactly one thing no guide mentioned, and it was a real
API split rather than a rename: 0.19 divided `UiDebugOptions` into a
per-node `Component` and a new `GlobalUiDebugOptions` resource, so asking
for the old one as a resource stops compiling in a way that says "not a
`Resource`" rather than "moved." Everything else was mechanical, and the
widest change — 25 `Assets::get_mut` sites now returning `AssetMut<A>` so
that `AssetEvent::Modified` only fires on real mutation — was applied from
rustc's own machine-applicable spans rather than by hand. The compiler
knows where the bindings are; there is no reason to guess and no reason to
take credit for finding them.

One habit paid for itself twice. A workspace test failed after the bump, in
a crate with no Bevy anywhere in its tree. The cheap move is to reason that
it can't be related and move on. Instead it got checked out at the pre-bump
commit in a worktree and run there, where it failed identically — so it went
into the backlog as a pre-existing drift-lane regression with proof, rather
than as a suspicion. Inference would have reached the same answer here.
Sooner or later it won't, and the worktree costs two minutes.

## The denial that pointed at a locked door (August 12)

The sharpest item in the kernel backlog had been sitting there since the
task_status boot flood in early August, wearing a description that was
wrong. When a context's `create` rc lifecycle failed at its binding step,
the context was created anyway, holding nothing — and the entry called that
a total lockout. It isn't. `kj context switch` and every read verb are
ungated *by design*, and the code says so in as many words, so the operator
can always walk out of a broken context and can always read the Error blocks
the failed lifecycle left behind. Diagnosis works. Only action is blocked.

Amy's question was the one that unlocked it: **when would `create` actually
need to abort?** Almost never, it turns out. A failed stance script costs a
system prompt; a failed cache script costs tokens; only the binding step
leaves something inert. And even there, a freshly-created context holds
nothing worth saving, so abort and create-then-discard cost about the same.
The case for aborting was ergonomic, not safety — and aborting has a cost
the bug doesn't: it destroys the Error blocks that explain *why*, which is
the one part of the story that currently works.

Reading the gate turned up something sharper than the entry recorded. The
denial didn't merely fail to mention the exit; it advised one that was
locked. Every refusal ended with *grant with `kj binding allow`*, and the
binding-write authorizer refuses widening from any caller that is neither
privileged nor binding-admin — which is exactly the caller reading the
message. Underneath it, the same line was collapsing a KernelDb read
*failure* into "denied," so a database fault and a missing grant were the
same sentence. That is the August 3–4 lesson resurfacing in authz clothes,
in the one copy of it the broker's fix hadn't reached. And a third: a failed
lifecycle was a `tracing::warn!` under a plain "created context" success
line, so the operator was told it worked and found out at their first real
verb.

So the fix came in two halves that meet. The gate now has three outcomes
and keeps them three — DB failure, no usable loadout, missing capability —
and the unbound case names exits that exist: `kj context rebind`, or
create-switch-remove, which had worked all along as oral tradition and was
written down nowhere. Creation stays un-aborted and simply stops lying,
reporting the outcome it produced. And `kj context rebind` re-runs the
`create` lifecycle on a context that has none, ungated on precisely the
argument that leaves `create` ungated: the loadout comes from rc, not from
the caller, so a rebind grants what birth would have granted and nothing
else. Gating it on `Operator` would have put the repair behind a capability
the broken context cannot hold — the lockout itself, rebuilt as a feature.

Both halves gate on the *outcome* — "does this context have a usable
loadout?" — never on which script failed. Keying on script identity would
hard-code rc layout into the kernel and would miss the quieter case: a
binding step that ran to completion and bound nothing. The test that matters
most is the one asserting `rebind` is not capability-gated, because that is
the decision a future refactor is most likely to undo while tidying.

One test earned its keep by failing for an honest reason. The repair test's
rc script reported success while the loadout stayed empty: `test_dispatcher`
leaves the broker's DB handle unset, so `kj binding allow` had written to a
cache the authorization path never reads. Wiring the handle as production
does made the test faithful — and made the fixture's own gap visible instead
of letting a green run paper over it.

## The mirror that stopped being a mirror (August 13)

`kaijutsu-mcp` was a CRDT replica. It held a `SyncedDocument` of the joined
context, authored blocks into it, pushed the resulting ops upstream, and
maintained the whole apparatus that keeps a replica honest: a sole-writer task,
a command channel, resync coalescing, a pushed-frontier, an event bridge. By
the end of one day it held none of that, and the thing that replaced it is a
single integer going up.

The day did not start as a demolition. It started with a schema question —
there was no block-authoring verb anywhere in the `Kernel` interface, and the
tempting shortcut (`block_create` via `executeTool`) hardcodes `after`, status
and content-type, and parses a `metadata` argument it never reads. Tool blocks
routed through it would arrive with no name and no input **and nothing would
error**. So `authorBlock`/`completeBlock` were appended at the next free
ordinals, refusing two things rather than papering over them: an unparseable
principal, and a ToolResult without its call's id.

Amy's framing is what made the shape right. The earlier design had wanted an
atomic `authorToolPair`, on the theory that a ToolCall without its result is
corruption. Her model dissolved that: *a tool call should have a quick lock at
startup, then run independently of other tool calls* — so the atomic part is
only the **reservation**, and a ToolCall sitting at `Running` is a legitimate
pending state, not an orphan. Reserve, then flow.

Then the layers came off in order, and each one was smaller than it looked
because the previous one had removed the reason for it. Authoring moved to RPC,
which meant the mirror had no local writer, which meant the resync's pre-fetch
flush and its abort-on-failure guard were protecting against a hazard that
could no longer arise — two documented races gone by construction rather than
by guard. The cold readers moved to server queries. The shell completion poll
moved last, and inverting it was the key: **polling became the guarantee and
events became a hint**. Before, the event feed was the mechanism and an
authoritative catch-up was the emergency — so the common path trusted a cache
fed by exactly the feed whose death the emergency path existed to detect. After,
a dead feed is not a condition to detect and recover from; it just means nothing
arrives early. Nothing to detect is nothing to get wrong. Two filed bugs died in
that inversion without being fixed.

With the last reader gone the mirror was write-only — maintained for nobody —
and 624 lines of replica machinery went with it.

The same consolidation later reached the hook boundary. Claude Code and Codex
hooks had entered through two Bash programs and two jq maps before reaching a
Rust client that already owned compaction, socket discovery, session matching,
transport, and deny handling. `kaijutsu-mcp hook claude|codex` absorbed both
native protocols, including their response envelopes, and the wrappers
disappeared. The identity distinction stayed explicit at that boundary:
`session_id` is the hosting conversation/thread used to select and stabilize a
listener, while `agent_id`/`subagent_id` is only the principal acting inside
that session. Collapsing those two would route a subagent event to a different
conversation; preserving them made the adapter deletion an ownership cleanup,
not a protocol change.

**What the day was really about was tests that cannot fail.** Every defect found
passed careful reading and died on execution, and there were five. A cancellation
leak the refactor itself created: `with_hook_budget` is `tokio::time::timeout`,
which *drops* the future, so a budget expiry between the reservation and its
completion stranded a ToolCall at `Running` forever — the old design could not
have that bug, because the pair was written under one lock with no await between
them to cancel at. Splitting a local critical section into sequential awaits
creates cancellation windows that did not exist. Worse, the design doc had argued
`Running` was acceptable *because it was transient*; the refactor falsified the
premise and left the argument standing.

A wire field nobody read — `completeBlock` shipped with an `isError` the server
ignored, which is precisely the `block_create` defect that justified building the
verb, written down in the schema and the commit message and reproduced one
ordinal later anyway. Writing the rule down did not prevent it; a reviewer
reading the handler did. Made a refused-on-contradiction check, it caught a live
inconsistency in our own hook path on its first run.

And the sharpest one, because it was invisible until something forced it: the
test guarding "shell survives a dead event feed" ran `echo`, which finishes
server-side before the first poll. Under the old design the mirror *could not*
hold the answer with the feed dead, and that alone is what made a fast command
exercise anything; the moment the poll asked the server instead, the test proved
"one query works" and nothing else. It was found by deleting the poll floor and
watching the test pass anyway.

The habit that worked every time was not review. It was **running the exact
thing against the real system** — falsifying each new assertion by breaking the
code under it, and probing idioms in a live kernel instead of reasoning about
them. That is also how the day's other lane found that our own bug report was
too kind: we reported a shell expansion yielding empty, and their re-probe found
the word vanishing from the AST entirely, so inside quotes it produced a *wrong
path* rather than a missing one. A report describes what was visible from where
you stood; a probe finds the shape.

An untested mechanism is a claim, however carefully its prose is worded.

## The instrument could not say who was in the room (August 15)

The day's first task was a function that was correct, tested, and called from
nowhere. `roster_sources::spawn_periodic_refresh` had shipped with the live
roster the evening before — unit-tested against `refresh_once`, and never
wired into the server's boot, because the branch that built it could not start
a live kernel and declined to add an unverified call. That is a shape unit
tests structurally cannot catch: the function is right, its caller is absent,
and every test of the function still passes.

So the test that fixes it reads no roster surface at all. Both read paths
self-heal the boot rule inline, which means a test that touched one would have
passed with the spawn deleted — the only honest assertion is the one that
refuses to look. Proof it ticks came from the live kernel rather than the
suite: sampling `/run/roster/index` at twelve-second intervals returned
`recorded_at` values exactly ten thousand milliseconds apart. The samples land
on the loop's own grid instead of on the read times, which is the difference
between scheduled-periodic and read-triggered, and no unit test can show it.

Then a `cat` of that index returned exit code 3, and the thread it pulled ran
all day.

kaish caps captured output by replacing it with a preview and remapping the
exit code — deliberately, so an embedder can tell. The remap is right. Its
*audience* was wrong: it also reaches the running script's `$?`, so inside a
kaish program a command that succeeded and merely printed a lot reads as
failed, and `set -e` and `cmd || fallback` both take the error branch. Our MCP
tool already unwrapped it correctly; rc bodies and hook bodies did not — and
the approval gate's classifier escalator is designed to be an rc script that
branches on a captured response. It would have escalated on a *good* long
answer.

The fix was to stop asking "how much do we trust this caller" and start asking
**who consumes this output**. Model-facing shells keep the cap, because bounded
output is the point there. rc bodies, hook bodies, and the editor's `:r !cmd`
splice — which pastes command output into a document, where a head-and-tail
preview is not truncation but forgery — get a runaway backstop instead. The
test pins both halves, including kaish's current wrong behaviour, so that when
upstream fixes it the test fails and says the workaround can go.

The same investigation falsified our own filed report in both directions. We
had recorded that truncation set no failure code (it does) and that command
substitution silently truncated at 8 KB (108896 bytes now round-trip intact).
The correction deliberately does not conclude the original was imagined —
someone watched that happen, and a negative probe is a claim about the probe.

The roster's size turned out not to be a leak but a shape: its `recent` source
is one row per non-archived context, so it rendered 199 rows to report three
live entities, over the model-facing output cap, which is how a model asking
who was around got a truncated splice of mostly-dead rows. Filtering it to
"who is around" — hiding only what we positively know is dead, because
`live == None` means *unknown* and a status-only entity has exactly that shape
— dropped it to four rows. And the filter immediately exposed a bug the 195
idle rows had been burying: the CLI rendered the same principal twice, because
presence rows are per-connection and the VFS had always grouped by entity while
the CLI never did. Two surfaces disagreeing about the same data is how "the
roster is flaky" starts.

**The part worth keeping is what the machine could not do.** Clearing 194
stale contexts needed a safety filter, and roster liveness looked like exactly
the right one. It is not: `recent` means "appended a block in fifteen minutes",
not "someone is attached". It reported four live contexts while twenty-four had
been active that day and a Codex lane sat mid-review, connected and thinking.
Archiving on roster-idle would have soft-deleted attached sessions' contexts.
Nothing in the instrument said so. Amy did — *"there should be moltar app, this
claude code, maybe subagents, and a codex session working on acp"* — and
checking that against the process table found every one of them. The rule that
came out is dull and the way it arrived is not: **use last-activity age, and
treat "attached" as a question the roster currently cannot answer.**

Replacing ROOT then made the same point structurally. ROOT is special by
convention — a label plus a promotion — while every generic mechanism treats it
as an ordinary context. The three-hour sweep would have taken it at 29 days
idle. Label uniqueness locked its own name against reuse, reporting the label
both "already in use" and "not found". And archive cascades to structural
children, so parenting the new root under the old one for honest lineage and
then archiving the old one destroyed the new one — the confirm prompt had said
`1 children`, which is inventory where it needed to be consequence. None of
those are bugs in those mechanisms. Each is correct for an ordinary context.

Amy settled the shape: *"I had thought to make it a dag but the data is
naturally a forest and drifts create cycles if you count them."* The code
already agreed — `insert_edge` cycle-checks `Structural` edges only, leaving
drift exempt by construction. Checking that invariant was actually enforced
turned up a real bug: `kj context move` deletes the old parent edge before
inserting the new one, with no transaction and with cycle detection inside the
insert, so a *refused* move orphans the context it refused to move. Which is
also, wryly, the only way to make a detached context from `kj` today.

Anchors are the answer, and their justification is not tidiness but fork cost:
an anchor is what you fork from, and forks copy history, so every block that
lands in one is paid for again by every descendant forever. The old ROOT
carried ninety.

Two lessons, and they are the same lesson from opposite ends. A mechanism can
be correct in isolation and wrong in place — a function with no caller, a
signal aimed at the wrong audience, a specialness that lives only in a label.
And the loop is not decoration: the fact that prevented the day's one
irreversible mistake was held by the human, because the instrument had no way
to represent it.

## The melt begins, and finds two armed fields (August 15)

The CRDT position paper had already ruled: one authoritative kernel sequencer,
rich RPC authoring, projected event streams, no client dependency on the text
engine. What began this day was the migration itself, and it went one step past
the paper — replacing CRDT-shaped durable storage wherever semantic kernel
operations are sufficient, rather than only closing the client boundary.

Amy's rulings came first, because the config half could not start without them.
The four config roots melt into one git worktree, one commit per accepted
mutation — the git log *is* the config oplog, which is the whole reason to
prefer files over documents. Seeding stays bootstrap-only through the migration:
a deleted file stays deleted, new shipped defaults do not appear, and the
migration does not also introduce tombstones. And no client outside the repo
uses the raw push/sync RPCs, so they could be frozen outright instead of
carrying an open-ended compatibility promise.

One ruling was a correction. The plan had made honest per-mutation provenance a
precondition for the git work, since a commit is supposed to record who asked.
Amy declined the gating: *"I don't think the principal plumbing should gate the
git work. Let's make a local note to do a sweep across the code and look at
principal plumbing holistically."* The gap is real but it is a pattern across
seams, not a config bug, and fixing it under whichever lane happens to be
standing there fixes it in exactly one place.

### Phase 2, and the comment that kept it alive

The MCP shell-completion path had two phases. Phase 1 waited for a tool result
to reach terminal status; Phase 2 then pulled the entire context snapshot,
decoded the oplog into a throwaway document, and re-read the same block. Its
comment justified the second read carefully: content, exit code and status ride
three independently-reorderable topics, so an observed terminal status does not
prove the rest has replicated.

The argument was sound. It was also about a local mirror that the August 13
demolition had already replaced with an authoritative server query — a fact
visible in the variable still named `local`. Phase 1's result had been complete
for two days. The comment kept arguing for machinery that no longer fed it, and
because the argument read as current, nobody re-derived it.

Two independent traces — an outside model reading the real code, and the
implementing agent — checked field parity, write ordering, lock coverage and
output caps before anything was removed, and agreed. Phase 2 was deleted rather
than reimplemented, and with it the last production oplog decode in the MCP
crate. The lesson generalizes past this one function: a stale comment is not
cosmetic debt, it is a false premise parked where the next reader will pick it
up. Two more were found and corrected the same day, one of them citing three
schema ordinals that were all wrong.

### The fields nobody read

The audit that mattered came out of a throwaway question — whether a projected
block query returns the same snapshot as a decoded sync payload. Field for
field, almost. Two exceptions: `excluded` and `created_at` were both serialized
by the server and never read back by the client. Every block that arrived over
the projected path reported itself as not excluded, and reported the moment it
was parsed as the moment it was created.

Neither was doing damage, and that is precisely why they had survived. The app
and ACP still read blocks off the sync payload, which carries both fields
correctly. But the remaining work in this lane is moving those two clients onto
projected queries — which would have converted both gaps, on the same day, into
silent loss of user-curated exclusions and the corruption of every block's
creation time. The time well seats contexts on rings by idle age; exclusions are
an explicit invariant of the migration.

So: a wire field that no client currently reads is not dormant. It is armed, and
the migration is what pulls the pin. Both were fixed before the clients moved,
and a full field-by-field sweep confirmed they were the only two — with the
nested payload structs recorded honestly as spot-checked rather than audited.

The zero case for `created_at` got its own decision, and it is the house style
in miniature: propagate a zero faithfully rather than substituting "now". A
visibly absurd 1970 timestamp is debuggable. A silent substitution makes an
upstream defect indistinguishable from a correct fresh block.

### A flake that was not one

A kernel test was failing on main, and the first agent to meet it reported a
pre-existing flake. The claim was true and the explanation was not. The test
asserts that `mount` runs and exits zero in an exec-granted shell; `mount` on
that host prints fifteen kilobytes against an eight-kilobyte output cap, and
kaish remaps a capped command's exit code to signal the truncation. The test was
reproducing a real bug, and it passes anywhere `mount` happens to print less —
which is exactly why it reads as noise.

Following it found the durable half. An earlier investigation had concluded that
the tool-facing callers were safe, on the strength of one call site that consults
the preserved original code. The path that writes the durable record does not:
a command that exits zero but prints past the cap records a failure code on its
result block, permanently, while its status still reads as done. Wrong data, at
rest, wearing a healthy status — filed rather than patched, because the fix
belongs with whoever also makes that test's dependence on the host's mount table
explicit instead of incidental.

## The day the wire stopped being a storage engine (August 15, later)

The melt's first day ended somewhere its plan had not imagined, because the
constraint the plan was written under turned out to be optional.

Everything until then assumed the wire was near-frozen: additive changes only,
freeze a method before deleting it, negotiate a capability bit so old and new
clients could coexist. Amy lifted it in a sentence — the protocol is not locked,
flag-day changes are fine where they reduce technical debt, every client is
in-repo and rebuilt together. Later she added that the app could break and be
rebuilt, and that ACP could break too, since it is still experimental.

Three things fell out immediately, and the third was the one that mattered.

The first was that freezing collapsed into deleting. `pushOps` and
`pushInputOps` let a client push raw CRDT operations into kernel documents; they
had no production callers and had not for some time. Deleting them removed about
a thousand lines.

The second was that the deletion was worth more than its line count. The
kernel's `merge_ops` had exactly one caller — the `pushOps` handler. Oplog replay
does not use it; replay applies a document's own history in order and never
reconciles a concurrent branch. So removing that handler did not merely retire
dead code, it made concurrent merge into kernel documents **impossible**. The
migration plan had listed, as the gate on replacing the CRDT text engine, an
instrumentation task to measure whether non-trivial merge ever happened inside
the kernel. That instrument had been built earlier the same day. It was deleted
a few hours later, along with a sibling counter in the same position, because a
structural impossibility is a better answer than a metric reading zero forever —
and an unreachable instrument is worse than none, since it implies something was
measured.

### The design that was right in shape and wrong in placement

The third consequence took two reviews and a wrong turn to find.

Replacing the raw-operation text projection needed a replacement, and the first
proposal was two events: append a suffix, or replace the whole content, chosen
structurally by whether the new text starts with the old. A DeepSeek review
confirmed the shape and refuted the reasoning — the document claimed one tool
could produce a non-append change and there were five, and it never said *how*
the server would classify. The safe rule is content comparison, not a list of
tool names, because a list can be wrong and a comparison cannot.

Then a Gemini review, asked specifically to counter our anchoring now that the
additive constraint was gone, found the deeper error. Classification had been
specified *at the wire*, against a per-subscription record of the last text sent.
That is impossible and expensive at once: the internal event carries opaque CRDT
bytes, so a bridge cannot classify without linking the very library being
removed, and per-subscription tracking means one string buffer per block per
subscriber. Classification belongs inside the mutation lock, where both texts are
already in hand.

The same review found that gap recovery could not be implemented at all as
written, because the snapshot query returns no version — so a client cannot know
whether a queued append is already included, and applying it twice corrupts the
text.

And it argued for something larger: not two events bolted onto an interface of
thirteen, but one ordered per-context change feed carrying a list of events and
the version they bring the client to. That shape gets three things the pair
cannot. Coalescing becomes native, so the batching special-case that exists today
disappears rather than being reimplemented. A tool's final output and its
completion status arrive in one delivery, closing a race where a client renders a
finished tool with no output. And two clocks — an operation counter and a
delivery counter — collapse into one version.

Amy took it immediately: *"it's been creeping around my thoughts and is the right
move."*

One hazard came from the house's own doctrine rather than from any review. Two of
the thirteen events are musical: render cues and beat sync. Batching trades
latency for fewer messages, and the timebase doctrine forbids exactly that trade
for timing artifacts. They keep their own path, written into the specification as
its own rule rather than left to be remembered.

### What the day was actually about

Three bugs found that day shared a shape. A block's `excluded` flag and its
`created_at` were written by the server and never read by the client, harmless
only because the clients that would care still read a different path — and the
migration was about to move them onto the path where it stopped being harmless. A
shell command that printed more than eight kilobytes recorded a failure code on
its durable record while its status still read as done. And an ACP session
watching a spliced block appended a bogus suffix to stale text, rendering
characters no one wrote.

None of the three was visible as a failure. Each was a place where the system
said something confidently and wrongly, and stayed plausible while doing it. The
instrument's own stances are about being able to say who is in the room and what
happened; a projection that quietly disagrees with the kernel is that promise
failing quietly. The wire changes are the interesting engineering, but the reason
to make them is that the fewer things a client has to reconstruct, the fewer
places it can be confidently wrong.

### The kernel learns to say what a change was (August 15, evening)

Building the first slice of the feed was mostly unremarkable — the kernel now
decides append-or-replace where the mutation happens, and the snapshot query
reports the version it read at. Two things about it are worth keeping.

The first is a rule the specification had already written and the code had to
honor in an inconvenient place. Classification must not consult *who* made the
edit, only the coordinates: an insert at the end with nothing deleted is an
append, everything else is a replace. That is easy on the edit path, where the
length is already measured for a bounds check. It is not free on the streaming
path, where measuring the text before each token would restore an O(n²) that had
been removed a day earlier for exactly that reason. The append primitive is an
append by construction, so it asserts rather than measures — and a test appends
multibyte chunks and compares the published suffix against what the engine
actually stored, so the assertion is pinned to behavior instead of to a comment.

The second was found by a test that had no obvious relationship to the change.
While the old wire still carries raw operations, the kernel publishes both kinds
of event, and the old bridge simply does not send the new ones. Not sending them
was not enough. A batching test dropped from twelve batches to zero: the bridge
collapses a *run* of consecutive text operations for one block into one call, and
the new events, sitting between them in the queue, broke every run into
singletons. Worse and quieter, the bridge allocates a per-subscription sequence
number before it sends; allocating one for an event that never goes out punches a
hole in a lane whose whole contract is that a hole means the subscription died.
The fix was to drop them at ingress rather than at send time — an event nobody
sends still does damage while it waits in line.

A third thing happened alongside, prompted by Amy reading the test output rather
than the code: *"did I see tests accessing my ~ XDG path?"* She had. The shipped
default `mcp.toml` carried a server entry pointing at her own kaibo build, and
every kernel a test booted spawned it, whereupon kaibo opened her live state
database — the one a running kaibo was already using. An earlier fix had moved
external server startup off kernel construction onto the serving path for exactly
this reason, which had helped and had not been enough, because the tests that
boot a server take the serving path. The default now ships empty, with the real
entries kept in the file as commented reference, and the test asserting the
default configures *nothing* says why in its name. A shipped default is not
inert: it is a decision made on every machine that has not overridden it.

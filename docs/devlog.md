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

**`/v/blobs` — the CAS pool made reachable (July 2).** The clip design needed
"sync the CAS to the client," and track B went design → audible demo in one
arc: harden `kaijutsu-cas` first (atomic store, TOCTOU-free retrieve,
validating `ContentHash` deserialization — the client cache is multi-process
and a cache hit never re-hashes, so a torn object would be truth forever);
a read-only `CasFs` VFS backend at `/v/blobs` where immutability makes the
hard problems trivial; a client `BlobResolver` over its own SFTP connection
(SFTP futures are Send, the capnp world is !Send — they must not mix) that
re-hashes every fetch and hard-errors on mismatch; and the app sink consuming
CAS cues off a dedicated runtime. The review earned its keep: gemini caught
two real concurrency bugs (a transport-error handler that could wipe a fresh
connection, a single-flight lock leaked on cancellation) that both the author
and deepseek missed. Verified by ear: `kj cas put` → `kj play --cas <hash>` →
SFTP fetch → hash-verified XDG cache → speakers. One scar worth the telling:
kaish's overlay *reserves* `/v/blobs`, so `kaish ls` shows an empty shadow
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
("fantastic… I'm delighted"). The open design thread is ring *membership* —
explicit hot-row promotion verbs plus coarse (days) auto-decay, replacing
all-or-nothing idle bucketing.

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
directly; the generation counter (above) is its coherence primitive; `/v/blobs`
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
  in-process MIDI path, dead viz layout code — parity first, then delete
  whole, never strand a transitional path. The score being durable is what
  makes deleting renderers cheap.
- **Docs are living, not stratified.** Chronology belongs here and in git;
  design docs state the present. `docs/issues.md` deletes entries when they
  ship. And the acceptance test for music is the ear.

## Now

As of 2026-07-04: subprocess exec just shipped and the first audible `kj play`
after it is unverified; the time well carousel is in Amy's tuning hands; the
render seam is fully on the wire with clip/PCM prefetch proven; open work is in
`docs/issues.md` and the live handoff in `signoff.md` (ephemeral, repo root).

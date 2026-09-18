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
survive intact. Compressed 2026-09-08; the pre-melt text is one `git log -p`
away.

## Prologue — the first five months (January–May 2026)

Kaijutsu started 2026-01-15 as "what if my agent had a Bevy frontend and its
own shell." The first two days produced a UI shell, a Quake-style console, and
an SSH + Cap'n Proto connection layer; kaish was embedded by day three. The
ancestry is sshwarma — an SSH MUD that grew an equipment system for models and
nerdsniped its author into the context problem — and hootenanny, a retired pile
of music-model experiments. The README's developer note tells that part.

February consolidated the type system and taught contexts to survive a
restart. March made the block store correct (Lamport clocks, fork semantics,
order keys), unified two databases into one, and moved the app onto MSDF text
plus per-block Vello textures. April redesigned the tool system around the
MCP broker — everything routes through it, builtins included, as a virtual
in-process server — and landed CAS. May was the ABC crate's first deep spec
push, a Haiku-driven live-eval harness, and a kernel-wide timeout policy.

Two demolitions shaped the toolchain: the Rhai engine went once kaish could
carry scripting alone, and rig-core was dropped for hand-rolled providers,
because owning the wire layer is what later made cache breakpoints, CAS image
memoization and per-role routing tractable. kaish grew up rapidly inside
kaibo, which is in many ways the pragmatic take on what kaijutsu explores
maximally.

## The stance arrived mid-flight

The framing ideas weren't written first and implemented after; they
crystallized while building, mostly once the music work made "players" stop
being a metaphor.

**Instrument, not harness.** Kaijutsu is something you play — you, a model,
anyone with a connected app; many hands on one keyboard. The kernel is the
instrument's body: it supplies what a turn needs without playing the turn.
That reframe, and the composer→musician, explorer→toolie renames, live in
`docs/instrument-design.md`.

**Shared trust, crosstalk-as-feature** (settled late June). Should sibling
contexts be defended from each other? Won't-fix-by-design: every player is
inside the trust boundary, the kernel runs as one unix user, and the real
boundaries live outside it. Capabilities and loadouts are ergonomic nudges
for focus and mistake-prevention, never security; your neighbor's wrong note
is one you cover.

**Context vs conversation** is the load-bearing invariant underneath
everything: the context is the durable, multi-writer side; the conversation is
the append-only live session hydrated from it at boundary events. `stage
exclude` and `block edit` land at the next hydrate — remediate a poisoned
conversation by excluding in context, then forking.

The August 18 tool-pairing failure exposed two contributing factors: a
repair pass indexed a filtered sequence using positions from its input,
and the provider boundary trusted the result. Fixing the index stopped the
observed cascade, but logging an invariant violation still allowed a bad
request to leave. The September hydration work closes each call batch in
one repair pass and validates pairing before every provider dispatch,
including live-loop appends. Invalid history now stops locally with an
error and a terminal turn event. Late results retain the existing
interruption policy; preventing their interleaving in the durable context
is a separate writer-side design (`docs/conversation-session.md`).

**No first-class "agent."** An actor is always a principal; agent-ness
emerges from fork and drift, not from a noun in the schema. Later, a
principal with a sheet became a **character** (September).

## Config: the kernel owned it, then gave it back (June → August 29)

A silent-fallback bug in rc loading turned into the biggest structural
decision of June: rather than patch the dual-ownership cluster (stale-bytes
reads, append file-wipes, mtime no-ops, stale seeds), we deleted the class.
The kernel became the sole owner of `/etc/rc` and `/etc/config`, seeded once
from embedded defaults, nothing on disk to `vim`. It bought the corruption
class's absence at a real cost: nothing for git or an editor to see, and a
growing list of surfaces that had to special-case config paths.

That ownership did not survive the summer. **Permission to get simpler**
(Amy, August 15): *"if the agent can see the files and edit them, that's
fine, we don't need to complicate it just because it's config."* rc melted
back onto disk first (`docs/rc-on-disk.md`), then all four roots followed
under one mount registry — `/config/rc`, `/config/kernel`, `/config/client`,
`/config/midi`, each an ordinary host directory through `LocalBackend`
(`docs/config-namespace.md`). The June bug does not reopen: git is a choice
about a directory, never a mechanism the kernel runs, so there is still one
place the content lives. The case for dropping the `rc-write` guard was
first argued as "a gate that gates nothing," which was false; the right
argument was that rc had stopped being a special category.

The same weeks put teeth in the fail-loud posture. The kernel's `edit` tool
had fed byte offsets into a character-indexed engine — a silent splice on
any multibyte file, reported as success — and came back with byte→char
conversion, post-write verification that crashes over corruption, and
hashline addressing. The external MCP shell hang was executor starvation on
the client's single-threaded LocalSet, made permanent by a server reap on the
first stall; a 300 s command dropped to 285 ms. `FileAttr.generation` split
the coherence stamp from display mtime. And June 24's cache/cost session
added the lens that still guides prompt plumbing: the prompt cache is a
prefix match, so *where* a byte lands matters more than whether.

## The music stack — from one loop to a band on the wire (June 13 → July 3)

The longest arc, and the one that forced most of the system's ideas to get
real. Canonical designs: `docs/chameleon.md`, `docs/tracks.md`,
`docs/midi.md`, `docs/pcm.md`, `docs/hyoushigi.md`.

**The chameleon loop (June 13).** Models playing to a beat, a player's turn
text *being* the score. The hard-won constraint: players must be tool-free —
a small local model handed the full palette stalls the turn. Players are rc
programs; a musician is a context attached to a beat track.

**Tracks: the score outlives the players (June 28–30).** Three stages moved
the music substrate off contexts onto a durable per-track model: the clock,
then the score itself — a real, app-viewable per-track score context, minted
the lost+found way, reusing the whole per-context block machinery — then a
generalized clock with mutable tempo. The landed-code review caught three
places that had assumed tempo constant for all time, one a silent-fallback
restart data loss. `context_type` decomposed into rc along the way: musician-
ness became "your create rc arms you," and the rotate page-turn became a
five-line rc script.

**First sound (June 30).** A Haiku musician's line came out of a synth: ABC
turn → track → materialize → ALSA → TiMidity → speakers. The unit tests had
been green for weeks; the acceptance test was audible. Then a local
gemma4-e4b bass took the chair, dialed in by making the prompt small-model-
foolproof and having the tick rc precompute bar targets in kaish.

**Docs learned to stay present-tense (July 1).** Three intense weeks had
left the music docs teaching superseded mechanisms as current; "living" had
come to mean stratified. The fix was not banners but moving chronology here
and to git, and letting each doc state the present.

**Render convergence: bytes never ride the track (July 1–2).** Two
decisions, named in `docs/midi.md`: we take real time seriously by refusing
to chase it — micro-batch, promise only what we can hit, a speculation lead
of seconds, hard-realtime only at the node that owns the gear — and MIDI and
samples converge on one mime-keyed wire cue, `RenderCue { mime, payload:
Inline | Cas, lead }`. A placed sample is a clip cell: CAS ref plus
placement, bytes prefetched out of band. Once the app proved parity by ear,
the in-process render path was demolished whole (~1000 lines). The kernel
links no audio FFI; a headless kernel makes no sound, but the score is
preserved and replayable. The metronome built to settle a reviewer split
went from 50 ms inter-click stddev to 0.7 ms once integrator wind-up was
replaced by feedforward tempo with bounded phase correction.

**Clips and `/v/cas` (July 1–2).** A seven-industry survey
(`docs/cue-prior-art.md`) found every cue system re-inventing the same six
field clusters, half already on hyoushigi's `Cell`, so Cell does not expand;
a clip is a versioned JSON payload. Making the CAS pool reachable went
design → audible demo in one arc: harden `kaijutsu-cas` (a torn object in a
multi-process cache would be truth forever), a read-only `CasFs` at
`/v/cas`, a client resolver over its own SFTP connection that re-hashes every
fetch. Gemini caught two real concurrency bugs both the author and deepseek
missed.

**Demo #1 post-mortem (July 3)** burned a director's turns on stale docs,
then found kaish could not run `aconnect` at all. The deeper fix was
subprocess exec behind an `exec` loadout authority, deny-by-default;
coder/mcp/director carry it, musician/toolie never.

## ABC grows up (May, then June 30)

A kaibo three-model audit with the verbatim ABC v2.1 spec in context paid
off twice: fourteen real bugs fell TDD-first, and one confident wrong finding
was rejected because the code's accident was already spec-correct. A
robustness net (parse→midi→abc→parse never panics; NoteOns and NoteOffs
balance) immediately caught a real `L:1/0`. The engraver carried exact copies
of two MIDI bugs, fixed at the root by one shared `Key::signature()` so they
cannot drift again.

## The app — text, wells, and carousels (June–August)

**The vi editor (June 23)** is a kernel-owned session — pure modalkit behind
kernel `EditorSessions`, the Bevy app one renderer among many drivers — and
the feared render-path collision evaporated by decision: the app renders from
a kernel-served editor-state channel and never joins the editor's context
into its document cache. `docs/vi.md`.

**The time well** evolved more visibly than anything else: constellation,
spiral, tilted vortex with an accretion throat, then rings. Idle-age bands
lasted two days. **Placement you can't control isn't an instrument** (Amy,
July 5): two hand-curated rings sandwiching two automatic ones, ten seats
each, digits addressing the focused ring; promote by keystroke or by
visiting; demote steps one ring out and archives off the end; promoting an
archived context resurrects it, because the archive is memory to drift back
from, not trash. The HUD's four edge panels melted into the instrument a week
later (selection drapes the bowl wall, the reading card carries specs and
ancestry) and `hud.rs` died whole. On August 1 the rings lay parallel to the
floor and the arithmetic indicted the design: two lower rings rendered in the
room's basement, and asked what they were *for*, the honest answer was the
same thing. Two rings and a floor: ACTIVE, RECENT, and an accretion disc on
the room floor that is the event horizon. The reusable lesson: a geometry
change is a proof obligation against the design it renders.

**Conversation view hardening (July 3)** found error blocks stuck at the
bottom were Bevy child-ordering choreography, and "text loads with holes" was
a full MSDF atlas respawning generation tasks every frame — infinite CPU
churn wearing a missing-glyph costume. The atlas grows to a cap now, and
kanji-heavy conversations keep their glyphs.

## Wires and surfaces (June–July)

The RPC transport moved off a positional three-channel scheme onto one
channel requesting the `kaijutsu-rpc` subsystem by name, a flag-day cutover
with no shim. The SFTP adapter serves `kernel.vfs()` directly with the
generation counter as its coherence primitive. Subprocess exec (above) locked
a direction with Amy that inverts the mount posture: an opaque host, PATH-dir
bin mounts curated per context_type.

## How we work — the ritual and its lessons

The practices that survived contact, recorded because they are the real
product:

- **The house review ritual:** two models outside our family read the
  *whole files*, no diff, so they evaluate holistically. Cross-model
  divergence is the point; each has caught real bugs the other missed. When
  two competent readers model the topology differently, that is the signal to
  go look, and you diagnose from the code, not a reviewer's summary. Reviewer
  claims about engine scheduling get verified against the local Bevy checkout
  before any code moves.
- **Two voices at design time.** Big cuts get stress-tested by independent
  models before code; the findings fold into the tracker, not a rewrite.
- **TDD, red-first, and crash over corruption.** The recurring bug class is
  the silent fallback; the recurring fix is fail-loud verification plus a
  test that fails against the old code. Falsification is the lead's job: a
  lane that writes a test against the code it is building will honestly
  report green; only a targeted fault shows whether the test guards anything.
- **Demolition as practice.** Rhai, rig-core, the config flush backend, the
  in-process MIDI path, the KV store (its one caller moved first, then 1,600
  lines deleted whole), the CRDT replica in the MCP, the text engine itself
  — parity first, then delete whole, never strand a transitional path.
- **Docs are living, not stratified.** Chronology belongs here and in git;
  design docs state the present; `docs/issues.md` deletes entries when they
  ship. Shared docs get edited, never re-emitted from model memory.
- **Run the exact thing against the real system.** Twice in one afternoon a
  thing that appeared dead was merely never asked a question it could answer.
  Verify against the binary you think you shipped. `git log -S` a comment
  before building on it.
- **Fan-out and merge.** Sonnet lanes in disjoint file territories, no git
  from lanes, the lead commits path-scoped and re-runs every red-test
  mutation. It held on a single shared file with explicit region ownership.

## The instrument gets kinder to its players (July 4)

A player's-eye sweep of the kj surface, picked by one question: what does a
model hit mid-turn that a human wouldn't tolerate? Five parallel worktree
lanes; the first real test of fan-out-and-merge. `kj fork` worked from kaish
again once the kaish→kj bridge learned `Value::Json`. Contexts learned what
day it is through datetime rc seeds, and the load-bearing choice was the
block kind: a Notification hydrates as an appended message, while a System
block would invalidate the cached prefix daily. Config stopped lying about
unknown provider types. `kj block cat --latest <mime>` answers "give me this
turn's artifact" in one call. The unknown-command 300 s hang closed as a
proof that the dispatch fall-through is bounded at every await. Two kaish
bumps were zero-source because we ride the embedder API through low-level
primitives; the second fixed the `/v/cas` shadow where kaish's overlay had
reserved the whole `/v` tree.

## The kernel gets an interior (July 7–10)

The time well had proven kernel state could be a *place*; the scenes charter
(`docs/scenes/`) asked what building the rest would mean. Two days of design
(28 mockups culled to one canonical image per surface), then three days from
spec to a finished station.

Navigation grew one level up without a new grammar: Up/Down between detail
levels, Left/Right within one, Esc always walks up, and the well's mouth
exits through a double-tap speedbump. The camera taught the room's first hard
lesson: in a radial room every pose is a claim about what stands between you
and the center, so a focused station is approached from its own side, looking
outward. The patch bay went from black blob to instrument on a round table.
A two-hour hunt for a "missing" traffic pulse ended with staged shader probes
proving every layer correct — the 0.42 s default was faster than screenshot
sampling. Distinguish "the mechanism is broken" from "my observation can't
see it" before touching the mechanism.

Amy settled one scene graph, not separate ones, and the lifecycle bill came
due immediately (a context switch mid-dive leaked the room). Furnishing day
taught that inhabitable is mostly camera height and that you cannot light a
1% albedo — the material's diffuse response, not the lamp, is the knob. Then
walls became screens: eight content panels in an octagon, and "we could
almost drop the dive if the walls were 16:9" turned out to be structural.
With fullscreen as a camera pose plus a zoom field inside one Room state,
`Screen::PatchBay` dissolved and took the dive-exit lifecycle machinery with
it, including the leak fix built that morning. Deleting a state to delete a
bug class is the day's best trade.

## The app learns to mean its colors (July 12)

The terrace glyphs shipped ornate and Amy named the real problem: "muted like
the rest of the octagon… more synthwave than anything." The mutedness was
structural. Hues were governed, brightness was thirty scattered constants,
and the tonemapper had never been chosen. One kernel-owned `theme.toml` now
carries both lanes — sRGB UI and linear-HDR scene with a brightness ladder
and a hot-applying post chain — a `ScenePalette` resource absorbed every
scene constant, and a live A/B over BRP picked ACES; the muted look was
literally TonyMcMapface. `docs/color.md` is the contract. The mirror test
between compiled and file defaults caught a palette 13× off on one channel
family on its first run.

## The index learns to keep itself honest (July 12)

Three quiet debts in the semantic index went in one afternoon: HNSW cannot
delete, so eviction left dead vectors forever; nothing noticed a model swap;
synthesized gists evaporated at restart. The design that made rebuild
tractable is that **slots are never renumbered**, and a red test proved they
must never be *reused* either. First boot on the live kernel reclaimed eight
dead slots the real index had been carrying. Sonnet lanes wrote the code;
the lead's review, the outside models and the running kernel each found bugs
the other two missed. That triangle is the lesson.

The September synthesis pass exposed a second distinction: the search
projection and the synthesis input are different documents for hashing
purposes. A prefix hash cannot validate a gist drawn from the whole context.
Amy's direction to use lfm2d moved embedding inference out of the kernel,
and made purpose, normalization, and checkpoint identity explicit parts of
the interface. Caching unchanged work and bounding service calls came first;
changed contexts still need per-block reuse before automatic synthesis can
return. Keeping that switch off made it possible to replace the inference
mechanism without repeating the September startup load spike.

## The filesystem becomes a world, then ambient (July 12–13)

The fsn landscape went from vocabulary to a rendering world in one evening of
three lanes: relaxed-Voronoi layout in `kaijutsu-viz`, a `Vfs.snapshot` RPC
with generation stamps, and a dive-through scene. The live pass taught that
the unit trees were too polite: the real host tree killed the walker three
ways in an hour (a root-only directory, `/v` existing only in the mount
table, `/proc` PIDs vanishing between readdir and getattr), and each fix was
a design decision — denial renders as a seam, the mount table answers for its
own namespace, churn under the walk is operational.

Then Amy reframed it: the fsn world is **not a file browser**. Agents work
at the file level and the shell covers the rest; the filesystem is a free
source of ambient data that looks good in 3D. That sentence deprioritized
bloom, dive-to-vi and search, and promoted heat from the kernel's own hands
(the MountTable chokepoint already sees every mediated op), recency from the
wire, and the vessel inhabiting the world through two wall panels rendered by
an off-screen camera. `docs/scenes/vfs.md` is canonical.

## The filesystem joins the band — /r client shares (July)

Reverse the SFTP we already have, so a client can share `~/src` into the
kernel the way `code .` shares a directory: the client opens a channel and
speaks the *server* role, and the share describes itself with an in-band
`index` manifest. Gemini's pre-build review reshaped slice 0: `VfsOps::read`
is stateless and SFTP is stateful, so a naive pump pays OPEN/READ/CLOSE per
chunk; `open_read_stream` became the first thing built. A post-build
deepseek pass found six bugs the tests missed, the worst a `readlink` stub
that lied. `docs/slash-r.md`.

## The beat learns to carry its own clock (July 15)

The TRACKER station replaced a promissory nameplate with the instrument —
a pattern grid where each column scrolls at its own tempo past one fixed
playhead, because tracks are independent clock domains — and the first jam
on it found the clock bug. Amy heard the metronome "bumping a few times, not
evenly spaced." Beat references were stalling behind a musician's streamed
output on the single callback stream, arriving in bursts, and the receivers
folded every buffered reference against one frame-now. The burst behavior
had been encoded in a unit test as correct.

The fix was symmetry with the clock-in path: every timing artifact carries
its emission wallclock, sinks back-date, stale ones are demoted on a ladder,
and a metronome never stacks clicks — missed beats are missed. Then the
kernel grid went scheduled-periodic (re-arm on the deadline, not the wakeup)
and the phasor earned a deadband inside which it simply *is* the local
clock. From zero-millisecond click blobs and six-second holes in the morning
to four hundred click-to-bass pairs holding +0.2 ms with zero slope by late
afternoon. `docs/midi.md` "The one timebase" is that day written as
doctrine. The jam also showed that a track played for hours drowns every
musician who sits down at it, which filed the windowed band view.

## The stolen bridge (August 2–3)

An intermittent five-second tax on the MCP shell turned out to be one line
reading another: the server dedupes block subscriptions by (principal,
instance), and `kaijutsu-mcp` passed a literal `"mcp-server"` as its
instance, so every concurrent process for one principal was the same client
and whoever subscribed last silently evicted the rest. The app had the same
bug spelled `"bevy-client"`. Both now use per-process instances, and the
registry warns when a different connection displaces a live bridge. The same
day `kernel.db` got its first backup story (`kj db backup`, `VACUUM INTO`
against a live writer) and restore stayed a documented procedure, because a
live file swap under a kernel full of in-memory state is a lie waiting to be
discovered.

## Contexts join a band — SQL-native model config and casts (August 3)

Reading kaibo's cast concept against our `llm/` layer made the gaps obvious,
and Amy pushed past "adopt kaibo's TOML": *"I'd like to see the cast data
modeled in SQL directly and that's the source."* `models.toml` was demolished
for normalized tables edited through `kj backend|cast|alias`, registry rebuilt
live on every write. Presets survived on purpose once the code showed they
are patch recall over verb args: **cast = who plays, preset = the patch.**
Roles are context_types. Old aliases didn't make the trip — "they were
guesses" — so every row above the floor is something someone chose.

## Errors that were only strings (August 3–4)

Amy's rule arrived as a one-liner — *DB errors are P1, we fix them now* — and
the kernel spent two days proving why. A warn scrolling past every restart
since mid-July was two definitions of "already knows this context" a few
lines apart. `create_document` decided whether a failed insert was benign by
asking `e.to_string()` for "UNIQUE constraint", and two failures wore that
disguise: a benign PK conflict and a different document claiming a taken
path. The fix refuses to read messages at all: on a violation the DB layer
reads itself back and returns a typed answer, so classification depends on
the database's state rather than its prose.

The drift router's dead letters were being written by a caller that logged
and carried on; three lines above sat the invariant being broken (a
registered handle implies a row). Then the live run found the joke: restart,
orphan a drift, flush — *nothing to flush*, because the early return for an
empty caller queue sat above the global dead-letter half. The existing test
had met this and accommodated it in a comment that reads like a confession.
All three fixes were unit-green before that flush ran.

## Tasks join the block model — the household-agent arc (August 4)

A gap analysis against hermes-agent and QwenPaw found one gap kj had to fix:
no task state. Both harnesses write JSON to disk; a task that is a block gets
the block log for free. `task_status` reused the per-field LWW register
`content_type` had already established, and got its own enum because `Error`
means the tool crashed and a cancelled task is a choice. Mid-conversation
edits are cache-safe for the same reason Notification blocks are: the mailbox
translates a block id once. `builtin.tasks` sits beside `block.rs` so a
household agent can be grantable "groom tasks" without block surgery. Codex
later joined through two deliberately separate channels — its MCP subprocess
for identity, command hooks for lifecycle — because MCP identifies the player
but cannot observe prompts or session boundaries.

## The day toad played kaijutsu (August 5)

*"Using toad with kaibo was a delight."* Four lanes before lunch; the
headline was `kaijutsu-acp`, a thin ACP bridge in kaijutsu-mcp's image, whose
session picker served the app's ring-0 rank with zero schema changes. Then
Amy bounced the kernel and the first flight died on its first prompt: the
task merge had added an at-rest CBOR field without a serde default, and the
breakage sat latent until the bounce — every pre-task document undecodable,
rc unreadable, no create-time bindings, deny-by-default locking the
operator's own kj. A missing attribute became a total lockout through four
links of chain. New at-rest fields decode old bytes or they don't merge.

The afternoon was the best shakedown the project has had. Four incidents
traced to one defect — the FlowBus was a shared broadcast ring, so one slow
subscriber's overflow evicted events for everyone — and Amy set the doctrine
in a sentence: *"no lossy solutions. I'd rather be disconnected."*
Per-subscription bounded queues, lossless-or-terminated for ordered topics;
timing topics keep their own law and drop oldest. Sequence numbers mean a gap
without a termination is by definition a kernel bug. Per-lane guarantees say
nothing about ordering across lanes, and the completion event beat the final
text to the bridge until the bridge learned to settle before answering. And
the crosstalk stance stopped being theory: the model toured the repo in
Amy's toad session while the lead watched the same blocks from kj and Amy
from the app — three players reading one score.

## The instrument changes its strings (August 12)

The Bevy 0.19 plan was written by reading all 104 migration guides against
the app rather than from memory, because training data predates the rename
that matters. Seven of 104 touched us. What was load-bearing was a dependency
whose version number lied: `bevy_brp_extras = "0.19"` requires Bevy 0.18.1.
**Read a dependency's manifest, never its version number**, and prefer the
cargo registry cache as truth over a checkout that can be five months stale.
An unused `avian2d` was hard-pinning the old engine; dead dependencies vote
on your version constraints. Amy ruled rodio migrates first, on its own, so
the one-copy invariant stays true throughout. A workspace test that failed
after the bump in a crate with no Bevy got checked at the pre-bump commit in
a worktree, where it failed identically, and went into the backlog with proof.

## The denial that pointed at a locked door (August 12)

A failed `create` rc lifecycle left a context holding nothing, and the backlog
called that a lockout. It isn't: switch and every read verb are ungated by
design, so diagnosis works and only action is blocked. Amy's question — when
would `create` actually need to abort? — answered almost never, and aborting
destroys the Error blocks that explain why. Reading the gate found the sharper
defect: every refusal advised `kj binding allow`, which the caller reading the
message could not run, and a KernelDb read failure was collapsing into
"denied." The gate now keeps three outcomes three, the unbound case names
exits that exist, and `kj context rebind` re-runs `create` ungated on the
same argument that leaves `create` ungated: the loadout comes from rc, not
the caller. Gating the repair on a capability the broken context cannot hold
would have rebuilt the lockout as a feature.

## The mirror that stopped being a mirror (August 13)

`kaijutsu-mcp` was a CRDT replica with a sole-writer task, resync coalescing
and an event bridge. By the end of the day it held none of that. Amy's
framing made the shape right: a tool call takes a quick lock at startup, then
runs independently — so the atomic part is only the reservation, and a
ToolCall at `Running` is a legitimate pending state. **Reserve, then flow.**
`authorBlock` and `completeBlock` replaced the raw-op path. Then the layers
came off in order, each smaller because the previous had removed its reason,
and the last inversion was the key: **polling became the guarantee and events
became a hint.** A dead feed is no longer a condition to detect; it just
means nothing arrives early. 624 lines went. The hook boundary consolidated
the same way later — `kaijutsu-mcp hook claude|codex` absorbed both native
protocols and two Bash-plus-jq wrappers disappeared.

What the day was really about was tests that cannot fail. Five defects passed
careful reading and died on execution: a `tokio::time::timeout` that drops
its future stranded a ToolCall at `Running` in a cancellation window the old
single-lock design could not have; a wire field the server ignored,
reproducing one ordinal later the exact defect that justified the verb; a
"survives a dead feed" test running `echo`, which finishes before the first
poll. An untested mechanism is a claim, however carefully its prose is
worded.

## The instrument could not say who was in the room (August 15)

`spawn_periodic_refresh` was correct, tested, and called from nowhere. Proof
it ticks came from the live kernel: samples of `/run/roster/index` landed on
the loop's own grid, ten thousand milliseconds apart.

Then a `cat` of that index exited 3 and the thread ran all day. kaish caps
captured output by replacing it with a preview and remapping the exit code,
which is right, but its audience was wrong: it also reaches `$?`, so inside
an rc or hook body a command that merely printed a lot reads as failed, and
the gate's classifier would have escalated on a *good* long answer. The fix
asked who consumes the output: model-facing shells keep the cap; rc, hooks
and the editor's `:r !cmd` get a runaway backstop. The test pins kaish's
current wrong behavior so the workaround can go when upstream fixes it.

The part worth keeping is what the machine could not do. Clearing 194 stale
contexts needed a safety filter, and roster liveness is not it: `recent`
means "appended a block in fifteen minutes," not "someone is attached." It
reported four live contexts while a Codex lane sat mid-review, connected and
thinking. Amy held the fact — *"there should be moltar app, this claude code,
maybe subagents, and a codex session"* — and the instrument had no way to
represent it. Use last-activity age, and treat "attached" as a question the
roster cannot yet answer.

Replacing ROOT made the same point structurally: special by convention,
ordinary to every mechanism. Amy settled the graph — *"I had thought to make
it a dag but the data is naturally a forest and drifts create cycles if you
count them"* — and checking that invariant found `kj context move` orphaning
a context it refused to move. Anchors exist for fork cost: forks copy history,
so every block in a root is paid for by every descendant forever. Three weeks
later the archive cascade itself went: archive is a fact about one context,
and a lineage is worth more intact than tidy.

## The melt begins, and finds two armed fields (August 15)

The CRDT position paper had ruled: one authoritative sequencer, rich RPC
authoring, projected event streams, no client dependency on the text engine.
The migration began, and went one step past the paper — replacing CRDT-shaped
storage wherever semantic operations suffice. Amy declined to gate the git
work on principal provenance: the gap is a pattern across seams, not a config
bug, and fixing it under whichever lane stands there fixes it in one place.

The MCP shell-completion path had a Phase 2 that decoded the whole oplog to
re-read a block, justified by a careful comment about three reorderable
topics. The argument was sound and about a mirror the August 13 demolition
had already replaced; the variable was still named `local`. A stale comment
is a false premise parked where the next reader will pick it up. Two more
were corrected the same day, one citing three schema ordinals that were all
wrong.

A throwaway question — does a projected block query match a decoded sync
payload? — found two fields the server wrote and no client read, `excluded`
and `created_at`. Harmless only because the clients that care still read the
other path, which the migration was about to move them off. A wire field no
client reads is not dormant; it is armed, and the migration pulls the pin.
The zero case for `created_at` is the house style in miniature: propagate a
1970 faithfully rather than substitute "now." And a "flaky" kernel test was
reproducing a real bug: `mount` printed past the output cap, and the durable
record of a capped command carried a failure code under a healthy status.

## The day the wire stopped being a storage engine (August 15–16)

Everything until then assumed the wire was near-frozen. Amy lifted it in a
sentence: every client is in-repo and rebuilt together, so flag-day changes
are fine where they reduce debt. Freezing collapsed into deleting: `pushOps`
and `pushInputOps` went, a thousand lines, and with them `merge_ops`' only
caller. Concurrent merge into kernel documents became **impossible**, and the
instrument built that morning to measure whether it ever happened was deleted
hours later, because a structural impossibility beats a metric reading zero
forever.

The replacement design was right in shape and wrong in placement. A DeepSeek
review confirmed append-or-replace events and refuted the reasoning (content
comparison, never a list of tool names). A Gemini review, asked to counter our
anchoring, found that classification had been specified at the wire against
per-subscription state, where the bridge would have to link the library being
removed; it belongs inside the mutation lock where both texts are in hand.
The same review found gap recovery unimplementable because the snapshot query
returned no version, and argued for one ordered per-context change feed
carrying events and the version they bring the client to. Amy: *"it's been
creeping around my thoughts and is the right move."* Timing artifacts keep
their own path; the timebase doctrine forbids the batching trade for them.
`docs/change-feed.md`.

Building it taught two things. The snapshot query hides that `BlockSnapshot`
has no ordering key, which is exactly what ACP had been using the CRDT for,
so the insert event carries a position. And a client cannot edit a block's
text at all — `pushOps` was the only path — which "all authoring goes through
rich RPC" had been written as though the verbs existed. The client-side
follower is written once and refuses rather than guesses; its own tests
caught a stale-index bug in the move path. All three clients moved the same
evening and each deleted a staleness apparatus that had been load-bearing for
a replica that could quietly diverge. The one bug needed no concurrency: the
version rode a *delivery* while the rule reasoned per *event*, and a delivery
is a batch. The last holdout on the old surface was the time well's activity
glow, which counted events and never looked inside them; Amy disabled it
rather than migrate it, and four thousand lines went.

The renumber exposed an assumption the least visible way available: the
client crate's build script had never declared the schema as a dependency,
and every previous change had been additive enough to hide it. Amy reading
the test output rather than the code found the shipped default `mcp.toml`
pointing at her own kaibo, so every test kernel opened her live state
database. A shipped default is a decision made on every machine that has not
overridden it.

Then the text engine left. A read-only copy of the 861 MB production database
showed diamond-types-extended's snapshot encoding costing about four times
the text it stored, flat across version counts — a structural cost, not
debt. A forced compaction pass materialized every document's text first,
because any document edited since its last compaction held newer text only
in the oplog. Block content became a plain `String`: streaming is all append
and `push_str` is amortized O(1). The editor and file surfaces keep ropey,
where splice-heavy text earns it; a draft stays a `String` with a revision
counter. Three surfaces, three representations, each chosen against what it
does to its own text. `docs/crdt-position-2026-08.md`.

## Two doorbells, one ledger (August 18)

The approval ledger had carried a second, quieter system beside it for
months: `kj cc send` and `shell_write` went through `run_gate` with durable
rows and rules; a hook's `Ask` went to a `Uuid::new_v4`, a thirty-second
budget and a blocking round trip with no record. Three pieces of evidence sat
in the ledger's own source — `Origin::Hook` never constructed, `hook_id`
never populated, `NewOption` mapping one-to-one onto ACP's permission
options — so the ledger had been built to absorb the hook path and never
told. Amy retired the doorbell.

The instructive half was what fell out. `GateOutcome` could say `allowed:
false` and nothing more, so `run_gate` had been reporting its own faults as
denials, and a ruling from the day before required a model to distinguish a
refusal from an absent control. Both became one `Option<AskRef>`: a row
exists with a status, or there is no row. Then a gated call held eighty-one
seconds and returned after a human answered from another surface, the loop
working end to end for the first time — and the same day, driving kaijutsu
from an editor, no approval ever reached it because the editor launched a
binary compiled two minutes before the wire it spoke was retired. The answer
was a handshake that refuses a mismatched peer and names which side is stale.
Four primitives that day had been built, tested and never called; nothing in
a green suite says *this code has no caller*.

## The conversation stops being a widget tree (August 16–18)

No gain constant could make a wheel detent cheap while its cost scaled with
block size: the conversation was a Bevy flex column of per-block textures,
taffy in the scroll path and a silent clamp truncating blocks over ~273
lines. Amy's idea was to cache blocks as textures; the survey moved the cache
one level up, because the expensive step was shaping, not pixels. Blocks
became cached shaped glyph runs, assembled into instanced buffers over a
±1-screen window and drawn with scroll as a 64-byte uniform. Five slices in
one sitting, each review-gated and live-verified over BRP, and slice 5
deleted the legacy path the same day — Amy: "no reason to keep legacy in
this project." `docs/conversation-surface.md`.

## The file cache learns what vim already knew (August 18–20)

A cold miss served a kernel document written in June and wrote it back over
a file that had moved on: the kernel was the one player that could revert a
human's work and call it a save. `docs/file-buffers.md`'s thesis is that disk
is the source of truth and vim solved the rest decades ago: a buffer is a
view, an unsaved buffer is a swap, a swap that survives a crash is announced.
A dirty buffer leaves a durable row whose presence *is* the flag; `:w`
refuses when the disk generation moved unless the player types `!`. `Kernel`
came to own its block store and file cache by construction, deleting a
`OnceLock` pair that had let two caches exist over the same documents. The
test database stopped being `:memory:` across 228 call sites, because the
in-memory path was a second code path production could never reach. The
20th's audit found an evicted entry had made `:w` a silent no-op, and every
new test was broken once, on purpose, by the lead before it was kept.

## The scripts that shipped and never arrived (August 20)

A guard against `sh -c` and a risk scorer were written, tested, reviewed and
deployed, and neither had ever run. The kernel seeds rc only when the tree is
entirely empty — deliberately, so a deleted script stays deleted — and the
other half of that rule is that a script *added* to the embedded set after
first seeding never lands, and no surface said so. The fix was not to seed
harder but to make the discrepancy visible: a fourth status, "a seed exists
here and nothing is installed," which Amy chose over a repair verb because a
status announces itself to anyone who looks. It proved itself within the
hour. The scorer had armed itself only in one seat and fallen silent
everywhere else; unarmed and broken looked identical, which is the actual
defect. A system that cannot report an absence will keep the absence.

## The turn that said it had finished (August 22)

The coder became something you could hand a job to: `kj fork`, `kj drive`,
`kj wait`. The first thing it produced was a bug reading had never found:
`kj wait` reported `completed` mid-flight, because between a tool result
reaching `Done` and the next model block there is a provider round trip
during which nothing anywhere is `Running`. An instantaneous read of a
multi-writer log cannot tell "finished" from "between two blocks"; the
information is not in the log. Amy's ruling was to stop inferring: the
kernel owns a registry of turns in flight, set where a turn is committed and
cleared at every one of the stream processor's exits.

The day's rule, sharpened with kaish's lead who found the same shape twice in
their own tree: the dangerous comment is the one that is **the reason
something else is switched off** — an append path excluded from a gate
because "append never destroys content." A test asserts behavior at one
point; a load-bearing comment asserts reachability, and nothing checks
those. Green tests are the condition under which they rot unnoticed. The day
closed with a delegated coder concluding git was not installed when it was
on a shell that refuses external execution, and the shell said both with one
sentence; each project now names the condition it owns.

Then the gate went next. A gated call had been holding an RPC open while a
human decided, and the ask was asked to last an hour, an errand, a night. It
cannot: the client's deadline is a compile-time constant in a process that
cannot read kernel config. Amy: *"perhaps we should consider a state machine
and not actually having anything block on the wire."* The kernel announces,
the client may block locally, and when the answer lands the kernel performs
the action itself. The next day she killed restart survival too — *"the
kernel is really reliable and the only reason it restarts a lot right now is
because we're actively advancing it"* — because the one outcome that design
could produce, an approved destructive action running twice, was reachable
only on that path. `docs/gate-resume.md`.

## The seat you ssh into (August 30 → September 8)

Where should a terminal client live? The client crate has no Bevy in it, the
ACP bridge proves it renders anywhere, and of the app's ninety thousand lines
the part a terminal could reuse fits in three thousand. So the tui is a
standalone binary in the ACP bridge's shape with ratatui as the edge; a
kernel-served tui was feasible and declined because a client inside the
kernel process reaches around the wire rules the first time they are
inconvenient. Amy: inline viewport, not fullscreen; one `bindings.toml` for
both clients keyed by vim notation. The ssh-shell design retired into it.

The first cut shipped September 2 from five lanes in one day, live against
zorak: transcript printed once into the terminal's own scrollback, a
static-height band pinned at the bottom, vi compose over the kernel draft,
the Ctrl+Z shell with real suspend, the picker, the ask card, editor and
diff on alternate screens (`docs/tui.md`). Amy played it that evening and
her notes drove the next week: the thinking pane, the in-flight strip as one
fixed row, the draft that grows the band, an ask answered from another seat
the client holds. On September 8 a switch marker and a per-context copy mode
were tried and shelved — "marker and copy mode aren't gonna work" — and the
buffer question is open: a single-context app that composes with tmux, or
the tui *is* the mux on the alternate screen "all modern like in rust." A
session of its own.

## The answer that travelled as an error (September 1–2)

Probing a gated surface minted five asks from one probe, because its refusal
arrived as a transport error, and a transport error means *"I could not tell
you what happened,"* so the caller retried with different text and each
retry minted a row. A verdict is a result: the machinery worked and reached
an answer, even no, and the caller must not retry. A fault is an error. The
kernel knew the difference; the return path had nowhere to put it, because
every `capnp::ErrorKind` describes a fault, and a careful comment above the
collapse said exactly what was being lost.

An inventory of 152 RPC methods found one in eight with any second channel;
throwing was the default and the gate was merely where it hurt most. Amy's
ruling: **no general verdict facility**; the unit of work is the family. The
VFS family shrank from seventeen wire methods to four with callers (SFTP had
superseded the rest months earlier and nobody said so in the schema), and its
errno crosses the wire as POSIX semantics under a stable encoding because
`ENOTEMPTY` differs between Linux and macOS. The gate family got a `Refusal`
union. Then the fix that stopped at the wire bought nothing: the client's own
actor flattened the typed error back into a string one hop later, and the
receipt was in the tree, a roster function lowercasing kernel prose to search
for "not found." **Every boundary that stringifies is a place the type
dies**, and counting the methods in a family is not counting the work.

The state-machine ruling had said that when an answer lands the kernel
performs the action; nothing had. Amy ruled it the rest of the way: **approval
triggers execution.** `kj ledger allow <id>` runs it and fills the command
and output blocks already sitting `Waiting`. The driver reserves the rc
thread stack after the first heavy approval aborted the live kernel
mid-rotation of ROOT. The digest match was found to be the authorization key
for the retry path, not duplication, and stayed. For a free `${VAR}` Amy
asked *"how hard would it be to snapshot kaish state along with the
request?"* — easy, since both gated paths run on a single-use shell seeded
from durable state — so the names are captured with their values, restored
before the source runs, and shown to the human and the classifier alike. The
executor claims the redemption row first because the primary key is the only
exactly-once there is; a crash between claim and run loses the action rather
than doubling it. Its archived-context test went red on the first try and
found a bug older than the lane: archiving stamped a timestamp and left the
state column at `live`.

The first live probe said the ending was not there yet: only the shell gate
recorded executable source, and with hooks installed every production ask
comes from the hook gate, whose doc said a hook ask has no free variables —
false in a way sharper than the executor, since an allow rule on `dd
of=${DEV}` would have redeemed every future value. The hook gate plans a
shell call the way the shell gate does now. And the hook feeding the scorer
from Amy's terminal was shelling to whichever `kaish` was on the path, which
the 0.17 bump had swapped underneath it. *"Let's use kaish as a library which
mcp already does so there's no way to have version skew."* `docs/gate-shape-b.md`.

## The name that had no home (September 5–8)

Amy opened a Saturday with her own routine on the table: she restarts every
Claude Code session each morning, and again whenever a context's prompt
cache has gone cold, and the thing that makes a restart cheap is a handoff
she and the lead maintain by hand. She had been wondering whether it was
time for "a lil message board." Within a few exchanges she had reframed it:
*"really message board would be a rework of the drift queue."*

The musician already had the answer in miniature. A player's score lives in
a track's context, written ahead of the playhead; a page-turn forks a thin
child that re-attaches; the child rehydrates from a window plus the last
eight phrases the kernel hands it. Map that onto the lead and the seat is a
track, the handoff is the score, the restart is a rotate, and the morning
read is the window. The signoff file's flaws, whole-file rewrite, no stamp,
no author, no window, all fall out of using blocks instead.

What the conversation could not get past for a while was the connecting
noun. A context is ephemeral. The context graph is a forest, and Amy's
instinct that it could not be the container was right: the identity would be
a whole tree, and no tree is a thing you can hand a note to. The name is the
home, she said, but "the name needs a home in code. A bare string or
identifier probably isn't it." Game design supplied the shape: a character
sheet, a collection of things with an opaque id in the records and a given
name at the table. **Character** also turned out to be the missing word in
a set kaijutsu already used, cast and role and context, the thing an actor
is cast as. Amy is a character. So, eventually, is everyone in the house.

Then she pruned. Party went, because the accountability chain covers it.
Chair and seat went, because no concept needed them. The purse and an
availability field were deferred. Presence stayed, with a caution against
ever tying one client to one character, since she is connected two to five
ways on a normal day. And the night shift became a janitor first, with a
proctor sweep as its second job: notice an idle session, ask Amy, then drive
the session to write its own handoff note while its cache is still warm,
because *"the session's own model is both the best summarizer and the
cheapest one."*

The code review at the end changed the plan more than the design did. A
principal already has an opaque id and many credentials mapping into it; the
roster already knows principals, contexts and liveness. So a character is a
principal with a sheet, and the design adds no new identity type. The lead
first read the turn path as stamping a model's blocks with the human who
drove the turn; a kaibo review corrected it: the blocks carry the system
principal, because there has never been a principal for the model to be. A
frontier deliberation that evening moved the handoff onto an ordinary context
with a hydration window and split every turn's identity into the requester
who caused it and the character who performs it. `docs/character.md` carries
the slices.

The prework went out as six small lanes, and two turned into corrections: a
comment said the roster's refresh loop was never called, and it had been
called since August. The rule that came out is small: run `git log -S` on a
comment before building on it. The third lane found the restart mystery from
Thursday, which was never the kernel: the hook listener archived its own
context on any session's end, because the id it thought was its own came
from a transcript scrape that named the session before.

The sheet and the keyring shipped the next day, and the keyring changed
shape while it was being built. The reading that moved it was that the
kernel never reads `auth.db`: the join key was always a bare principal id,
and every name in the system was a display label cached wherever the
identity had been seen. Amy took it further in two steps, first *"maybe
authdb should bind a key to a principal id only, and nicks melt into the
character,"* then *"why have a name in principal at all at that point?"*
The struct lost its name fields and then its reason to exist. `add-key --as
<character>` binds and never mints; retire takes a character's contexts with
it; a fresh kernel seeds `hajime`, a character built to be retired, with a
minted id because *"deterministic feels like a choice we'd regret."* The
migration ran on Monday morning after a full rehearsal on a snapshot, and
every rehearsed number held. The one thing found on the way was that an
unmigrated restart would have come up half-working and silent, since the old
table satisfied `CREATE TABLE IF NOT EXISTS`; the server now refuses to start
on a pre-melt keyring, and the first draft of that test proved the guard by
hanging.

The handoff followed the same afternoon. A note is ordinary authoring, so it
is not gated; a `tail` never mints, because a read-only verb's whole flag
surface must be incapable of a write; `--for` lets one character write into
another's log under its own name, which makes "a message board is this log
read by someone else" true before addressing exists. `S16-handoff.kai`
injects the character's recent notes at create. Its last mile was a power
cut: zorak lost power mid-verification, the unit restarted itself on the
right binary, and the only write in flight had never run, because the
advisory gate had escalated `kj handoff note` from an MCP seat. That
escalation became the next design: Amy asked for a health survey of the
safety hooks and then for layered allow and deny lists, and a Crush session
driven by qwen wrote `docs/gate-policy-tuning.md` — one evaluator over
builtin, global, per-type and learned tiers, with the user's explicit list
outranking everything shipped. `signoff.md` gave up its durable third to
`docs/operating.md` the same day, on the way to being retired by the log it
described.

Banto made the next missing part concrete. Its director instructions knew its
name through `KJ_CHARACTER`, but the context had no performer in `played_by`.
Amy asked to align `AGENTS.md` with the character system, then added, "I'm open
to completing some of that character work too." Explicit create-time `--as`
now records the performer without changing the requester. Director and handoff
scripts read that metadata, removing the environment bridge. Retirement can
therefore find these contexts through the existing relationship. Provider block
attribution remains a separate change because it changes block identity.

A runnable handoff example exposed why dispatcher tests alone were insufficient:
the kj adapter rebuilt split arguments with flags after positional text, turning
`--for` into note content and writing into the caller's log. Kaish 0.17.2 already
had the ordered-argument interface the backlog said was missing. Using it removed
the reconstruction code. The regression executes the published advice and checks
which log received the note, including names that require literal preservation.

## The hardware gets its own body (September 7–8)

Amy wanted MIDI presence and music to survive closing the 3D app, then
widened the task to PCM: "a realtime audio daemon we can put on various
machines that have audio hardware." Realtime meant an ordinary service with
Linux RT priority available, not a new scheduling architecture. The DJ, the
PCM scheduler and the MIDI workers moved into a reusable library; each node
connects over SSH and performs kernel cues; the kernel remains the sole
sequencer while nodes keep local beat phasors for display. Device-open
failure is explicit, RT-priority failure is a warning, and the hardware
lifetime no longer depends on a window. `docs/audio-daemon.md`.

Retrospective recording followed from *"grab a happy accident real quick."*
MIDI input feeds bounded per-source RAM history independently of recording
a context; a keep copies a complete recent window, protects it from
eviction, and exports it through the existing SFTP path into CAS before the
RAM is released. Source generations and explicit loss reject incomplete
windows instead of claiming a complete recording.

The daemon's first fitness review settled two things. The app's opt-in
in-process host was deleted and the last ALSA read left the app with it: the
patch bay now reads the kernel's projected audio inventory, so a remote
node's rack is visible from any app. And the live probe found moltar's clock
100.9 s behind zorak's with NTP on neither host, which the one-timebase
doctrine had quietly assumed away. Amy: "I want to consider if we can be
resilient to some clock skew, even lean into it a lil." The answer made the
kernel's clock the timebase by definition: every node models its offset from
the ping round trip and mints and ages stamps in the kernel's domain, and NTP
became optional. The same morning explained a ghost peer registration — the
bridge task's self-detach lived on a LocalSet that was dropped before it
could run — and moved that cleanup onto the connection's own Drop, the only
teardown that runs.

The later inference review kept placement open. Amy: "for now we'll
evaluate kaijutsu's tradeoffs, and decide later about the new home." Local
embeddings had moved to lfm2d, but offline beat analysis still decoded and
ran ONNX graphs inside the kernel. Optimized probes processed a synthetic
three-minute track in 2.32 s with 564 MiB peak RSS; two simultaneous jobs
peaked at 1.37 GiB. Small weights did not remove the full-file buffer cost.
The review proposes a bounded analysis executor, distinct from the kernel's
music state and audiod's hardware timing. `docs/audio-inference.md` records
the evidence and the limits; no model or service was moved.

Amy clarified the performance model: "near-term latent decisions", models
"dancing at their own pace, integrated just in time for the performance."
Seconds of model work and SFTP transfer fit when anticipation supplies the
lead time. The relevant measurement is valid work ready for its musical
moment on the shared pulse. That points back to the existing resolver and
commit contract. She also questioned beat analysis as a core `kj` verb;
an optional tool with a resolver adapter is a candidate, with its eventual
home still undecided.

The principle now leads README and contributor guidance: "The kernel’s
central responsibility is coordinating anticipation and commitment on the
shared pulse. Each model can have its own pace, provided its output arrives
while it’s still useful." The iteration plan starts with dependable
interaction and controlled producers sharing a timeline. Source review found
the important limit behind the resolver design: `resolve` still runs inline
under the timeline lock, and the production adapter validates prepared CAS
content. Slow model preparation needs an explicit completion path before
that seam can carry it. The next proof should expose this gap without
spending model tokens or choosing another service architecture first.

## The file that answered a question the code already knew (September 9)

Amy read `contrib/kj-expectations.toml` and said something felt off without
being able to say what. The file was the authored half of a probe corpus:
clap reflection listed every `kj` leaf, and the TOML said for each whether
it mutated, whether its handler asked for confirmation, and which severity
the lfm2d scorer was expected to assign. Two of those three were facts about
the handler, written down beside it in a sidecar that a coverage test could
check for presence and never for truth. "Does this verb write?" had three
answers that never checked each other: the TOML, a two-token pass list in
`kj/readonly.rs` that could not see past the second word and so gave up
`backend default show` and `block cat` wholesale, and the handler body. The
gate-policy plan was about to add a fourth column to the same file and say
in the same breath that it must not agree with the third. And the one
column that was genuinely an expectation was the scorer's vocabulary, not
ours.

Amy: "the declaration whether something mutates should be required on every
kj verb, and can be accessed there directly as source of truth. I think at
one point I asked for a file export and it got over-generalized." And: "I
don't expect the classifier to ever learn kj vocabulary and we control the
code here." So the declaration moved onto the verb. Every subcommand enum
carries an exhaustive match to `Effect::Read | Write | Destroy`; a new
variant with no arm does not compile, which is the entire coverage
mechanism. Because the match is over the parsed value and not the leaf name,
an argument that changes the effect is visible to it: `block cat` is a read
and `block cat --out` is a write, in one arm, where the old table had to
refuse both. A root clap enum wraps the forty-two domains so the reflected
surface and the classified surface are one parse, and `classify(argv)` runs
no handler.

Three mechanisms fell out. The read-only module kept its five structural
conditions and lost its tables. The eight handler-local confirmation checks
became one gate in dispatch, and the decision to always latch a Destroy cost
two handlers their state-dependent messages, accepted. `kj transport list`
became a read on its own merits, undoing an earlier instruction to keep the
whole verb out of the pass list, and a `${VAR}` in a typed slot fails
closed rather than earning a second, lenient parser. And the lfm2d fixture
left the kernel: severity derives from effect, the probe keeps only the
calibration deltas beside itself, and the word "expectation" is not
kaijutsu vocabulary any more.

Two lessons. Reflecting the keys of a hand audit mechanizes half of it and
leaves the values to rot at the old rate; the fix is to put the judgment
where the compiler can demand it. And the corpus, built to remove clauses
that do not parse, had been scoring eighteen of them: synthesis had filled
positionals and never required options. Classifying every leaf through its
own synthesized clause found that on the first run.


## Context types choose their prompts (September 10)

We started with a source dossier: omp's assembled prompts and setup, then
DeepSeek Harness, Codex, Crush, Goose, Hermes, Aider, Kaibo, and Polytoken's
published documentation. The aim was to learn from their accumulated failures
without importing their whole execution model. Complete Kaijutsu prompt bodies
went into an offline comparison so Amy could read the old and proposed wording
together. That comparison now reads current seeds against a pinned baseline;
its source hashes keep regeneration stable after a commit, and its dark mode
makes it usable alongside the rest of the working environment.

The reviews made the input mechanism matter more than the word choice.
Distillation ignored exclusions, lost every block's tail after 2000 bytes, and
asked models to preserve identifiers the formatter had never supplied. Compact
forks also lost the coder's stance: their fork hooks did not run its create
script. The fixes share hydration admission, carry source and tool evidence,
bound complete turn groups, and retain chosen instruction blocks and a recent
native turn. Briefing and continuation now have separate host-file instructions
and source-context length controls. A stale source version fails compaction
explicitly; a filtered copy cannot point at a child block it omitted.

Claude's review exposed a conflicting contract in the proposed universal base:
working guidance about investigation and handoffs could interfere with a
musician's ABC-only performance. Amy removed the universal premise: “I don't
think kaijutsu should force any one prompt into every context type.” Shared
prose is an ordinary rc file, selected by symlinks in coder and default. Other
types keep their own instructions. There is no new prompt registry and no
hidden global fallback. The shared file still ends with the encouragement Amy
wanted to keep: 頑張（がんば）って！

`docs/prompts.md` owns the implemented contract and migration. The source
comparisons remain in `docs/oss-comparisons.md`; checked review mistakes became
evidence-status examples in the Kaibo proposal. Temporary drafts and review
plans were melted into those documents, with their earlier versions recoverable
from git and the private review archive.

The Kaibo implementation review also corrected our own explanation of timing:
stored instruction blocks are read again before each turn, even though ordinary
history edits wait for hydration. The distinction supports quick prompt
iteration. A failed instruction read now stops preparation instead of silently
running a context without its chosen prose. The review confirmed the main
contracts and left metadata snapshot and fork-initialization fault handling as
named follow-up work, with their limits recorded rather than called solved.

The same separation now governs the repository instructions. `AGENTS.md` keeps
work rules, essential invariants, and pointers; `docs/writing.md` owns the full
writing guide and terms. The character document separates current behavior from
its original inventory and rollout. This removes planned sheet fields from the
working contract and corrects the claim that all block edits wait for hydration.
The aim is fewer competing explanations, with correctness checked against code;
source length alone is not evidence of better model behavior.

## The kernel that fsynced every word (September 11)

Banto's seat was rotated onto the character work the morning after it
landed: build, force reseed, bounce, one `kj context create --as banto`,
then archive and rename. The first job through the new seat was the context
cleanup Amy had promised it: 421 no-model contexts, most of them mirrors of
dead MCP sessions from mid-August. Banto reconnoitered on its own before
acting, submitted the archive as one gated loop, and the shell gate held it.
The lead denied that first ask: the filter excluded rows aged `now`, and a
live seat reads `1m` after a minute idle. The deny resumed banto with the
corrected filter already in its history, it re-verified the two live seats
were absent, and the second ask ran 412 archives. Two facts about driving a
seat came out of it. A user block written into a running turn's log does not
reach that turn; it lands on the next drive, or on the resume a deny triggers.
And `kj context list` reports "(no model)" for cast-resolved seats and handoff
logs too, so a cleanup rule needs cast and handoff exclusions, not just the
model column.

The P1 that made the tui feel slow turned out to be an fsync per word. An
Opus lane measured the live kernel while a probe context streamed: about
200 KB of block-layer writes per delta, 428 MB for a 10 KB turn. Each delta
was two autocommits on one connection, the oplog insert and the context's
activity touch, and `init_connection` had never set `synchronous`, so SQLite
ran at FULL and fsynced both. On btrfs with DUP metadata each fsync writes
its metadata twice. A control on the same volume with the same shape fell
from 175 KB to 11 KB per delta at NORMAL with identical syscall bytes, which
put the cost in the filesystem's fsync path rather than the payload. Amy
took the durability trade with no UPS: "go with NORMAL, seems fine." Under
WAL, NORMAL stays consistent through a process crash and only a power loss
can drop the last commits. The two writes now share one transaction.
Measured live after the bounce, a prose-only probe wrote 13 to 15 KB per
delta with syscall bytes equal to block-layer bytes: the amplification is
gone, and what is left is the payload of two WAL pages and a log line per
token.

A smaller storm rode along in the logs. The `llm.turn` span recorded its
usage fields from every `Done` event, and tracing-subscriber's fmt layer
appends each record to a span's formatted fields instead of replacing it, so
by iteration ten every log line under the span carried ten usage blocks. A
drop guard now records the final call's numbers once at turn exit. Its test
taught a lesson about tracing in a large test binary: a callsite caches its
interest against the default subscriber of whichever thread hits it first,
and under a full suite that is a sibling test with no subscriber, so a
thread-local test subscriber saw no spans at all. The test installs a global
default and tells spans apart by the thread that created them.

With the unit at `RUST_LOG=info`, INFO still read like a debug log: a line
per recovered context at boot, four per SSH connection, a reconnecting
client's hang-up as `ERROR Session error: IO(…)`, the shell command text
twice per command, the audio inventory every ten seconds, and eleven lines
per turn iteration with tool params in full. The diet keeps what an
operator reads a turn by: the stream start with context and model, each
iteration's completion with stop reason and usage, interrupts,
cancellations, and tool refusals; one line per connection and one summary
per boot. The kernel's own disk writes did not move with the log level,
which put the log cost on journald where it belonged.

The ask link went live in the afternoon, written by a DeepSeek coder driven
from the tui, and a probe coder confirmed it: an allow filled the Waiting
pair with the real output, authored no second pair, and the next turn
hydrated clean. Asked for that output, the model then insisted nothing had
run. Two things in the durable log said so. The `gate.pending` error child
authored beside a Waiting result outlived the ask, because the fill touches
only the linked pair, and error children sorted after every later block of
the turn, because the loop re-anchored on the result block rather than the
child it had just hung off it. A pending ask is not an error, so the
mapping now withholds the payload for a Waiting dispatch and no child is
authored; and both dispatch paths anchor the next block past an error
child when one is. Four tests pin it, two of them reproducing the exact
`#18 #19 #17` order from the probe before the fix.

## The approval that resumed its coder (September 12)

Saturday opened on a five-item list, smallest first, and the first three
landed by mid-morning. The approval chain from Friday had one piece left:
an allow filled a model's own Waiting pair, but the driver's rule dated
from when only a human's shell pair was linked, so a linked pair "told
nobody" and the coder read its own stale "waiting on a human" text as the
last word. The design doc already had the answer: a pair whose turn ended
at the gate gets a seed block and a turn request. The ledger now records
who owns a linked pair, `turn` from the model tool path and `session` from
the interactive shell, and a turn-owned fill wakes the model exactly as a
driver-authored one does. Amy confirmed the consequence: "kj ledger allow
should auto-resume", with a staleness check to follow, since an approval
answered hours later revives a turn whose prompt cache is long gone and
that costs real money off a subscription. The seed is written even while
a turn is running, because the fill is an in-place edit a cached mailbox
never re-reads; only the turn request waits.

Two smaller ones rode along. The activity stamp on every streamed delta
rewrote the contexts page in the WAL to move a millisecond timestamp the
readers show in minutes; it is throttled to once a second per context, to
be measured on the next bounce. And `kj rc list`, the one surface that
compares live rc against the shipped seed, walked `lib/hooks` and then
dropped every entry because a hook body is not a lifecycle script path.
That was how Thursday's stale lfm2d hook stayed invisible for a day. Hook
bodies now list and show beside the scripts.

The morning also had two of Amy's sessions in one checkout. A second
session began an approval-identity change in the same ledger files the
pair-owner lane had just finished in, building on the uncommitted work and
re-indexing its row reads. Separating the hunks by hand would have broken
the other session mid-edit, so the lead rebuilt its own versions of the
five shared files from HEAD plus the lane's known edits, staged them with
`hash-object` and `update-index` without touching the working tree, and
proved the staged tree on a detached worktree with its own target
directory before committing. The other session's diff against the new
HEAD is then only its own change. Four coder-stance e2e tests turned out
to have been red since the September 10 prompt rework, pinning phrases
that no longer ship; they pin the current tier lines now.

The next approval problem was identity. Amy asked why the TUI could not let
her answer a coder in the context she was already reading: "When I connect
via app or tui, I should appear as myself." The ledger compared contexts,
the TUI chose a second one to answer, and model tools still carried the
requester's principal. Replacing only the comparison would have made Amy
and her coder indistinguishable. Her second scenario supplied the reviewer:
"the coders would come back to the lead model which can evaluate & approave"

Invocations now carry requester, performing character, and assigned reviewer.
A model turn resolves its performer from `played_by` and its reviewer from
the context, then keeps them through tools, shell commands, hooks, and
replay. The requester stays the capability and redemption identity. Asks
snapshot the three identities; reviewer decisions work in the raising
context, and switching contexts never makes the performer eligible. The
reviewer can pass an ask onward, and its requester or performer can cancel
it. A performer change revokes learned session rules and prevents replay of
a linked approval raised by the old performer. Decisions recheck reviewer
eligibility under the ledger transaction, so escalation cannot race that
check.

The TUI's second-context workaround is gone. It displays the connected
character and the ask's identities, implements full detail, and keeps
unshown asks available for later presentation. ACP uses the same reviewer
rule and sends decision failures into the client conversation. New ACP
sessions select their performer explicitly with `--character`; creation
uses `kj context create --as` so identity is present when rc runs.

The regression tests exposed the useful mistakes: the MCP RPC entry point
lost the reviewer, shell completion hooks rebuilt a requester-only caller,
and recursive context queries still read depth from the column now holding
the reviewer. The wire test exercises two real credentials: Amy answers the
coder from the same context, then the coder is refused from another one.
Terra agents implemented the ledger, invocation propagation, and clients;
Astra integrated and reviewed the resulting paths. Amy authorized sending
source through Kaibo for its independent review. It found that ACP treated a
prompt timeout as the reviewer denying the ask. Timeouts and cancelled
prompts now leave the durable ask pending; only a selected decision records
a verdict. Review also separated the TUI's pending snapshot from its shown
card set, preserving the next ask while another card is open.

Fable's review had also found that settling a denied model tool pair did
not tell its turn. Denials and cancellations now settle the pair, evict the
cached conversation, and resume it with an explicit no-run receipt. A failed
shell materialization tells the waiting model too; connected session pairs
continue to receive their result through the block feed. Wire regressions
cover both denial and cancellation without executing the command.

The local deployment verified the same contract with a DeepSeek Flash coder.
Its ask carried `coder` as performer and `kaijutsu-lead` as requester and
reviewer. The coder stopped; the lead approved the exact echo; its output
filled once and the coder resumed without a second tool call. The probe was
archived and its scoped hook removed. Banto's ROOT now names Amy as reviewer.

Amy then asked to look at the Bevy app and to "double check we are attaching
the character info to otel spans where appropriate". The app already knew
its SSH identity but used a random principal to find and hide its own draft.
That principal now comes from `whoami`, is cleared while disconnected, and
is refreshed on reconnect. Explicit key selectors make the credential choice
available at launch. Submission errors reach the UI, and failed text stays
with its original context and character. The app still needs a dedicated
ledger surface; its shell is the current approval interface.

Execution traces now distinguish requester, performer, reviewer, and deciding
actor. Model turns also carry the character names resolved at turn start.
Tool spans take IDs from the invocation, while approval replay takes them
from the durable ask. The trace audit found two contributing factors beyond
missing attributes: RPC injection read OTel's thread-local context instead
of the active tracing span, and the MCP server-call span held an entered
guard across suspension. Injection now uses the tracing span's OTel context,
and the future carries its span while polled. Character IDs remain off
metric attributes.

Kaibo's GLM-5.3 review confirmed propagation and found a missed-status path
in the app: a replacement actor could connect before the UI subscribed, so
the old identity survived until the new `whoami` arrived. Connection status
now identifies its actor and transport; lag recovery asks for the current
state, and actor exit clears authenticated identity. The full headless app
suite and client suite passed, with focused capture tests for exported trace
parents, concurrent tool actors, and approval replay. GUI and macOS execution
remain unverified; this followup did not restart Amy's running processes.

Amy clarified the default: "Amy approves by default; delegate approval
explicitly", then "Allow an explicit director-wide delegation". A creator's
role as director is now separate from reviewer authority. Resolution prefers
an explicit context override, then a grant for the director, then the
configured default reviewer. A grant may name the director or a separate
adjudicator; it neither creates that character nor schedules a review turn.
Amy controls grants and routing changes, and can explicitly reclaim a pending
ask from an unavailable reviewer. The audit keeps the caller separate from
the old and new reviewer.

Revocation exposed two timing boundaries. A running turn may retain an old
reviewer, so the gate resolves the current assignment while inserting its
ask. An assignment command may await configuration after checking authority,
so its transaction checks authority again before committing. Pending asks
must settle or cancel before their context's routing changes. The migration
clears old automatic reviewer assignments with the new director column in
one transaction; an injected reset failure verifies rollback and reopening.

The SSH/RPC regression uses Amy, lead, coder, and judge credentials. It checks
Amy-default review, refused self-grants, judge approval after explicit
delegation, Amy reclaim, pending-ask refusal on revoke, and Amy routing after
revoke. Kaibo's DeepSeek review found that broken reviewer metadata also hid
the context information needed for repair, and that eager default resolution
overrode valid explicit assignments. Inspection now exposes the error;
invalid default configuration clears its cached authority without replacing
a valid explicit reviewer.

Amy separated approval lifetime from coder continuation: "an ask doesn't
really need to expire" and "I like continuation window". The window now lasts
30 minutes from the last actual provider inference request, including each
tool-loop iteration; yielding does not renew it. It governs automatic model
resumption, never ask expiry. `kj handoff signoff <note>` closes the window
immediately.

Shell work is now kaish work. `foreground: false` is the default and returns a
stable receipt with durable operation and optional ask IDs; completion is a
separate fact. `foreground: true` waits for the result. `kj wait` observes
operations, asks, and jobs without controlling the work or resuming a model.
Coder checkpoints retain unfinished operation and ask IDs before a yield or
signoff. Rotation remains manual while we design successor linkage and a
janitor for obsolete asks and unfinished operations.

The source audit also corrected a lifecycle assumption: a pending tool gate
does not force the model loop to stop. It receives the pending result and can
write a handoff. A stable receipt and a separate completion record keep that
original model receipt immutable.

## The message that knew where the player was looking (September 13)

Amy: "the ui knows what the user is seeing when they send an async message.
we also know wall clock time but that's not what matters; the context at the
time the user sent it." A message sent while a turn runs lands after every
block the model produced meanwhile, and the model reads it as a reply to its
newest output. Wall clock cannot fix that; the reference the model needs is
a position in the conversation.

The client is the authority on what it showed. `submitInput` takes an
optional edge, the newest block the client had shown plus a character count
when that block was still streaming, and the kernel stores it on the user
block in the same journal op as the draft promotion. The kernel never
guesses or validates an edge, and an older client sends none. The rendering
is not kernel code: a `submit` rc verb fires awaited inline after the
promotion with the input block, the edge, the log tail, and turn liveness
as variables, so every experiment is a script edit. Amy on turn ordinals:
"agreed on the ordinals, it would be difficult to do well. the block id you
suggest should be sufficient, an rc script can decide what to do with it
(make up an ordinal that's good enough, give a rough %, etc.)". The shipped
example emits one notification only when the edge sits behind the tail, and
no type links it by default. The tui sends the live band's tail when it drew
one, else the last block printed.

Two lessons. A lane that can only test a kaish script against a stub `kj`
has not tested it; the wire test that links the seeded script into a type
and asserts the exact rendered line is what proved `jq`, `cut`, and `kj
block read` behave. And a `case` pattern in kaish never expands a variable,
so the script compares with `test`.


## The tui takes the alternate screen (2026-09-13)

Two weeks on the inline viewport settled the buffer question. Amy: "I
think I'm going to change my mind about the terminal history, and let it
be trashed. We can build up what I want from scrolling even better." The
costs that decided it were all one root: nothing printed into scrollback
can be redrawn, and the terminal cannot say what it kept. A shrink left
blank rows under the band; a slow ssh hop stalled two seconds on the
cursor query every grow; contexts interleaved in one history; copy mode
was a snapshot. Decision, mouse contract, buffer model, slices and the
terminal features it unlocks are in `docs/tui.md`, "The owned screen".

Lesson from the research: the mouse question is settled by not asking.
With mouse reporting off, wezterm and vim both turn the wheel into arrow
keys on the alternate screen, and the terminal keeps select, autocopy
and paste with no modifier. Crush captures the mouse every frame and
rebuilds selection inside its widgets; that is the model to avoid.
Reports in `~/exomemory/kaijutsu/terminal-research-2026-09-13.md`.

The morning's smaller fixes (picker follows the kernel, every exit
restores the terminal, bracketed paste, the cursor-query fallback) stay:
the first two survive the move, the last is deleted with the viewport.

All five slices landed the same day (998cd3c8, f5cd5dae, 56101212,
66dea65c), each as an Opus lane with no git authority, a Sonnet docs
pass, and a kaibo review whose findings went red-first into the same
commit. Two lessons. A guard the harness can count beats a guard it
cannot see: the zero-cursor-query probe caught `Terminal::clear` asking
on every resume, which no reading found. And the crusoe cast timed out
on transport twice mid-afternoon; the deepseek cast carried the last
three reviews without a miss.

## Retiring duplicated state (September 16)

Amy asked for a whole-file architecture scan, then to "proceed with 1-3"
from its bounded cleanup plan. The recurring problem was divided ownership:
older representations retained responsibilities after their replacements
arrived. Larger changes are marked at their implementation sites and tracked
in `docs/issues.md`, "Architecture cleanup plan".

The first deletion removes `/v/input`. Its adapter used the authenticated
requester while nested shell commands carried a distinct performer, so a
model's read-only shell could read the requester's unfinished draft. The
writable shell could replace or clear it. Three regressions demonstrated those
paths before the adapter and mount were deleted. Client compose remains on
its RPC path; this deletion does not establish that generic block access or
`/v/docs` excludes drafts. Those routes remain an explicit audit item.

Kernel construction now has one service initializer. `Kernel::new` supplies
its existing identity and flow-bus defaults to `with_flows`, so broker setup,
file-cache wiring, and unfinished-operation recovery cannot drift between
two copies. Ephemeral construction continues through that same path.

The legacy `KernelState` facade had no production callers for its variables,
history, checkpoints, or second UUID. Removing it leaves the kernel name in
its own lock and identity in `Kernel::id()`. Durable `context_env` and
`context_shell`, per-invocation kaish scope, and connection command history
remain their existing owners. Tests solely exercising the retired API were
deleted with it; context-shell and embedded-kaish tests cover the live paths.

The next batch separates turn ownership from mailbox reset. Simultaneous
first lookups and reset during an active turn both reproduced distinct locks
for one context. The registry now serializes lookup and idle eviction; a held
session keeps its mutex and consumes a pending reset when the next turn locks
it. Idle LRU hydration policy stays unchanged. The regressions pass, including
the existing two-turn approval-resume test that verifies refreshed tool output.

Draft isolation now extends through generic block access. `/v/docs`, `kj`
block commands and search, semantic source adapters, and both MCP block
adapters exclude unsubmitted blocks. Generic status commands cannot create or
promote drafts. Historical reads reject draft-era text even after submission.
The regressions reproduced reads through both model-shell flavors, `kj`, MCP,
and journal replay; client compose and feed queries retain their draft view.
The separate wire-facade identity question remains in `docs/issues.md`.

Block mutations now have one acceptance owner. It retains the document guard
through preparation, durable commit, compaction, and publication; metadata
projections come from that same state. Controlled pauses reproduced the old
released-guard window, and a failed database write reproduced readable
uncommitted text. The failure policy is a poisoned document until restart,
which recovers the durable prefix without copying the conversation for every
token. Journal counters advance only after commit.

The same audit caught failed forks visible before their initial snapshot.
All three fork variants now share atomic row/snapshot persistence before
publication. Keeping lock order explicit also exposed a subtree fork that
reacquired its held database mutex; a persistent-store regression reproduced
it where memory-only fixtures had not. Compound compose selection and complete
change-feed acceptance groups remain separate, marked follow-ups.
Journal replay now distinguishes an append from an explicit edit at the end.
Appends preserve existing style spans; edits invalidate them. A regression
through the real oplog reproduced lost spans and checks text, spans,
provenance, and the edited marker, including an empty append. The payload
already carried the distinction, so this needs no storage migration.

Compose now retains the document guard from draft selection through creation,
editing, or clearing. Each write uses the same acceptance helper; there is no
second compose lock. Draft creation journals both hydration exclusions in its
initial snapshot. Controlled pauses reproduced all three released-guard
windows; competing create/submit calls and journal replay check the result.

Change-feed publication now carries explicit group boundaries through FlowBus.
The bridge finishes each group before closing a delivery, preserves publish
order without sorting, and discards incomplete groups on termination. Topic
filters mark their own final matching event; timing messages stay independent.
A batch-limit regression reproduced a split draft submission, then exposed the
client mirror's assumption that every event had a distinct version. The mirror
now accepts equal versions inside one delivery while rejecting duplicates
across deliveries. Separate output and status mutations remain ordered separate
acceptances; batching never made them one transaction.

A refused or timed-out feed callback now ends the feed and disconnects for
snapshot recovery. Only acknowledged deliveries advance the reported version;
an uncertain append cannot be retried safely. Both regressions first reproduced
a feed that kept running after failure. They also check that a refused or hung
termination notice cannot prevent disconnect.

Shell submission captures the draft text with a process-local revision token.
After command acceptance, it deletes the draft only if that revision still
matches under the document guard. Edits, replacement, promotion, and reload
invalidate the token; unrelated output does not. The token is not persisted
or exposed on the wire. Three SSH regressions first reproduced lost typing
while a hook paused submission, including editing back to the original text.
Kernel tests cover guarded deletion, stale-token no-ops, and journal replay.

Amy then asked where the recent kaish setup helper landed and whether rc and
kaish integration needed clearer owners. `spawn_kaish_thread` centralizes stack
reservation; the older context-shell factory already shares construction.
Completion still crosses server RPC helpers and separate MCP projection code.
Rc is an adjacent lifecycle owner, while hooks and editor commands use kaish
under their own contracts.

The next objective is to "*completely* migrate kaijutsu and clean up all the
call sites," starting with documentation. `docs/kaish-integration.md` records
the caller inventory, execution contracts, phased deletion, verification, and
comment cleanup. This is planned work; the documentation commit changes no
runtime behavior. The architecture summaries now identify the kernel-owned
block store and four flow buses, and the stale background-exec exception was
removed from the live issue list.

Amy is "open to removing the .md feature" and confirmed "that's fine if the
script uses kj." The plan calls for explicit `.kai`
instruction authoring with `$0` naming the invoked VFS path. The locked kaish
already supplied positional-parameter setup; the rc adapter had not exposed it.
Printing Markdown currently produces diagnostic trace text, so replacement
must author a block and verify its content, metadata, and rendered instructions.
Ordinary companion-file reads also differ from the loader's current snapshot
of all executable bodies; that choice must stay visible in the migration.

Contextual construction moved from the eight dispatcher factory methods to
`EmbeddedKaish::for_context` in `runtime/context_shell.rs`. Every caller now
supplies `ShellIdentity` and a named `ShellPolicy`; rc policy carries authority
that only lifecycle orchestration can construct. The old module and entry
points are deleted. Synthesis block-source adapters moved from lifecycle into
runtime ownership. Adjacent comments now describe current behavior rather than
retired tool names or rollout history.

Construction no longer silently omits `kj` and editor builtins when dispatcher
registration is missing. A regression reproduced that fallback before it was
replaced with an explicit initialization error. Lifecycle test fixtures now wire
the dispatcher as production does. That change retained rc loading and command
settlement for their own migration steps.

Lifecycle orchestration moved into `rc`, alongside the shared runtime. One
`rc::run` entry takes the invocation facts; create, fork, attach, drift, beat,
rotation, and submit callers all moved, and the dispatcher lifecycle methods
were removed. Discovery and `kj rc` now share the rc module's path grammar.
Tests live beside the lifecycle owner, with the unused-argument fixture adapter
deleted. An unknown verb now returns an error; its regression first reproduced
the previous silent success. Amy resolved the instruction-authorship split: "Use the invoking performer
consistently with kj." Existing instruction blocks retain their authors.

Rc supplies the invoked VFS path as `$0`. `kj block create` accepts exact
stdin text and an explicit content type, so scripts author Markdown through
the ordinary performer-attributed write path. File redirection preserves
trailing newlines and avoids stdout preview limits. Automatic `.md` loading
is deleted; every shipped Markdown instruction has a `.kai` partner, including
composed script/data symlinks. Canonical Markdown remains visible as data in
`kj rc` inspection and seed comparisons. Non-forced reseeding installs wrappers
without replacing custom Markdown, and custom entries need their own scripts.

Tests cover distinct identities, symlink-relative companions, input beyond the
internal output ceiling, invalid/missing/empty input, explicit content precedence,
and every migrated context type's rendered instructions. Executables remain
snapshotted before a run; companion data is read during execution. The script
digest records the executable only, and the authored block retains input text.
Two tests exposed contributing factors outside the loader: kaish silently expands
unsupported `${0%.kai}` syntax to an empty value, and clean file-cache entries
used a symlink's own generation to judge its target's freshness. Scripts use
supported `dirname`/`basename`; clean symlink reads refresh their targets.
The remaining dirty-buffer/generation audit is recorded in `docs/issues.md`.

The interactive/approved command owner moved from server `shell_run.rs` to
`runtime/command.rs`. Result conversion and cwd/export snapshots moved out of
RPC; MCP envelope construction uses the shared result module. The old server
module and duplicate text-replacement helper are deleted. Paused result hooks
reproduced premature Done publication in both interactive execution and the
structured-kj wire path. Both now defer terminal blocks until hooks settle.
A late export-write failure also reproduced partial cwd/environment persistence;
the shared state owner now commits the diff atomically and returns errors.
Interactive and approved commands now retain one `CommandOutcome`. A durable
record keeps the raw execution separately from a hook replacement or refusal;
receipt polls retain their existing effective-envelope shape. Blocks, receipts,
and kaish jobs project from that record, and the block-reading reconstruction
helper is deleted. Regressions first reproduced replacements carrying stale
failure exits and real exits 2/3 producing successful blocks. Replacements now
clear old metadata, retain structured content, and report no physical exit.
Kaish job control uses an explicitly synthetic 0/1 result where its API requires
an integer. Unmodified jobs retain their complete execution result.

Receipt and raw record commit atomically before terminal block publication.
Failure injection verifies rollback and retry without execution; restart reads
retain both raw failure and synthetic success. Paused hooks and an SSH/RPC
replacement test check the publication boundary. The remaining structured,
streaming, and MCP paths still need this outcome owner. Automatic projection
recovery and approval of already-executed result hooks remain explicit work;
returning a persistence error does not by itself recover an accepted operation.

Recovery tests then reproduced two crash windows: a failed receipt write lost
captured execution, and a committed receipt could leave Running blocks after
restart. Settlement now retains the immutable outcome and a pending-projection
marker before any block writes. Startup hydrates and repairs those projections
without entering kaish or invoking hooks. A committed receipt plus terminal
output is proof that output publication finished; recovery preserves subsequent
edits and finishes only the remaining command status or marker cleanup. Another
regression showed why terminal status alone is insufficient: an old placeholder
can already be Done before a new outcome is projected. Preparation, completion,
and cleanup reject transitions that would discard a retained outcome. Initial
retention failure remains separate work.

Result review exposed another execution hazard: PostCall/OnError asks carried
the command source, so the generic approval driver could execute it again.
Interactive and approved commands now retain a review checkpoint and consume
the answer while holding the ordered hook snapshot. `hook_result` asks bind the
phase and captured result, carry no executable source, and stay out of the
generic resume queue. A second regression reproduced an identical later review
collecting the retained owner's answer; result reviews now bypass generic retry
redemption as well. Approval continues remaining hooks, including another
review, without repeating earlier hooks. Cwd/exports persist before the wait.
Cancellation, dropped waits, and restart retain execution and report interrupted
review; restart cannot restore the in-memory hook snapshot. SSH regressions
cover approval, denial, OnError, kaish escalation, and sequential reviews with
one observed command side effect. Unmigrated consumers fail result escalation
before creating an ask; structured/streaming RPC and MCP migration remains open.

Structured `executeKj` then moved out of RPC into `runtime/structured.rs` and the
shared command owner. Its separate settlement path dropped replacement data and
retained failed command metadata after a successful hook replacement. Regressions
reproduced both that data loss and unavailable result review. Authored calls now
register durable receipts, return Pending without holding the RPC open, and keep
the command task alive through review. Quiet calls share captured execution and
result projection without authoring a pair or receipt; their result-review
retention remains open. Context switches stay pinned for structured callers.
Typed refusals travel in retained outcomes, with absent fields omitted to preserve
the encoding of earlier immutable records. Literal argv and requester/performer
separation are checked at the kernel entry point. The literal-argument test
also exposed Bash-style backtick escaping that kaish preserved as extra text;
quoting now follows the pinned kaish parser. Its native argv API lacks per-call
execution options, so this path retains source execution and cancellation.

Quiet structured calls then gained the same retained review owner. The review
store now associates every ask with its invocation and an optional operation
receipt; a quiet call creates a record only if a result hook opens an ask.
Completion retains both captured execution and the final result, exposed through
`kj ledger show`, without inventing transcript blocks. Earlier sequential asks
keep their operation link. Tracked completion remains atomic with operation
outcome preparation; quiet completion stores an immutable result on its own.
Restart reports interrupted review for both, and the schema upgrade preserves
checkpoints written before quiet reviews were supported.

Streaming RPC then moved to the same command owner without transcript blocks.
Its adapter keeps execution IDs, subscriptions, history, and concurrency; the
runtime owns state write-back, result hooks, and retained review. Replacements
and denials now affect delivered output in every phase. Cancellation reaches
kaish through ExecuteOptions and remains active during result review. An SSH
regression exposed the unresolved truncation code in exit events; streaming now
reports the physical exit. A disconnect test corrected the initial lifetime
inference: each SSH channel drops its dedicated LocalSet when RPC ends, so the
retained review guard already settles interruption and preserves execution.
Teardown tests did expose two omissions: registered execution tokens were not
cancelled on connection drop, and local tasks dropped outside an entered runtime.
Connection teardown now cancels those tokens; both production and the shared SSH
test helper keep the runtime entered through LocalSet destruction.

MCP shells followed the same command owner. A regression showed PostCall could
replace the admission receipt and erase its operation ID before execution had
finished. Result-hook ownership is now explicit: ordinary servers use the
broker, while shell commands apply their hooks at execution completion using
the original tool identity and arguments. Background blocks, receipts, and jobs
share one outcome; foreground review releases the tool call without rerunning.
Read-only shells discard local shell state; writable shells persist changes.

Worker tests caught two handoff errors: resetting recursive hook depth and
borrowing a runtime that could shut down before accepted work finished. The
kernel now owns a lazy local executor on the reserved kaish stack, carries hook
depth across admission, and stops it with the kernel host. Shutdown also forbids
late startup. Completion notification reads the settled receipt; durable retry
for that notification and interruption before capture remain in the audit.

The scope wrapper enlarged nested rc futures enough to overflow their stack.
Boxing the evaluator fixed the existing context-creation regression without
raising stack limits. Background output tests also caught a lost contract:
jobs expose each completed statement before execution finishes. Raw streams
now follow those observations, including an empty stream when a silent command
has its final result replaced by a hook. Receipts and job wait results carry
the completed hook-processed outcome.

Shutdown tests then exposed a lifetime gap: dropping the worker's LocalSet
discarded running command owners before they could settle. The worker now tracks
accepted tasks, cancels them on shutdown, and drains their settlement before its
runtime exits. A paused result hook must also yield to cancellation. If dropping
a review wait already committed its terminal record, the enclosing command uses
that exact outcome for its job result. Already-cancelled admissions settle without
entering kaish. Panics and abrupt destruction remain separate recovery work.

The process signal path bypassed Drop and exited immediately after a WAL
checkpoint. A subprocess SIGTERM regression proved that accepted shell receipts
were still running on disk. The signal handler now waits for a shared worker
join before checkpointing and exiting. Dropping one join waiter does not lose
the join; repeated and concurrent callers observe the same completion. The
same audit found that block pairs without receipts ignored explicit cancellation;
all command runs now install that token before deciding whether to track a job.

Panic tests exposed a second way to strand an accepted command: Tokio reported
a task panic while its receipt stayed Running, and worker shutdown still returned
success. Shared capture/review now catches unwinding only long enough to settle,
then resumes the original panic. Before capture, the record says side effects
may have occurred; after capture, it preserves the executed result. A separate
test caught losing output when context-state publication panicked after execution.
Another caught a completed statement still queued when the next statement
panicked; the output writer now drains that queue before unwinding. The worker
stops admission on task failure, cancels remaining work, and returns a failed
shutdown result. The kaish thread helper now carries that result through its join.

The next ownership move puts the model loop, turn identity resolution,
conversation sessions, and interrupts in kernel runtime modules. Interactive
and headless callers now use `Arc<Kernel>`; Cap'n Proto error translation stays
in RPC. Existing model-loop and conversation-lock tests move with their owner.
Task placement remains on the caller's LocalSet until admission and shutdown
can move with terminal-event cleanup.

Lifetime regressions then showed that accepted turns disappeared with their
submitting LocalSet, rejected startup left an interrupt entry, and provider
panics left subscribers waiting. Accepted turns now use the kernel worker.
Startup finishes its checks before registering the interrupt; one finalization
path clears state and records yield before publishing the terminal event.
Panics publish failure before the original unwind reaches the worker.

Shutdown cancels queued turns without acquiring their held conversation lock,
and open streams retain their configured cancellation drain. A slow-connection
regression found another uncancellable phase: opening the provider stream.
Connection setup and retry backoff now race cancellation too. Request/resume
drivers, multiple queued turns' liveness, and exact ownership of unfinished
blocks remain separate pieces of the runtime migration.

Headless request and approval-resume logic then moved out of server RPC into
runtime modules. Both now accept `Arc<Kernel>`; approved execution obtains the
registered dispatcher from the broker, preserving the same contextual shell
constructor and no-shell refusal path. Durable cwd reads also moved into
runtime for all RPC and headless callers. The dedicated driver threads still
need startup readiness and joined shutdown; moving source ownership makes those
lifetimes independent of the server registry without claiming they are finished.

Headless requests now enter the kernel worker directly. Counting FlowBus
subscribers had let an observer appear to accept execution; events now report
admitted work, with Requested guaranteed to precede its outcome. Fork/drive,
approval continuation, and async shell completion all use the same admission.
The request thread is deleted. Each queued or running turn owns a lease, so
ending one cannot erase another's liveness or interrupt state. Context interrupts
signal every accepted turn; automatic continuation reserves only idle contexts.

Approval delivery now uses the same worker. Regressions showed shutdown
returning while the old subscription still listened and approved output was
unsettled. Startup subscribes and snapshots old answers before returning, with
errors reaching the host and one owner per kernel. Idle delivery holds a weak
kernel reference. Shutdown cancels shell preparation or running execution and
waits for command settlement; a claimed action is never replayed.

The remaining preparation gap now has an explicit owner: after redemption,
unwinding settles only the claimed pair, or records a no-run error when no pair
exists. Ownership transfers before shared command capture so cleanup cannot
replace captured output. Shutdown also retains the spent approval's delivery
seed before joining, while refusing any follow-up model turn. Regressions pin
both gaps; the original panic still reaches the runtime worker.

Structured kj execution now enters that worker too. Three regressions exposed
its transport-owned lifetime: execution after stopped admission, loss on LocalSet
destruction, and result hooks left unsettled by kernel shutdown. Runtime owns
pending/result channels and carries hook recursion depth across admission; RPC
only resolves identity and translates replies. Shutdown cancels preparation and
pre-call hooks as well as captured result review. Pre-call panics settle unrun
pairs before propagating; accepted review survives a real SSH disconnect.

Interactive submission now has the same lifetime. Real SSH regressions exposed
unsettled work after both disconnect and shutdown; the kernel worker now owns
construction, PreCall and execution. Draft consumption moved with acceptance so
caller teardown cannot leave an accepted command ready to resubmit; revision
checks preserve later typing. Explicit context addressing also removes a split
where the pair used the requested context but construction used the connection's
ambient context. The adapter acknowledges context switches before runtime
publishes completion, with disconnect/shutdown releasing the wait. PreCall
unwinds settle the unrun pair and preserve the original panic.

Streaming RPC completes this execution-owner migration. Its construction,
PreCall, execution and result review now run on the kernel worker. The adapter
reserves its slot before preparation and retains IDs, accepted history, output
subscriptions and connection-local context switches. A dropped reservation
cancels execution and releases the slot; the kernel still owns settlement.
Regressions first showed stopped kernels accepting source and shutdown leaving
result hooks pending. The new owner joins cancellation, including retained
approval review, while preserving captured output. Interactive and streaming
callers now share the acknowledged context-switch channel, and the unused
ambient-context constructor wrapper is deleted. The shared executor is now
`RuntimeWorker`, with `spawn_runtime_task`, `stop_runtime_worker` and
`shutdown_runtime_worker` naming its full scope: commands, model turns and
approval delivery. All callers moved; no command-only compatibility alias remains.

The terminal exposed a result that executed and settled correctly but rendered
blank. Shared settlement clears obsolete structured output; the context-feed
decoder turned its absent Cap'n Proto pointer into `Some(empty)`, which the
renderer preferred over the real stdout. Snapshot and block-callback decoders
made different guesses about presence. All three now read pointer presence:
absent clears output, present empty stays present, and JSON-only output survives.
Three decoding regressions first failed, then passed; the real `:!echo hi`
terminal probe confirms that accepted output becomes visible to the player.

Editor shell reads now belong to the kernel runtime and return complete UTF-8
text or an error before splicing. A pending-tool cancellation test exposed a
second gap: the kaish MCP adapter created a fresh token, so the shell's
watchdog and cancellation could not reach its tool. It now forwards kaish's
execution token. Context construction also reports loadout/cwd read failures
and refuses a directory that no longer resolves. Explicit current-versus-
captured cwd selection preserves approval pins without first consulting a
newer directory; the two post-construction restore paths are removed.
DeepSeek's Kaibo review caught a conflation between no approval pin and a
captured unset cwd. A real auto-allowed shell test failed by landing in HOME;
`GateOutcome` now carries the explicit cwd source too. RPC cwd validation
starts independently of the previous directory, so it can repair a removed
cwd instead of refusing its own remedy.

The identity audit found two adapter losses: MCP dispatch rebuilt the performer
from the requester, and editor opens retained only the requester. Interpreter
constructors now carry `ShellIdentity`; editor reads retain all five fields.
`vi` resolves the current context at open, matching `kj editor open`, and later
navigation leaves that captured context alone. Actual block-author regressions
also exposed requester/default-store authorship in MCP block, rich-content,
and task writes. These now attribute content and edit provenance to the
performer. Lower-level identity constructors are crate-private, with explicit
engine fixtures retained.

Kaibo's follow-up review exposed mutating tool paths in the read-only model
shell. A broker-level regression created blocks, allocated editor sessions,
and changed an existing editor through `:r !cmd`. Read-only `kj` now classifies
resolved argv before dispatch; generic MCP calls, editor tools, and curl are
refused. Existing effect declarations remain authoritative. Kaish's aggregate
backend read-only flag includes its writable temporary overlays, so the
integration carries the contextual policy explicitly instead of inferring it
from that flag. Unknown commands retain exit 127, and help and reads remain
available. This is execution policy within the shared trust boundary.
The read-only check also exposed that synthesis arguments lived outside the
shared command declaration. `kj synth` now parses, classifies, and renders help
from that declaration. Synthesis status/help and the dispatcher's existing
trailing-help normalization remain available without a separate read allow-list.

Approval delivery had conflated context read errors with reassignment or archive.
Fault injection now verifies that the answer and pair stay untouched until a
successful retry. Context validation and the execution claim share one database
lock. A repeated delivery after reassignment had also overwritten completed
output with an error; only the claim winner may now settle a stale performer's
pair. A separate regression exposed duplicate wake seeds after turn admission
failed. Delivery completes when the seed is written, independently of whether
an automatic model turn can start; ordinary answers remain redeemable by their
callers. Tests drive actual delivery with a later answer as a scan marker so an
unfinished configuration read cannot masquerade as a successful check.

The input-capture audit found the same read-error conflation before an ask existed:
missing cwd and unreadable cwd were both `None`, while an environment read fault
invented unset variables. Capture now reads cwd and free variables under one
lock and refuses an unreadable snapshot before creating an ask. Dry runs follow
the same rule. The classifier's plan reader propagates environment faults before
executing its hook body. This fixes storage-fault capture; interpreter defaults
and synthetic export-name collisions remain in the effective-environment audit.

The temporary-name test then lost a legitimate `__kj_env_0__` export when its
restore overlay was popped. Durable exports and approval values now share one
restore helper, choosing temporary names outside the complete target set,
including unset entries. The regression also preserves existing globals that
share temporary names, literal dollar signs/quotes/newlines, and refuses an
invalid name before changing any value. Kaibo review raised duplicate names:
the planner deduplicates, but approval rows only key by sequence number. A second
red/green check now rejects that ambiguous capture before mutation too. This
removes two copies of the restore mechanism without reserving a namespace from
players.

A real SSH approval then recorded HOME as unset even though construction supplied
it. `ContextShellInputs` now owns the selected cwd, durable exports, and external
execution policy for both construction and capture. One defaults provider supplies
HOME/PATH; kaish starts at the selected cwd so its initial PWD matches. Tests compare
actual scope and execution under writable/read-only policies and durable overrides.
The SSH scenario approves in directory A, changes cwd and exports to B, and verifies
execution still uses A and the captured values. The two old database restore APIs
are deleted, with their export and VFS-only-directory coverage moved to contextual
construction. The kernel boundary also exposed four fixtures with separate kernel
and dispatcher databases; those now use one owner, matching production. The
full SSH suite passes, including default capture and the original-directory
scenario. Kernel validation also reproduced the recorded ordering-stress flake;
its evidence remains in docs/issues.md rather than being attributed to this
unrelated change.

The controlled timing scenario began with a failed producer. The engine recorded
its error and discarded the cell, so its required fallback never ran. A failed
source now retains its scheduled commitment deadline; the transport selects the
fallback there and authors any replacement cell separately. That matters for
`UseLastGood`: another producer can supply a better phrase between the failure
and the deadline. Tests cover all three policies, lane isolation, failed
re-speculation, and no repeated settlement. A second red test found due actions
could rewind from tick 10 to tick 9 after late admission; those actions now run
at the current playhead, retaining their original intended start.

The SSH scenario drives the production scheduler with explicit beats and OODA
disarmed. The actual client reads one error in the producer's conversation and
one later fallback in the score, attributed to the transport. Its fixture uses
the context's restored playhead: rc-created history means a real context does
not necessarily begin at tick zero. Comments now state that resolvers still run
synchronously under the timeline lock. Pending work, stale completion rejection,
and bounded admission remain the next part of the scenario; the passing failure
case is evidence for that part of the contract, not the whole performance goal.

Pending preparation now has an owner on that same timeline. A resolver returns
an owned future; beats poll it without waiting, and readiness belongs to the
actual observation tick. A clock jump cannot make late bytes timely by visiting
old deadlines. UUIDs identify work, attempts carry their basis, and cancellation
or replacement drops the old delivery path. The controlled SSH scenario now runs
fast, delayed, failed, superseded, missed-deadline, and stale-basis producers
alongside each other. It reads their status through `kj transport work` and checks
accepted score blocks, including the transport's declared fallback. The status
history is bounded and process-local; it does not pretend to be durable recovery.

CAS preparation moved into a small resolver adapter on Tokio's existing blocking
pool. Four process-wide permits bound running operations, and a cancelled read
retains its permit until it actually finishes. A channel-controlled test pins
that ownership. Review also found that FileStore retrieval did not verify content
against the requested hash; a red test changed the bytes under a valid hash, and
the adapter now refuses the mismatch. Equal-deadline decisions use admission
order, emitted admission failures are reported, invalid clock inputs are rejected,
and cancelled admission cannot make an already-used timeline virgin again.

Review exposed a second, unused timebase in `Timeline::pump`, with ambiguous seed
and tempo semantics. Only its own tests called it; it is removed. The scheduler
supplies ticks, and the engine converts cost estimates into lead time. Older CAS
fixtures also assumed all preparation finished inside one call, so they now wait
for readiness at the current tick before moving their controlled clock. Live model
turns still schedule relative to completion; connecting those turns to admitted
intent remains the next timing task, alongside the unfinished caller migration.

The next caller audit found a cwd read failure still meant “unset” to headless
turns and RPC. The real SSH client confirmed the false answer. Those callers now
share a fallible read with contextual construction, and stored or captured
relative paths are refused. Switching had also changed its context binding before
applying configuration, while failed cwd saves and missing directories only
logged warnings. It now validates target cwd and exports and persists outgoing
cwd before changing live state. Fault tests preserve context, cwd, and exports
across read/write errors, missing directories, and invalid export names.
Reattaching the current context keeps the live cwd it saves; a separate regression
caught that path restoring the older snapshot after write-back.

DeepSeek's review exposed a write/read asymmetry: shell-state persistence could
accept a relative cwd that construction would then refuse. A red test pinned that
path; validation now precedes the cwd/export transaction. Fixtures that switched
from an unmounted default HOME now supply real VFS directories. The live timing
trace also confirmed that turn events have no attempt identifier; the next handoff
must carry admission identity through completion instead of matching by context.

The lease now supplies that attempt identity. A shared UUID `TurnId` replaces
its private numeric generation and follows startup, queuing, inference and every
terminal path. Headless admission returns it, `kj drive` exposes it, and all three
TurnEvents callbacks carry it. The client refuses missing or malformed IDs instead
of making one up. A controlled SSH test holds the conversation lock, admits two
turns, matches their returned IDs to start callbacks, then cancels both and checks
that each original ID settles once without inference. Normal completion, startup
failure and panic coverage follow the same identity. Timing admission still needs
an owned target/basis handoff; an observation event is not its completion owner.

Timing now belongs to admission. Musician rc chooses an absolute future tick;
`kj drive` returns its turn and work IDs, and the lease delivers prepared ABC
through a channel owned by that timeline entry. Completion no longer grants a
new phrase of lead. Expiry cancels only the owning turn. Replacing a registered
resolver cannot change already-admitted work, and a model handoff cannot replay
its producer on a changed basis. Accepted model/tool effects remain durable.

The dependency projection is deliberately small: seed content and eligibility,
plus the latest committed score content before the target. It does not promise a
snapshot of every model input. Runtime owns notation validation and quarantine;
the timeline consumes prepared bytes and chooses fallback. Feedback keeps the
source anchor and only advances its cursor after a successful write. Removing
the completion listener also removed its duplicate scheduling API and tests;
identity, completeness, malformed-output and score behavior now exercise the
admission handoff and actual client.

DeepSeek's review prompted an extra cancellation audit. A deterministic test
found prepared bytes could win over a hard interrupt before the timeline had
observed them. Both handoff observation and runtime preparation now prioritize
that interruption. Two reasoning-enabled review calls exhausted their answer
budget; smaller direct-answer reviews completed, with findings and dispositions
kept in the private execution review notes.

The broader check also reproduced the recorded LocalBackend empty-read race.
A deterministic `/dev/full` test showed its stronger failure mode: the adapter
reported success while Tokio still held bytes whose host write would fail.
Awaiting `flush` now makes the VFS result describe host completion and errors,
without adding an fsync promise. The fix ships separately from timing admission.

A turn's liveness lease did not yet own its opened blocks. Provider panic,
mid-stream error and an unclosed EOF could leave Running text or thinking after
the writer exited. The lease now records exact block IDs across ordinary and
inline tool paths and receipt setup. Terminal cleanup selects Running blocks
under the document lock and commits their Error statuses together, preserving
finished output, waiting approvals and another writer's work. Tool-panic testing
also caught a missing cancellation signal; panic now signals its calls before
publishing failure. The original panic still reaches worker supervision, and
cleanup faults cannot turn into a successful terminal event. Tests cover replay
and the actual SSH callback observing already-settled partial output. Required
stream writes and transfer of Waiting receipt ownership remain in the audit.

Rejecting a model text insert still produced Completed with no output. Required
text/thinking inserts, appends, signatures and completion statuses now return
write errors to the terminal owner, which interrupts, settles and publishes
Failed. Fault injection rejects each write before mutation and distinguishes
optional display summaries from content needed for hydration. Tool persistence
and malformed content framing remain in the same audit.

Ordinary and inline tools now share result creation, shell/ANSI projection and
settlement. A rejected result insert had still allowed execution; Running is
now part of the insertion acceptance, and a missing pair stops dispatch.
Content, styles, error flag and both pair statuses commit together. The prior
status-only update left failed results with is_error=false, changing hydration;
settlement and orphan cleanup now retain the error meaning. A persistence fault
signals cancellation while joining sibling results before terminal failure.
Replay, fault injection, sibling cancellation and an actual SSH read pin these
contracts. The approval-link and receipt ownership transfer remain separate
work; this does not turn an accepted ask into completed execution.

Provider framing is now enforced at the model-turn writer: a delta/end must
match its open text/thinking block, and starts, tools and Done require a content
boundary. Unframed deltas had disappeared while reporting Completed; even a
valid Done kept waiting for EOF. The writer now fails incomplete EOF and stops
at Done, so a complete-looking score without terminal confirmation cannot
commit. Cancellation preserves the accepted prefix and may drain usage under
one absolute deadline. A controlled trickling provider proved that resetting
an idle timeout for each drained event could otherwise extend cancellation
indefinitely. The SSH timing scenario now includes closed text followed by EOF
and verifies the declared fallback at its admitted tick. A separate race test
made cancellation and a provider terminal ready together: the unbiased select
could report a transport error instead of the requested cancellation. The
stream select now gives the hard cancel priority over ready provider output.

Generic block status writes also synchronize a ToolResult's `is_error` in the
same acceptance, including the `completeBlock` client path. Command failures
had published Error while hydration still classified their results as success.
Metadata now precedes terminal status, and replay preserves both fields.
Fault injection, real nonzero shell exits, and an SSH completion test cover the
contract. Receipt registration and approval linkage still need their separate
ownership transfer; consistent error flags do not close that handoff.

Shell operation setup now accepts its command/output pair, receipt and optional
ask link in one kernel.db transaction. Separate writes had left live-looking
pairs after registration failed. The same acceptance path serves interactive
commands, structured kj, background shells and pending model receipts; both
blocks publish with their initial status, authorship and exclusion already set.
Waiting model results also require their ask link in the result transaction,
replacing the best-effort link after publication. Failed writes keep the existing
fail-closed document policy: no events publish, and durable reload is required.

A repeated setup for the same ask returns the existing receipt under the
context's document guard; a different source or identity refuses before
mutation. A regression first reproduced the unique-constraint failure on retry.
Receipt tests now use one database for the registry and journal, matching
production; database-free fixtures cannot prove this transaction. Session
refusal linkage and approval execution/delivery ownership remain separate work.

Refusal delivery now preserves its answer when pair settlement fails. A model
pair's denial or cancellation is consumed with its notification block in the
same journal transaction; a session consumes only after its pair settles.
The ledger's redemption operation can join its caller's transaction, retaining
its redemption-event atomicity. SQLite fault injection proves rollback of both
notification and redemption, and retry creates one notification.

Review exposed a competing gate redemption between delivery's precheck and
commit. A test reproduced a poisoned document after losing that race. The
notification writer now retains the database guard through commit, reuses it
for compaction, and releases it before publishing events. Document-before-
database lock ordering is preserved. Already-claimed approved actions and
startup/backlog policy remain in the ownership audit.

Approval resumes that need a new pair now share atomic setup too. The separate
writer could leave half a pair and gave completed commands no recovery receipt.
The receipt keeps requester and performer distinct, links the original ask,
and retains the captured outcome before output projection. A new pair for a
claimed ask starts Running; merely carrying an ask ID no longer implies
Waiting. The command preserves its model role. Setup failures report that the
approval is spent and nothing ran, without leaving durable partial blocks.
Already-linked pairs without receipts and notification delivery after redemption
remain separate ownership work.

The next ownership handoff belongs at linkage, before a model publishes Waiting.
Kernel ask linkage now adopts an existing pair into the receipt registry or
reuses its receipt, in the caller's transaction. Model content/status acceptance
and execution ownership therefore succeed together. Receipt or link failures
roll back both; context and performer mismatches and link replacement refuse.
Non-executable asks retain only their pair link. Result reviews can change the
current ask without hiding the receipt from its original execution ask; setup
retries reuse that pair too. The unused ask-based completion API is deleted.
Early answers racing the original caller and approved notification recovery
remain open.

Command result publication now uses the same acceptance as model tool results.
Text, structured output, stderr, exit, content type, ephemeral flags, ANSI
originals and spans, both statuses and receipt state commit together. Session
pre-call refusals supply their pair owner there, removing the late ask link.
Captured terminal outcomes still precede projection so a failed acceptance
can recover without executing source again.

A fault test first showed new stdout surviving a failed receipt write. Receipt,
journal and provenance faults now leave the previous durable projection intact
and publish no events. Restart projects the retained outcome; failure to clear
the recovery marker preserves later edits after a successful commit. A snapshot
fault after commit also recovers the accepted result and receipt. The real
pre-call gate also proves that a link failure cannot publish Waiting. Model
fixtures now use the kernel's journal database rather than unrelated stores.
Rc diagnostic provenance remains best effort and belongs to its lifecycle audit.

The approval scan also cached linkage before claiming an answer. A session
could publish its pair between those operations; the driver then spent the
answer and tried to adopt that session pair as its own Turn pair. A late model
link could miss the performer-change check entirely. Admission now reads
linkage, context state and redemption under one database guard. The regression
fails against both stale-scan assumptions, and fixtures now record real links.
This closes the scan-to-claim window. The earlier gap still needs explicit
caller-to-driver ownership transfer: absence of a link cannot distinguish
pending publication from a caller that will never author a pair.

The gate now records that distinction with the ask. Model results, interactive
pre-call hooks, authored structured commands and asynchronous shell operations
promise a pair; quiet, streaming and direct foreground calls do not. The ledger
creation API accepts related caller state in its transaction without changing
its commit-before-return contract. No invocation pool or context-wide lock is
needed.

The first regression reproduced early driver execution, and the second showed
a matching gate retry taking the same answer. Driver admission now waits for
the caller's release; executable paired answers remain with their operation.
Non-executable answers can still return to retry after release. A third test
showed why linkage alone was insufficient: a caller could link while publishing
a terminal error. Release now commits only with a Waiting result. Link or release
faults roll back together, and publication wakes an already-deferred answer.
Terminal results now retire their unreleased invocation in the same transaction.
A pending ask becomes Abandoned; a terminal reviewer answer remains unchanged
but is spent without executing source. Publication abandonment has its own
reason in `kj ledger show`, so an Allowed decision never implies execution.

Startup first recovers retained results, then retires unpublished invocations,
and only then settles other interrupted operations. This ordering
keeps generic cleanup from taking a receipt that still has a more specific
settlement. Tests reproduce pending holds surviving restart and answered holds
remaining unspent after terminal failure; fault injection checks retirement and
result rollback together. Receiptless original-pair recovery, abrupt live failure
without a result, and claimed notification delivery remain open.

The typed client now preserves the publication abandonment reason. The TUI's
full ask detail and the app's recent ledger display it with the unchanged
decision; terminal TUI details offer no decision keys. The app only opens
pending asks, so a sheet-only change would have hidden the disposition. An
isolated startup/SSH test uses the real ActorHandle and show_ask_detail path to
prove allowed and denied unpublished invocations return their decision and
retirement reason, with no source side effect. Review caught a misleading
"Approved source" phrase for denied asks; the reason now says "Source did not run." Rendering tests pin the explanation in both
views. No running GUI was rebuilt or deployed.

The unfinished-operation sweep previously completed only receipts. A restart
regression left both original blocks Running. Startup now commits the original
pair and receipt through the shared journal acceptance, preserving stored text,
structured output, ANSI spans and original bytes, and each block's hydration
policy. It appends an interruption reason to stderr without inventing an exit
code. Source and hooks are never replayed. Known outcomes and retained reviews
keep their more specific recovery paths; a corrupt pair or failed acceptance
refuses startup before admitting writers.

Receipt and journal fault tests reload durable state and prove no partial
settlement or events. Repeated startup preserves later edits and the original
receipt. The isolated SSH test checks typed block reads and repeated
`kj wait --operation` polls against the same interrupted operation. DeepSeek's
review found no actionable defect; fail-fast recovery is intentional, and its
partial-move compile concern was incorrect. Old receipt-only abandonment rows
remain recorded in the ownership audit.

Receiptless writers used a separate sweep that overwrote stderr and committed
status before its explanation. The regression reproduced that lost stderr.
The sweep now selects open blocks under the document guard and commits every
status, appended tool stderr and one Error child together. Output, structured
data and ANSI provenance survive. Startup and both fork paths propagate failure;
startup also refuses a failed approval retirement instead of serving stale asks.
The error names interrupted writers, so copied fork history no longer claims
a kernel restart. Adjacent comments now state the recovery rules directly.

Journal and post-commit compaction faults prove the common commit point and
idempotent recovery. A server boot test rejects approval and journal failures,
then recovers with one explanation. The SSH client checks receiptless output
beside registered shell receipts. Fork refusal leaves its parent untouched;
fork initialization across the earlier document copy remains a separate issue.
DeepSeek found no actionable defect. Its reachability questions were checked:
acceptance delegates to the journal transaction, persistent stores refuse a
missing database, and the fork child has no context registration at this step.

Completion delivery had a different owner gap: an allowed ask was already spent
when its in-memory notification text reached the writer. A failed write removed
it from the ledger's retry scan. Async shell delivery had no marker at all; the
first regression appended two notices when delivery was retried.

Execution notifications now reserve ownership with the claim or async admission.
The shared record stores the source key, prepared message, delivered block and
suppression reason; identity stays on the existing approval or receipt. Shell
settlement prepares its message with the terminal receipt. Approval delivery
retains a live copy if message persistence fails. Notice insertion and its marker
commit in one journal acceptance. Startup recovers pending messages after result
recovery, disables old provider wakes, and never replays source. Sessions retain
an explicit disposition that their caller reads the command pair directly.

The existing worker scans each second as well as on ledger events, processing
up to four deliveries per scan. Larger backlogs now drain without another answer.
`kj wait --operation` reports notification states; `kj ledger show` distinguishes
completion delivery from the reviewer's decision. Reassignment and retirement
retain suppression reasons. Tests cover reservation/claim rollback, message and
publication retry, missing-pair restart, journal/marker/compaction faults, preserved
full output, and restart without provider wake. The SSH client reads the recovered
notice and its disposition. Durable delivery and provider admission remain
separate: the general continuation-versus-reassignment race stays in issues.

DeepSeek reviewed design and implementation. We rejected moving claim and final
notice into one transaction across execution: a spent claim must never be restored
to retry delivery. The implementation review's suggested startup race is absent
before worker admission; terminal/suppressed summaries are intentional audit data,
and corrupt ownership refuses startup. Its reassignment concern prompted a check
of continuation admission, where the existing epoch does not bind the performer.

Performer reassignment now invalidates earlier continuation epochs atomically
with the assignment. Queued startup checks its claim; every inference attempt
checks invalidation before provider entry. A regression proved that closing only
the window was insufficient after a claim: the provider loop ignored a failed
request stamp. A second regression rejected treating every failed stamp as a
refusal: explicit signoff and newer drives must leave already accepted work able
to finish. The continuation row now retains an invalidation watermark across
new epochs. Request stamping remains separate from inference admission, so old
accepted turns cannot extend a closed or newer window. Explicit preparation
also rechecks performer assignment before opening an epoch.

Tests cover assignment rollback, reassignment back to the original performer,
legacy-schema migration, queued startup, the wait after claiming, and signoff
and newer-drive compatibility. A real SSH client changes the performer while
inference waits, then observes failure without provider output. An inference
admitted before reassignment may finish with its captured identity; subsequent
requests are refused. DeepSeek's first review suggested a claim race inside one
transaction; the database guard and transaction exclude it. Its policy question
prompted the explicit compatibility regression and revised admission check.

The caller-lifetime audit found an await between async receipt creation and
worker admission: waiting for broker policy could lose the caller and strand
the receipt. Policy now resolves before admission. A regression drops that wait
and requires no operation, blocks, or events. A second drops the caller after
admission but before job readiness; the retained worker cancels and settles the
original pair without executing source.

Projection failure also changed the job's result after retaining the original
command outcome. A fault regression showed exit 1 in the job versus exit 0 in
the retained outcome. Jobs now keep the captured hook-processed result; the
publication error is returned separately. Receipt-write and post-commit
compaction faults preserve job streams and agree with recovery. `kj wait` help
and the identity contract distinguish a finished job from an operation awaiting
durable publication. The SSH test observes both from a healthy context: the
failed target document refuses further acceptance until restart. DeepSeek found
no concrete defect; its remaining questions were checked against read-only
policy lookup, retained worker ownership, and hook-processed result conversion.
Initial outcome-retention failures remain unfinished work.

The first terminal-outcome write now has a live retry owner. The receipt
registry retains its immutable serialized result on a SQLite failure; domain
conflicts cannot replace it. Final receipt-free result reviews use the same
mechanism. The command runner keeps its admitted receipt through execution, so
post-execution lookup failure cannot discard capture. The existing worker retries
up to four oldest attempts per scan, rotating even after lookup failure. Source
and hooks do not run again. Once SQLite accepts the result, durable projection
recovery owns it. A poisoned document refuses settlement with a restart-required
error rather than panicking the retry worker.

Shutdown joins execution, attempts retention again, and reports any results still
held only in memory. A later shutdown call can finish after storage recovers.
Operation waits and result-review ledger reads expose retention errors. The SSH
client sees the failed write, then completed output after the fault is removed.
Tests cover source execution once, immutable conflicting retries, receipt-free
review completion, shutdown refusal and retry, fair scans, and poisoned-document
recovery. Live copies remain volatile until SQLite accepts them; review checkpoint
faults before terminal retention and prolonged-fault admission pressure remain
in the inventory. DeepSeek review questioned missing or contradictory receipt
states; retaining the live owner and refusing clean shutdown is intentional,
and no receipt-deletion path was found in the kernel.

Result-review admission now commits the ask, captured execution, optional
receipt update, and invocation link in one transaction before notification.
The prior handoff overstated checkpoint failure: ordinary failures already
became terminal hook refusals. The concrete gap was an announced ask without
its captured result. Injected checkpoint/link failures reproduced an orphaned
approval row. Admission now rolls all of those writes back, while terminal
settlement preserves tracked execution. The wait only publishes Waiting state
and consumes the answer; it does not create a second checkpoint.

The gate uses the ledger's existing transaction callback. The callback receives
the connection and never reacquires the database guard. Caller error types are
preserved across that transaction. DeepSeek found no concrete admission
regression; its questions about sequential reviews and automatic decisions were
checked against the retained per-invocation owner and exact-statement policy.
Validation passed 3,192 kernel tests, 170 ledger tests, and 33 SSH gate/publication
tests; workspace all-targets also passed. The SSH failure regression reads from an unaffected reviewer context and
checks refusal, absent ask, preserved raw execution, and one source execution.
Cancellation/panic recovery read failures, abandonment failure, and prolonged
storage-fault admission pressure remain in the inventory.

Interrupted commands now keep one outcome before attempting storage. A dropped
review could retain its refusal after a failed write, while outer cancellation
read SQLite, found no result, and constructed a different refusal without the
ask ID. The immutable retry owner correctly rejected that second result. Read
failure in the same recovery lookup could instead bypass command settlement
and job completion altogether.

The command owner now carries its admitted receipt and caches its first
interrupted outcome. Drop, cancellation and panic recovery reuse that exact
result without database reads. Normal settlement uses the same owner; captured
attempts no longer return a storage-read error before settlement. Removed the
receipt-free terminal lookup API that existed only for this recovery path.
Fault injection covers failed writes and refused reads with and without a
transcript pair, plus cancellation/panic before an ask. Jobs complete while
retention reports its storage fault, and retry settles once without source replay.
Failed ask abandonment remains a separate disposition audit. Validation passed
3,194 kernel tests (6 ignored) and 32 SSH gate tests, including shutdown refusal
and retry under interrupted-review retention faults. DeepSeek found no concrete
regression in the cached receipt, interruption result, or lock ordering.
Workspace all-targets checking also passed.

Terminal review outcomes now commit with closure of their unanswered asks. A
failed abandonment could leave a pending ask beside an already settled result;
the regression reproduced that state. Retention closes only pending/claimed
asks linked to the invocation, preserving reviewer decisions and unrelated
execution asks. Ask-update or audit-event failure rolls back both the result
and closure. Nested ledger SQLite errors keep the same immutable retry owner;
successful retry publishes a ledger notification without repeating a transition.

Removed independent abandonment from review waits and restart recovery. The
command owner already handles cancellation, so the wait's duplicate cancellation
branch was removed too. Cancellation coverage now exercises the actual MCP
caller; storage regressions cover pending/claimed asks, tracked and receipt-free
results, event rollback, shutdown refusal/retry and preserved decisions. The
ledger sweep comment now describes its existing transaction-joining behavior.
Validation passed 3,195 kernel tests (6 ignored) and all 32 SSH gate tests.
DeepSeek raised a decision-race concern excluded by the held DB guard and SQLite
write transaction, and an abandonment-loop concern contradicted by the wait's
terminal return. The review disposition records both checks. Workspace
all-targets checking also passed.

Admission receipts now stay with interactive, structured, tool and approval
execution through preparation, refusal, capture and settlement. The regression
first left an admitted interactive pair Running when a pre-call hook refused
receipt-source reads. Passing the receipt removes that lookup and the runner's
optional unregistered-pair branch. Result review derives its pair from the same
receipt; recovery alone discovers an owner when no live invocation remains.
Linked approvals resolve their receipt before claiming the answer, under the
same DB guard as context validation and redemption. Read failure leaves the
answer available for retry. Execution setup faults retain NotRun, finalize
job streams and deliver the job result without entering the interpreter.
Validation passed 3,197 kernel tests (6 ignored), 42 real SSH/RPC tests, and
workspace all-targets checking. DeepSeek's follow-up concerns were checked
against kaish's job retention, handoff retirement and immutable terminal retry
ownership; none established an additional defect. The private review archive
records that disposition. Abrupt worker destruction and the full caller
inventory remain open.

The worker lifetime audit reproduced a second panic boundary: evaluating a
submitted factory before `spawn_local` unwound the supervisor and stranded an
admitted sibling command. Factory evaluation now occurs inside the task, so
Tokio reports the panic through the existing JoinSet failure path. The worker
stops admission, cancels siblings, drains accepted work and reports the failure
from shutdown. No new executor or recovery mechanism was needed.

Regression coverage checks actual interactive commands paused before and after
capture: the first settles NotRun, the second keeps captured execution, both
pairs settle, and jobs complete with closed streams. A separate deterministic
shutdown test queues a failing factory and a later non-Send future, then checks
that construction stays on the worker thread and queued cleanup still finishes.
Validation passed 3,199 kernel tests (6 ignored), 51 SSH/RPC tests and workspace
all-targets checking. DeepSeek found no regression. Its claimed infinite
retention retry was contradicted by the finite one-pass snapshot; admission
pressure during prolonged faults remains tracked separately.

Context-level `kj wait` now uses the same completion condition on both paths:
evidence that a turn ran and no accepted turn still in flight. A regression
showed its event path returning on the first of overlapping turns despite the
polling path's aggregate liveness check. The reader retains the latest observed
terminal detail while waiting for idle. It does not infer every turn's success
from a lossy bus. Event-path block reads now fail explicitly instead of silently
returning an older snapshot.

A second regression reproduced a terminated subscription busy-looping until
timeout, preventing the runtime timer that released the turn lease from
running. Closed subscriptions now use the same paced state polling. Rewrote
adjacent comments around the actual liveness contract and clarified that job
waits expose process-local status/exit code while operation IDs are durable.
Context-removal cancellation without joining/fencing and durable per-turn
outcome recovery remain recorded work. Validation passed 3,201 kernel tests
(6 ignored), 12 SSH/RPC tests, and workspace all-targets checking. The new wire
test verifies overlapping turns and emits `kj wait --help`; its cursor and
timeout descriptions now avoid promising unbounded history or lossless events.
DeepSeek review added no confirmed regression. Its fairness and missing-event
concerns are recorded; its claim that retained terminal detail was discarded
contradicted the implementation.

Amy changed the context lifetime policy while we audited cancellation on
removal: "`kj context remove` probably shouldn't exist in the system" and
"archive should be good enough for everyone." Retired `remove` and `rm`, the
metadata-only deletion API, and its unused cancellation helper. Archive keeps
blocks, lineage, receipts, and approval history; promote restores the context.
Document deletion now refuses registered contexts in a database transaction,
including archived contexts, so `kj doc delete` and the virtual document
filesystem cannot bypass retention. Unregistered documents remain deletable.

Well-known context publication now commits its context row and role assignment
together. Injecting a role-write failure reproduced a partial context; rollback
now leaves only an unregistered document that creation can discard. Tests also
failed first on context removal and document deletion, then passed with the
retention policy. Actual SSH/RPC clients verify archive/restore and a command
finishing after archive with its captured output and durable receipt retained.

Archive admission still needs a consistent contract across execution paths.
Index eligibility is separate: `kj search --all` skips archived contexts, while
the semantic watcher sees terminal blocks without context-state filtering.
Recorded Amy's suggested don't-index policy, including existing vectors and
in-flight publication, rather than claiming archive provides that guarantee.

Validation: 3,201 kernel tests passed (6 ignored), 12 SSH/RPC tests passed,
and workspace all-targets checking passed. A final client assertion confirms
standalone File documents remain deletable. The retired drift unregister API
is now only a private recovery-test helper; 122 drift tests passed afterward.
The offline probe generator produced 193 live verbs and 16 extras with no
context-deletion command. Removed stale deletion probes; the older checked-in
corpus still needs a broader refresh. Kaibo/DeepSeek Flash found no confirmed
defect (25,646 input / 637 output tokens). Review and disposition are archived
in `~/exomemory/kaijutsu/reviews/2026-09-18-execution/archive-*`.

## The kernel with no one to answer to (September 16)

Amy wiped her local kernel and started it fresh, and it deadlocked quietly. It
seeded `hajime`, the shipped `approval.toml` named `amy` as the default
reviewer, and ROOT was a `director` context with no performer. Every `kj`
call warned that `amy` had no sheet, and nobody could assign ROOT a performer,
because that check resolves the reviewer first. The guide rc `hajime` was
supposed to carry had never been written.

Each piece had been reasonable alone. Together they asked a new kernel to
know a person before any person had arrived. Amy's question went to the
shape: *"maybe before the first connection the user creates themself and the
rest is more mechanical?"* Then, in short order: drop `default_reviewer`;
ROOT is *"a root for attaching bantos to, and a model-less place I can type kj
admin commands"*; *"there should be no anonymous at all!"*; adding keys stays
host-only; and *"equal roots. 1 will be typical, more than one just needs to
be possible for now."*

`kaijutsu-server init --as <name> --key <pubkey-file>` now creates the root
character and binds its key before the first start, and a kernel with no live
root refuses to start. Each root character gets a root context of the new
model-less `root` type, labeled with its name. The ephemeral test config runs
the same `init`, so tests stopped relying on the anonymous path production
never had. The lesson is the one the approval work keeps teaching: a fallback
that names a specific person is configuration pretending to be a relation. The
default reviewer comes out next, and the context tree answers who reviews.

The same day, the parentless-context question turned into a smaller wire.
Amy first wanted `--parent` required, *"explicit, discoverable, and
discourages making piles of unrooted contexts"*, then asked whether
`createContext` was a mistake. It was: `createContext @26` is retired, and a
client runs `kj context create` through `executeKj` from the context that
becomes the parent, so a parentless create cannot happen. Fork was the other
candidate and the wrong one, since it copies history, type, and performer. A
client with no context yet names `--parent` or takes the kernel's only root
context; with several roots it refuses. Removing the RPC exposed that the `kj`
path recorded a characterless caller as director, which the documented rule
never allowed. A transport error and a kernel refusal stay distinct types, so
the MCP's label-conflict retry cannot mistake a dropped connection for a race.

The default reviewer came out on September 17. Resolution is now override,
delegation, then the walk. When the walk runs out, only a live root character
confirms its own statement; anyone else has no reviewer, and the ask refuses by
name instead of quietly naming its own actor. The authority the default used to
hold went to the **lineage root**, the root character at the top of a context's
`forked_from` chain. It changes routing and reclaims asks in its lineage, while
any live root grants and revokes delegation. Test fixtures had leaned on the
default: about thirty raised asks from parentless contexts with nobody above,
which a real kernel can no longer build, so they now hang under a root context
like production work. `kj context prompt` stopped resolving a reviewer for a
context with no performer, since the runtime fact names who reviews that
performer's asks.

Rotation became a verb the same day. The plan said `kj context rotate
<character>` would read the character's `root_ctx`, but only roots had one,
and a model's "home seat" pointer would have needed either a new flag or a
guess. Amy cut the knot: rotate a context, and move a character's pointer
only when it named that context. The successor copies everything, env
included, and runs the create lifecycle before it takes over. That lifecycle
cannot share a transaction with the takeover, so the takeover waits for a
usable loadout, and a failed successor is left unlabeled beside a predecessor
that is still live. Writing the test showed that a root context could not pass
the model-performer self-review check. A root plays its root context and
confirms its own statements there, so that check now skips roots. The session
scenario had been booting amy as an ordinary character beside an unrelated
ephemeral root; it now boots her as the root, and her rotation of banto's seat
runs through the verb.

The last slice shrank once rotation existed. The plan was for a refused
`create --as` to raise an ask, which mattered while self-rotation went
through `create`. With a creator already directing what it creates, and a
director allowed to cast its children with `set --as`, an ask on create would
have put more friction on a create than on the create-then-set path it equals.
Amy chose parity instead: `create --as` needs Operator and a live character
caller, like `set --as`.

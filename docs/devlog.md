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

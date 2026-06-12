# Project Chameleon — models playing to the beat

> Status: design jam 2026-06-10; batch 1 selected and vocabulary locked
> 2026-06-11 (tracks not voices, phrases not bars in the kernel, silence
> until first good). Batch 1 substrate corrections 1-3 (track on Cell,
> notation-first commits, phrase vocabulary) have **shipped**. Batch 2 began
> 2026-06-11: the **transport report** shipped — `$TICK`/`$PHRASE`/`$TEMPO`
> heartbeat scalars + a `$HEARD` JSON window of recent committed notation,
> seeded into the `tick` lifecycle; `kj drive --prompt` now writes the report
> as a durable, hydrating block. `$HEARD` ships as a pragmatic JSON-string push
> (the array form + read-on-demand pull are follow-ups on the arrays/hashes
> plan). The quantized mailbox flush and the cost-guard hydration marker are
> still deferred (see mechanism 4-6 below). Companion doc: `hyoushigi.md` (the
> timing substrate this builds on).

Chameleon teaches models to play music to a beat, starting with a small local
model playing the bass line of Herbie Hancock's *Chameleon* (the Head Hunters
vamp: B♭ Dorian, B♭m7–E♭7) and working backwards toward a band. The tune is
chosen deliberately: it is a two-chord vamp, so the degraded mode — repeating
the last bar — is musically indistinguishable from the job. The system sounds
correct even when a player contributes nothing; every successful turn is pure
upside.

## The band

| Chair | Player | Why |
|---|---|---|
| Bass | small local model ("bass-gemma", Strix Halo via lemonade) | lives in the building, plays 24×7, electricity not tokens; the pocket needs presence, not genius |
| Drums | Haiku | tightest k in the band — shortest turns, beat-level quantization; a speed profile, not a depth profile |
| Keys | Sonnet | comping = reactive harmonic judgment at bar cadence; smart enough to voice-lead, cheap enough to play all gig |
| Booth (producer) | Opus | the only entity with ears; wide-parameter-space one-shots; one considered sentence per 32 bars |
| Vocals | Fable | synthesis + restraint; entrances quantized to phrases; vocoder per *Sunlight* (1978) — repertoire, not gimmick |

Members are **contexts**, not a new agent type (agent-emerges-not-noun). The
model is per-context config; tiering costs zero new surface. Nobody in the
band owns time — the transport (拍子木) does.

## Decisions

- **Tracks, not voices (2026-06-11).** The stable lane identity on the
  timeline is a *track* (DAW sense): the track persists while players come
  and go; the scheduling principal separately records who played, so a
  substitute player inherits the lane (`UseLastGood`, `$HEARD`) intact.
  "Voice" stays reserved for ABC's `V:` field — one track may project to
  several ABC voices (keys on two staves). "Lane" stays reserved for
  automation inside a track.
- **Phrases, not bars, in the kernel (2026-06-11).** The timebase speaks
  `beats_per_phrase`; the kernel chunks musical time in phrases only. Bars
  are a notation/human affordance — ABC barlines, UI labels, charts,
  conversation — and those layers keep their bar vocabulary by translating
  at the edge (a chart says "4-bar phrase in 4/4" and hands the kernel 16
  beats). This sidesteps mixed-meter complexity: a phrase is N beats however
  notation bars it. Field shape stays open to per-phrase beat counts
  (irregular phrases) later.
- **Hearing is symbolic.** Players exchange ABC + data, never audio. Leaning
  shared score: one timeline, players as tracks, `$HEARD` shows sibling
  tracks' recent phrases. Consequence: groove/feel must be *explicit data*
  (micro-timing offsets as an automation lane) — symbolic players are
  quantized by construction, so laying back is a parameter, not an accident.
- **Repeat-on-dropped-phrases = the required Cell `fallback`.** Do not build
  a second repeat mechanism; `UseLastGood` per track is the vamp insurance.
  An empty track resolves to `Skip` — silence until the first good phrase
  (decided 2026-06-11; no chart seeding, no arm-gating).
- **Notation is the score; MIDI is a render of it.** The committed track
  content is `text/vnd.abc`; derived MIDI is a derived sibling *block*,
  paired at the write barrier via `parent_id` provenance (as built, batch 1
  fork 2). The substance is unchanged from the original decision — notation
  is the score, MIDI a render — only the mechanism moved off the timeline:
  MIDI is never a committed cell, so the `UseLastGood` pool stays
  notation-pure by construction.
- **Event taxonomy by delivery semantics, not source:**
  - *heartbeat* → kaish vars, overwrite-in-place (`$TICK`, `$PHRASE`,
    `$TEMPO`)
  - *stream* → arrays (ring buffer `$HEARD` of recent phrases, all tracks)
  - *cue* → traps on musical signals (`trap '…' PHRASE%4`) — cron in musical
    time; quantized system messages and quantized obligations are the same
    feature; bar-speak in human-authored cues translates to phrases at the
    edge
- **Turns are launch-quantized** (Ableton-style): `drive` requests snap to the
  grid — beat or phrase per context. Coding contexts quantize to ∞
  (unchanged); a player quantizes to its chair's cadence.
- **The model plays ahead, not the beat it hears.** Turn latency makes the
  loop anticipatory by construction. Measure each player's reach k ("averages
  2.3 phrases per turn") and put it in the transport report so the player
  knows not to schedule inside its own latency.
- **Knobs are cells.** Parameter automation = cells with an automation MIME on
  the *same* timeline (no second grid). A **patch sheet** (curated, named,
  musically-described knob subset) lives in the system slot; artistic-range
  growth = the capability allow-set pattern (widen the loadout over time).
  CLAP hosting (clack) when plugins land — the clappers clap the CLAPs.
- **Producer loop = drift on a slower clock**, two distinct output channels:
  quantized live cues (traps, phrase boundaries — notes from the booth) and
  durable chart/patch-sheet revisions at hydrate boundaries (the
  self-introspection-kernel pattern). Evals: deterministic rulers first
  (onset-vs-grid, ABC parse-failure rate, fallback-fire rate), audio ML taste
  models later. **Feedback must arrive in the receiver's control vocabulary**,
  heavily attenuated — one cue per phrase, never the raw eval firehose.
- **Big models author vocabularies; small models speak them.** "Dump this VST
  into the chain" is an Opus one-shot whose output is a patch-sheet revision
  drifted to the player. ABC-only output (no tool calls) is ideal
  small-local-model UX — the symbolic decisions accidentally made the player
  role exactly the shape small models are good at.

## Timeline gap analysis (vs. hyoushigi as landed)

The temporal substrate is ready; the timeline is a **solo instrument**. One
concept — *track* — collapses most of the gap:

1. **`Cell` has no track** (`{span, body, state}`). Smoking gun:
   `Fallback::UseLastGood` is documented as "this track's last content" but
   no track exists — with two players the bass fallback could repeat the
   drums' last phrase. Materialization hardcodes `PrincipalId::beat()`.
2. **Score commits the wrong symbol** — `schedule_abc_cell` commits the MIDI;
   the ABC lives only in CAS params.
3. **One timeline per context, single scheduler-writer** — shared score wants
   band-as-context owning the score, players scheduling cross-context.
   Mechanically near-free (kernel is sole sequencer); no API/ownership story.
4. **No windowed read** — `committed()` is the full slice; `ContextQuery` is
   `{lookback, ambient_keys}`, track-blind. `$HEARD` can be a block-log
   tick-window query (blocks already sync) once track identity lands.

## Foundational changes, sequenced

**Substrate corrections (small, contained, first):**

1. **Track on `Cell`** — ✓ shipped (batch 1, fork 1). Threaded through
   materialization: track identity replaces hardcoded `beat()` (scheduling
   principal recorded separately as `played_by`); `UseLastGood` per track,
   empty track → `Skip`; per-track seq lane for `BlockId`. First because
   later APIs would bake in the no-track assumption.
2. **Notation-first commits** — ✓ shipped (batch 1, fork 2). Committed cell
   = ABC, MIDI = derived sibling block paired at the write barrier
   (`parent_id` provenance).
3. **Phrase vocabulary** — ✓ shipped (batch 1, fork 2). `beats_per_phrase`
   on the timebase/policy so phrase boundaries / `PHRASE%4` cues are
   expressible; bar affordances in other layers translate to phrases at the
   edge.

**New mechanisms:**

4. **Quantized flush + transport report** — turn preamble = transport report
   (now-tick, tempo, phrases elapsed, window contents, measured reach k); the
   seam keeps hyoushigi ignorant of conversations and the mailbox ignorant of
   tempo, with the beat scheduler as the third party that assembles the facts.
   **Partially landed (2026-06-11):** the *now-facts* half — playhead tick,
   phrases elapsed, tempo — ships as the `$TICK`/`$PHRASE`/`$TEMPO` heartbeat
   scalars (`beat.rs::transport_vars`), seeded into the `tick` rc lifecycle via
   `run_rc_lifecycle_with_vars` and composed into the drive prompt by
   `S10-drive.kai`. `kj drive --prompt` now **writes the report as a real
   User block** (it was silently dropped before — the turn driver reads the
   seed from the log, not `TurnFlow.content`), so the report hydrates as the
   fresh user turn. *Window contents* → `$HEARD` (see 5) also landed. **Still
   open:** (a) the *quantized mailbox flush* — async inbound events (sibling
   messages, shell output) emitting their digest on the grid crossing rather
   than the next turn; not load-bearing for a solo player, wanted at band time.
   (b) *measured reach k* — stubbed/omitted; turns still schedule a fixed one
   phrase ahead.
5. **kaish heartbeat vars** — the scalar now-facts (`$TICK`/`$PHRASE`/`$TEMPO`)
   **landed** with mechanism 4, and so did **`$HEARD`** — as a **pragmatic
   JSON-string push**: `beat.rs::heard_json` reads the committed notation in the
   last `HEARD_WINDOW_PHRASES` (block-log tick-window, `ContentType::Abc` only,
   all tracks, oldest→newest) and seeds it as a JSON array string the model
   reads natively in the prompt. This matters **even solo**: materialized score
   blocks are `ephemeral` (hydration-silent), so `$HEARD` is the *only* channel
   that shows a player its own prior phrases. Two follow-ups when the
   arrays/hashes plan lands (Chameleon is its first consumer): expose `$HEARD`
   as a real kaish **array of hashes** (indexable, `for phrase in $HEARD`), and
   re-shape **push → pull** (a `kj`-reachable read so the script chooses
   depth/track rather than a fixed injected window — shares the windowed read
   with the RC hydration-marker archive verb). TODOs on the code. Traps later.

**Cost guard:**

6. **Hydration windowing / the stamp-turn model** — see open question below.
   Decide before slice one runs at tempo: append-only conversation at 120 BPM
   teaches wrong cost lessons in the first hour.

**Decide now, build later:** band-as-context score ownership; `$HEARD` as
block-log window query; eval ruler counters (cheap, answer bass-gemma
viability for free).

Slice one = bass-gemma vamping in B♭ Dorian: needs exactly 1–6, nothing from
the deferred list.

## Open question: the stamp-turn process model

The near-stateless player turn is template + data → output: a stable prefix
(stance + chart + patch sheet — system slot, prompt-cache-aligned so the
Anthropic-style snapshot is cached and cheap) executed over and over with a
fresh transport-report suffix each time. **No real conversation.** At beat
rate the prefix cache never goes cold and per-turn cost ≈ window + output;
local players don't care at all.

Fork is mechanically right for this (fork = spawn from the prepared base) but
the current model doesn't capture **process cleanup**: there is no exit/reap.
Endlessly created-and-archived per-turn children make parts of the DAG very
dense (thousands of rows/day at tempo).

**Leading candidate — the hydration marker (Amy, 2026-06-10):** a context
keeps endless rows, but turns hydrate only `[0, marker] ∪ [now − window, now]`
— a pinned prefix plus a sliding tail. Blocks between marker and window are
durable record, never hydrated (equivalently: new blocks are born excluded
past the marker). This makes the generation log *be* the block log — no new
document kind, no fork-per-turn, no reaping, no DAG density. The prefix is
byte-stable so the Anthropic prompt cache aligns perfectly; the tail is the
fresh transport report. It is the conversation-side twin of hyoushigi's write
barrier: behind the marker, the immutable cached *program*; ahead of it, the
open *record*. **Moving the marker = committing a durable revision** (a
producer chart update writes new program blocks and advances the marker past
them) — which is also exactly where the existing exclude/edit-at-boundary
semantics already live. Mechanically: a per-context hydration policy consulted
by `ConversationMailbox` (default = today's behavior, marker = ∞); exclusion
machinery is the precedent. Convergence worth noting: pinned-prefix +
sliding-window is precisely how attention-sink / StreamingLLM inference
manages context at the attention level — same shape, one layer up.

**RC-drives-the-marker (Amy, 2026-06-11) — keep policy out of Rust.** Don't
hardcode "how much to keep" in the kernel. The Rust side is a *minimal
mechanism*: the hydration policy on `ConversationMailbox` above (keep
`[0, marker] ∪ [now − window, now]`, default marker = ∞) plus a `kj` verb that
advances the marker / archives the blocks between it and the window. The
*policy and trigger* live in **rc**: an **on-turn rc hook** (a `tick`-time or a
new `turn`/`drive` lifecycle script) that, in composer mode, does roughly "mark
the last N blocks archived, then reset the context's hydration to checkpoint +
new — so only fresh blocks past the marker hydrate next turn." This is the
self-introspection-kernel pattern: the model's own lifecycle scripts manage how
much of its past it carries, the same way they already manage cache breakpoints
and system-prompt slots. Marker-advance reuses the existing exclude/edit-at-
boundary rule (a queued exclusion takes effect at the boundary), so there is no
separate marker machinery to invent — the rc hook is just the policy author.
This is the **cost guard** for the per-phrase report blocks that mechanism 4 now
writes: without it, `kj drive --prompt` appends one durable User block per
phrase forever (fine while hand-testing below tempo; a leak once a set runs at
120 BPM). Build it before slice one runs sustained at tempo. Open: where the
on-turn hook lives (reuse `tick`, or a dedicated post-turn verb so the marker
advances *after* the model reads, not before), and the windowed archive `kj`
surface (shares the block-log windowed-read primitive `$HEARD`'s pull wants).

**Rotation (Amy, 2026-06-10) — the growth answer:** cap context length and
cycle via **shallow fork** — "fork without history": copy the program
(behind the marker) + the tail window, nothing else; the old context becomes
a closed cold segment; the player's label moves to the new head. Log
rotation, literally. A day of playing = a short chain of segments in the
DAG (one fork edge per rotation — the time-well's fork-lineage grammar draws
it natively); the app attaches only to the live head, so dense history never
reaches the renderer. This is also the second concrete voice for the
fork-selectivity revisit (`kj fork` is a full copy today).

Rotation invariants:

- **Invisible to the player** — the test for a correct rotation: program
  bytes identical (prompt cache survives — Anthropic caching is
  content-keyed), tail window carried, `$HEARD` unbroken. The player cannot
  tell a rotation happened.
- **Tick continuity** — the new segment's timeline/playhead seeds from the
  old segment's committed max; musical time is global and monotone across
  segments, never reset. (Adjacent to hyoushigi's "seed playhead on re-arm"
  open item.)
- **Quantized to safe boundaries** — rotation fires between turns at a
  phrase boundary (a trap is the natural trigger: every N blocks or M
  phrases, quantized), so it can never split a tool_use/tool_result group or
  land mid-phrase.
- **Forward-only, like everything else** — a closed segment never reopens;
  revisiting it is a read/export ("what happened at bar 212" is a windowed
  read over cold record, not a context promotion).
- **Rotation IS a normal hydrate boundary — eat the cache warmup** (decided
  after weighing a "continuation fork" that would transplant the live
  `ConversationMailbox` to keep wire bytes identical). The economics don't
  justify the variant: rotation cadence is hours, the miss costs one
  full-price prefix read + the cache-write surcharge (pennies), and for
  local players one slow turn that **the fallback covers** — the vamp eats
  the warmup, which is what `UseLastGood` is for. The transplant would have
  bought that penny with a second fork flavor carrying *different invariant
  behavior* plus BlockId-remap surgery in the conversation path. Bonus from
  keeping one fork semantic: **rotation becomes the producer's delivery
  point for free** — chart/patch-sheet revisions and exclusions queued in
  the context take effect at the next rotation via the existing
  exclude/edit-at-boundary rule, no separate marker-advance machinery.
  The invisibility test weakens deliberately from byte-invisible to
  *musically* invisible (one slow turn, no uncovered dropped beat).
  Re-hydration byte drift is real but harmless — one-time miss, then warm
  (known sources: `tool_use_id` fallback to `block.id.to_key()` at
  hydrate.rs:161; rc-fork block injection — composer fork rc should stay
  lean). Measure, don't guess: rulers track turn-after-rotation latency +
  fallback-fire-on-rotation; revisit the continuation fork only if cadence
  must rise to where warmups bite.

Remaining residue (deferred, mechanical):

- **Window-edge atomicity** — tail-window selection must respect
  must-travel-together groups; the mailbox's snapshot repair is the existing
  gate to extend.
- **Closed-segment lifecycle** — archive RPC still missing; eventual
  compaction/export of cold segments is now a per-segment, offline concern
  with no tempo pressure.

Rejected-for-now alternatives: fork-and-reap with destructive compaction
(reaping = compaction into a log row); a first-class WAL document kind
(unnecessary once the block log itself is the WAL — revisit only if a second
consumer genuinely needs a standalone log type).

Related existing gaps: no archive RPC yet (hyoushigi.md "not yet" list).

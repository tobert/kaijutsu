# Project Chameleon — models playing to the beat

> Status: design jam 2026-06-10; vocabulary locked 2026-06-11 (tracks not
> voices, phrases not bars in the kernel, silence until first good).
> **Rewritten to present tense 2026-07-01**, after the track substrate landed
> (`docs/tracks.md` Stages 1–3 M1, 2026-06-29/30) and the first end-to-end
> sound shipped: local model → ABC → committed score → derived MIDI →
> `AlsaMidiOut` → TiMidity, live on zorak 2026-06-30. The how-we-got-here
> chronology lives in `docs/devlog.md` (the Tracks Stage 1/2/3, rotate-action,
> beat-state-persistence, and context_type-decomposition entries) and in git
> history of this file. Companions: `docs/hyoushigi.md` (the timing substrate),
> `docs/tracks.md` (clock domains — the load-bearing implementation tracker),
> `docs/midi.md` (hardware MIDI I/O), `docs/pcm.md` (samples).

Chameleon teaches models to play music to a beat, starting with a small local
model playing the bass line of Herbie Hancock's *Chameleon* (the Head Hunters
vamp: B♭ Dorian, B♭m7–E♭7) and working backwards toward a band. The tune is
chosen deliberately: it is a two-chord vamp, so the degraded mode — repeating
the last phrase — is musically indistinguishable from the job. The system
sounds correct even when a player contributes nothing; every successful turn
is pure upside.

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
band owns time — the **track** does (`docs/tracks.md`, landed): a track is a
clock domain owning the clock, the playhead, the score (held by a durable
**score context**, `context_type="score"`), a pluggable `ClockSource`, and a
set of attachments, each with its own wakeup + rotate cadence. *Being a
musician = being attached to a track with the musician tick behaviour.* The
musician rc bundle attaches its context at create
(`assets/defaults/rc/musician/create/S20-arm.kai` → `kj transport attach`,
which derives the lane from the label, attaches stopped + OODA-armed).

## Decisions

- **Tracks, not voices (2026-06-11).** The stable lane identity on the
  timeline is a *track* (DAW sense): the track persists while players come
  and go; `Cell.played_by` separately records who played, so a substitute
  player inherits the lane (`UseLastGood`, `KJ_HEARD`) intact. Now structural:
  the track is a first-class entity and its score survives every page-turn.
  "Voice" stays reserved for ABC's `V:` field — one track may project to
  several ABC voices (keys on two staves). "Lane" stays reserved for
  automation inside a track. **"Clip"** is reserved for a placed media
  reference on a track (a committed cell with the clip MIME — design in
  `docs/clips.md`); "cue" keeps meaning a trap message (the event taxonomy
  below).
- **Phrases, not bars, in the kernel (2026-06-11).** The timebase speaks
  `beats_per_phrase` (on `BeatPolicy`, a track-level knob); the kernel chunks
  musical time in phrases only. Bars are a notation/human affordance — ABC
  barlines, UI labels, charts — and those layers keep their bar vocabulary by
  translating at the edge (the musician tick rc computes `bars = KJ_PHRASE_BEATS
  / 4` in kaish and hands the model spelled-out fill targets). This sidesteps
  mixed-meter complexity: a phrase is N beats however notation bars it. Field
  shape stays open to per-phrase beat counts (irregular phrases) later.
- **Hearing is symbolic.** Players exchange ABC + data, never audio. One shared
  score per track; `KJ_HEARD` shows the recent committed notation — and since
  the score moved onto the track (`tracks.md` Stage 2) it is the *real* band
  view: all producers, across rotations, read from the track's score context.
  Consequence: groove/feel must be *explicit data* (micro-timing offsets as an
  automation lane) — symbolic players are quantized by construction, so laying
  back is a parameter, not an accident.
- **Repeat-on-dropped-phrases = the required Cell `fallback`.** Do not build
  a second repeat mechanism; `UseLastGood` per track is the vamp insurance.
  An empty track resolves to `Skip` — silence until the first good phrase
  (decided 2026-06-11; no chart seeding, no arm-gating). `UseLastGood` is
  **lane-scoped, producer-blind** (with co-producers, A's fallback may repeat
  B's last phrase — a decision, not a bug), and it now survives a kernel
  restart: attach rehydrates the committed log from the score context's ABC
  blocks (`tracks.md` Stage 2 WI 7).
- **Notation is the score; MIDI is a render of it.** The committed cell is the
  ABC itself (`text/vnd.abc`, via the validating `cas_commit` resolver); the
  write barrier consults the mime-keyed `DeriverRegistry` and inserts MIDI as
  a **derived sibling block** (same tick, same track, `Role::Asset`,
  `parent_id` = the ABC source). MIDI never enters the committed log, so the
  `UseLastGood` pool is notation-pure by construction. MIDI-out **to
  hardware** is the same idea one step further: a `RenderTarget` on the track
  (`kj transport render --track <t>`, `AlsaMidiOut`) emits committed cells to
  ALSA, scheduled on the speculation lead — landed with `tracks.md` Stage 3
  M1; design in `docs/midi.md`. Samples follow the same seam (`docs/pcm.md`).
- **Event taxonomy by delivery semantics, not source:**
  - *heartbeat* → env vars injected on the tick lifecycle, overwrite-in-place
    (`KJ_TICK`, `KJ_PHRASE`, `KJ_TEMPO`, `KJ_PHRASE_BEATS`, plus `KJ_PULSE` /
    `KJ_EPOCH_NS` for ordering and cross-context join)
  - *stream* → windows (`KJ_HEARD`, the recent committed notation across the
    track, oldest→newest)
  - *cue* → traps on musical signals (`trap '…' PHRASE%4`) — cron in musical
    time; quantized system messages and quantized obligations are the same
    feature; bar-speak in human-authored cues translates to phrases at the
    edge. **Not built yet.** Mechanics when it lands: scheduler-side, riding
    the already-populated boundary signals (`BeatOutcome.phrase_due`,
    `beat.rs`) — a trap registers (context, musical signal, script) and the
    boundary crossing fires the script / injects the block; never a blocking
    kaish sleep.
- **Turns are launch-quantized** (Ableton-style): each attachment carries its
  own `wakeup: Cadence` divisor, so one track wakes a musician every phrase
  and a conductor every 64 bars with no new machinery. Coding contexts
  quantize to ∞ (unchanged — a coder never has a heap entry).
- **The model plays ahead, not the beat it hears.** Turn latency makes the
  loop anticipatory by construction; turns schedule **one phrase ahead**
  (`phrase_delta()`). Measuring each player's actual reach k ("averages 2.3
  phrases per turn") and putting it in the transport report is still open.
- **Knobs are cells.** Parameter automation = cells with an automation MIME on
  the *same* timeline (no second grid). A **patch sheet** (curated, named,
  musically-described knob subset) lives in the system slot; artistic-range
  growth = the capability allow-set pattern (widen the loadout over time).
  CLAP hosting (clack) when plugins land — the clappers clap the CLAPs.
  **Not built yet.**
- **Producer loop = drift on a slower clock**, two distinct output channels:
  quantized live cues (traps, phrase boundaries — notes from the booth) and
  durable chart/patch-sheet revisions at hydrate boundaries (the
  self-introspection-kernel pattern). Evals: deterministic rulers first
  (onset-vs-grid, ABC parse-failure rate, fallback-fire rate), audio ML taste
  models later. **Feedback must arrive in the receiver's control vocabulary**,
  heavily attenuated — one cue per phrase, never the raw eval firehose.
  **The producer chair is not built**; the chart gap is the live consequence
  (see Open items).
- **Big models author vocabularies; small models speak them.** "Dump this VST
  into the chain" is an Opus one-shot whose output is a patch-sheet revision
  drifted to the player. ABC-only output (no tool calls) is ideal
  small-local-model UX — the symbolic decisions accidentally made the player
  role exactly the shape small models are good at.
- **Players are rc programs; the producer authors them (2026-06-12).** A
  player's behavior lives in its rc scripts, not Rust — the producer steers by
  editing them (the self-introspection-kernel pattern; less logic in Rust).
  Locked consequences, as landed:
  - **Thin fork = rc-rebuilds.** A player is spawned by a `spawn`-preset fork
    (`kj fork --preset spawn`, implemented per `docs/fork-filters.md`) that
    keeps ~nothing; the child's `musician/fork/` rc re-establishes setup
    (hydration re-mark via `fork/S40-hydrate.kai`) — setup is *already*
    declarative rc, so a fork just re-runs it.
  - **Fork-lineage IS song form.** Each thin fork is a section/movement. The
    page-turn is scheduler-driven: at the rotate horizon the scheduler
    synchronously detaches the retiring parent (race-free by construction) and
    fires the `rotate` rc — `musician/rotate/S10-rotate.kai` is exactly
    `kj fork --preset spawn --switch && kj transport attach && kj transport
    play`. The child inherits the parent's **attachment row** (track + wakeup
    + rotate cadence) via the fork copy, so the cadence travels with no
    explicit flag and the song keeps turning. The time-well's
    fork-lineage-down grammar draws the performance natively.
  - **Tick continuity is free.** The clock and playhead live on the *track*
    and never leave with a retired context, so musical time continues across
    every page-turn by construction. (The 2026-06-29 `playhead_tick` carry —
    copying the number context→context — was a stage-0 stepping stone,
    deleted by `tracks.md` Stage 1; its tests were re-pointed at "the clock
    stayed on the track.")
  - **Producer edits are horizon-latched.** Scripts snapshot at instantiation
    (updates don't leak into a live context), so a producer's rc edit lands at
    the player's *next* page-turn — musically right (direction changes on a
    section boundary, never mid-phrase). The snapshot-on-instantiation
    behavior is the *update channel* here, not a limitation.
  - **The marker owns cost; the fork owns structure.** Rotation is NOT a
    storage/cost mechanism — storage on btrfs+sqlite is cheap, and the
    hydration marker already bounds per-call tokens. All blocks stay real and
    stored: the app shows the whole performance, every segment is a complete,
    durable context (contexts **are** the history — that's *why* we rotate
    instead of wiping and reusing). The thin fork's only jobs are lean-player
    spawn, song structure, and rc-refresh.
- **`context_type` is an rc bundle of features, not a Rust enum — DECOMPOSITION
  COMPLETE (2026-06-28).** The beat runtime keys off *attached/armed state*,
  never a type name: `on_turn_completed` (the OODA Act) gates on the
  attachment's `ooda_armed`; **zero** `== "musician"` string checks survive in
  production code (only comments and test fixtures — verified 2026-07-01).
  Everything a "musician" *is* lives in the open layers:

  | Feature of a "musician" | Where it lives |
  |---|---|
  | tool-free, drive-only loadout | rc binding (`musician/create/S10-binding.kai`) — the capability allow-set |
  | ABC-output stance primer | rc `.md` → system slot (`create/S15-abc-primer.md`) |
  | hydration window policy | rc-driven (`create/S30-hydrate.kai` → `kj context hydrate --window`) |
  | beat participation + track lane | rc-driven (`create/S20-arm.kai` → `kj transport attach`; label → lane, loud refusal on an unsluggable label) |
  | OODA Act crystallizes turn→cell | gated on the attachment, structural (`beat.rs`) |

  So a new beat-bearing context_type (`funkMusician`,
  `lyricist_in_time_with_music`) is a pure rc bundle — no kernel edit.
  **Focus lives in the loadout, not in privilege:** a player is kept safe to
  jam by what its loadout can *reach* (a musician is tool-free except `drive`,
  so it can't fork-bomb or stomp a sibling by construction), never by auth
  denials — crosstalk that leaks through is tolerated, because crosstalk is a
  feature (`docs/instrument-design.md`, "Many hands, one trust boundary").
  The kernel's own beat lifecycles (`tick`/`rotate`) deliberately run able to
  act. The remaining axes of the decomposition are open: **decouple the OODA
  Act from ABC** (content-type-keyed validation/derivation) and **per-type
  `BeatPolicy` defaults** (a funk player shouldn't be stuck on
  `musician_default()`; the per-attachment `wakeup` divisor already landed) —
  both tracked in `docs/issues.md`.

## Substrate status (2026-07-01, verified against code)

The original gap analysis (2026-06-11, "the timeline is a solo instrument") is
fully closed:

1. **Track on `Cell`** ✓ — `Cell.track` + `Cell.played_by`
   (`kaijutsu-hyoushigi/src/cell.rs`); `UseLastGood` per track, empty track →
   `Skip`.
2. **Notation-first commits** ✓ — `cas_commit` commits the ABC; the
   `DeriverRegistry` derives the MIDI sibling at the write barrier.
3. **Shared score** ✓ — the track owns its `Timeline` via the durable score
   context (`tracks.md` Stage 2); N producers are structurally supported
   (copies + `played_by`, ties at a tick allowed); music keeps one *playing*
   binding per track as **loadout policy**, not structure.
4. **Windowed read** ✓ — `KJ_HEARD` reads the score context
   (`beat.rs::heard_json`, window = `HEARD_WINDOW_PHRASES` (8) ×
   `beats_per_phrase`), notation-pure, all producers, across rotations.

The three new-mechanism items from the original build sequence (numbered 4–6
there; the four gaps above were a separate list), where they stand:

- **Mechanism 4 — quantized flush + transport report.** The now-facts half landed:
   `KJ_TICK`/`KJ_PHRASE`/`KJ_TEMPO`/`KJ_PHRASE_BEATS` (+ `KJ_PULSE`/
   `KJ_EPOCH_NS`) are seeded into the `tick` rc lifecycle, and `kj drive
   --prompt` writes the transport report as a **durable User block** that
   hydrates as the fresh user turn. Still open: (a) the *quantized mailbox
   flush* — async inbound events emitting their digest on the grid crossing
   rather than the next turn; not load-bearing solo, wanted at band time.
   (b) *measured reach k* — turns still schedule a fixed one phrase ahead.
- **Mechanism 5 — kaish heartbeat vars.** Landed, including `KJ_HEARD` as a pragmatic
   **JSON-string push**. Two follow-ups when the kaish arrays/hashes plan
   lands: expose it as a real kaish **array** (indexable, `for phrase in
   $KJ_HEARD`), and re-shape **push → pull** (a `kj`-reachable windowed read
   so the script chooses depth/track — one read, three consumers: `KJ_HEARD`,
   fork-carry, marker-archive). Traps (*cue* delivery) later.
- **Mechanism 6 — hydration windowing.** Shipped + review-hardened; below.

## The stamp-turn process model + the hydration marker

The player turn is near-stateless: template + data → output. A stable prefix
(stance + chart + patch sheet — system slot, prompt-cache-aligned) executes
over and over with a fresh transport-report suffix each time. **No real
conversation.** At beat rate the prefix cache never goes cold and per-turn
cost ≈ window + output; local players don't care at all.

The cost guard is the **hydration marker** (Amy, 2026-06-10): a context keeps
endless rows, but turns hydrate only `[0, marker] ∪ [now − window, now]` — a
pinned prefix plus a sliding tail. Blocks between marker and window are
durable record, never hydrated. The generation log *is* the block log — no new
document kind, no fork-per-turn, no reaping. The prefix is byte-stable so the
Anthropic prompt cache aligns; the tail is the fresh transport report. It is
the conversation-side twin of hyoushigi's write barrier: behind the marker,
the immutable cached *program*; ahead of it, the open *record*. **Moving the
marker = committing a durable revision** (a producer chart update writes new
program blocks and advances the marker past them). Convergence worth noting:
pinned-prefix + sliding-window is precisely how attention-sink / StreamingLLM
inference manages context at the attention level — same shape, one layer up.

**As built (shipped 2026-06-11, hardened 2026-06-12 by a three-reviewer pass —
full story in devlog/git history).** The Rust side is a minimal mechanism; the
*policy* lives in rc:

- `ConversationMailbox::rehydrate_windowed` + the keep-set selection
  (`llm/mailbox.rs`) rebuild the windowed view each turn; the `[0, marker]`
  prefix stays byte-stable so the wire prompt cache aligns. A `windowed` flag
  guards the windowed→full transition (the review's one real corruption bug —
  fixed, TDD).
- Durable per-context policy (`context_hydration`: marker + window), absent
  row = hydrate everything. Applies on **cold start** too — restart is a
  non-event.
- **Fail-loud on bad policy:** a DB read error or corrupt persisted policy
  fails the turn rather than silently hydrating everything (silently disabling
  the cost guard on a context driving at tempo is the wrong fallback). The one
  genuine fail-safe: a marker that parses but names an absent block (e.g.
  excluded) warns and hydrates the whole log — show more, never corrupt.
- Surface: `kj context hydrate [<ctx>] --window <N> [--mark <block>] |
  --clear` (Operator-gated; `--mark` validated against the context,
  `--window 0` rejected). The musician `create` rc (`S30-hydrate.kai`) sets
  the window once at birth; `fork/S40-hydrate.kai` re-marks the thin child at
  its tail.

Honest edges (open): **(a)** the marker-advance-on-durable-revision flow isn't
built (comes with the producer). **(b)** Windowing bounds *tokens*, not
RAM/disk — accumulation is fine, storage is cheap. **(c)** `window` counts
**blocks**, not turns/phrases (~2-3 blocks per OODA turn). **(d)** cache
breakpoints sit at message indices that windowing shifts — harmless for the
local bass (no prompt cache), reconcile when API-model chairs join.

## Rotation — structure, not storage

Rotation (the page-turn) is about **song form and lean players**, not space:
storage is cheap and the marker owns per-call cost. Blocks accumulate freely;
nothing is rotated for space. (The original "growth answer" framing and the
carry-based continuity mechanism are in git history / devlog.)

Invariants, as they stand on the track substrate:

- **Invisible to the player** — the test for a correct rotation: program
  bytes identical (prompt cache survives — Anthropic caching is
  content-keyed), tail window carried, `KJ_HEARD` unbroken (it reads the
  track's score, which never rotates). The player cannot tell a rotation
  happened.
- **Tick continuity by construction** — the clock and playhead live on the
  track and never pause for a page-turn; there is no carry, no seed dance, no
  horizon race. A truly *fresh* musician (a `create`, not a rotation)
  correctly starts at 0.
- **Gap, not overlap** — at the horizon the scheduler synchronously detaches
  the retiring parent before firing the rotate rc, so there is never a beat
  with two producing bindings; the speculation lead covers the notation across
  the handoff (the committed score already extends past the boundary), so the
  producer gap never reaches the DAC. An atomic `Swap` that suspends the clock
  across the handoff stays a deferred polish item (`tracks.md`).
- **Quantized to safe boundaries** — rotation fires between turns at a phrase
  horizon (`phrase % rotate_every == 0`, decided scheduler-side precisely so
  it's race-free), never splitting a tool_use/tool_result group or landing
  mid-phrase.
- **Forward-only, like everything else** — a closed segment never reopens;
  revisiting it is a read/export, not a context promotion.
- **Rotation IS a normal hydrate boundary — eat the cache warmup.** One
  full-price prefix read per rotation (cadence is hours; pennies), and for
  local players one slow turn that **the fallback covers** — the vamp eats the
  warmup, which is what `UseLastGood` is for. Bonus of keeping one fork
  semantic: rotation is the producer's delivery point for free —
  chart/patch-sheet revisions and exclusions queued in the context take effect
  at the next rotation via the existing exclude/edit-at-boundary rule. The
  invisibility test weakens deliberately from byte-invisible to *musically*
  invisible. Measure, don't guess: rulers track turn-after-rotation latency +
  fallback-fire-on-rotation; revisit a continuation fork only if cadence must
  rise to where warmups bite.

Remaining residue (deferred, mechanical): **window-edge atomicity** (tail
selection must respect must-travel-together groups; the mailbox's snapshot
repair is the gate to extend); **closed-segment lifecycle** (archive RPC still
missing); **rotate chains pollute the context tree** (a director scanning
`kj context list --tree` wades through N-deep chains — UX fix ideas in
`docs/issues.md`).

Rejected-for-now alternatives: fork-and-reap with destructive compaction
(reaping = compaction into a log row); a first-class WAL document kind
(unnecessary once the block log itself is the WAL); a byte-identical
"continuation fork" for rotation (the economics don't justify a second fork
flavor — see the eat-the-warmup invariant).

## Open items (the live list)

- **The chart / gig channel** — the now-facts channel is wired but *nothing
  tells a player what tune it's playing*; the gig is currently hardcoded in
  the tick prompt. Minimal fix: a `musician/create/S05-chart.md` in the cached
  system prefix (pure rc, zero kernel changes — it slots before
  `S15-abc-primer.md`); becomes the producer's drift-delivered revision
  surface when that chair lands. (`docs/issues.md`, "No chart is seeded".)
- **Measured reach k** in the transport report (turns schedule a fixed phrase
  ahead today). Instrumentation sites: `on_turn_completed` (`beat.rs:1536`)
  records scheduled-vs-committed ticks; `BeatOutcome` (`beat.rs:352`) carries
  them out of `process_track`; a rolling average feeds a `KJ_REACH_K` var.
- **Quantized mailbox flush** (band time). The hook already exists:
  `process_track` pushes `BeatOutcome.phrase_due` (`beat.rs:963`) — the flush
  emits a digest block into each phrase-due context at that crossing.
- **Eval ruler counters** (onset-vs-grid, parse-failure rate,
  fallback-fire rate — cheap, and they answer bass-gemma viability for free).
- **`played_by` provenance is half-degenerate** — fallback repeats authored
  `beat()` are *correct* provenance (the transport played them — no forgery);
  the degenerate half is turn output landing under `system()` instead of a
  per-player principal. Harmless solo; mis-attributes when multiple models
  share a context. Fix site: `on_turn_completed` → `schedule_abc_cell`
  (`beat.rs:1536`), deriving the principal from the turn's model identity
  (`docs/issues.md`).
- **Decouple the OODA Act from ABC** + **per-type `BeatPolicy` defaults** —
  the remaining context_type-decomposition axes (`docs/issues.md`).
- **Cue traps** (`trap '…' PHRASE%4`) — the third delivery semantic, unbuilt.
- **Per-track MIDI channel** (needed the moment two tracks sound at once). The
  render seam landed single-track (PCM slice 5c): the app MIDI sink schedules
  every cue on MIDI **channel 0** (`MidiParams::default().channel`), so two
  simultaneous tracks would collide on one channel. The band needs a track→
  channel assignment carried in the cue (or derived at the sink from the track/
  lane), so keys, bass, and drums land on distinct channels (drums → ch 9 GM).
  Until then, "one producing binding at a time" (the loadout policy, above)
  keeps it safe. Sites: `RenderCue`/`MidiParams` (`kaijutsu-audio`, the app's
  `midi.rs` render); it's the render-side twin of the "tracks, not voices" lane.
- **Per-track render-cue routing (flush + fan-out).** The wire `RenderCue`
  carries the track's **score `context_id`**, but the app sink currently ignores
  it: it plays every cue and a `RENDER_FLUSH_MIME` cue flushes the sink's *whole*
  ALSA queue — so stopping track A would silence track B through the same sink.
  Multi-track needs the sink to key its scheduled events + flush by the cue's
  context (one queue-region per track, or per-track ports). This is the same
  routing gap `docs/pcm.md` "Distributed listening" names for multi-listener
  playback — solve them together. Harmless single-track today.

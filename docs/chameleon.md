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

> **Direction (2026-06-29): the beat is moving off the context and onto the
> track** — the track becomes a *clock domain* a context **attaches to** to be
> beaten, and the track owns the score, the clock source, and each attachment's
> wakeup + rotate cadence. This is the substrate under both this doc and the
> (now-retired) myaku pulse facility; it retires the per-context playhead carry
> and the `track_head`/`stop --track` overlay we'd sketched. Design + staged
> implementation plan: **`docs/tracks.md`**. Chameleon stays the *music
> application* that consumes tracks; the band/players/rotation-as-song-form
> below remain valid, read on the track's clock rather than the context's.

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
  notation-pure by construction. *(2026-06-29: MIDI-out **to hardware** is the same
  idea taken one step — a **render target** on the track that emits committed cells
  to ALSA MIDI out on an edge node, alongside app-display and audio samples. Design
  in `docs/midi.md`.)*
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
- **Players are rc programs; the producer authors them (2026-06-12).** A
  player's behavior lives in its rc scripts, not Rust — the producer steers by
  editing them (the self-introspection-kernel pattern; less logic in Rust).
  Worked out across a long design thread; what got us here is below the line.
  Locked consequences:
  - **Thin fork = rc-rebuilds.** A player is spawned by a `spawn`-preset fork
    (`kj fork --preset spawn` per the 2026-06-12 fork-filters design,
    `docs/fork-filters.md`; formerly `--shallow`) that keeps ~nothing; the
    child's `musician/fork/` rc re-establishes
    setup (stance/chart/patch-sheet — already declarative) and arms itself
    (`kj transport ooda on --context <child>`; parent disarms — pure rc, the
    verb already takes `--context`). Carrying recent notation into the child is
    an rc-scripted block copy, not a fork-filter feature — and folds into the
    `$HEARD` push→pull read (one windowed-notation read, three consumers:
    `$HEARD`, fork-carry, marker-archive). We chose rc-rebuilds over
    preserving the parent's setup blocks because setup is *already* declarative
    rc, so a fork just re-runs it — which also dissolves the "preserve
    `P_parent`, build a read surface" detour: a thin child's tail-at-fork is
    small, so the fork rc re-anchoring the marker at the child tail (mirror of
    `create`) is cheap and correct, no env-var / `--show` needed.
  - **Fork-lineage IS song form.** Each thin fork is a section/movement; the
    page-turn is the player **self-`kj fork --preset spawn`-ing on a horizon**. The
    time-well's fork-lineage-down grammar draws the performance natively.
    *✅ Rotate action shipped 2026-06-28* (`musician/rotate/S10-rotate.kai`): the
    scheduler stops the parent at the horizon and fires the `rotate` rc, which forks
    a spawn child, `--switch`es onto it, and arms+re-rotates+plays it on the
    PARENT's track (fork now copies `beat_state`, so the lane survives the
    page-turn). **✅ Tick continuity shipped 2026-06-29.** `beat_state` now carries
    the lane's live playhead (`playhead_tick`): the scheduler snapshots it into the
    parent's row at the rotate horizon, the fork copy hands it to the thin child,
    and the child's `arm` seeds from `max(block-log max tick, carried playhead)` —
    so musical time *continues* across the page-turn instead of resetting to 0. No
    history is moved or archived to do this: the parent stays a complete, durable
    segment (contexts **are** the history — that's *why* we rotate instead of
    wiping and reusing), and only the playhead *number* is carried forward. The
    `max()` rule means a real context with committed history (cold restart) still
    trusts its block log; the carried tick only supplies time a thin child lacks.
  - **Producer edits are horizon-latched.** Scripts snapshot at instantiation
    (updates don't leak into a live context), so a producer's rc edit lands at
    the player's *next* page-turn — musically right (direction changes on a
    section boundary, never mid-phrase). The snapshot-on-instantiation behavior
    is the *update channel* here, not a limitation.
  - **The marker owns cost; the fork owns structure.** Rotation is NOT a
    storage/cost mechanism — storage on btrfs+sqlite is cheap, and the hydration
    marker already bounds per-call tokens. All blocks stay real and stored: the
    app shows the whole performance, the parent keeps everything (browsable via
    fork-lineage). The thin fork's only jobs are lean-player spawn, song
    structure, and rc-refresh.

- **`context_type` is an rc bundle of features, not a Rust enum — the beat is a
  capability a context_type *consumes*, not a name the kernel matches
  (direction set 2026-06-28).** The trigger: `kj transport arm` (the manual
  restart-recovery verb, shipped 2026-06-28) gates on `context_type == "musician"`,
  and a survey asked how deep that string goes. Answer: **the beat *runtime* is
  already feature-decomposed — it keys off "armed", not the name.** `on_turn_completed`
  (the OODA Act that crystallizes a turn's ABC into a cell) gates on
  `self.armed.get(&ctx)` + `ooda_armed` (`beat.rs:842`); so do `fire_due` and
  transport play/pause. Once a context is **armed**, nothing downstream asks "is
  it a musician?". The literal `"musician"` survives only at the three create-time
  **entry gates** that decide a context *becomes* armed and *gets a lane*:
  `rpc.rs:1672` (send `BeatCommand::Arm`), `context.rs:628` (derive the track from
  the label), and `transport.rs:268` (the arm verb's refusal). Everything else a
  "musician" *is* already lives in the open layers:

  | Feature of a "musician" | Where it lives |
  |---|---|
  | tool-free, drive-only loadout | rc binding (`musician/create/S10-binding.kai`) — the capability allow-set |
  | ABC-output stance primer | rc `.md` → system slot (`S15-abc-primer.md`) |
  | hydration window policy | rc-driven (`kj context hydrate --window`) |
  | OODA Act crystallizes turn→cell | gated on **armed** (structural, `beat.rs:842`) |
  | beat participation + track lane | the **only** Rust-string-hardcoded bit (the 3 gates above) |

  So `context_type` is *already* "the name of an rc bundle that composes
  features" — the rc lifecycle + capability allow-set + the `armed` flag **are**
  the feature-decomposition system; only the beat *entry gate* is still keyed on
  the literal name. The trajectory (consistent with **"Players are rc programs"**
  above — even arming is rc, not Rust):
  - **✅ SHIPPED 2026-06-28 — arming moved from Rust to rc.** `kj transport arm`
    (the rc-callable primitive) now runs from `musician/create/S20-arm.kai`; it
    derives the lane from the label and picks the policy (persisted, else
    `musician_default()`). Both Rust arm sites — `rpc.rs` create_context_inner
    **and** the `context.rs` `kj context create` builtin (which *duplicated* the
    same logic) — deleted; one rc script replaces the pair. The two create-time
    `== "musician"` string checks are **gone**.
  - **✅ SHIPPED — the arm gate is a property, not a name.** `kj transport arm`
    no longer checks `context_type == "musician"`; it arms any context whose
    label yields a valid track lane (else refuses, so no silent shared lane).
    Arming IS the opt-in — shared-trust (capabilities are ergonomic nudges, not
    security). A type-changed context still re-arms from its persisted row.
    - **Focus lives in the loadout, not in privilege (decided 2026-06-28).** A
      player is kept safe to jam by what its loadout can *reach* — a `musician`
      is tool-free except `drive`, so it literally can't call `kj transport`/
      `fork` and can't fork-bomb or stomp a sibling, by construction — NOT by
      auth denials. So "less privileged" for a player means *narrower focus*,
      especially for a quantized model jamming freely; crosstalk that does leak
      through is tolerated, because crosstalk is a feature (see
      `docs/instrument-design.md`, "Many hands, one trust boundary"). Corollary:
      the kernel's own beat lifecycles (`tick`/`rotate`) run *able to act* — the
      page-turn needs to fork + arm — even though `fire_lifecycle` builds an
      unprivileged caller; we deliberately do NOT make capabilities hard
      boundaries, so that create-vs-beat privilege asymmetry is fine. Don't route
      mistake-prevention through the permission system (it gets in the way of the
      jam); route it through the loadout.
  - **✅ ENABLED — new context_types are pure rc, zero Rust.** `funkMusician` /
    `lyricist_in_time_with_music` become rc bundles whose `create/` calls
    `kj transport arm` + a flavored stance (+ later their own tempo). Building
    those specific roles is future work, but the kernel no longer stands in the
    way — no `== "<name>"` branch, no enum variant. (Caveat: arming reports a
    LOUD rc Error block when no beat scheduler is wired — embedded/test only;
    the server always wires one. More honest than the old silent `log::warn`.)

  Two open issues are the *other axes* of the same decomposition: **"Decouple the
  OODA Act from ABC"** (content-type-keyed validation/derivation — *what artifact*
  a player produces, separate from *whether* it has a beat) and **"Cadence
  settable per context"** (per-type `BeatPolicy` defaults so a funk player isn't
  stuck on `musician_default()`). Corollary, now realized: the *shallow* fix — a
  `ContextType(String)` newtype to centralize the literals — is moot for the beat;
  the create-arm move left **zero** beat-related string checks. Prefer the
  decomposition over the newtype; don't do both.

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
   *(Track reframe, 2026-06-29: today `$HEARD` is a tick-window query over the
   per-context block log; once the **track** holds the score (`docs/tracks.md`),
   `$HEARD` re-targets the track's score — the same window, read off the track's
   copies rather than the context's log. The "score vs context-conversation seam"
   in tracks.md tracks exactly this.)*

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

**RC-drives-the-marker (Amy, 2026-06-11) — keep policy out of Rust. SHIPPED
(first cut, 2026-06-11).** Don't hardcode "how much to keep" in the kernel. The
Rust side is a *minimal mechanism*; the *policy* lives in rc. As built:

- **Window selector** — `select_hydration_window(blocks, marker, window)`
  (`llm/mailbox.rs`) returns `[0, marker] ∪ last-window`, deduped where prefix
  and tail meet, and **fails safe to the whole log** if the marker is stale
  (never hide context behind a broken marker).
- **Durable policy** — a per-context `context_hydration` table (`marker` =
  `BlockId::to_key()`, `window_size`); accessors `set/get/clear_hydration_policy`.
  Absent row = hydrate everything (every non-musician context, untouched).
- **Mailbox rebuild** — `ConversationMailbox::rehydrate_windowed` rebuilds the
  windowed view each turn (a sliding tail can drop a block, which the append-only
  `catch_up` can't express; bounded to prefix + window). The `[0, marker]`
  prefix is byte-stable across rebuilds, so the wire prompt cache still aligns.
- **Wired** into `process_llm_stream`/`hydrate_messages`: reads the policy each
  turn (read failure or corrupt policy → fail the turn loudly — see *Hardened*
  below), windows when set. Applies on **cold start**
  too — the same path a restart re-hydrates through, so the persisted marker
  bounds cold-start hydration as well as steady state (restart is a non-event).
- **Surface** — `kj context hydrate [<ctx>] --window <N> [--mark <block>] |
  --clear` (Operator-gated); the marker defaults to the current tail. The
  musician `create` rc (`S30-hydrate.kai`) sets `--window 16` once at birth —
  policy in rc, the self-introspection-kernel pattern.

**The tail slides in memory; the row is upserted only at create + on a durable
revision** (re-running `kj context hydrate` after the producer writes revision
blocks), NOT per turn — so there is no per-turn rc hook, declarative-window
(Shape 1) over the imperative rc-advanced boundary (Shape 2).

**Hardened (2026-06-12, independent review — Fable + Gemini + DeepSeek,
unanimous).** The first cut shipped TDD-green but un-reviewed; three reviewers
found the core selector math clean (no off-by-one) but surfaced a real
corruption bug and a cluster of silent-failure gaps. Fixed, all TDD:
- **CRITICAL — windowed→full scramble.** The `ConversationMailbox` persists
  across turns; after a windowed turn `seen` has a hole where the archived
  middle was, so a later un-windowed `catch_up` folded the middle and *appended*
  it after the tail → out-of-order wire (silent corruption). Fixed with a
  `windowed` flag: `catch_up` rebuilds from scratch on the windowed→full
  transition.
- **Fail-loud, not fail-safe-silent, on bad policy.** A DB read error or a
  *corrupt* persisted policy (unparseable marker / `window < 1`) now returns
  `Err` and **fails the turn** rather than silently hydrating everything —
  silently disabling the cost guard on a context driving at tempo is the wrong
  fallback (per the kernel's "silent fallbacks are a mistake" stance). The
  **runtime** stale-marker case (marker parses + names a real block id but it's
  absent from the log — e.g. excluded) *keeps* the warn + fail-safe-to-whole-log
  fail-safe: that's the one genuine "show more, never corrupt" case, and failing
  it would kill a player just because a block was excluded.
- **Validated surface.** `--mark` is checked against the context (was:
  parseable-only → guard silently off forever); `--window 0` is rejected (it
  drops the current turn from the wire).

Deferred / honest edges: **(a)** the marker-advance-on-durable-revision flow
isn't built (P is set once at create; the producer path that moves it forward
comes with the producer). **(b)** Windowing bounds *tokens*, not *RAM/disk* —
cold start still loads the full log to window it — but **accumulation is fine**
(storage is cheap; rotation-for-space is dropped — see the rotation reframe
below). **(c)** `window` counts **blocks**, not
turns/phrases (~2-3 blocks per OODA turn). **(d)** the musician's S20 cache
breakpoints sit at message indices that windowing shifts — harmless for the
local bass (no prompt cache), to reconcile when API-model chairs join.

**Reframed (2026-06-12): rotation is structure, not storage.** Storage on
btrfs+sqlite is cheap and the hydration marker already bounds per-call cost — so
the "growth answer" framing below is **not** the driver, and rotation-for-space
is dropped. The shallow/thin fork *survives* with a different, now-locked
purpose: spawning lean players and turning the page on song sections (see the
"Players are rc programs" decision). Blocks accumulate freely; nothing is
rotated for space. The invariants below **carry over unchanged** to the
page-turn fork — they're exactly what makes a self-fork invisible to the player.
The original growth reasoning is kept because it's how we arrived at the
thin-fork mechanism and these invariants.

**Rotation (Amy, 2026-06-10) — the growth answer [superseded as the *driver*;
mechanism + invariants retained]:** cap context length and
cycle via **shallow fork** — "fork without history": copy the program
(behind the marker) + the tail window, nothing else; the old context becomes
a closed cold segment; the player's label moves to the new head. Log
rotation, literally. A day of playing = a short chain of segments in the
DAG (one fork edge per rotation — the time-well's fork-lineage grammar draws
it natively); the app attaches only to the live head, so dense history never
reaches the renderer. This was also the second concrete voice for the
fork-selectivity revisit — resolved 2026-06-12 in `docs/fork-filters.md`
(interval selection + presets; the `window`/`spawn` factory presets are
exactly these two voices, brought to the design together).

Rotation invariants:

- **Invisible to the player** — the test for a correct rotation: program
  bytes identical (prompt cache survives — Anthropic caching is
  content-keyed), tail window carried, `$HEARD` unbroken. The player cannot
  tell a rotation happened.
- **Tick continuity** (✅ shipped 2026-06-29) — musical time is global and
  monotone across segments, never reset. The carry is the playhead *number*,
  not blocks: `beat_state.playhead_tick` records the lane's live position, the
  scheduler snapshots it at the rotate horizon, the fork copies it to the child,
  and `arm` seeds from `max(block-log max tick, carried playhead)`. So a `spawn`
  rotation that copies **no** blocks still continues musical time (the carried
  tick supplies what the empty log can't), while a context with real committed
  history trusts its block log (the max never lets a stale carried value hide
  committed time). A truly *fresh* musician — a `create`, not a rotation — has no
  carried tick and correctly starts at 0; there is no prior segment to continue.
  Nothing is retired or archived to achieve this: the parent remains a complete,
  durable segment, because the contexts **are** the history.
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
  hydrate.rs:161; rc-fork block injection — musician fork rc should stay
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

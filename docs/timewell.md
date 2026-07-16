# Time Well — Evolution Plan

2026-07-03. The time well (`crates/kaijutsu-app/src/view/time_well/`) is now a
**carousel of four per-band magic-circle rings** — HotNow / ThisWeek /
ThirtyDays / Horizon, stacked by depth and receding into a central
vortex/throat glow on a shared tilted funnel axis (Konosuba-"Explosion"
HDR/bloom aesthetic). This supersedes the continuous log-spiral prototype this
doc was originally written against: the spiral read well and the musician
swarm falling toward the event horizon was the right feeling, but at 56 live
contexts the assumptions the whole well was built on had broken, and its real
job had come into focus. Keyboard nav is now ring-centric
(`(focused_ring, ring_pos)`, spin-to-gate + camera dolly) rather than the
spiral's odometer walk, but the two-stage Enter survived the rebuild intact.
This doc remains the forward plan: what the time well is *for*, the ontology
it navigates, and the staged evolution from fundamental fixes (now landed) to
a professional-feeling instrument.

**Relationship to the other docs.** `docs/time-well-concepts.md` stays as the UX
design record (the 40 mockups, the visual grammar). `docs/viz-substrate.md` was
retired 2026-07-04: its still-true substrate reference (D3 decomposition, data
flow / wire grounding, card + shader vocabulary, wire gaps, dependency notes) is
folded into the "Substrate notes" appendix below; its superseded halves (the
three-band layout thesis, the build order, the spiral) live only in git history.
New open questions land here.

## Status (2026-07-03)

What's actually built, so the staged plan below isn't mistaken for vaporware:

- **Stage 0 (tourniquet)** — shipped: hot sorts newest-first, default
  selection is the mouth.
- **Stage 1, kernel half** — shipped: real `last_activity_at` on the wire,
  `liveStatus` cached incrementally (O(1) per event, not a 5s full-scan), and
  idle-age bands (`assign_idle_band` over `HotNow|ThisWeek|ThirtyDays|Horizon`)
  as pure derivations of `now − last_activity_at`. No expiry daemon.
- **Stage 1, app half** — shipped, but not as originally planned below (which
  described terracing the continuous spiral). The app instead went through
  the spiral and out the other side: idle-age bands now render as **four
  discrete magic-circle rings**, one per band, each seated with its own cards
  as perpendicular 3D "slides," navigated ring-centric —
  `(focused_ring, ring_pos)` — with left/right spinning the focused ring to a
  fixed gate angle and up/down changing the focused ring while the camera
  dollies to frame it. `CompactingBandLayout`/`RadialBands`-layout is deleted;
  the geometry substrate for every stage below is now rings, not a spiral.
- **Stages 2–6** — still the forward plan, mostly unbuilt (read them as
  designed against ring geometry, not the spiral this doc originally
  described) — with one exception: **Stage 3's wire slice shipped 2026-07-04**
  (`TrackInfo` + `listTracks @92` + `ContextHandleInfo.trackId @16`, answered
  from the beat scheduler's in-memory state), ahead of its layout half.
- **Ring membership** — rebuilt 2026-07-05 as **explicit placement** (see "Ring
  membership becomes explicit" below): 10 seats per ring, ring 0 hand-curated,
  ring 3 hand-demoted, the two middle rings automatic by recency, overflow past
  the seats falls to the event horizon (no card). Supersedes the Stage 1
  idle-age banding and lands the core of Stage 2's rank as ring 0.
- **Live-state layer** — shipped 2026-07-04 (explore-live-time-well-action),
  a layer this plan hadn't staged: the well reacts *between* polls.
  `view/time_well/live.rs` ingests the kernel-wide event stream ungated —
  per-context **tail buffers** (HUD South returned as the selected card's
  tail -f; falls back to the polled preview) and per-track **beat phasors**
  (`LocalBeat` keyed by score context, slewed by low-rate `BeatSync`, reset on
  RENDER_FLUSH — the multi-track cut `metronome.rs` deferred). Cards carry
  event-driven material lanes (no texture rebuild): `dim.y` chatter (cyan rim
  lift the instant a context talks), `dim.z` beat (gold thump), `border` =
  track hue; the ring deck's throat glow breathes on the loudest rolling
  track's beat (`energy.y`). `view/time_well/rays.rs` renders **tracks as
  rays**: one beam per track down the funnel wall at an FNV-name-hashed
  bearing (stable across restarts — a bearing you learn), pulsing mouth→throat
  on each beat while playing; HUD East gained the lane line ("track ♪ bass ▶
  ♩120 · tick 128"). Beat stance is docs/midi.md's *distribute tempo, not
  pulses* applied to viz.
- **HUD melt** — shipped 2026-07-12, four slices: the camera-parented edge-HUD
  panel set (N identity / E specs / W lineage / S tail / always-on legend)
  absorbed into scene-native surfaces and retired. Slice 1: **lineage drapes**
  down the bowl wall (`drape.rs` — 27's silk threads, built). Slice 2: the
  selected card's face grew a **live-tail band** (South's job). Slice 3: the
  reading card absorbed **specs + ancestry + a deeper tail cut**
  (`text.rs::reading_specs_text`/`ancestry_text`; East/West's reference
  content at focus time; North's identity/status already lived on the card
  header + shader status rings). Slice 4: `hud.rs` deleted; the keyboard
  legend survives as a **transient `?` toggle** (`legend.rs` — dived-only,
  bottom-left corner panel, dismissed by `?`, zoom-out, or room exit). The
  well's mouth is the open browser space again; readouts live on the
  instrument, not on floating chrome.

---

## What the time well is for

The time well replaces a terminal multiplexer. The concrete target is Amy's
wezterm session: **9 terminals — 4 kaijutsu, 2 kaibo, 2 kaish, 1 htop — most
running a claude code session**, some holding finished work waiting for morning
review. The remix: kaijutsu ditches the pty/terminal boundary and re-cuts
terminal / ssh / claude-code / model along its own lines — contexts over a shared
kernel, kaish + VFS instead of a shell in a pty.

That use case fixes the requirements precisely:

1. **~10 addressable slots**, number-key fast, with *rule-governed* addresses —
   Amy doesn't keep a fixed layout ("kaijutsu is usually 2 because I start it
   first; it changes constantly"): what muscle memory needs is a predictable
   rule (start-order, compaction on exit), not frozen positions.
2. **Recent history skimmable** — yesterday's concluded work visible on the way
   down, reachable in a couple of keystrokes, without competing with the hot set.
3. **An event horizon** — past some depth, contexts stop being objects you scroll
   past and become an archive you *search* (form-oriented, not spatial).
4. **Mixed occupants** — claude-code-style work contexts, 1–2 shells, and a
   system-stats surface are all first-class tenants of the rank.
5. **Fast to skim** — the whole surface must reward flow: track → contexts →
   detail → inside, each hop one keystroke, legible at a glance.

## Where we are (verified 2026-07-03)

Ground truth from a code deep-dive, a wire/census inventory, and an external
(DeepSeek) review. The short version:

- **The "new stuff at the bottom" glitch — resolved by the ring rebuild.** The
  original hot-sorts-oldest-first bug (Stage 0) is long fixed, and the rebuild
  replaced spiral-index digit addressing entirely: navigation is now
  ring-centric `(focused_ring, ring_pos)`, so there's no flat spiral index for
  a digit to land on and no "ten oldest" trap. (Rule-governed digit-addressing
  — the rank — is still Stage 2, unbuilt; see "Two navigation surfaces"
  below.)
- **"Hot" has stopped meaning anything.** Banding keys only on `concluded_at`
  (`kaijutsu-viz/src/layout.rs:130`). The census: 56 live contexts — ~23
  `musician` (16 of them unlabeled rotation-children), 9 `score` (kernel
  plumbing, one per track), ~18 `mcp` verify/dev sessions, 6 `director`. Almost
  nothing concludes, so almost everything is "hot," so the hot arc is 40+ cards
  long and the design's ≤10 assumption is gone.
- **No recency is possible today.** There is no last-activity timestamp anywhere:
  not on `ContextHandleInfo` (`kaijutsu.capnp:494-511`), not in the `contexts`
  table. `liveStatus` is a coarse enum recomputed **every 5s poll by re-scanning
  every block of every context** (`kaijutsu-server/src/rpc.rs:7328`) — the
  timestamps are computed and thrown away.
- **No LOD, no horizon, no archive.** Every non-archived context is a full
  `Mesh3d` + MSDF-texture card entity forever (`sync.rs:137-170`); the concept
  doc's chips/points/particle tiers were never built. Archived contexts are
  hard-filtered (`sync.rs:37`) — they vanish rather than fall in. At the throat,
  card width (88) exceeds inter-card spacing (~57): physical overlap.
- ~~**Tracks have zero wire surface.**~~ RESOLVED 2026-07-04: `TrackInfo` +
  `listTracks @92` + `ContextHandleInfo.trackId @16` shipped (the Stage 3 wire
  slice). The wire reads the scheduler's **in-memory** `TrackState` via a
  `BeatRequest::Snapshot` query — never the persisted row, whose playhead lags
  (written only on transport transitions). Still true: attachment *requires* a
  wakeup cadence (`wakeup: Option<Cadence>` remains open).
- **Solid and worth keeping:** the pure/tested band → slot-order → flatten →
  index pipeline (`card.rs`), the `Join` reconcile, two-stage Enter, the edge
  HUD, the activity-ring pulse, `haystack_order_keys` as a grouping primitive.
  The prototype's bones were good — the ring rebuild kept all of this and only
  replaced the spiral's odometer walk with ring-centric nav.
- **Dead code:** `CompactingBandLayout` (+ `RadialBands`, `LayoutConfig`) in
  `kaijutsu-viz/src/layout.rs` — no consumer since the 7.8 spiral. **Decision:
  delete it** (the spiral is append-stable by construction; the crate's own rule
  is no abstraction without two consumers). `Band`/`assign_band`/`Join`/scales
  stay for now — noting that `assign_band`'s `concluded_at` keying is itself
  replaced by the Stage 1 idle-age bands when they land.

---

## Ontology: track → context → block

The missing layer is the durable one. Contexts are **performances** — cheap,
churning, born constantly and (today) rarely even concluded (the census proves
it: 90% of the live population is synthetic churn). What Amy actually navigates between are the
**lanes the performances happen in**: kaijutsu-coding, kaibo, kaish, eurorack,
practice machine. Those are stable across weeks. Nothing in the system models
them today — which is why the well drowns.

**Decision: generalize the existing Track rather than invent a fourth grouping
concept.** We already have `context_type` (an rc bucket, not a grouping),
semantic clusters (kernel-derived, cold-data only), and `workspace_id`
(unshipped). Adding a separate "lane/project/workspace" noun would make four.
Instead, the hyoushigi Track — already a named, persisted, attachable lane —
grows into the general concept:

- **A track is a durable named lane** with identity (`TrackId`), label,
  description, created_at, and equipment. **Every track has a clock slot; a
  non-musical track gets a noop clock** (a `ClockSourceKind` variant that never
  fires — confirmed with Amy 2026-07-03), rather than clock-optional plumbing.
  `docs/tracks.md` already designs non-musical clock sources (solar,
  compute-cycle), and the expectation is a *coder clock* develops over time
  (CI/build/test-cycle shaped) — or a coding track just borrows a musical clock
  for fun. A eurorack track carries a real clock + score context; a coding
  track starts on noop.
- **Attachment generalizes, gently**: today `Attachment.wakeup` is mandatory
  (`hyoushigi/mod.rs:122-155`). With noop clocks, that mostly stops mattering —
  a cadence on a clock that never ticks never fires, so plain membership on a
  coding track needs **no attachment schema change**. `wakeup: Option<Cadence>`
  is still worth doing for the mixed case (a member of a *clocked* track that
  shouldn't be woken — e.g. a score-viewer riding eurorack), but it's a
  refinement, not a prerequisite.
- **Contexts keep emerging, tracks persist.** This preserves the
  agent-emerges-not-noun stance: the actor is still always a principal in a
  context; the track is where the work accumulates. Fork-children inherit the
  parent's track binding (the rotation mechanism already works this way).
- The progression the UI navigates: **track → context → context detail →
  inside the context** — each level one keystroke apart.

This is the ontology bet of the plan. If it's wrong, it's wrong cheaply: the
wire additions (below) are additive, and a track with no clock is just a row.

## Two navigation surfaces, decoupled

The spiral prototype conflated the digit keys with a flat spiral position —
that was the root of the muscle-memory failure this section originally
diagnosed. The two-surface split below is still the target; only the
*mechanism* under the skim surface changed with the ring rebuild:

- **The rank** (digits `0–9`): an ordered active list with mux semantics —
  **rule-governed, not frozen**. Amy's real usage (2026-07-03): she adapts to a
  recent set — kaijutsu is usually `ctrl-a 2` *because she starts it first* —
  and the layout changes constantly. So the contract is predictability, not
  permanence: a context joins the rank **appended at the end** (start-order,
  the "I started it second, it's 2" rule), holds its position while it lives,
  and **conclude compacts** the later slots by exactly one. That's the whole
  rule; muscle memory rides the rule, not fixed addresses. (This is the
  "predictable motion, not zero motion" principle from the substrate notes
  appendix, applied
  to the keyboard surface too.) Pinning is deferred until the itch is real.
  The rank is the keyboard surface, curated by *membership* — shells, work
  contexts, a stats surface. **Since built** as ring 0 (see "Ring membership
  becomes explicit" below): in the well, digits address the focused ring's
  seats; from everywhere else, switching is specced as the `Ctrl+A` prefix —
  `Ctrl+A 0–9` reaches ring-0 seats without entering the well
  (`docs/input.md`, 2026-07-16). The table-edge rail rendering at the mouth
  is still open.
- **The rings** (arrows): pure recency river, now four discrete terraces
  instead of a continuous spiral. Newest activity sits in the `HotNow` ring at
  the mouth; everything sinks band by band toward `Horizon` at the throat as
  it idles. Left/Right spin the focused ring so the selected card eases to the
  gate; Up/Down change which ring is focused and dolly the camera to frame it.
  No stability contract at all — it's for *skimming*, not addressing. Rank
  members would also appear in the rings (the rank is an index, not a place).

Effort scales with coldness, as the original design said: keystroke (rank) →
skim (rings) → search (horizon). The change is that the keystroke surface is no
longer *derived from* the skim surface.

## Ring membership becomes explicit (2026-07-05)

The idle-age bands lasted two days as the sorting rule. Live use surfaced what
the ring rebuild had deferred: **placement you can't control isn't an
instrument.** Amy's model, designed and built this session, replaces derived
banding with explicit placement — two hand-curated rings sandwiching two
automatic ones, every ring exactly **10 seats** (digits `0–9` address the
seats of the *focused* ring; jump-and-commit unchanged), the whole well ≤40
cards:

| Ring | Name | Membership |
|---|---|---|
| 0 | ACTIVE | explicit: `p` or visiting a context; append-ordered by `promoted_at` (stable seats); kernel-capped at 10 |
| 1 | RECENT | automatic: the 10 most-recently-active eligible contexts |
| 2 | BUMPED | automatic: LRU overflow from ring 1; concluded contexts compete here, never for ring 1 |
| 3 | DEMOTED | explicit: `d`; ordered `demoted_at` descending |
| — | event horizon | archived ∪ everything past seat 9 of rings 2/3 — **no card entity**, a "+N" count at the throat |

The verbs (keys provisional; the in-well legend — the transient `?` toggle,
`legend.rs` — is their source of truth):

- **`p` promote** — take a ring-0 seat. When the ring is full the explicit
  verb **fails loud** ("active ring full — demote something first"); seats
  never appear or vanish without a hand on them. Visiting a context
  auto-promotes it (the kernel does this in `setLastContext`, which the app
  already calls on every switch — zero app changes), but a *full* ring makes
  the visit-promote skip quietly: the visit itself must succeed. Promoting an
  **archived** context *unarchives* it (Amy, 2026-07-05): the archive is
  memory to drift back from, not trash, and `p` is the resurrection door —
  rare in practice (work starts fresh; memory diffuses into the code), but
  it's the recovery path the Stage-5 search will feed. Only explicit `p`
  resurrects; visits never do.
- **`d` demote** — one step outward per press, kernel-owned ladder:
  promoted → automatic placement; automatic → DEMOTED; already demoted →
  **archived** (single context, no subtree, no latch — past the horizon).
- **`a` archive** — straight past the horizon from anywhere (same unlatched
  single-card semantics; `kj context archive` keeps its latched subtree form).
- **`z` pause** — *suspend activity*, *designed now, gated later*: the
  `paused_at` stamp, wire field, toggle, and dimmed/badged card ship today;
  the behavioral gate (skip hyoushigi wakeups; reject turn-starts loudly) is
  deferred, seams documented on the column.
- **`c` conclude** — unchanged verb, new consequence: concluding clears the
  ring-0 seat (the mux-exit). Archive clears both placement stamps.
- **Sticky**: demoted and concluded contexts are never re-promoted by visits —
  only an explicit `p` brings them back.

What died: the `HOT_NOW`/`THIS_WEEK`/`THIRTY_DAYS` age constants, the
running-forces-hot override, conclude-demotes-one-band, and the
`DEV_SPREAD_RINGS` dev stand-in (its reason — everything hot — is fixed).
Liveness shows as *light* (chatter/beat lanes, rays), never as placement.

Kernel/wire state (all additive): `contexts.promoted_at/demoted_at/paused_at`;
`ContextHandleInfo` `promotedAt @17` / `demotedAt @18` / `pausedAt @19`;
`promoteContext @93` / `demoteContext @94` / `setContextPaused @95` /
`archiveContext @96`; `kj context promote|demote|pause|resume`;
`ACTIVE_RING_CAPACITY = 10` enforced kernel-side (`RING_SLOTS = 10` is the
app-side seat count — keep them in agreement).

What this absorbs from the staged plan: **Stage 1's** banding half (recency
now only *orders* the automatic rings, it no longer defines bands); **Stage
2's** rank core — ring 0 *is* the rank: ≤10, append-ordered, digit-addressed,
kernel-owned (still open from Stage 2: the table-edge rail rendering and
deliberate reorder à la `kj rank move`; digit switching from the conversation
view is specced as `Ctrl+A 0–9` in `docs/input.md`);
and **Stage 5's** cutoff — no-entity-past-the-seats arrived early, while
search-at-the-horizon and the `since` payload valve remain open.

## The bowl, revisited — what mockup 27 still teaches

*(The canonical image is preserved in-repo at `docs/mockups/27-time-well.png`.
The full 01–40 mockup set moved out of the repo with Amy; only the winner is
kept here.)*

The original ring layout was abandoned for the continuous vortex because it
wouldn't land at the time — but the diagnosis mattered: **the rings failed for
population reasons, not geometry reasons.** Fixed-pitch rings wrapped onto
themselves past ~24 cards (issues.md, "fixed-pitch overlap") because nothing
bounded band membership — "hot" was everything unconcluded. The vortex
degraded more gracefully under unbounded load, which is why it "worked a
little better," but it paid for that by dissolving everything 27 got right.
Stage 1's idle-age bands removed the cause (a bounded hot set), and the app
rebuilt the geometry back toward the bowl — **not** the terraced-spiral
middle path this section originally proposed, but a fuller return to mockup
27's ring grammar: one magic-circle ring per idle-age band, receding into the
throat. What 27 taught, now built:

- **Terraced bands, not continuous depth.** The eye finds "THIS WEEK" /
  "MONTHS AGO" / "YEARS OF HISTORY" because band boundaries are *steps*, not a
  smooth gradient — now literally four separate rings at four depths, rather
  than a quantized spiral. Cards seat evenly around their own band's ring
  (perpendicular 3D "slides"), smaller per deeper ring. In-world band
  *labels* are not built — the terrace step/gap and each ring's own size carry
  the boundary today.
- **The mouth stays open.** In 27 the center is calm — sediment glow, no
  cards. The built well keeps a radius floor per ring and reserves the shared
  vortex core for the throat glow, so the axis never fills with cards.
- **The rim is a carousel.** Hot cards stand upright, evenly spaced,
  shoulder-to-shoulder, facing the viewer — a readable instrument panel, not
  weather. This is now true per ring, not just the hot rim: every band's ring
  is a carousel, spinnable to bring any of its cards to the front gate.
- **Lineage drapes down the bowl wall.** The silk threads from rim cards to
  their ancestry below are the fork-lineage grammar made visible — and they
  need no new wire (`forked_from` is already on every context). Built
  2026-07-12 (HUD melt slice 1, `drape.rs`): on-selection draped ribbons down
  the bowl wall; ambient/always-on stays a later taste call.
- **The table edge is the control ring.** 27 puts filters/affordances on the
  physical table rim. The Stage 2 rank rail should *be* that ring — stations
  around the table edge, instrument-styled — rather than a floating HUD strip.
  Still forward-looking.

The stages below absorb these as noted in place; none of them reorder the
plan.

---

## The stages

Each stage is shippable alone, in order. Wire changes are additive throughout.

### Stage 0 — Tourniquet (one sitting)

Flip the hot sort to newest-first and default the selection to the mouth.

- `card.rs:161-162`: hot sorts id-**descending** (flip the
  `band_orders_rank_each_band_by_its_own_axis` expectation).
- Default selection stays index 0, which now = newest open context.
- Digits temporarily mean "newest ten" — acceptable until Stage 2 gives them
  their real meaning.

Acceptance: create a context; it appears at the mouth, selected state reachable
in 0 keystrokes.

### Stage 1 — Kernel truth: activity recency

> **2026-07-05:** the idle-age *banding* half of this stage is superseded by
> explicit placement ("Ring membership becomes explicit" above). The kernel
> truth half — `last_activity_at`, incremental `liveStatus` — stands and is
> what the automatic rings now sort by.

Make "recent" a real, cheap, wire-visible fact.

- **`last_activity_at` on `ContextHandleInfo`** (+ `contexts` column, additive
  migration), maintained on block append/mutation — not derived by scanning.
- While there: make `liveStatus` derivation incremental (cache per-context, bump
  on block events) instead of the every-poll full-scan (`rpc.rs:7328`). The
  5s poll stays; it just stops being O(all blocks).
- Vortex order becomes **last-activity descending**, and the depth zones become
  **idle-age bands** (the original ladder, restored): *hot now* (active today /
  running / rank members) → *this week* (LRU) → *30 days* (LRU) → the event
  horizon. The bands are **pure derivations of `now − last_activity_at`** — no
  expiry daemon, no kernel state changes; a context expires by idling. The
  explicit verbs keep their jobs on top of the decay: `conclude` is the mux-exit
  (removes it from the rank once Stage 2 lands, and demotes past *hot now*
  immediately regardless of recency); `archive` remains the manual hard-hide. This replaces the
  concluded-keyed banding (`assign_band` on `concluded_at`), whose "hot = not
  yet concluded" definition is what let 40+ idle contexts squat the hot arc.
- This alone fixes most of the census problem *without filtering*: idle
  score-contexts, spent verify sessions, and dormant musicians sink naturally;
  whatever is actually moving stays at the mouth. (The musician mass at the
  horizon — the part that already works — is preserved and *earned* now.)
- **App half — landed differently than planned: discrete rings, not a
  terraced spiral.** Where this stage originally proposed quantizing
  depth/radius on the continuous spiral (a step + gap per band boundary,
  spiral ordering continuing within each terrace), what actually shipped is
  four separate magic-circle rings — one per idle-age band
  (`assign_idle_band` over `HotNow|ThisWeek|ThirtyDays|Horizon`), stacked by
  depth on a shared tilted funnel axis, receding into a central vortex/throat
  glow. Cards seat evenly around their band's ring as perpendicular 3D
  "slides," smaller per deeper ring. Navigation became ring-centric
  `(focused_ring, ring_pos)` rather than the odometer walk: Left/Right spin
  the focused ring so the selected card eases to a fixed gate angle; Up/Down
  change the focused ring and dolly the camera to frame it. In-world band
  *labels* were not built. The radius floor that keeps the mouth open
  survived into the ring geometry.

Acceptance (met): touch an old context via kj; its card surfaces at the mouth
on the next poll — and a screenshot shows four legible rings, receding into
the throat, with an open center.

### Stage 2 — The rank (replaces the mux)

> **2026-07-05:** the rank's core landed as **ring 0** ("Ring membership
> becomes explicit" above): kernel-owned, ≤10, append-ordered by
> `promoted_at`, compact-on-demote/conclude, digit-addressed while the ring is
> focused. Still open from this stage: the table-edge rail rendering, digit
> switching from the *conversation* view, and a deliberate-reorder verb.

The `0–9` ordered active list, decoupled from spiral index. Semantics per "Two
navigation surfaces": append-on-join, compact-on-conclude, rule over address.

- `rank: Vec<ContextId>` (≤10 addressable) — **kernel-owned** (KV or a column),
  not app state: the rank must survive app restarts, be visible to kj
  (`kj rank`, `kj rank add|drop|move <ref>`), and be shared by any peer/app.
  Thin client rule applies.
- Digit keys address rank positions everywhere — in the well *and* in the
  conversation view (the well is for browsing; slot-switching shouldn't
  require it). `Ctrl+W` remains the browse verb.
- Membership lifecycle: join appends at the end (start-order); conclude/archive
  removes and **compacts later positions by exactly one** (the memorable rule);
  `kj rank move` exists for deliberate reshuffles but nothing reorders
  implicitly. Which contexts auto-join on creation is rc-configurable per
  context_type — user-created work contexts yes; musician rotation children and
  MCP verify sessions never.
- Render: the rank is the **table-edge ring** (mockup 27's control ring) — 10
  stations around the rim of the well, occupied ones showing a mini-card
  (digit, label, status pulse), empty ones dim. The rail is the "top of
  vortex," styled as instrument hardware, not a floating HUD strip.
- Mixed occupants: shells are contexts already; add them. A system-stats
  surface starts as a rank-member context whose blocks are telemetry
  render-cues (the htop slot) — good enough until a dedicated monitor surface
  exists.

Acceptance: a full day of real work driven from the rank without reaching for
wezterm; across ~20 creations/conclusions, every position change is explained
by the append/compact rule (nothing ever moves "by itself").

### Stage 3 — Tracks on the wire, and in the well

The durable lane layer, per the ontology above.

- **Wire** — ✅ SHIPPED 2026-07-04, leaner than drafted: `TrackInfo` (id,
  scoreContextId, playing, playheadTick, periodUs, beatsPerPhrase, beatCount,
  lastEpochNs, clockKind, attached ids) + `listTracks @92`; `trackId @16` on
  `ContextHandleInfo` (empty = unattached). No label/description/createdAt —
  the `tracks` table has no such columns; the TrackId name *is* the label
  (add columns + fields when a track needs prose). Answered from the
  scheduler's in-memory `TrackState` via `BeatRequest::Snapshot`, never the
  lagging persisted row.
- **Kernel**: `Attachment.wakeup` becomes optional (pure membership);
  track create/label/describe via `kj track` (transport verbs stay under
  `kj transport`).
- **Well**: within each vortex zone, group by track using the
  `haystack_order_keys` pattern (track-adjacent, unattached trailing), each
  track run led by a **track header chip**. The 16 rotation-children collapse
  into their track's deck ("bassline ×16 · 1 running") — the aggregation
  grammar from mockup 37, applied where the census says it's needed most.
- Score contexts (`context_type=score`) stop rendering as peer cards; they are
  equipment, shown on their track's chip/detail instead.

Acceptance: the well shows ~6 track decks + a handful of loose contexts instead
of 56 cards; the musician swarm reads as one deck that still spills its mass
toward the horizon.

### Stage 4 — The progression: track → context → detail → in

The two-level navigation rule from the concept doc, made the primary grammar.

- **Level 0 (mouth/overview)**: the rank rail + active track decks + loose
  (unattached) contexts. Enter on a track dives into it.
- **Level 1 (track dive)**: that track's contexts re-lay-out on the well
  (recency spiral scoped to the track — same code path, filtered input). This
  is the "dive-in re-layout" the substrate doc deferred; the scoped spiral *is*
  the freer layout, no new algorithm needed.
- **Level 2 (context detail)**: the existing focus/reading card (unchanged;
  it carries the retired edge HUD's reference content since the HUD melt).
- **Level 3**: second Enter commits into the conversation (unchanged).
- Esc walks back up one level each press. Skim test: mouth → any context's
  detail in ≤4 keystrokes, every hop animated fast enough to not break flow.
- Track detail for clocked tracks shows transport state (playhead, tempo,
  attached musicians) — the first tracks-viz integration. A richer
  transport/score scene can become its own view later; the well's job is only
  to *reach* it fast.
- **Lineage drapes** (27's silk threads): selecting a card draws draped curves
  down the bowl wall to its ancestors on colder terraces — the fork-lineage
  grammar made spatial. Shipped 2026-07-12 (HUD melt slice 1, `drape.rs`); no
  wire change (`forked_from` + the existing `ancestors` walk). On-selection
  only; ambient/dim always-on stays a Stage 6 taste call.
- Focus presentation: the current in-world focus card at the well's mouth vs
  27's expand-in-place on the ring — Amy's call when the terraces exist to
  judge it against (flagged, not decided).

Acceptance: skim all active work (every track, every hot context's preview) in
under 30 seconds without touching the mouse.

### Stage 5 — The event horizon becomes real

> **2026-07-05:** the cutoff arrived early — contexts past their ring's 10
> seats (and archived ones) get no card entity, and the throat shows a "+N"
> count. Still this stage's to-do: search at the horizon (`/`), the
> `listContexts` `since` valve, and LOD chips for the seated-but-deep cards.

Old contexts stop being geometry and become an archive.

- **Cutoff**: past the 30-day idle band (and for archived contexts), contexts
  get **no card entity**. They render as the horizon's accretion mass —
  per-track sediment arcs feeding the existing ring deck (counts, not cards).
  The `Join` already diffs enter/exit; the cutoff is a filter on which ids get
  entities. Archived stops meaning "vanished": it means "past the horizon,
  search-only" — same recovery surface as aged-out contexts.
- **Scaling valve**: once the horizon is real, the 5s poll shouldn't ship the
  whole history — add a `since` (activity-window) filter to `listContexts` so
  the steady-state payload is the visible well only; the archive is fetched by
  the search form on demand. Additive wire change, deferrable until payload
  size actually hurts.
- **LOD between mouth and horizon**: mid-river cards drop to instanced chips
  (`MeshTag` + shared material, per the appendix's validated rendering notes) —
  fixes the throat overlap and the 50-entity texture cost.
- **Search at the horizon**: `/` opens the archive form — query over
  label/keywords/`search_similar`/`get_clusters` (the RPCs exist and are
  already client-exposed), results as a flat legible list, Enter
  resurrects (select / fork / `rank add`). Archived contexts become reachable
  here instead of hard-vanishing.

Acceptance: 200+ context history renders at full frame rate with ≤30 card
entities; any archived context findable by a word of its label in <5s.

### Stage 6 — Professional polish

The "not another JavaScript thing" pass, once the structure is honest.

- Mouse story: `MeshPickingPlugin` observers (click select, double-click
  commit), hover previews; the rank rail clickable.
- Motion identity: easing pass over dive/back-out, ripple/ring balance, bloom
  budget; deliberate type scale on cards vs chips vs headers.
- A semantic accent palette (context_type/track → stable hue map) replacing the
  FNV-hash-to-HSL placeholder (`scene.rs:733-742`).
- Perf: poll costs (5s/8s cadences), texture rebuild audit, throat instancing.
- Empty/degraded states: fresh kernel, no tracks, kernel restart mid-view.

---

## Doc + code hygiene

- ✅ `docs/viz-substrate.md`: retired (2026-07-04, `git rm`). Its still-true
  content is the "Substrate notes" appendix below; the superseded sections
  (three-band layout, build order, reading-slot nav, continuous spiral) are
  git-history only. The code comments that cited `docs/viz-substrate.md`
  (`time_well/mod.rs`, `card.rs`, `sync.rs`; `kaijutsu-viz` `join.rs`,
  `scales.rs`, `lib.rs`) were repointed here 2026-07-04.
- ✅ `CompactingBandLayout` + its config/tests — **deleted** from
  `kaijutsu-viz` (2026-07-03; decision recorded above). `RadialBands`/scales
  stay.
- ✅ `docs/time-well-concepts.md`: has a pointer to this doc at top; the
  `mockups/context-browser/` reference is fixed — the full set moved out of
  the repo, the canonical 27 preserved at `docs/mockups/27-time-well.png`.

## Execution notes — details easy to miss

Global conventions are in `CLAUDE.md`; judgment calls on feel/aesthetics are
Amy's (the Stage 2 rank earns its verdict by her driving it for a real day;
Stage 6 is entirely her bar). Everything else here is just facts an executor
won't discover until they hurt:

- **Live verify**: the runner (`./contrib/kj status|tail|rebuild|restart`) +
  BRP; Ctrl+W enters the well via `brp_extras_send_keys`; screenshot often.
  The kernel is a systemd user service — restart to load changes, then
  re-attach the MCP session (stateless-deterministic, not flaky). `kj` is the
  kernel builtin; `contrib/kj` is only the runner control script.
- **Wire changes** (Stages 1, 3, 5): `kaijutsu.capnp` appends at the next free
  ordinal (check the current max, never renumber) → `kaijutsu-types` →
  server → client parse → app. SQLite migrations are additive `ALTER TABLE`;
  at-rest serde is versioned CBOR (additive, fail-loud). Never touch
  `kernel.db` directly.
- **Review**: kaibo per stage, whole files, no diff — gemini-pro *batch* +
  a deepseek consult over the same surface. Defer unapplied findings to
  `issues.md`.
- **Parked on purpose** (don't pick up opportunistically): drift arcs/particle
  layer (needs a context→context edge-list wire), the JOIN dive-through
  camera, ViewSpec extraction. The conversation view's `Screen`-transition
  formalization (appendix, "ViewSpec + the kj→app seam") is a known precursor to the
  Stage 2 digit-key hook — do the minimum.
- **Landmines**: Bevy 0.18 renames (trust the `CLAUDE.md` table over training
  memory). BRP `send_keys` ships named keys with `logical_key=Unidentified`
  (keyconv has a `key_code` fallback). `VelloFont::layout` drops the brush —
  pass it explicitly or text renders black. Keep the card change-guard
  discipline (`sync.rs` flag-flips) or textures rebuild every frame.
  `kernel_id` persists across kernel restarts — never use it to detect one.
  UUIDv7 short prefixes collide under burst creation (the census hit this) —
  use labels or full ids in scripts/tests. Musician/player contexts must stay
  tool-free (small models hang with a full palette) — relevant if Stage 3
  touches attachment/rc plumbing.

## Open questions

1. **Rank scope** — one global rank, or per-track ranks with the global rank
   holding tracks? Start global (matches the 9-terminal reality); revisit when
   track count grows past what ten slots serve.
2. ~~**Auto-join policy**~~ — RESOLVED (Amy, 2026-07-05): **visits promote**.
   The kernel auto-promotes on `setLastContext` (the app reports every switch),
   so the ring-0 row builds itself from where you've actually sat; demoted and
   concluded contexts are sticky (only explicit `p` returns them), and
   synthetic churn never visits, so it never joins. No per-context_type policy
   needed yet.
3. **Conclude hygiene for synthetic churn** — mostly dissolved by the idle-age
   bands (Stage 1): expiry is a pure derivation, so idle verify/mcp sessions
   sink on their own with no daemon. The residual question is only whether
   anything should *auto-conclude* for rank purposes (a disconnected mcp
   session can't be holding a slot it never had, so probably not) and whether
   30-days-idle should eventually auto-set `archived_at` to bound the
   `listContexts` payload — deferred until the `since` filter (Stage 5) decides
   whether it's even needed.
4. **Stats surface** — is a telemetry-fed context enough for the htop slot, or
   does the well eventually want an ambient stats readout somewhere in-scene
   (the edge HUD it would once have ridden is retired)? Defer until the rank
   exists and the itch is real.
5. ~~**Track ↔ hyoushigi naming**~~ — RESOLVED (Amy, 2026-07-03): track is the
   general concept; every track has a clock slot, noop for non-musical lanes,
   with a coder clock expected to develop over time (or a musical clock used
   for fun). One noun, no rename.

---

## Appendix: substrate notes (folded from `docs/viz-substrate.md`, retired 2026-07-04)

The engineering substrate under the well — `kaijutsu-viz` + the
`view/time_well/` code. Compressed from the retired doc; its superseded halves
(three-band thesis, build order, reading-slot nav, continuous spiral) are in
git history only.

### Stance

Procedural, not framework-y. Port the ideas from D3 that ECS lacks; delete the
ones the engine already gives us (frame loop, camera, depth, shaders, picking).
A substrate to build views *on*, not a charting library — no abstraction until
a second consumer exists (the crate's standing rule).

### D3, decomposed — what was ported, what wasn't

- **Data-join — ported; the foundation.** `Join<K,V>` in `kaijutsu-viz`: keyed
  data in, entity diff out (enter/update/exit). Key on `ContextId`, never an
  array index — stable keys are what keep transitions coherent. The two
  cadences are *structural*, not a caller convention: the **data tick**
  (`update`/`touch`, event-driven, cheap, must never relayout) vs the **layout
  tick** (`enter`/`exit`, poll-driven, `needs_relayout()`).
- **Scales — ported.** Pure functions, domain → range, **invertible** (invert
  is the picking/hit-test path); invert round-trip proptests. `RadialBands`
  stays in the crate but the ring path doesn't use it — the app divides its own
  radius/depth envelope directly.
- **Transitions — not ported.** D3 transitions exist because the DOM has no
  frame loop. Hand-roll on `bevy_math::curve` (`EasingCurve` +
  `sample_clamped(t)` driven by a tiny `Tween` component, ~15 lines). Caveat:
  material **alpha** lives in `Assets<Material>`, not on the entity —
  position/scale via `Transform` are free. Never a build step; they run
  throughout.
- **Layouts — one concrete algorithm at a time, pure & stateless.** Position
  derives from a stable `order_key` at fixed pitch; no persisted slot table
  (`LayoutState` was drafted and disproved). The motion invariants: *append*
  moves nothing; *conclude* shifts later slots by exactly one. The bar is
  **"predictable motion, not zero motion"** — motion must be rule-governed and
  memorable; a count-relative reflow of a live band stays banned (non-local,
  not rule-memorable). A `Layout` trait waits for the second layout.

### Data flow — grounded in the wire surface

- **Layout tick = poll.** There are **no context-lifecycle push events** in the
  schema; the view polls `listContexts` and diffs — the diff *is* the layout
  tick. Topology changes are rare relative to frames; the payload is one struct
  per context, with `forkKind`/`parentId` aboard so lineage is known at enter
  time. `getClusters`/`getNeighbors` feed the cold-band relation layer on the
  same cadence, pull-only.
- **Data tick = subscribe** — *built differently than this bullet originally
  said* (corrected 2026-07-04). `liveStatus` **became** a `ContextHandleInfo`
  field (Stage 1: kernel-derived, O(1) incremental cache, delivered on the
  poll for every card), so the poll — not a subscription — carries the status
  rim. What rides the event stream instead is the sub-poll live layer
  (`view/time_well/live.rs` + `activity.rs`): the app's one **kernel-wide**
  block subscription (no per-visible-context scoping — the well renders all
  contexts anyway) feeds the deck ripples, the per-card chatter/beat lanes,
  and the tail buffers. Scoping the filter to visible contexts remains a
  Stage-5 scaling valve, not a correctness need.
- Two cadences, two kernel surfaces, opposite cost profiles — why the split is
  structural in the `Join`.

### The card

Populated from `ContextHandleInfo` via the join: `label` → title (short-id
fallback), `contextType` → accent, `model`/`provider` → badge, `forkKind` →
fork badge, `keywords` → chips, `topBlockPreview` → preview, `parentId` →
lineage, `trackId` → border hue + the reading card's track line; the live
status glyph rides
the poll (`liveStatus`), while the chatter/beat lanes ride the event stream
(see the corrected data-tick bullet above). Anything that writes
`ContextHandleInfo` is picked up on the next poll — new ways to distinguish
cards are new metadata fields + encoding rules, not new rendering code.

Rendering notes (validated against Bevy 0.18):

- **Instance ≠ entity.** Entities sharing one mesh + material auto-batch into
  one draw; each chip stays its own pickable `Entity` (`MeshPickingPlugin`
  resolves to entities). **Per-instance color breaks the batch** unless it goes
  through `MeshTag(u32)` → storage buffer sampled in the shader; `Transform`
  varies free while batched. Billboarding is a manual `looking_at` system.
- **Card material: MSDF text + SDF decor, not vello RTT.** Vello RTT was the
  first-cut expedient but blurs under the focus dolly; the split is
  `block_fx.wgsl`'s own — texture = text content, shader
  (`WellCardMaterial`/`well_card.wgsl`) = border/glow/rings/pulse, because
  baked pixels can't animate cheaply and shader uniforms can. Slice 1 (the
  material) shipped; migrating card *text* onto MSDF glyphs is unfinished
  (glyphs are atlas-async — decor-only for a frame or two is fine).
- **Glow = HDR + Bloom on the one shared `Camera3d`** (`Hdr` +
  `Bloom::NATURAL` + `TonyMcMapface`; the UI pass runs after tonemapping, so
  the conversation view is untouched). The tiering rule: **bright = live
  action** — selection rims/status pulses go HDR (>1.0) so they bloom; passive
  structural state (drift shimmer) stays LDR. The activity ring deck rides the
  kernel-wide `ServerEvent` stream (zero new wire), each event rippling at the
  producing context's ring angle (`activity.rs`, pure + unit-tested).

### Data-model gaps — wire additions still open

Gap 0 (`conclude` + `concludedAt` + `ContextState::Concluded`) shipped long
ago. Still open: **block/message count** (only if card size should encode
conversation length; one `UInt64` field or a count RPC); **fork density**
(derivable client-side by folding over `parentId` in the poll — no wire
change); **context-level live status** (defer until an overview needs
"streaming" on a context whose blocks aren't subscribed); **drift edges,
context → context** — the arcs/particles-between-cards layer needs an edge
*list* (active + historical) that neither `driftQueue` nor per-block Drift
snapshots give; the per-card shimmer shipped without it.

### ViewSpec + the kj→app seam

`ViewSpec { query, layout, encodings }` (so `kj view …` spawns views) stays
deferred until it can be *extracted from two real consumers* — it was the part
most at risk of being over-built. The deferral is safe because the transport
already exists: `invoke_peer` / `PeerCommands.invoke(action, params)` is live
(proven by `switch_context`), so a future `kj view` is purely additive — a
handler, a `dispatch_peer_action` arm, a message, a `Screen` variant; no
wire-schema change. One precursor: `Screen` has only `Conversation`, and
context switches don't drive `Screen` — formalize that linkage when the second
screen lands.

### Dependency notes (verified June 2026)

- **Scales: our own** (~200 lines; band-invert is a bisect, time-`nice` a few
  extra lines). No maintained d3-scale crate worth a dependency. **Tweening:
  `bevy_math::curve`**, no crate (`bevy_tweening 0.15` is the fallback if
  declarative sequences are ever wanted).
- **`fjadra` 0.2.1 — adopt *if* force ever lands**, don't reimplement
  (`Simulation` in a `Resource`, `step()` per frame, never `iter()` in a frame
  loop). The well is deterministic geometry; this may never be needed. Borrowed
  patterns if it does: egui_graphs' stateless-algorithm / persisted-state
  split; Rerun `re_view_graph` — persistent sim, re-heat alpha on structural
  change, seed new nodes near their neighbours.
- **Layout output stays dependency-free 2D `f32`** (`LayoutPos`); the app lifts
  to `glam::Vec3` at its boundary — keeps `kaijutsu-viz` zero-dep and dodges
  lockstep with Bevy's pinned `glam` (the tree already carries two `glam`s).
  `kurbo` is card *rasterization*, wrong layer for scene coordinates.

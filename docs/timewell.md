# Time Well — Evolution Plan

2026-07-03. The vortex context browser (`crates/kaijutsu-app/src/view/time_well/`)
works as a prototype: the spiral reads well, the musician swarm falling toward the
event horizon is the right feeling, keyboard nav and the two-stage Enter are sound.
But at 56 live contexts the assumptions it was built on have broken, and its real
job has come into focus. This doc is the forward plan: what the time well is *for*,
the ontology it navigates, and the staged evolution from fundamental fixes to a
professional-feeling instrument.

**Relationship to the other docs.** `docs/time-well-concepts.md` stays as the UX
design record (the 40 mockups, the visual grammar). `docs/viz-substrate.md` stays
as the substrate reference (join / scales / data flow / wire gaps), but its build
order and its three-band layout thesis are superseded — first by its own step 7.8
vortex addendum, now by this plan. New open questions land here, not there.

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

- **The "new stuff at the bottom" glitch.** Hot contexts sort **id-ascending =
  oldest-first** (`card.rs:161-162`); a new context (largest UUIDv7) lands at the
  *end* of the hot run — deepest in the funnel, smallest, furthest from the
  default selection (index 0 = the *oldest* open context, `sync.rs:99-101`).
  Digits `0–9` address spiral indices 0–9 (`scene.rs:355-393`) — i.e. the ten
  *oldest* open contexts. Every part of that is backwards for the mux use case.
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
- **Tracks have zero wire surface.** `TrackState` lives only inside the server
  process (`kaijutsu-server/src/beat.rs:86-159`; `tracks` + `attachments` tables
  at `kernel_db.rs:630-658`). No capnp `TrackInfo`, no `listTracks`, no way for
  the app to know tracks exist. Today's track is purely a **clock domain** —
  attachment *requires* a wakeup cadence.
- **Solid and worth keeping:** the pure/tested band → slot-order → flatten →
  index pipeline (`card.rs`), the `Join` reconcile, the odometer walk + two-stage
  Enter, the edge HUD, the activity-ring pulse, `haystack_order_keys` as a
  grouping primitive. The prototype's bones are good.
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

The prototype conflates the digit keys with spiral position — that's the root of
the muscle-memory failure. They become two different instruments:

- **The rank** (digits `0–9`): an ordered active list with mux semantics —
  **rule-governed, not frozen**. Amy's real usage (2026-07-03): she adapts to a
  recent set — kaijutsu is usually `ctrl-a 2` *because she starts it first* —
  and the layout changes constantly. So the contract is predictability, not
  permanence: a context joins the rank **appended at the end** (start-order,
  the "I started it second, it's 2" rule), holds its position while it lives,
  and **conclude compacts** the later slots by exactly one. That's the whole
  rule; muscle memory rides the rule, not fixed addresses. (This is the
  "predictable motion, not zero motion" principle from viz-substrate, applied
  to the keyboard surface too.) Pinning is deferred until the itch is real.
  The rank is the keyboard surface, curated by *membership* — shells, work
  contexts, a stats surface. Rendered as a fixed rail at the mouth of the well
  (and later, mirrored in the conversation view's dock so switching doesn't
  require entering the well).
- **The vortex** (arrows, the spiral): pure recency river. Newest activity at
  the mouth, everything sinks toward the horizon as it idles. No stability
  contract at all — it's for *skimming*, not addressing. Rank members also
  appear in the river (the rank is an index, not a place).

Effort scales with coldness, as the original design said: keystroke (rank) →
skim (vortex) → search (horizon). The change is that the keystroke surface is no
longer *derived from* the skim surface.

## The bowl, revisited — what mockup 27 still teaches

*(The canonical image is preserved in-repo at `docs/mockups/27-time-well.png`.
The full 01–40 mockup set moved out of the repo with Amy; only the winner is
kept here.)*

The original ring layout was abandoned for the continuous vortex because it
wouldn't land — but the diagnosis matters: **the rings failed for population
reasons, not geometry reasons.** Fixed-pitch rings wrapped onto themselves past
~24 cards (issues.md, "fixed-pitch overlap") because nothing bounded band
membership — "hot" was everything unconcluded. The vortex degrades more
gracefully under unbounded load, which is why it "worked a little better," but
it pays for that by dissolving everything 27 got right. This plan removes the
cause (idle-age bands bound the hot set; track decks aggregate churn; LOD +
horizon cap the tail), so the geometry can converge back toward the bowl.
What 27 teaches, concretely:

- **Terraced bands, not continuous depth.** The eye finds "THIS WEEK" /
  "MONTHS AGO" / "YEARS OF HISTORY" because band boundaries are *steps*, not a
  smooth gradient. Middle path that keeps the vortex's strengths: **quantize
  depth/radius by idle-age band** (a visible step + gap between terraces) while
  the spiral ordering continues *within* each terrace. Append-stability and the
  odometer walk survive unchanged; the boundaries become legible. In-world band
  labels ride the terrace edges.
- **The mouth stays open.** In 27 the center is calm — sediment glow, no cards.
  The live vortex piles cards into the axis. Visual invariant from Stage 1 on:
  cards keep a radius floor; the core is reserved for the ring deck / accretion
  glow (fully honest once the Stage 5 cutoff stops rendering the tail).
- **The rim is a carousel.** Hot cards stand upright, evenly spaced,
  shoulder-to-shoulder, facing the viewer — a readable instrument panel, not
  weather. Bounded hot membership is what makes this possible at all.
- **Lineage drapes down the bowl wall.** The silk threads from rim cards to
  their ancestry below are the fork-lineage grammar made visible — and they
  need no new wire (`forked_from` is already on every context). The current
  selection-highlight rings are a pale substitute; draped curves (on-selection
  first, ambient later) belong in the plan.
- **The table edge is the control ring.** 27 puts filters/affordances on the
  physical table rim. The Stage 2 rank rail should *be* that ring — stations
  around the table edge, instrument-styled — rather than a floating HUD strip.

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
- **App half — terrace the vortex** (per "The bowl, revisited"): quantize
  depth/radius by idle-age band — a visible step + gap at each band boundary,
  spiral ordering continuing within each terrace — and float in-world band
  labels at the terrace edges. Keep the radius floor that leaves the mouth
  open. Geometry-only change (`card::spiral_local` keyed on band + within-band
  index); ordering, odometer nav, and append-stability untouched.

Acceptance: touch an old context via kj; its card surfaces at the mouth on the
next poll — and a screenshot shows four legible terraces, labeled, with an
open center.

### Stage 2 — The rank (replaces the mux)

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

- **Wire**: `TrackInfo` struct (id, label, description, createdAt, clockKind,
  playing/playhead when clocked, scoreContextId, attached context ids) +
  `listTracks` RPC. `trackId: Option<Text>` lands on `ContextHandleInfo` (a
  context surfaces its primary attachment).
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
- **Level 2 (context detail)**: the existing focus card + edge HUD (unchanged).
- **Level 3**: second Enter commits into the conversation (unchanged).
- Esc walks back up one level each press. Skim test: mouth → any context's
  detail in ≤4 keystrokes, every hop animated fast enough to not break flow.
- Track detail for clocked tracks shows transport state (playhead, tempo,
  attached musicians) — the first tracks-viz integration. A richer
  transport/score scene can become its own view later; the well's job is only
  to *reach* it fast.
- **Lineage drapes** (27's silk threads): selecting a card draws draped curves
  down the bowl wall to its ancestors on colder terraces — the fork-lineage
  grammar made spatial instead of the current highlight-ring substitute. No
  wire change (`forked_from` + the existing `ancestors` walk). On-selection
  first; ambient/dim always-on is a Stage 6 taste call.
- Focus presentation: the current in-world focus card at the well's mouth vs
  27's expand-in-place on the ring — Amy's call when the terraces exist to
  judge it against (flagged, not decided).

Acceptance: skim all active work (every track, every hot context's preview) in
under 30 seconds without touching the mouse.

### Stage 5 — The event horizon becomes real

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
  (`MeshTag` + shared material, per the substrate doc's validated plan) —
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
  budget; south HUD slot (preview) ships; deliberate type scale on cards vs
  chips vs headers.
- A semantic accent palette (context_type/track → stable hue map) replacing the
  FNV-hash-to-HSL placeholder (`scene.rs:733-742`).
- Perf: poll costs (5s/8s cadences), texture rebuild audit, throat instancing.
- Empty/degraded states: fresh kernel, no tracks, kernel restart mid-view.

---

## Doc + code hygiene (do alongside Stage 0)

- Trim `docs/viz-substrate.md`: mark the three-band layout section and build
  order as superseded (pointer here); keep D3 decomposition, data flow, wire
  gaps, dependency notes as the substrate reference.
- Delete `CompactingBandLayout` + its config/tests from `kaijutsu-viz`
  (decision recorded above).
- `docs/time-well-concepts.md`: add a one-line pointer to this doc at top, and
  fix the stale `mockups/context-browser/` reference — the full set moved out
  of the repo; the canonical 27 is preserved at `docs/mockups/27-time-well.png`.

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
  formalization (viz-substrate "ViewSpec" section) is a known precursor to the
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
2. **Auto-join policy** — which context_types auto-join the rank on creation?
   Proposed: rc-configurable, default *only* explicitly-created user contexts
   (never rotation children, never verify sessions spawned over MCP).
3. **Conclude hygiene for synthetic churn** — mostly dissolved by the idle-age
   bands (Stage 1): expiry is a pure derivation, so idle verify/mcp sessions
   sink on their own with no daemon. The residual question is only whether
   anything should *auto-conclude* for rank purposes (a disconnected mcp
   session can't be holding a slot it never had, so probably not) and whether
   30-days-idle should eventually auto-set `archived_at` to bound the
   `listContexts` payload — deferred until the `since` filter (Stage 5) decides
   whether it's even needed.
4. **Stats surface** — is a telemetry-fed context enough for the htop slot, or
   does the well eventually want an ambient stats strip in the HUD? Defer until
   the rank exists and the itch is real.
5. ~~**Track ↔ hyoushigi naming**~~ — RESOLVED (Amy, 2026-07-03): track is the
   general concept; every track has a clock slot, noop for non-musical lanes,
   with a coder clock expected to develop over time (or a musical clock used
   for fun). One noun, no rename.

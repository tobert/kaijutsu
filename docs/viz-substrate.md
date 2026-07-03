# Viz Substrate — Design

A small, procedural, D3-inspired layer for building bespoke data-driven views in
Bevy 0.18. First and only committed consumer: the **time-well context browser**
(see `docs/time-well-concepts.md` for the UX direction and the mockup record).
This document is the engineering substrate *under* that UX — what code primitives
produce the well — and supersedes the exploratory `d3.md`.

**Relationship to time-well-concepts.md:** that doc decided *what it looks like
and how you navigate* (40 mockups, "time well" won, two-level navigation is the
load-bearing rule). This doc decides *what code produces it*. Where the two have
disagreed, this doc reconciles them and says so.

---

## Stance

Procedural, not framework-y. Port the ideas from D3 that ECS lacks; delete the
ones the engine already gives us (frame loop, camera, depth, shaders, picking).
This is a substrate to build views *on*, not a charting library. Build only what
the first consumer needs; resist abstraction until a second consumer exists
(see "ViewSpec — deferred" — this is the project's own stated rule:
two concrete implementations before an abstraction).

Estimated size: a few hundred lines for join + scales + layout. The
layout *algorithm* and the card rendering are the real work, and they belong to
the consumer, not the substrate.

---

## D3, decomposed — what to port, what to drop

D3 is four separable ideas. They map onto ECS unevenly.

### 1. The data-join — PORT. The foundation.

Keyed data in, entity diff out. The single most valuable thing to take from D3,
and it maps onto ECS almost for free: Bevy gives us the retained scene graph D3
had to fight the DOM for.

```
join(commands, query, data, key_fn) ->
    enter:  spawn entity for new keys
    update: mutate components for existing keys
    exit:   despawn (or mark for animated exit, then despawn)
```

- **Stable keys are non-negotiable.** Key on `ContextId` (the 16-byte UUIDv7),
  never an array index. Stable keys are what make transitions coherent when the
  dataset shifts.
- **Two cadences — and they map onto distinct kernel surfaces** (see "Data flow"
  below). The join helper must make the distinction *structural*, not a caller
  convention:
  - *data tick*: `update` mutates components on existing entities. Cheap,
    frequent, **event-driven** from block subscriptions. Does NOT invalidate
    layout. Status changing must never relayout.
  - *layout tick*: `enter`/`exit` change the entity set. Triggers relayout.
    **Poll-driven** from a `listContexts` diff. A context being created / forked
    / archived relays out; nothing else does.

### 2. Scales — PORT. Cheap, high-leverage.

Pure functions, domain → range, **invertible** (invert is needed for
picking / hit-test). No good Rust equivalent; trivial to write. This is where
bespoke viz stops feeling ad hoc.

- Need now: `scale_time`, `scale_linear`.
- Need for the well geometry: a **quantized/threshold radial scale** — the 3
  lifecycle bands each get an *equal-width annulus* regardless of how much time
  they span (this is the "history grows denser, not bigger" rule: band 0 gets as
  much radial room as band 2 even though band 2 spans months). This is
  `scale_threshold`-shaped, **not** a linear `scale_radial`.
- `scale_band` (count-relative sibling spread) is needed **only for the dived-in
  subgraph snapshot**, not the live bands — see the layout section.
- Later as wanted: `scale_log`, `scale_sqrt` (area-correct sizing).

Keep them dependency-free and standalone-testable. These are the easiest TDD
target in the whole substrate — pure in, pure out, plus the invert round-trip
property (`invert(scale(x)) ≈ x`) as a test that *will* fail when a scale is
wrong.

### 3. Transitions — DO NOT PORT.

D3 transitions exist because the DOM has no frame loop. We have one, plus easing,
camera animation, depth. Interpolating a card from old to new position is a tween
on `Transform`. Writing a transition system would reimplement what Bevy already
does well.

**Concretely (validated against 0.18):** hand-roll on `bevy_math::curve` —
`EasingCurve::new(start, end, EaseFunction::CubicInOut).sample_clamped(t)` driven by
a small `Tween { start, end, timer }` component, ~15 lines, ~30 easing functions
available. No tween crate needed (`bevy_tweening 0.15` is a 0.18-compatible fallback
if declarative chained sequences are ever wanted). One caveat: material **alpha**
lives in `Assets<Material>`, not on the entity, so opacity tweens either touch the
asset or use a custom material field — position/scale via `Transform` are free.
Transitions are never a build step; they run throughout.

### 4. Layouts — write one algorithm; trait + state both turned out unneeded.

```
// SHIPPED (kaijutsu-viz::layout): one concrete layout, pure & stateless.
fn compute(&self, entries: &[ContextEntry<Id>]) -> BTreeMap<Id, LayoutPos>;
```

Decoupled from rendering and from the join. The join writes layout output to
`Transform`s; Bevy tweens animate the delta.

**It is pure and stateless — `LayoutState` was not needed** (an earlier draft, by
analogy to egui_graphs, assumed a persisted slot table). The spike disproved that:
each card's angle derives from its **stable `order_key`** (the kernel's CRDT tick /
UUIDv7, unique within a band) at a **fixed angular pitch**, so re-deriving from the
current set on every tick is *not* the count-relative reflow we banned — the
`order_key` already carries the durable per-context identity a slot table would
have held. The two motion invariants then hold on a pure function:
*append* (a context with the max `order_key`) lands at the growing edge and moves
nothing; *conclude* (removal) shifts only later-in-band cards by exactly one pitch.
(Precondition: `order_key` unique per band — kernel-guaranteed. Ties would make
the stable sort depend on input order; that's the one thing state would buy, and we
don't need it.)

**No `Layout` trait yet.** There is one concrete `CompactingBandLayout`. Per the
crate's own rule (two implementations before an abstraction — same reasoning as
ViewSpec), the trait waits for the second layout (the dive-in / freer tree layout).

**Output is dependency-free 2D.** `LayoutPos { band, x, y }` (`f32`); the Bevy app
lifts it to `glam::Vec3` at its boundary (mapping band → well depth). This keeps
kaijutsu-viz zero-dependency and dodges lockstep with Bevy's pinned `glam`
(0.30.10; the tree already carries a second `glam` 0.31 — proof of the hazard).
`kurbo` (2D/f64, already present via vello) is for card *rasterization*, not 3D
scene coordinates — wrong layer.

The *which algorithm* question is settled — see "The layout — DECIDED: three
lifecycle bands" below.

---

## Data flow — grounded in the wire surface

This is the part `d3.md` left as an open question; `kaijutsu.capnp` answers it.

### Layout tick (topology) — poll `listContexts`

There are **no context-lifecycle push events** in the schema. Context
create / fork / archive do not emit a callback. The view therefore polls
`Kernel.listContexts @15 -> List(ContextHandleInfo)` and diffs against its last
snapshot to drive `enter` / `exit`. This is fine: topology changes are rare
relative to frames, the payload is small (one struct per context, no blocks), and
the diff *is* the layout tick. `forkKind` and `parentId` arrive in the same
struct, so lineage is known at enter time.

`getClusters @64` (semantic clusters) and `getNeighbors @63` feed the haystack's
relation layer on the same cadence — they change slowly and are pull-only.

### Data tick (live status) — subscribe to block events

Live status (streaming / waiting / error, token activity) is **not** a field on
`ContextHandleInfo`; it must be inferred from block activity. The wire gives us
exactly the right tool: `subscribeBlocksFiltered @67 (callback, filter, instance)`
with a `BlockEventFilter` constrained to the *currently visible* contexts'
`contextIds`. Relevant `BlockFlowKind`s: `statusChanged` (a model block goes
running → done/error → maps to the card's status glyph), `inserted` (activity /
token pulse), `metadataChanged`. The filter means we only pay for status on cards
that are on screen — the rim, not the deep core.

**Mapping to the two cadences:**

| Cadence | Source | Triggers | Cost profile |
|---------|--------|----------|--------------|
| Layout tick | poll `listContexts` diff | `enter`/`exit`, relayout | infrequent, whole-set |
| Data tick | `subscribeBlocksFiltered` on visible ids | `update` (status, pulse) | frequent, scoped to rim |

This is why the two-cadence split must be *structural* in the join: they are
literally fed by two different kernel surfaces with opposite cost profiles.

---

## The layout — DECIDED: three lifecycle bands

> **SUPERSEDED — see `docs/timewell.md`.** The time-well plan's own words: "its
> build order and its three-band layout thesis are superseded." The 7.8 spiral
> replaced the ring layout in code (`CompactingBandLayout` deleted 2026-07-03),
> and `docs/timewell.md` ("Where we are") replaces the three-band thesis below
> with the rank/vortex split + idle-age bands. Left in place as substrate
> history — the D3 decomposition, data flow, wire gaps, and dependency notes
> elsewhere in this doc still apply.

The old framing (layered-DAG vs. one-radial; `d3.md` vs. mockups) turned out to be
the wrong axis. The decision was settled by grounding it in the target workflow —
4–5 concurrent agent sessions, 10–20+ contexts/day, worked like terminal-mux
windows. The result is **one radial well with three bands**, where the bands are
**lifecycle stages, not clock buckets**, and the two coordinates carry orthogonal
meaning:

- **Radius = lifecycle band** (3 discrete bands; a context migrates *inward* as it
  ages out of active use).
- **Angle = position within the band** (what "position" means differs per band —
  see below).

This dissolves the old "active view vs. haystack view" split: they are not two
views, they are two radii of one well. The haystack *is* the inner band.

### The three bands

| Band | Stage | Angle encodes | Representation / LOD | Reach |
|------|-------|---------------|----------------------|-------|
| 0 (rim) | **hot** — open, not concluded | **position** in a flat compacting list | full cards (entities + RTT text) | keyboard `ctrl-a 0–9` |
| 1 (mid) | **recent-concluded** — last N=10 | **recency** (a clock of "what I just finished") | chips / role-decks (instances) | clickops |
| 2 (core) | **haystack** — aged past N | **semantic cluster** (`getClusters`/`searchSimilar`) | sediment / particle cloud | search / clickops |

**Band 0 is a terminal-mux window list.** New context appends to the end
(`ctrl-a c`); `conclude` removes it and the slots after it **compact** to fill
(`exit`). It is the *only* keyboard surface — `ctrl-a 0–9` addresses the open work,
capped at 10 by the digit keys (an 11th open context simply isn't hotkey-
addressable; no error). Lineage/fork structure is **not** the band-0 angular
encoding — it is an on-demand overlay (select a card, ancestry lights up). The
fork-heavy multi-agent case (1 driver + 16 workers) is a **dive-in**, not the
default rim.

**Band 1 holds the last 10 concluded**, recency-ordered, also compacting: a new
conclusion pushes onto the warm end; the 11th-oldest falls into the haystack. This
is kaijutsu's improvement over a mux, where `exit` destroys the window — here
concluded ≠ gone. Clickops only; no muscle-memory depends on band-1 positions, so
it is free to reorder/cluster however reads best.

**Band 2 is the haystack** — semantic, search-driven, recovery-is-rare-and-not-
first-class. Angle re-encodes to similarity because lineage has stopped mattering
for cold data. This is where the embedding RPCs live.

### The principle: predictable motion, not zero motion

An earlier draft argued for stable slots that *leave gaps* on departure, to protect
spatial memory. The mux workflow corrected this: the real bar is not "nothing
moves," it is **"motion is rule-governed and predictable."** Linear compaction
(everything after the gap shifts toward the front by exactly one) is a *memorable
rule*, so spatial memory survives even though absolute positions change. The
force-layout hairball failed this bar by moving *unpredictably* and globally — not
by moving. So:

- **Append** (new context) never moves an existing one. ✓
- **Conclude** (compaction) shifts later slots by exactly one — deterministic. ✓
- A **count-relative** `scale_band` that re-divides the whole ring on every
  enter/exit is still banned: its motion is non-local and not rule-memorable. ✗

This gives a **testable invariant on the `Layout` trait**: an `enter` at the
growing edge agrees with the prior layout on every existing element; an `exit`
produces exactly the one-slot compaction and nothing else. A property test on both
will fail loudly the day a global reflow sneaks in.

The reach cost mirrors the geometry: **effort scales with radius / coldness** —
hot work is one keystroke, recent-concluded is a click, ancient is a search. You
never burn a scarce hotkey on cold data, and never pay search cost for hot work.

### What survives from the earlier analysis

- **Crossing-minimization is still moot:** `ContextHandleInfo` carries a single
  `parentId`, so fork lineage is a **forest of trees**, not a DAG; drift is a
  separate non-structural particle layer. And since band-0 angle is *position*,
  not lineage, layout never even traverses the tree for placement.
- **`scale_band` is not retired** — it moves to the **dived-in subgraph**, a
  *snapshot* that is laid out once and barely churns while viewed. Count-relative
  spread is fine there. Only live bands need the compacting-list discipline.
- **Volume still forces the overview to summarize.** At ~20 contexts/day the
  haystack is hundreds of contexts; even band 1 cycles fast. The overview shows
  roots/drivers + aggregated role-decks, never the full set — which keeps the
  layout tick cheap. The workflow is the strongest argument *for* two-level nav.

---

## The card

A tiny schema, populated from `ContextHandleInfo` via the join. The wire fields
map almost directly:

| Card element | Source field | Notes |
|--------------|--------------|-------|
| title | `label` | falls back to short id if empty |
| accent color | `contextType` (or `provider`) | the rc bucket / mode bundle |
| model badge | `model`, `provider` | |
| fork badge | `forkKind` | "full"/"shallow"/"compact"/"subtree" |
| band (radius) | lifecycle + `concludedAt` | hot / recent-concluded / haystack — see gaps |
| keyword chips | `keywords` | synthesis output, may be empty |
| preview | `topBlockPreview` | |
| lineage | `parentId` | single parent → tree edge |
| **live status glyph** | block events | NOT in the struct — data tick |

Cards are **billboarded** (always face camera) — keeps text readable without
committing to true-3D text layout. LOD tiers map 1:1 to Bevy instancing tiers
(see time-well-concepts.md): rim cards = full entities with RTT text; chips /
decks / sediment = `MeshTag` instances; deep core = particle cloud.

**Rendering notes (validated against Bevy 0.18):**
- *Instance ≠ entity.* Every chip stays its own `Entity`; many entities sharing one
  mesh + material handle auto-batch into a single draw. So instancing is a draw-level
  win that costs nothing in pickability or per-entity state — `MeshPickingPlugin`
  resolves to entities, so each chip is individually pickable (band-1 clickops holds).
- *Per-instance status color is a shader concern, not a component write.* `Transform`
  (position/scale) varies per-entity for free while batched, but per-instance **color
  / pulse** breaks the batch unless it goes through `MeshTag(u32)` → a storage buffer
  sampled in the shader (see `automatic_instancing.wgsl`). So the data tick updates a
  per-instance status *index/value*; the shader maps it to color. Rim cards (full
  entities) don't have this constraint — they tween material normally.
- *Rim-card text* reuses the app's existing `vello_ui_texture` RTT primitive
  (`kaijutsu-app/src/view/vello_ui_texture.rs`, already driving docks): build a
  `vello::Scene`, rasterize to a texture, sample on the card quad.
- *Billboarding is manual* — no built-in component in 0.18; a one-line
  `Transform::looking_at(camera_pos, Vec3::Y)` system per card.

Card manipulation is free: anything that writes `ContextHandleInfo` fields (e.g.
`renameContext @50`, `setContextState @71`, a future badge field) is picked up on
the next layout-tick poll. New ways to distinguish cards = new metadata fields +
encoding rules, not new rendering code.

---

## Data-model gaps — wire additions the consumer will need

The substrate works with what exists, but the *full* time-well grammar needs
fields that aren't on the wire yet. Named here so they're not a surprise:

0. ✅ **The `conclude` verb + lifecycle distinction** — *the load-bearing
   addition, SHIPPED (step 5).* The band-0→band-1 transition is an **explicit,
   intentional** "this context is done" act (the kaijutsu equivalent of
   `exit`-ing a mux window), distinct from a **transient detach** (app restart,
   dropped connection, closed lid) which must **not** demote. Resolved as: (a) a
   `conclude @83` operation; (b) `concludedAt @13` on `ContextHandleInfo`, giving
   band 1 its recency rank; (c) `ContextState::Concluded`, distinct from
   transient-detached (stays `Live`) and from `archived` (hidden). Bands 1/2 are
   a client-side recency split over `concluded_at`; archived contexts are
   filtered out of the well. *(Note: the old claim that `contextLeave @74`
   "marks a context archived on leave" was already stale — `contextLeave` only
   drops the session binding; archive is a separate `archive_context` call.)*
   `conclude` is reversible (fork/recover from the haystack) but deliberately not
   first-class — no prominent un-conclude affordance.
1. **Block / message count** — `ContextHandleInfo` has none. Needed only if card
   *size* should encode conversation length. Smallest addition: one `UInt64`
   field on `ContextHandleInfo`, or a dedicated lightweight count RPC. (Already
   noted as a gap in time-well-concepts.md.)
2. **Fork density** — the "filigree halo encodes how many forks spawned" grammar
   needs a child-count. Derivable client-side by counting contexts whose
   `parentId == this` from the `listContexts` result — **no wire change needed**,
   just a client-side fold over the poll.
3. **Live status as a context-level concept** — currently inferred from block
   events per visible context. Fine for the rim; if overview ever needs
   "streaming" on a context whose blocks aren't subscribed, a context-level
   status field or event would be required. Defer until proven necessary.
4. **Drift edges (context → context)** — partially closed 2026-06-17. The per-card
   **drift shimmer** ("drift = shimmer" bling) shipped using the existing
   `driftQueue @48` (staged) — a card whose context is a staged-drift endpoint
   (source or target) sweeps an HDR sheen (`card::drift_endpoints` +
   `scene::highlight_drift`, no wire change). What still needs the wire is the
   **drift *arcs/particles between* cards** ("drift arcs above the rim"): that
   wants a context-to-context drift edge *list* (active + historical, not just
   pending), which neither `driftQueue` nor per-block `Drift` snapshots give as a
   context-pair list. Still deferred with the drift-particle layer; the per-card
   shimmer is the foundational step that didn't need it.

Gap 0 is the one a real consumer hits first (the well's whole radial axis is
lifecycle); gaps 1 and 4 are further real wire additions; 2 and 3 are client-side
or deferrable. None block the *foundation* (join + scales + layout + card from
existing fields) — but the active view (band 0) needs gap 0 before it means
anything.

---

## ViewSpec — deferred until a second consumer

`d3.md` proposed a declarative `ViewSpec { query, layout, encodings }` so `kj`
could spawn views, with built-in Rust views as well-known specs and
`kj view ...` hitting the same code path. The idea is attractive and fits the
kj/kaish-everything philosophy — the query side would share whatever surface
`kj ctx ls` already exposes over `listContexts`, so "the spiral" and
"`kj ctx ls --since 2w`" become two renderings of one query.

**But it is the part most at risk of being over-built ahead of its second
consumer.** The project's own rule is: bring ≥2 concrete implementations to an
abstraction's design, not as follow-up. So `ViewSpec` is *not* a foundational
step. Build the active view concretely, build the haystack view concretely, then
extract `ViewSpec` from the two real call sites — at which point its shape is
evidence-based instead of guessed. (This reorders `d3.md`'s build list, which
had ViewSpec before the haystack existed.)

**The kj→app seam is ready (validated against current code).** The deferral above
is safe because the transport already exists and building the foundation now won't
preclude it. The `invoke_peer` / `PeerCommands.invoke(action, params)` callback is
live and proven by the app's existing `switch_context` / `active_context` actions
(`kaijutsu-app/src/peers/systems.rs`). A future `kj view <spec>` is purely additive:
a new `kj/view.rs` handler calling `kernel.invoke_peer("kaijutsu-app", "spawn_view",
…)`, a new `"spawn_view"` arm in `dispatch_peer_action`, a `ViewSpawnRequested`
message, and a new `Screen` variant. No wire-schema change (the `invoke` callback is
generic JSON). **One precursor to track, not a blocker:** `Screen` currently has only
`Conversation`, and context switches update `active_id` without driving `Screen`
(the `switch_context_screen_transition` gap). That linkage must be formalized when
the second screen lands — which is exactly when the time-well view ships.

---

## Build order

> **SUPERSEDED — see `docs/timewell.md`.** This numbered step sequence (the
> compacting-band layout, the active/haystack view split) is superseded by the
> timewell plan's staged rollout. Kept for the historical record of what
> actually shipped through step 7.8; do not use it to plan new work.

*Status (June 2026): steps 1–3 shipped in `crates/kaijutsu-viz/` — pure,
dependency-free, TDD, deepseek-reviewed. **Step 4 shipped** in
`crates/kaijutsu-app/src/view/time_well/` (Ctrl+W enters, Esc leaves) — the
first concrete consumer. The remaining steps are the `conclude` wire work
(step 5) and the two concrete views (6–7).*

1. ✅ **Scales** (`ScaleLinear`, `ScaleTime`, `ScaleThreshold` + `RadialBands` —
   the quantized 3-band radial; `scale_band` deferred to the dive-in). Pure,
   invert round-trip proptests.
2. ✅ **Keyed join** — the reconciler core (`Join<K,V>`: `reconcile` enter/update/
   exit value-change-aware + idempotent; `touch` set-preserving data-tick update;
   `needs_relayout()` = the structural two-cadence line). The *wiring* to
   `listContexts` (layout tick) / `subscribeBlocksFiltered` (data tick) is the
   integration step (needs app + client).
3. ✅ **Compacting band layout** (`assign_band` + `CompactingBandLayout::compute`)
   — pure & stateless; the two motion invariants as proptests. No `Layout` trait
   (one impl; trait waits for the dive-in layout).
4. ✅ **Card** schema + billboarding + the join writing `ContextHandleInfo` →
   card components; live status glyph from the block-event data tick. *(First
   Bevy-side step — `LayoutPos` → `glam::Vec3` lift lives here.)* Shipped in
   `view/time_well/`:
   - `card.rs` — pure `ContextInfo`→`CardData` map + band assignment + layout +
     the `Vec3` lift (8 unit tests). `ContextInfo` gained `PartialEq, Eq` so it
     can be a `Join` value.
   - `scene.rs` — `Screen::TimeWell` + a `Camera3d` well (the 3D camera owns the
     background at order 0; the existing 2D UI camera moves to order 1 with a
     transparent clear so the dock hint composites on top). Cards are
     double-sided billboarded quads eased toward a `CardTarget` (exponential
     smoothing — the "transitions are Bevy's job" stance).
   - `sync.rs` — the layout tick rides the **existing `DriftState` poll** of
     `listContexts` (no new poll), reconciles the `Join`, spawns/despawns, and
     writes targets; plus the data tick (`apply_block_status`) tapping the
     existing `ServerEvent::BlockStatusChanged` stream.
   - `text.rs` — a parley-`Layout` → `vello::Scene` glyph encoder + per-card RTT
     texture (the documented vello route, since the app's normal text path is
     MSDF and doesn't fit a free 3D quad). **Gotcha:** `VelloFont::layout` does
     not push the brush onto the layout (MSDF supplies color separately), so the
     brush must be passed explicitly to the glyph draw — reading
     `glyph_run.style().brush` yields parley's default black.

   *Step-4 simplifications to revisit:* lifecycle bands use `archived` as a
   `concluded` proxy until step 5 lands `concludedAt` (single point of change in
   `card.rs::assign_bands`); the status data tick reflects only already-subscribed
   contexts — a `subscribeBlocksFiltered` over the full visible set is the
   follow-up (gap 3). Card sizing/zoom for readability is cosmetic polish.
5. ✅ **`conclude` wire work** (gap 0) — shipped. `conclude @83` RPC +
   `concludedAt @13` on `ContextHandleInfo` + `ContextState::Concluded` +
   `concluded_at` column (additive migration). `conclude` is distinct from
   `archive`: it sets state `Concluded` + stamps `concluded_at` (reversible via
   fork, idempotent), while `archive` hides the context from the well entirely.
   Driven by `kj context conclude <ref>` (alias `done`, ungated/unlatched —
   routine, not destructive) and `KernelHandle::conclude`. The well replaced its
   `archived`-proxy with the real `concluded_at` and filters archived out of the
   snapshot. Verified end-to-end: concluding a context migrates its card from the
   hot rim to the recent-concluded ring. Open Question #1 below is resolved.
6. 🟡 **Active view** = band 0 (hot compacting list) + band 1 (recent-concluded).
   Band-0 keyboard interaction shipped (`scene::well_keyboard`): **plain `0–9`**
   selects the hot card at that slot and switches to it (the tmux-`ctrl-a`
   prefix was dropped — the well is a dedicated full-screen nav surface, so no
   prefix is needed; decided with Amy 2026-06-16); arrows/Tab move the selection,
   Enter switches, `c` concludes the selected (→ `ActorHandle::conclude` →
   `KernelHandle::conclude`), Esc returns. Selection is highlighted by a scale
   pop. Verified e2e: digit-switch changes the active context + exits; `c`
   migrates the selected card hot→recent. *Remaining for step 6:* band-1 angle
   refinement (the "clock" vs newest-first sweep — open question #2); the
   selection highlight could be richer than a scale bump.
7. 🟡 **Haystack view** = band 2 as the second concrete consumer (semantic).
   Shipped this slice:
   - **Client RPC surface** (7a): `search_similar` / `get_neighbors` /
     `get_clusters` on `KernelHandle` + `ActorHandle` (the server impls already
     existed against `kernel.semantic_index`; the client just didn't expose
     them). New `SimilarContext` / `ContextCluster` client types.
   - **Cluster labels are kernel-synthesized** (thin-client rule): a new
     `ContextCluster.label @2` wire field; the index's `clusters()` derives the
     label from members' synthesis keywords (top summed term, alpha-tiebroken)
     and the server copies it to the wire. The app never derives labels.
   - **Band-2 semantic angle** (7b): the well polls `get_clusters` on a coarse
     well-only cadence; haystack `order_key` now groups same-cluster contexts
     angularly adjacent (`card::haystack_order_keys`) instead of creation-id
     order — band-2 angle finally encodes semantic cluster. Empty clusters (no
     semantic index) fall back to id order. Haystack cards render a `◇ <label>`
     footer tag.
   - **On-demand lineage overlay** (7c): selecting a card lights up its
     fork-ancestry (`card::ancestors` walks `forked_from`, cycle-safe) with an
     amber ring distinct from the blue selection ring. No wire change.
   - *Drift shimmer (2026-06-17):* a card whose context is a staged-drift endpoint
     sweeps an animated HDR sheen (`card::drift_endpoints` + `scene::highlight_drift`,
     params.w; `well_card.wgsl`). No wire change. Completes the bling vocabulary.
   - *Remaining for step 7:* the **drift arcs/particle layer *between* cards**
     (still needs the context→context drift-edge *list* wire — gap 4, deferred —
     the per-card shimmer above is the foundational step that didn't); richer
     per-cluster label placement (currently per-card footer, not a cluster arc);
     wiring `search_similar`/`get_neighbors` into an app affordance (the surface
     exists, no consumer yet).
8. **ViewSpec**: extract from the two consumers now that both exist.

`fjadra` (pure-Rust d3-force port) only if a free-form *relational* view proves
necessary — time-well rejected force layout outright, so this may never land.

Transitions are never a build step — Bevy tweens on `Transform`/opacity
throughout.

---

## Navigation & the reading slot (step 7.5)

The first GPU framing exposed two real problems: cards on the rings are too
small to *read*, and the keyboard only moved *within* the hot rim — there was no
way to walk into the colder bands. Pushing the camera closer to enlarge cards
just crops the side cards (tried; rejected). The fix is two coupled additions
that together make the well browsable, decided with Amy 2026-06-17:

**Cross-band navigation.** Selection moves on a 2D grid over the rings, not a
1D hot list:
- **Left / Right / Tab** — move *within* the current band, in its angular slot
  order (the same `order_key` ranking the layout uses, so keyboard order ==
  visual order). Wraps.
- **Up / Down** — *hop bands*. Up warms toward the rim (Haystack → Recent →
  Hot), Down cools toward the core (Hot → Recent → Haystack). Landing slot =
  the nearest index in the target band (clamped), so the selection stays roughly
  where your eye is.
- **`0–9`** — unchanged: a hot-rim quick-jump that selects + switches + exits.
  Muscle memory for "I know which one"; the arrows are the *browse* path.
- **Enter** switches to the selection; **`c`** concludes it; **Esc** leaves.

This needs each band's slot order, not just the hot list. The per-band order is
the single source of truth: `card::band_orders(contexts, bands, cluster_of) ->
[Vec<ContextId>; 3]` (indexed by `Band::index`) produces each band's ids in
angular slot order, and `layout_positions` derives every entry's `order_key`
from its index in that vector — so the keyboard, the angle, and the digit
addressing can never disagree. `TimeWellState.band_order` holds it, rebuilt each
layout tick (it subsumes the old `hot_order`).

**The reading slot.** A reserved **center-bottom detail card** renders the
current selection at a readable size while the rings stay behind it as the
spatial index — the legibility answer that zoom couldn't give. Implementation:
one `ReadingCard` entity parented to the well camera at a fixed lower-center
local offset (so it's HUD-stable regardless of camera motion), a larger quad +
higher-res RTT texture, re-rendered on selection change. To keep one render
path, `text::build_card_scene` is parameterized by `(w, h)` with font sizes/pad
derived as fractions of the dimensions — the rim cards pass the small size, the
reading slot passes a large one, and vello rasterizes both crisply. This is the
keyboard-driven precursor to the eventual "dive through the focused card → it
unfolds into the conversation" JOIN transition (mockup 34); the reading slot is
that focused card, parked rather than dived-through.

**Evolved (2026-06-17):** the flat 2D bottom panel was tried and rejected — it
read as generic web chrome ("a JavaScript thing"). The focus presentation is now
an **in-world 3D card** floating lower-center at the mouth of the well, and a
**two-stage Enter** drives it: from the overview, **Enter focuses** (the camera
dollies head-on into the focus card so it fills the view), **Esc backs out**, and
a **second Enter commits** (switch to the context + leave). The camera also
**leans toward the selection** in the overview (eased). See step 7.5/7.6 in the
build order; the JOIN dive is the committing Enter continuing *through* the card.

---

## Card foundation & shader bling (step 7.6 — decided 2026-06-17)

Cards now have a **per-band recline** (`scene::card_tilt`: hot ~upright, colder
bands tilt back) layered on the billboard so they lie along the funnel slope like
mockup 27. The next foundation work makes the well *look cool* (Amy's bar: not
"another JavaScript thing") — decided direction:

**Drop vello for cards; use MSDF text + SDF shapes in a 3D card material.** Vello
RTT was the step-4 expedient for free 3D quads, but (a) it rasterizes at a fixed
texture resolution → blurry when the camera dollies in on focus, and (b) it adds
nothing a card needs that MSDF+SDF don't. The app already has the pieces:
- **MSDF** (`text/msdf/`) is a generic "positioned glyphs + target texture →
  render" pipeline (`renderer.rs`); the block coupling is only in its *driver*.
  It stays **crisp at any zoom** and shares the atlas/parley path with the rest
  of the app. Cards get an MSDF glyph set (parley layout of their fields, via
  `layout_bridge`) rendered into the card texture.
- **SDF shapes + glow** — the card's accent background, rounded-rect, selection /
  lineage rings, and status pulse move *out of the baked texture* and *into the
  card material* as SDF (the exact split `block_fx.wgsl` already uses for 2D
  blocks: texture = text content, shader = border/glow/animation). Baked pixels
  can't animate cheaply; shader uniforms can.
- **3D card `Material`** — port `assets/shaders/stack_card.wgsl` (holographic edge
  glow, chromatic aberration, time shimmer, LOD, back-face — left from the
  conversation stack; shader intact, Rust `Material` dropped in the app reset)
  into a `WellCardMaterial` (`AsBindGroup`: glow color/intensity, time,
  selection/lineage/status flags, band LOD) + register `MaterialPlugin`; the card
  spawn in `sync.rs` swaps off `StandardMaterial`. Vello stays for SVG/ABC blocks
  (arbitrary vector), just not for cards.

**MSDF migration — de-risked plan (2026-06-17).** The key unlock: the MSDF
render pipeline's extract query is **generic** —
`Query<(&MsdfBlockGlyphs, &BlockRenderMethod, &VelloUiScene, &VelloUiTexture)>`
(`block_render::extract_msdf_blocks`) — *not* coupled to `BlockCell`. So any
entity with those components gets its glyphs rendered into its texture by the
existing render-world pass. **No render-world changes needed.** Slice-1
(`WellCardMaterial`) is shipped. Remaining (main-world only):
1. Add `MsdfBlockGlyphs` + `BlockRenderMethod::Vello` to card entities (rim cards
   in `sync.rs`, focus card in `scene.rs`). `Vello` method = MSDF composites on
   top of the vello content (the bg).
2. Both card-text systems collect glyphs: `text::build_card_scenes` (rim,
   `CARD_TEX`) **and** `text::update_reading_card` (focus, `READING_TEX`). Each
   needs `ResMut<MsdfAtlas>` + `ResMut<FontDataMap>`; for each text field
   (title/model/fork/tail/cluster) lay out with parley (already done), register
   fonts (`font_data_map.register`), and `collect_msdf_glyphs(layout, &[],
   &fallback_brush, (pad, y), atlas)` accumulating into `MsdfBlockGlyphs.glyphs`
   at the field's `(pad, y)` offset (same origin the vello `draw_layout` used).
   The vello scene then draws **only** the decor (accent bg + top bar + status
   dot); text moves to MSDF (crisp at any zoom — fixes the focus-dolly softening).
3. **Ring-move:** drop the selection/lineage rings from the vello decor; draw
   them in `well_card.wgsl` as SDF rounded-rect strokes keyed off the `params`
   uniform (`selected`, `in_lineage`), updated per-card by a small system from
   `Card.selected`/`in_lineage` (mirrors `sync_block_fx`). The accent bg can stay
   vello for now (static content) or move to SDF later.
- *Watch-outs:* glyphs are async — the atlas generates them over a few frames, so
  cards may show decor-only text-blank for ~1–2 frames after a change (fine).
  Two systems now share `MsdfAtlas` (ResMut) → they serialize, no conflict.

**Glow via HDR + Bloom on a single shared camera (2026-06-17).** The app now has
**one always-on `Camera3d`** (`main.rs::setup_camera`, `IsDefaultUiCamera`) with
`Hdr` + `Bloom::NATURAL` + `Tonemapping::TonyMcMapface`. Bevy UI renders on it —
the UI pass runs *after* tonemapping/bloom in the render graph, so the
conversation UI is untouched — and the well repurposes the same camera on enter
(adds the `TimeWellCamera` marker, swaps the clear color) rather than spawning its
own. That dissolved the old two-camera composite (an HDR 3D camera + an LDR
`Camera2d` on one target made the cards vanish; see issues.md) and there is no
`Camera2d` anywhere now. The well cards are 3D meshes in the main pass, so they
bloom. The bling vocabulary (selection = rim, status = pulse, drift = shimmer,
band = LOD/desaturation) is SDF in `well_card.wgsl` driven to **HDR (>1.0)** values
via the `params` uniform so the bright rims/pulses spill into bloom (the
conversation's own glow stays the LDR SDF falloff in `block_fx.wgsl` — it does not
bloom, which is fine). `MeshTag(u32)` per-instance params only if batching matters
at scale (not yet).

---

## The pulse: kernel-activity rings (step 7.7 — 2026-06-17)

The bright/animated vocabulary was being spent on *passive* info (the drift
shimmer). Re-tiered so **bright = live action**: a base **ring deck** behind the
cards renders the well's pulse, driven by the **kernel-wide `ServerEvent` stream
the app already receives** (zero new wire — token streaming weighs most). Idle =
dim/calm; busy = bright rings + faster flow + a faster-spinning core. Each event
fires a **ripple at the producing context's ring angle** (`atan2(card.y, card.x)`)
— activity localizes to *which* conversation is moving. The drift shimmer was
dropped to LDR (no bloom) so it reads as the passive structural state it is.
Code: `view/time_well/activity.rs` (pure energy/ripple model, unit-tested),
`shaders/well_rings_material.rs` + `assets/shaders/well_rings.wgsl`,
`scene::{accumulate_ring_activity, tick_and_sync_rings}`.

## The edge HUD (step 7.7)

Selection detail fans to the **four screen edges** (N identity · E specs · W
lineage · S preview) instead of a panel pulled into the center, so the well's
mouth stays the open browser. The in-world focus card is hidden in the overview
(shown only on Enter-focus). First cut renders with native Bevy `Text`; a
vello/MSDF styling pass + dropping the focus card for good is the follow-up.
Code: `view/time_well/hud.rs` (formatters unit-tested).

## The vortex: one continuous spiral + tipped axis (step 7.8 — 2026-06-17)

Decided with Amy: drop the three discrete rings; make the well a **single
continuous vortex**, which is more honest to the lifecycle metaphor (age = how
far you've fallen down the funnel) and to the concept art.

- **Tipped funnel axis.** The whole well is reclined about X by `card::WELL_TILT`
  (the *geometry* carries the recline, not the camera): the mouth opens up toward
  the viewer, the throat drops low-and-away. This is the right lever — camera
  pitch alone can't move the throat off the line of sight; tipping the axis can.
  Cards billboard, so they face the viewer regardless; the per-band recline is
  retired.
- **Continuous spiral (option B).** Every card sits on one log-style spiral
  indexed mouth → throat (`card::spiral_pos`/`spiral_scale`/`spiral_order`):
  radius shrinks + depth grows geometrically per index, so cards wind inward and
  down and asymptotically crowd the throat. Append-stable (position keys only on
  the integer index). The lifecycle order (hot → recent → haystack, each zone in
  its `band_orders` slot order) is preserved *as the sequence*. A short arm stays
  in the upper funnel, leaving an **intentional empty stretch above the horizon**
  — nothing has fallen in yet; it fills as contexts age.
- **Odometer navigation.** B keeps number-addressability: the spiral index is the
  address. **Left/Right = ±1, Up/Down = ±10**, digits `0–9` = the first decade at
  the mouth. (Replaced band-hop; `scene::well_keyboard`.)
- **Accretion-horizon throat.** The ring deck's center is an **accretion-disk
  event horizon**: a dark singularity ringed by a hot HDR glow with log-spiral
  arms and a rotating Doppler-bright side (`well_rings.wgsl`) — the bottom of the
  vortex the haystack falls into.
- **Superseded + removed.** The band-positioning path (`lift`, `layout_positions`,
  `WellGeometry`, `CompactingBandLayout` usage, the `layout`/`geom` state fields)
  is deleted — the spiral replaced it. `band_orders`/`assign_bands`/
  `haystack_order_keys` survive (they still order the sequence + label clusters).

Tuning knobs live as named constants (`SPIRAL_R_MOUTH`/`_DECAY`/`_ANGLE_STEP`/
`_SCALE_THROAT`, `WELL_TILT`, `CARD_WIDTH/HEIGHT`, camera `CAM_BASE_*`).

---

## Open questions that remain

The capnp closed events-vs-polling and forest-vs-DAG; the workflow grounding
closed the layout question (three lifecycle bands). These remain genuinely open:

1. ✅ ~~**`conclude` wire shape** (gap 0)~~ — RESOLVED (step 5): `conclude @83`
   + `ContextState::Concluded` + `concludedAt`, distinct from `archived`;
   `setContextState @71` keeps its `Staging→Live` v1 gate (conclude is its own
   verb, not a state-set), and `contextLeave @74` is unrelated (session-binding
   drop only).
2. **Band 1 angle — literal clock-face or just a recency-ordered arc?** Decided
   it's recency, not lineage; the open part is whether it reads as a 12-o'clock
   "most recent" clock or a simpler newest-first sweep.
3. **Overview summarization rule** — which contexts the broad view shows: roots +
   drivers, roots + anything-with-active-drift, or RC-configurable per
   `contextType`? Affects the layout tick's query, not the substrate.
4. **Dive-in re-layout** — the multi-agent fork-tree case is a dive-in; does it
   use the same band grammar at smaller scale, or a freer tree layout (where
   `scale_band` finally earns its keep) on the same table?

---

## Dependency notes (verified June 2026)

- **Scales: write our own.** No maintained d3-scale crate is worth a dependency;
  the family we need (linear, time, band, log, threshold + invert) is ~200 lines.
  Subtleties to budget: band-`invert` is a bisect (d3 doesn't provide it natively),
  time-`nice` tick generation is a few extra lines.
- **Tweening: `bevy_math::curve`** (`EasingCurve` + `EaseFunction`), no crate.
  `bevy_tweening 0.15` is the 0.18-compatible fallback if declarative sequences are
  ever wanted.
- **`fjadra` 0.2.1 — adopt *if* force ever lands**, don't reimplement. Pure-Rust,
  zero-dep, framework-agnostic (Rerun). Integration: hold the `Simulation` in a
  `Resource`, `sim.step()` once per frame, copy `sim.positions()` into `Transform`s
  via a parallel `Vec<Entity>` (sim is index-ordered). Do **not** use `sim.iter()`
  in a frame loop — it blocks to convergence. Still may never be needed (the well is
  deterministic geometry, not force).
- **Borrowed patterns (not dependencies):**
  - *egui_graphs* — the stateless-algorithm + persisted-`LayoutState` split was
    *considered* but the spike showed the compacting layout needs no persisted
    state (the stable `order_key` already carries per-context identity). Keep the
    split in mind for the force-based dive-in, where it would apply; the
    pluggable-extra-forces shape is the part worth borrowing there.
  - *Rerun `re_view_graph`* — for the dive-in / any future relational view: keep a
    *persistent* sim and **re-heat alpha on structural change** rather than resetting,
    and seed new nodes near their neighbours (`Node::fixed_position` pins anchors), so
    existing nodes barely move while new ones settle. (Rerun rejected hierarchical
    layouts precisely because they need full re-layout on change — our deterministic
    bands sidestep that for the live well; this pattern only matters if force is added.)
- **Bevy 0.18 mechanisms (validated — see "Rendering notes" and §3):**
  auto-instancing (draw-level; entities stay individually pickable), `MeshTag(u32)`
  → storage-buffer for per-instance color, `MeshPickingPlugin`, manual billboard
  look-at, `bevy_math::curve` easing, and the app's own `vello_ui_texture` RTT for
  card text. Examples: `~/src/bevy/examples/shader/automatic_instancing.rs`,
  `picking/mesh_picking.rs`, `animation/eased_motion.rs`.

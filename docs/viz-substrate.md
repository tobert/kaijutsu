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

Estimated size: a few hundred lines for join + scales + the Layout trait. The
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

### 4. Layouts — PORT THE TRAIT, write one algorithm ourselves.

```
trait Layout {
    // Stateless algorithm; persisted slot assignments live in `state`.
    fn layout(&self, graph: &ContextForest, state: &mut LayoutState)
        -> HashMap<ContextId, Vec3>;
}
```

Decoupled from rendering and from the join. The join writes layout output to
`Transform`s; Bevy tweens animate the delta.

**Not pure — it carries `LayoutState`** (pattern borrowed from egui_graphs:
stateless algorithm + persisted state). The compacting-list band and the band-1
recency clock must *remember* each context's slot across ticks — a fresh
`data → positions` recompute would re-derive positions from the current set, which
is exactly the count-relative reflow we banned. `LayoutState` holds the durable
slot table (which angular position each `ContextId` occupies in its band); `layout`
reads it, assigns new entrants at the growing edge, and applies the deterministic
one-slot compaction on exit. This is what makes the two motion invariants (below)
enforceable.

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

0. **The `conclude` verb + lifecycle distinction** — *the load-bearing addition.*
   The band-0→band-1 transition is an **explicit, intentional** "this context is
   done" act (the kaijutsu equivalent of `exit`-ing a mux window). It must be
   distinct from a **transient detach** (app restart, dropped connection, closed
   lid), which must **not** demote — a detached-but-not-concluded context stays
   hot in band 0. Today `contextLeave @74` marks a context archived on leave,
   which conflates the two; the model needs (a) a `conclude` operation, (b) a
   `concludedAt` timestamp on `ContextHandleInfo` to give band 1 its recency rank
   and the LRU eviction order, and (c) a lifecycle value for "concluded" distinct
   from transient-detached and from `archived`. Bands 1/2 are then just a
   client-side recency split over concluded contexts (no per-band wire state).
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
4. **Drift edges (context → context)** — the "drift arcs above the rim" grammar
   needs which contexts are drifting to which. Today only `driftQueue @48`
   (staged, pending) and per-block `Drift` snapshots exist; there is no
   "context-to-context drift edge list" RPC. This is haystack/active-decoration
   work, not foundational — defer with the drift-particle layer.

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

1. **Scales** (`scale_time`, `scale_linear`, the quantized 3-band radial scale;
   `scale_band` deferred to the dive-in). Pure, TDD-first, invert round-trip as a
   failing-capable test. Consumer-agnostic, so safe to land first.
2. **Keyed join** with the two-cadence behavior baked in: layout tick from a
   `listContexts` diff, data tick from `subscribeBlocksFiltered`. Foundation for
   everything; test the enter/update/exit diff against a synthetic context set.
3. **`Layout` trait** + the **compacting-list band layout** as first algorithm.
   Test the two motion invariants: `enter` (append) moves nothing existing;
   `exit` (conclude) produces exactly the one-slot compaction; a status-only data
   tick moves nothing.
4. **Card** schema + billboarding + the join writing `ContextHandleInfo` →
   card components; live status glyph from the block-event data tick.
5. **`conclude` wire work** (gap 0) — the operation + `concludedAt` + lifecycle
   value. Without it band 0 has no exit and the radial axis is inert.
6. **Active view** = band 0 (the hot compacting list) + band 1 (recent-concluded
   clock), keyboard `ctrl-a 0–9` on band 0. First concrete consumer.
7. **Haystack view** = band 2 as the second concrete consumer (semantic; wire in
   `searchSimilar`/`getNeighbors`/`getClusters`, cluster labels, on-demand
   lineage, drift-particle layer + its wire additions).
8. **ViewSpec**: extract from the two consumers now that both exist.

`fjadra` (pure-Rust d3-force port) only if a free-form *relational* view proves
necessary — time-well rejected force layout outright, so this may never land.

Transitions are never a build step — Bevy tweens on `Transform`/opacity
throughout.

---

## Open questions that remain

The capnp closed events-vs-polling and forest-vs-DAG; the workflow grounding
closed the layout question (three lifecycle bands). These remain genuinely open:

1. **`conclude` wire shape** (gap 0) — exact RPC + lifecycle enum, and how it
   relates to the existing `contextLeave @74` / `setContextState @71` / `archived`
   surface. The one addition a real consumer needs first.
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
  - *egui_graphs* — the stateless `Layout` + persisted `LayoutState` split (adopted
    above) and pluggable extra-forces shape.
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
</content>
</invoke>

# Time Well — Context Browser Concept Exploration

June 2026. Amy + Claude worked through ~40 generated mockups to find a
direction for rebuilding the constellation and conversation card stack as
instanced 3D scenes (Bevy 0.18). This doc records the decided direction, the
process, and the prompts that produced the keepers, so future iterations can
pick up where we left off.

Mockups live in `mockups/context-browser/` (numbered 01–40). Images were
generated with gpal's `generate_image` (Imagen 4 Ultra and Nano Banana Pro).
Companion memory: `project_time_well_context_browser` in auto-memory.

## The decided direction

**Time well** (mockup 27 is the canonical image): contexts arranged in
concentric time rings descending into a glowing well — newest on the rim as
full readable cards, older rings compressed inward/downward to chips, then
points, then sediment-grain at the core. The ~80% use case is "today through
the last two weeks," so the rim gets the space; history grows *denser*, not
bigger.

> **Refinement (June 2026, design follow-up — see `docs/viz-substrate.md`).**
> The radial axis was sharpened from *calendar time* to **three lifecycle
> bands**, which is what the engineering design builds:
> - **Band 0 (rim) — hot:** open contexts, a flat **compacting list** worked like
>   terminal-mux windows. New context appends to the end; an explicit **`conclude`**
>   act (≠ a transient detach) removes it and the rest compact. The only keyboard
>   surface: `ctrl-a 0–9`, capped at 10. Lineage is an on-demand overlay, not the
>   angular layout.
> - **Band 1 (mid) — recent-concluded:** the last 10 concluded, recency-ordered;
>   the 11th falls inward. Clickops only.
> - **Band 2 (core) — the haystack:** angle re-encodes to *semantic cluster*
>   (lineage stops mattering for cold data). Search-driven recovery, not
>   first-class.
>
> So "active view vs haystack view" are not two views — they are two radii of one
> well. Each band gets an *equal-width annulus* (history denser, not bigger →
> a quantized radial scale). Reach cost scales with coldness: keystroke → click →
> search. The bands map 1:1 onto the LOD/instancing tiers in the table below.

**Two-level navigation (the load-bearing rule, decided last):** the broad
view does NOT show everything. It shows roots and drivers (or a similar
summarization rule) — it exists to show the lay of the land. Selecting a
system/subgraph dives in: that subgraph re-lays-out on the same table with
its full topology, optimized for itself. Overview never overloads; detail is
opt-in per subgraph.

> **Refinement (2026-06-17 — keyboard browse + reading slot; engineering in
> `docs/viz-substrate.md` "Navigation & the reading slot").** The first GPU
> framing showed ring cards are too small to read and the keyboard only walked
> the hot rim. So selection became a 2D walk over the rings — **left/right**
> within a band, **up/down** to hop bands (up warms toward the rim, down cools
> toward the core), `0–9` still a hot quick-jump — and a **center-bottom reading
> slot** renders the current selection at full, legible size with the rings
> staying behind it as the spatial index. This is the keyboard precursor to the
> mockup-34 JOIN dive: the reading slot *is* the focused card, parked rather than
> dived-through. (Zooming the whole well to enlarge cards was tried and rejected
> — it crops the side cards.)

### Visual grammar

- **Two flow systems, never mixed**: fork lineage descends the well wall
  (structural, calm threads); drift traffic arcs above the rim (live,
  particles). Every context participates in both without visual collision.
- **Hub-and-spoke only**: draw edges to the driver/hub, never
  worker-to-worker. Peer relationships are implied by shared sector + common
  ancestor.
- **Role aggregation**: sibling groups collapse into fanned decks with
  count/status badges ("REVIEWERS ×8 · 3 streaming"); expand on demand.
- **Fork density is a material property**: heavy forkers wear filigree
  halos; hover unfurls the halo into timestamped fork chips. No edges drawn
  for density.
- **Spent forks become sediment** strata down the well wall (年輪 nenrin).
- **Status colors**: cyan pulse + rising token-particles = streaming; amber =
  focused/waiting; red = error; dim grey = archived. Round avatar chips show
  *which peer* is working.
- **HUD**: search + breadcrumb top; filter rail left (provider / context_type
  / archived); detail sidebar right (label, model, keywords, preview,
  lineage tree, JOIN); TODAY ◂▸ PAST jog-dial scrubber bottom; status strip
  with counts ("14 contexts · 3 streaming · 1 waiting"); activity ticker.
  Mockup 33 (top-down mandala) doubles as a minimap.
- **Join transition** (mockup 34): camera dives through the focused card,
  which unfolds into the conversation view — browser and conversation are
  one continuous scene.

### Engine mapping (Bevy 0.18)

LOD tiers map 1:1 to instancing tiers:

| Tier | Representation | Bevy mechanism |
|------|----------------|----------------|
| Rim cards / focused / driver | Full entities, RTT text | `Mesh3d` + `ExtendedMaterial`, existing BlockTexture pipeline |
| Chips, decks, sediment | Instanced quads, per-instance state | shared mesh+material auto-batching + `MeshTag(u32)` |
| Deep history core | Particle cloud | single instanced point/particle system |

Picking via `MeshPickingPlugin` observers; labels via `Text2d` in world
space. Key examples: `~/src/bevy/examples/shader/automatic_instancing.rs`,
`extended_material.rs`, `picking/mesh_picking.rs`.

Data gap: `ContextInfo` has no block counts client-side; needs a small RPC
addition if size should encode conversation length.

## How we worked (the process, reusable)

1. **Broad divergence (01–12)**: twelve deliberately different art
   directions in one round — glass fork-tree, orbital, zen lanterns,
   blueprint, motherboard city, gallery hall, holo table, instanced galaxy,
   deep sea, HUD lanes, ukiyo-e paper, helix timeline. Amy browsed and named
   pulls: holo table (07) "close to what I was imagining," helix (12) "*COOL*
   but maybe too compact."
2. **Variations + hybrids (13–22)**: variations on the two favorites plus a
   few wildcards. The favorite-marriage (13, helix-on-table) was the
   standout — when two concepts both pull, render their hybrid early.
3. **A user insight reframes the axis (23–28)**: Amy: archived/old stuff
   pushed inward, reduced to points as history grows, user goes "in" and
   "out" to browse the past; 80% use case is recent. That turned the radial
   axis into *time* → growth rings (年輪) → time well (27) emerged and won.
4. **State studies on the winner (29–34)**: focused state, parallel
   activity, full HUD chrome, top-down minimap, join transition. This round
   produced the interaction vocabulary, not new geometry.
5. **Stress test with a real scenario (35–40)**: 1 driver + 4 coders + 8
   reviewers + 4 fixers, drift hub-and-spoke. Forced the aggregation /
   anti-spaghetti grammar, and led directly to the two-level navigation
   rule.

Each round: generate ~6 in parallel, spot-check 2–3 by reading the images,
report, let Amy browse the rest. Don't refine before the human has reacted.

## Prompting notes (gpal image gen)

- **Nano Banana Pro (`nano-pro`) is the workhorse for UI mockups**: far
  better at legible in-image text, HUD chrome, labels, badges, and counts.
  Every text-heavy keeper (03, 13, 23, 26, 27, 31, 35–40) was nano-pro.
- **Imagen 4 Ultra (`imagen`)** is better at atmosphere/composition but
  washes out into grey "concept gloss" on busy prompts (17) and garbles
  small text. Use it for mood/motion studies (25, 34).
- **Name the real data fields** — label, model badge, context_type, status,
  keywords, archived-dimming, lineage. Prompts grounded in `ContextInfo`
  produced mockups that map directly onto the data model; vague prompts
  produced pretty pictures.
- **Give the model concrete example labels** ("main", "unrig",
  "debug-auth", "coder/refactor") — it uses them and invents consistent
  siblings.
- **State the layout rule, not just the look**: "outermost ring = this
  week's contexts as full cards; each ring inward older, shrinking to
  points" was followed startlingly well. Same for "hub spokes only, no
  worker-to-worker edges."
- **Numbers are followed**: "4 teal coder cards, 8 violet reviewer cards,
  4 green fixer cards" rendered with correct-ish counts and sector arcs.
- **Ask for the moment, not the diagram**, when exploring interaction:
  "a moment of navigation — camera diving inward, points blooming into
  cards" (25), "a review wave — eight reviewers streaming, ripples
  interfering" (40).
- **Gotcha**: "desktop application screenshot" can be taken literally —
  nano-pro rendered the orrery (21) *on a physical monitor on a desk*.
  Prefer "concept art for a desktop application" or "application screenshot
  framing" when the scene should fill the frame.
- **The model invents good UI**: the TODAY ◂▸ PAST physical jog wheel (26),
  the driver inbox of labeled drift slips (36), and the "review round 2/3 ·
  5 of 8 complete" chip (40) were all model inventions worth keeping.

## The keeper prompts (verbatim)

### 27 — time well (the winner)

> 3D UI concept: a holographic "time well" above a circular table in a dark
> room — concentric rings of conversation contexts descend into a soft
> glowing well at the center, each lower ring older and smaller: outer rim
> holds full readable glass cards (this week), rings below hold shrinking
> chips, the deep center a faint spiral of light grain (years of history).
> The camera looks over the rim into the well at a gentle angle. Fork
> lineage threads drape down between rings like silk. One rim card glows
> amber and expanded. Table-edge filter ring. Mysterious but calm and
> readable, indigo-teal-amber, concept art for a desktop application.
> *(nano-pro, 16:9)*

### 07 — holo table (the seed)

> First-person view of a holographic projection table in a dark control
> room: a 3D fork-tree of conversation contexts projected above the table
> as translucent cyan cards with amber accents, connected by glowing
> splines from parent to child. Floating labels, small model badges, soft
> activity pulses on streaming nodes. The projection has faint scanline
> shimmer and volumetric light. Around the table edge runs a thin UI ring
> with filter toggles: provider, context type, archived. Sci-fi but legible
> and calm, concept art for a real desktop application. *(nano-pro, 16:9)*

### 13 — helix-on-table (the hybrid that proved marriage works)

> First-person view standing at a circular holographic projection table in
> a dark control room, camera close over the table edge. Rising from the
> table center is a luminous double-helix timeline of conversation contexts
> — time ascends upward, fork branches spiral off the main strand as their
> own smaller strands. Each context is a small translucent cyan card with a
> short label and model badge; one branch lineage is highlighted amber.
> Generous spacing between cards, airy and navigable, not dense. The table
> edge has a thin UI ring with filter toggles: PROVIDER, CONTEXT TYPE,
> ARCHIVED. Volumetric light, faint scanlines, calm sci-fi, concept art for
> a real desktop application. *(nano-pro, 16:9)*

### 26 — wide today-ring (the 80% layout + jog wheel)

> Overhead three-quarter view of a circular holographic projection table
> where the layout honors the 80% use case: the outermost ring is WIDE,
> occupying most of the table surface, holding this week's conversation
> contexts as generous readable glass cards with labels, model badges,
> keyword chips, and status glows. All older history is compressed into a
> small dense core of concentric micro-rings of glowing points at the
> center, like the heartwood of a tree. Thin date labels separate the core
> rings. A few lineage threads run from outer cards into the core. Filter
> ring and a small TODAY ◂▸ PAST scrubber on the table edge. Dark, elegant,
> readable, concept art for a desktop application. *(nano-pro, 16:9)*

### 31 — full HUD (closest to a product spec)

> Full desktop application screenshot of a context browser called Kaijutsu
> (会術): center stage is a holographic time well above a circular table —
> rings of context cards descending into a glowing history core. AROUND IT,
> a complete HUD: top bar with search field and breadcrumb
> "kaijutsu://contexts"; left edge a slim vertical filter rail (provider,
> context type, archived toggle); right side a detail sidebar for the
> selected context (label, model, keywords, preview, lineage list, JOIN
> button); bottom edge a time scrubber labeled TODAY ◂▸ PAST with a small
> jog wheel, plus a status strip showing "14 contexts · 3 streaming · 1
> waiting". A thin activity ticker streams recent events in one corner.
> Dark theme, cyan/amber on near-black, crisp legible UI typography.
> *(nano-pro, 16:9)*

### 30 — parallel activity (status vocabulary)

> A holographic "time well" context browser above a circular table, viewed
> from slightly above the rim: concentric rings descending into a glowing
> history well. PARALLEL ACTIVITY: four rim cards are working
> simultaneously — each streaming card pulses cyan with a thin column of
> tiny token-particles rising from it, one card shows an amber WAITING
> badge, one flickers red with an error glyph. Each active card has a small
> round agent avatar chip attached (different colors per AI peer). Faint
> ripples spread across the rings from each active card like raindrops on
> water. The rest of the well stays calm and dim. Readable labels and model
> badges, elegant, dark, concept art for a desktop application.
> *(nano-pro, 16:9)*

### 33 — top-down mandala (minimap / ambient state)

> Top-down view straight into a holographic time well: concentric rings of
> context cards spiraling down to a bright history core at center, like
> looking into a luminous nautilus. Several contexts active at once — their
> cards glow cyan and their lineage threads light up as radial spokes,
> making the well read like a clock face of parallel work lanes. Small
> agent avatar chips ride the active spokes. Quiet contexts stay dim glass.
> A delicate radial HUD floats over the rim: month labels around the
> circumference, a search field at top, filter glyphs at the cardinal
> points. Dark, symmetric, mandala-like but crisp and readable, concept art
> for a desktop application. *(nano-pro, 16:9)*

### 35 — maypole driver (two flow systems)

> A holographic "time well" context browser above a circular table,
> three-quarter view: concentric rings of context cards descending into a
> glowing history core. A MULTI-AGENT WORKFLOW is running: a single DRIVER
> context card (amber, labeled "driver") hovers above the well's central
> axis like a conductor; sixteen worker contexts stand on the rim grouped
> into three colored sector arcs labeled CODERS (4 teal cards), REVIEWERS
> (8 violet cards), FIXERS (4 green cards). Luminous drift streams arc UP
> from worker cards to the driver and back down, like a maypole of light —
> only hub spokes, no worker-to-worker edges. Meanwhile their fork lineage
> threads run DOWN the well wall to a common ancestor chip deep below.
> Small agent avatars on each card, streaming cards pulse. Readable,
> elegant, dark, concept art for a desktop application. *(nano-pro, 16:9)*

### 37 — role decks + sediment (aggregation grammar)

> Holographic time well context browser showing GROUP AGGREGATION in a
> heavily forked workspace: on the rim, a deck of eight violet reviewer
> contexts is collapsed into one fanned stack with a badge "REVIEWERS ×8 ·
> 3 streaming"; next to it one teal coder cluster is EXPANDED, revealing
> its local fork chain — the coder card with two small review-fork chips
> and a fixer-fork chip branching off it on short stems, each stem a tiny
> lineage thread. The amber driver card stands apart with spokes to the
> group decks, not to individuals. Below the rim, hundreds of spent
> micro-fork chips from past rounds spiral down the well wall like fine
> sediment layers. Readable labels, dark elegant palette, concept art for a
> desktop application. *(nano-pro, 16:9)*

### 39 — fork halos (density without edges)

> Holographic time well context browser handling EXTREME fork density
> without visual spaghetti: worker context cards on the rim each wear a
> soft luminous halo whose density and brightness encode how many forks
> they've spawned — heavy forkers glow with fine filigree texture, light
> forkers are clean glass. No edge lines between workers at all; only four
> bright threads connect the currently ACTIVE contexts to the amber driver
> card hovering above the axis. One reviewer card is hovered by the cursor
> and its halo unfurls into a readable ring of tiny fork chips with
> timestamps. The well below shows compressed strata of thousands of past
> forks as shimmering sediment rings. Calm, readable, dark, concept art for
> a desktop application. *(nano-pro, 16:9)*

Other useful ones not quoted in full: 23 (nenrin top-down with ring date
labels), 29 (focused card + amber ancestry trace), 32 (focus + background
parallel + drift toast), 34 (join transition, imagen), 36 (driver inbox of
drift slips), 38 (switchboard top-down, spoke brightness = traffic), 40
(review wave). Dead ends worth not repeating: 17 (imagen grey-wash on a
busy geometric prompt), 21 (literal-monitor gotcha), ikebana (20 — clever
but ベタ/クサい, i.e. cheesy).

## Open questions for the next iteration

These are the UX-level questions; the engineering-level open questions (the
`conclude` wire shape, band-1 clock-vs-arc, dive-in re-layout) now live in
`docs/viz-substrate.md`. The radial-axis question is resolved there (three
lifecycle bands).

- What exactly is the overview summarization rule — roots + drivers, or
  roots + anything-with-active-drift, or RC-configurable per context_type?
- Dive-in layout for a subgraph: same ring grammar at smaller scale, or a
  freer tree layout while still on the table?
- How does the join transition (34) hand off to the rebuilt conversation
  view — shared camera rig? Same scene graph?
- Block counts / sizes need an RPC addition before size-encodes-length.
- How do drift queue depths render at overview level (36's inbox is per
  driver card; what does a backlog look like from afar)?

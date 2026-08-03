# The Horizon Dive — searching what fell past the event horizon

2026-08-01. The time well seats forty contexts on four rings and renders
everything else as a "+N" chip at its throat (`docs/timewell.md`, "Ring
membership becomes explicit"). That chip is currently a dead end: a count with
nothing behind it. This doc designs what happens when you **activate** it —
a dive into an abstract space that is, first and foremost, a **search
interface** over the demoted, concluded, archived and overflowed contexts
behind that number.

*Status: the spike this doc describes lives on branch `feat/horizon-dive`,
deliberately not on `main` — it runs against ~300 synthetic contexts behind a
debug `F2` binding, not live kernel data. "Slices" below is what a real v1
still needs.*

The well's own charter already called this shot. Requirement 3 of "What the
time well is for" reads: *"An event horizon — past some depth, contexts stop
being objects you scroll past and become an archive you search (form-oriented,
not spatial)."* This design honours the first half literally — the query line
is the form, and it is the primary input — and then argues the parenthetical
was half right. Search should be form-oriented. **Presentation** does not have
to be, and the thing a flat result list throws away is the one advantage a
long-lived archive can give you: you have been here before, and you remember
where things were.

A spike lives at `crates/kaijutsu-app/src/view/horizon_dive/`. It runs on ~300
synthetic contexts, is reachable on `F2` (or `h` in the zoomed well), and its
notes are at the end of this doc.

## The reference point, and what's wrong with it

Amy's reference is No Man's Sky's galaxy map, offered with the framing "I find
it frustrating but I understand why it is that way." That's the right way to
hold it: the constraints are real and shared — a great deal of information
compressed into a tile and a star field, controller-first navigation, and an
underlying structure that is an undirected graph with cycles rather than a
tree. Our graph is fork lineage plus drift and crosstalk links between
contexts; theirs is jump range between stars. Same shape.

The frustrations are specific and each one is designable-out:

| NMS does | Cost | We do instead |
|---|---|---|
| Free-flight camera you aim | You spend attention on *flying*, not on *choosing*. Overshoot, drift, re-approach. | The camera is a **consequence of the selection**, never an input. Directional keys move the selection; the camera follows. |
| Search re-centres and re-frames the map | You lose your place. The map you learned is not the map you're looking at. | The query changes **brightness, scale, and the reading line**. Positions never move. |
| Everything is a star | A star tells you nothing until you're on it. | Everything is a **card** — the well's card, with a title, an accent, and a state line, legible before you commit. |
| Every connection is drawn | A hairball, at every zoom. | Only the **current selection's** neighbourhood is drawn, capped. |
| Position encodes little you can predict | You can't develop spatial memory, so every visit starts from zero. | Position is a pure function of `(lineage family, id, when it fell past the horizon)`. Same context, same seat, every visit. |

The through-line: NMS gives you a *vehicle* and a map. We give you a *query*
and a place that holds still.

## The five principles, and where they live in the code

1. **Search-first, space-as-presentation.** `/` is the primary input. The
   space presents ranked results; it is not a level to traverse.
   `scene::ease_dive_camera` — the camera is derived, never driven.
2. **Stable positions, query as light.** `layout::stream_coord` takes no query
   and no light. That is enforced by the signature, deliberately: the moment a
   search can re-flow the layout, spatial memory is dead and the space has
   stopped earning its cost. `scene::sync_card_visuals` writes brightness and
   scale only.
3. **Local edge reveal.** `layout::neighborhood` returns the selection's
   parent, children, siblings and drift partners — capped at
   `layout::MAX_EDGES` — and one mesh draws them (`scene::sync_constellation`).
   The full graph is never rendered, at any zoom, ever.
4. **Snap navigation.** `layout::snap_neighbor` picks the nearest candidate
   inside a screen-space cone. `hjkl`, arrows, dpad. No cursor, no aiming.
5. **Cards, not stars.** Every context is a `WellCardMaterial` quad with an
   MSDF face — the well's own visual language, reused wholesale rather than
   re-invented. The selection ring, the lineage ring, and the drift sheen in
   `well_card.wgsl` all already exist; the dive borrows them.

## Where the cards sit: the accretion stream

Three axes, one function (`layout::stream_coord`):

**Angle — lineage family.** A stable FNV hash of the family root's context id
picks a bearing on the circle; each member is jittered `±FAMILY_SPREAD` (~10°)
around it. A family therefore reads as a **lane**, not a line: a forty-context
lineage doesn't stack into one pillar, and two unrelated families don't
interleave. Same recipe the well's track rays use for the same reason — a
bearing you learn stays learned across restarts.

**Depth — time since falling past the horizon.** Log-compressed:
`ln(1 + age_days) / ln(1 + 365)`, clamped. The distribution of real archives
piles up recent and thins out old; a linear map would spend most of the depth
budget on an empty tail. The clamp gives the archive a floor rather than an
infinite runway — a year gone and five years gone are both just "gone."

**Radius — narrows with depth.** The stream funnels as it falls, continuing
the well's own geometry rather than cutting to an unrelated space. Plus a
per-context radial and depth jitter so two same-family contexts that fell on
the same day don't occupy the same point.

The whole thing is one function of `(family, id, fell_at, now)`. It is
deterministic, it is testable without a `World`, and it has no query in it.

### The one thing that does drift

`now` is captured once per dive, not per frame — otherwise every card would
creep imperceptibly forever, which is worse than an honest jump. But across
*days*, depth does move: a context that fell yesterday is at a different depth
next month. Log compression makes this slow (the whole first week occupies
about a fifth of the depth axis), and the angular lane — the part you actually
navigate by — never moves at all. **Open question for Amy** below.

## The interaction model

### Entering

The real entry is activating the well's "+N" horizon chip: `Enter` on the
horizon label at the throat, which is currently inert. The dive is a screen
transition, not a station zoom — like the FSN landscape, it's an unbounded
world, too big to stand as room furniture (`docs/scenes/shell.md`, "N stays a
dive-THROUGH door").

*The spike does not wire that entry.* It ships `F2` from anywhere and `h` in
the zoomed well, both through the central action table, so the prototype
doesn't have to restructure the well's keyboard to be evaluated. `Esc` returns
to whichever screen you dived from — the dive records its origin rather than
hardcoding "back to the room", so the debug entry and the real entry can
coexist.

### Typing and moving are different modes

This is the one place the design takes a hard line, and it is enforced
structurally rather than by a flag inside a handler.

- **Navigation mode** (default). `InputContext::HorizonDive` is active. `hjkl`
  / arrows snap, `Enter` opens, `p` surfaces, `?` toggles the legend, `Esc`
  leaves. Everything arrives as `ActionFired`; nothing reads raw keys.
- **Query mode.** `/` hands the keyboard to the query line as an explicit
  `KeyboardGrab::HorizonQuery`, exactly the way the vi editor and the compose
  VimMachine take it (`docs/input.md`, "Keyboard grabs are explicit"). While
  the grab is held, `InputContext::HorizonDive` is **not derived at all** —
  which is what makes it impossible for `p` to surface a context out from
  under the letter you meant to type. `Ctrl+A`, F1 and F12 survive, because
  the dispatcher keeps matching Global bindings under a grab.

`Esc` or `Enter` returns to navigation. Both do the same thing, and that is not
an oversight:

### The query is never submitted

The query applies live, on every keystroke. There is nothing for `Enter` to
commit. `Enter`-to-search is a habit from interfaces where searching is
expensive; here it is a substring scan over a few hundred records (and, with a
real index behind it, a single embedding plus a top-k lookup). Making you press
a key to see the effect of the key you just pressed is a tax with no payer.

`Enter` therefore means "I'm done typing, give me the keys back", identically
to `Esc`. They differ in intent, not effect.

### The query never moves you

Relighting does not touch the selection. If you type something that lights
nothing, you are still standing exactly where you were, in the dark.

This is the least obvious decision here and the one most likely to be argued
with, so: **the selection is where you are, not what you found.** A search that
evicts you has taken away the ability to search *from* somewhere — "what near
this thing mentions capnp" becomes impossible, because the moment you type
`capnp` you're somewhere else. Keeping the selection put makes the query line
a *filter on the space around you* rather than a teleporter, and snap
navigation then walks you to whatever it lit. There is a unit test for this
(`scene::tests::the_query_never_moves_the_selection`) because it is exactly the
kind of invariant a later convenience would quietly break.

### Snap navigation: what a direction key may land on

The candidate set is the **union** of

- everything the query lit at or above `LIT_THRESHOLD`, and
- everything the current selection is *linked* to (its constellation),

minus the selection itself (`layout::nav_candidates`). The union is the point.
Query-only navigation strands you the moment a visible neighbour isn't lit;
link-only navigation can't leave the local component. Together, the same four
keys mean both "walk the results" and "walk the graph", and which one you're
doing is determined by what you typed, not by a mode you have to remember.

Selection rule (`layout::snap_neighbor`): project every candidate to screen
space, accept those within a ±60° cone of the direction, and minimise
`along + 1.6 × across`. Screen space, because "left" has to mean what the user
sees. The cone is generous on purpose — a sparse lit set often has nothing
within a tight cone, and a direction key that silently does nothing reads as
broken. Ties break on the lower index, so identical geometry never produces a
wobbling selection.

### Verbs on the selection

| Key | Verb | What it means |
|---|---|---|
| `Enter` | **Open** | Switch to this context and leave the dive. You came here to find something; finding it should end the trip. |
| `p` | **Surface** | Promote it back onto a ring — it stops being past the horizon. The card leaves the dive; the selection steps to its nearest link so you're never parked on something invisible. |
| `Esc` | **Leave** | Back to the screen you dived from. One `PopLevel`, per the Esc doctrine. |

`p` reuses `Action::Promote` and `Enter` reuses `Action::Activate` — the same
actions the well binds — rather than minting dive-specific verbs. Likewise
`StepPrev`/`StepNext` are "move within the level" and `LevelUp`/`LevelDown` are
"shallower/deeper", which in a stream is literally what up and down mean. The
dive adds exactly two new actions: `DiveHorizon` and `EditQuery`. Gamepad
support, `bindings.toml` rebinding, and the `?` legend then come free.

## Where a real search plugs in

`corpus::HorizonRanker` is the seam:

```rust
fn light(&self, query: &str, corpus: &[HorizonContext]) -> Vec<f32>;
```

One `0..=1` light per corpus entry, **in corpus order** — a dense vector, not a
ranked list. That shape is deliberate: the scene never sorts and never places,
it only decides how brightly each card burns where it already sits. A ranked
list would invite exactly the re-flow principle 2 forbids.

The prototype ships `SubstringRanker` (whitespace tokens, ANDed, over titles
and keywords, with a fuzzy-subsequence fallback). A real implementation embeds
the query once, takes `SemanticIndex::search(query, k)`, writes those scores
into the vector and leaves the rest at `0.0`. `kaijutsu-index` already returns
`SearchResult { context_id, score, label }` with `score` normalised to `0..1`,
so the adapter is a `ContextId → index` map and a scatter.

An **empty query lights everything**. No query is a query that matches all of
it, and a space that goes dark when you aren't typing has stopped being a
space you can navigate.

## What the kernel still owes us

The dive needs four things per context. Today the wire (`ContextInfo`) carries
one and a half:

| Needed | Status |
|---|---|
| Fork parent | ✅ `forked_from` |
| Title, keywords, accent | ✅ `label`, `keywords`, `context_type` |
| **When it fell past the horizon** | ❌ Derivable-ish from `demoted_at` / `concluded_at`, but plain overflow has no stamp at all — it fell past because *other* contexts arrived, and nothing recorded when. |
| **Drift / crosstalk partners** | ❌ `StagedDriftInfo` covers the *staged* queue only. Historical drift between two long-cold contexts isn't queryable. |
| **The horizon list itself** | ❌ `assign_ring_seats` returns `horizon: Vec<Id>` app-side from a full context list. Fine at 300; a paging RPC at 10k. |

None of these are hard, and none belong in a spike. The depth axis is the one
that actually bites: without a "fell past the horizon at" stamp, an overflowed
context's depth has to fall back to `last_activity_at`, which is a different
quantity (when it was last *touched*, not when it stopped being reachable).
For a v1 that's an acceptable stand-in; it should be written down as a
stand-in rather than quietly conflated.

## Slices

**v1 — the honest minimum.** Real corpus from the kernel (with
`last_activity_at` standing in for the fall stamp, documented as such).
Substring/keyword ranking over titles + keywords, no index. Stream layout,
snap navigation, lineage-only constellation (no drift edges — the data isn't
there). Open and Leave. No Surface. Entry from the well's "+N" chip.

*This is genuinely useful on its own*: it is the first time the "+N" leads
anywhere at all.

**v2 — the search gets real.** `SemanticIndex` behind `HorizonRanker`. Drift
edges once the kernel can answer "what has ever drifted with this context".
Surface (`p`). The `?` legend rendered from `InputMap` labels like the well's,
so a rebind shows up for free.

**v3 — the space gets rich.** Sparklines and model glyphs on card faces (the
well's `live.rs` lanes already exist). Cluster tinting from
`kaijutsu_index::compute_clusters` — clusters are a *colour*, never a position,
or principle 2 falls. A "you were here" trail. Query history on `Ctrl+A '`-style
prefill.

**Not planned.** Free-flight. Zoom levels. A minimap. Multi-select. Each is a
vehicle, and the whole design is an argument that you don't need one.

## Open questions for Amy

1. **Does depth drifting over days bother you?** Depth is age, so cards sink
   slowly. The alternative is depth-from-a-fixed-epoch (absolutely stable, but
   the recent band gets more crowded forever) or bucketed depth (stable within
   a bucket, visible jumps at bucket boundaries). I picked drift because the
   angular lane — the axis you actually navigate by — is rock stable, and
   because "older is deeper" stays true under drift. Worth a look in motion.

2. **Should the query be able to move you, on an explicit key?** I've argued
   hard that a query must not evict the selection. But "jump to the brightest
   result" is a real thing to want. A separate key (`n`/`N`, next/previous
   result by light) would give it without breaking the invariant. I left it
   out of the spike to see whether snap navigation alone is enough.

3. **Is `Enter` = open-and-leave right, or should it be open-and-stay?**
   Leaving matches "you came here to find something." Staying supports "open
   three things in a row." The prototype logs instead of opening, so this is
   still fully open.

4. **Should surfacing (`p`) be reachable at all from down here**, or is the
   horizon a place you only *read* from? Making a placement verb reachable in
   the archive is convenient; it also means a search interface can mutate
   ring membership, which is a different kind of surface than a browser.

5. **The right entry gesture.** `Enter` on the "+N" chip is the obvious one.
   But the chip is only reachable by navigating to the deepest ring first. A
   `Ctrl+A /`-style prefix chord ("search everything, from anywhere") might be
   the shortcut that actually gets used — at the cost of a slot in a table the
   docs say is deliberately kept to one screenful.

6. **`FOCUS_RADIUS` / the attention falloff.** Cards fade with distance from
   the selection (see the notes below for why). That is a *second* dimming
   axis on top of the query, and two dimming axes can fight. It reads well at
   the current numbers; it may want to be brightness-only, with scale left to
   the query alone.

---

# Prototype notes (2026-08-01)

Built and run against a Wayland session with a real GPU; ~300 synthetic
contexts, driven over BRP. Screenshots in the session scratchpad
(`dive-04-rest.png`, `dive-05-query.png`); not committed.

## What worked, first try

- **Query-as-light is immediately convincing.** Typing `wire` took 300 cards
  down to 19 lit, with everything else still visibly *there* as a dim field.
  The lit set was scattered across the whole stream — which is the honest
  result, and precisely the case a list view flattens into meaninglessness.
  Watching the field respond per-keystroke is the moment the design justifies
  itself.
- **The selection holding still through a query change felt right, not
  broken.** I expected to want it to jump. I didn't. You type, the space
  around you re-lights, and you walk to what you wanted.
- **Local edge reveal reads beautifully.** One long line from the selection
  across the stream to a cross-family drift partner tells a story that a
  hairball never could. Capping at 24 never triggered on real data.
- **Reusing `WellCardMaterial` was the right call.** Selection ring, lineage
  ring and drift sheen all already existed in `well_card.wgsl`; the dive maps
  its edge kinds onto them and needed no new shader at all.
- **The grab-based query line worked exactly as `docs/input.md` promised.** No
  "am I typing?" check exists anywhere in a domain handler — the mode is
  expressed once, in `derive_contexts`, and everything downstream is a
  consequence.

## What felt wrong, and what I did about it

- **The first framing was a wall.** With an empty query everything is lit by
  contract, and 300 fully-bright overlapping cards is not a space, it's a
  texture. Fixed by adding an **attention falloff**: brightness drops with
  distance from the selection, floored at 16%, with linked cards exempt. Near
  the selection you read cards; far from it you read a field. See open
  question 6 — this is the tuning I trust least.
- **Cards were far too large and the camera far too close.** First numbers had
  150-unit cards at 980 units of standoff in a 620-radius stream. Retuned to
  108-unit cards, 1900 standoff, 1150 radius, with the jitters roughly
  doubled. Better, still not composed: the stream reads as a diagonal band with
  the right third of the frame empty, because the camera looks straight down
  −Z from a selection that sits on the *edge* of the stream's cross-section.
  A real slice should aim the camera at the stream axis, not at the card.
- **A linked card near the camera becomes a billboard.** Because linked cards
  are exempt from the falloff, a drift partner that happens to sit shallow
  fills a quarter of the screen. Informative, but it shouts. Wants a distance
  clamp on scale.
- **The selection was getting occluded** by neighbours a few units nearer.
  Fixed with a fixed 130-unit lean toward the camera applied to whichever card
  is selected — presentation, not layout; the seat is unchanged, and it moves
  with the selection rather than with the query.

## Things found on the way that aren't mine to fix

- **The shared app camera's far plane is Bevy's default `1000.0`,** and
  `Projection::compute_frustum` uses it for real frustum culling. The FSN
  landscape flies to ~1400+ units and the room's octagon has a 1200 apothem —
  **both may already be silently culling geometry at distance.** The dive
  claims a 30000 far plane on entry and restores the original on exit, so it
  doesn't paper over the issue for anyone else. Worth a look with fresh eyes;
  belongs in `docs/issues.md` if confirmed.
- `view::editor::keys` went from `mod` to `pub(crate) mod` (and `pressed_char`
  to `pub`) so the dive's query line reuses the layout-aware key→char law
  rather than keeping a second copy of it. Two tokens; the alternative was
  duplicating a rule that has to stay correct for AZERTY.
- `scene::line_list_mesh` is a local copy of `fsn::scene::line_list_mesh_colored`
  (`pub(super)`). A spike isn't reason enough to widen another module's
  visibility; de-duplicating it is cleanup for whoever lands v1.
- The dive's `?` legend is a string literal, not rendered from `InputMap`
  labels the way the well's is. Shortcut, called out in the code, and on the
  v2 list.

## Cost estimate for a real v1 slice

Assuming the spike's pure modules survive largely intact (they should — they
have no Bevy in them and 50-odd tests):

| Work | Estimate |
|---|---|
| Kernel: `horizonContexts` RPC (list + the fall stamp) | ~1 day, mostly deciding what the stamp *is* |
| App: corpus adapter, `ContextInfo` → `HorizonContext`, keyed-join reconcile like `time_well::sync` | ~half a day |
| Wire the well's "+N" chip as the entry; retire the debug bindings | ~2 hours |
| Camera framing pass (aim at the stream axis; the composition problem above) | ~half a day, mostly live-tuning over BRP |
| Real `Enter` (switch context and leave) | ~2 hours |
| Legend from `InputMap`; visual polish pass with Amy in front of it | ~half a day |

**~3 days of focused work to a shippable v1**, plus however long the tuning
conversation takes — and that conversation is the part that decides whether
this is good, so it shouldn't be squeezed. v2 (semantic index + drift edges)
is gated on kernel work more than app work.

The pure math is done and tested. The uncertainty is entirely in the framing
and the feel, which is exactly where it should be after a spike.

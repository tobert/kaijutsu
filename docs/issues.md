# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md) (the landed-work story). See the three-file working-notes pattern in `CLAUDE.md`.

---

## Input: selection auto-copies to PRIMARY (seeded 2026-07-16, input rework)

The other half of the xterm clipboard model (Ctrl+V + middle-click paste
shipped; so did the full prefix set incl. `'`/`A` prompts and the
armed-footer legend): needs a selection UX first. The overlay's
`selection_anchor` has no live producer (mouse drag-select and vi visual
mode are both unwired); when one lands, copy-on-selection to PRIMARY rides
it — `InputOverlay::selection_range` is the read point.

## msdfgen-rs `Shape::get_bound()` / `Contour::get_bound()` zero-seeded (seeded 2026-07-16, msdf-geometry lane)

`msdfgen-rs` (`/home/atobey/src/msdfgen-rs`, local path dep, do-not-modify
per lane boundary) has a real bug independent of the three geometry bugs
this lane fixed: `Shape::get_bound()`/`Contour::get_bound()`
(`src/shape.rs`, `src/contour.rs`) seed their accumulator with
`Bound::default()` == `(0,0,0,0)`, then call the raw C++ `bound(l,b,r,t)`
out-parameter method, which only ever *shrinks toward* an extreme
(`sys/lib/core/edge-segments.cpp`'s `boundPoint`: `if (p.x < l) l = p.x;`
etc.). That out-parameter method is designed to be called with the caller
pre-seeding `±LARGE_VALUE` — which is exactly what the C++ convenience
`Shape::getBounds()` does, but msdfgen-rs never binds `getBounds()` at all.
Net effect: `left`/`bottom` silently stay `0.0` whenever a glyph's true
left/bottom edge is positive — the common case for glyphs with left-side
bearing or sitting above the baseline. Verified by hand against
`CascadiaCodeNF.ttf`'s `.` glyph (glyph_id 1862): ttf-parser's tight bbox is
`x_min=452`, but `Shape::get_bound()` reports `left=0.0`.

Consequence in `kaijutsu-app`: `generator.rs::generate_glyph`'s bitmap
sizing/centering (both before and after the Fix-1 scale correction) reads
`bounds.left`/`bounds.bottom` from this buggy call, so bitmaps are
oversized and off-center for affected glyphs. Fix 1's scale correction
(`msdf-geometry` lane) is unaffected in *magnitude* — ink size is an affine
difference, so the wrong `translate` offset cancels out — but the glyph's
absolute *position* in the padded bitmap (and thus the anchor) is still
built on the wrong `bounds.left`/`bounds.bottom`. Fix, when picked up:
either patch msdfgen-rs's `get_bound()`/`get_bound_miters()` to seed
`±f64::MAX` before calling the raw `bound()`/`boundMiters()`, or have
`kaijutsu-app` stop relying on `Shape::get_bound()` for glyph extents and
use `ttf_parser::Face::glyph_bounding_box()` instead (already proven
accurate against this bug in `generator.rs`'s tests).

**Upstream checked 2026-07-16 (Fable session)**: our clone is AT
`katyo/msdfgen-rs` master tip (0 behind); upstream is dormant (last release
2020, 3 open issues, no fix) — waiting for a new version is not a path.
Preferred fix = the `glyph_bounding_box()` route in our `generate_glyph`:
`face` is already parsed there, `None` → existing placeholder path replaces
the empty-shape edge case for free, control-point bboxes are ⊇ curve bboxes
so framing stays safe, and it shrinks atlas bitmaps (origin-inclusion waste
gone) with no third-party fork to maintain. Patching the clone + upstream
PR is a nice-to-have on top, not a prerequisite.

## Context lifecycle: "done for now" marker (seeded 2026-07-16, input rework)

Amy wants a soft "done for now" intent marker on contexts — distinct from
`ContextState::Concluded` (done, sticky, never visit-repromoted) and from
demotion (placement, not intent) — as the hook for automation like
"summarize contexts that are done changing". `Ctrl+A q` (close-and-demote,
`docs/input.md`) works today without it; this is the seed for the semantic
layer above placement. Design question when picked up: a new `ContextState`
vs a stamp alongside `promoted_at`/`demoted_at`/`paused_at`.

## Audio sink follow-ups (seeded 2026-07-16, clip-arc live verify)

- **Metronome stops when kaijutsu-app is backgrounded** (Amy, by ear): the
  rodio scheduler thread free-runs, but cue dispatch + the metronome click
  ride Bevy `Update` systems — an unfocused/minimized window throttles the
  frame loop and the clicks stop. Confirms the sink's *dispatch* is tied to
  the render loop; if background playback should keep time, the
  ServerEvent→scheduler bridge (and the click) need a home that survives
  frame throttling (the rodio thread itself is the obvious candidate). NOT
  touched by R4's prepare-horizon work (`docs/pcm.md`) — this is
  dispatch/`Update`-scheduling territory, a separate lane.
- **CasResolver's SFTP session has no proactive keepalive** (seeded R4,
  2026-07-16): R4 (`docs/pcm.md`) bounded per-fetch recovery with a
  `FETCH_TIMEOUT` + logged redial, closing the "slow + silent" symptom the
  fanfare-clip failure exposed — but nothing yet keeps the session alive
  *between* fetches (no TCP/SFTP keepalive ping), so a long-idle connection
  still goes stale; R4 just detects and redials it promptly now instead of
  ~70s later with nothing in the log. Add a periodic no-op ping on the
  resolver's connection if idle recovery still needs to be faster than
  `FETCH_TIMEOUT` in practice.

## Conversation virtualization follow-ups (seeded 2026-07-16, from `6504fafe`)

The O(visible) virtualization shipped with two accepted v1 limits, named in
the commit message and carried here so they outlive it:

- **Estimated-height placeholders**: first load of a long conversation still
  pays one O(N) layout pass before the window collapses to Display::None;
  seeding never-measured blocks with an estimated height would cap it.
- **Despawn (not just Display::None) offscreen blocks** to reclaim per-block
  RTT texture memory — Display::None leaves the entity + its render target
  alive; long sessions accumulate GPU memory proportional to history, not
  window. Needs the respawn path to rebuild from the block store cleanly.
- Also accepted: a never-measured/stale tail block forced visible while
  scrolled far away can make the shown set briefly non-contiguous for one
  self-correcting frame.

## Anthropic client maturity (seeded 2026-07-15, thinking-enable arc)

Adaptive thinking (`{type: adaptive, display: summarized}`, model-gated)
landed 2026-07-15 in `claude::Client::stream()`. The client is young; we own
it precisely so we can tailor to the provider — gaps observed while wiring
thinking, roughly by priority:

- **No config path into provider clients.** The documented seam
  (`llm_stream.rs` "provider-specific knobs applied inside `Client::stream()`
  from configuration and context state") works, but "configuration" is
  currently a hardcoded default — there is no per-context or models.toml
  route to say "thinking off for this context" or "effort: low here".
  Natural homes: a per-model options table in models.toml, or context config
  KV (`/etc/client`-style cascade). Decide once, then the same pipe carries
  future knobs (effort, `output_config`, fast mode).
- **Model capability knowledge is string-parsing.** `Thinking::default_for_model`
  parses `claude-<family>-<major>-<minor>` and gates on `>= 4.6`. Fine for
  one knob; a second capability (effort levels, sampling-param rejection on
  4.7+, 1M context) wants a small capability table — or a startup query of
  Anthropic's Models API (`GET /v1/models/{id}` returns `capabilities`),
  which is the tailor-to-provider move.
- **`temperature` will 400 on Opus 4.7+/Sonnet 5/Fable if ever set.**
  `BuildOpts.temperature` is currently never set on the Claude path, so
  latent — but nothing gates it. Same capability-table story as above.
- **Cross-model history replay is untested.** A context that switches
  Claude→other-provider (or Claude-with-thinking→haiku) replays
  `ContentBlock::Reasoning` blocks into requests where they may be rejected
  or silently dropped. Hydration/splice may need a per-provider filter.
- **`available_models()` is a hand-maintained list** (opus-4-8 was missing
  until 2026-07-15; fable-5 still absent pending a routing decision). The
  Models API query above would retire it.

## Beat-tracking + local-model follow-ups (seeded 2026-07-15, rten/beat-this arc)

`kj audio beats` (beat-this crate, rten backend) and the rten embedder swap
landed 2026-07-15. Deliberately left out, in rough priority order:

- **Track integration is the real prize**: seed a track's tempo/cadence from a
  reference recording (`kj audio beats` output → transport arm), and run beat
  analysis on rendered/captured clips once the clip seam (`docs/pcm.md`) exists
  (bytes never ride the track; beats are exactly the derived-result shape that
  should cross the wire instead).
- **Model registry / `kj models` verb**: two model dirs now follow the
  `~/.local/share/kaijutsu/models/<name>/` convention (bge-small-en-v1.5,
  beat-this) with install instructions living in models.toml comments and a
  README. A registry (name → expected files → checksum → fetch) would make
  `kj models list/fetch/verify` possible and close the manual-download gap.
  This is also where a `kaijutsu-inference`-style shared crate becomes
  justified — explicitly deferred (Amy, 2026-07-15) until there's real
  sharing; the Embedder trait + per-crate rten deps are the seam until then.
- **models.toml `[audio]`/beat-this section**: the verb hardcodes the model
  dir convention; a config section needs toml_config plumbing + the
  seeded-once CRDT caveat handled.
- **Vendor risk note**: beat-this is v1.0.0, single maintainer (danigb), MIT —
  small enough to vendor/fork if it stalls. rten GPU support (Metal-first) is
  on its author's 2026 roadmap; CPU is fine for our workloads.
- **MCP `shell` tool returns `data: null` for every kj verb** (observed
  2026-07-15 during live verify): even `kj context list`, whose .data shape
  is documented, comes back null over the MCP surface — so `kj audio beats`'
  structured payload is unreachable there too. Either the MCP shell path
  drops KjResult data or it was never wired; find which and either wire it
  through or fix the tool description that promises it.

## MIDI device profiles + device contexts (seeded 2026-07-15, `docs/midi-next.md`)

Design direction captured in `docs/midi-next.md` (living doc): CRDT-owned
device profiles under `/etc/midi/devices/` (rc-style buckets: static `.md` +
kai-synthesized current picture; settings vs capabilities ground-truth
split), track bindings as *device.role* not raw
channel ints, rc-injected device contexts as side channels (profile as skill
body + narrow loadout + cheap model), `kj midi` emit verbs + provenance-tagged
`/run/midi/<device>` state, SysEx via a sink `exchange()` method (transfer
job shape deferred). Nothing in code yet. Slice order in the doc; first
consumer: Minibrute on the laptop app, then the per-track channel-routing fix
(this file → Hyoushigi/Musician area; `docs/chameleon.md` open items) built
on profile vocabulary.

## App (and headless sink) as MCP clients offered back to the kernel (seeded 2026-07-15)

Eventual direction (Amy): `kaijutsu-app` — and the future headless sink
variant — should also be **MCP clients**, offering their local capabilities
(audio devices, capture, render, screenshots?) back to the kernel as tool
surfaces, the way `kaijutsu-mcp` exposes the kernel outward today. Deep work
(client-side MCP host, capability registration, routing through the broker);
deliberately parked — noted while designing audio capture so the capture
seams don't foreclose it.

## Grooming tracks — kaijutsu-style cron (seeded 2026-07-15, MIDI-profiles round)

Scheduled background operations as **tracks**: a slow clock + probe
attachments (`ooda_armed: false`) firing kai scripts on beats. Kinship:
chameleon's cue traps are "cron in musical time" (`docs/chameleon.md`,
unbuilt); this is the same machinery at ops tempo, and the rc synergy is
direct (groomer scripts are CRDT-owned, `kj rc edit`-able). Use cases
queued up: device-profile refresh (`kj midi identify`/`pull` sweeps,
`/run/midi` staleness, pulled-vs-document drift flags — likely first
consumer, `docs/midi-next.md` "Keeping it current"), archive rotation,
index/synthesis grooming, oplog/CRDT compaction, auto-memory grooming.
Needs a design round before code — write the companion doc when the first
consumer is real.

## Pre-existing `clippy --all-targets` deny failure (found 2026-07-12, FSN lane C)

`cargo clippy -p kaijutsu-app --all-targets` currently fails to compile the
test target: `crates/kaijutsu-app/src/text/sparkline.rs:336` asserts against
`vec![1.5, 3.14, 7.0]`, and `#[deny(clippy::approx_constant)]` rejects the
literal `3.14` as "approximate value of `f32::consts::PI`". Unrelated to any
in-flight lane — pre-existing on `main`, just not caught because `cargo build`
(rustc) doesn't run clippy lints and CI apparently doesn't run
`--all-targets`. Either rename the test literal to something clearly not
π-shaped (e.g. `3.5`) or scope an `#[allow]` to that one assertion.
Same class, found 2026-07-13 (r-pump lane): `cargo clippy --all-targets -p
kaijutsu-kernel` fails on `deny(clippy::reversed_empty_ranges)` in
`crates/kaijutsu-kernel/src/llm/splice.rs` test code (`&[3..1]`) — also
pre-existing, also only under `--all-targets`.

## FSN landscape follow-ups (updated 2026-07-13 post-slice-1, `docs/scenes/vfs.md`)

Slice 1 (ambient world) shipped 2026-07-13: kernel-native heat digests,
recency glow, N-archway glow, ship overhead, windows (vfs.md Status). Amy's
reframe — ambient instrumentation, not a file browser — DEPRIORITIZED the
bloom/vi-dive/search items below; they stay on record, not on deck.

- **Generation/staleness invalidation.** `view::fsn::sync::FsnState` still
  caches listings forever once fetched. Slice 1 laid groundwork: activity
  digest entries now carry each directory's current listing-generation
  (`VfsActivityEntry.generation`), so a per-cell stale-detect + re-pull is
  buildable without new wire. Stage-2 inotify remains the real fix for
  non-VFS-mediated writes (generations are blind to them). Deprioritized:
  stale geometry is acceptable in the ambient reading; heat is the live
  signal.
- **Heat drama pass (Amy eyeball).** Live-verified working, but the material
  warm reads subtle at distance: the hue lerp + `HEAT_GAIN_LIFT` (0.6,
  `view/fsn/heat.rs`) compete with baked recency gold on fresh districts, and
  a deep storm reaches visible fields only through ancestor attenuation
  (0.5^depth). Candidates: raise HEAT_GAIN_LIFT, HDR-boost the hot hue, or
  bloom the joints (the solid-tier plan). All consts at tops of heat.rs /
  scene.rs / layout.rs / backdrop.rs, tagged **Amy-tunable**.
- **Root-fetch truncation starves hot districts of their own fields.** The
  "/" fetch (depth 2, 4000-entry cap) truncates before alphabetically-late
  children on a real root (/tmp, /usr, /var got no listing → no field of
  their own; their heat shows only via the root field's material). Slice-0
  behavior, more visible now that heat wants those fields. Candidates:
  per-child follow-up fetches, higher cap, or fetch order by heat.
- **Seam grid is the parent's structural cross, not per-quadrant-occupied
  boundaries** (`view::fsn::layout::seam_grid`'s own doc) — revisit with
  vfs.md Open Question 2.
- **Subdir "bloom" grammar** + **dive into vi on a file cell** — unbuilt,
  deprioritized (ambient reframe).
- **`/` search-and-fly-to** (vfs.md OQ 5) — unbuilt, deprioritized.
- Zone tint (vfs.md OQ 4) untouched. Windows (OQ 3) SHIPPED in slice 1.
- **Portal camera: controls + scripted flybys + heat-directed retargeting**
  (Amy direction, 2026-07-13 — "later tho, just for direction rn"): when
  the portal is focused/fullscreened, add (a) manual camera controls, (b) a
  library of scripted camera moves with the current orbit as the default
  automatic flyby, and (c) data-driven retargeting — e.g. the camera swings
  toward `~/src/kaijutsu`'s district when it heats up. Iterate as more data
  feeds the world (trickle enumeration, stage-2 inotify host weather). The
  `orbit_pose` seam is already the single pose authority for both the RTT
  camera and the visible vessel — a camera-director that outputs poses
  slots in there, and whatever it flies, the dived-world vessel follows
  for free.
- **CAS as a hash-ring neighborhood** (Amy seed, 2026-07-13): `/v/cas` is
  already 2-hex-prefix sharded (256 buckets) — render it as a bespoke ring
  district (a "central neighborhood" rotunda) with shards placed by hash
  prefix around a circle instead of the generic Voronoi field:
  deterministic, stable, visually distinct from directory districts. Today
  `/v` renders as a flat cell at best (sorts past the root-fetch truncation
  + backdrop cap). Plugging it in means opening the layout-mapping seam
  slice 0 deliberately fence-posted (`view/fsn/layout.rs` module doc:
  "deliberately a single pure function, not a mapping-selection system") —
  a per-path layout override with the CAS ring as its first customer.
  Pairs with the mime-keyed CAS / clip-cell ideas in `docs/pcm.md`.
- **`Screen::Fsn` dive is keyboard-unreachable** (2026-07-13, the
  whole-wall zoom retune): Enter on N now fullscreens the portal
  (`station_is_zoomable`, `room/mod.rs`) instead of transitioning to
  `Screen::Fsn`; the dived world, its fly camera, and `toggle_time_well`'s
  sibling paths all still exist and pass tests. Deliberate — Amy: "we will
  probably not have a dive into fsn any time soon." When the dive earns a
  surface again, candidates: Enter-again while zoomed on N (progressive
  zoom; needs same-frame key-ordering care — see `room_keyboard`'s doc), or
  a dedicated key. If it stays unreachable long enough, consider deleting
  the screen instead of carrying it.

## `cargo fmt` unsafe repo-wide (found 2026-07-13, FSN slice-1 lanes; re-bitten 2026-07-16)

**2026-07-16 update (both pcm lanes hit it independently):** the installed
rustfmt (1.9.0, no pinned toolchain, no `rustfmt.toml`) reformats ~350 lines
of pre-existing code even when scoped to a single touched file, and
`cargo fmt -p <crate>` ignores file arguments entirely (one lane reformatted
100 unrelated files before catching it; both caught + reverted via
`git status` before committing). Until a `rustfmt.toml` or pinned toolchain
lands, subagent briefs should say "hand-match surrounding style, do NOT run
a formatter" — the current briefs' `cargo fmt` instruction is a trap.

This environment's rustfmt (1.9.0-stable) disagrees with whatever version
produced the repo's current formatting: a bare `cargo fmt -p <crate>`
reformats ~55–95 pre-existing files (pub mod/use reordering etc.). Both
slice-1 lanes caught it, reverted the collateral, and hand-formatted their
new code instead. No committed `rustfmt.toml` exists. Fix: pin a rustfmt
edition/config that matches the tree (or bite the bullet with one dedicated
whole-repo fmt commit), then re-enable fmt in the loop.

---

## SFTP over the VFS (slices 0–2 + extensions + tracing landed 2026-06-26; slice 3 dissolved; limits + TOCTOU open)

Read + write + OpenSSH extensions ship (`crates/kaijutsu-server/src/sftp.rs`,
the `"sftp"` arm in `ssh.rs`). Two DeepSeek reviews + a Gemini Pro batch
whole-file review are folded. Remaining, in `docs/sftp.md` slice order:

- **Slice 3 dissolved (2026-06-27, `docs/slash-v.md` "Capability")** — SFTP stays
  read/view with the lexical `privileged_write_denied` deny; per-operation join
  on the ambient `context_id` covers the real write surfaces. Surviving crumb:
  register SFTP connections in the participant registry (slash-v track V slice 2).
  Hygiene note (slice-4-adjacent): the lexical deny sits *above* symlink
  resolution — verified not-a-bypass (twice: `LocalBackend::resolve`
  canonicalizes *and* re-clamps with `canonical.starts_with(canonical_root)`,
  `vfs/backends/local.rs:102-113`, so an escaping symlink is rejected
  `path_escapes_root`; and gated paths are a separate `ConfigCrdtFs` mount
  reached by VFS prefix, not OS-symlink-reachable) but the gate belongs below
  resolution.
- **Slice 4 — adapter limits.** Rate-limiting + traversal-depth/size caps to
  survive an editor-indexer crawl (the access-pattern-shift DoS in
  `docs/sftp.md` → Security posture). The open-handle cap (1024/session) is a
  coarse down-payment; also need true streaming `readdir` — `VfsOps::readdir`
  loads the whole entry list, so only the heavy per-entry `File` build is chunked
  today, not the `DirEntry` fetch. **The retained-list angle (gpal batch
  2026-06-27):** `opendir` (`sftp.rs:392`) eagerly materializes the *entire*
  `readdir` `Vec<DirEntry>` into the session handle map at open; an editor indexer
  crawling `/v/ctx` holds many such lists open at once, so the OOM vector is the
  sum of retained `DirEntry` lists across open dir handles, not just one page's
  `File` build. The real fix is paginating `VfsOps::readdir`
  (`readdir(path, offset, limit)`) so the handle holds a cursor, not the list.
- **TOCTOU atomicity refactor.** The write/fsetstat generation guard
  (`sftp.rs:595-608`) has two non-atomic facets. (a) The post-write re-getattr can
  adopt a concurrent replacement's generation. (b) **Concurrent-appender lost
  update** (gpal batch 2026-06-27, verified): `getattr` → generation-check →
  `attr.size` → `write` spans separate `.await`s with no CAS, and APPEND offset is
  `attr.size`. The guard catches rename-replace (its job) but *not* two appenders —
  both read gen=N, both pass, both write at the same offset, one clobbers the
  other. **Scope = cross-session** (two SSH connections to the same path); a single
  client's pipelined writes are serialized by the handler's `&mut self`, so this is
  not intra-session. Returning the new `FileAttr` atomically from
  `VfsOps::write`/`setattr` closes (a); (b) also needs an atomic-append primitive
  or per-path write serialization. Kernel-wide change, worth doing before slice 4.

## `/r` client shares — reverse SFTP (design `docs/slash-r.md`; slices 0+1+stitch SHIPPED 2026-07-13)

Shipped: streaming pump + `VfsOps::open_read_stream` + streaming CAS +
`kj cp` (`ad4b212e`), the full read-only reverse-SFTP loop + held-handle
stream stitch (`99d4e5cd`). Design + review trail in `docs/slash-r.md`.
Remaining, roughly in order:

- **Live verification** — not yet run on a real kernel: kernel restart,
  kaish `ls /r` (check the kaish shadow-overlay papercut that bit `/v/cas`),
  an app launched with `--share`, `kj cp` out of the share, disconnect
  behavior. Needs an app invocation carrying the flag (runner arg).
- **`kj share` verbs** (slice 2) — `ls` (render `/r/index`), eject;
  `/v/session` rows for share channels.
- **`:rw` writable shares** (slice 3) — parsing ships; both-ends enforcement
  + write path don't.
- **Notify push** (slice 4) — client-side watcher → generation bumps +
  activity digests (FSN heat for client-local edits).
- **Generation lookups are unbatched** — the `kaijutsu-generation@` EXTENDED
  request carries `paths: Vec` but `ShareFs::getattr` sends one path per
  call: 2 RTTs per stat, and forward-SFTP `readdir` over `/r` costs
  N×(LSTAT+EXTENDED). Batch at the `readdir` seam when it hurts.
- **Reconnect leaks one `SshClient` handle** per re-dial for the process
  lifetime (`share_dial.rs` — `russh_sftp::server::run` exposes no
  completion signal to know when dropping is safe). Slow leak, reconnects
  are rare; fix wants an upstream hook or a wrapper stream signal.
- **Crawl opacity reuses `snapshot`'s `denied` wire field** — FSN renders
  an opaque `/r` the same as a permission-denied dir; a distinct bit (and a
  deliberate FSN rendering for "someone's machine is here") is follow-up.
- **kaish `cp` still slurps whole files** (kaish-kernel 0.12
  `tools/builtin/cp.rs:202`) — upstream candidate now that the kernel-side
  pump exists as prior art.

## Shared state space + myaku (design `docs/shared-state.md`; myaku detail in git history)

High-level sketches landed 2026-06-28; dedicated design sessions to follow. The
thesis: the VFS *is* the shared-state namespace; tiers are mounts (`/run`
`MemoryBackend` for ephemeral read-write — its own mount, `/scratch` likely retired
— and `/v` for read-only/CRDT durable). No bespoke store. Open work that's already
concrete:

- **`VfsOps::append` (or open-for-append cursor).** No append primitive today;
  `write_all`/`>>` are O(n) truncate+rewrite (`vfs/ops.rs` `write_all`;
  `MemoryBackend::write` is O(1) at `offset=size`). myaku sidesteps via bounded
  rewrite and OODA writes are turn-cadence, so this is not blocking — but an O(1)
  append would make jsonl logs and `>>` cheap. Also closes the SFTP
  concurrent-appender lost-update facet noted in the SFTP section above.
- **myaku pulse facility — RETIRED 2026-06-29** into beat-on-track (a probe is a
  context attached to a system-clock track whose tick writes `/run`; detail in git
  history, `docs/myaku.md` deleted). Surviving open pieces: the `/run` output
  substrate + `pulse_emit` land here (write up the `/run/pulse/<x>/` layout when
  they do); the app `DockSparkline` rewrite-to-read-`/run` note still stands.

## `/v` surfaces (design canonical in `docs/slash-v.md`; track B landed 2026-07-02)

Track B (`/v/cas` + client CAS sync) is LIVE; track V (`/v/ctx` + `/v/session`)
is unbuilt. (`/v/docs`/`/v/input` are kaish-side mounts, not kernel-`MountTable`,
so not SFTP-visible.) The design details live in the doc, shipped-story in
devlog/git; this entry is the backlog pointer:

- **Track B follow-ups (not blocking; the landing incl. the audible
  `kj play --cas` demo is live-verified 2026-07-02):** fetch-on-cue today
  → two-phase **prepare-horizon** prefetch + precise `lead` scheduling (warm the
  cache when a cell becomes known — `docs/pcm.md` "Open questions"); the blocking
  `FileStore` cache read in the async resolve wants `spawn_blocking`; and the
  **clip-record** path (parse Shape A `Clip` → resolve `media`; the audio bytes
  path already exists). Ingest stays `kj cas put` (SFTP→`/tmp` two-step);
  writable staging-over-SFTP deferred; B2 `index` deferred (below).
- **Track B kaibo-review deferrals (2026-07-02, low/pre-existing; the review's
  real findings shipped in `95785e28`).** Left for later, none blocking: **(a)** `CasFs`
  does synchronous `std::fs` inside `async` VfsOps — a large SFTP read blocks a
  tokio worker; this matches `LocalBackend` and is a VFS-layer pattern, not a
  track-B bug (fix the whole layer with `spawn_blocking`/`tokio::fs` if RPC
  latency ever demands it). **(b)** `VfsOps::read_all` casts `getattr().size`
  (u64) to u32 — truncates a >4 GiB file; shared-trait, theoretical for CAS.
  **(c)** a `store()` error after staging leaves an orphan staging file (random
  name; wants the same GC as abandoned uploads). **(d)** `remove()` leaves empty
  `objects/<ab>` shard dirs (cosmetic; `readdir` of one returns `[]`). **(e)** a
  drop-order regression test for `SftpClient`'s field ordering (the contract is
  commented but compiler-invisible). **(f)** client-side `spawn_blocking` for the
  blocking `FileStore` cache read in the async resolve (already an app follow-up).
- **`/v/cas/index` TSV — DESIGNED, DEFERRED (2026-07-02, Amy).** The B2
  resolver file (`hash  mime  size  path`, absolute path column, mime from
  `inspect()`) is fully designed in `docs/slash-v.md` but was **not shipped**:
  nothing consumes it (the client resolver addresses objects by exact hash, never
  by reading `index`), and the first-cut shape — regenerate by walking
  `objects/` (O(N) `stat`+`inspect`) on *every* read, no cache — is
  under-designed and would bake a bad ABI. Build it only with (a) a real
  consumer *and* (b) a cache keyed on a pool-version stamp (invalidate on
  store/remove), or a per-shard `index` (256-way) if a single roster gets large.
  `kj cas ls` covers human listing meanwhile.
- **Track V — `/v/ctx` + `/v/session` (redesigned 2026-06-27 — script-first:
  TSV `index` resolver, sharded pools, symlink edges; no `by-id`/`by-time`/
  `live` farms; no writable `bound` — the capability apparatus dissolved into
  per-operation join, SFTP stays read/view).** V0 `content_len` on `BlockHeader`
  (prerequisite, additive CBOR); V1 `/v/ctx` backend (trailing-byte context
  shards, `blocks/index` ordered by `block_ids_ordered()`, `generation` ←
  `DocumentEntry::version()`); V2 `/v/session` over `PeerRegistry` (+ session
  `kind` field; `context` from live `SessionContextMap`, never KV); V3 SFTP
  mounts them read-only. Deferred optimization (V1 ships naive):
  `block_ids_ordered()` re-sorts per call — cache the ordered `Vec<BlockId>`
  keyed on `DocumentEntry::version()`. Open: huge-`content` range-read vs cap.

## Instrument reframing & RC stances (follow-ups from the 2026-06-22 pass)

The pass that reframed kaijutsu as an instrument, rewrote the rc create-stances,
and renamed `composer→musician` / `explorer→toolie` left these threads open:

- **Toolie taxonomy:** today's `toolie` is the read-only kind (kaibo-explorer
  style). Add a second, Edit-capable toolie that does bounded editing work —
  distinct binding + stance.
- **Future `composer` context_type:** a musically-enabled *synth director* that
  drives many `musician` contexts interactively. The name is now free (the old
  beat-voice `composer` became `musician`).
- **`orchestration.md` needs a fuller rewrite:** stale persona content (personas
  yanked 2026-05-02) and example `explorer` labels remain; only the top-level
  framing was moved off the control register this pass.
- **README doc-table** repoints to `docs/instrument-design.md` in the working
  tree but is uncommitted until that doc lands.

## Architecture & System Design

- **Headless render sink (edge-node agent) — MIDI + PCM:** PCM slice 5c-3
  demolished the server's in-process `AlsaMidiOut` + `kj transport render`, so the
  kernel/server binary now links **no** audio/MIDI FFI (goal achieved). The app is
  the render sink today; a **headless kernel with no app attached makes no sound**
  (MIDI is sink-dependent by design — `docs/midi.md`). The remaining gap: a
  headless edge-node agent that attaches over RPC and plays cues (Symphonia/ALSA
  for PCM, ALSA-seq for MIDI) — `midi.md`'s "first kernel-owned compute node" (M4)
  and `pcm.md` slice 4. Reuses the exact wire `RenderCue` the app consumes; the
  speculation-lead `at`→`lead` scheduling already travels with it.
- **SSH shell subsystem (`kaijutsu-shell`):** give an `ssh` user an interactive kaish
  with `kj` that starts in a lobby and attaches into contexts (VFS reflows on switch).
  Design + wiring captured in [`ssh-shell.md`](ssh-shell.md). Start after the SFTP
  read-path work settles (shared subsystem plumbing). Open decisions noted there:
  per-principal home vs shared lobby anchor (copy the `lost+found` `ensure_*` pattern —
  *not* the global-singleton `scratch` context), and whether `Send`-ness lets it run
  SFTP-style or needs the RPC dedicated-thread treatment.
- **VFS facade delegation:** `Kernel` implements `VfsOps` directly (`crates/kaijutsu-kernel/src/kernel.rs:984`) as a facade. Backend multiplexing already exists — `MountTable` impls `VfsOps` over `MemoryBackend`/`LocalBackend` (`crates/kaijutsu-kernel/src/vfs/mount.rs:261`). The open question is whether the `Kernel`-level facade should delegate more to `MountTable` (and what stays on `Kernel`), not whether to build a manager from scratch.
- **Server RPC Modularization:** `crates/kaijutsu-server/src/rpc.rs` is a massive file (~301KB / ~7,000 lines — by far the largest in the server). The monolithic implementation of the Cap'n Proto traits should be split into smaller modules by domain (e.g., `rpc/vfs.rs`, `rpc/llm.rs`, `rpc/mcp.rs`).
- **`context_type` newtype — declined, not deferred (2026-06-28).** The beat
  coupling that motivated it is gone (arm moved into rc; the gate is "has a track
  lane"). Do NOT make `context_type` a closed `enum` or newtype: it names an open
  **rc-bucket directory** (`project_rc_lifecycle`). Live follow-ons are the other
  axes (decouple-Act-from-ABC; per-type `BeatPolicy`), tracked under Hyoushigi.
- **Cap'n Proto Schema Clarity (doc-only):** The `BlockKind` vs `ContentType` boundary is already settled — `BlockKind` is the structural DAG role, `ContentType` is the raw MIME rendering hint. Remaining work is purely to write that distinction into `kaijutsu.capnp` as schema comments so it stops reading as overlap.
- **Context-type tool policy (unified governance):** The `kj` surface is now
  capability-gated — escalation-relevant verbs check the caller's loadout via
  `KjDispatcher::require_cap` (five authority caps: `drive`/`fork`/`drift`/
  `transport`/`operator`, plus reuse of `rc-write` and the `builtin.block`/
  `builtin.policy` tool caps). `kj` was previously an ungated hole behind
  `facade:shell`. Remaining:
  - Dynamic / principal-scoped overrides.
  - Self-lockout ergonomics (narrowing binding to exclude `builtin.bindings`).
  - Per-principal budgets + fair queuing.
  - **Live contexts need re-create/restart:** broadened role loadouts only reach
    newly-created contexts; existing ones keep their old (now authority-less)
    binding until they're re-created or the kernel restarts. (Editing the seed
    via `kj rc edit` / `kj rc reset` changes what *new* contexts get, not live
    ones — rc fires at lifecycle boundaries, not retroactively.)
- **RPC session reaping — residual only (mostly closed 2026-06-14).** Keepalive
  reaps dead peers (30s × 3) and the watchdog is activity-gated. Residual (by
  design, low): a *truly* wedged `current_thread` LocalSet can't be force-killed
  from outside, and the in-thread watchdog goes quiet with it — that silence is
  the only remaining signal. Not worth chasing until it actually recurs. Related:
  `tech_debt_peer_reattach_on_reconnect`.
- **Post-reconnect re-sync — CLEANUP only (detection + re-fetch delivery both
  shipped 2026-06-24; story in devlog/git).** There are **two `SyncGeneration`
  types** — `kaijutsu-client` (`subscriptions.rs`, currently unused) and the
  app's own (`actor_plugin.rs`, the wired one). Fold the app's into the client
  one (or delete the dead one) as part of moving the cache; moving the whole
  `DocumentCache` into the client is the bigger refactor.
- **LLM providers:**
  - Move per-model knobs out of the config layer (`models.toml`), into the app.
  - Push subscriber for `ConversationMailbox`.
  - **`Registry::resolve_model` pins a bare model name on the *default*
    provider** (`llm/mod.rs:721`) — the sharp edge behind the 2026-07-04
    cross-provider distill bug (fixed by routing the distill default around
    it, not by changing `resolve_model`). Audit its remaining callers for the
    same trap.
- **Reasoning-continuity cross-provider guard (policy, not Rust; the rehydration
  machinery itself shipped):** block `kj context set --model` across provider
  families when signed Thinking exists in history (a DeepSeek nonce fed to
  Anthropic 400s); allow the transition only at `fork`, where an rc script
  decides to elide thinking or downgrade it to plain blocks.
- **Cold start seeds no binding-admin context (want a ROOT director).** The
  bootstrap (`kaijutsu-server/src/rpc.rs:1369`) seeds exactly one **`coder`**
  context (`genesis`) when the kernel comes up with zero contexts — nothing with
  `admin`/`rc-write`. Consequence: any binding-admin op (e.g. repairing a live
  context whose loadout came from a stale seed — see the stale-rc entry under
  Control Plane — or running `kj rc reseed`, which needs `rc-write`) requires
  manually `kj context create <x> --type director` first, since only rc-privileged
  callers or an `admin`-capped context can widen another's loadout, and no
  user-facing shell is rc-privileged. Want: a fresh kernel seeds a **ROOT
  director** (the `director` type already grants `admin`+`rc-write`). Design
  wrinkle: a `director` loadout has **no `drive`/`fork` authority**, so ROOT can't
  itself be the conversational default the app opens into — either seed *both*
  (ROOT director + a usable coder), or have ROOT spawn the coder and let the app
  default to the coder. Confirmed not implemented as of 2026-06-13; genesis was
  repaired by hand this session via a throwaway director.

## Drift — June 2026 audit

- **Extract `ContextRegistry` from `DriftRouter`:** DriftRouter carries ~7
  responsibilities (context registry, per-context LLM config, staging
  queue, dead-letter queue, lost+found lifecycle, context state, trace-ID
  assignment) — `drift.rs:172-563`. Everything that needs "what contexts
  exist" takes a dependency on drift, inverting the hierarchy. Pull
  register/resolve/list/llm-config/trace-id into a `ContextRegistry`;
  drift keeps the queues. Cold-start hydration (`rpc.rs:1150-1183`) moves
  with the registry. (Considered 2026-06-13; deferred — it's a cohesive
  multi-file extraction touching drift.rs + rpc.rs + every "what contexts
  exist" caller, best done when the kernel isn't under concurrent edit.)
- **`drift_flush` is non-atomic over the router lock:** takes the write
  lock four separate times (`kj/drift.rs:422`, `:510`, `:516`, `:521`),
  allowing interleaving with concurrent stage/cancel between windows.
  Document why that's safe or restructure drain→requeue as one critical
  section. (The suspected lock-across-await is NOT real — db lock at
  `:455-471` drops before the `:487` await.)
- **`kj/drift.rs` orchestration bloat:** push/pull/merge/flush each inline
  variations of "insert drift block + record edge + run rc lifecycle".
  Extract the shared operation; the command layer should dispatch, not
  orchestrate.
- **Drift distillation half-integrated:** `build_distillation_prompt`
  machinery sits behind a "drift engines removed" comment + TODO
  (`drift.rs:602-665`). Decide: integrate or delete.

## Turn Loop (kaijutsu-server/src/llm_stream.rs) — June 2026 audit

- **Decompose the agentic loop** (after FlowBus settles; they share event
  paths): mailbox catch-up/snapshot (`:341-391`), cache-breakpoint policy
  via ad-hoc DB reads (`:500-511`), one-shot image resolution that goes
  stale across tool iterations (`:403`), dual-layer timeout semantics
  (`:603-634`) are all inlined in one ~1,235-line file.

## Cleanup — June 2026 audit

- **App-side ABC parse failure renders `Tune::default()` silently**
  (`kaijutsu-app/src/text/rich.rs:413-423`) — render the kernel's
  structured ABC error spans instead. Also: the app re-parses ABC on every
  view; consider a cached AST keyed on block content version.

## Persistence & Sync

- **Phantom KV document row on pre-2026-07-04 DBs (cosmetic).** The KV store
  deletion (`6301e033`) intentionally left live data alone, so a long-lived DB
  keeps one `documents` row with `doc_kind='kv'` (the reserved KV doc every
  startup used to mint). `doc_kind_from_sql`'s unknown-variant fallback loads
  it as `Conversation` (warn-logged), and with the `DocKind::Kv` filter gone it
  now shows as a phantom doc in `kj doc list`. Fix when it annoys: a one-time
  row delete, or teach the fallback to hide retired kinds.
- **CRDT-owned config/rc (design: `docs/config-crdt-ownership.md`) — slices 1+2
  shipped 2026-06-16/17 and long since exercised live** (`kj rc edit`/`kj config
  set` are the daily surface). Remaining: the deferred CRDT scratch mount.
- **rc cutover follow-ups (from slice 1):**
  - **DB-backed test block-store deadlocks `kj::fork` tests.** `test_dispatcher_crdt_rc`
    (DB-backed block store sharing the in-memory `KernelDb` handle) hangs the
    `kj::fork` tests — a latent lock-ordering / re-entrant-`parking_lot` issue.
    Worked around by keeping the *global* `test_dispatcher` db-less + LocalBackend;
    only rc-scoped tests use the CRDT dispatcher. Production runs db-backed and fork
    works there, so it's likely test-harness-specific — but worth a look (could flag
    a real reentrancy risk). Until fixed, the global rc test tree is still host-disk
    (`ensure_rc_seed_files` + LocalBackend), inconsistent with production.
  - **Teach `FileDocumentCache` to pass through CRDT-native mounts.** `ConfigCrdtFs`
    carries an in-memory advancing mtime purely so the cache (used by agent
    `builtin.file:read /etc/rc/…`) reloads after a `kj rc` write. Cleaner: the cache
    skips mirroring `real_path()==None` mounts entirely (read straight through),
    dropping the mtime workaround. Touches all cache consumers — separate slice.
- **Graceful-shutdown WAL checkpoint on SIGTERM:** `SharedKernelState::drop`
  checkpoints only on clean exit, but the server `run()` loop never returns and
  dies on SIGKILL/SIGTERM without unwinding, so systemd `stop` skips it.
  Proactive compaction checkpoints cover durability (no data loss); this gap
  only affects bare-file forensics between the last compaction and shutdown.
  Fix: a `tokio::signal` SIGTERM handler that checkpoints before exit (needs the
  run loop to become interruptible). Forensics hygiene: tracing logs UTC,
  systemd speaks local — cite both zones when recording restart times.
- **`KernelDb` connection pool + god-table — DEFERRED ON PURPOSE (2026-06-16).**
  Currently `Arc<parking_lot::Mutex<KernelDb>>` (`block_store.rs:74`); the file is
  one ~20-table module and every write serializes on the one lock. Recognized
  smell, **not being acted on**: the justifying pressure (measured write-contention
  under concurrent contexts) isn't expected soon, so we revisit only when it's an
  observed problem — do not pre-emptively refactor (annotated at the top of
  `kernel_db.rs`). When it does come up: the single mutex prevents using WAL for
  concurrent readers; migrating to `r2d2`/`sqlx` would allow non-blocking reads
  during LLM streams. Note SQLite serializes *writes* regardless of pooling, so
  the win is concurrent reads (WAL only) — verify WAL first; narrowing lock scope
  may matter as much.
- **Config CRDT ops:** config docs (`DocKind::Config` on `ConfigCrdtFs`) need DTE
  integration so config/rc changes replicate across peers.
- **Theme hot-reload-on-edit (slice 2 follow-up):** the app fetches `theme.toml`
  over RPC only on connect (`apply_theme_from_rpc`). A live `kj config set
  /etc/config/theme.toml` won't re-theme a running app until reconnect. Closing it
  needs the app to subscribe to the config doc (or a config-changed notification)
  and re-fetch. Low priority — theme edits are rare and a reconnect already picks
  them up.
- **`kj config` help doc:** add `crates/kaijutsu-kernel/docs/help/kj-config.md`
  (parallel to the rc/cache help docs) once the surface settles.
- **`blocks_ordered()` allocation churn + sort:** `block_store.rs:185-188` calls `order_key().to_string()` for *every block*, then `sort_by` on the strings — so it's O(N log N) **plus a String allocation per block per call**. It runs on per-frame hot paths (`kaijutsu-app/src/ui/card_stack/sync.rs:48`, `view/components.rs:163`), so the allocation churn is likely the bigger cost than the asymptotics. Fixes: compare `order_key` without stringifying, and/or cache the ordering and invalidate on block change. Add a secondary sorted index when scale demands.
- **Latch state should persist with the context:** 
  - `set -o latch` mode is per-shell and lost on restart.
  - Latch nonces should eventually live in a SQLite table rather than in-memory.

## User Interface (kaijutsu-app) & UX

- **Beat-reference delivery + turn-cadence follow-ups** (deferred from the
  2026-07-15 timestamped-beat-refs fix, merged `0a39718b` + live-verified;
  the arc's story is in the devlog — "The beat learns to carry its own
  clock"):
  - **Delivery head-of-line lane for `block.beat_sync`** (`rpc.rs`
    per-connection forward task, ~2185-2600): one serialized capnp callback
    stream shared with turn output delays refs by seconds during a turn.
    Back-dating makes that harmless for correctness; a dedicated
    low-latency lane matters only if reference latency ever does (live
    tempo ramps mid-turn).
  - **Turn-overlap gate/tuning**: the musician wakeup divisor (32 beats ≈
    16s default) can be shorter than a real turn (~18s on gemma4-e4b), so
    the next OODA iteration spawns before the last finishes — observed
    live, one spawn per wake. No in-flight gate today. A behavior/tuning
    question, not correctness.
  - **`async_broadcast` overflow eviction** can silently drop buffered refs
    on a slow client (warn only); any surviving ref re-locks the phasor, so
    low priority.

- **Score/KJ_HEARD injection is unbounded — a long-lived track drowns every
  musician** (found 2026-07-15 re-establishing the jam): a FRESH musician
  context attached to the morning-old `bassline` track sent **190k tokens**
  on its first turn (47 blocks → 12 messages, so single injected blocks are
  enormous — the track score's committed ABC riding in whole); the original
  `bassline` context had grown to 467k with auto-compaction failing
  (`exceed_context_size_error` on every wake — the turn never runs, the
  track goes silent). Rotation doesn't help: the score outliving the player
  is the DESIGN (docs/tracks.md), so the band view (`KJ_HEARD` / hydration
  of score content) must be **windowed** — recent N phrases, not the whole
  committed log. Decide the window's home (attachment? track policy? the
  musician rc?) and whether drive-path hydration needs the same cap.
  Workaround live today: play on a fresh track (`groove`).
  **2026-07-15 late-day datapoint that sharpens the diagnosis**: `bassline-b`,
  a FRESH context on the YOUNG `groove` track, hit 91k tokens after ~2h of
  16s OODA wakes — so the accumulator is the musician's own conversation
  growing per wake (KJ_HEARD + prompt + response blocks every 16s), not only
  old-score injection. Auto-compaction can't save it: the summarization
  request itself exceeds the model window once past it (the 467k case
  failed exactly there). So the fix needs BOTH a windowed band view AND a
  wake-conversation cap (drop/summarize old wake turns; rotation on a
  token/wake budget rather than phrase count is a candidate). Third fresh
  chair of the day (`bassline-c`) is the standing workaround.

- **`kj drive` on a non-OODA-armed musician silently discards its ABC**
  (cost an hour of verify confusion 2026-07-15): `on_turn_completed`
  refuses to crystallize unless `attachment.ooda_armed` ("not an
  OODA-armed musician we manage") — so `ooda off` + manual `kj drive`
  produces model/text ABC that never reaches the score, no cues, no
  sound, no warning anywhere. Either crystallize driven turns regardless
  of the OODA arm (drive is explicit human intent — arguably MORE
  deserving than an automated wake), or log loudly at the refusal.
  Decide the semantics; the silent path is the bug.

- **Musician create-rc auto-attaches to a label-derived track before an
  explicit `--track` can move it** (bit twice 2026-07-15): `kj context
  create <name> --type musician` runs the create rc, which attaches to
  track `<name>`; a following `kj transport attach --context <name>
  --track <other>` moves the context but leaves a freshly-minted stray
  track `<name>` + score context behind (cleaned up with `kj transport
  delete` both times — tombstones `bassline-b~…`, `bassline-c~…`). Fix
  shape: teach `context create` a `--track` passthrough the create rc
  honors, or make the create rc skip auto-attach when the caller
  will bind explicitly (a `KJ_NO_AUTO_ATTACH` env? a create flag?).

- **Tracker station slice 1: score cells on the grid** (2026-07-15, the
  designed-in seam after slice 0 shipped): rows carry note content read
  from each track's score context (`text/vnd.abc` blocks). Prereq: decide
  the read-a-second-context plumbing — one-shot `get_all_blocks(score_ctx)`
  vs `subscribe_blocks_filtered` + a `SyncedDocument`; `WellTracks.beat_key_of`
  already resolves the ids. Row identity is beat-mod-R with kernel-anchored
  phrase alignment, so cells attach as per-row content children on the same
  `row_offset` math; the column subtree is grouped (header/grid/playhead)
  so cells are an added group, not a restructure. Revisit a per-column
  shader or Vello layer only if room-scale cell text is wanted.
- **Tracker station: Amy eyeball items** (2026-07-15, all Amy-tunable
  consts at the top of `view/tracker/mod.rs` + `palette.rs` "Station E
  contract"): overall grid brightness (rows on `etch`, phrase rows on
  `trough_subtle` after the live-verify swap — dimmer rows may read even
  better), `ROW_SPACING`/`PLAYHEAD_FRAC`/`COL_W_MAX`, dot/glyph sizes, and
  the "TRACKER" title plate seated ABOVE the face (`FACE_H/2 + 44`) sits
  outside the zoomed camera frame so it's effectively invisible — decide:
  move it inside, or delete it (room-scale shows no text by design, and
  you know what you zoomed into). Header abbreviates to `N/PHR` because
  the shared 340×100 plate is single-line-sized; a wider tracker-specific
  plate would fit `/PHRASE`.
  2026-07-11): `room_keyboard`'s Enter dives (`zoomed = Some(TimeWell)`),
  and because the dived-only chain's `run_if(well_zoomed)` is evaluated
  after that same-frame write, `well_keyboard` runs in the SAME frame,
  sees the same `just_pressed(Enter)`, and — `state.selected` persisting
  across dives by design — treats it as a focus-Enter, jumping straight
  to the reading card and skipping the ring-overview stop. The Enter
  analog of the Escape double-fire Slice F hardened (see
  `well_keyboard`'s `.after(room_keyboard)` doc). Pre-existing behavior,
  NOT introduced by the freeze-fix (neither keyboard handler changed);
  only fires when a prior dive left a selection. Fix shape: give
  `well_keyboard` the same freshly-dived guard the Escape fix reasoned
  through — e.g. skip Enter handling on the frame `zoomed` flipped, or
  latch the dive keypress so one press can't be consumed twice. Decide
  first whether "Enter resumes where you were" is accidentally *good*
  (it skips a hop of the skim ladder) — Amy's call before hardening.
- **Shell: message-wall MSDF ticker on a diagonal panel** (seeded
  2026-07-10; this entry's header was restored 2026-07-12 after an edit
  had glued its body onto a neighboring entry): one diagonal octagon
  panel renders MSDF text — messages flowing through (block/drift traffic
  as a scrolling violet ticker, newest line blooms). Design + buildability
  notes in shell.md "Ambient telemetry rules"; rides the existing MSDF
  panel pipeline + event stream. Good next wave after trace-glow ships.
- **Theme: push config changes to connected apps** (found during the color
  pass, 2026-07-12): the app fetches theme.toml only at connect-time
  bootstrap — a `kj config set` mid-session applies to nobody until the
  next reconnect/restart. The `[scene.post]` hot-apply plumbing is already
  in place app-side (`apply_scene_post_on_change` fires on any ScenePalette
  write), so the missing piece is kernel→client: a config-changed
  notification that re-triggers the theme fetch. Matches the still-open
  "live hot-reload-on-edit" note in config-crdt-ownership.md.
- **kj config show needs a --raw flag** (2026-07-12): `show` decorates
  output with a path/length header + toml code fences — piping it into a
  backup file and later `kj config set`-ing that file stores the decoration
  as content (the color pass did exactly this; the app's loud
  parse-failure caught it). A `--raw` mode kills the round-trip footgun.
- **Theme: tokenize the remaining compiled-only color families**
  (2026-07-12, follow-up to the color pass): `block_*` conversation text
  colors, `syntax` highlighting, `md_*` markdown, `sparkline_*`,
  `output_*`, and `agent_color_*` exist only in
  `ui/theme.rs::Theme::default` — theme.toml cannot express them, so
  alternate skins (contrib/themes/tokyo-night.toml) can't restyle them.
  Extend ThemeData + the From impl + theme.toml; keep the
  MarkdownColors/SparklineColors mirror tests in step (they pin
  Theme::default's md/sparkline values today).
- **Shell: drift-layer representation — design question** (Amy,
  2026-07-10): the aurora placeholder is PAUSED. shell.md's "air carries
  drift" stands,
  but before building anything decide *what information* rides the air and
  whether the render is aurora arcs at all — Amy is weighing a point cloud
  with behavior responsive to kernel activity ("lots of cool options") over
  a scripted-pretty arc. Revisit with a couple of concrete candidates
  (blocks in flight, mailbox depth, drift routes?) before spawning geometry.
- **Shell: nameplates fade toward tooltip/debug over time** (Amy,
  2026-07-10): labels stay boring on purpose (TRACKER, not RHYTHM GATE) —
  the intent is that as real detail fills the stations in, the engraved
  plates recede: dimmer with familiarity, eventually maybe tooltip-only or a
  debug toggle. Keep this in mind before investing further in plate polish.
- **`specs_text` orphaned by the HUD-melt slice 4 retirement**
  (`time_well/text.rs`): its only caller was the retired HUD East panel;
  `reading_specs_text` (the reading card's own, header-trimmed sibling) is
  the live surface now. Kept `#[allow(dead_code)]` as a tested pure
  primitive per its own doc's note that the track transport line "rides
  along here until timewell Stage 3 gives it a real home on a track
  surface." Decide when that stage lands: give it that home, or delete it
  (and its dedicated tests) if nothing claims it.
- **Patch bay: extract shared wire-geometry helper** (deepseek review,
  2026-07-09): `selected_chord_apex` re-derives the group→seat→angle→chord
  pipeline that `rebuild_patch_scene` also computes (identical today,
  verified). A future edit to one side floats the inspection card off its
  chord. One pure `wire_geometry(snapshot, wire_idx)` helper, both callers.
- **Rename `BlockScene` → `BlockContent`:** the component no longer holds a
  scene (scene + `built_*` live on `VelloUiScene`); it's now pure build-
  bookkeeping (`content_version`/`last_built_version`/`scene_version`/`text`/
  `color`). Name is misleading. Mechanical rename across `block_render.rs`,
  `lifecycle.rs`, `overlay.rs`, `shell_dock.rs`, `render.rs`.
- **Verify two unexercised render surfaces:** (1) a Vello-content *cell*
  (ABC/SVG/sparkline, `has_vello_content == true`) rasterizing via
  `render_vello_scenes` then compositing MSDF labels on top — needs a
  conversation with rich content; (2) the unfocused-pane summary, the one
  surface on Bevy's native `Text` pipeline (`tiling_reconciler`), needs a
  multi-pane layout. All MSDF-only surfaces + docks + role borders verified.
- **Vi editor command mode (Slice 3, `docs/vi.md`) — steps 1–3 shipped; open
  remainders:** runner-verify the slice-3 polish (capnp `@6` ⇒ kernel+app
  rebuild+restart; eyeball `:r !cmd` splice, bad-`:cmd` E492 on the strip, `fg`
  from a second window; also the 2026-07-07 error-channel unification —
  dirty-`:q` E37 and a failed `:r` must show on the strip, not vanish);
  **step 4 `:e <path>`** (rebind the session to another
  block) deferred; the Ctrl+Z shell may become a **shadow context** (its own
  design pass; `project_shadow_context_shell` memory).
- **Vi editor — residual `config_owned` prefix on the cache-invalidation path.**
  `resolve_editor_target` now decides config-ownership via the mount table
  (`MountTable::owner_of` + `VfsOps::owns_config_docs`, 2026-06-27), but
  `Kernel::invalidate_config_file_cache` still uses the hardcoded `config_owned`
  prefix check. It's the **sync** guard on the sync `editor_quit` path; routing it
  through the async mount-table query would cascade `editor_quit` (+ its wire
  handlers) to async. Unify when that path is reworked, or add a sync
  mount-ownership lookup. Low stakes (cache-coherence optimization), but it's a
  second source of truth for config-ownership.
- **User presence (novel surface):** The compose input is a shared CRDT document. Surfacing in-flight compose state to an opted-in model would enable mid-sentence collaboration. Gate with explicit user opt-in.
- **Connection Polling Efficiency:** `ActorPlugin` in `crates/kaijutsu-app/src/connection/mod.rs` polls broadcast channels every frame. While `UpdateMode::reactive` helps, consider event-driven wakeups or bridging async streams directly into Bevy events more efficiently if latency/power becomes an issue.
- **Card-stack view:** Card size tuning, read-only scroll on focused card, dive-in (Enter), mouse click to focus, momentum scrolling, camera parallax, streaming card texture updates, card grouping evolution, ambient environment.
- **Card-stack texture quality (3D direction):** the renderer presents vello/MSDF
  content as textures on cards, so the 3D move brings (a) **mipmaps** on block/card
  textures — cards receding in perspective shimmer without them; (b) **reading-mode
  hi-res re-render** — promoting a card close to the camera re-renders its content at
  higher resolution (discrete, debounced — same machinery as re-render-on-change);
  (c) **MSDF live-quad escape hatch** — MSDF's scale-independence is spent at bake
  time, so if reading-mode text quality disappoints, render MSDF as live quads in the
  3D scene (the atlas + shaping pipeline already support it; a renderer change, not
  architectural). Arbitrary zoom over vector content is explicitly declined.
- **Text rendering (MSDF / 次):** TAA temporal super-resolution, glyph spacing per-font tuning, 1-frame blank flash on texture resize, large-context Vello "paint too large" crash.
- **MSDF whole-document settle window (residual, after the 2026-07-03 atlas
  fixes `a6734cbf`).** The silent failure modes are gone (atlas grows to 4096,
  terminal failures are loud, the respawn loop is dead), but the *transient*
  is inherent: async glyph generation means a freshly loaded document shows
  partial text for a few frames until the last atlas batch lands and
  re-composites. If it still reads as jank, the polish is presentation-side:
  hold a block's texture (or fade it in) until its first *complete* composite
  — every glyph region present — instead of showing partial bakes.
- **Pre-existing clippy deny blocks full-crate `--tests` runs:**
  `text/sparkline.rs:331` uses `3.14` as a test literal and trips
  `deny(clippy::approx_constant)`, so `cargo clippy -p kaijutsu-app
  --all-targets` fails before reaching new code (both 2026-07-03 fix agents
  had to allow-list around it). Rename the literal (e.g. `3.5`) or allow the
  lint on that test. Also: `ExtractedMsdfAtlas::default()`
  (`text/msdf/renderer.rs:425`) hardcodes `1024` duplicating the atlas's
  initial size — harmless (pre-first-extract only) but now that the atlas
  grows at runtime the magic number is worth deleting.
- **Pre-existing clippy deny blocks `kaijutsu-kernel --tests`/`--all-targets`
  too (found 2026-07-11, gist-line lane):** `llm/splice.rs:267` builds a test
  fixture range `3..1` (deliberately reversed, exercising `plan_splice`'s
  edge-case handling) which now trips `deny(clippy::reversed_empty_ranges)`.
  `cargo clippy -p kaijutsu-kernel` (lib only) is clean; adding `--tests` or
  `--all-targets` fails the whole crate before reaching anything else. Same
  shape as the `kaijutsu-app`/sparkline entry above — rewrite the fixture to
  express "empty range" without triggering the lint (e.g. `#[allow]` on the
  test, or a helper that constructs the range without a literal clippy can
  reverse-detect) rather than changing `plan_splice`'s actual behavior.
- **Auto-follow on local submit:** the conversation only re-engages
  scroll-follow when already at the bottom
  (`view/sync.rs:200-206`); a shell-dock submit is a strong signal of
  intent to watch the result — force `start_following()` on local
  submits (mirror the `InputCleared` handler at `sync.rs:309`). A
  "new content below" affordance would cover non-local appends.
- **Live-verify the error-block ordering fix (view-order holes, fixed
  2026-07-03, `a47c9a18`).** The three diagnosed mechanisms are fixed with
  unit + headless-App tests (see devlog), but the original "errors pinned at
  the bottom" symptom (2026-06-17, session `019ed674`) was never reproduced
  live before the fix landed. Next time an agentic session produces mid-turn
  error blocks, watch for the new fail-loud `error!` logs from
  `reorder_conversation_children` — if they fire, the upstream
  container-entry gap they point at is the remaining bug to chase.
- **Verify the interrupt ladder actually cancels an in-flight drive**
  (originally observed 2026-06-17 as "triple-Esc doesn't interrupt";
  reframed 2026-07-16 by the input rework: Esc is vi's/PopLevel now,
  interruption is **Ctrl+C**'s job — docs/input.md). The app side fires
  `interrupt_context(ctx, immediate)` on the Ctrl+C ladder; what was never
  confirmed is the kernel side cancelling a mid-drive turn/tool loop
  (rather than only a streaming LLM turn). Next agentic session: Ctrl+C
  twice mid-drive and watch whether the loop actually stops.

## Control Plane & Navigation (kj)

- **`kj transport restore` — only if delete accidents actually happen**
  (decided 2026-07-15 with the tombstone delete): recovery is sqlite-only by
  design (the one-line UPDATE is in `kj transport delete --help` +
  docs/tracks.md). If someone actually fat-fingers a delete and the sqlite
  path proves annoying, a `restore --track <tombstone-name>` verb is the
  shape (rename back + clear `deleted_at`; refuse if the original name has
  been retaken by a fresh track).
- **`--out` writes bypass the VFS (`kj cas get` + `kj block cat`; gemini-pro
  review 2026-07-04).** Both verbs `std::fs::write` the `--out` path
  (`kj/cas.rs:119`, `kj/block.rs:730` — the new verb deliberately mirrored the
  old one's convention). Not a trust issue (shared-trust kernel) but a
  coherence one: the write lands relative to the *server process* cwd, not the
  shell's VFS cwd, and never hits VFS mounts/caches. Decide once for both:
  route `--out` through `VfsOps::write_all` (needs the block/cas dispatch arms
  async) or document host-side semantics loudly.
- **`KjBuiltin` argv/stdin quirks (gemini-pro review 2026-07-04, both
  pre-existing/low):** (a) the `--json` global flag is stripped with
  `argv.retain(|a| a != "--json")` (`runtime/kj_builtin.rs:524`), which also
  eats a literal `"--json"` passed as a *value* (e.g. `--content --json`) —
  prefer a targeted named-arg strip; (b) `wants_stdin_content` promotion means
  a forgotten `--content` on an interactive TTY blocks reading stdin until
  Ctrl+D instead of failing "missing content" — cat-like POSIX behavior, but a
  papercut worth a TTY check if it ever bites; (c) the `{other:?}` fallback
  arm in argv reconstruction (`:499-502`; same pattern in positionals `:450`)
  would Debug-format a future non-Array `Value::Json` into a garbage token —
  no trigger today (deepseek: accepted risk), but the arm should fail loud if
  kaish ever grows a new value shape.
- **Workspace path mount points:** `kj workspace add --mount <target>` was
  documented + parsed but silently ignored (no backing storage) — removed during
  the clap migration so it now fails loud. To implement: add a `mount` column to
  `WorkspacePathRow` (`kernel_db.rs:168`, SQL migration), thread it through
  `workspace_add` and the context-mounting path, decide mount semantics, then
  re-add the `--mount` flag + help example.
- **Tab completion:** Context labels, preset labels, workspace labels, tag syntax. Integrate with kaish.
- **Cross-kernel drift:** Schema preserves `kernel_id` everywhere; not yet implemented.
- **Compact quality:** Distill model selection, preset-level or context-level summary-style control.
- **POSIX context quartet:** Implement `kj wait` and `kj stop` to complete the fork/drive/wait/merge paradigm.
- **`kj drive` follow-up:** Add verb-level refusal for driving Staging contexts.
- **Autonomous turn runaway guard:** Add a `drive_depth` cap to prevent unbounded fan-out from `--prompt` forks.
- **TurnFlow bus lossy + in-memory:** overflow eviction is now LOUD (`FlowBus::publish` warns when a full channel drops a slow subscriber's oldest event, `flows.rs`); the zero-subscriber case was already surfaced by `kj drive`/`kj fork --prompt`. Durable delivery (persistence) for `turn.*` remains the follow-up.
- **Headless turn cwd is `/`:** Decide whether to thread the context's stored shell cwd into the headless `ExecContext`.
- **`--switch --prompt` double-drives:** Clarify semantics when both human and autonomous turn try to drive a child.
- **Context-type ↔ fork asymmetry (discovery 2026-06-17, fork code is fresh —
  worth a code-side look).** `--type` exists only on `kj context create`
  (rc-dispatch `context_type` → selects which `/etc/rc/<type>/` bundle runs), NOT
  on `kj fork`. Fork inherits the parent's type and re-runs the *parent type's*
  `fork/` bundle, so **there is no way to fork into a different type** — switching
  type means `kj context create --type <T> --parent <src>`, which gives a
  structural edge but (apparently) none of fork's history/preset copy semantics.
  Observed: a `context create --parent .` shows `Fork: <id> ()` — empty parens
  where `kj fork` shows the preset (e.g. `Full`/`Window`). Open questions for the
  fork/create code (`kj/fork.rs`, `kj/mod.rs` context_create, `rpc.rs`
  create_context_inner): (a) is the type-on-fork omission deliberate or just
  unbuilt? (b) does `context create --parent` copy ANY blocks, or only wire the
  DAG edge — i.e. does a director created this way see what it needs to coordinate,
  or start blank? (c) should `kj fork --type <T>` exist (fork history + run the
  *target* type's create/fork bundle) for the common "branch this work into a
  director/toolie" move? Surfaced while standing up a `director` context to
  experiment with coordination.
  - *Reconfirmed 2026-06-17: the child's block log was its own rc output (`system/text` stance,
    `system/notification` tool-adds, S10/S20 rc traces) plus the seed
    `--prompt`; **zero blocks copied from the parent**. So the create-with-
    parent path starts the child blank (correct for a clean coder, wrong if
    you wanted fork's history). Strengthens the case for (c) `kj fork --type`:
    the director's natural move is "branch this work into a coder *with* the
    working context," which neither verb currently does in one step.

### kj / MCP ergonomics (UX)

- **Stale rc seed → live contexts keep broken loadouts (detection SHIPPED
  2026-07-04; repair gap remains).** rc is seeded-once, so a live script can
  drift behind its embedded default; the recurring symptom was contexts created
  from a stale `S10-binding.kai` missing newer authorities. The *detection*
  half shipped: `kj rc list` now marks each script in-sync / differs-from-seed
  / no-seed (live body vs `seed_body()`, seed-shape-aware for symlink seeds),
  with per-entry records under a new `--json` flag; `kj rc reset <path>`
  remains the manual pull (live is truth, no auto-overwrite). Remaining gap —
  the worse half: `reset`/`reseed` only fix *future* contexts. A context
  already created from a stale seed keeps its broken loadout and can only be
  repaired from a binding-admin context, which the cold-start bootstrap
  doesn't provide (see "Cold start seeds no binding-admin context" under
  Architecture).
- **`local` is a kaish reserved word (like `set`).** `--model local` lexes as
  the `local` builtin keyword → `found ';' expected identifier`. Same class as
  the `set` reserved-word gotcha; quote it (`--model "local"`) or pass the full
  spec. Consider letting reserved words bind as plain args after a flag.
  (kaish-lexer change in `~/src/kaish`, not kaijutsu-side.) NOTE: alias
  *resolution* is now fixed — `kj context create/set --model "local"` expands
  the `models.toml [model_aliases]` entry to its concrete `provider/model`
  before storage (`resolve_context_config`, 2026-06-14), so the quoted form
  works end-to-end; only the bare-`local` lexer footgun remains.
- **Turn-loop timeout gaps (residual of the local-model stall, re-triaged
  2026-06-16; the dual-layer watchdog + tool-free player loadout cover the main
  path).** Genuinely unguarded: (a) the `provider.stream()` start `.await`
  (`llm_stream.rs:815`) has retry/backoff but **no explicit timeout** — a provider
  that accepts the connection but never returns the response object leans on
  reqwest's defaults; (b) pre-stream hydration / cache reads have no timeout, so a
  wedge *before* the stream loop emits no terminal event. Fix each with an
  explicit timeout + a regression test that wedges the path and asserts a loud
  `TurnFlow::Failed`. Also worth: per-provider/per-context `default_tools` as the
  norm so players never get `all`; per-model timeout overrides if 30s/300s ever
  prove wrong for a slow local model.
- **P3 — external `mcp__kaijutsu__shell` `data` needs a persisted block field.**
  The *in-kernel* `builtin.shell` now carries kj's `.data` in its `structured`
  envelope (shipped 2026-06-14, `mcp/servers/shell.rs`), and `kj <cmd> --json`
  returns the payload in stdout for any consumer. The remaining gap is the
  *external* `mcp__kaijutsu__shell`, which observes the result via CRDT sync
  (polls a block snapshot, reads `snapshot.output`) rather than a return value.
  Root cause (traced 2026-06-14): kj sets `ExecResult.data` (a kaish `Value`),
  but the server's `shell_execute` only persists `ExecResult.output()`
  (`OutputData`) onto the block (`rpc.rs:6104` → `set_output`), and the block
  carries only `output: OutputData` — which can't faithfully hold arbitrary JSON
  (an inspect object). Faithful fix: a new persisted `data` field on the block,
  mirroring the `.output` vs `.data` split — thread through `kaijutsu-types`
  `BlockSnapshot`, `kaijutsu-crdt` (content/document/block_store), the capnp
  `BlockSnapshot` wire (the real cost — three-binary bounce), then `set_data` in
  `shell_execute` and read it in the MCP `to_json` (`kaijutsu-mcp/src/lib.rs`).
  CBOR oplog evolution is additive (safe); capnp is the work. P3 because the
  `--json` envelope already unblocks consumers today.
- **External shell-hang fix — one residual verification.** The 2026-06-17
  executor-starvation hang is fixed (`SubscriberHealth` reap tolerance +
  `resubscribe_blocks` + joined-context-scoped subscription; story in devlog).
  The server fix is verified live; the *client-side* scoping + resubscribe (2,3)
  ride in the MCP binary and are covered by `e2e_shell` until a session whose MCP
  binary is rebuilt confirms them in situ. Related: P3 above +
  `project_mcp_synceddocument_sync`.
- **mcp-context default model is an invalid id (observed 2026-06-17).** A context
  created via `register_session` (context_type `mcp`/`default`) defaulted to
  `anthropic/claude-haiku-4-5-20250101` (also seen as `…-20250929`) — a wrong date;
  the valid id is `claude-haiku-4-5-20251001`. Chat turns fail with
  `not_found_error` after 3 attempts. Fix the default model id wherever mcp/default
  contexts are seeded.
- **`builtin.file` hardening — remaining (small; the byte→char corruption fix +
  hashline addressing shipped 2026-06-17, story in devlog +
  `project_file_tools_hashline`):** (1) in-context recovery affordance — expose
  `git`/a revert or `kj block diff --original` in the kaish shell; (2) the
  post-write verification reads the CRDT cache, not the VFS disk, so a faulty
  flush is only caught by `flush_one`'s own error (documented in `edit.rs`);
  (3) `FileDocumentCache` CRDT-native pass-through (tracked under Persistence &
  Sync) would let `read`'s hashes anchor `/etc/rc` cleanly.
  - **kaish-side build-out — design direction (not yet built).** The hash is an
    *edit-addressing* feature, so the kaish read surface wants **two read modes**:
    keep `cat`/`tail`/`sed`/`grep` streaming + **hash-free** (logs/huge files; never
    materialize), and put hashes only on a **bounded, dedicated `read` verb**
    (window-scoped hash, range arg, `--json`) paired with `edit --anchor`. To serve
    **kaibo** (only has `run_kaish`), push `line_hash` *up* into the kaish crate
    (`~/src/kaish`) as a builtin; the MCP tools become thin wrappers. Rejected: a
    `hashread`/`hashedit` pair (the edit half duplicates `edit --anchor`; doubles
    standing tool-desc tokens) and `cat -H` (cat is the large-file streaming dumper —
    a hash flag invites whole-file hashing). Add a size guard so the hashline reader
    declines huge files. (Kaish-crate work, kaijutsu-driven.)

- **`StreamingBlockHandle` implementation:** Single-block streaming primitive.
- **LLM streaming rewrite:** Move `process_llm_stream` onto `StreamingBlockHandle`.
- **Block content abstraction:** Blocks as containers for multiple content artifacts.
- **MCP `progress` → `StreamingBlockHandle` bridge.**

## Domain-Specific (ABC Parser & Engraving, Index)

- **kaijutsu-abc MidiWriter leaves pitch/velocity unmasked** (gemini review
  fallout, 2026-07-09): `note_on`/`note_on_channel` build raw channel-voice
  bytes without the `& 0x7F` data-byte mask that `kaijutsu-app::midi::click_bytes`
  now applies. Safe today — the app's only caller uses
  `MidiParams::default()` (fixed velocity 80), nothing config-sourced. Mask
  at the writer if `MidiParams` ever becomes config-driven.

- **`hnsw_rs` reverse-edge quirk:** Reverse edges written at neighbour's assigned layer.
- **Embedder: BERT-only I/O contract** (2026-07-12 index review): `OnnxEmbedder`
  hardcodes `input_ids`/`attention_mask`/`token_type_ids` and mean-pools
  `outputs[0]` (`kaijutsu-index/src/embedder.rs`). E5/jina-style models (no
  token_type_ids, CLS pooling, or a ready pooled output) won't load. Growth
  path: introspect `session.inputs` for the input set + a small per-model
  manifest (pooling strategy) in models.toml. The `Embedder` trait is the seam;
  nothing structural blocks this.
- **Embedder: serialized CPU-only inference** (2026-07-12): one ONNX session
  behind a `Mutex` with `intra_threads(1)`; no execution-provider plumbing
  despite the GPU box. Live data point: `kj synth all` over 54 real contexts =
  ~8 min wall clock (memory now bounded by embed_batch chunking, but the FLOPs
  are all one thread). When it cracks, the reviewed playbook (gemini
  deliberate 2026-07-12) is two-phase indexing: reserve slot under the
  metadata lock, embed lock-free, re-take the lock, re-verify content_hash,
  write — plus intra_threads / EP selection in `[embedding]`.
- **Index: slot-space vacuum watermark** (gemini deliberate, 2026-07-12):
  slots are monotonic-never-reused by design, so max-slot-ever grows with
  lifetime churn — the embeddings cache is `Vec<Option<…>>` sized by it (~24B
  per dead slot; harmless at human scale, unbounded in principle). When
  warranted: watermark trigger (e.g. `next_slot > 2 × live rows`) → offline
  compaction that renumbers into a fresh generation (new graph + one SQLite
  transaction rewriting slots). Deliberately NOT built now.
- **Index: synthesis child tables lack FK cascades** (gemini deliberate,
  2026-07-12): cleanup is manual transactional DELETEs across three tables;
  correct today, but a future fourth synthesis-adjacent table that someone
  forgets to add to `delete_synthesis_rows` silently leaks ghost rows that
  re-hydrate. `PRAGMA foreign_keys=ON` + `ON DELETE CASCADE` needs a
  table-rebuild migration (SQLite can't add constraints in place) — do it
  next time the schema changes anyway.
- **Index: unopenable index_meta.db disables the index** (deepseek review,
  2026-07-12): if SQLite itself won't open (true corruption), SemanticIndex
  errs → kernel degrades to no-index, and recovery is a manual file delete.
  Arguably should treat unopenable-like-mismatched: wipe + start fresh (it's
  a derived cache). Low likelihood (WAL), low cost to leave.
- **ort 2.0 stable watch** (2026-07-12): pinned `2.0.0-rc.12` (latest; no
  stable 2.0 published). Re-check occasionally; rc-series has broken API
  before. ort arena never shrinks — chunked embed_batch keeps its peak ~1GB.
- **ABC multi-tune files vs blocks:** Split tunes across sibling blocks or stack inside one block.
- **ABC file-header inheritance:** `M:`/`L:`/`Q:` defaults prevent proper inheritance.
- **ABC features:** `I:linebreak`, `m:` macro expansion, `%%` directives, Unicode escapes/fonts.

## Viz substrate (kaijutsu-viz) — plan in `docs/timewell.md` (substrate notes in its appendix; `viz-substrate.md` retired 2026-07-04)

- **Pause gating (suspend activity)** — the `z`/`kj context pause` verb ships
  design-only (2026-07-05, `dcbb75e4`): `paused_at` persists and the card dims,
  but nothing behavioral gates yet. The decided semantics (Amy): a paused
  context receives **no beat/OODA wakeups** (seam: hyoushigi attachment wakeup
  fire) and **rejects turn-starts loudly** with a resume hint (seam: kernel
  turn-start). Both seams are documented on `ContextRow::paused_at`. Do as its
  own slice; decide then whether human submit auto-resumes or fails loud.
- **Ring placement residuals** (explicit-placement review, 2026-07-05; both
  reviewers, accepted-not-fixed): promote's ring-full refusal (and other verb
  errors) reach only the log from the app's fire-and-forget keys — a HUD
  toast/flash slot is wanted; the 10-seat cap check is read-then-write under
  the single KernelDb mutex (atomic enough in-contract — direct DB writers are
  already forbidden); `conclude` RPC accepts Staging contexts (pre-existing,
  probably fine, never decided).

- **Time well evolution — plan is canonical in `docs/timewell.md`.** Staged:
  0 tourniquet + 1 idle-age recency (both SHIPPED 2026-07-03 — Stage 1's app
  half landed as the four-ring carousel, not the terraced spiral; see the doc's
  Status) → 2 stable `0–9` rank slots (kernel-owned, mux semantics) → 3
  `TrackInfo`/`listTracks` + optional-cadence attachment + track decks in the
  well (wire slice SHIPPED 2026-07-04, with the live-state layer: tails,
  beat phasors, track rays — see the doc's Status) → 4 track→context→detail
  progression → 5 event-horizon cutoff + LOD + `/` archive search → 6 polish.
  Individual entries below fold into those stages as they ship.
- **Live tail misses streaming model text (found building the live layer,
  2026-07-04).** `live::tail_line` skips empty inserts because model prose
  streams in via CRDT `BlockTextOps` the well doesn't decode — the HUD South
  tail shows whole-content blocks (prompts, tool calls, score cells, errors)
  while a streaming turn only reads as chatter glow + running rim. Refinement
  candidates: decode ops for the *selected* context only (the conversation
  view already has the machinery), or re-fetch the block head on its
  `Done`/`Error` status flip. Bound whichever lands to the selected card.
- **Track rays don't organize the cards angularly (deferred by design,
  2026-07-04).** Cards seat evenly by recency within their band ring; a
  track's cards ignore the ray's bearing. The follow-up is the haystack
  grammar applied to angle — same-track contexts gravitate toward their
  track's ray (`rays::ray_angle`) within each ring, unattached trailing.
  Needs care against the "predictable motion" bar.
- **In-world band labels — still TODO (Stage 1 residue, re-anchored to rings).**
  "HOT NOW" / "THIS WEEK" / "30 DAYS" / "HORIZON" floating at each ring, per
  `docs/timewell.md` "The bowl, revisited". The pure helpers
  (`card::band_label_pos`, `card::band_label_text`, the `LABEL_RADIUS_OFFSET`
  placeholder) exist and are tested, but no entity spawns/renders them — wiring
  is an MSDF panel per band (`panel::create_msdf_panel`, the `ReadingCard`
  pattern), gated on font-asset load the same way `text::build_card_scenes` is,
  and — landmine — pass the brush explicitly to `VelloFont::layout`/
  `collect_msdf_glyphs` or the text renders black. The band-boundary constants
  (`HOT_NOW_MILLIS`/`THIS_WEEK_MILLIS`/`THIRTY_DAYS_MILLIS`, `layout.rs`) are
  placeholders Amy tunes live once labels are on screen.
- **HDR bloom follow-on:** drive the well cards' SDF rims/pulses to HDR (>1.0)
  so they bloom brightly (`WellCardMaterial` `params`/emissive). (The shared
  single-camera HDR+Bloom fix itself shipped 2026-06-17; devlog.)
- **Card readability:** text is small at the default framing; tune when the
  active view (timewell Stage 6) lands.
- **Edge HUD follow-ups (panels shipped 2026-06-18; devlog):** the mid/lower
  E/W sides are open canvas — candidates for the drift arcs / activity layer or
  a secondary readout; the E specs panel wraps a long model badge (cosmetic).
- **RTT follow-up (rename/split shipped 2026-06-18):** `overlay.rs` /
  `shell_dock.rs` could adopt `create_msdf_panel`/`commit_panel_glyphs` for
  their MSDF surfaces (optional, low).
- **Time-well — deferred UI ideas.** All real, none blocking; parked on purpose
  (see `docs/timewell.md` → Execution notes, "Parked on purpose"):
  - *JOIN dive (mockup 34):* the committing Enter currently just switches
    context + leaves. The cool version continues the camera *through* the focus
    card so it unfolds into the conversation — one continuous focus→enter
    gesture. Polish ideas: fade/dim ring cards while focused; tune focus-card
    size/pos (it's large in the overview).
  - *Clean Running-pulse re-check:* the per-context teal Running rim is
    mechanism-proven (identical shader path as the verified selection/lineage
    rims) but never caught in a clean live screenshot — the earlier attempt was
    blocked by the (now-fixed) MCP-shell hang + a bad mcp default model id. A
    ~5-sec re-check once a working-model turn can be staged.
  - *Drift arcs / particle layer (gap 4):* the bigger drift visualization —
    arcs/particles *between* the source/target cards, not just the per-card
    shimmer already shipped. Needs a new context→context drift-edge *list* wire
    (the per-card shimmer rode the existing staged-queue poll; arcs can't).
  - *Ring 3-down is unreadable at the gate framing* (Amy, 2026-07-06). With
    the camera framed on the focused ring's gate, the third ring down
    (Bumped when focused on Active) sits too oblique/deep to read its cards.
    Needs a different angle or framing idea — maybe steepen `RING_CAM_LIFT`
    per focus depth, or tilt deeper rings' cards toward the camera
    (`card_tilt` is the parked per-band knob).
- **Time-well ring-carousel — review findings (2026-07-03, gemini-pro batch +
  deepseek).** The ring-per-band carousel (`band_ring`/`ring_seat_rotated`,
  ring-centric nav, projector spin-to-gate, focus dimming) got a two-model
  review. The safe wins (per-frame change-detection guards on the easing
  systems; dead `card_tilt` multiply gated; stale `ring_seat` gate doc) are
  **applied**. Remaining, recorded not-yet-fixed:
  - *Cuboid face UVs by hardcoded vertex index are fragile* (gemini, medium).
    `card_block_mesh` (`scene.rs`) V-flips the front face as indices `0..4` and
    (since 2026-07-06) sentinels the side faces as `8..24`, which breaks if
    Bevy changes its cuboid vertex order. Robust fix: classify faces by
    `ATTRIBUTE_NORMAL` (front ≈ `[0,0,1]`, sides ⟂ Z) instead of index ranges.
  - *Redundant per-tick sort* (gemini, simplification). `sync_time_well` calls
    `spiral_positions` (sorts via `band_orders`) **and** `ring_cards =
    band_orders(...)` — the same recency sort twice per tick. Call `band_orders`
    once and derive the flat odometer order + `(band, within_index)` pos-map from
    that single result.
  - *Passive-aging short-circuit* (gemini, design). `sync_time_well` early-exits
    on an empty join diff, so cards don't re-band as wall-clock time passes — a
    context won't demote to a deeper ring on idle alone until some *other* diff
    arrives. Ties to the ring-MEMBERSHIP / coarse auto-decay thread (explicit
    hot-row + coarse decay, see `signoff.md`): the band derivation likely needs a
    coarse timer independent of the block diff.
  - *`build_card_scenes` rebuilds MSDF glyphs on param-only changes* (deepseek,
    minor). Selection/lineage/drift toggles flip `Changed<Card>`, which re-lays-out
    glyphs even though only `mat.params` changed. Split the `mat.params` sync into
    a separate `Changed<Card>` query that doesn't re-shape text.
  - *Empty-ring spin* (deepseek, minor). A focused but empty ring still eases its
    rotation toward the gate each tick (harmless); a `flen > 0` guard would skip it.
  - *Spin chaining on rapid reversal* (deepseek, medium → downgraded on code-read).
    `spin_target_to_gate` measures the short path from the accumulated *target*, not
    the eased position, so a very fast direction-reversal could feel like the ring
    keeps going before reversing. **Not a correctness bug** (resting target is the
    gate π, steps are one-card; math verified sound by both models) — a possible
    feel-tuning item only.
  - *VERIFIED-FINE, do not re-audit:* gemini's "CRITICAL: no system writes Card
    flags → material params" is **false** — `text.rs build_card_scenes` (a
    `Changed<Card>` query) writes `mat.params = card_params(card)`; gemini's batch
    context simply lacked `text.rs`. Selection/lineage/drift/status render correctly.
- **`ScaleLinear`/`ScaleTime` round-trip loses precision under extreme
  domain→range compression** (≳10³–10⁸×): inverting through a tiny range
  amplifies f64 representation error past any sane tolerance. This is an f64
  limitation, not a logic bug — the `invert` algebra is exact. The proptest
  strategy constrains the compression ratio to a realistic band (`rwidth_factor`
  ∈ [0.1, 10]) so the property isn't flaky; the well's actual domains (time, band
  fractions) never approach the pathological ratio. Follow-up if it ever bites: a
  one-line doc note on `ScaleLinear` about the compression boundary (parallel to
  the existing 2³ ms note on `ScaleTime`). Discovered during the scales spike
  (deepseek review N3), 2026-06-15.
- **ABC duration-summing ruler:** kaijutsu-abc has no total-beats-per-voice
  machinery; needed to validate that a committed phrase's ABC sums to
  `beats_per_phrase` (Chameleon eval ruler, new code). The tuplet/broken-rhythm
  handling in `midi.rs:261-274` is the acceptance spec.
- **ABC layout:** Linear duration spacing (needs Gould spacing/justification), system bracket/brace, closed-score layout.

## Hyoushigi / Musician

- **Beat-on-track — remaining stages** (Stages 1–3 M1 shipped 2026-06-29/30;
  story in `docs/tracks.md` + devlog): M2–M4 (input telemetry, drift-modeled
  clock-in, edge node) sequenced in `docs/midi.md`; external-signal clock sources
  (solar/compute-availability) ride the same `ClockSourceKind` seam.
- **MIDI-in follow-ons (deferred by decision 2026-07-06 — score first, perceive
  later; M2 capture design is canonical in `docs/midi.md`):**
    1. **Perception.** Captured cells are data-only and invisible to `KJ_HEARD`
       (`heard_json` filters `ContentType::Abc`; the capture mime projects to
       `Plain`). Candidates when we want musicians/coders to hear the room: a
       `MidiToAbcDeriver` notation sibling at the write barrier (mirror of
       `AbcToMidiDeriver` — keeps `KJ_HEARD` unchanged and notation-pure; costs
       a crude quantized transcriber), extending `heard_json` with a MIDI
       digest, or new heartbeat vars. Plus the fun one: a small system
       whisper into coder contexts when the room is playing ("the band is on —
       eurorack on track X") — `BlockKind::Notification` shaped, never the
       cached system prefix (per the datetime-seed lesson).
    2. **CAS write surface (client→kernel put).** `/v/cas` is read-only by
       construction (`vfs/backends/cas.rs`) and the sftp client has no put;
       `commitCapture`'s `Cas(hash)` payload arm is dormant until this lands.
       Needed at the first heavy payload: audio capture windows, client-recorded
       clips. Two shapes to weigh then: capnp `casPut(bytes)→hash` vs teaching
       the sftp/VFS seam write-with-verify (only content matching its address —
       plain `sftp` could seed objects; but it breaks the backend's
       read-only-by-construction stance deliberately).
    3. **Analysis trackers.** Beat-tracking models (Beat This! et al.) run on
       ring windows as just-another-tracker; note Beat This! is *audio*-native
       (fits the audio2midi mic upstream; MIDI windows need render-to-audio or
       a symbolic tracker). Their tempo/phase/downbeat output is
       `Timebase`-shaped corrections — i.e. a second concrete M3 estimator
       candidate (clocked case: pulse-interval filter; unclocked case: beat
       tracker on what Amy actually played).
    4. **Ear slice-1 residuals** (shipped 2026-07-06, `app/src/midi_in.rs`):
       cuts are wall-clock (4 s) — phrase-aligned cuts want the metronome
       phasor + phrase length app-side (`BeatRef` carries no
       `beats_per_phrase`); a kernel-refused batch is warned-and-dropped, not
       requeued; the commit target is the app's *current* context (an
       explicit per-client `midi_in.toml` — capture context + source
       allowlist, the third `/etc/client` consumer — replaces that when
       ambient-vs-seat needs separating); `played_by` is the shipping caller,
       not per-source lanes (sources ride inside the record). And the
       **third-party-thru echo**: the ear excludes kaijutsu's own clients,
       but a synth/DAW/hardware soft-thru re-emitting the render port's
       output IS an external source the ambient ear subscribes — dirty
       capture today, model-hears-itself feedback once perception lands.
       Fixes when it matters: the `midi_in.toml` source allowlist, and/or
       MIDI echo cancellation (the app knows every event it emitted — the
       cutter can fingerprint-subtract captures matching recently-rendered
       (note, channel, ≈time) before shipping). Also deferred by decision
       (2026-07-06): a **payload size cap** on `commitCapture` (a runaway ear
       could land a giant block in the score context; honest worst case
       today ≈2 MB — a loud refuse-over-N-MB in the RPC handler is the cheap
       nudge), and **filter placement** (`keep_at_ingest` drops `F8` clock
       pulses pre-ring, so the M3 clock observer can't be a ring tracker —
       either move filtering to per-tracker cut time or give the observer a
       pre-ring tap in the capture thread; pick deliberately at M3).
    5. **Estimator re-lock after a tempo step is slow and stall-spammy**
       (observed 2026-07-07): the EMA `ClockEstimator` keys on the ALSA
       address, so a restarted master at the same client:port inherits the
       old regime's state — a 540→100 BPM step took minutes of convergence
       with "stall observed" warns at ~2 Hz the whole way. A stall episode
       is strong evidence the source restarted: use it to reseed (or widen
       alpha on) the estimator instead of easing out of stale state. Real
       case: a player switching/restarting master clocks mid-session.
- **Relative-lead timing — open findings from the 2026-07-02 analysis** (the
  substrate verdict + resolved findings live in `docs/midi.md` "The relative-lead
  timebase, analyzed" and `docs/pcm.md`; phase-align 2026-07-15 closed two more:
  the `now + period` re-arm random walk — grid is scheduled-periodic now — and
  the capture-`now`-close-to-the-send gap in `publish_render_cues`. This is the
  still-open remainder):
    2. **Bevy has no audio-scheduling primitive** — the real PCM build risk.
       `AudioPlayer` plays on spawn (`audio.rs` ignores nonzero `cue.lead`);
       honoring `lead` for samples is net-new substrate (delayed-spawn at ~16ms
       frame granularity, or pierce to the `rodio` Sink — `docs/pcm.md` R5,
       open decision 3). MIDI delegates sub-ms timing to the ALSA seq queue.
    4. **Multi-sink flam + whole-queue flush** (`midi.rs` flushes the *whole*
       ALSA queue regardless of track) — future; per-track flush + shared-clock
       scheduling are the eventual answer.
    5. **PLL failure modes to design against** when the modeled clock lands
       (deepseek): starvation drift (ref rate must bound drift < ~1ms),
       tempo-step slew limit, phase-slew-not-step, reference-jitter outlier
       rejection. The absolute-tick-through-PLL shape is the *upgrade path*,
       reached for only if the metronome test shows per-cue boundary jitter
       audibly pulling away from the visual playhead.
- **Metronome — configurable + silence-when-idle SHIPPED 2026-07-05; residuals
  open.** The core asks landed: silence-when-idle (`3fdf1045`,
  `halt_on_connection_loss` resets the phasor on any non-`Connected` status — no
  more free-running onto a wired synth after a kernel restart) and the
  configurable click (`feat/metronome-config` merge: note/channel/velocity/gate/
  enabled from a per-client `/etc/client/metronome.toml`, cascade + app apply).
  **Still open:**
    - **Downbeat accent** — a different note on bar-one needs meter info the
      `BeatRef` doesn't carry yet.
    - **Write ergonomics** — `--global` flag + caller-scoped write default (so a
      client tweaks its own `/etc/client/<id>/…` without spelling the id); needs
      `kj` to resolve the caller's client-id, the same MCP/headless durable-id
      prereq (`docs/config-crdt-ownership.md` "Per-client config" → Open).
    - **Config-change push** — the app applies `metronome.toml` once per
      (re)connect; a live `kj config set` doesn't reach it without a reconnect.
    - **`kj config list` omits `/etc/client`** — only readdirs `/etc/config`, so
      the per-client files aren't discoverable via list.
- **Metronome controller — graduate to PI/PID later.** The slosh was fixed
  (`d2b1f55c`, P-phase correction with feedforward tempo — diagnosis in
  `79c4b6b5`'s message). Remaining: graduate to a full PI/PID (damping + integral
  for steady-state) when a modeled/remote clock (M3) introduces real drift
  feedforward can't cancel; add a phasor-slew metric (correction magnitude per
  reference) to quantify — pairs with the OTel-metrics note.
- **Musician loadout is tool-free by design (2026-06-13)** — a player is an
  ABC-only voice; a small local model handed the full palette stalls the turn.
  Open migration note: the gig (key/tune/register) belongs to the stance +
  producer chart, NOT the base rc — migrate any song-specific primer content to
  the producer/chart layer when it lands ("big models author vocabularies").
- **No chart is seeded into a player's context — the gig metadata gap (found
  2026-06-30, standing up a bass player for the Chameleon line).** The
  musician stance + ABC primer (`musician/create/S00-stance.md`, `S15-abc-primer.md`)
  both say "your chair, key, tune, and register come from your stance and the
  chart the producer has set" — but **there is no chart**. A search of every
  document + KV finds the Chameleon spec (B♭ Dorian, B♭m7–E♭7 vamp, bass chair)
  only in `docs/chameleon.md`; **nothing writes it into a musician context**, and
  no `create` script seeds it. So a freshly-created player arms correctly, hears
  itself + siblings via `KJ_HEARD`, and drives on the beat — but does **not know
  what tune it's playing**. The *now-facts* channel (`KJ_TICK`/`KJ_PHRASE`/
  `KJ_TEMPO`/`KJ_HEARD`) is wired; the *gig* channel is not. This is the producer's job
  (Opus authors the vocabulary, the player speaks it) and the producer chair
  isn't built — but slice one (bass-gemma vamping B♭ Dorian) needs a chart NOW.
  Minimal fix that fits "players are rc programs / setup is declarative rc":
  a `musician/create/S05-chart.md` (numbered into the cached system prefix,
  before the generic primer) carrying the song-specific gig — key, vamp changes,
  register, the bass chair. Hand-authored for the audition; becomes the
  producer's `drift`-delivered, hydrate-latched revision surface when that chair
  lands. Pairs with the "migrate song-specific primer content to the producer/
  chart layer" note in the tool-free-loadout entry above and the
  marker-advance-on-durable-revision item below. Decide: per-song chart files vs.
  a single chart whose body the producer rewrites — the rotation/hydrate boundary
  already gives a clean delivery point either way.
- **Decouple the OODA Act from ABC (generalize the loop primitive).** The Act
  path is hardwired to one notation: `on_turn_completed` → `schedule_abc_cell`
  eager-*parses ABC* to validate, and the `DeriverRegistry` derives MIDI from
  it. The loop *shape* — drive → validate turn output → crystallize a cell →
  derive sibling artifacts — is general and would serve other loops: a
  MIDI-native model (emits MIDI directly, no ABC), non-music content, or any
  "model produces structured artifact on a beat" workflow. Generalize to a
  content-type-keyed `schedule_cell(content, content_type)` where validation is
  pluggable (the player's track/role declares its expected content type) and
  derivation stays the already-content-type-keyed `DeriverRegistry`. Then the
  malformed-quarantine (just shipped, beat.rs:850 `set_excluded`) and the
  header-carry follow-up below both become per-content-type validator behavior,
  not ABC special cases. Keep ABC as the first registered validator/deriver.
  This is one axis of the broader **`context_type` feature-decomposition**
  (`docs/chameleon.md` → "context_type is an rc bundle of features"): *what
  artifact* a player produces, separate from *whether* it has a beat.
- **Header-carry for headerless player output (robustness).** A windowed player
  naturally emits a bare continuation body (no `X:`/`K:` header) once it has a
  full tune in its context; the schedule-time validator then rejects it. Today
  we lean on the tick prompt to demand a complete tune every turn — brittle for
  small models. Robust fix: in the score scheduler, if the output is a bare body
  for a track with a last-good tune, prepend that track's last-good header
  before validating/deriving. Pairs with the decouple above (a per-content-type
  "complete the fragment" step).
- **Cold-start re-attach is MANUAL, not automatic (by choice, 2026-06-28;
  re-stated in track vocabulary 2026-07-01).** The scheduler starts with an
  empty track map on restart; nothing automatically re-attaches persisted
  musicians. **What exists:** `kj transport attach` recovers a musician after a
  restart from its persisted `tracks` + `attachments` rows — real tempo/cadence
  back, attaches stopped + OODA-armed, playhead + committed log rehydrated from
  the score context (restart-safe by construction, `tracks.md` § Restart
  contract).
  **Deliberately deferred** (Amy's call): an automatic cold-start sweep that
  re-attaches every persisted attachment on boot; the natural seam is the
  recovery loop in `rpc.rs`, and it must run *after* the beat scheduler is
  wired. Adjacent to `tech_debt_peer_reattach_on_reconnect`.
  - **Follow-ups:** (a) `beat_count`/`KJ_PULSE` are NOT persisted — documented
    as the contract (`tracks.md` § Restart contract); persist them
    holistically when the sweep lands. (b) attachment-row cleanup on
    disarm/archive once an archive RPC lands (no row leak today).
- **Per-type `BeatPolicy` defaults (the surviving half of "cadence settable per
  context").** The per-context cadence knob LANDED with the track model:
  `kj transport attach --wakeup N --rotate N` sets each attachment's divisors,
  persisted in the `attachments` row. What remains is per-*type* defaults for
  the track-level knobs (period / `beats_per_phrase`) so a `funkMusician` rc
  bundle isn't stuck on `musician_default()` — an axis of the **`context_type`
  feature-decomposition** (`docs/chameleon.md`).
- **`kj transport meter` inbound verb (Chameleon batch 1, F2):** add
  `kj transport meter <beats_per_phrase>` with a `--bars N --beats-per-bar M`
  convenience that multiplies to beats *at the edge* → new
  `BeatCommand::SetMeter`. Home is `kj/transport.rs`, and it gets the first
  bars→beats translation test (the kernel only ever sees beats; bars live in the
  human-facing arg). Pairs with the cadence-knob item above.
- **`ooda_every` stays beat-denominated (Chameleon batch 1, F2):** the OODA
  cadence field is kept in beats even though its default is *expressed* in
  phrases (`8 * 16`); a phrase-typed `ooda_every` is deliberately deferred —
  revisit once irregular phrases (per-phrase beat counts) make the beat
  denomination awkward.
- **Transport surface beyond `kj`:** app transport buttons / spacebar + a capnp
  transport surface (today
  `kj transport attach|detach|play|pause|stop|tempo|ooda|rotate|render` only —
  no app/capnp surface). A restart-recovery `attach` button is a natural fit.
  Overlaps the retired playback.md's `TransportFlow` idea, now recorded in
  `docs/pcm.md` § Distributed listening.
- **Per-listener audio routing (PCM slices 1–3 landed 2026-07-01):** `kj play`'s
  `BlockFlow::PlayAudio` deliberately **bypasses `matches_filter`** — every
  attached client hears every `kj play`, regardless of which context it's on.
  Correct for first-sound (robust when the caller's context ≠ the app's joined
  context), but the eventual "every listener hears playback on their own output =
  shared listening" (`docs/pcm.md` § Distributed listening) wants context-scoped
  routing + a `kj transport route <sink>` verb. Revisit when listening goes
  multi-peer; it's the natural home for the `PeerConfig` capabilities bag.
- **A capnp callback-method addition can wedge a stale client (found 2026-07-01
  during PCM live-verify):** adding `BlockEvents.onPlayAudio @13` means every
  client's `block_events` forwarder must implement it. A client built from the
  OLD schema returns `Unimplemented: Method not implemented` when the kernel
  pushes the new callback — observed on the un-rebuilt `kaijutsu-mcp` binary
  (rebuilt `kaijutsu-server` + app, forgot the MCP server), and it appeared to
  **wedge that client's MCP↔kernel session for ~300s** (a `kj play` shell RPC
  timed out at 300s, then the session reconnected and the retry returned in
  118ms; the sound itself played fine — only the un-rebuilt subscriber erred).
  Two takeaways: (1) **operational** — a capnp change requires rebuilding ALL
  clients (`-server`, `-app`, AND `-mcp`), not just the two obvious ones; worth a
  note in the dev-loop docs. (2) **design** — should the kernel tolerate a
  subscriber that `Unimplement`s a *newer* callback method without wedging or
  eventually dropping its whole (still-valid) block subscription? The bridge
  already logs+counts the failure (`SubscriberHealth`/`MAX_SUBSCRIBER_FAILURES`),
  so a forward-compat client loses its subscription for not knowing one new push.
  A "best-effort, ignore-if-unimplemented" push tier for directive-style events
  (vs. must-deliver block ops) might be the right shape.
- **PCM review findings — open remainder (gemini-pro batch 2026-07-01; the FIXED
  and verified-not-real verdicts are in devlog/git):**
  - **Encoded byte-churn — deprioritized on purpose (Amy):** the fix is
    architectural (route bulk through CAS — the slice-5 convergence), not an
    `Arc<[u8]>` micro-opt; revisit `Arc` only if a real tiny-sample hot path
    shows churn.
  - **`kj play` requires an ambient context — MINOR.** Falling back to
    `ContextId::nil()` (which `on_play_audio` tolerates) would let a truly
    context-less caller broadcast. Design nicety.
  - **capnp union default — NOTE.** The lowest-`@` arm is the default
    discriminant, so a malformed cue decodes as empty-inline → sink EOFs on 0
    bytes (logged, benign). Document if another arm is ever added.
  - A `directive_id` nonce + client LRU dedupe is a reasonable *future*
    idempotency guard if one client ever fans into many subscriptions.
  - `from_path_extension` uses `rsplit_once('.')`; `Path::extension()` is more
    idiomatic (edge case already fails loud).
- **App track chip + "transport" label for beat():** author chips show the
  player's principal on played phrases and `beat()`'s on transport fallback
  repeats — truthful but mildly noisy. Add a track chip (the lane identity) and a
  "transport" label for `beat()`-authored fallback repeats so a vamp insurance
  repeat reads as the transport, not a mystery principal.
- **`KJ_HEARD` shipped as a JSON push; array + pull are follow-ups (Chameleon
  batch 2, 2026-06-11; re-pointed at the track score with Stage 2):**
  `KJ_HEARD` ships as a pragmatic **JSON-string push** — `beat.rs::heard_json`
  reads committed notation in the last `HEARD_WINDOW_PHRASES` (8) from the
  **track's score context** (`ContentType::Abc` only, all producers, across
  rotations — the real band view) and seeds it as a JSON array string.
  Load-bearing **even solo**: score blocks are `ephemeral` (hydration-silent),
  so this is the only way a player sees its own prior phrases. **Two follow-ups
  (TODOs on the code), when the kaish arrays/hashes plan lands:** (1) expose it
  as a real kaish **array of hashes** (indexable, `for phrase in $KJ_HEARD`)
  instead of a JSON string the script can't index; (2) re-shape **push → pull**
  — a `kj`-reachable windowed read so the script chooses depth/track rather
  than a fixed injected window (shares the read with the RC hydration-marker
  archive verb and fork-carry — one read, three consumers). Also open:
  per-context window tuning (`HEARD_WINDOW_PHRASES` is a const). `content_before`
  in `ResolverCtx` stays deliberately track-blind regardless (no resolver reads
  it; `CasCommitResolver` reads CAS by hash).
- **Player spawn / rotation — open remainders** (mechanism shipped; current
  design in `docs/chameleon.md` § Rotation, chronology in devlog). Residual
  narrow race: a rotate rc already in flight ends in `kj transport play` and
  could restart a just-stopped track — add a scheduler-side halt check if it
  ever bites. Still open:
  - **Rotate chains pollute the director's context tree (found ~2026-06-29, DS
    Director `019f14ba`; the entry's original "2026-07-15" was an in-app
    hallucinated date, corrected 2026-07-03 — see Context time awareness).**
    Every page-turn is a thin `spawn` fork, so a song
    running N phrases produces N+1 contexts in a linear chain — `kj context
    list --tree` renders the whole lineage and an operator must visually skip
    past it (a 17-deep chain observed from one song). Fix ideas (pick one):
    (a) `--hide-archived` collapse, (b) fold same-track rotate chains into a
    compact `root→…→tip (N segments)` one-liner, (c) auto-archive rotated-out
    segments. No correctness issue — operator UX tax.
  - **The windowed-notation pull primitive.** No cross-context block-copy verb
    exists; a player carrying recent notation into its thin-forked child needs
    one. Same windowed read as `KJ_HEARD`'s push→pull follow-up and the
    marker-archive read — **one read, three consumers**; keeps the carry in rc.
  - **A declarative "fire script at tick T" timeline scheduler** — worth
    building once the producer schedules more than rotates (section/tempo/
    dynamics events are the clear second consumers).
  - **Marker-advance on durable revision** — when the producer writes revision
    blocks, re-run `kj context hydrate` to advance the marker. Pure rc once
    the producer exists.

- **Fork primitives — full/thin mental model (Amy, 2026-06-12).** Full fork
  (regular `kj fork`) is the *powerful* path: take the whole context into a fresh
  lineage = a **new KV cache** (resume-a-session-as-another-model, orchestrator
  repair, drift-a-summary-back). Thin fork is *reuse/reduce*: save tokens for a
  long-running iterating player (the `window`/`spawn` factory presets per
  `docs/fork-filters.md`). Copy cost is a non-issue (storage cheap); the axis is
  KV-cache strategy. Remaining open primitives:
  - **Retire the `max_blocks` fork field (slice 4):** `fork_filtered` now builds
    its positional universe in document (`order_key`) order, so `max_blocks`
    indexes the timeline correctly in the interim (test
    `fork_filtered_max_blocks_keeps_most_recent_by_timeline`), but the field is
    only deprecated, not removed. Fold `--depth N` into the selection engine as
    `--include end-N:` over the `block_ids_ordered()` snapshot and delete the
    field. (BlockId order is `(context, principal, seq)` — principal-major; it
    only coincides with timeline order for a single principal, so a multi-principal
    `max_blocks` over raw BTreeMap iteration was the original bug.)
  - **A snapshot/savepoint marker verb (speculative, not-now — direction set
    2026-06-12).** Absorbed by the fork-filters range grammar as a future
    **label endpoint** (`docs/fork-filters.md`): a savepoint is a colon-free
    name on a block, usable as a range endpoint (`kj fork --include 0:bridge`)
    — no new fork machinery, no verb semantics of its own. Still not-now;
    build labels when the orchestrator work or the time-well wants named
    points.
  - **Presets as a deep kaijutsu concept (design thread, 2026-06-12).**
    Preset = a named **ensemble of argument values**, not a behavior — the
    audio patch-recall model (hit "e-piano", every knob moves, same synth).
    Extends the existing model/prompt preset table (normalized `preset_args`
    child table, verb-scoped from day one) to carry fork filters; a `player`
    patch can move filter + model knobs in one recall. Recall-then-tweak:
    scalars override, filters compose under the include invariant; recall is
    a snapshot (horizon-latched, like rc scripts). Fork is the only wired
    verb for now — generalizing to other verbs (discovery, user banks,
    sharing) deserves its own design session.

  **Remaining follow-ups (deferred — from the same review):**
  - **P1 ×2 — absorbed into the shared SEAM MODULE (re-prioritized
    2026-06-12: FIRST in the fork-filters build order).** The tool-pair /
    turn-boundary tail snap (orphan `tool_result` silently dropped by the
    snapshot repair; a marker on a `tool_call` injects a synthetic
    "interrupted" result every turn forever) and the missing archive seam
    (prefix+tail concatenate with no "[N blocks archived]" signal; cross-gap
    `Model/Text` fragments can merge into false continuity) were "latent
    until musician gets tools" as hydration bugs — but fork-filters' hand-cut
    ranges make both reachable immediately. One first-class module owns every
    keep-set cut edge: turn-boundary snapping (never start an interval on
    `ToolResult`/`Model`-continuation), synthetic user-role seam injection
    (after the prefix, cache-stable), tool-pair integrity. Consumers:
    `rehydrate_windowed`, fork selection, the pull primitive. Contract in
    `docs/fork-filters.md`.
  - **`window` counts RAW blocks, not turns/phrases** (~2-3 blocks per OODA turn,
    and musician score/Trace blocks are hydration-silent so the *visible* tail is
    smaller still) — revisit if a phrase/turn-denominated window reads cleaner.
  - **Cache-breakpoint ↔ window interaction** — the musician's S20 cache
    breakpoints sit at message indices that windowing shifts; harmless for the
    local bass (no prompt cache; musician sets no breakpoints today so the
    byte-stable prefix is inert), reconcile when API-model chairs join.
- **Standing per-phrase `UseLastGood` cells (whole-turn-miss hole) (Chameleon
  batch 1, F2):** `UseLastGood` only fires when a cell was *scheduled* and then
  squashed; a turn that produces no cell at all (the model never spoke) leaves no
  cell to fall back on, so the phrase is silent rather than a vamp repeat. The
  natural hook is the new `phrase_due` boundary: stand up a per-phrase
  `UseLastGood` cell at each phrase boundary so an unscheduled phrase still vamps
  the last good one. Out of scope for batch 1; recorded so the hole is known.
- **Deriver-budget enforcement beyond convention (Chameleon batch 1, F2):** the
  `Deriver` contract says ≲1 ms per cell (it runs on the beat thread under the
  timeline lock) but nothing enforces it — today it is a measured convention
  (T22 prints ~300 µs release for the ABC deriver). Add a timed `debug_assert`
  (or a soft warn) around `derive()` so a future heavy deriver trips loudly in
  dev rather than silently stalling the beat under the lock.
- **In-RAM committed `Vec` / RAM-CAS unbounded growth (Chameleon batch 1, F2;
  reframed 2026-07-01):** the track timeline's committed `Vec` and RAM CAS grow
  without bound for a long-playing track (every phrase appends). Rotation is
  deliberately NOT the answer anymore — the track timeline *survives*
  page-turns by design (`tracks.md`, the per-track score context). The durable
  record already lives
  in the score context's blocks + CAS, and `UseLastGood`/`KJ_HEARD` only need a
  recent tail, so the fix is windowing/compacting the *in-RAM* committed log
  (drop cells older than the largest read window; rehydration-from-blocks
  already exists for the tail). Until then a marathon set leaks RAM.
- **Band track↔chair mapping source of truth:** musician-create derives a track
  from the context label (`TrackId::new`→`slugify`, hard-error on empty slug).
  Once a band config exists (multiple chairs on one timeline), decide where the
  track↔chair mapping lives — there is no registry today (track is self-describing
  on every block, by design).
- **`played_by` collapses to `system()` — `who-played` provenance is degenerate
  (Chameleon batch 1, F2):** F1 §1.2 records "who played" as `BlockId.principal_id`,
  meant to be the player's principal. But the musician turn's model-text output
  block is inserted under `PrincipalId::system()` (`llm_stream.rs` `StreamEvent::TextStart`,
  the standing model-text convention), and `on_turn_completed` (`beat.rs`) sets
  `played_by = b.id.principal_id` = `system()`. The OODA `tick` verb also fires
  under `system()` (`beat.rs::fire_tick`), so `TurnFlow::Completed.principal_id`
  carries `system()` too — reading it instead of the block author would NOT help.
  So every materialized score block is authored by `system()` (plus `PrincipalId::beat()`
  for fallback repeats). **Harmless today** — one model per musician context, and
  lanes key on `track`, not principal, so no correctness/collision issue (the
  per-principal seq lane just has a single `system()` writer). **Will mis-attribute**
  the moment multiple models share a context or we want to distinguish player from
  transport. Not a one-liner: needs the musician turn to run (and author its
  output) under a distinct per-player principal. Surfaced in the F2 adversarial
  review (deepseek+gemini, 2026-06-11); the two silent-failure bugs from that pass
  (resume parent-id from log tail; hydration-failure publishing no terminal event)
  were fixed in-slice.
- **`kj track` listing surface:** no way to enumerate the tracks present on a
  context's timeline. Add a `kj` listing surface (which tracks exist, which
  principals played each) once tracks are user-visible.
- **Section-placement policy:** the OODA notation cell is scheduled a fixed
  **one phrase** ahead (`phrase_delta()`; `OODA_LEAD` is gone, Chameleon batch 1,
  F2); a real musician wants musical placement (next section boundary, loop
  region) and a richer `compute_basis`.
- **`Midi` render variant + UI timeline:** `audio/midi` projects to `ContentType::Plain`
  today; add a `Midi` variant + renderer, and the scrubbable timeline render.
  **Deliberately deferred to its first consumer (an app-side MIDI renderer /
  peer sink — `docs/pcm.md` § Distributed listening), not added in
  Chameleon batch 1, F2:** `ContentType` is a closed enum that rides
  `BlockHeader` inside `SyncPayload` ops, and the CBOR codec is fail-loud by
  design — a new variant breaks old decoders. Per the project rule a variant
  lands with its renderer, never speculatively. Interim sink key:
  `Role::Asset && parent_id → ABC source` (one hop); the authoritative mime is in
  the CAS sidecar.
- **midi→pcm re-anchor (playback slice 3) (Chameleon batch 1, F2):** the
  `abc_to_midi` *resolver* is gone — ABC→MIDI is now a barrier-side `Deriver`,
  not a timeline resolver, so the midi→pcm chain for dumb (PCM-only) sinks has no
  resolver shape to copy. Two candidate re-anchor shapes to pick between when
  playback slice 3 lands: (a) a deferred PCM **cell keyed on the derived MIDI
  hash** (real lead time, scheduled like any resolver), or (b) a measured
  **budget-excepted deriver** (only if midi→pcm proves fast enough to run at the
  barrier — almost certainly not, soundfont synthesis is heavy). See
  `docs/pcm.md` § Distributed listening (playback.md retired 2026-07-01).
- **Clip cells — R1+R2+R3+R5 LANDED 2026-07-16** (`docs/pcm.md` "The
  remaining work" is the map; research record `docs/cue-prior-art.md`).
  Still open:
    - **R4 prepare horizon** — the prepare directive at commit + the
      skip-loud late gate (interim: a late CAS resolve fires late, which is
      right for `kj play --cas` but wrong for a musically-placed clip).
    - **Attach-time rehydration is notation-only** (`beat.rs` rehydrate
      filters `ContentType::Abc`): after a kernel restart the in-memory
      committed log drops past *clip* cells (the score context keeps them
      durably; `UseLastGood`'s notation-purity is unaffected — clips carry
      `Skip`). Matters only if something later reads the committed log for
      historical clips; fold clip-aware rehydration in then.
    - **Slice 4 edge-node sink** (midi.md M4) and the bevy full
      feature-enumeration (MUST land before any bevy upgrade — the
      two-rodio/two-cpal device fight, pcm.md polish list).
- **Trace span attribute:** attach `hyoushigi.tick` on the materialize→insert
  spans now that a producer exists.
- **Multi-listener playback (was `docs/playback.md` — retired 2026-07-01).**
  The 2026-06-10 peer-sink design predates the track/`RenderTarget`
  architecture; its superseded mechanism decisions (sink-pull scheduling, the
  pause=mute verb remap) are recorded as such and its surviving ideas
  (peer capability advertisement, capnp/`TransportFlow` transport surface,
  routing, the metronome slice, midi→pcm for dumb sinks) now live in
  `docs/pcm.md` § Distributed listening. Longer-term design conversation, not
  a task yet: unify hyoushigi beat-time and conversation wall-time ("the
  conversation has a tempo") so the timeline is the kernel's one clock rather
  than a music sidecar.

## config-shadow cache: residual cross-alias staleness (found 2026-06-24; common case fixed)

Invalidation after a direct config write is by the written/opened path only
(`Kernel::invalidate_config_file_cache`, the fixed common case), so writing one
symlink alias and reading another stays stale until cache eviction — e.g.
`kj rc reset lib/S20` then `cat coder/S20` (coder→lib). Cosmetic (cat path
only), self-heals on LRU/TTL. A full fix needs alias-aware invalidation
(forward-resolve the written path to its terminal *and* reverse-scan symlinks
that point at it) — deferred.

## VFS / cache: coherency + consistency + test-coverage audit (2026-06-27)

External reviewers (the gpal/Gemini batches especially) keep poking at the cache
layer and finding *plausible* coherency holes that mostly turn out narrower than
claimed once checked against the wiring — but the recurring near-misses say the
substrate deserves a systematic pass rather than per-claim firefighting. The trigger
this round: SFTP rides `Arc<MountTable>` directly (`sftp.rs:115`, from
`kernel.vfs()`), while the `FileDocumentCache` write-through lives one layer up in
`MountBackend` (`runtime/mount_backend.rs:43-49`), which SFTP never traverses. Not
the "silent divergence" the review claimed (CRDT mounts still hit `ConfigCrdtFs`
in-table; the generation/mtime staleness reload exists precisely to catch
bypassing writers — that's how host `vim` stays coherent) — but the two-layer split
is real and under-tested.

Scope a deliberate audit covering three axes:

- **Cache coherency.** Enumerate every `FileDocumentCache` consumer and every path
  that *bypasses* it (SFTP via `MountTable`, app renderer, `ConfigCrdtFs` execution
  reads, kaish/MCP file tools via `MountBackend`). For each: does the generation/
  mtime staleness reload actually fire? Map the **dirty-cache-wins** windows (an
  in-flight cached edit shadows an external/SFTP write until flush) and the
  byte-offset-write vs document-level `WriteMode` impedance (SFTP `write(path,
  offset, data)` onto a UTF-8 CRDT doc). Fold in the residual cross-alias staleness
  above — it's the same family.
- **Code consistency (async-correctness).** `LocalBackend` mixes `tokio::fs` and
  blocking `std::fs` on the async worker: `write`/`read`/`truncate` use `tokio::fs`
  (offloaded, fine), but `create` (`local.rs:290`), `mkdir` (`:307`), and
  critically `resolve()` — called on *every* op, doing synchronous
  `canonicalize()` at `:80,93,105` — block the runtime thread. Under a slow/stalled
  host FS those starve the ambient tokio pool, which is exactly the path the
  "ssh-in-when-the-app-is-down" fallback depends on (the gpal `spawn_blocking`
  note, verified — but mis-aimed at `write`; the offenders are `resolve`/`create`/
  `mkdir`). Fix: route the blocking calls through `spawn_blocking` or `tokio::fs`.
- **Test coverage.** We lack concurrent multi-writer VFS tests (the kind that would
  have surfaced the SFTP concurrent-append lost-update directly), cross-layer
  coherence round-trips (SFTP write → kaish `cat` sees it; kaish edit → SFTP read
  sees it), and staleness-reload tests per backend. Build these as the audit's
  exit criteria, not an afterthought.

Not urgent, but a good forcing function alongside the SFTP/shell sidequest, which
is the consumer that stresses all three axes at once.

## FSN world — `Vfs.snapshot` stage-0/1 known gaps (landed 2026-07-12, Lane B)

`kaijutsu.capnp` `Vfs.snapshot` + `MountTable::snapshot`
(`crates/kaijutsu-kernel/src/vfs/mount.rs`) shipped the recursive-listing +
generation-stamp plumbing from `docs/scenes/vfs.md` stage 0/1. Two
deliberately-scoped simplifications, documented in the method's own doc
comment, tracked here for stage 2+:

- **Generation blind spot to non-VFS-mediated writes.** Listing-generation
  bumps happen at the `MountTable` chokepoint (create/mkdir/unlink/rmdir/
  rename/symlink/link). An external process writing directly into a
  `LocalBackend`-backed host path — `cargo build` populating `target/`, a
  human `vim`-ing a file outside the app — never touches `MountTable`, so the
  generation counter doesn't bump even though the real directory listing
  changed. `snapshot`'s own `readdir` still sees the real, current listing
  (it's not stale content) — only the *generation stamp* lags, which matters
  once a client starts caching listings keyed on generation (stage 2). Closes
  when inotify lands (`docs/scenes/vfs.md` stage 2: `IN_Q_OVERFLOW` →
  rescan-and-bump covers this exact case).
- **`ignored` gitignore classification is best-effort, not git-exact.** Two
  gaps in `MountTable::ignore_stack_matches` / `build_ignore_level`: (1)
  closest-directory-wins folding across `.gitignore` levels approximates but
  isn't identical to git's precise cross-file cumulative precedence (a
  negation in a shallower file cannot override an ignore decided by a deeper
  one — the dominant real-world case, but not literally correct in every
  edge case); (2) only `.gitignore` files at-or-below the snapshot root are
  consulted — an ancestor `.gitignore` *above* the requested root path is
  never read, so `kj vfs snapshot /mnt/project/src` won't see a pattern that
  only lives in `/mnt/project/.gitignore`'s parent-relative form if `src`
  itself isn't the walk root. Both are fine for slice-0 (`ignored` is display
  metadata, never a filter — a wrong classification never hides data), but a
  real Lane C world render leaning on `ignored` for visual treatment should
  know it's approximate.

Neither gap blocks Lane C (the Bevy world renderer): the snapshot tree itself
is always structurally correct (real listings, real attrs); only the
generation staleness signal and the ignored-styling hint have known slop.

## kaijutsu-mcp — invoke_peer double-encodes object params (found 2026-06-23)

Calling the `invoke_peer` MCP tool with an object `params` (e.g. `{"context_id":
"019ec11b"}` for `switch_context`) fails: the app's `dispatch_peer_action`
rejects it with `invalid type: string "{\"context_id\": ...}", expected struct
Params`. Diagnosis: `InvokePeerRequest.params` is `serde_json::Value`
(`models.rs:144`) and the server does the right thing
(`serde_json::to_vec(&req.params)`, `lib.rs:1166`) — but `req.params` *arrives*
as a `Value::String` holding the JSON text, not a `Value::Object`. So the
tool-call layer stringified the object one extra time before it reached the
server; `to_vec` then emits a quoted JSON string and the app's `from_slice`
sees a string. Surfaced now because `invoke_peer` is rarely exercised (Amy:
"we haven't used it much until now"). **Proposed fix (server-side, tolerant):**
in `invoke_peer`, if `req.params` is a `Value::String`, attempt to parse it as
JSON and use the result (accept either an object or a JSON-string-of-an-object);
fail loud if neither parses. Real root may be client-side arg encoding for
`serde_json::Value` fields — worth confirming. Blocked the isolated peer-path
verification of the Screen-transition fix; verified instead via the
server-pushed `ContextSwitched` path (`kj context switch`), which exercises the
same `handle_context_switch` landing.

## kaijutsu-mcp — June 2026 SyncedDocument migration review

Surfaced by a DeepSeek (concurrency) + Gemini (architecture) review of commit
`ac5f518` (Remote backend cut over to `kaijutsu_client::SyncedDocument`). The
dropped-stdout bug and the content/exit_code completion race are fixed (poll now
does an authoritative `get_context_sync` read after terminal status); these are
the *remaining* findings, triaged.

- **HIGH (PARTIAL) — hook authoring vs resync: sole-writer + pushed-frontier.**
  `HookListener` writes blocks directly (`doc_mut().insert_*` under
  `synced.lock()`), so the bg listener is NOT the sole writer. `apply_sync_state`
  replaces the doc wholesale, so un-pushed hook blocks could be wiped on resync;
  and `push_ops` bases `ops_since` on the inbound SyncManager frontier, which
  local authoring never advances → every hook event re-pushes all prior local
  ops (idempotent but O(N)). MITIGATED 2026-06-13: `resync_synced` now FLUSHES
  local ops (`flush_local_ops`) before fetching the snapshot, so hook blocks
  round-trip through the server and survive the common case. REMAINING (cohesive
  follow-up, needs design + CRDT-frontier testing): (a) a dedicated "pushed"
  frontier so flush stops re-sending; (b) close the residual flush→apply window
  where a block authored mid-resync is still lost — cleanest via a command
  channel that makes the bg task the true sole writer (authoring + push + resync
  all serialized in one task).
- **LOW — `agent.compact` hook event is mapped but unhandled.** The adapter maps
  Claude `PreCompact` → `agent.compact`, but
  `HookListener::process_event` has no arm for it (falls to `_ => {}`), so a
  compaction boundary silently produces no block. Either author a System/Trace block
  marking the compaction, or drop the mapping. (Found during the 2026-06-18 bitrot
  pass; design pass in git history — `docs/mcp-hook-alignment.md`, deleted
  2026-07-04.)
- **LOW — `claude-hooks.json` uses a repo-relative adapter path.** `command:
  "contrib/adapters/claude.sh"` only resolves when Claude Code's cwd is the kaijutsu
  repo root. The adapter itself now resolves its own filter via `BASH_SOURCE` dir, so
  only the settings.json entry is cwd-sensitive. Document the absolute-path
  requirement in the sample, or have install copy an absolute path.
- **MED — multi-context operations silently collapse to one in Remote.**
  `search_context`, `list_resources`, the `kaijutsu://docs` reader, and
  completions call `context_ids()`, which in Remote returns only the single
  joined context (`crates/kaijutsu-mcp/src/lib.rs`). A global search now silently
  skips every other context on the server. Fix: add an async
  `actor.list_contexts()`-backed lister for Remote multi-context surfaces.
- **MED — resource/prompt handlers hardcode `kind = "Conversation"` for Remote**
  (`analyze_document`, doc-tree, `read_resource`). Loses the real context type.
  Fix: carry the kind through the sync state or a metadata RPC.
- **MED — Remote input tools vs Local divergence:** Local `read/write/edit_input`
  swallow `create_input_doc` errors via `let _ =`; `submit_input` is
  unimplemented in Local mode. Either implement Local submit or document the gap.
- **LOW — hook insert/push failures only `warn!` then return `allow`.** The
  agent proceeds while its action's CRDT blocks are silently dropped — counter
  to the crash-loud stance. Consider returning `deny` (or a visible error) on
  push/insert failure.
- **LOW — `SyncedDocument::pending_events` not drained on `apply_sync_state`**
  (`crates/kaijutsu-client/src/synced_document.rs`): events buffered before a
  resync are never replayed against the new doc. Harmless if the server snapshot
  is always ahead of the buffered events; otherwise a silent loss.
- **LOW — dead `push_to_server` on `KaijutsuMcp`** (lib.rs): nothing calls it
  (the hook listener has its own `push_ops`); carries the same stale-frontier
  bug. Delete or consolidate.
- **PERF follow-up — the shell poll's authoritative read pulls the full context
  snapshot per command** (`execute_and_poll_shell`, Phase 2). Fine for short MCP
  contexts; a per-block read RPC (`actor.get_block(ctx, id)`) would avoid the
  O(blocks) transfer for large conversations.
- **TEST gaps beyond `tests/e2e_shell.rs`:** no coverage for Remote
  input tools, the hook-listener socket path, prompts, resources, or
  reconnect/resync. Add e2e cases (the harness in `e2e_shell.rs`
  generalizes).

## Testing & Tooling

- **russh teardown panic:** `ChannelCloseOnDrop::drop` panics with "there is no reactor running" in tests.
- **`kj` help docs predate the clap migration.** The live top-level help is
  `crates/kaijutsu-kernel/docs/help/kj.md` (`include_str!` at `kj/mod.rs:451`)
  but its command table is stale — it still lists the retired `kj kv`, the
  pre-preset `fork --shallow`, and the old `transport arm|play|…` verb set
  (no `attach`/`detach`/`rotate`/`render`, no `config`/`play`/`binding exec`).
  Its six siblings (`kj-cache/context/drift/fork/preset/workspace.md`) have
  **no consumer** (only a doc-comment mention of `kj-cache.md`). Reconcile with
  the clap-reflected reality: refresh `kj.md`, decide the siblings' fate
  (delete, or wire as `kj <cmd> help` bodies). NB `docs/kj-help` is a symlink
  into that dir — not a docs-cleanup candidate.
- **Capnp schema change ⇒ three binaries to bounce:** the dev runner
  only rebuilds/restarts `kaijutsu-app`; `kaijutsu-server.service`
  (systemd user unit) and `~/bin/kaijutsu-mcp` (running MCP processes
  hold the old binary; `cp --remove-destination` to replace, then
  reconnect MCP) keep stale codegen and fail handshakes with
  `Message contains non-list pointer where data was expected` (worse
  now that Kernel interface ordinals renumber on method deletion,
  e4c8417). Teach `contrib/kaijutsu-runner.sh`/`kj rebuild` to rebuild +
  restart all three, or at least print a loud reminder when
  `kaijutsu.capnp` changed.

---

## Architecture mapping pass — 2026-06-16

New observations from the crate-by-crate architecture sweep (see
`docs/architecture/`). Not fixed; recorded for later. Items that confirm an
existing entry are marked *(confirms above)*.

**Silent fallbacks (violate the "crash over confuse" stance):**
- `Kernel::list_tool_defs_via_broker` returns `Vec::new()` on *any* broker error
  (`kaijutsu-kernel/src/kernel.rs:467`) — a broken binding silently presents the
  LLM a tool-less context, no log/trace.
- `dispatch_tool_via_broker` does `broker.binding(...).unwrap_or_default()`
  (`kernel.rs:346`) — binding-fetch failure silently becomes deny-all; surfaces
  later as a confusing `ToolNotFound`.
- `MountBackend::read` falls through to raw on-disk content on *any*
  `FileDocumentCache::read_content` error, not just "missing/binary"
  (`kaijutsu-kernel/src/runtime/mount_backend.rs:267`) — a CRDT error could serve
  stale bytes.
- Additive `ALTER TABLE` migrations swallow SQL errors with `let _ =`
  (`kaijutsu-kernel/src/kernel_db.rs:873`) — a real failure surfaces as a
  confusing read-time error later.

**CRDT data model:**
- **Dual storage impls.** `BlockStore` (target) and `BlockDocument` (legacy) are
  both `pub` and in use; the legacy path returns newer fields
  (`ephemeral`/`stderr`/`signature`/`track`) as hardcoded `None`/`false`
  (`kaijutsu-crdt/src/document.rs:482`) and retains the duplicate-block seq bug
  fixed in `BlockStore` (`document.rs:892` vs `block_store.rs:320`). Pick a
  migration deadline.
- `calc_order_key` calls `block_ids_ordered()` (O(N) sort) on **every** insert
  (`kaijutsu-crdt/src/block_store.rs:390`); the bench exposing it is `#[ignore]`d.
- Tombstones aren't a first-class `BlockSnapshot` field — they ride a side
  `deleted_blocks` list re-applied by hand (`block_store.rs:1637`).
- `StoreSnapshot` has a breaking-format note with no migration path ("delete
  existing databases when upgrading", `block_store.rs:1680`).

**UTF-8 offset hazard:**
- `EditEngine` passes **byte** offsets/lengths to `block_store.edit_text`
  (`kaijutsu-kernel/src/file_tools/edit.rs:132`) while `FileDocumentCache` is
  careful to use **char** counts (`cache.rs:276`). Multi-byte content can corrupt
  the CRDT splice. Audit `edit_text`'s parameter semantics and unify.

**`LocalBackend::setattr` mtime is a no-op** (`kaijutsu-kernel/src/vfs/backends/
local.rs:354`) — it opens the file but doesn't set the timestamp, yet mtime is
load-bearing for `FileDocumentCache` staleness detection.

**`kj` single-source guarantee is manual** — `dispatch()` routing and
`kj_command()` schema tree must be hand-kept in sync; a subcommand added to one
but not the other is unreflectable (`kaijutsu-kernel/src/kj/mod.rs:589`).

**Types-crate layering** — `ThemeData` (~60 visual fields + `include_str!` of
`assets/defaults/theme.toml`) lives in the foundational `kaijutsu-types`
(`theme.rs:59`). Belongs in a UI/config crate.

**`kaijutsu-index`:**
- `rebuild()` is a TODO stub (`lib.rs:214`) — evicted HNSW slots accumulate
  forever.
- Metadata lock held across ONNX `embed()` (`lib.rs:160`) serializes all
  `index_context` calls.
- `ort` uses `download-binaries` — fetches ONNX Runtime at build time, breaks
  air-gapped builds.

**`kaijutsu-cas`** — no refcounting/GC (`remove` is unconditional,
`store.rs:330`); object+metadata write isn't atomic (crash between leaves a
metadataless blob, `store.rs:254`).

**`kaijutsu-telemetry`** — the Bevy path leaks a `tokio::runtime::Runtime` and
upcasts its `EnterGuard` to `'static` (`otel.rs:28`); soundness rests on the
leaked runtime outliving the guard.

**`kaijutsu-client`:**
- Backoff reset bug — `finish_closing` reads `self.state` *after* `mem::replace`
  moved it to `Idle` (`actor.rs:1451`), so the attempt counter isn't preserved
  through `Closing → Cooldown`; backoff always resets to 1 s after a post-connect
  failure.
- `is_disconnect_error` matches on the capnp error `Display` text
  (`actor.rs:1214`) — fragile; a capnp formatting change would stop triggering
  reconnect. Prefer a typed `ErrorKind::Disconnected` match.
- Peer-reattach residual: initial `attach_peer` isn't remembered until the first
  *successful* user call, so a kernel restart before that leaves the peer
  un-reattached (`actor.rs:1933`). *(extends `tech_debt_peer_reattach_on_reconnect`)*

**App (`kaijutsu-app`):**
- Triple Chat/Shell discriminator — `FocusArea` + `ActiveSurface` +
  `InputOverlay.mode` (the last unread by submit); collapse to
  `FocusArea::Compose(ActiveSurface)` (`input/focus.rs:71`,`:116`,
  `view/components.rs:285`).
- 77 `#[allow(dead_code)]` suppressors for future-phase API — prefer
  `#[cfg(feature)]` so dead-code discovery still works.

**`kaijutsu-abc`** — `to_abc()` round-trip silently drops
`InlineField`/`Decoration`/`VoiceSwitch` (`lib.rs:406`); tuplet writer omits the
optional `:r` count (`lib.rs:366`).

**Server `unwrap()`** — `create_shared_kernel` panics on workspace-insert failure
(`rpc.rs:1092`) instead of `?`-propagating like its neighbors.

**Cap'n Proto evolution is comment-only** — no `@version`; removed-method ordinals
are renumbered/reused with a "safe because all clients updated" comment
(`kaijutsu.capnp:921`,`:933`,`:1169`). *(confirms above — fragile with 7+ dependent
crates)*

---

## Gemini CLI feature comparison — candidate work (2026-06-23)

Differentiators surfaced by scanning `~/src/research/gemini-cli` (Node/TS terminal
agent) with sonnet subagents, each verified against the kernel source before being
listed. Lens: capabilities gemini-cli has that kaijutsu plausibly lacks — *not* a
full feature inventory. Filtered through the **instrument-not-harness** stance:
items tagged ⚠️ sit in tension with it (silent override / harness-UX) and want an
opt-in or kernel-capability reframing before adoption. Pick from these; they are
candidates, not commitments.

### Provider resilience (the headline — Gemini's alignment + availability is the reason we're here)

- **LLM retry + backoff with jitter.** Claude/OpenAI/DeepSeek clients issue a
  single HTTP request and propagate `LlmError::RateLimited`/`ApiError`/`NetworkError`
  with no retry — one transient 5xx/SSL hiccup aborts the whole turn and loses
  context. gemini-cli `retryWithBackoff` (`packages/core/src/utils/retry.ts`): exp
  backoff, ±30% jitter, `Retry-After` respect, retryable-vs-terminal classification.
  Transparent to the user; clean instrument fit. **Strongest, lowest-risk item.**
- **Model availability state + fallback chain.** No per-model health map; a 429
  just fails. gemini-cli tracks terminal/transient health and walks a policy chain
  (pro→flash→…). ⚠️ Make it opt-in (`--allow-fallback` / alias policy) so the kernel
  doesn't silently swap the model the user chose.
- **Extended-thinking wiring — nearly free.** Types, builder, and SSE parsing are
  *done* (`Thinking::Enabled`, `with_thinking()` `llm/claude/build.rs:224`,
  `ResponseBlock::Thinking`) but the stream path hardcodes `thinking: None`
  (`build.rs:143`) and no `BuildOpts` field exposes it. ⚠️ `Thinking::Enabled {
  budget_tokens }` 400s on Opus 4.8 (adaptive-only) — wire **adaptive thinking +
  `effort`** through `BuildOpts` + per-model config, not `budget_tokens`. Toggling
  thinking doesn't invalidate the tools+system cache, so it's safe to flip per-context.
  Claude 4.x thinks by default — small delta, real win.
- **Token-aware context window.** Only an *output* cap exists (`max_output_tokens`,
  default 64K); no per-model *input* limit table, no pre-send estimate, no media
  weighting. Windowed hydration (`llm/mailbox.rs:197`) is block-count, not token-count
  — near-limit contexts get silently truncated or 400'd by the provider. Add a
  per-model input-limit table + pre-send estimate warning. Optionally an EMA
  chars-per-token calibrator fed by actual API usage (gemini's `AdaptiveTokenCalculator`).
- **Classifier-based model routing.** ⚠️ Opt-in only: route cheap turns to haiku,
  hard turns to opus via a fast classifier (gemini `ModelRouterService`). Surface as
  `route: auto` alias policy, never a silent override.

### Context & memory

- **Auto token-threshold compression with LLM summarization + verification.**
  Windowed hydration drops the middle block range with no summary (the motivating
  incident in `docs/conversation-session.md`). gemini `ChatCompressionService` fires
  at ≥50% of the window, LLM-summarizes the older segment into a `<state_snapshot>`,
  then runs a verification turn to catch omissions. Pair this with the windowing notch
  so dropped history leaves a distilled trace.
- **JIT subdirectory context injection.** *(merged: surfaced by both the tools and
  context scans.)* On a tool crossing into a new subtree, gemini crawls upward for
  not-yet-loaded `GEMINI.md` and appends it to the tool result. kaijutsu loads rc/
  stances at context-create only — no path-triggered per-directory injection. Append
  any `KAIJUTSU.md` between the accessed path and workspace root on first access.
- **Filesystem memory-file discovery.** No traversal of the host FS for
  user-maintained markdown memory. gemini crawls up to the git root merging
  `GEMINI.md` tiers (global→project) + recursive `@path` imports. Discover/inject a
  per-directory `KAIJUTSU.md` at hydration — user-editable working agreements that
  attach to a directory without touching kernel config.
- **Date / OS / cwd in situational context — cheap.** `build_system_prompt`
  (`llm/system_prompt.rs:69`) injects id/label/model/tools but *not* today's date,
  platform, or cwd. ~20 tokens kills a class of stale-temporal and platform-wrong
  reasoning. Add fields to `SituationalContext`.
- **`kj memory show`.** No way to inspect the *assembled* system prompt (base + rc
  sections + situational) without reading source files — memory debugging is opaque.
  Add a render command; optionally `kj memory refresh` to hot-reload stance edits
  without re-creating the context.
- **Memory inbox (LLM-proposed durable patches).** Drift targets live contexts, not
  files. gemini lets the model propose memory edits as unified diffs queued for
  user apply/dismiss. A file-targeting analog of drift: model proposes a stance/memory
  patch → inbox → user reviews before it takes effect.

### Tools

- **`web_fetch` + `web_search` builtins.** Zero web-acquisition tools exist (reqwest
  is LLM-API-only). Without a fetch primitive the instrument can't research anything
  not pre-loaded; every harness must BYO scraper MCP. Add a `builtin.web` server:
  HTML→text fetch (rate-limited, private-IP block, untrusted-content wrapper) + search.
- **Background shell + process management.** `builtin.shell` is synchronous only —
  no `is_background`, no PID registry, no tail-read companion. Long builds/test-suites/
  service-starts can't be modeled without serializing. Add `is_background` +
  `list_background_processes` / `read_background_output`.
- **`read_many` (multi-glob batch read).** Today: glob then loop-read. gemini
  `read_many_files` expands patterns, reads all matches (incl. images/PDF/audio),
  returns one joined payload with per-file truncation markers. Saves turns on
  codebase-wide context loading.
- **Omission-placeholder validator on edits — fits "crash over corruption."**
  `EditEngine` validates the `old_string` match but doesn't scan `new_string` for LLM
  shorthand (`// rest of code…`, `(unchanged)`) — so a placeholder gets applied
  verbatim, corrupting the file *past* the hash check. Reject pre-apply. Directly
  serves the no-silent-corruption directive.
- **Structured `ask_user` tool.** `KjResult::Latch` is a single destructive-op
  confirmation, not a model-callable way to surface ambiguous decisions mid-turn.
  gemini `ask_user` submits a batch of typed questions (text/confirm/choice) that
  block until answered. Kernel supplies the interrupt primitive; harness chooses to
  expose it.
- **Optional edit-correction hook.** ⚠️ When `old_string` misses, gemini runs a
  second LLM pass to repair the search string (fuzzy fallback first). kaijutsu fails
  loud *by design*. Don't auto-repair (corruption risk) — but emit a structured
  error + correction-context block so a harness can opt into recovery, mirroring
  gemini's `getDisableLLMCorrection` toggle.
- **Plan-mode toggle.** `read_only_shell` is a static binding, not a model-asserted
  mid-session mode. gemini `enter_plan_mode`/`exit_plan_mode` flips to read-only with
  a visible reason. A lightweight plan-mode token (vs the heavier fork) for
  single-session exploration constraint, surfaced to the harness via a `KjResult` variant.
- **Socket hook vs. Hook Table alignment.** The legacy MCP socket hook (for session mirroring) has drifted from core structures, causing silent data loss
  (e.g., `agent_id` vs `principal_id` mismatch, obsolete `tool_response` key, fragile PID-based socket discovery). Details in
  the design pass in git history (`docs/mcp-hook-alignment.md`, deleted 2026-07-04).
- **Silent fallbacks in tool/binding lookup.** [Kernel::list_tool_defs_via_broker](file:///home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/kernel.rs#L465) maps lookup errors
  to empty vectors, silently stripping the LLM of tools. [dispatch_tool_via_broker_with_cancel](file:///home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/kernel.rs#L336) defaults to empty bindings on lookup
  failure, causing confusing `ToolNotFound` errors rather than propagating the underlying DB/resolver error.
  Related quirk (found 2026-07-04): `Broker::list_visible_tools` reads the in-memory binding directly
  (`unwrap_or_default`) instead of the lazy-loading `binding()`, so the FIRST dispatch in a fresh process
  queries zero servers until `binding()` caches on that same call — self-heals, but a latent surprise.
- **Latency overhead on visible tool scans.** [list_visible_tools](file:///home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/mcp/broker.rs#L1081) is called on **every single tool dispatch** to refresh naming resolutions, causing lock contention
  on `self.instances` and `self.bindings` and extra async hops. These resolutions should be cached per context and invalidated only when bindings change.
- **Contradictory hook persistence documentation.** [BuiltinBindingsServer](file:///home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/mcp/servers/bindings_builtin.rs#L64) claims hooks are "in-memory only" when they are actually eagerly hydrated and written to SQLite in `broker.rs`.

### Safety, sandboxing & policy

- **Kernel-level process isolation for kaish shell — HIGH.** EmbeddedKaish runs real
  binaries with the full kernel process's privileges; `WorkspaceGuard` is VFS-layer
  only and is bypassed the moment a builtin shells out via `LocalBackend`. A
  compromised tool can read `/etc/passwd`, `ptrace`, or exfiltrate keys. gemini wraps
  exec in `bwrap --unshare-all` + seccomp (Linux) / seatbelt (macOS). Add an OS-isolation
  wrapper for shell-tool exec, toggled by a capability binding.
- **Env/secret masking for agent-invoked shell — HIGH (supply-chain).** A
  coder-context agent can `echo $ANTHROPIC_API_KEY` — the context env (incl. provider
  keys) is handed to kaish unstripped. gemini bind-mounts zero-byte files over `.env*`
  and strips `*_API_KEY`/`*_TOKEN`/`GITHUB_*` from the sandboxed env. Strip
  credential-pattern vars from the env visible to agent shell commands; configurable allowlist.
- **Network egress cap.** Capability model has no network axis; MCP subprocesses get
  unrestricted sockets. `npm install`/`curl` leak data with no gate — sharper risk
  given the multi-user SSH model. Add a `network` binding axis (deny-by-default),
  enforced via net-namespace when OS isolation lands.
- **Declarative policy loader + argument-pattern deny rules.** *(merged: the
  sandboxing and extensibility scans both hit this.)* Bindings are coarse (whole
  tool/instance, no arg matching) and only authorable via kaish hook syntax or
  `builtin.bindings`. gemini has a tiered TOML engine (Default<Workspace<User<Admin),
  `argsPattern` regex, `allow`/`deny`/`ask_user`. Add a TOML/`policy.kai` loader that
  hydrates PreCall Deny/Allow hooks from declarative rules (tool glob + args pattern +
  decision) at create time — e.g. "deny `write_file` to `~/.ssh/*`" without writing Rust.
- **Workspace rc trust gate.** No "do you trust this project?" gate before executing
  `.kai` rc from a workspace dir — a malicious rc runs on context create, affecting an
  always-on multi-user kernel. gemini gates project config behind a trust dialog that
  audits discovered commands/MCP/hooks. Require operator approval before running rc
  from a non-trusted-root; surface discovered rc/mcp/binding config first.
- **Sandbox-expansion protocol.** `WorkspaceGuard` denies a path with a hard failure
  and no escalation. gemini surfaces a "grant session/persistent?" modal on denial.
  Emit a structured expansion request (Cap'n Proto event) so the operator can grant
  session-scoped paths without tearing down the context.
- **Pre-execution veto hook (external checker protocol).** MCP hooks fire on
  lifecycle events, not as a pre-call veto. gemini runs external checker subprocesses
  via a versioned JSON protocol (stdin: tool call + context; stdout: allow/deny/ask;
  fail-closed on timeout). Lets operators plug in compliance/content/rate-limit checks
  without patching the kernel — a clean instrument capability.
- **LLM-derived task policy (conseca-analog).** ⚠️ Risky as a sole gate (LLM error
  ⇒ allow). gemini derives per-prompt least-privilege constraints from the request +
  tool list, then enforces at call time. Only as an *optional secondary* stage after
  static bindings, fail-open with telemetry.

### Session & workflow

- **Pre-edit filesystem checkpoint + `kj restore` — HIGH.** `KernelState::checkpoint`
  (`state.rs:160`) snapshots in-memory vars only, not the host FS, and isn't tied to
  tool execution. A bad edit run leaves files half-modified with no mechanical rollback.
  gemini auto-commits a shadow git snapshot before every file-write tool, with
  `/restore`. Auto-snapshot + `kj restore <checkpoint>` to revert FS + conversation.
- **Turn rewind + FS revert.** `kj fork` is a forward branch (explore), the inverse
  of "undo that last edit." gemini `/rewind` walks back N turns and reverses file
  edits (exact-match, patch-merge fallback) with a diff preview. A backward escape
  hatch that doesn't spawn a new context branch.
- **Named conversation bookmarks (save/resume in-place).** Fork diverges history;
  there's no "park this state, try another direction in the *same* context, snap back."
  gemini `/resume save|resume|delete <tag>` snapshots and restores LLM wire history
  in place. Distinct from fork — avoids unbounded DAG branching for quick what-ifs.
- **User-defined prompt command templates.** rc scripts fire on lifecycle events;
  there's no user-authored named command. gemini loads `.toml` from
  `~/.gemini/commands/` + project dirs → `/git:commit` etc. with `{{args}}`. Add
  `~/.config/kaijutsu/commands/` + `<project>/.kaijutsu/commands/`, invocable as
  `kj cmd:<name> [args]`.
- **Inline `@{file}` prompt injection.** The user can't say "here's the file I mean"
  in prompt text — they must wait for the model to choose to call `read`. gemini
  expands `@{path}` (text/image/PDF) in the input before submission. Parse `@{path}`
  in `write_input`, respecting the VFS boundary.
- **`!{shell}` injection in prompt templates.** Pairs with command templates: gemini
  expands `!{git diff --staged}` stdout into the prompt at construction time (policy-
  confirmed), outside the model's tool loop — e.g. a `/git:review` one-liner.
- **Conversation export.** `block_list`/`block_read` extract internally but nothing
  produces a portable file. Add `kj conversation export <path.md|json>` for sharing/
  bug-reports/archival outside the system.

### Extensibility & integration

- **Turn- and model-boundary hooks — HIGH.** The hook table (`mcp/hook_table.rs`:
  PreCall/PostCall/OnError/OnNotification/ListTools) is scoped to MCP tool calls only;
  the socket listener (`hook_listener.rs`) is an inbound *mirror*, not an outbound
  interceptor. gemini has BeforeAgent/AfterAgent/BeforeModel/AfterModel that can
  block/rewrite. The kernel owns the turn loop (`llm_stream.rs`) — add BeforeModel/
  AfterModel + BeforeTurn/AfterTurn phases so rc scripts can reshape requests/responses
  (cache hints, PII filter, retry) without bespoke Rust. **Decided 2026-06-24 — see
  *Cache & cost* below:** phase named `BeforeModelTurn`/`AfterModelTurn`; rename the
  existing MCP `PreCall`/`PostCall` to MCP-scoped names; mechanics(Rust transport) /
  policy(per-provider data) / decisions(kaish hook) split; contract = `HookAction`
  verdict + stdout→block payload (append-only).
- **Headless one-shot with JSONL streaming — HIGH.** `kj drive --prompt`
  (`kj/drive.rs:93`) fires-and-returns; the turn runs server-side with no
  consume-until-done path. CI/eval harnesses need a blocking subprocess. Add
  `kj run --prompt … --output-format jsonl` that streams turn events
  (turn.requested/tool_call/tool_result/turn.completed) and exits with a machine code.
  *(relates to the existing "headless turn cwd is `/`" item.)*
- **Python/TS thin SDK.** `kaijutsu-client` is full-featured but requires Rust
  compilation; eval/CI tooling lives in Python/TS. Wrap `kj run --json` JSONL (or the
  RPC bindings) into an async session driver so harnesses don't compile Rust.
- **IDE peer integration.** No editor bridge (`kaijutsu-editor` is a terminal vi
  builtin, not an IDE plugin). The peer model (`PeerRegistry`/`invoke_peer`) already
  fits: a VS Code extension registers as a kaijutsu peer, sends open-file/cursor/
  selection blocks into the active context, and renders kernel-proposed edits as diffs.
- **Extension bundle manifest.** rc bundles exist but with no named-unit manifest,
  install/update lifecycle, or scoped enable/disable. gemini's `gemini-extension.json`
  bundles MCP servers + hooks + commands + context as one versioned, git-URL-installable
  unit. An `extension.toml` (rc scripts + contrib adapters + context configs) installable
  via `kj extension install <git-url>` — configures the instrument, doesn't host it.
- **Hook fingerprinting / trust.** CRDT ownership is the integrity model but there's
  no change-detection warning when an rc/hook body changes via `kj rc reset` or sync
  (extends the existing "stale rc seed" item). gemini fingerprints project hooks and
  warns on change. Track hook-body hashes; warn/block-by-default on unexpected change.

### Cache & cost — decided direction (2026-06-24)

A working session with the lead context converged several candidates above into
decisions. Organizing lens: **the Anthropic prompt cache is a prefix match — any byte
change in the `tools → system → messages` prefix invalidates every cached token after
it** (writes 1.25×/5m, reads ~0.1×, ≤4 breakpoints, model-scoped). We already ship the
machinery: `cache_breakpoints: Vec<CacheTarget>` (`llm/stream.rs`), set per-context via
rc create/fork/drift (`project_cache_breakpoint_policy`); `usage.cache_*` parsed back
(`llm/claude/stream.rs`). So these are placement/policy decisions, not new infra.

- **Cache placement is load-bearing, not cosmetic.** Three rules fall out of the prefix
  invariant and should hold by construction:
  - **Date/OS/cwd in situational context** (the "cheap ~20 token win") is a *silent
    invalidator* if it lands in the cached `system` prompt — date rolls at midnight, cwd
    churns, blowing tools+system every change. MUST land *after the last breakpoint* (a
    message), never in `build_system_prompt`.
  - **JIT `KAIJUTSU.md` injection** must *append to the tool result* (extends the prefix,
    cache-neutral), not re-hydrate into `system` (mutates prefix, cache-hostile). Same
    content, opposite cost by placement.
  - **Model switching invalidates the whole cache** (model-scoped). Classifier routing /
    fallback-chain must therefore be fork/subagent-grained, never per-turn — reinforces
    the ⚠️ opt-in framing.
- **Compression: not pursued.** SQLite-on-btrfs (compressed) covers storage for a long
  horizon; conversations flush organically to `signoff.md` near ~80% window and restart.
  If it ever lands, it fires only at the fork/hydrate boundary (cache already cold),
  never mid-conversation.
- **AdaptiveTokenCalculator — EMA, not PID.** Token estimation is an observer problem,
  not control: use an **EMA** for the chars→tokens ratio, calibrated by the provider
  `usage` we already parse. No local Claude tokenizer exists and `tiktoken` is wrong for
  Claude, so the loop is: local estimate gates the (block-count) windowing in
  `mailbox.rs` + a near-limit warning; provider `usage` corrects the ratio after each
  turn. A static **per-model input-limit table** is just config and kills the "blindly
  400'd by the provider" case on its own. Optional follow-up: escalate to the
  `count_tokens` endpoint only when the estimate is within ~10% of the limit. No
  budget→window controller — windows aren't dynamic in practice.
- **Per-turn seam: `BeforeModelTurn` / `AfterModelTurn`.** A new turn-loop hook phase,
  *distinct from* the MCP-tool-call hooks. **Rename the existing `PreCall`/`PostCall`
  (`mcp/hook_table.rs`) to MCP-scoped names** — they only fire around MCP tool calls — so
  the two surfaces are separable and a script can subscribe to just one. Design:
  - **Mechanics compiled, policy as data, decisions as hooks.** The retry *loop*
    (backoff, jitter, `Retry-After`, SSE re-issue) is one Rust implementation in the
    transport. The retry *policy* is a per-provider data table (max attempts, base delay,
    jitter %, retryable codes). "Gemini has different retry needs" (e.g.
    `RESOURCE_EXHAUSTED` vs bare 429) is a **policy row, not a code fork** — folds into
    the declarative-policy-loader item. Per-turn *decisions* are the kaish hook surface.
  - **Engine always runs with sensible defaults** — no "zero-overhead when unhooked"
    special case; the retry/policy engine works unconfigured. A *slow* hook script is the
    author's problem, not the framework's.
  - **Append-only / transport-wrapping only** — a hook may append a `role:"system"` note
    (cache-safe mid-conversation injection on Opus 4.8) or wrap the call; it must never
    rewrite the cached prefix. Enforced by the channel shape below.
  - **Contract — three channels, each already precedented:** verdict =
    `HookAction::{Allow, Deny(reason), Log}` (mirror the existing MCP hook return, don't
    invent a parallel protocol); payload = **stdout → block** (the `rc .kai` stdout-
    producer idiom — stdout becomes an *appended* block, so a hook physically cannot
    rewrite the prefix; System/Text → mid-conversation system note, Trace → model-hidden
    usage capture for the EMA); side effects = the script calling builtins (KV, drift),
    its own business, *not* the verdict path (a tool call as the return path is a
    reentrancy trap). stdin carries the event-kind + assembled-request metadata (model,
    context_type, token estimate).
- **Fork-boundary rc vs per-turn hook — don't conflate.** Fork-boundary rc owns
  *context-shaping* and runs once per hydrate boundary: transplanting a conversation (or
  a selected interval) into a new `context_type` is fork-with-filters — the interval
  primitive is already LOCKED (`docs/fork-filters.md`), and retargeting `context_type`
  just runs that type's create rc. The per-turn seam owns only the reactive/mechanical
  (retry, estimate-gate, usage capture). Rewriting the request every turn would fight the
  cache by construction — keep that out of the per-turn hook.

**Remaining work (not yet code; the `HookPhase`→`McpHookPhase` rename already
shipped 2026-06-24, freeing the sibling enum):**
- **Per-model input-limit table** — static config + `model_input_limit(model) -> Option<u32>`.
  Kills the "blindly 400'd by the provider" case on its own; foundation for the calculator.
- **AdaptiveTokenCalculator** — EMA chars→tokens ratio, calibrated by the provider `usage`
  already parsed at `llm/claude/stream.rs`. Feeds the (block-count) windowing in
  `mailbox.rs` + a near-limit warning. No local Claude tokenizer; `tiktoken` is wrong.
  Optional follow-up: escalate to the `count_tokens` endpoint only within ~10% of the limit.
- **`RetryPolicy` data type + per-provider table** — one Rust backoff engine (jitter,
  `Retry-After`, SSE re-issue) reads it; provider divergence (gemini `RESOURCE_EXHAUSTED`
  vs bare 429) is a policy row, not a code fork. Engine runs with sensible defaults even
  unconfigured (no zero-overhead-when-unhooked special case).
- **`BeforeModelTurn`/`AfterModelTurn` sibling phase** (e.g. `ModelTurnPhase { Before, After }`)
  on the LLM turn loop. Contract: `HookAction` verdict + stdout→block payload (append-only)
  + side-effects-via-builtins; stdin carries event-kind + assembled-request metadata.
  ⚠️ **OPEN FORK: reuse the `HookEntry`/`HookAction`/kaish-body/persistence stack, or a
  parallel table? Decide before laying code.**
- **Encode the cache-placement rules by construction:** situational date/OS/cwd lands
  *after* the last breakpoint (a message, not `build_system_prompt`); per-directory
  `KAIJUTSU.md` *appends to the tool result*, never re-hydrates `system`.

---
## kaijutsu-abc — ABC v2.1 spec conformance (audit 2026-06-30)

Three-model holistic audit; 14+ bugs fixed TDD across two rounds (lists in
devlog/git — suite 320 → 336 green).

**Still open:**
- **LOW — tuplet default-q for `(5 (7 (9` ignores compound meter** (3 in 6/8). §4.13. Skipped:
  `default_q` is computed in `try_parse_tuplet` with no meter access; threading the meter
  through `parse_body → … → try_parse_tuplet` is high churn (10 test call sites) for a rare
  corner (5/7/9 *without* explicit `:q` *in compound meter*).
- **LOW — `Duration::to_ticks` integer-truncates** (odd denominators; inaudible at 480 TPQN).
  Would need rational accumulation; leave unless it bites.
- **LAYOUT (rendering phase) — `+:` continuation corrupts lyric alignment** (joined with `\n`;
  `tokenize_lyrics` doesn't treat `\n` as whitespace). §3.3.
- **LAYOUT (rendering phase) — lyrics `w:` `|` barline-sync marker ignored** (v1 limit). §5.1.
- **Engrave parity (rendering phase):** `engrave/layout.rs` has its own copies of the
  tuplet-drops-rests/chords and key-signature bugs — fix when we move to rendering.

**Verified NOT bugs (don't "fix"):** cross-octave accidental propagation (spec default
`%%propagate-accidentals pitch` = all octaves); unit-length default; broken-rhythm multipliers.

---

## Players / loadout

- **EXPLORE — give players a read-only kaish instead of "tool-free" (found 2026-06-30,
  standing up the bass player).** Today a musician's loadout grants only `drive` and **no
  tools at all**, because a small local model handed the full tool palette emits a thinking
  block then *hangs* (GPU cold, no completion, no error — a fail-loud violation; the
  hard-won Chameleon lesson, `project_chameleon_first_loop`). "Tool-free" was the blunt
  fix. The better future: a **read-only kaish** loadout — the same RO-kaish posture kaibo
  already uses (reads the repo, never mutates), which is *great* for cheap on-the-fly
  arithmetic/lookups that are cheaper via a tool than via the model's weights (true for
  humans and models alike). A player could compute bar math, transpositions, scale degrees,
  etc. with RO kaish rather than burning weights or risking a wrong count. **Not wiring
  this now** — the immediate bar-fill math is precomputed in the tick rc (kaish math in
  `musician/tick/S10-drive.kai`, injected as spelled-out facts), so the model needs no tool.
  But RO-kaish-for-players is worth designing: it removes the "tool palette = hang" cliff by
  construction (no mutation surface to stall on) and makes the calculator-as-tool option
  real. Pairs with the precompute-in-rc win (rc does the arithmetic) — RO kaish is the
  *escape hatch* for math the rc didn't precompute. Decide: which RO builtins (math/`expr`,
  read-only `grep`/`glob`, block/resource reads — but no mutation) + whether small local
  models tolerate a *read-only* palette where they choke on the full one.

## kaijutsu-abc — engrave (SVG rendering) audit (2026-06-30, kaibo/deepseek)

Audit of engrave/layout.rs; the fix rounds shipped in `d722f492`/`8fb17d87`
(lists in git). Remaining, ranked; delete when shipped. (Most are IR-assertable
in tests/engrave_tests.rs.)

**Still open:**
- **MED — `K: middle=<pitch>` ignored** (only the per-clef default middle line is used).
- **LOW — grace notes use the regular notehead glyph**, not the SMuFL small notehead.
- **LOW — every `SourceSpan` is hardcoded `(0,0)`**, so click-to-edit span attrs are dead.
- **POLISH — title text can overlap a tuplet bracket** when the first group is near the start
  (title baseline ≈ bracket y); nudge the title up or the bracket down.
- ~~MED — redundant key-sig accidentals~~ — VERIFIED NOT A BUG: the parser doesn't stamp
  key-sig accidentals onto `note.accidental`, so `K:G FFFF` draws exactly 1 sharp. (False positive.)

---

## kj config / shell surface (papercuts — found 2026-06-30 wiring local llama.cpp providers)

Standing up a local-model musician meant editing `models.toml`, which surfaced a
cluster of friction in the config + shell surface:

- **Config drift is silent (want a `kj config doctor`).** The live CRDT
  `models.toml` pointed its local providers at `ollama` (:11434) and `lemonade`
  (:8000) — both stopped/disabled — with **no provider** for the actually-running
  llama.cpp servers (:2020 gemma4-26b, :2021 gemma4-e4b); the stale host
  `~/.config/kaijutsu/models.toml` pointed at a *third* dead endpoint (vestigial
  lemonade :13305). Nothing flags that a configured provider's `base_url` is
  unreachable until a turn fails (or, worse, hangs). A `kj config doctor` /
  startup probe that pings each enabled provider's `base_url` and warns on the dead
  ones would turn a silent config-vs-reality drift into a loud one (same class as
  the rc/source drift we watch for).
- ~~`kj config set` ignores piped stdin~~ / ~~No `kj config edit`~~ — **SHIPPED
  2026-07-04**: stdin promotion generalized from the rc-only gate
  (`wants_stdin_content()`, `runtime/kj_builtin.rs`) so piping works as the
  help always claimed, and `kj config edit <path>` opens a vi session via the
  same `editor_open_signaled` path as `kj rc edit` (`ConfigCommand::Edit`;
  `Set`'s old `edit` alias retired).
- **The MCP/context shell is read-only for host writes** — `> file` (even
  `> /dev/null`) fails `redirect: read-only filesystem`, so you can't stage a temp
  file in-shell; the edit had to be staged via a separate host write and read back.
  If RO is intentional, a `/dev/null` sink and a writable scratch dir would remove
  the sharp edge for scripting.
- **`kj context create` took ~60 s** for one musician (others were instant) —
  anomalous, possibly a blocking create-time hydrate/model call; worth a look.
- **(unreproduced) a `kj` shell call hung the full 300 s timeout once** mid-session;
  `kj context list --tree` and `kj model list` both return fast now, so noting it
  as a one-off to watch, not a fixable repro yet.
- **Provider naming: the `type =` field decision (residue of the silent
  unknown-provider drop; the fail-loud halves SHIPPED 2026-07-04).** Shipped:
  `kj config set`/`edit` on `models.toml` now parses the TOML and **rejects**
  unknown `[providers.<name>]` types at write time, and the boot warn says
  `unknown provider type 'X' (supported: anthropic, deepseek, openai, ollama,
  lemonade, local)` — the "missing API key?" guess now appears only on actual
  `AuthError`. Remaining design decision (human call): a real `type =` field so
  a provider can be named freely (`[providers.gemma-e4b] type = "openai"`)
  instead of overloading the four OpenAI-compat type-names as base_url slots.
  **Latent skew found while implementing:** `Provider::from_config` still
  accepts `"claude"` as a legacy alias for `"anthropic"`, but the write-time
  validator doesn't — `[providers.claude]` boots fine but is now rejected at
  `kj config set`. No shipped config uses it; decide alias-in-both or
  alias-nowhere (dovetails with the `type =` decision). Also (deepseek review
  2026-07-04, LOW): the interactive `kj config edit` editor-session save path
  writes through `blocks.edit_text()` and never calls `validate_config_write`
  — only `set`/`edit --content` validate. Backstopped by the boot-time
  unknown-type warn and the `config-write` gate; close it by validating at
  editor save/flush for config-owned docs if it ever bites.
- **Two `models.toml` files, only one is read.** The kernel loads providers from
  the **CRDT** `/etc/config/models.toml` (via `kj config`), and the legacy host
  `~/.config/kaijutsu/models.toml` is **ignored** — but it still exists, looks
  authoritative, and disagrees (it had a vestigial `openai-local` → dead :13305).
  Editing the host file does nothing; you must `kj config set`. Either delete the
  host file on migration, or have the kernel warn that it found+ignored it. (Same
  CRDT-vs-host ownership confusion as rc, but here there's a stale host artifact
  actively misleading.)

---

## kaish PATH / external binary access

Observed 2026-07-02 during Music Demo #1 (`019f249d`): kaish has no `$PATH` and
won't run binaries by absolute path (`/usr/bin/aconnect`, `/usr/bin/pw-cli` etc. all
hit "command not found"). `export PATH=...` is rejected as "undefined variable".
`which` is also absent. Only binaries in kaish's built-in command set are reachable.

Practical consequence: any shell step that needs a system tool (ALSA `aconnect`,
PipeWire `pw-cli`/`wpctl`, `which`, etc.) silently fails with no obvious
workaround from inside an agent turn. We had to ask the user to run `aconnect 128:0
129:0` manually to wire the app's render port to TiMidity.

**Diagnosed + FIXED (slice 1) 2026-07-03 — it was never PATH; external exec was
compiled out three layers deep.** Full design + direction now canonical in
`docs/mounts.md` (the "opaque host" inversion: drop the host-root mount, curate
PATH-dir bin mounts per context_type, VFS-mediated resolution upstream in kaish).
Slice 1 shipped: `subprocess` feature on; `ExternalExec` deny-by-default policy at
materialization gated on the new `exec` loadout authority (coder/mcp/default +
director seeds grant it; musician/toolie never); `MountBackend::resolve_real_path`
implemented (sync mount-table walk + `VfsOps::real_root`); `$PATH` seeded from the
kernel process env into exec-granted shells.

**Open remainder:**

- **Pre-slice contexts need a one-time `kj binding allow exec`** from a
  binding-admin context. The deploy latch itself is DONE (2026-07-03: both
  S10-binding rc seeds reset, kaijutsu-server rebuilt + restarted, verified
  live incl. re-making the aconnect wire from a context shell) — but rc fires
  only at lifecycle boundaries, so contexts created before slice 1 keep their
  exec-less loadout until re-created or manually widened.
- **`kj audio` / `kj midi` verbs still worth having** for the ALSA wiring
  operations (connect, disconnect, list-clients): the wire is kernel-owned state,
  not a shell errand, and the musician-adjacent flow shouldn't need raw
  `aconnect` even with exec working. Related: nothing owns the
  `aconnect 128:0 129:0` app→TiMidity wire; it dies on every app restart (the
  app auto-connecting its render port when TiMidity is present is the likely
  home).
- ~~Unknown-command 300 s hang~~ — **CLOSED 2026-07-04, dispatch proven
  bounded.** The fall-through path (kaish → `call_tool` → broker →
  `ToolNotFound` → 127) has no unbounded await — verified by unit tests in all
  three shell flavors (deny / read-only / exec-granted, each traversing the
  full builtin broker set), a kaibo cross-model audit, and a live-kernel probe
  (bare `mount` ≈ 300 ms). The original "git fast / mount hang" contrast was
  cross-regime: pre-subprocess `git` fast-failed 127; post-subprocess `mount`
  spawns the real binary (bounded by the shell request timeout). Regression
  tests now lock the fast-fail invariant; the likely culprit for the observed
  300 s was the known stale-FlowBus MCP observation gap, not execution.
- **kaish `resolve_in_path` does synchronous `std::fs` stats on the tokio
  worker** for each `$PATH` dir when a name misses early — fine normally, but
  a `$PATH` entry on a hung filesystem would block a worker thread.
  (kaish-crate concern, `~/src/kaish`; found 2026-07-04 during the
  unknown-command investigation.)
- **Later slices** (bin-mount catalog, VFS-mediated resolution, dropping the
  host-root mount): `docs/mounts.md`, coordinated with the kaish mounts release.
- **MCP-created context invisible to `kj context list` after kernel restart
  (found 2026-07-03 during the exec live-verify).** A `register_session` context
  (`investigate-d1d3257e`) kept working across a kernel restart — shell executes,
  blocks write, `kj context switch <full id>` resolves it with its label — but
  `kj context list` no longer shows it, and prefix/label resolution
  (`kj binding allow exec 019f29bb`) fails "no context matches". The row is
  durable; the *listing* filter loses it. Smells like the peer/registry
  re-attach gap (auto-memory `tech_debt_peer_reattach_on_reconnect`) extended to
  MCP sessions: list is registry-driven, resolution-by-full-id is DB-driven.
  Symptom cost: an operator can't see or target a live working context by
  prefix. Find where `list_all_contexts` vs the session/registry filter diverge
  post-restart.

---

## Context time awareness — per-type date/time injection (found 2026-07-03; slice 1 SHIPPED 2026-07-04)

In-app contexts had no wall-clock source, so models hallucinated dates in
durable artifacts (three incidents — the third being the 2026-07-04 issues.md
ghost re-introducing an already-corrected date).

**Slice 1 SHIPPED 2026-07-04:** `lib/{create/S25,fork/S40}-datetime.kai` rc
seeds (kaish's chrono-backed `date` builtin → `kj block create --kind
notification`), symlinked init.d-style into coder/director/mcp/default;
musician/toolie deliberately get none (musical time is their only time base).
`BlockKind::Notification` was the load-bearing choice: it hydrates as an
appended user-role message and is never swept into the system prompt — a
`(Role::System, BlockKind::Text)` block would be folded into the cached prefix
by `extract_system_prompt_sections` on every call and silently invalidate the
`--target=system` breakpoint daily (the exact anti-pattern the cache-placement
rules forbid; rc `.kai` stdout was also ruled out — it lands as model-hidden
`Trace`). Tests pin the mechanism: visible in hydrate, absent from
system-prompt sections, per-type policy matrix, fork re-seeds.

**Remaining — cadence (slice 2, not-now):** regular re-seeding (director's
"note when the turn gap crosses a threshold / every N turns") wants the
`BeforeModelTurn` hook seam (Turn Loop section) once it lands; per-turn drip
stays out of the cached prefix by the same placement rule.

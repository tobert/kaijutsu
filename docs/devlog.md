# kaijutsu devlog

Narrative history of landed work and the decisions behind it — the "how we got
here" color that doesn't belong in [issues.md](issues.md) (open work) or the
code. This is *not* the authoritative record: `git log` (commits, SHAs) is
canonical and the design docs under `docs/` hold the locked decisions. This is
the story, melted out of the ephemeral session handoffs as they retire.

Newest work first within each area; dates are when the work landed.

---

## SSH transport & RPC surface (design: `docs/sftp.md`)

### Client actor connects eagerly, not on first command (landed 2026-06-26)

Surfaced while flag-day-reconnecting the MCP during the named-subsystem cutover
(below): the first call after any cold start returned `not ready: idle`, and you
had to call twice. That was by design — the client RPC actor
(`kaijutsu-client/src/actor.rs`) was a *lazy*-connect FSM: it rested in `Idle`
and only dialed when the first command arrived, deliberately rejecting that
command (`NotReady(Idle)`) rather than blocking the caller for up to ~25s through
the SSH+capnp handshake. Reasonable for a retry-loop poller, but the MCP/app tool
surface doesn't absorb the kick, so the `NotReady` leaked to the human/agent as a
visible first-call failure on every reconnect.

Amy's steer: a client should reach for the connection as soon as it can — the
early connected/failed signal is worth more than the apparent efficiency of
deferring, and it shouldn't sit in a state that bounces commands. So the `Idle`
arm now calls `start_connecting(1)` immediately instead of waiting for a command;
`Idle` becomes a transient bootstrap the actor never rests in. The change is
surgical because the post-disconnect path *already* self-reconnected
(`finish_closing` → `Cooldown` → timer → `Connecting`); only cold start waited on
a command. Commands that race the handshake are still handled by the `Connecting`
arm (`NotReady(Connecting)`, or served once `Connected`), and shutdown (mpsc
closed) is observed there too — so nothing regressed. The e2e contract test was
rewritten from `first_call_rejected_with_idle_then_actor_connects` to
`actor_connects_eagerly_without_a_command`: it waits for `Connected` with **no
command sent**, which hangs→times-out on the old lazy code. Live-validated over
MCP — the first `whoami` after reconnect now reaches the kernel instead of being
bounced.

### RPC migrates from channel-ordinal to a named SSH subsystem (landed 2026-06-26)

Slice 0 of the SSH-surface renovation in `docs/sftp.md`. The Cap'n Proto RPC
transport used to ride a **positional** channel scheme: the client opened three
session channels in a fixed order (control / rpc / events) and the server keyed
the RPC handler purely by **ordinal** — only channel index 1 got the capnp
handler thread. `control` and `events` were dead weight, retained client-side in
an `Arc` only to hold the connection open; the real event stream already flows
as capnp *over the single RPC channel*. So two of three channels existed only to
pad the ordinal.

Now the client opens **one** channel and calls `request_subsystem(true,
"kaijutsu-rpc")` before `into_stream()`. The server stashes every opened channel
in a per-connection `HashMap<ChannelId, Channel<Msg>>` at `channel_open_session`
time and implements `subsystem_request`, which drains the map and dispatches by
name: `kaijutsu-rpc` spawns the existing RPC thread (extracted into a
`ConnectionHandler::spawn_rpc_thread` helper); an unknown name gets
`channel_failure` + `close`. Every exit path acks (`channel_success` or
`channel_failure`) because the client requests with `want_reply` — a silent
return would hang it. `channel_close` also drops the channel from the pending
map. The subsystem name lives in one shared const, `kaijutsu_types::
SSH_RPC_SUBSYSTEM`. Client-side, `SshChannels` and `retain_ssh_channels` are
gone; `connect()` now delegates to a generalized `connect_subsystem(name)` seam
(the same seam a future kaijutsu-native subsystem — e.g. debug-kaish — requests
through) and returns the one bound channel.

This is the **shared retention-and-dispatch scaffold** the rest of the plan
needs: SFTP and a debug-kaish shell become additional `match name` arms over the
same pending-channel map. Doing RPC-named *first* proves the scaffold on the path
we exercise constantly — the full SSH e2e suite (24 `rpc_integration` tests incl.
subscriptions, 7 `reconnect_fsm`, 3 `editor_wire`, 8 `peer_e2e`) stayed green,
and a new `test_unknown_subsystem_is_not_bound_to_rpc` guards against any
ordinal-style "attach RPC to any channel" fallback. **Breaking wire change**, no
compat shim — a flag-day cutover (early dev, single user): kernel + app + MCP all
rebuilt together. A deepseek review via kaibo confirmed the refactor correct;
its two notes (a misleading `connect_subsystem` doc comment — `want_reply` makes
the *server* ack, it doesn't fail the client call early; and a defensive
`pending_channels.remove` on the unreachable unauthenticated path) were folded in.

---

## VFS & filesystem

### `FileAttr.generation` — coherence stamp split from display mtime (landed 2026-06-25)

Prework for exposing the VFS over SFTP (design: `docs/sftp.md`). A Gemini Pro
review of that design surfaced that the cache leaned on mtime as its coherence
signal, and Amy flagged the deeper worry: was virtual-file mtime "really a
counter"? It wasn't — `FileAttr.mtime` is a real `SystemTime` everywhere — but
`ConfigCrdtFs` had two footguns (seeded files reporting `UNIX_EPOCH`; a silent
no-op `setattr(mtime)`) and a latent coherence bug: mtime was wall-clock
`now()` on write, so two writes inside one clock tick didn't advance it, while
`FileDocumentCache`'s staleness reload requires a strict advance.

Decision (Amy, from a three-way choice): introduce a **generation counter** as
the real coherence primitive and keep mtime purely for human display — rather
than clamping wall-clock to monotone, or mapping a logical version into a
`SystemTime` (which would show 1970-ish garbage in `ls -l`). So `FileAttr` gains
`generation: u64`: `ConfigCrdtFs`/`MemoryBackend` source it from a monotonic
per-backend counter bumped on every content mutation; `LocalBackend` derives it
from host mtime-nanos. The cache compares generation (`loaded_generation`, the
`d > l` check), not mtime. `setattr(mtime)` is now honored for display on the
CRDT and memory backends but deliberately does not bump generation, so `cp -p`
/ `touch -d` / rsync stop silently losing mtime there *and* a pure attribute
touch never triggers a reload. (`LocalBackend::setattr` mtime is still a no-op —
a pre-existing item in `issues.md`, untouched here.) The `UNIX_EPOCH` default is
replaced by a real backend-creation timestamp. The same generation is what a
future SFTP `OPEN` will capture for its TOCTOU re-verify — handle guard and
cache share one primitive.

A follow-up kaibo review pass (deepseek + chimera) then caught three real
defects in the first cut, fixed the same day: a `ConfigCrdtFs::bump()`
fetch-then-insert race that could *reverse* generation under concurrent writers
to one path (now a `max`-folded DashMap `entry`); `MemoryBackend::rename`
carrying the source's stale generation to the destination, regressing it when
overwriting a higher-generation target (now stamps a fresh generation on every
moved entry); and a spurious generation bump when `setattr(size=…)` hit a
directory/symlink where the resize was a no-op. TDD throughout: same-instant
strict-advance tests on both CRDT and memory backends, a rename-no-regress
regression, plus epoch-default and display-only-setattr regressions; full
workspace green.

---

## In-app vi editor

Editing as a kernel-owned session: `EditorCore` (pure modalkit vim) → kernel
`EditorSessions` → tool-shaped surface, with the Bevy app as one renderer among
many drivers. Design + live state: `docs/vi.md`. Memory: `project_vi_editor`.

### Front doors + slice-2 groundwork (landed 2026-06-23)

Slice 1 (kernel, headless) was already green; this stretch added the ergonomic
front doors and started the app-renderer groundwork.

- **`vi`/`edit` builtin + `kj rc edit` opens the editor** (`82cd5f8`). A real
  kaish `Tool` (`ViBuiltin`, registered in `context_shell.rs` under both names)
  and bare `kj rc edit <path>` (no `--content`) both open a session via the one
  shared `Kernel::editor_open` primitive — three front doors, one primitive, one
  `EditorState::to_json` shape. Decisions (Amy): a real builtin, not a `kj editor
  open` alias; session-id-only (no peer signal until slice 2). Verified live over
  MCP (`vi`/`edit`/`keys`/`state`/`quit`).
- **Render-path decision: Design A** (`docs/vi.md` risk #7). The app will render
  the editor from a kernel-served `editor_state` channel and never join the
  editor context into `DocumentCache` — so the feared `DocKind`-discriminator
  collision can't occur (the cache only holds *joined* contexts; ops for
  un-joined contexts are dropped). Item "DocKind discriminator" evaporated.
- **General Screen-transition fix** (`116ed57`). A landed context switch now
  drives the `Screen` FSM (`screen_revealing_switched_context`), so a
  kernel/peer-driven `switch_context` or server-pushed `ContextSwitched` while
  the app is in the time well reveals the conversation instead of stranding the
  user in the well — closing `tech_debt_switch_context_screen_transition`.
  Runner-verified (in the well → `kj context switch` → pops to conversation).
- **invoke_peer double-encoding fix** (`4614c29`). Object `params` arrived at the
  MCP server as a JSON *string*; `normalize_peer_params` unwraps one layer.
  Surfaced verifying the Screen fix; de-risks slice-2's editor-open signal.
- **App-id addressing infrastructure** (decisions: submitter-aware routing with
  principal fallback; build the infra now). Built in slices, all committed +
  tested + single-window runner-verified:
  - `whoami` stamps `principalId` (`55d285e`) — the canonical
    principal-population gap (wire field existed, nobody set it; server is
    authoritative).
  - Peer registry keyed by per-window `instance` + server-stamped `principal` +
    by-nick/principal/instance addressing (`41e236c`), fixing the latent "second
    window evicts the first" clobber.
  - Bridge-task self-detach on `conn_cancel` + `reap_closed` backstop
    (`bdcc0b2a`) — the peer-invoke bridge previously lingered on connection drop
    (parked on `rx.recv()` with the registry holding the sender); now it follows
    the FlowBus bridges' cancel idiom.
  - capnp `instance` + app mints a per-process UUID + a `same_channel`
    self-detach **identity guard** (`63ff4d7a`) — the peer e2e caught that an old
    task's self-detach would otherwise clobber a peer that re-attached under the
    same key (reconnect / same nick). Verified live: the app registers under a
    real instance and a `kj context switch` reaches it through the new registry.
  Remaining (slice-2, with `open_editor`): capture the submitter at
  `editor_open` and route (verify `KjCaller.principal` is populated there); live
  2-window coexistence check.

## Time-well context browser

The radial 3D "well" of context cards (`docs/viz-substrate.md` build order;
`docs/time-well-concepts.md` UX). **Ctrl+W** enters from the conversation, Esc
leaves. Two concrete consumers (active band-0 nav, semantic haystack band-2)
drive the substrate; `ViewSpec` extraction waits on a third (don't build
speculatively). Memory: `project_time_well_context_browser`.

### Edge HUD → in-scene MSDF panels (landed 2026-06-18)

The HUD's first-cut flat Bevy `Text` nodes became in-scene **MSDF panels**: 3D
quads parented to the well camera (screen-stable, no billboard), drawn as thin
glowing accent-tinted borders with no body fill (`WellCardMaterial.border`
uniform), MSDF text inside — same HDR/bloom + depth vocabulary as the cards. N
centered top with the context name, E/W tucked into the top corners, S hidden.
Placed via the pure unit-tested `hud_slot_offset` (aspect-adaptive, size-aware
fit). Built on the shared `view/time_well/panel.rs` primitive (`create_msdf_panel`
+ `commit_panel_glyphs`), also used by the rim/reading cards. Same pass renamed
the RTT plumbing: `vello_ui_texture.rs` → `ui_rtt.rs`, `VelloUiTexture` →
`UiRttTexture`, `VelloUiScene` → `UiVectorScene` (vello-only; pure-MSDF surfaces
carry no vello type now).

### HDR + Bloom via one shared camera (landed 2026-06-17, `9c5e831`+`4ae2f6d`)

Dissolved the two-camera composite that made bloom'd cards vanish. The app's lone
camera is now a single always-on `Camera3d` (`main::setup_camera`,
`IsDefaultUiCamera`) with `Hdr` + thresholded additive `Bloom` +
`Tonemapping::TonyMcMapface`. Bevy UI renders on it and the UI pass runs *after*
tonemapping/bloom, so the conversation is visually untouched; the well's 3D card
meshes bloom. The well no longer spawns its own camera — enter/exit repurpose the
shared one (add/remove the `TimeWellCamera` marker + swap clear color). The
earlier "NOT bloom / SDF-only" reframe was reverted; `well_card.wgsl` drives the
bling to HDR (>1.0) so only it blooms (bodies stay crisp): selection = breathing
blue rim, lineage = amber rim, status = breathing teal (running) / steady red
(error). **Decision worth keeping:** bloom is the *right* tool here — colors are
placeholders so app-wide HDR is fine.

### Card material foundation → MSDF text → full GPU card (landed 2026-06-17)

Cards moved off `StandardMaterial` onto a 3D-material foundation in three slices:
- `4c60ce8` — `WellCardMaterial` (3D `Material`, samples the RTT texture, Mask
  alpha; `params` uniform `[selected,in_lineage,status,time]` wired for FX).
- `7c64cc2` — card text renders via the app's MSDF pipeline (`text::card_text_glyphs`
  lays out each field with parley, the generic block MSDF pass composites). Crisp
  at any zoom — the focus dolly no longer softens text. vello now draws only decor.
- `38e2992` — `WellCardMaterial` draws the *whole* card on the GPU (accent
  rounded-rect body + selection/lineage rings as SDF in `well_card.wgsl`, driven
  by uniforms). vello no longer touches card textures at all (stays for SVG/ABC
  elsewhere). Cards use `BlockRenderMethod::Msdf`.

### Vortex + odometer nav + status + drift shimmer (landed 2026-06-17)

The big evolution toward the concept art (`viz-substrate.md` §7.7–7.8), verified
live on the GPU runner:
- **Kernel-activity rings (§7.7):** a base ring deck pulses with the live
  kernel-wide `ServerEvent` stream (zero new wire); ripples localize to the busy
  context's angle. Re-tiered so bright = action. `activity.rs` + `well_rings.wgsl`.
- **Vortex (§7.8):** dropped the 3 discrete rings for one continuous spiral, axis
  tipped back (`WELL_TILT`) so the mouth opens toward you, with an accretion-disk
  event horizon at the throat. Odometer nav: spiral index = address (←/→ = ±1,
  ↑/↓ = ±10, digits = mouth decade). Band-positioning code deleted; `band_orders`
  survive for slot order + label clusters.
- **Status coverage (`df3b65b`):** kernel-derived `ContextInfo.liveStatus @14` —
  the server reads each context's block statuses in timeline order
  (`derive_context_live_status`: any Running→Running, else tail Error→Error, else
  idle; non-sticky) and ships it on every `listContexts` poll; the well sets
  `Card.status` from it for every visible card. Retired the event-based
  `apply_block_status` (single source = the poll; breathe is continuous via
  `globals.time`). Thin-client aligned (`feedback_thin_client_smart_kernel`).
- **Drift shimmer (`66ad2e4`):** a card whose context is a staged-drift endpoint
  sweeps an animated HDR diagonal sheen (`card::drift_endpoints` → `params.w`),
  reading the staged queue already on the `DriftState` poll (no wire change). The
  bigger drift arcs/particles *between* cards still need a context→context
  drift-edge list wire (deferred, gap 4 — see issues.md).

### Steps 1–7.6 substrate + consumers (landed through 2026-06-17)

Steps 1–3 (scales / join / compacting layout) shipped pure + TDD in
`crates/kaijutsu-viz/`. Step 4 (card consumer), step 5 (`conclude` wire/lifecycle
— `kj context conclude`/`done`), step 6 (band-0 keyboard nav + band-1 recency
clock keyed on `concluded_at`) shipped (`77146c3`, `ffa7f4e`, `1d3a9ab`). Step 7
(haystack, `7a+7b+7c`) added band-2 semantic angle from `get_clusters` with
kernel-synthesized cluster labels (`ContextCluster.label @2`,
`pick_cluster_label`), same-cluster contexts grouped adjacent, and a fork-ancestry
lineage overlay (`card::ancestors`, amber ring). Step 7.5 added cross-band nav, an
in-world 3D billboarded focus card at the mouth of the well (the flat 2D bar was
tried and rejected — "reads as a JavaScript thing"), eased camera follow, and the
Enter-twice focus state machine (overview → focus → commit). **Next** (in
issues.md): the JOIN dive (camera continues *through* the focus card into the
conversation, mockup 34).

## CRDT-owned config/rc (design: `docs/config-crdt-ownership.md`)

Started as "clear a footgun" (a silent-fallback bug), followed the contributing
factors up to the real structure, and locked a design that **deletes** the whole
silent-fallback cluster instead of patching it: make the CRDT the sole owner of
rc + config; embedded Rust (`assets/defaults/`) seeds it once; no host
flush/reload, so the dual-ownership bug class (stale-bytes read, append file-wipe,
mtime no-op, stale-rc-seed drift) can't exist for these mounts. Memory:
`project_crdt_owned_config`.

### Slice 2 — config TOMLs (shipped 2026-06-17)

Converged onto `ConfigCrdtFs`: the bespoke `ConfigCrdtBackend` (debounced host
flush + watcher + dirty tracker + disk read-back) was **deleted**; a second
`ConfigCrdtFs` mounts at `/etc/config` seeded from embedded. Kernel readers
(models.toml, system.md) route VFS-direct; `kj config show/list/set/reset` is the
editing surface, gated on a new `config-write` authority; the app fetches
`theme.toml` over RPC (`get_config` + `apply_theme_from_rpc`) on connect instead
of reading host disk. `config_dir` revived as a one-time CRDT seed source
(production = embedded; tests inject a mock models.toml). Commits
`93c72a7`/`fdd1c18`/`9e581aa`/`a30b266`/`6f2ce9f` (+ `3d548ca` docs).

### Slice 1 — rc (shipped 2026-06-16)

`ConfigCrdtFs`, the CRDT-native `VfsOps` backend: `UUIDv5(path)→DocKind::Config`
docs, virtual dirs synthesized from descendant paths, the `documents` table *is*
the readdir manifest (`create_document_with_path` + `documents_under_path`), an
in-memory advancing mtime as the single version stamp. `/etc/rc` remounted on it,
seeded from embedded on a fresh kernel only; `kj rc` + `load_rc_scripts` route
VFS-direct (dropped `FileDocumentCache` from the rc path). The host rc tree +
`ensure_rc_seed_files` host write + the legacy `rc_scripts` migration were
removed; CLAUDE.md retired `vim`-the-rc-file. Commits `04ce36e` (fail-loud
`CacheReadError` sweep), `e702ee2` (project-source residual closed), `debfb33`
(foundation), `2b763c6`/`49c819a`/`a2c1045` (seed + cutover). Both slices boot the
real kernel on the new mounts in integration tests; **live-runner verification of
both is still open** (issues.md — needs a real `systemctl --user restart`).

## builtin.file edit/read hardening + hashline (shipped & committed 2026-06-17)

Closed the `docs/issues.md` corruption post-mortem (THE_DIRECTOR `019ed674`).
Commits `899c340` (fix), `435a7e7` (docs/issues), `7217a3e` (hash pin); 1163
kernel tests green + two DeepSeek/kaibo reviews + `/code-review`. Memory:
`project_file_tools_hashline`.

**Disambiguation (a real trap):** this is the *kaijutsu kernel's* `builtin.file`
read/edit — surfaced as `kaijutsu:read` / `kaijutsu:edit`, driven by agents
inside a context — **not** Claude Code's host-side `Read`/`Edit`.

**Root cause the original post-mortem missed:** `edit` fed BYTE offsets
(`match_indices`/`.len()`) into the CHARACTER-indexed CRDT `edit_text` — a silent
splice/over-delete on any file with multibyte UTF-8 before the edit site, while
honestly reporting `Replaced 1 occurrence` (the byte search *did* find a match).
Fixed: byte→char offset conversion, char-count delete lengths, and **fail-loud
post-write verification** (an independently-computed `expected` compared to the
read-back — crash over corruption). Plus **hashline addressing** (per
anthropics/claude-code #25775): `read` prints `LINE:hash→ content`; `edit` gained
an `anchor` mode (`N:hash` / `N:hash..M:hash`) that re-verifies the line hash
before writing (stale → fail loud). CRLF-preserving; unit + e2e broker tests. The
forward design direction for the kaish-side build-out (two read modes; push
`line_hash` up into the kaish crate) is recorded in issues.md.

## External `mcp__kaijutsu__shell` hang root-caused + fixed (2026-06-17)

After a server+app restart, *every* external shell call timed out — `echo hi` at
20s, `kj context list --tree` at 300s — returning a `block_id` but never the
output. **Root cause (not the network — it's localhost): executor starvation on
the MCP client's single-threaded RPC LocalSet, made *permanent* by a too-aggressive
server reap.** Three compounding factors: the MCP subscribed to block events
kernel-wide (firehosed by every other context after a restart); each event woke
the shell poll's `find_terminal` → `blocks_ordered()` re-sort under the lock; and
`from_sync_state` replays the full op-log synchronously on the same thread.
Stacked on one `current_thread` executor a multi-second stall is easy — and the
server's FlowBus bridge broke the subscription *permanently* on the first 5s
callback timeout (`if !success { break }`). Three fixes: (1) the server bridge
tolerates transient stalls (`SubscriberHealth`, reap only after 3 consecutive
failures); (2) the client re-subscribes on a shell-poll timeout
(`resubscribe_blocks`); (3) the MCP's block subscription is now scoped to its
joined context (`block_events_client_and_filter`), cutting foreign-context volume
to zero. Verified live against a busy 24-context kernel: the 300s command returned
in 285ms. Related memory: `project_mcp_synceddocument_sync`.

## Transport ACK review — the no-deadlock property is real but contingent (2026-06-29)

A morning second-opinion pass over the day's rotate/transport fixes (the ACK fix
`f0c3eb90`, the sub-ms-tempo + rotate-cadence-persistence fix `f4af0bba`, the
rc-verb single-source fix `8e2a158c`) — run through codebase-aware kaibo `consult`
rather than the prior no-repo-access batch precedent, so the reviewers read the
live tree themselves. Two independent casts (deepseek-v4-pro, chimera/claude-haiku;
gemini was provider-overloaded all three tries) returned **zero correctness
findings**: RecvError on a dropped oneshot is surfaced loudly, no lock is held
across the `.await`, the BPM ceiling and the additive `rotate_every_phrases`
column round-trip, and `RC_VERBS` is now genuinely single-source.

The one thing worth a devlog beat: the two reviewers *disagreed on the threading
topology*, which is exactly the kind of divergence that hides a latent hazard, so
I traced it to the source. Truth (`beat.rs:1079–1098` + `:810`): the scheduler
owns a dedicated `"beat-scheduler"` thread with its own `current_thread` runtime +
LocalSet, and the rc lifecycle fires `kj transport` via `spawn_local` **onto that
same LocalSet**. So the author's "one LocalSet" mental model was right (deepseek's
read; chimera had described the interactive-RPC path instead). That makes the
self-referential rc path — a rotate/arm script awaiting the scheduler that spawned
it — a *separate task on the same single-threaded executor* as the scheduler loop.
It does not deadlock, but **only because `apply_command` is fully synchronous**:
nothing is awaited between the ingress `recv` and the `reply.send`, so the loop
yields back and services the waiting rc task in the same poll cycle. The day
someone adds an `.await` inside `apply_command`, the rc-fired path self-deadlocks
(the scheduler parks awaiting work the parked rc task can't deliver). The property
held by structure, but nothing *enforced* it — so the fix is a documented
invariant: a "MUST STAY FULLY SYNCHRONOUS" note on `apply_command` plus a pointer
at the `select!` ingress arm where a future edit would be tempted to await
(`8afbf8fe`). The lesson echoes the rotate fork-bomb retraction from the day
before — diagnose threading from the code, not from a reviewer's summary, and when
two competent readers model the topology differently that *is* the signal to go
look.

## Tracks Stage 3 M1 — first sound, all the way through to a synth (2026-06-30)

The milestone the rest of M1 was building toward: kaijutsu *composed a line and it
came out of a synth*, end to end on zorak. Chain, every hop real:

  musician (Haiku, tool-free) → `X:1` ABC turn → `on_turn_completed` →
  `schedule_abc_cell` onto the `synth` track timeline → beat commits the cell →
  `materialize_track` → `emit_to_render_targets` → `AlsaMidiOut` (`kaijutsu 129:0`)
  → `aconnect 129:0 128:0` → TiMidity 128:0 → PipeWire → speakers.

Orchestrated live over MCP: `kj transport render --track synth` opened the seq port
(confirmed in `/proc/asound/seq/clients` + `aplaymidi -l`), `aconnect` routed it to
TiMidity, `kj transport play` + `kj drive synth --prompt "…"` produced the phrase.
Two findings from the live run worth keeping:

- **A cloud model needs a user turn.** The musician's create-time hydrate fired with
  a system-only message and Anthropic rejected it (`invalid request`); the OODA *tick*
  injects a `Transport: …` **user** block, so ticked turns (and `kj drive --prompt`)
  succeed. The chameleon loop was bootstrapped on a local model that tolerates
  system-only — Anthropic is stricter. (Use `--prompt`, or rely on the tick's user msg.)
- **It plays sparse, not continuous — by configuration, not bug.** The musician's
  `wakeup` cadence defaults to `8 * 16` = 128 beats (~once/minute at 120 BPM) and the
  primers ask for "a bar or two," while the phrase window is 16 beats — so you hear a
  short phrase then a long rest. The contiguity *mechanism* is already right (the
  schedule lead is `phrase_delta()`, one phrase ahead); the dial-in is wakeup-=phrase +
  telling the model the phrase-window beats + "fill it." That's the next thread.

This is `midi.md` M1 reaching its real acceptance test — audible output — not just the
unit/loopback tests. `aseqdump` captures read 0 only because the background dump kept
getting reaped when its wrapper shell exited; the ear was the instrument.

## Tracks Stage 3 M1 — sound out the door (clock_kind, render seam, AlsaMidiOut) (2026-06-30)

With WI 1+2 (the `ClockSourceKind` enum + mutable tempo) and the three landed-code
fixes (below) in, the rest of Stage 3 M1 landed in four commits
(`b3cd1761`→`508bc0c4`) and the track grew an actual *sound output*. The arc is
**midi.md M1**: ABC committed on a track → MIDI scheduled into an ALSA seq queue,
system clock, output-first.

**WI 3 — `clock_kind` persistence (`b3cd1761`).** The clock *source* is the
deliberate opposite of the set-once `score_context_id`: a track's identity is
durable, but you sketch on the system clock and later slave the *same* track to a
MIDI master, so `clock_kind` is MUTABLE — it rides the `ON CONFLICT UPDATE SET`,
persisted on every driver swap (gemini won that call in the lock; my original
"set-once" line was wrong). `attach` reconstructs the source from the row, not just
the period — and because M1 can only build `"system"` (`ModeledClock` is
uninhabited until M3), a `"modeled"` row makes `attach` crash loud rather than
silently downgrade the clock. Additive ALTER, so a pre-Stage-3 row backfills to
`"system"`, never NULL.

**WI 5 — the ABC per-event stream (`f5b7cb16`).** `midi.rs` already built a timed
`MidiEvent` list internally and then framed it as an SMF blob; re-parsing that blob
to recover the events would be backwards. So `pub fn events(tune, params)` exposes
the sorted, absolute-tick stream, and `generate` became a thin SMF wrapper over the
shared `build_single_track_writer` — its bytes unchanged (the whole pre-existing
suite guards that).

**WI 4 — the render-target seam (`77f73fa1`).** A render is a *consumer* of the
score, not a producer — so `RenderTarget` hangs on `TrackState` (a small `Vec`), not
as an `AttachedContext`, and is fed from the materialize crossing: each
newly-committed `Concrete` ABC cell's resolved ABC + a near-future instant go to
every target. Two review fixes are baked into the seam. The instant is computed off
a **jitter-free** reference — `last_fire_scheduled`, the heap entry's *scheduled*
fire `Instant` latched in `fire_due`, NOT the `SystemTime` latched after the pop (the
jittery *actual* wakeup) — so per-beat scheduler jitter never accumulates into the
output (deepseek). And because the speculation lead means a device queue holds ~a
phrase of future events, `stop`/`pause` call `flush_scheduled_after(now)` to truncate
the tail rather than let the clock-stop play it out (both casts, SEV-1). The seam is a
zero-CAS-read no-op when a track has no targets — the common case.

**WI 6 — `AlsaMidiOut`, live on zorak (`508bc0c4`).** The one real target: opens an
ALSA seq client + a subscribe-readable port + a started real-time queue. `emit`
resolves the ABC to the WI 5 event stream and schedules each channel-voice message
**relative** to now — `(at − now) + within-phrase-tick × secs-per-tick` — so we never
have to sync the kernel's monotonic clock to the ALSA queue's clock; the per-tick
duration comes from the ABC's own `Q:` tempo. Raw MIDI bytes go through the `alsa`
crate's `MidiEvent` encoder (no bespoke seq-event construction). `flush_scheduled_after`
drops the queue's pending OUTPUT and fires ALL_SOUNDS_OFF/ALL_NOTES_OFF direct on every
channel. The loopback test (a reader port subscribed to the out port, assert four
NoteOns arrive in ascending pitch order, then flush) is `#[ignore]` so CI without ALSA
stays green — and it **ran live on zorak this session: passes**. (The encoder isn't
`Send`, so it's built per-emit; `AlsaMidiOut` itself is `Send` because `alsa::Seq` is.)

The decisive constraint from `docs/midi.md` held all the way through: the scheduler
stays purely generative-local. There is no realtime pulse path — the heap, the
generation/stale-drop logic, and zero-CPU idle are all untouched. M1 ships the system
clock only; the `ClockSource` trait is shaped (estimate/remote-aware `next_fire`,
the M3 heap-re-enlistment hook) so the drift-modeled MIDI-*in* clock slots in at M3
without rework — but no `ClockEstimate` stub ships now (no dead-code theater).

## Tracks Stage 3 — three clock-correctness fixes from the landed-code review (2026-06-30)

Stage 3 generalised the clock source behind a `ClockSourceKind` enum and made tempo
mutable mid-flight (WI 1 + WI 2, `2e3dc6c5`→`4c55a5dd`). We ran the usual two-voice
review on the *shipped* code: deepseek's consult came back clean, and a gemini-pro
**batch** (the resilient path under gemini's interactive 503s) surfaced three real
correctness hazards we'd missed. All three are now fixed TDD — each with a test that
fails against the old code. Full record: `docs/tracks.md` ("Landed-code review of WI 1
+ WI 2").

The headline one is a **silent-fallback data loss**, exactly the class CLAUDE.md tells
us to crash over: a cold restart reverted a saved tempo. `set_tempo` persists
`period_ms` to the `tracks` row *precisely so a restart recovers it* — but `attach`'s
track-create path read the row only for the playhead and the score context, and armed
the live clock from the attaching context's `BeatPolicy` DTO (which defaults to 120
BPM). So the first re-attach after a restart quietly threw the saved tempo away. The
fix builds an `active_policy` from the persisted row when one exists; the DTO is the
seed only for a genuinely fresh track.

The other two are latent traps the mutable-tempo work exposed in `engine.rs`.
`commit_or_squash` validated the basis at `self.playhead` (the commit deadline) while
`speculate` used the cell's `start` — so any resolver reading `ctx.now()` would
mispredict and squash forever; dormant only because `CasCommitResolver` ignores
`now()`. And `pump`/`tick_at` mapped `since_epoch × current_rate`, so a tempo change
reinterpreted *all* of prior elapsed time at the new rate — a 120→240 jump would
rocket the playhead decades ahead and fire every scheduled cell. `BeatScheduler` dodges
it (it drives `advance_to` with event-counted ticks, never `pump`), but it was a public
footgun. We removed `tick_at` and made `pump` integrate phase incrementally, so a rate
change only re-rates time after it.

The pattern across all three: WI 2 made the tempo *dynamic*, and each bug was a place
that had quietly assumed the tempo was constant for all time. Gemini-pro's whole-repo
read caught what a diff-focused pass would have missed.

## Tracks Stage 2 — the score moves onto the track (2026-06-29)

Stage 1 (earlier the same day, `f6478bdf`→`d43fe9aa`) moved the *clock* — playhead,
beat_count, transport, the scheduler heap — from per-context onto a per-track
`TrackState`. Stage 2 moves the other half: the **score** (the `Timeline`'s open
future + committed cell log) off the per-context `Kernel.timelines` map and onto the
track, so the score never leaves when a producer detaches or rotates, and N producers
attached to one track share one open future. Canonical plan + the two-voice review:
`docs/tracks.md` ("Stage 2 implementation").

We designed it as a co-design round, then **stress-tested the locked design with two
independent voices before cutting code** (the Stage-1 habit): DeepSeek (agentic, reads
the repo itself) and a gemini-pro *batch* — and a side-lesson landed there: interactive
gemini-pro 503'd hard under load while the batch sailed through on separate capacity, so
batch is the resilient path for a big gemini-pro review (now an auto-memory). Both voices
endorsed the design and converged on a subtle point — the squash/misprediction path is
dormant for today's only resolver, and that's *correct*: two players landing a note at
one tick should both commit (a chord), not cancel each other. They also caught the one
real new mechanism the "concurrent producers are free" claim needs, and three
implementation gaps; all folded into the tracker before coding.

The headline decision (with Amy): the score's container is a **real per-track "score
context"** (option C) — a normal, app-viewable context (`context_type="score"`, minted
the `lost+found` way: real row + document + drift handle) whose Conversation document
holds the materialized score, but which never takes a turn or hydrates. This reused the
*entire* per-context block machinery via a real `ContextId` — no `TrackId` block-store
API, no index/RPC ripple — and it embodies the doc's own thesis: the track persists, the
players come and go. (Rejected: a synthetic ContextId, which would violate
`handle_implies_row`; and a parallel `track_documents` store, which was the most new code.)

The cut landed in increments so the tree stayed green: (1) a `TrackId`-keyed timeline
registry on the Kernel; (2) the score context — a set-once `score_context_id` on the
`tracks` row (the frequent tempo/playhead upserts mustn't wipe it) minted/recovered in
`attach`; then (3) the breaking re-point — `schedule_abc_cell` + materialize route to the
track timeline + score context, materialize **hoisted once-per-beat** out of the
per-context loop (or N attached contexts would re-emit each cell), the cursor + failure
ledger moved onto `TrackState`, and the shared failure ledger drained **per producer**
(each `FailureEvent` now carries `played_by`, routed back to the producing context's
conversation so a player reads its *own* failures). `KJ_HEARD` finally reads the score
context — the real band view it had only ever claimed to be. The Stage-1 per-context
bridge slew is gone; the track timeline's `advance_to` is the legit clock pump.

The biggest sweat was the test migration: ~16 tests were welded to per-context timelines
and `block_snapshots(ctx)`. The marker/clock tests now pre-arm the *track* timeline with a
commit-margin-0 clock (idempotent, so the later `attach` keeps it) and read the score
context; `second_context` became a genuine two-producers-share-one-score test; and a new
`two_producers_failures_route_to_their_own_conversations` pins the concurrent-producer
mechanism. 37 beat + 1266 kernel tests green; full workspace builds.

Deferred (in `docs/tracks.md` "Still open"): persisting the in-RAM committed `Vec<Cell>`
so the `UseLastGood` vamp-insurance pool survives a restart (the score *blocks* persist;
the cell pool doesn't yet), and Stage 3 (the `ClockSource` trait + a MIDI driver).

## Musician beat-state persistence + manual re-arm (2026-06-28)

A kernel restart silently stopped every musician: auto-arm fires only on context
*create*, so the scheduler's `armed` map came up empty on cold start and there was
no verb to recover — a fail-silent that taxed every iteration on the runner (which
restarts constantly). Amy's steer was to make re-arm **possible but not automatic
yet**: ship the recovery path and the durable state it needs, but hold off on an
automatic boot sweep.

Two halves landed together. (1) **Durable `BeatPolicy`** — a new `beat_state`
table mirrors each musician's live policy (period/beats-per-phrase/ooda) + lane;
the scheduler writes it through on every policy mutation (`arm`, `set_tempo`), so
the row never drifts behind the running `BeatState`. A db-less store (embedded/
test) no-ops; a db-backed *write failure* is loud, never silent — a musician
silently re-arming to the default tempo after a crash is exactly the fallback we
reject. A corrupt row (zero period, empty track) reads back as a loud `Validation`
error, not a silent default. (2) **`kj transport arm [--context]`** reconstructs
the `Arm` from the persisted row (real tempo/cadence restored), falls back to the
musician default + a label-slugged lane for a never-persisted musician, and
refuses loudly on a non-musician. It arms *stopped* + OODA-armed (no surprise
token spend — `kj transport play` starts the clock); the playhead reseeds from the
block log's max tick via `arm()`'s existing virgin-only seed.

This **decoupled** the old "sweep + persistence land together or not at all"
bundle: persistence + a manual recovery shipped without the automatic sweep, which
is now the deferred piece (the `rpc.rs:1270` cold-start recovery loop is the seam,
once it's sequenced after the beat scheduler is wired). Not persisted: `beat_count`
(the OODA phase counter resets on re-arm — the playhead is what carries musical
position). `clear_beat_state` exists but waits on an archive RPC to call it. Tests:
DB round-trip + corruption-loud + clear; scheduler write-through (arm + tempo);
verb (persisted / default-fallback / non-musician-refusal). Memory:
`project_chameleon`, `project_composer_transport`.

## context_type decomposition — the beat moved from Rust to rc (2026-06-28)

Immediately after shipping `kj transport arm`, Amy asked how deep the
`context_type == "musician"` strings ran — and whether the real answer was to
break musician-ness into features a context_type *consumes* rather than a name
the kernel matches. The survey (written up in `docs/chameleon.md`) found the beat
*runtime* was already decomposed — `on_turn_completed` and friends key off
"armed", not the name — and the literal survived only at the create-time arm
gates. Two of those were *duplicated* Rust blocks: `create_context_inner`
(`rpc.rs`) and the `kj context create` builtin (`context.rs`) each carried the
same `if context_type == "musician" { derive lane; send Arm }`.

`kj transport arm` was the missing rc-callable primitive, so we executed the
decomposition: a new `musician/create/S20-arm.kai` runs `kj transport arm`, and
both Rust arm sites deleted — one rc script replaces the duplicated pair, and the
two create-time string checks are gone. The arm verb's own gate dropped
`== "musician"` for "does the label yield a track lane?" — arming is the opt-in
(shared-trust: capabilities are nudges, not security), and a type-changed context
still re-arms from its persisted row. Net effect: a context_type is a beat
participant exactly when its `create/` rc arms it, so `funkMusician` /
`lyricist_in_time_with_music` are now pure rc bundles with no kernel edit — and
the `ContextType(String)` newtype became moot (zero beat-related string checks
left), so we declined it rather than deferring.

Two integration subtleties worth recording. (1) rc `.kai` scripts run
**privileged** (`materialize_context_kaish_rc`), so the rc `kj transport arm`
clears the Transport cap gate under the musician's narrowed loadout. (2)
`test_dispatcher` doesn't call `set_self_arc`, so `kj`-inside-rc falls back to
bare kaish and silently no-ops — which is why the existing musician-create test
passed for the *wrong* reason (it was testing the Rust arm, not the rc). Rewiring
that test to `Arc::new(test_dispatcher()).set_self_arc()` turned it into a real
end-to-end check: create musician → rc runs → `BeatCommand::Arm` fires. One honest
behavior change: arming with no beat scheduler wired now surfaces a LOUD rc Error
block instead of a quiet `log::warn` (embedded/test only; the server always wires
a scheduler). Memory: `project_chameleon`, `project_rc_lifecycle`.

## Rotate action — the page-turn lands (2026-06-28)

With `kj transport arm` now an rc-callable primitive, the long-deferred rotate
ACTION became writable. The scheduler trigger was already built (at a phrase
horizon it stops the parent synchronously and fires the `rotate` lifecycle); what
was missing was the lifecycle being *wired* (`rotate` wasn't in `verb_is_wired`,
so it silently no-op'd) and the rc script itself. Both landed:
`musician/rotate/S10-rotate.kai` = `kj fork --preset spawn --switch && kj transport
arm && kj transport rotate --every $ROTATE_EVERY && kj transport play`. The
`--switch` moves the rc shell onto the freshly-forked child so the bare transport
calls target it — no context-id capture in shell. Chained with `&&` so a failed
fork can't fall through and re-arm the parent the scheduler just stopped.

Two enablers fell out of writing it. (1) A spawn-fork is *labelless*, and a
forked player must keep the parent's **track** anyway (the lane is the durable
identity across a fork-lineage — UseLastGood/$HEARD are per-track). So fork now
copies `beat_state` parent→child (`insert_forked_context`, alongside the
binding/env copy); the child's `kj transport arm` finds the inherited row and
re-arms on the parent's exact lane + policy, not a label slug. (2) The scheduler's
`rotate_every_phrases` is a runtime BeatState field that doesn't travel with the
fork (re-arm starts un-rotating, like stopped/ooda), so the song would turn once
and stop — fixed by seeding `$ROTATE_EVERY` into the transport vars when rotating,
which the rc replays onto the child.

The end-to-end test fires the `rotate` lifecycle and asserts the three child
commands in order (Arm on the parent's track, SetRotate at the same cadence, Play)
plus that the child is a real fork. Honest remaining gap, recorded in issues.md:
**tick continuity** — a thin child has no committed blocks, so its playhead seeds
from `max_tick`=0 and musical time *resets* across the page-turn. The fix is the
chameleon retire-history-and-carry-tick invariant, still unbuilt; until then the
page-turn restarts the timeline. Memory: `project_chameleon`,
`project_rc_lifecycle`.

## Chameleon / musician — first loop reached MIDI (2026-06-13, `da59499`)

The musician (né composer) transport + OODA loop reached its headline: the first
Chameleon loop produced MIDI end to end. Players are tool-free by design (a small
local model handed the full palette stalls the turn); a player's turn text *is*
the score (`on_turn_completed` eager-parses ABC). The remaining work — the
rotate-action rc script, the `--ooda-every` cadence knob, the windowed-notation
pull primitive (slice 5) — is parked in issues.md. Memory:
`project_chameleon`, `project_chameleon_first_loop`, `project_composer_transport`.

## Gemini-CLI cache/cost decisions + `McpHookPhase` rename (2026-06-24)

A design session read the Gemini-CLI feature comparison (`docs/issues.md`) through one
lens: **the Anthropic prompt cache is a prefix match — any byte change in `tools →
system → messages` invalidates everything after it.** That reframes several "cheap
wins" as cache footguns (date/cwd in the system prompt is a silent invalidator; the
fix is *where* it lands, not whether) and several cost levers as cache costs (model
switching is model-scoped, so classifier routing must be fork-grained). The converged
decisions are durable in `issues.md` → *Cache & cost — decided direction*: EMA (not PID)
for the chars→tokens calculator calibrated by provider `usage`; compression dropped
(SQLite-on-btrfs + organic ~80% flush-to-signoff covers it); a per-turn
`BeforeModelTurn`/`AfterModelTurn` seam with a **mechanics(Rust) / policy(data) /
decisions(kaish-hook)** split, so "gemini retries differently" is a policy row, not a
code fork; hook contract = `HookAction` verdict + stdout→block payload (append-only,
so a hook physically can't rewrite the cached prefix).

Landed in code: **`HookPhase` → `McpHookPhase`** (108 refs / 10 files). All five
variants (`PreCall`/`PostCall`/`OnError`/`OnNotification`/`ListTools`) fire around the
MCP broker, so the *enum* was scoped rather than prefixing each variant — and the
model-turn seam becomes a clean sibling. Persistence is decoupled (`phase_to_str` maps
to stable `pre_call`… strings), and the DB is empty anyway, so no migration. Module +
enum docs now state the MCP scope and forward-reference the sibling surface.
`cargo build -p kaijutsu-kernel -p kaijutsu-server` green. Remaining prep (input-limit
table, EMA calculator, `RetryPolicy`, the sibling phase enum + its open reuse-vs-parallel
fork) is logged in issues.md.

## 2026-06-30 — kaijutsu-abc spec-conformance round (kaibo three-model audit)

Ran a holistic ABC v2.1 conformance audit on the `abc` crate using kaibo: two interactive
deepseek consults on the semantic core, plus gemini-pro and claude-opus **batches** each
handed the full verbatim spec cache (`docs/abc-spec-cache.md`) + all 38 src/test files (no
diff — holistic, the way we like it). High cross-model agreement. The spec-in-context paid
off twice over: it let us *reject* a confident "SEVERE cross-octave accidental" finding —
ABC's `%%propagate-accidentals` defaults to all-octaves, so the code was already right.

Fixed 14 real bugs, strict TDD (failing test first), suite 320 → 336 green, zero regressions.
Highlights: `Q:` tempo ignored the beat unit (half-note tempos played at half speed);
multi-measure `Z` ignored the meter denominator (2× in 6/8); tuplets silently dropped inner
rests/chords; `K:` explicit accidentals never reached MIDI (`K:Hp` lost its C#); sharp-minor
keys got flats instead of sharps; chords ignored inner note durations; a tie carrying an
accidental across a bar line left a **hung MIDI note**; and — the big one — first/second
variant endings weren't expanded at all (`|1…:|2…` played both endings once instead of
selecting per pass). The variant-ending fix needed the bar tokenizer normalized first
(`|2` had been mislabeled `FirstEnding`).

Remaining open items (MED/LOW) recorded in `docs/issues.md`; engrave has its own copies of
the tuplet + key-signature bugs to revisit when we turn to rendering. Next: Round 2 — deeper
edge-case / round-trip / malformed-input testing — then rendering.

### 2026-06-30 — abc Round 2: reliability test net (+ a div-by-zero the fuzz caught)

Added `tests/robustness.rs`: the parse→to_midi→to_abc→parse pipeline must never
panic on adversarial input, NoteOns/NoteOffs must balance (no hung notes) across
ties/slurs/chords/tuplets/repeats/endings, the SMF must be well-framed (incl.
multi-voice format-1), and round-trips must preserve the pitch sequence. The
adversarial corpus immediately found a real divide-by-zero: `L:1/0` (and `M:0/0`,
`A/0`) carried a zero denominator into the tick math. Guarded all four tick-math
sites — a parser over untrusted ABC must degrade, not panic. Also ratcheted the
spec-fixture warning baseline 59 → 49 (the `A//` and `|2`/`:|N` fixes removed
unknown-character warnings). Suite now 340 green.

### 2026-06-30 — abc MED/LOW sweep (6 more fixes)

Cleared the remaining medium/low spec items so MIDI + parse are fully aligned
before rendering: `X` invisible multi-measure rest; broken rhythm now transparent
to grace notes and accepts chords on either side (§4.4/4.12/4.17); inline mid-tune
`[M:]`/`[L:]` take effect in MIDI (per-voice unit-ticks/meter made mutable, joining
the `[K:]` handler); `%%MIDI transpose N`; short-form letter decorations H/T/u/v
parse before their note (the old guard rejected every alphabetic follower, so they
never fired — 4 `#[ignore]`d tests un-ignored); and the dead `ast::Note::to_midi_pitch`
helper + its tests were aligned to the live uppercase-C=middle-C convention. Spec
fixture warning rail ratcheted 49 → 44. Suite 345 → 349. Left for later (in
issues.md): grace-note MIDI (needs a timing decision), tuplet default-q in compound
meter (high-churn, rare), and the lyric-alignment items (rendering phase).

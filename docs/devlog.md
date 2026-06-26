# kaijutsu devlog

Narrative history of landed work and the decisions behind it — the "how we got
here" color that doesn't belong in [issues.md](issues.md) (open work) or the
code. This is *not* the authoritative record: `git log` (commits, SHAs) is
canonical and the design docs under `docs/` hold the locked decisions. This is
the story, melted out of the ephemeral session handoffs as they retire.

Newest work first within each area; dates are when the work landed.

---

## SSH transport & RPC surface (design: `docs/sftp.md`)

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

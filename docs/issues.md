# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md) (the landed-work story). See the three-file working-notes pattern in `CLAUDE.md`.

---

## Triage of a real context's 37 "failed tool calls" (2026-08-16)

Amy noticed `kaijutsu-chan` (a `director` seat on deepseek-v4-flash) showing
**37 failed tool calls** and asked whether that was model error or a tool
problem — and whether a `method_missing` response listing available tools
would help. Triaged against the live context. Four findings, in descending
order of how much they matter.

### 1. There were 12 failures, not 37 — the count triples every one

Each failed call paints **three** error-status blocks: the `tool_call`, the
`tool_result`, and the companion `BlockKind::Error`. 12 × 3 = 36, plus one
orphan stream error = the 37 on screen. Whatever counts "failed tool calls"
for the UI should count *calls*, not error-status blocks, or every failure
reads as three.

### 2. Two of the twelve are not failures at all — `;`-chain exit-code bleed

A `;`-separated command chain is judged by the **last** command's exit status,
and a nonzero one prefixes the *entire* accumulated stdout with `Error:`.
Both instances produced exactly the output the caller wanted:

- `... ; grep -n "signoff\|chan" .gitignore ; ls kaijutsu-chan.md` — the `ls`
  found nothing (the file didn't exist yet), so a perfectly good `git status` +
  `wc -l` + `grep` result came back labelled `Error:  M .gitignore`.
- `for f in /etc/rc/musician/*; do echo "FILE: $f"; head -25 "$f"; done` —
  `head` on a directory exits 1, so a listing that correctly enumerated all
  four rc verb directories came back as `Error:`.

That is ~17% of the "failures" being successes wearing an error label, and it
actively teaches a model that a working command didn't work. Worth deciding
what a multi-command chain's status should mean — kaish reports the last
command's exit faithfully, so the question is whether the *tool* should be
flagging `is_error` off it, or off something else (any nonzero? all nonzero?
an explicit `set -e`?).

### 3. `method_missing` would have caught ZERO of them

None of the twelve is an unknown or missing tool. The breakdown:

| cause | n |
|---|---|
| kaish parse error | 9 |
| `;`-chain exit bleed (false failure) | 2 |
| `old_string not found` on a file edit | 1 |

So the immediate motivation doesn't hold up. The idea may still be worth
having on its own merits — and the semantic-search-by-relevance angle is the
interesting half, since `builtin.tool_search` already exists and could back it
— but it should be justified by a case where a model actually reached for a
tool that wasn't there, not by this.

### 4. The parse errors are the model's fault, and the error messages are good

This is the part worth being honest about. kaish's diagnostics are excellent —
line:column, an explanation, and worked examples:

```
1:4 [parse]: an unquoted comma splits this into separate words — kaish
reserves `,` (brace expansion, lists); quote a comma-bearing argument to keep
it one word, e.g. cut -f "1,3", sort -k "2,2n", or echo "a,b"
  | ps -o pid,pcpu,etime,stat,cmd -p 2286142; …
```

The model was told exactly what was wrong, where, and how to fix it — and made
the same *class* of error nine times (`echo ===` word-pasting and unquoted
commas, mostly). The full detail does reach the model: the `tool_result` block
carries all of it. Only the companion `Error` block truncates to the first
line, which is a display concern, not a model-facing one.

**Conclusion: model error, well-diagnosed.** The lever is not better errors —
it is getting these constructs into the kaish primer so they are avoided rather
than diagnosed. The freehand-model trap list (bare `===`, bare `yes`/`no`, bare
`,`, compound-into-pipe, `grep -e`) is uniform enough to be a short primer
paragraph, and every one is a *lexer* error on a construct that is idiomatic
in bash — which is precisely why a model reaches for all of them.

**And most of that lever is already built upstream — it just isn't pulled.**
Amy, 2026-08-16: *"the unquoted comma gets better after kaish upgrades I
think."* Confirmed against `~/src/kaish` `CHANGELOG.md` `[Unreleased]`
(a891cc5), i.e. past the 0.14.1 we have not yet bumped to:

- **BREAKING: comma is significant only inside a `[...]`/`{...}` literal or
  pattern** — `sed -n 1,3p`, `cut -f 1,3`, `sort -k 2,2n`, `echo a,b,c` all
  work unquoted. That deletes an entire class of these failures rather than
  diagnosing it.
- **`kaish-help` gained three Foundations fragments** — *a compound statement
  cannot feed a pipe*, *`[ … ]` is not a command*, and *bare `yes`/`no` are
  lexer errors* — and they now reach `Recipe::agent_onboarding()` and
  `tool_description()`.

The second is the one that matters structurally: `S05-kaish.kai` composes
`kj kaish primer` from the linked `kaish-help` crate at every context create,
with **no static copy in the CRDT to rot**. So a kaish bump delivers those
warnings into every new context's system prompt with zero kaijutsu edits —
exactly the payoff that design was for.

Scoring the five traps against a bump: comma **fixed outright**, `yes`/`no` and
compound-into-pipe **now warned in the primer**, leaving only bare `===`
(word-pasting) and `grep -e` unaddressed. That reframes the kaish 0.14 → 0.15
bump (filed separately below) from a dependency chore into a **tool-call
reliability fix** — it is worth re-weighting against other work on that basis.

### 5. One real bug, unrelated to the rest

```
stream error: Failed after 3 attempts: invalid request: An assistant message
with 'tool_calls' must be followed by tool messages responding to each
'tool_call_id'. (insufficient tool messages following tool_calls message)
```

A hydrated conversation went to the provider with a `tool_call` whose
`tool_result` was missing. Keeping those pairs together is the stated job of
`ConversationMailbox` (the atomicity gate, per `CLAUDE.md`), so this is either
a gap in that gate or a path that bypasses it. Retried 3× and failed 3×, so
it was not transient. Needs its own investigation — file/line unknown.

---

## `kj block read` rejects the block id that `kj block list` prints (2026-08-16)

`kj block list` renders ids in a short form:

```
2d25fb02#60  system/error  [error]  tool error: …
```

Feeding that straight back in fails:

```
$ kj block read 2d25fb02#60
kj block read: malformed id '2d25fb02#60' (expected context_hex_principal_hex_seq)
```

The accepted form is the full `01a00bbc…2d1e2d3_5948b2…fb02_60`, which the
listing never shows — so the only way to read a block you just listed is to
reassemble its id by hand from `kj context info` plus the principal hex. Found
while triaging the entry above; it cost several turns and is very likely a
contributor to models thrashing in the block tools.

**A tool's output should be accepted as that tool family's input.** Either
`block read` accepts the short form (resolving `<principal8>#<seq>` against
the `--context` already supplied, which is unambiguous), or `block list`
prints the full id. The first is better — the short form is what makes the
listing readable.

Amy, on the same thread: *"I wonder if we should hide the block tools from
most kinds of context. y'all seem to really like peeking around in them. or at
least we need to give you better search tools."* Worth noting the lever
already exists — block tools are a binding (`kj binding allow builtin.block`,
granted by the `director`/`coder` rc, withheld from `musician`), so scoping is
a per-context_type rc edit, not new machinery. But the thrashing this entry
documents is an *addressing* failure, not an excess-of-capability one: the
tool could not be used correctly even by someone who wanted it. Fix the
addressing before deciding the tools are the problem.

---

## The file write/edit tools are not gated by the approval ledger (Amy, 2026-08-16)

Amy, after a director context rewrote this file wholesale: *"not bad but I
suppose we need to get the write tool hooked up to the approval ledger soon."*

`builtin.file:write` and `builtin.file:edit` are granted as ordinary capability
tokens — a context either holds them or does not, and once held every write is
unreviewed and unlogged. The approval ledger already exists and is already
migrated into `KernelDb` (`KernelDb::migrate_ledger`, which calls the
`approval_ledger` crate's own `migrate`), so the missing piece is routing the
write tools' call path through it, not building a ledger.

**Today's receipt.** A `context_type=director` context (cast `house`,
deepseek-v4-flash) was asked to compress `docs/chameleon.md` and to *append*
findings to `docs/issues.md`. It compressed both: `issues.md` went 5859 → 1969
lines in a single edit (`@@ -6,3504 +6,30 @@`, 5117 deletions). Nothing was
lost — `HEAD` held the full file and the rewrite was archived first — but
nothing stood between the model and the file either. A ledger entry would have
made the write reviewable *before* it landed rather than forensically
afterwards.

Note what this is **not** an argument for. Per `docs/instrument-design.md`
("Many hands, one trust boundary"), the ledger is not a security boundary
between players and must not become one: every player is inside the trust
boundary and crosstalk is a feature. This is the ergonomic-nudge case — a large
destructive edit is a *footgun*, and the ledger's job is to make it visible and
undoable, exactly as Amy framed `kj cc send`: *"gated to start with… I want to
start with watching it and exercising the ledger while we refine it."* The
ledger is a learning instrument, not a verdict.

Design questions this inherits from the gate work already queued: whether a
model should be able to distinguish "gate unavailable" from "denied" (D-28
collapses both into `McpError::Denied`), and whether approvals are
digest-keyed. Both are already awaiting Amy's ruling — see the gate entries
below.

A cheaper partial that is worth considering on its own: a **size-delta
threshold**. An edit that removes more than N lines or more than X% of a file
is categorically different from an edit that changes a function, and that
distinction needs no ledger at all.

---

## A kaibo-like `kaish_ro` — the read-only twin exists, the scratch space doesn't (Amy, 2026-08-16)

Amy: *"we may also end up offering a kaibo-like `kaish_ro` tool, with the
mutating shell having (more) approvals in it — a kaijutsu variant of `kaish_ro`
might have some scratch space and stuff mapped for text processing, not as
strict as kaibo, but still constrained for fun and safety."*

**Half of this is already built and it is worth knowing which half.** The
read-only twin exists: `builtin.shell_readonly` exposes a `read_only_shell`
tool (`kernel.rs:782-794`), it is what a `toolie` gets, and it pins
`ExternalExec::Deny` (`kj/context_shell.rs:390`) over a structurally read-only
mount backend. So the *tool* is not the gap.

**The gap is that `Deny` means no host subprocess at all** — no `sed`, `awk`,
`sort`, `rg`, `jq`. Only kaish builtins and `kj`. That is *stricter* than
kaibo, which allows a curated read-only host toolset, and it is exactly why
"text processing" doesn't work in the read-only shell today. Amy's "not as
strict as kaibo" reads as being about writes; the honest comparison is that on
the exec axis we are already stricter, and on the write axis we are all-or-
nothing.

What that suggests, concretely: a **third `ExternalExec` variant** — a curated
allow-list of non-mutating binaries plus a per-context writable scratch dir
mapped into the VFS, so `sort | uniq -c > /scratch/counts` works without
opening the tree. Two constraints on how it gets built:

- **It must be a variant of the existing enum, not a new exec site.** CLAUDE.md
  is explicit: host exec has one owner, and `ExternalExec::Allow{path}|Deny`
  (`runtime/embedded_kaish.rs:78-86`, set in `kj/context_shell.rs`) is the one
  place exec authority, ignore config, output limits, and VFS cwd resolution
  live. A second policy path re-derives what kaish already owns and drifts.
- **A binary allow-list is a nudge, not a boundary.** `find -exec`, `awk`'s
  `system()`, `sed -i`, and `sort -o` all mutate; a curated list is
  mistake-prevention (footguns absent by construction), which is precisely the
  capability doctrine in `docs/instrument-design.md`. Do not let it be
  described as a security control — every player is inside the trust boundary
  already.

**Why this pairs with the approval-ledger entry above, and is arguably
prerequisite to it: approval fatigue kills a gate.** If every `ls` and `grep`
needs a ledger entry, the ledger becomes noise and gets clicked through, which
is worse than no ledger — the record exists and means nothing. Splitting the
surface is what keeps the mutating shell's approvals rare enough to actually be
read. So the ordering is: widen the read-only shell until it is genuinely the
comfortable default, *then* tighten approvals on the mutating one.

Open: whether the scratch dir is per-context or per-session, and whether it is
a real host tmpdir mapped in or a VFS-native surface (the latter keeps the
"one owner" property but means host binaries can't see it, which defeats the
purpose — probably a real dir, VFS-mounted).

### RULED by Amy, 2026-08-16

**Keep the pair, and make it explicit.** Amy: *"I think we have the pair, one
that's clearly a text processing space that can't corrupt the system, and
another that's hot and can edit and rm and stuff… I thought about suggesting
`shell` be routed transparently but I think that would be even more dangerous
in the end, so explicit is better."*

Transparent routing (one `shell` that silently escalates when a command
mutates) is rejected. Beyond the danger: it makes the capability
**un-auditable**. With two tools, "which contexts can mutate the host" is
answerable from the loadout; with routing, every context holds the hot
capability latently and you only find out at runtime — and the approval
prompt then arrives mid-pipeline, which is the worst possible moment to ask
a human anything.

**Names: `shell` (safe) and `shell_write` (hot).**

```
shell         Run commands. Reads anywhere you can read;
              writes confined to your scratch space.
shell_write   Same shell, plus modify and remove files
              outside scratch. Granted, not default.
```

The deciding argument is **which one gets reached for by accident.** Models
reach for the unmarked, shortest, most obvious name. Today that is `shell`
and today `shell` is the hot one, so every casual `ls` routes through the
dangerous tool — which is exactly what drowns the approval ledger (a gate
that fires constantly gets clicked through, and then the record exists and
means nothing). So the safe tool takes the unmarked name.

This is a semantic flag day on `shell`, accepted because **it fails in the
right direction**: an old caller saying `shell` is denied a write rather than
silently granted one. `read_only_shell` also goes away as a name — it is a
negative framing that reads as "the lesser tool", and it will become
inaccurate the moment scratch writes land.

**`sandbox` is ruled out as a name** in any position. It claims a security
boundary, and `docs/instrument-design.md` is explicit that capabilities are
ergonomic nudges inside one trust boundary, never enforcement between
players. The name would lie about the trust model, and someone would
eventually rely on the lie.

**Default grants stay per-rc, as today** — each `context_type`'s rc decides.
Not an unconditional grant in every bucket.

**Sequencing: do this with the kaish 0.14 bump, not after it.** The bump
deletes the entire latch/nonce confirmation surface (6 call sites, 5 files —
measured, see the 0.14 entry), which is the same mechanism write approvals
would otherwise extend. Amy: *"maybe we do the `kaish_ro` thing at the same
time (and end up with one unified read-only-ish shell with path boundaries
for all agents)."* Rebuilding confirmation on the new API and defining the
read/write split in one pass means designing the boundary once instead of
porting the old shape and immediately reshaping it.

---

## `kj rc render <context_type>` — let one context type assimilate another (Amy, 2026-08-16)

Amy: *"some way to quickly assimilate another context type… one we have done a
few times now is start with orchestrator but load musician expertise by reading
its RC code so we can know how to talk to a musician."*

The maneuver already works and costs no kernel code — `kj rc list` filtered by
type, then `kj rc show` on each script, about eight calls. It has been done by
hand several times. The verb is worth having for a reason other than call
count.

**Render, never run.** The `.kai` half has real side effects — `kj binding
allow`, `kj block create`, `kj cache add`, and `musician/create/S20-arm.kai`
does a `transport attach`. A "dry run" that executes either mutates the caller
or needs a throwaway context, and the throwaway costs a document, a `contexts`
row, and a pile of rc `Trace` blocks to clean up afterwards. Quoting the source
is honest and, for the script that matters most, plenty readable:
`S10-binding.kai` is literally a list of `kj binding allow` lines.

**Reframe to third person — this is the whole value-add.** rc stance is
second-person imperative: `musician/create/S00-stance.md` opens *"You're a
musician here, playing on an internal beat."* A director that concatenates that
into its own context has been handed **instructions, not information**, which
is how an orchestrator starts playing bass instead of conducting. The render
must emit *"a musician is told…", "a musician can…", "a musician is driven
by…"*. That transformation is precisely what `cat` cannot do and a verb can.

**Three parts, and the stance is only one of them:**

| part | source | what it tells the orchestrator |
|---|---|---|
| stance | the `.md` files | how it thinks |
| allow-set | `S10-binding.kai` | what you can actually *ask it for* |
| verb set | `ls /etc/rc/<type>/` | the interaction protocol |

The verb set is free and probably the most useful row. `musician` has
`create / fork / rotate / tick`; `director` has `create / fork / drift`. So: a
musician is driven by `tick` and page-turns on `rotate`; a director is reached
by `drift`. That *is* the answer to "how do I talk to one," and it is a
directory listing.

**Prefer `kj rc render` over a `kj context assimilate`** that lands the briefing
as a `Role::System` block on the caller. Keep the landing separate and let the
model choose (`kj rc render musician | kj block create --role system`):

- auto-injecting into the system prompt fights the `--target=system` cache
  breakpoint that `S20-cache.kai` sets, so every assimilation silently costs a
  cache write
- there is no undo for a system block short of `block exclude` + fork, and
  "I want to know how musicians work" should not be a one-way door

Admin-shaped and occasional, so it is a `kj` verb and earns no wire method —
the rule in `CLAUDE.md` ("kj is good enough for all admin-like stuff").

Related, already shipped: type composition itself exists via rc symlinks —
`bassist` is 100% symlinks into `musician`, all seven scripts. What is missing
is composing knowledge *across* types at runtime, which is what this is.

---

## Did the modeled clock ever phase-lock to a real MIDI master? (2026-08-16)

`kj transport list` shows an `ear` track with `clock=modeled` at **338 BPM**,
playhead 399800, dormant, one attachment, and its score context is not live.
338 BPM is not a tempo anyone dials in, which leaves two readings and they have
opposite consequences:

- a previous session fed the M3 edge estimator **real** clock-in references and
  it converged on a wrong-but-derived number — in which case the wire worked
  end-to-end at least once and the estimator has a bug worth finding
- the value is synthetic (a test fixture, a default, a free-running phasor that
  drifted) — in which case M3 has never seen a real master and "modeled clock
  works" is unproven

Find out which before a live jam trusts the modeled clock. The estimator is
described in `docs/midi.md` M3; `kj transport clock <system|modeled>` is the
switch.

(Found by the `kaijutsu-chan` director context during a pre-jam sweep. Its two
other observations were dropped on review: the "kaijutsu-server burns 92% CPU"
finding is a **false positive** — `kj` executes in-process, so the server's
cumulative CPU is every session's shell commands all day, and a clean 10-second
idle sample measures 0%; the zorak audio-stack-down note is host ops state, not
repo backlog.)

## Hi-res wheel (v120) blocked at winit/sctk — slow drags are a compositor dead zone (2026-08-16)

Amy's MX Master free-spin wheel: slow drags produce NOTHING until ~a full
detent accumulates, then a 40px jump. Diagnosed end-to-end; the app pipeline
is exonerated (a synthetic BRP Line event moves the view on the first notch,
and the live wheel log shows only whole-integer `Line` events, never
fractions, never `Pixel`). Chain: MX Master emits sub-detent v120 → KWin
(Plasma Wayland) accumulates → our client bound wl_pointer below v8 because
**sctk 0.19.2 has no `AxisValue120` handler at all** (verified in the cargo
cache: `seat/pointer/mod.rs` handles `AxisDiscrete` only) → KWin's
backward-compat path only releases whole detents. winit 0.30.13 (Bevy
0.19's pin) then prefers `discrete` → integer `LineDelta`. Nothing app-side
can recover events never delivered.

Upstream status (checked 2026-08-16): sctk **master** handles AxisValue120
(exposes `value120: i32` on AxisScroll), but winit has **zero** value120
references even on master (GitHub code search), and winit 0.30.13 pins
`smithay-client-toolkit = "0.19.2"` exactly. So the block is winit, not
sctk. Compositor-agnostic: any compositor must quantize for a ≤v7 client —
**switching KWin→Mutter would not fix this**. (Browsers had the same bug
and fixed it client-side: Firefox bugzilla 1831893/1836886.) Two paths:
(a) wait for Bevy's winit to bump onto a value120-consuming stack; (b) a
small carried patch — fork sctk 0.19.2 (add the AxisValue120 arm + v8+
bind) and winit 0.30.13 (prefer `value120/120.0` as fractional LineDelta
when nonzero) via `[patch.crates-io]`; one file each, removable when
upstream lands. Given the catch-up loop is "the most important interaction
in the whole app", (b) is a plausible day-lane.

**Experiment log (2026-08-16 evening), lane PARKED by Amy's call** ("fix
kaijutsu-app for what already works for other apps right now; experiment
later with the HID++ device"):
- Carried forks BUILT and PROTOCOL-VERIFIED live: sctk branch
  `tobert/axis-value120-0.19` (value120 field/arm/merge, seat bind 1..=8,
  bind receipt log) + winit branch `tobert/wayland-axis-value120-0.30`
  (prefer value120/120 as fractional LineDelta), both in
  `~/src/research/{client-toolkit,winit}`. App logged
  `bound wl_seat@11 at version 8` — every layer above the kernel driver
  confirmed working. `[patch.crates-io]` since removed from Cargo.toml
  (path deps must not be committed); re-wire via tobert/* GitHub forks +
  git refs when the lane resumes.
- Root probe: kernel `REL_WHEEL_HI_RES` emits ONLY ±120 — the mouse (MX
  Master 4, WPID B042, Bolt receiver 046d:c548) never had hi-res mode
  enabled. `modinfo hid_logitech_dj` lacks c548; the Bolt receiver runs
  on hid-generic, so hidpp never manages the mouse.
- solaar `hires-smooth-resolution true` tried: generic driver mismaps —
  each sub-detent counted as a WHOLE detent (y=-17 events, ~8-17x speed),
  broke wezterm/CC, event storm × ~26ms frames pegged a core. REVERTED.
- Next when resumed: `~/src/research/bolt-dj-bind-test.sh` (root) —
  runtime `new_id` rebind test of the one-line kernel fix (add c548 to
  hid-logitech-dj); if dj claims it, hidpp manages the mouse and hi-res
  works properly (fractions for v8 clients, whole detents preserved for
  legacy). Probe scripts + tmp.log parked in `~/src/research/`.
  Possibly upstreamable to the kernel — MX Master 4 is new; others will
  hit this.

Meanwhile: pipeline stays fraction-ready; do NOT re-add smoothing hacks to
fake sub-detent motion. Wheel trace at `debug!` in `input/dispatch.rs`.

## Scroll feel polish notes (2026-08-16, post slice-0)

Current state Amy accepted ("seems ok rn"): whole-detent input, line_gain
60 (3 lines/notch), smooth_speed 83.18, unfocused-mode Continuous gate.
Polish backlog:
- `smooth_speed` never re-tuned after the detent-size change — live-tune
  over BRP (`ScrollConfig`) in a sitting; crisper (100-130) may suit
  3-line steps better than the carried-over 83.18.
- `pixel_gain` 3.0 is 3x finger speed; browsers do 1:1. Retune to ~1.0
  the day a Pixel-unit device (touchpad/touchscreen — moltar's monitor
  has touch, unplugged) is actually in hand. Touch will also need fling
  physics — the momentum do-not-build fence gets revisited then, not
  before.
- Unfocused baseline is still 2Hz `reactive_low_power(500ms)` when
  nothing is active; fine for power, but if unfocused reading feels
  laggy on first wheel touch, consider reactive(100ms) unfocused while
  the conversation screen is showing.
- Flick event bursts each pay a full ~26ms frame on a 205k-px document
  (heats a core) — that cost is the surface renderer's to kill
  (docs/conversation-surface.md), not a scroll-tuning knob.
- App window came back 960x600@scale1 after restarts (was ~1920@2x that
  morning) — window geometry restore may be broken or KWin-side; check
  whether kaijutsu should remember size/position itself.

## Catch-up seam: mark and jump to the read/unread boundary (2026-08-16)

Amy's core loop: flick up, visually hunt for "something I've seen before",
slow-scroll to the seam, click down while reading. The hunt is the app
failing to serve a boundary it can know: where the reader last left the
tail. Proposal: record the seam (app-local first; the kernel roster/context
could carry per-principal read state later), render a subtle horizontal
rule at it, and bind a jump-to-seam chord so catch-up starts with one press
instead of scroll dexterity. Complements (does not replace) scroll-feel
work; pairs naturally with sticky follow, which already knows the moment
the user leaves the tail.

## Error stub polish: dedupe summary-vs-detail, cap wrapped height (2026-08-16)

First light of the collapsed error stub (`view/format.rs` error arm) showed
two refinements: (a) stream errors carry a `detail` that starts with the same
text as the summary (`block.content`), so the stub renders the message twice —
skip leading detail lines identical(-ish) to the summary; (b) the stub cap
counts *source* lines, so one long line still wraps to ~5 screen lines — add
a char budget alongside the line budget. Shot:
`~/archive/kaijutsu-shots/2026-08-16-error-stub-first-light.png`.

## vi input editor stopped repainting after a small in-place edit (found 2026-08-16, live on moltar)

Amy was editing a typo ("rost" → "rest") in the compose-block vi input.
Positioning over the `o` and doing `i e <Esc>` (insert `e`, then leave insert
mode) changed the buffer but the on-screen render did not update — the stale
text stayed visible. `dw` to delete the whole word, followed by retyping it
fresh, rendered correctly. Not yet root-caused; likely a missed redraw/dirty
flag on a single-char insert-then-escape path rather than a buffer-state bug,
given `dw` + retype (a full replace) recovered cleanly. Vi input handling
lives in `crates/kaijutsu-app/src/input/vim/mod.rs`; worth checking whatever
marks the input view dirty for repaint against the insert-mode commit path.

---

## The roster index has no kernel-now reference, so client-rendered ages mix two clocks (2026-08-16)

`/run/roster/index` gives each row a `recorded_at` on the **kernel's** wall
clock (`kaijutsu-kernel/src/roster.rs` — deliberately the kernel's own stamp,
per `docs/midi.md` "The one timebase": never trust a source's clock). The
document then says nothing about when the kernel thinks *now* is, so a client
rendering "◐ 4m" (`kaijutsu-app/src/ui/quick_context.rs::row_line`) subtracts
the kernel's stamp from its own `now_millis()`. The one-timebase discipline
stops at the kernel boundary.

Accepted for now: kaijutsu's machines share an NTP-disciplined LAN, so the
skew is far below the display's resolution (coarse from a minute up, negative
deltas clamp to "now"). It becomes wrong the moment a client is somewhere
with a clock nobody is disciplining.

Fix, two candidates:
- a kernel-now value in the index itself — a second header line, or a column
  — so the client can compute `kernel_now - recorded_at` entirely in kernel
  time and never involve its own clock;
- `FileAttr::mtime` on the index, once a client can read attrs at all — which
  needs the wire work in the `FileAttr`/`generation` entry below, so the two
  are worth doing together.

---

## The wire `FileAttr` carries no `generation`, so clients cannot do a conditional VFS fetch (2026-08-16)

The kernel stamps `FileAttr::generation` precisely so a caching reader can
skip a re-read (`crates/kaijutsu-kernel/src/vfs/types.rs`: *"Coherence
decisions use `generation`, not mtime"*), and the roster VFS backend sets it
on every `getattr` (`vfs/backends/roster.rs`). None of that reaches a client:
the capnp `FileAttr` (`kaijutsu.capnp`, next free ordinal 6) has size/kind/
perm/mtime/nlink and no generation field, and `Vfs.snapshot` reports
generation `0` for any non-directory (`MountTable::snapshot_node`), so the
one other path that *does* carry a generation cannot carry a file's.

Consequence: `connection::roster` (the app's roster poll) reads the whole
`/run/roster/index` every 5s and diffs the bytes, because a
getattr-then-maybe-read poll would be two round trips that can never skip the
second. Fine at this size; wrong shape for the next VFS-backed feed that is
not a handful of lines.

Fix: append `generation @6 :UInt64;` to `FileAttr` (dense append, backward
compatible), set it in `set_file_attr` (`kaijutsu-server/src/rpc.rs`), and add
a thin `RpcClient::vfs_getattr` + `ActorHandle` passthrough. Then the roster
poll becomes getattr-gated and `RosterFeed::revision` can carry the kernel's
generation instead of a local content counter. Deliberately not done inside an
app-scoped UI slice — it is a wire-schema change and wants its own review.

---

## Four write-once block fields don't survive oplog replay before the next compaction (found 2026-08-16, DTE removal)

`set_stderr`, `set_signature`, `set_tool_use_id`, and `set_output`
(`crates/kaijutsu-kernel/src/block_store.rs`) mutate `BlockContent` fields
that live outside `BlockHeader` — `merge_header` never touches them. Their
journaled `SyncPayload` (built via `SyncPayload::from_updated_header`)
therefore carries the block's header, unchanged, and nothing that would let
`merge_ops` recover the new value. The value survives fine through the next
`compact_document` (a full `BlockSnapshot` covers every field) — the exposure
is only the window between the mutation and the next compaction: a kernel
restart inside that window replays the oplog and rebuilds the block without
the stderr/signature/tool_use_id/output change.

This is not new — it predates DTE removal. The pre-migration `ops_since`
(diamond-types-extended-backed) had exactly the same gap: none of these four
fields were DTE ops or `BlockHeader` fields, so nothing in the old payload
carried them either. Found while auditing every `frontier_before`/`ops_since`
call site during the DTE removal, not introduced by it.

`move_block`'s `order_key` has the same shape of gap (`order_key` lives on
`BlockSnapshot`, not `BlockHeader`) and is likewise pre-existing.

Fix, if worth it: give `SyncPayload` a slot for these snapshot-only fields
(or journal a full snapshot instead of a bare header for these five
mutations), then replay it in `merge_ops`. Not done here — out of scope for
a DTE-removal migration whose job is text, not fixing an unrelated
pre-existing gap in metadata replay fidelity.

---

## The dead-letter queue does not survive a restart (found 2026-08-16)

`DriftRouter.dead_letter` is `Vec<StagedDrift>` held in memory
(`crates/kaijutsu-kernel/src/drift.rs:165`) and there is **no table for it** —
`grep dead_letter crates/kaijutsu-kernel/src/kernel_db.rs` returns nothing. A
kernel restart discards every dead letter.

**The mechanism whose whole job is "content is never silently discarded"
silently discards content.** Its own doc comment at `:162-165` states that
guarantee. Worse, the API invites you to come back later — `dead_letters()`
(`:626`) is explicitly non-consuming "for clients that pair with
`replay_dead_letter`" (`:635`), and a restart is exactly the event after which
someone would go looking.

**The fix is probably not a table.** Kaijutsu already has durable queuing and
the answer was blocks: `ConversationMailbox` stores nothing at all
(`llm/mailbox.rs:37-42` is a `HydrationState` plus a `seen: HashSet<BlockId>`),
because the block log IS the queue and the mailbox is only a cursor over it.
Landing dead letters as blocks in the lost+found context — which
`lost_found_id`/`adopt_lost_found` (`drift.rs:425-448`) already exist to
provide — makes them durable, inspectable through ordinary block tooling,
replayable, and visible in the app, with no new storage and no new format.

Sized as a bug rather than a design: the lost+found context is already the
documented destination for drained dead letters, so this is moving the drain
earlier, not inventing a destination.

## Flaky: ACP's tracing-capture test fails about 1 run in 9 (2026-08-16)

`kaijutsu-acp`'s `update::tests::shrink_and_divergence_log_a_loud_warning_not_
silence` (`crates/kaijutsu-acp/src/update.rs:1088`) failed once during a
`cargo test --workspace` run, then passed 3 isolated runs, 5 full `-p
kaijutsu-acp --lib` runs, and a second full workspace run. **Not a regression
from the DTE removal** — the ACP crate no longer links `kaijutsu-crdt` at all.

**What the failure looked like:** the test asserts two warnings land in a
captured buffer — one naming a shrink, one naming a divergence. The buffer
held **only the first**. The divergence branch cannot have been skipped by
logic: `prefix_hash` uses `DefaultHasher::new()`, which is fixed-seed, so
`prefix_hash("Xabcdef", 6) != mark_for("abcdef").hash` is deterministic. The
warning fired and was not captured.

**Mechanism unknown, and stated as unknown rather than guessed.** The test
installs a thread-local subscriber via `tracing::subscriber::with_default` with
a shared `CaptureWriter`, and both `observe` calls run on that thread, so the
obvious explanations do not hold up. Suspicion is an interaction with parallel
test execution under load — it only appeared in a full-workspace run, which is
the most contended case — but that is a hypothesis, not a diagnosis.

**Why it matters more than one flaky test:** a suite that fails once in nine
runs for an unexplained reason trains everyone to re-run and move on, which is
exactly how a real failure gets waved through. Worth fixing by making the
capture deterministic (a subscriber the test fully owns, or asserting on a
returned value instead of on log output) rather than by adding a retry.


## A context's version is unobservable from `kj` (2026-08-15)

The context version is now load-bearing: it is the client's hydration anchor
(docs/change-feed.md rules 21-26), it survives restarts as of `e0bb2076`, and
Amy wants it as the coordinate a repair replay is addressed by. There is no way
to read it from the operator surface.

- `kj context` has no `inspect`/`show` verb at all (only the tip "a similar
  subcommand exists: 'unset'").
- `kj block history <id>` reports version info but demands a full
  `context_principal_seq` id, which `kj block list` does not print — it prints
  the short display form (`45d6b370#6`), and `--data` returned empty.

Found trying to verify on the live kernel, after the flag-day restart, that a
long-lived context resumed its real version rather than restarting near zero.
The fix landed with unit tests and **has still not been checked against
production data**, because there is no way to ask.

Wanted: a `kj context inspect <ref>` (version, block count, seq range, live
status) — the `getContextVersion` RPC already exists and is what it would read.
It also gives the "did the version resume?" check a one-line answer after any
restart.

## `kaijutsu-mcp --connect serve` silently runs local (2026-08-15)

`kaijutsu-mcp --connect` attaches to the server. `kaijutsu-mcp --connect serve`
— the same flag with the subcommand spelled out — **silently ignores it and runs
in local mode**. `whoami` then answers `{"mode":"local"}` and `register_session`
replies "requires --connect to kaijutsu-server", while the caller believes it
passed exactly that.

Found while probing the live kernel after the wire flag day. Costs a few minutes
of confusion each time, and it is a silent fallback of the kind we treat as a
defect: it should either honour the flag under `serve` or refuse the
combination, not quietly do the other thing.

## The config git worktree has no index — `git status` will lie to an operator

Lane B's seam (`crates/kaijutsu-configgit`) writes commits straight from the
worktree: it walks the live files, builds the tree, commits. It never touches a
`.git/index`, because the aligned gitoxide plumbing pin set has no stage-all
helper and staging through an index would be a second copy of the truth.

Consequence, and it matters because Lane B's stated point is that the directory
is **an operator-visible recovery surface**: someone who cds into
`<data_dir>/config` and runs real `git status` sees **everything as untracked**,
while the history is complete and correct. `git log`, `git show` and
`git checkout` all work; only the index-derived views are wrong.

Decide before the kernel wiring slice, not after someone is confused at 2am:

- write an index alongside each commit (more gitoxide plumbing we then own), or
- leave it and **say so in the directory** — a README committed at init
  explaining that this worktree is kernel-written, that `git status` is
  meaningless here, and which commands do work.

The second is cheaper and honest, and it fits the "no watcher, no implicit
import" ruling: an operator is a reader here, not a committer. Related: ruling 3
wants unexpected dirtiness detected and failed loud, which is still doable
without an index (compare the worktree walk against the HEAD tree), just
hand-rolled.

Also from the same slice: `init_or_open` hand-writes `HEAD` and a minimal
`config`, because the pin set has no plumbing `init` (that lives in
`gix-repository`, deliberately outside the aligned set). Two `fs::write` calls,
but it is one more piece of git's on-disk format this crate now owns and must
keep correct by hand.

## Lane B storage half is unbuilt — config documents are still CRDT documents, not files (2026-08-16)

`kaijutsu-configgit` (the git write seam above) is tested and unwired — see
`docs/config-crdt-ownership.md`, "Lane B — the git-worktree seam". `/etc/config`,
`/etc/client`, and `/etc/midi` are still `ConfigCrdtFs` mounts backed by
`kernel.db`; nothing reads or writes `<data_dir>/config`. What shipped
2026-08-15/16 (`988122f9`) was only the write *gate* — the file tools can now
reach the CRDT-backed mounts directly — not a storage migration.

Two shapes are live candidates, not decided: wire `kaijutsu-configgit` in as
designed (one git worktree, auto-commit per mutation, service-authored
commits — the rulings in `docs/config-crdt-ownership.md`), or go simpler per
Amy's 2026-08-15 lean — plain files on disk, keep the reset-to-embedded-
default tool, and demote git to a skill invoked through rc or the help
system rather than kernel machinery. Whoever picks this up should settle
that question first; building the kernel-wiring slice for the git-worktree
shape before it is confirmed as the plan would be work a later decision
could throw away.

## `kaijutsu-crdt` is a block store now, not a CRDT (2026-08-16)

diamond-types-extended left the crate, and the build graph, on 2026-08-16
(`fc616aa6`, `133b5814`): block text is a plain `String`, and concurrent
merge into a kernel document is structurally impossible (the sole-sequencer
ruling; `pushOps`'s deletion removed `merge_ops`'s only concurrent caller).
What is left in `kaijutsu-crdt` is a Lamport-clocked, fractional-index,
DAG-validating block store with document/snapshot/oplog persistence — a
real thing, just not a conflict-free replicated data type.

The name now promises merge semantics the crate does not have and will not
need again. Only `kaijutsu-kernel` and `kaijutsu-server` still depend on it
(client, acp, mcp, and app all dropped it during the melt). Renaming the
crate — `kaijutsu-blockstore` is the obvious candidate — is open work: pure
churn with no behavior change across two dependents, which is exactly why
it has stayed a name change rather than a priority.

## The well's activity glow wants a derived signal (2026-08-15)

The glow is **disabled**, not broken: `RingActivity`'s decay/ripple math is live
and unit-tested, and nothing calls `record` (see the module doc in
`kaijutsu-app/src/view/time_well/activity.rs`).

What it did before was count kernel events as a pulse — token streaming loudest,
because it means a model is writing right now. True, and it cost the entire
kernel-wide event stream to learn: the app received every token of every context
to choose a brightness. It was also the last consumer of the raw CRDT wire, which
is how it surfaced.

Amy: *"we'll be doing embeddings for a lot of that content kernel side and maybe
we can emit something more useful and derived."* So the replacement is not the
same signal on a new pipe. Sketch of the shape, not a decision:

- a kernel-wide, low-rate event carrying `(contextId, weight)` at minimum —
  a **hint**, so it rides the directive path beside `onRenderCue`/`onBeatSync`
  rather than the change feed; a dropped pulse costs a dimmer glow, never a
  wrong document, and it must never be batched (docs/midi.md's trade);
- weight derived from what a context is *about* rather than how chatty it is —
  embeddings make "these two contexts are working on the same thing" expressible,
  and the ripple machinery is angle-based, so anything yielding `(context,
  weight)` drops straight in;
- the per-context decay ceiling (`CONTEXT_MAX`) and `RIPPLE_LIFETIME` are already
  Amy-tunable constants; a derived signal should keep them meaningful.

To re-enable: feed `RingActivity::record` and re-register an ingest system in
`time_well/mod.rs`.

## `connection/drift.rs` still reads block events off the kernel-wide stream

Drift-arrival notifications detect `ServerEvent::BlockInserted` with
`kind == Drift`. That is a real feature, not decoration, so it kept its source
through the flag day — the per-block *semantic* events are not CRDT-carrying and
were not part of that deletion.

Moving it onto the change feed has a genuine question in it rather than being
mechanical: the feed is per-context, so only contexts the app follows would
notify. Today a drift into any context pops a notification. Decide whether that
scope change is wanted before doing the move — it may be an improvement (drift
into something you are not watching is arguably not urgent), but it is a
behavior change, not a port.

Same shape, second site (found 2026-08-16, app remnant sweep):
`view/time_well/live.rs` `ingest_live_events` (~434-478) builds `ContextTails`
— per-context activity tails for the whole ring, including contexts nobody
follows — off the kernel-wide `BlockInserted`/`BlockStatusChanged` stream. The
same per-context-feed-can't-serve-unfollowed-contexts question applies, and
the two sites should get one answer, not two ports. (Deliberately NOT in this
bucket: `update_event_pulse`, switchboard, editor, fsn/heat, room/activity —
those consume `TurnEvents`/`EditorEvents`/`VfsActivityEvents`/directive-only
members the change feed deliberately excludes; see docs/change-feed.md.)

## Two findings from the change-feed step-1 review (2026-08-15, kaibo/DeepSeek)

Both pre-existing; the change feed is what makes them matter. A third — the
context version resetting on restart — was **fixed** the same day: Amy ruled
*"persist & restore the version, it'll come in handy when we need repair
replays"*.

### 1. `BlockContent::append_text` materializes the whole block per token

The streaming path's O(n²) is **not** gone. `append_text` computes
`self.text().chars().count()` on every call (`kaijutsu-crdt/src/content.rs`), so
each streamed token materializes the entire block. `08793e71` removed a
*different* one (per-op re-materialization while journaling), and the kernel's
classification deliberately avoids adding a *third* — but the per-token cost
remains, inside the CRDT layer. The fix belongs there: get the DTE text length
without building a `String`. **Resolved by the representation change** (2026-08-16): block text is a
plain `String`, so a character count no longer requires materializing one.

### 2. kaish VFS write passes a byte length as a character count

`kaish_backend.rs`'s write path computes `current_len` as `b.content.len()`
(bytes) and passes it to `edit_text` as the `delete` **character** count. On a
block containing any non-ASCII text this fails with `PositionOutOfBounds` rather
than corrupting — loud, which is why nobody has hit it quietly — but it means
writing to a multibyte block through `/docs/<ctx>/<block>` simply does not work.
The sibling `patch` path already has the byte→char projection
(`wire_byte_to_char`); `write` never got it.

## Model names via hooks — the plumbing exists, the data mostly does not arrive (2026-08-15, Amy)

Amy, seeing `cc-.crush-c776babb  now  (no model)` in `kj context list`:
*"getting model names in via hooks should go on the todo, seems tricky."*

**Do not start by building the plumbing — it is already there.** `HookEvent` has
`model: Option<String>` (`crates/kaijutsu-mcp/src/hook_types.rs:30`), the adapter
parses it (`hook_adapter.rs:35`), and `hook_listener.rs:440-457` already sets the
context's model on `SessionStart` when the field is present. Codex sends it —
see the `model: "gpt-5-codex"` in
`crates/kaijutsu-mcp/tests/fixtures/codex/session_start.json`, the only fixture
that carries one.

**The real gap is the source data**, and it is three different problems wearing
one hat:

1. **Claude Code does not put the model in its hook payload at all.** Recovering
   it means reading `transcript_path` (the JSONL carries `model` on assistant
   messages) — a file read on a hot path, with the session's own format as an
   undocumented dependency.
2. **Crush/qwen evidently does not send it either** — that is the observation
   that started this. Confirm what its hook payload actually contains before
   assuming it can be asked for.
3. **`SessionStart`-only is stale by construction.** A user typing `/model`
   mid-session silently invalidates whatever was recorded. Whatever lands should
   refresh on later events, or the field should be honest that it is
   "model at session start" rather than "current model".

**Why it matters beyond cosmetics.** The roster and `kj context list` are how a
human or a sibling agent answers "who is around and what are they." A context
that reports `(no model)` is not neutral — it reads as *unconfigured* when the
truth is *unreported*, which is the wrong kind of wrong for an instrument whose
whole job is telling you who is in the room. If a source genuinely cannot supply
it, say "unreported", not "(no model)".

**Suggested shape:** treat it as a per-source capability question first — a short
table of {agent, does its hook payload carry model, if not what is the cheapest
honest fallback} — and only then write code. The answer may well be "Codex yes,
everyone else needs transcript sniffing," which is worth knowing before building
transcript sniffing.

---

## `/v/docs` block filenames do not sort into document order (2026-08-15, Amy)

Blocks are already exposed in the VFS as `/v/docs/<context_id>/<block_id>`, each
a readable file (verified live: `cat` returns block content). But `ls` order is
useless for reading a conversation — block ids are `<ctx>_<principal>_<seq>`, so
lexical sort is **principal-major**, and the sequence sorts as a *string*. Real
output: `_0, _1, _10, _11, _12, _13, _14, _15, _2`.

Amy: *"we should modify the generated block filenames so they sort lexically more
naturally. or maybe kaish could offer a way to plug default sorts into ls?"*

**Recommendation: change the filenames, not kaish.** A pluggable `ls` sort
changes a general shell contract to solve one VFS's problem, and every other
consumer of that VFS still gets the wrong order. Encode the order in the name.

`order_key` is already a **base-62 lexicographic fractional index** built for
exactly this (`crdt/content.rs`, "Fractional index for sibling ordering"), so a
name like `<order_key>__<short_block_id>` sorts into document order by
construction, stays unique, and needs no shell change. Open questions before
building it: `order_key` changes when a block moves, so the filename is not a
stable identifier — decide whether that matters for the consumers we want (a
subscription view probably doesn't care; a bookmark would). Also check whether
`readdir` could simply return `block_ids_ordered()` order and whether kaish's
`ls` preserves readdir order or re-sorts.

**Why it's worth doing:** it unblocks a genuinely nice interface Amy sketched —
netrw-style directory open, but as a *subscription*: new blocks appear at the
top as they arrive, highlight an id to see a tail, select to open it read-only in
vi. That needs three things and ordering is the first. The other two: a
synthesized index so `ls` shows role/kind/status/preview instead of opaque hex
(`RosterFs`'s `/r/index` TSV is the precedent to copy), and wiring the existing
`onBlockInserted` feed to a VFS view. Amy sees this as "a substantial part of our
eventual tui experience, app too".

---

## Principal plumbing — a holistic sweep, not a per-lane patch (2026-08-15, Amy)

Amy: *"I don't think the principal plumbing should gate the git work. Let's make
a local note to do a sweep across the code and look at principal plumbing
holistically and wire it down to more places."*

**Trigger.** The CRDT melt's Lane B rules that config mutations auto-commit to
git, one commit per accepted mutation, each recording principal and operation id.
But config mutation APIs don't carry honest principal metadata today — a VFS write
arrives without knowing who asked for it, so those commits would be
service-authored with the real actor lost. That is one instance of a pattern, not
a Lane B bug: the same "who did this" gap shows up wherever a mutation crosses an
internal seam and the actor is dropped on the far side.

**Ruling: this does NOT gate the git work.** Lane B ships with service-authored
commits. Principal fidelity is a separate, wider improvement that lands on its own
schedule and retro-fits the commit author when it does.

**The sweep, when it happens.** Not a list of call sites to patch — a survey
first:

- Inventory every mutation path that reaches durable state and ask what it knows
  about its actor. VFS/config writes, rc edits, block mutations, editor sessions,
  MCP tool calls, kaish builtins, drift, and the `kj` verbs.
- Classify each: carries a real `Principal`; carries a synthetic/service one;
  carries nothing and infers; or genuinely has no actor (kernel-internal timers,
  boot seeding). The fourth category is legitimate — the goal is that it be
  *chosen* rather than the accidental default.
- Note where principal is available at the caller but dropped at the seam. Those
  are the cheap wins and should be the first patch series.
- Related, already-known: `architecture_agent_emerges_not_noun` — the actor is
  always a `Principal`, there is no first-class "agent" type, so this sweep is
  also the thing that makes provenance queries answerable at all.

**Why it's worth doing beyond git.** Provenance is what makes the shared-trust
model legible. Crosstalk is a feature here, and the kernel deliberately does not
enforce boundaries between cooperating players — which means the *record* of who
did what is the thing that keeps a many-hands instrument debuggable. Today a
mutation's author is recoverable in some paths and guessed in others.

**Do not** turn this into an authorization mechanism. Principals are for honest
attribution and recovery, not for denying operations between players
(`docs/instrument-design.md`, "Many hands, one trust boundary").

---

## Reconnect follow-ups from the auto-reconnect + backoff task (2026-08-14)

Landed: indefinite reconnect with jittered exponential backoff
(`crates/kaijutsu-client/src/actor.rs`'s `backoff_for_attempt_jittered`), and
a single post-reconnect re-init path in the app for the theme/metronome/
scroll config trio (`crates/kaijutsu-app/src/connection/actor_plugin.rs`'s
`refetch_config_on_reconnect` → `fetch_startup_configs_with`, triggered off
the same `ServerEvent::Reconnected` `bump_sync_generation_on_reconnect`
already used). Two things came up during that pass that were out of scope
for "reconnect correctness" and are recorded here instead of fixed in place:

**1. `SyncedInput` is never resynced after a reconnect, only after a fresh
context join.** `crates/kaijutsu-app/src/view/sync.rs`'s
`handle_block_events` only builds `cached.input` when
`RpcResultMessage::InputStateReceived` arrives AND `cached.input.is_none()`
(line ~140-168) — true on the initial `ContextJoined`, never true again once
an input doc exists. Unlike the block CRDT (which gets a real resync via
`get_context_sync` → `ContextResynced`, fired eagerly in
`RpcActor::enter_connected` on every reconnect — `crates/kaijutsu-client/src/actor.rs`
line ~2038-2064), an `EditInput`/`SubmitInput` a peer issued *during* the
outage never reaches this client's `SyncedInput` after it reconnects: the
input-ops stream rides the same block-events subscription (which the actor
does re-subscribe on every reconnect), so *future* edits are fine, but
whatever happened while this client was disconnected is a gap nothing
backfills. Fixing it isn't a one-line add — `get_input_state` exists as an
RPC, but `SyncedInput` has no `apply_sync_state`-equivalent reconciliation
method the way `SyncedDocument` does (see `synced_document.rs`
`apply_sync_state`), so wiring a raw re-fetch into the existing
`cached.input.is_none()` guard would either no-op (guard blocks it) or, if
the guard is loosened, risks clobbering in-flight local edits — exactly the
"crashing preferred over data corruption" case, not a silent-patch case.
Left alone rather than rushed.

**2. `poll_connection_status`'s comment about `periodic_reconnect` describes
a system that does not exist.** `crates/kaijutsu-app/src/connection/actor_plugin.rs`
(`poll_connection_status`, ~line 750 and ~line 795): "removes the `RpcActor`
resource so `periodic_reconnect` can spawn a fresh one." Grepped the whole
app crate — there is no `periodic_reconnect` fn, and `BootstrapCommand::SpawnActor`
is sent exactly once, from `ActorPlugin::build`. In practice this is dead
code, not a live bug: the actor's own FSM (`crates/kaijutsu-client/src/actor.rs`)
already retries indefinitely on every transient failure without ever exiting
`run()` or closing the status broadcast, so a normal kernel bounce never hits
this path. It would only matter if the actor's own tokio task panicked
outright (a real bug elsewhere, not a bounce) — in which case the app
currently has **no** recovery short of a restart, despite the stale comment
implying one exists. Worth either writing the `periodic_reconnect` system
the comment describes, or correcting the comment to say "there is currently
no recovery from this — restart the app" — but that's a design call
(does an actor-task panic deserve auto-respawn, and does a fresh actor need
a fresh `instance` or the same one?) outside this task's scope.

---

## Managing roots — the concept kaijutsu is missing (Amy, 2026-08-15: "eventually we need to come up with a way to manage these roots")

Seeded from replacing ROOT, where the three edges below each nearly killed the
new root inside twenty minutes. The pattern under them is one thing:

**"ROOT" is pure convention — a label plus a promotion — while every generic
mechanism treats it as an ordinary context.** Archive cascade took it. The
3-hour sweep would have taken it (29 days idle). Label uniqueness locked its
own name against reuse. None of those are bugs in *those* mechanisms; they are
each correct for an ordinary context, and the root is not one. More careful
procedure will not fix this — the specialness has to become structural or it
will keep being rediscovered by whatever generic pass runs next.

**The shape is settled (Amy, 2026-08-15):** *"I had thought to make it a dag
but the data is naturally a forest and drifts create cycles if you count them.
So, yeah, pinned or anchored contexts."* So: **a forest of one-parent trees,
with drift as a separate overlay that is deliberately NOT part of the
structural graph.** The code already agrees — `KernelDb::insert_edge` runs
`would_create_cycle` **only** for `EdgeKind::Structural`, leaving drift edges
exempt by construction. That is the invariant to keep: structure stays
acyclic because it is a forest, and drift is allowed to be cyclic because
nothing walks it as structure.

**Anchors are seats, not workspaces** — Amy's practice, and an open question
about enforcing it: *"my practice, maybe we should enforce, will probably be
to leave the anchors mostly unused, and create children of them for doing
stuff."*

The argument FOR enforcing is stronger than tidiness, and it is **fork cost**:
an anchor is the thing you fork from, and `kj fork` copies history by default
(see the fork-filters entry), so every block that lands in an anchor is paid
for again by every descendant, forever. The old ROOT had **90 blocks** — each
fork carried them. An unused anchor is not just clean, it is cheap, and the
cost of violating the convention is invisible at the moment you violate it
(you pay later, in every child). That is exactly the shape of rule worth
enforcing rather than remembering.

Partial enforcement already exists and is worth not re-deriving: the
`director` loadout has no drive/fork authority, so ROOT structurally *cannot*
drive turns already (`rpc.rs` genesis comment). The open question is narrower
than it looks — whether `anchored` should *imply* that loadout restriction, or
stay orthogonal to it.

**Two things worth deciding before designing anything:**

1. **One tree or a forest? This has never actually been decided — it was
   defaulted into.** `kj context create` resolves an absent `--parent` to the
   caller's context, so everything ends up in one tree descending from
   whatever ran the command. Genesis creates ROOT with `parent = None`, so a
   forest is *representable* and simply unreachable from `kj`. Are several
   live roots wanted (work / music / household, each its own tree), or one at
   a time with generations succeeding each other? Amy's phrasing — *"fork it
   to a new clean generation and archive it"* — points at succession, but the
   two are not exclusive.
2. **Is a root a seat of authority or a container of work?** Today it is the
   former: binding-admin, `director` loadout, deliberately *cannot* drive
   turns, forked-from rather than worked-in. That is why it sits idle for a
   month and why every activity-based heuristic reads it as dead. If that
   holds, then **idleness is a root's normal state**, and any liveness or
   recency signal is structurally the wrong instrument to point at one.

**The connection worth not missing: the janitor needs this exact concept.**
Whatever marks "never reap this" for a root is the same marker a janitor must
consult (see the janitor/librarian entry). Solve them separately and we build
two overlapping mechanisms that disagree at the edges. Solve the root marker
first and the janitor inherits its safety rule for free.

**Two shapes, cheapest first:**

- **A — an `anchor` bit, no new noun.** A context can be anchored: parentless
  by construction, cascade stops there (never archived as a descendant),
  never swept by age. `kj context create --detached` sets it. Roots become
  "anchored director contexts", multiple are allowed, and the forest falls out
  without being designed. Fixes all three edges below and gives the janitor its
  rule. Note **promotion is NOT already this** — the new ROOT was promoted
  before the archive and the cascade took it anyway.
- **B — a `kj root` verb family** over A: `root new` doing the whole
  succession dance atomically (create detached → promote → retag → retire the
  previous), `root list` showing generations, `root retire`. Worth it because
  that dance is five latched steps and getting the order wrong is what
  destroyed a context today — but only once A exists.

Recommendation: **A now, B when there is a second reason to want it** ("start
with less, it's easier to add more than take away").

### QUEUED by Amy 2026-08-15: *"when we're done with other things let's do the anchors and related fixups and guardrails"*

Slices, in dependency order. Slice 1 is worth doing even if the anchor design
changes — those are plain bugs, and two of them are what make the current
tree fragile.

**Slice 1 — guardrails (no new concept, all independently correct).**
- `kj context move` in ONE transaction, or cycle-check before deleting the old
  edge. Today a refused move orphans the context (edge 2b above, proved live).
- The archive latch prints a **consequence**, not an inventory: "this will also
  archive N descendant context(s)" instead of `1 children`. This is the line
  that cost a context today.
- Free the label on archive, or make the conflict error say the holder is
  archived and name `retag`. Right now "already in use" and "not found"
  contradict each other and neither points anywhere.

**Slice 2 — the anchor bit.** A column (`anchored_at`, same shape as
`promoted_at`/`archived_at`) plus three behaviours: **parentless by
construction**, **cascade stops** (never archived as a descendant), **never
swept by age**. `kj context create --detached` sets it. Genesis marks ROOT.
Multiple anchors allowed — the forest falls out rather than being designed.
Both layers, per the approval-ledger/roster precedent: a schema CHECK or
trigger that refuses a structural parent edge into an anchored context, *and*
the Rust check for the typed-error contract.

**Slice 3 — enforcement, pending Amy's ruling.** Does `anchored` imply the
no-drive restriction (making "leave anchors unused" structural rather than
habitual)? Argument for is fork cost, above. Note the `director` loadout
already provides most of it, so this may be "anchored implies director-ish"
rather than new machinery.

**Slice 4 — `kj root`/`kj anchor` verbs.** Only once 1–3 exist.

**Migration, do it in slice 2:** the current ROOT (`f0a66870`) is a structural
child of a `cc-kaijutsu-*` session context. Nothing sweeps automatically today
so it is not urgent, but it is the live instance of the landmine — anchoring it
must also detach it.

---

## Context lifecycle — three sharp edges found while replacing ROOT (2026-08-15)

Found the expensive way, replacing ROOT with a fresh deepseek-v4-flash
generation. All three are real; the first destroyed a context.

**1. `kj context archive` CASCADES to structural children, and the confirm
prompt does not say so.** The latch prints `(90 blocks | 1 children | 0 drift
edges)` — a count, not a consequence — and then reports `archived 2
context(s)`. I had reparented the new ROOT under the old one (to give the new
generation honest lineage), archived the old one, and **took the new ROOT with
it**. The blast-radius line reads like inventory; it should say what will
happen to those children, because "1 children" and "this will archive 1 other
context" are read completely differently at 2 lines of terminal output.
*This matters far beyond one mistake:* a janitor sweeping on age will archive
parents, and every descendant goes with them regardless of its own age. The
age filter people will reason about is per-context; the effect is per-subtree.

**2. There is no way to create a detached (parentless) context from `kj`.**
`kj context create`'s `--parent` resolves to the *caller's* context when
absent (`context_create`, "Default to root if no current context" — only
reached when the caller has none). Genesis makes ROOT with `parent = None`
(`rpc.rs`, `create_context_inner(..., None, ...)`), which `kj` cannot express.
So the root of the tree can only be born at genesis; recreate it any other way
and it descends from whatever session happened to run the command. Combined
with #1 that is a live landmine: **the current ROOT (`f0a66870`) is a
structural child of a `cc-kaijutsu-*` session context**, so archiving that
session — which the 3-hour rule will eventually do — cascades into ROOT.
Wants either a `--detached` flag or a `--parent` sentinel.

**2b. `kj context move` is NOT atomic — a REFUSED move orphans the context it
refused to move.** `context_move` deletes every existing structural parent edge
first, then calls `insert_edge`, which is where cycle detection lives — with no
transaction around the pair. So a rejected move has already destroyed the old
edge. **Proved live 2026-08-15**, not inferred: created `cyc-parent` with a
child, ran `kj context move cyc-parent cyc-child`, got the correct
`cycle detected: adding this edge would create a cycle` — and the tree then
rendered `cyc-parent` at top level with its real parent edge gone. A failed
operation left the tree changed.

Two consequences. It is a plain data bug (wrap the delete+insert in one
transaction, or check the cycle *before* deleting). And it is currently the
**only** way to produce a detached context from `kj` — via a failure path —
which is a wry confirmation of #2: the forest is representable and renderable
(the orphan displayed correctly as a root), and the CLI simply cannot ask for
it deliberately.

**3. An archived context still holds its label, so the label is
simultaneously "in use" and "not found".** `kj context create ROOT` →
`label conflict: label 'ROOT' already in use`; `kj context info ROOT` →
`not found: no context matches 'ROOT'`. The uniqueness check sees archived
rows, resolution does not. `retag` *can* still see the holder (it reported
`currently held by ROOT (b94d3f85)` for an archived context), which is the
only reason recovery was possible. Either the conflict check should ignore
archived rows, or the error should say the holder is archived and name
`retag` as the way through — right now the two messages contradict each other
and neither points anywhere.

**Also worth a ruling: should a promoted (ring0) context be sweep-exempt and
cascade-exempt?** Promotion did NOT protect the new ROOT from the cascade in
#1 (it was promoted before the archive). ROOT itself had 29 days of no
activity, so the 3-hour rule would archive the root of the tree on its own
merits — the manual sweep only spared it because it was excluded by label.

---

## Janitors and librarians — long-running contexts that tend the kernel (Amy, 2026-08-15)

Amy's direction, prompted by finding 195 idle contexts behind the roster:
**cleanup will eventually happen from within kaijutsu**, by contexts rather
than by a sweeper we bolt on. Her words:

> "We'll have a bunch of musician-like contexts that are janitors (cleaning up
> old contexts) and librarians (indexing, summarizing, and filing away).
> Similar shaped problem — stuff that runs forever and has a log-like
> structure to its observability."

Three things that ruling settles, worth not re-deriving:

1. **Musician-shaped, not cron-shaped.** These are contexts attached to
   something that runs, in the `docs/chameleon.md` sense — players, not
   scripts we schedule. That distinguishes them from "Grooming tracks —
   kaijutsu-style cron" below, which is the *scheduling* substrate; a janitor
   might ride it, but the janitor is a context.
2. **The observability shape is the shared problem, and it is log-like.**
   Anything that runs forever produces a stream, not a state — so the
   interesting question is what its *observability* looks like, not what its
   return value is. Same shape as the roster's own "history is otel, not a
   table" ruling: current state in one place, the narrative in the stream.
   Solve it once for the class.
3. **Two roles, deliberately distinct.** A janitor *removes* (archiving stale
   contexts); a librarian *preserves in cheaper form* (indexing, summarizing,
   filing). They fail in opposite directions, so they should not be one agent
   with a policy flag. Cross-refs already in this file: "Archive-time
   summaries, written by a local model" is librarian work; "Context lifecycle:
   'done for now' marker" is the signal a janitor would read.

Not scheduled, no slices cut. This is the durable answer, not the urgent one.

**The interim rule, from the first manual sweep (2026-08-15): no activity for
3 hours ⇒ expired.** Amy: *"anything older than 2-3 hours ago is expired and
can be archived."* 194 of 200 contexts went in one pass; ROOT and the five
live session contexts survived.

**That rule is provisional and its expiry condition is known.** Amy, same
session: *"eventually we'll have longer-living sessions that do local
inference and don't care about KV caches but for the moment it's a cheap
cleanup rule."* So the 3-hour number is not a judgment about when work goes
stale — it is downstream of **hosted-model KV-cache economics**, which is why
a session that has gone cold is worth little. A local-inference session has no
such cliff and may legitimately sit idle for days. **A janitor must therefore
take its cutoff from the context's own economics (is this a cached hosted
session or a local one?), not from a global constant** — bake 3h in as a
literal and the first long-lived local musician gets reaped mid-thought.

Three things the manual sweep taught, worth not re-learning:

- **Roster liveness is the WRONG safety filter for archiving, and it looks
  right.** `recent` liveness means "appended a block in the last 15 minutes",
  not "someone is attached". The roster reported **4** live contexts while
  **24** had been active within the day and an ACP lane was mid-review; a
  session that is connected but thinking has a live connection and an idle
  context. Filtering on roster-idle would have soft-deleted attached sessions'
  contexts. Use last-activity age, and treat "attached" as a separate question
  the roster cannot currently answer per-context (its `bound` rows are keyed by
  principal).
- **The archive latch scopes its nonce to the RESOLVED LABEL**, so confirming
  with the id you just listed fails with `nonce scope mismatch: unauthorized
  path '<id>' (authorized: ["<label>"])`. Unlabeled contexts scope to the short
  id. This is the gate research pass's finding #3 observed live — see "Gate
  slice 1a" below, which already says `authorized_label` must become the raw
  typed reference.
- **kaish loop counters do not persist across iterations here**, so a batch
  guard written as `if test "$i" -ge 10` never trips and a "batch of 10" runs
  the whole list. Nothing was lost (the per-item skip checks are independent
  of accumulators, so ROOT and live contexts were still protected), but verify
  bulk work by re-querying state, never by a counter the loop printed.

**Still unaddressed: the pile regrows on its own.** 9 `mcp-kaijutsu-*` contexts
existed on 08-15, **all minted that day**, and two (`0815-1201`, `0815-1203`)
appeared during kernel restarts within the hour. Same phenomenon as the filed
`cc-kaijutsu` prefix pileup.

*Corrected on the spot, because the obvious generalisation is wrong:* a later
restart in the same session minted **nothing** and the MCP client re-attached to
its existing context. So it is **not** one-context-per-reconnect — something
about the reconnect path sometimes reuses and sometimes mints, and **which is
which is the actual question**, not "reconnect leaks". The two naming schemes
in the wild are the visible half of that fork: `mcp-kaijutsu-<HHMM>` vs
`mcp-kaijutsu-<hex session id>` come from different registration paths, and the
timestamp-named ones are the suspicious set. A janitor that only sweeps is a
treadmill while the mint runs, so this wants diagnosing before a janitor is
built to paper over it.

---

## Live roster — push-on-attach is the remaining unwired half (2026-08-14)

Slices 1–4 (`crates/kaijutsu-kernel/src/roster.rs`, `roster_sources.rs`,
`kj/roster.rs`, `vfs/backends/roster.rs`) shipped in full: schema+store,
sources+refresh, `kj roster status`, `kj roster list` + `/run/roster`. Two
things were deliberately built but not wired into the running server,
because that branch could not start or verify a live kernel. **The first
shipped 2026-08-15** — `spawn_periodic_refresh` is now called from
`create_shared_kernel`, cancelled by `SharedKernelState::shutdown` on drop,
and covered by `tests/roster_refresh_boot.rs` (which reads no roster surface
on purpose, so the read path's inline `ensure_refreshed` cannot mask a
missing spawn). This one is still open:

**Push-based refresh on peer attach/detach isn't wired.** The design
record calls for pushing "where events already exist: peer attach/detach,
status post." Status-post is push-based today (`RosterStore::write_status`
writes immediately). Peer attach/detach still only reconciles on the next
pull tick or on-demand via `ensure_refreshed`. Correct either way (a `bound`
peer is never wrong for longer than one ~10s refresh interval), just not the
lowest-latency version the design allows. Would need a call from wherever
`kaijutsu-server`'s RPC layer currently calls `PeerRegistry::attach`/`detach`
into a narrow single-peer reconcile (or just `refresh_once`) — the server now
holds its own handle for exactly this, `SharedKernelState::roster`.

Also worth a look, not urgent: `roster_sources::RECENT_LIVE_WINDOW_MS` (15
minutes) is a v1 starting guess, not tuned against real usage — a config
knob if it needs adjusting.

---

## Gate slice 1a — three findings from the research pass (2026-08-14)

Slice 1a (gate the six destructive `kj` verbs, ledger via `KernelDb`,
`kj approve` CLI) stopped at the manifest wiring when the day ended. The
research pass found three things worth having before anyone resumes.

**1. kaish's own watchdog will kill a blocking gate — this is the trap.**
`kaish_request_timeout` (default 1800s, but **30s/15s/10s** for rc/hook/init
paths) bounds any `kj` builtin call independently of whatever timeout the gate
uses. A multi-minute human-answer wait gets killed by kaish before the gate's
own deadline ever fires. The fix pattern already exists in `kj_builtin.rs` —
`ctx.patient(budget)`, used today by the distill verbs. Wants a
`gate_wait_timeout` on `kaijutsu_types::TimeoutPolicy` (where
`llm_request_timeout` lives) so the patient-hold and the gate's poll deadline
read **one shared number** rather than two that can drift apart.
**Why it would have hurt:** the gate passes tests and dies in production rc
paths, where the budget is 10–30s rather than 1800s.

**2. Blocking at the six sites makes `KjResult::Latch` dead at runtime, and
five existing tests assert the opposite.** The six producers are the only ones
in the crate (grepped). Once the gate blocks there, nothing constructs a
`Latch` — the enum and nonce infra stay compiled but unreachable. The
`.is_latch()` assertions at `kj/workspace.rs:538`, `kj/doc.rs:876`,
`kj/context.rs:2619,3040`, `kj/preset.rs:525` **cannot pass alongside the
gate** and must be rewritten to assert gate behavior. Budget them into 1a, not
into the 0.14 bump.

**3. `authorized_label` must be the RAW typed reference, not the resolved
label.** The existing latch resolves `ctx_ref` → id → current DB label and uses
*that* as its nonce scope. Per Amy's label-not-id ruling the gate must use what
the caller actually typed, for both the statement's rendered text and
`authorized_label` — a deliberate divergence from the latch, and easy to
"fix" back by accident while reading the old code.

**Loose end:** `approval_ledger::ask::list_pending` was added and compiles but
has **no test** — undertested in a crate whose whole premise is tested
guarantees. `rules::list_rules` doesn't exist yet and `kj approve rules` needs
it. Both are first work on resume.

## kaish 0.14 bump — a confirmation-subsystem replacement, and it unpins `kaish-help` (2026-08-13)

> **Read the approvals ruling first.** Amy already decided the shape on
> 2026-08-12 — see *Drive gates* below, "kaish latches are going away —
> approvals become ours, bespoke". That entry correctly called this a
> **removal** (not a `LatchRequest` change), enumerated the latched verbs, and
> set the steer: kaish's command-visibility tools are the substrate, not a
> reimplementation of latches. It also carries a property to KEEP — the nonce
> is scoped to the **label, not the id**, so confirming names what it
> authorizes and an id-keyed batch fails loudly on scope mismatch.
>
> This entry disagreed with that one for two days and a 2026-08-14 session
> corrected *this* one without finding *that* one. Two entries in one file
> disagreeing is the real defect; they are cross-linked now. One addition from
> the re-probe: the filed verb list omits `kj doc delete` (`kj/doc.rs:434`),
> so there are **six** producer sites, not five.

We pin **kaish 0.13** (`Cargo.toml`). **0.14 removes the latch surface**
(`latch_request()`, `LatchRequest`, `.nonce`) — flagged by the kaish lead as
the translation-site port predicted during the cut, now concrete.

**Scope — CORRECTED 2026-08-14, and the earlier correction was itself wrong.**

The first pass here read: "one production call site
(`shell.rs:467`), five test fixtures — a small port, not a six-site sweep."
That is accurate *for the symbol it counted* (`latch_request()`) and wrong for
the work. It missed that **kaijutsu built its own confirmation subsystem on
kaish's `nonce` primitives**, which 0.14 deletes outright (not renames):
`kaish_kernel::nonce`, `NonceStore`, `ExecContext::verify_nonce`,
`ExecContext::latch_result`, `ExecResult.latch`, `JobStatus::Latched`.

Verified in-tree 2026-08-14:
- `kernel.rs:107` — `nonce_stores: DashMap<ContextId, kaish_kernel::nonce::NonceStore>`
- `kernel.rs:1435-1448` — per-context mint/lookup accessor
- `runtime/kj_builtin.rs:596-597` (`--confirm` extract), `:649`
  (`ctx.verify_nonce`), `:744` (`ctx.latch_result`)
- `kj/mod.rs:127-138,149,157` — `KjResult::Latch` + `is_latch()`
- **six producers**: `kj/context.rs:1672,1908,2020` (archive/remove/retag),
  `kj/workspace.rs:322`, `kj/doc.rs:434`, `kj/preset.rs:303`
- `mcp/servers/shell.rs:467,495` — the MCP envelope's `"latch"` key

So this is a subsystem replacement, not a port. **The lesson filed in
`signoff.md` about this entry — "say what a number means for the work" —
recursed: a correction praised for good framing was framed against the wrong
symbol.** A count is only as good as the question it answers; check that the
symbol you counted is the one the work is about.

**kaish removed the gate on purpose.** `~/src/kaish/docs/EMBEDDING.md:793-812`:
no kernel-held decision, no nonce, no interception hook. The embedder calls
`plan_program(source)`, judges, and executes or doesn't. So the replacement is
**ours to own end-to-end** — and it should be ONE path shared with the
permission-Ask seam (`HookAction::Ask`, `mcp/permission.rs`,
`subscribePermissionEvents @103`) rather than a second bespoke confirmation.
Six `kj` verbs and the `shell` tool gate want the same thing.

**Free win in the same bump:** 0.14 adds
`KernelConfig::with_job_manager(Arc<JobManager>)` — that is blocker #1 of the
three listed under *Background exec → kaish's job system*. The other two
(mid-run output forwarding, PDEATHSIG) are unchecked.

**Plan-API constraints worth knowing before designing against it:** there is no
execute-a-Plan API and no per-command interception hook (grepped for
specifically) — you re-submit the original source text, so a gate is
**all-or-nothing per statement**. And `presented_keys` / `--confirm` redaction
in the plan surface is vestigial: nothing in 0.14 mints or redeems a confirm
key (only `kaish-trash empty` keeps a bare no-nonce `--confirm` flag).

**It also closes an open TODO.** `kaish-help` is pinned to a git rev (not
crates.io) because published 0.13 forces an "Overlay mode" paragraph into every
composed recipe and kaijutsu never enables overlay mode — we would be telling
our models to run `kaish-vfs commit` for a mode that is off. That rev shipped
in **0.14.0**, so the TODO's own stated exit condition is met: flip
`kaish-help` to `"0.14"` in the same bump (`docs/composable-help.md` step 4).
The two are coupled — the help unpin is the reward for doing the latch port.

**`${var:0:N}` is NOT fixed by pinning crates.io `"0.14"` — MEASURED
2026-08-14, correcting this entry's original claim.** The fix is real but lives
in the **12 unreleased commits past the `v0.14.0` tag** (`d129cb5`). Built the
tag in a throwaway worktree and probed it directly:

| probe | published `v0.14.0` | HEAD `d129cb5` |
|---|---|---|
| `echo "${d:0:4}/file"` | **`/file`, exit 0, no diagnostic** | loud parse error naming `${d[0:4]}` |
| `echo "[${d:0:4}]"` | `[]`, exit 0 | loud parse error |
| `echo "[${d[0:4]}]"` | `cannot subscript a string` | `[/hom]` ✓ |

So on the published tag the trap is **live** (silent wrong path — the data-
corruption shape, not the missing-value shape) **and the documented replacement
syntax does not work on strings at all**. Pinning `"0.14"` from crates.io buys
nothing here.

**RESOLVED — Amy 2026-08-14: "the string range stuff will be in kaish 0.14.1."**
So: **no git-rev pin.** Do the bump against `"0.14"` (the confirmation-subsystem
replacement is the long pole and is independent of slice syntax), then flip the
pin to `"0.14.1"` when it is cut. The `${var:0:N}` trap stays live in the
interim, which is exactly the status quo on 0.13 — no regression, and a rev pin
would have re-created the very problem this bump retires by unpinning
`kaish-help`.

**Probes 4–6 showed no tag-vs-HEAD delta**, so these are unaffected by the pin
choice: `||` after a `$()` assignment still never fires (also absent from the
unreleased changelog — nobody upstream is tracking it).

**The leading-zero trap is BROADER than filed, not narrower.** A first probe
(`x=007; case $x in 007)`) matched, which looked like evidence the loss was
`for`-only — it is not, and the probe was under-designed: it normalizes BOTH
sides, so a match cannot distinguish "neither normalized" from "both
normalized". The decisive forms:

```sh
case "03" in 03) …      # NO-MATCH — the PATTERN normalizes
x=007; echo "[$x]"      # [7]    — bare-numeric ASSIGNMENT normalizes too
for i in 007; …         # [7]
```

Assignment was not previously recorded anywhere. So any rc script doing
`hour=08` — not just `case`/`for` — silently holds a different value than it
reads. Ruled INTENDED upstream, so plan around it permanently: **quote the
literal** (`x="007"`) whenever a leading zero is data. Worth knowing *why* it got
prioritized: our report said the expansion yielded empty, but the receiving
lane's re-probe found the word vanishes from the AST entirely — so inside
quotes it produced a **wrong path, not a missing one** (`"${d:0:4}/file"` →
`/file`, meaning `rm "${d:0:4}/file"` silently targeted the wrong thing). A
report describes what was visible; a probe finds the shape. Separately, the
bare-numeric leading-zero normalization is **ruled intended** (coerce to number,
quote for a string) and becomes a docs task, not a fix — though the `1e2`
inconsistency is real and should ride the write-up.

### kaish 0.15 gives the gate a real heredoc surface (relayed 2026-08-16, kaish lead)

**Status: on kaish `main` (`b58e492`, PR #340), NOT released.** Built partly for
our approval-ledger hooks, so it lands in the gate lane rather than the bump.
Two additions:

1. **`PlannedCommand.heredocs: Vec<PlannedHeredoc>`** — for `python3 <<'PY' …
   PY` the gate gets the command name, the delimiter word (`PY`, `SQL` — the
   language hint agents actually write), `literal`, `strip_tabs`, the body
   **verbatim**, and the body's own free variables. Today that body arrives as a
   single-quoted `'\''`-escaped blob inside `PlannedRedirect.target` with the
   delimiter rewritten to `EOF`, which is unreadable to a human confirming a
   statement.
2. **`Kernel::expand_fragment(source, FragmentAddr::new(stmt, heredoc), &scope)`**
   → `Expansion::Complete(String)` or `Blocked { holes }`. **You supply the
   scope; the kernel never peeks session state** — a `read TOKEN` binds at
   runtime, so a peeked value is stale exactly when it matters. A `$(…)` in the
   body does **not** run: it returns as a `Hole` carrying its nested `Plan`, and
   running it is the embedder's decision.

Three things to know before wiring it, two of which are silent-wrongness shapes:

- **`literal` is the security-relevant field.** Quoted delimiter → the published
  body IS what the command reads. Unquoted → the shell expands `${…}` and `$(…)`
  first, so a substitution can land **inside a string literal in the other
  language**. A gate that renders a statement for human confirmation without
  reading `literal` shows text that is not what runs.
- **`Complete` means "this is what runs", NOT "everything was supplied."** An
  unsupplied variable expands to empty, and **that is correct, not a gap** —
  verified against the binary, and deliberately not made an error: *a rule
  stricter than the interpreter would hand you a body the command never sees*,
  which is the same failure class the feature exists to prevent (kaish lead,
  2026-08-16). So `Complete` is honest about what it claims. The trap is purely
  that the word invites a stronger reading than it makes. `free_variables ⊆ your
  scope` is the stricter check, and the plan publishes exactly that list per
  heredoc.
- **BREAKING for any consumer of `PlannedRedirect.target` on a heredoc:** it now
  carries the delimiter word, not a rendering of the body.

**A gotcha for every consumer of `PlannedStatement.index`, not just this
feature:** `plan_program` numbers statements **before** dropping empty ones, so
a leading comment or blank line leaves a gap in the published indices. Anything
that filters `Stmt::Empty` and then indexes by a published index is off by one.
It bit `expand_fragment` itself and returned the wrong heredoc's body silently
until review caught it.

**The mechanism, which generalizes past kaish** (kaish lead, 2026-08-16): it was
not that the consumer filtered wrongly — it was that the **publisher and the
consumer disagreed about what an index means, and no type distinguished them.**
A `usize` looks identical either way. The sibling defect in the same PR had the
same shape (two AST walks that had to agree), and **both were fixed by deleting
the second thing rather than by aligning it.** Worth holding next to our own
rule that a structural impossibility beats a metric: two things that must agree
is a bug waiting for the day they don't, and the repair is usually subtraction.
Seven review defects on that PR across two passes, six of them confident-wrong-
output, none catchable by a gate.

**Scope it does NOT cover** — do not assume otherwise when arguing about gate
coverage: `python3 -c '…'`, `echo … | python3`, and write-then-run-later. It
improves the common case; the airtight configuration is still `subprocess` off.

Docs: `docs/EMBEDDING.md` "Command analysis", and
`crates/kaish-kernel/examples/heredoc_demo.rs`.

**Sequencing — build the confirmation renderer ONCE, against 0.15.** Nothing is
queued for 0.14.x; kaish `main` carries this surface plus #325/#326/#327, all
behavior changes aimed at 0.15, and the next work (#255 parser rebuild, #194
compounds in unquoted `$()`) is 0.15-shaped too. Amy, relayed 2026-08-16: *"I'm
tempted to go right on to 0.15 fyi"* — **a lean, not a ruling**, and the kaish
lead deliberately declined to upgrade it into a commitment. Treat 0.14.1 as the
waypoint that unpins `kaish-help`, and target the gate at 0.15. A hypothetical
0.14.2 would be a patch and would not carry this surface, so that risk does not
change the plan.

## Three findings from the slice-3 cross-model review (2026-08-13, gemini-pro + deepseek)

Both models independently confirmed the read-replica invariant holds (three
writers to `RemoteState.synced`, all server-sourced; zero `push_ops` callers in
the crate), so the flush/frontier deletions are sound. These are what they
found *around* it. The two hook-path defects they found are already fixed
(drop-guard for cancellation, `isError` consistency check); these three are not.

**1. MED — the shell poll's stall backoff is defeated by the doc task's own
resync bump.** `execute_and_poll_shell` treats any `change` bump as "the event
feed is alive," but `do_coalesced_resync` bumps `change` unconditionally when it
finishes — including the resync the fallback itself just requested
(`doc_task.rs`, end of `do_coalesced_resync`; `lib.rs`, the `watch_progressed`
arm). So the loop reads its own resync as delivery progress, resets
`stall_window` to the 5 s initial value and clears `stall_resubscribed`. The
documented 5/10/20/30 backoff never accumulates past its first step, and
`resubscribe_blocks` re-fires every ~5 s instead of once per episode. Net cost
on a dead bridge during a long command: a **full `get_context_sync` snapshot
every 5 s** for the command's whole runtime — precisely the tight poll the
backoff exists to prevent, on top of the already-filed per-call snapshot cost.
Wasted work, not wrong data.

*Attribution corrected by probe:* deepseek blamed slice 3's "uniform bump."
Wrong — `git show 7b1e288b:…/doc_task.rs` has the same unconditional
`bump(change)` in `do_coalesced_resync`. It arrived with the sole-writer doc
task (`be7b8b63`, 2026-07-17), whose own doc comment advertises the uniform bump
as the *fix* for the old listener not bumping. It fixed one thing and broke
another, and nothing noticed for four weeks. Fix shape: have the fallback
compare the `change` generation across its own resync, or give resync-origin
bumps a distinguishable marker.

**2. MED — `ResyncReason::StallFallback` sits outside `do_coalesced_resync`'s
staleness safety argument.** That argument (see the function's doc comment)
says an `ApplyEvent` processed after a swap is causally at-or-after the
snapshot, because every resync trigger *comes from* the ordered event stream.
`StallFallback` does not: it fires on a local timeout, exactly when the feed is
suspected slow or dead. An event delivered during the fetch may therefore
reflect **older** server state than the snapshot, and applying it afterward
regresses the field — silently, because the header setters stamp a fresh local
tick rather than doing LWW against the event's own timestamp. Pre-dates slice 3;
the code comment now carries the caveat. Real fix wants a server-side ordering
token on events, not a guess.

**3. LOW — rejoin race: an old doc task can clobber a fresh seed.**
`finish_join` writes the new snapshot into `remote.synced` *before* the old
`JoinedContext` drops and aborts the old doc task. In that window the old task
can apply a queued `Resync`/`ApplyEvent` to the same `Arc<Mutex<…>>`, and
`apply_sync_state` sets `context_id` from the payload — so the just-seeded
document is replaced by the *old context's* snapshot. Reachable via
`stabilize_context_label`'s reattach path calling `finish_join` a second time.
Does not violate the read-replica invariant (the clobbering content is still
server-sourced), but it is a wrong-context race: the seed write and the abort
are not ordered by any lock. Fix shape: abort the old task before seeding.

**Also noted, not filed as a defect:** `completeBlock`'s `isError` is redundant
with `status` by construction. It is now read as a consistency check (a
contradiction is refused) rather than ignored. If a future schema revision is
happening anyway, dropping the field is the cleaner end state — but it is not
worth a bounce of three binaries on its own.

## Theme changes never reach a running app — there is no live config push (2026-08-13, revised)

The 2026-08-12 version of this entry blamed raster-time gates in the app for
theme staleness. Those gates are now fixed and tested
(`repaint_block_scenes_on_theme_change` in `view/block_render.rs` reopens
BOTH doc-version gates — `last_render_version` for color re-derive and
`last_built_version` for glyph re-bake — one frame, on any `Theme` resource
change). But live BRP verification exposed the deeper contributing factor
the old entry got wrong: **`ThemeReceived` has exactly one send site, the
connect-time bootstrap fetch** (`actor_plugin.rs`), and `ServerEvent` has no
config/theme variant at all. `kj config set /etc/config/theme.toml` updates
the CRDT and nothing tells a running app — verified pixel-identical
before/after a live set, while the same change applied fine across a
restart. The 08-12 dock A/Bs must have ridden restarts/reconnects.

Remaining work is the delivery leg (kernel + client + app): a config-changed
server event (or a config subscription) that re-fires `ThemeReceived` on
theme writes. The app side is ready — the moment `Theme` is replaced, the
full repaint happens (this is unit-tested). `docs/color.md` sells `kj config
set` as a "live color-management console"; until the push exists, it is a
next-connect console.

Related smaller finds from the same session: block text colors
(`block_user`, `block_assistant`, …) exist only in the app's compiled-in
`Theme` — `ThemeData` (the TOML wire format) has no fields for them, so no
theme file can change conversation text colors at all. And `Theme` derives
neither `Reflect` nor registers with BRP, so it cannot be poked remotely for
testing.

## `kj config show` output is not round-trippable, and a corrupt theme falls back silently (2026-08-12)

Two contributing factors that compounded into an hour of phantom results:

1. `kj config show <file>` prints a human header (`path:`, `length:`, blank
   line, ` ```toml ` fence, closing fence). Piping it into `kj config set`
   — the obvious sed-tweak idiom, which the shell happily accepts — embeds
   the header into the stored file; each roundtrip nests another copy.
   Wants a `--raw`/`--body` flag (or: `set` could refuse content whose first
   line matches its own `show` header — it is never intentional). Even the
   "safe" strip idiom below accretes one trailing `\n` per roundtrip
   (measured 2026-08-13: 6502 → 6503 → 6504 … bytes) — harmless to TOML,
   but the length check must expect +1, not equality.
2. When the stored theme TOML fails to parse, the app falls back to
   compiled-in defaults **silently** (no dock indicator, nothing in the
   conversation). Every subsequent theme experiment silently tested the
   defaults instead. Doctrine says crash > corruption; here at minimum the
   parse error should surface loudly (drift notification / dock error glyph).
   Repro: any `show | sed | set` roundtrip before (1) is fixed.

## Dock RTT sizes skip physical-px rounding (2026-08-12, kaibo find)

`render_north_dock`/`render_south_dock` stamp `rtt.built_width = logical.x`
raw (`ui/dock.rs`), while block cells round via `round_to_physical_px`
(`view/block_render.rs:1222`). At fractional DPI that makes
`msdf_item_scale` a hair off exact, giving sub-pixel glyph drift on the
dock. Cosmetic, pre-existing (found during the premultiplied-compositing
review, deepseek job-1); fold into the tier-2 "unify RTT resize" cleanup.

## text_glow wants a re-tune with Amy's eyes (2026-08-12, post-blend-fix)

The violet text halo (`text_glow_radius = 2.5`, `#cbb8ff59`) now renders at
its true designed strength — the straight-alpha compositing bug had been
crushing it since birth, so its tuned values have never actually been seen
on screen. At a 960×600 window it reads as fuzz (it was most of the "text
looks fuzzy" report), with glow off the text is *crisp*; at 4k it may well be
the intended synthwave neon. Decisions: strength/alpha, and whether a
fixed-pixel radius should scale with font size / scale factor. Live CRDT
currently has it OFF for Amy to eyeball; repo seed still ships 2.5.

---

## kaish output limiting — REMEASURED 2026-08-15 against a live kernel, and both halves of the original filing were wrong

> **Read this correction before acting on the entry below.** Probed against
> the running zorak kernel (restarted 07:03 EDT onto `e2905a86`, still pinned
> to `kaish-kernel = "0.13.0"` per `Cargo.lock` — *not* the pending 0.14 bump).
> Every claim here is a measurement, with the control that isolates it.
>
> **1. Command substitution does NOT truncate at 8 KB on this build.**
> `big=$(seq 1 20000)` round-tripped **108896 bytes** intact.
> `n=$(seq 1 5000 | wc -l)` returned exactly **5000**, and
> `$(seq 1 5000 | grep -c .)` likewise — the pipeline-into-`grep -c` shape
> that produced the original 12-vs-105 report. It does not reproduce.
> **This does NOT mean it was never real.** The 12-vs-105 observation was
> made by someone watching it happen, and a negative probe is a claim about
> the probe (signoff process lessons, "a negative grep is a claim about your
> PATTERN"). What changed between 08-13 and today is unestablished and is the
> open question — not whether the original reporter was mistaken.
>
> **2. Truncation DOES set a failure code — the entry's central claim is
> backwards, and candidate fix #2 below is already shipped.** kaish remaps the
> exit code to **3** on `did_spill`, preserving the real code in
> `original_code`; it is documented at `kaish-kernel-0.13.0/src/output_limit.rs:14`
> and covered by its own `test_kernel_memory_mode_exits_3_preserves_original`.
> So "ask kaish to make Memory-mode truncation loud — a nonzero status" was
> asking for something kaish had already done, in the version we are pinned to.
>
> **3. The live hazard is the opposite shape from the one filed: loud but
> misattributed, not silent.** Controlled matrix, one variable at a time —
> `seq 1 100` captured → exit **0**; `seq 1 5000` captured → exit **3**;
> `seq 1 5000 > /dev/null` → exit **0**. Identical command, succeeds every
> time. So inside a kaish script, **`$?` is 3 for a command that worked**,
> purely because it printed a lot. Any `set -e`, any `cmd || fallback`, any
> `if cmd; then` takes the failure branch on success.
> **`kj`/MCP callers are NOT affected** — `mcp/servers/shell.rs:448` already
> does `result.original_code.unwrap_or(result.code)` with the right comment
> ("truncation is not failure"). The exposure is **rc and hook bodies**, which
> is precisely where the gate's classifier escalator is designed to live
> (signoff: "the classifier call is an rc/kaish thing"). An rc script doing
> `resp=$(<a POST that returns a large body>) || escalate` would take the
> escalate branch on a perfectly good response. Worth settling before that
> script is written, not after.
>
> Still true and untouched by this correction: the `localfs` analysis below
> (we build without it, so Memory mode is our only mode and no spill path
> exists), and the doctrine question in candidate fix #3.

### 2026-08-15, later: the exposure analysis above was too narrow — a second write path corrupts the DURABLE exit code

**RESOLVED 2026-08-15.** `execute_shell_command` (`rpc.rs`) now resolves
`result.original_code.unwrap_or(result.code)` before persisting `exit_code`
and before the `final_status` match (both now read the resolved code, not the
raw one — see "final_status" note below). `did_spill` remains discoverable via
kaish's own inline `[output truncated: N bytes total — ...]` marker baked into
the persisted block body (no structured field on this path, unlike MCP's
`shell.rs` envelope — see the new backlog item below). The host-dependent
`mount` test (`context_shell.rs::unknown_command_fails_fast_exec_granted_shell`)
now runs `id` instead, which is not a kaish builtin and always prints a short,
bounded line. A new regression test,
`test_shell_truncation_does_not_corrupt_exit_code`
(`crates/kaijutsu-server/tests/e2e_kj_workflow.rs`), pins the exact shape: `seq
1 5000` (a builtin, ~19 KB, always exits 0) must record `exit_code = Some(0)`,
not `Some(3)`. It failed before the fix and passes after.

One correction to "keep the `0 | 2 | 3` status match as-is (it is
independently correct)" above: it was **not** independently correct.
kaish's remap is unconditional — a command that *fails* and also spills
>8 KB gets `code = 3` with the real failing code in `original_code`, so
matching on the raw code folded a genuine failure into the `3 => Done` arm.
`final_status` now matches on the same resolved code as the persisted
`exit_code`; this is a no-op for every case except spilled-and-failed, which
it now classifies correctly.

**New backlog: the same bug shape, found by an exhaustive sweep for other
`ExecResult.code` consumers, in four places still unfixed** (kaijutsu-kernel
has two unrelated types both named `ExecResult` — kaish's, with
`did_spill`/`original_code`, and kaijutsu's own internal engine-call type in
`execution.rs` with neither; only the former is in scope here):
- `crates/kaijutsu-kernel/src/kernel.rs:1318` — `EditorIo::ReadShell` (vi's
  `:r !cmd`) checks `result.code != 0` raw; a `:r !cmd` whose output spills
  reports a spurious failure to the editor even though the command succeeded.
- `crates/kaijutsu-kernel/src/kj/lifecycle.rs:507,532` — rc-lifecycle `.kai`
  script execution matches `exec.code == 0` raw and **persists** the
  unresolved code into a durable rc-failure block on the fallthrough arm; a
  successful rc script that spills >8 KB gets permanently filed as failed.
- `crates/kaijutsu-kernel/src/mcp/broker.rs:1939-1953` — hook body execution
  (tool-call pre/post hooks) matches `exec.code == 0` raw and surfaces `"kaish
  hook exit {}"` with the unresolved code on failure; a spilled-but-successful
  hook can incorrectly abort/flag the tool-call pipeline.
- `crates/kaijutsu-server/src/rpc.rs:~1270` (`dispatch_output_events`, backing
  the streaming `execute` RPC — a different path from `shell_execute`/
  `execute_shell_command` above) — `set_exit_code(result.code as i32)` ships
  the unresolved code over the wire to every `on_output` subscriber.

None of these four are touched by this fix — flagged here per CLAUDE.md
("note problems we can fix later") rather than fixed opportunistically, since
each sits in a file this session was not scoped to touch.

The correction above concluded "**`kj`/MCP callers are NOT affected**" on the
strength of `mcp/servers/shell.rs:448` doing
`result.original_code.unwrap_or(result.code)`. That is true of *that* path. It is
**not** true of the path that writes the durable record.

`execute_shell_command` in `crates/kaijutsu-server/src/rpc.rs` persists
`result.code` directly (`rpc.rs:8544`, `set_exit_code(... exit_code_i32 ...)`
where `exit_code_i32` comes from `result.code`). It logs `original_code` five
lines earlier (`rpc.rs:8469-8471`) and then does not consult it. The comment
immediately above the write says *"Persist the **real** kaish exit code on the
ToolResult block"* — which is precisely what it does not do once output spilled.

**Consequence.** Any shell command routed through the server whose output
exceeds the 8 KB `OutputProfile::Agent` cap records `exit_code = 3` on its
ToolResult block, permanently, for a command that exited 0. `rpc.rs:4390`
describes that field as "the durable, authoritative" value read by MCP
`context_shell` return, BRP introspection, and history views. So this is wrong
data at rest, not a transient misreport.

It is *not* visible as a failure, which is what let it survive: `final_status`
matches `0 | 2 | 3 => Status::Done` (`rpc.rs:8588`), so the block looks fine and
only the number is wrong. Silent, and in the durable record — the shape CLAUDE.md
rejects twice over.

**Fix:** the same unwrap `mcp/servers/shell.rs:448` already does, applied before
the clamp at `rpc.rs:8543`. Keep the `0 | 2 | 3` status match as-is (it is
independently correct), and keep `did_spill` observable — truncation should be
*discoverable*, just not by corrupting the exit code.

**There is a failing test on `main` that reproduces this**, found 2026-08-15
while auditing an unrelated config slice:
`kj::context_shell::tests::unknown_command_fails_fast_exec_granted_shell`
(`crates/kaijutsu-kernel/src/kj/context_shell.rs:464`) asserts `mount` runs and
exits 0 in an exec-granted shell. It fails on `main` on this host — `mount`
prints 15,381 bytes here, well past the 8 KB cap, so kaish remaps the exit to 3
and `res.ok()` is false with an empty `res.err`.

**The test is host-dependent**, which is the trap: it passes wherever `mount`
happens to print under 8 KB and fails wherever it does not, so it reads as a
flake and will be dismissed as one. It is not a flake — it is the bug, reproduced.
Whoever fixes the exit-code write should also make this test's dependence on
host mount-table size explicit rather than incidental (bound the output, or
assert on `original_code`).

### Original entry (2026-08-13), retained for its reasoning

`OutputLimitConfig::agent()` (`kernel/src/runtime/embedded_kaish.rs:264`) caps
a builtin's captured stdout at 8192 bytes. Crossing it does **not** error and
does **not** set a failure code — the capture collapses to a ~1.6 KB
head+tail splice with `[output truncated]` between them. Command substitution
therefore hands the script a *plausible, wrong* value.

Found the expensive way during slice-1 rc work: an unbounded
`grep '^- \[' MEMORY.md | grep -c .` reported **12** where the truth was
**105** — the first pass overflowed the cap, and the second counted the
splice. Nothing anywhere reported a problem.

This is the silent-fallback shape CLAUDE.md rejects, and rc scripts are its
worst host: an rc script computing a digest, a count, or a hash off a
truncated capture writes a confident wrong fact into a context, and per the
`create` blast radius it does so on every session registration.

**This is ours, not kaish's** (corrected after filing — the first version of
this entry read as a kaish defect and would have misrouted the fix). kaish's
`OutputLimitConfig::agent()` specifies `SpillMode::Disk`: overflow goes to a
spill file and the truncation message *carries the path*, which is visible and
recoverable. We don't get that, because
`kaish-kernel = { version = "0.13", features = ["subprocess"] }` (workspace
`Cargo.toml:51`) **omits `localfs`**, and a build without it "always behaves
as `SpillMode::Memory` regardless of this setting" — silent head+tail splice,
no pointer. kaish's own module doc even says truncating silently "could
corrupt structured data that an agent acts on."

Candidate fixes, cheapest first:

1. **Raise or remove `max_bytes` for rc/context shells** — one line at
   `embedded_kaish.rs:264`. 8 KB is a sandboxed-agent default we inherited
   without choosing it.
2. **Ask kaish to make Memory-mode truncation loud** — a nonzero status or a
   testable shell var. The only genuinely kaish-side option, and worth it
   because Memory mode is the *only* mode for any no-`localfs` embedder.
3. **Enable `localfs`** to get the pointered disk spill. This is a **design
   conversation, not a patch**: it hands kaish host-filesystem writes, and
   CLAUDE.md's "host exec has one owner" doctrine means that routing decision
   is not a Cargo feature flag we flip quietly.

Also note we are pinned to kaish 0.13 while **0.14.0 published today** — check
whether anything here moved before acting.

Until one lands, the discipline is **bound every read before it lands in a
variable** (`head -n`, a filter) — recorded in `memory.md`'s mechanics section.

## Memory system — direction decided, slices open (2026-08-13)

Direction is now canonical in [`memory.md`](memory.md): **the kernel is
memory's best reader, not its new owner** — git keeps storage, kaijutsu grows
recall. Read it before proposing anything memory-shaped; several attractive
designs are explicitly dead there (CRDT memory tree, memory-as-contexts, fact
schemas), and one rule binds work well outside memory:

- **The derived-state rule.** "No second store" is enforced by asking *"can
  this state disagree with truth **silently**?"* — `derived` is not an
  exemption. Applies to every cache, index, and mirror we add from here.

Open slices, in `memory.md`: S15 recall script + baseline classification
(zero Rust), `kj memory search|recall` over MCP, write-back proposals with a
named drain, and the two kernel-only capabilities (per-turn mailbox recall,
change-push). Semantic indexing over the forests is **deferred with reasons**
— the index is `ContextId`-keyed end to end and HNSW's never-reuse slots leak
a permanent graph point per re-embed, which a git-tracked corpus edited
hourly would punish.

Two blockers that memory work now co-owns, both already listed elsewhere in
this file: **hook self-lockout has no recovery path** (third independent
reason to fix it — it gates the constraint-hook seam), and the
**external-drive gate** is a prerequisite for the resident-assistant seat.

## Two seeds from the rc-create lockout fix (2026-08-12, `788fb0d7`)

Found while closing that item; recorded rather than folded in, because
neither is that bug.

- **`test_dispatcher` leaves the broker's DB handle unset**, so
  `broker.set_binding` (and therefore `kj binding allow`) lands only in the
  in-memory cache — which `require_cap` and `has_usable_loadout` never read,
  by design: they read KernelDb, the authoritative store the broker
  write-through targets in production. The rebind repair test hit this the
  loud way: the rc script reported `allowed operator on context …` and the
  loadout stayed empty. Fixed *in that one test* with
  `broker().set_db(kernel_db.clone())`. **Open question: which other binding
  tests on this fixture are asserting against the cache instead of the
  store?** `kj::drive`'s gate tests already know (they write straight to the
  DB with a comment explaining why) — so the knowledge exists and is applied
  ad hoc per test. Candidate fix: wire the DB in `test_dispatcher` itself, so
  the fixture matches production and the per-test workaround stops being
  something each author has to know.
- **Sweep the authz-shaped `.ok().flatten()` / `unwrap_or(false)` sites.**
  The "errors that were only strings" pass (Aug 3–4, devlog) fixed this
  collapse in `Broker::binding_checked`; `require_cap` held an untouched copy
  for months, where a KernelDb read failure was indistinguishable from a
  missing grant. The family is "a fault silently becomes a policy decision" —
  worth grepping deliberately in gate/predicate paths rather than waiting for
  the next one to surface. Note the direction of the risk: deny-by-default
  makes the *safe* wrong answer easy to ship and hard to notice.

## Summaries drift stronger than what they summarise (2026-08-11, three instances in one day)

Not a code bug — a writing failure mode worth naming, because it cost real
scoping work today:

1. This file claimed "isotest proved fresh-$HOME boot ... and no host state".
   isotest proves process-lifecycle guarantees; fresh-$HOME is a precondition
   demonstrated by construction. Work on brak was scoped against the stronger
   reading (see the brak entry).
2. `crates/kaijutsu-server/src/clock.rs`'s own module header called
   `ModeledClock` "an uninhabited placeholder ... you cannot construct one
   until M3". It has had a body, a `ClockSource` impl, a persistence path, a
   `kj` selector and an end-to-end producer for weeks — and was contradicted
   by `from_persisted` a hundred lines below it in the same file. Fixed.
3. `docs/tracks.md` inherited (2) and listed M3 as "Ahead".

Same shape every time: someone summarised what was *observed*, the summary read
stronger than what was *pinned*, and the next reader inherited the stronger
version without re-deriving it. Note the direction is not always optimistic —
(2) and (3) *under*-reported shipped work, which is how a finished subsystem
stays invisible.

Candidate convention, **not yet a rule — Amy's call**: a claim about what is
*proven* carries the assertion name or a `file:line`, so the next reader can
tell pinned from observed at a glance. Three instances is a pattern; a rule
wants her word.

---

## rmcp protocol-version fallback drops us to 2025-11-25 (2026-08-11, via kaibo lead; re-verified against our own version)

**Trigger has a date, not a probability: the day a client requests a protocol
version newer than `2026-07-28`.** Claude Code is the client that will do it.

kaibo lead flagged this as a latent bug across rmcp-based servers. It applies
to us, but **two of the relayed specifics are wrong for rmcp 3.0.1, which is
what we pin** (`Cargo.toml:141`; kaibo is on 3.1.2) — and the difference
matters because the recommended fix does not fix the actual failure.

Read from `~/.cargo/registry/.../rmcp-3.0.1/`:

- **`supported_protocol_versions` is NOT 3.1.0+**, and is not our problem. It
  exists in 3.0.1 (`src/handler/server.rs:328`) and its trait default already
  returns `ProtocolVersion::KNOWN_VERSIONS`, which **includes**
  `V_2026_07_28` (`src/model.rs:186`). So the "strict client rejects
  tools/list" path (`src/handler/server.rs:65-71`) does not fire for
  2026-07-28. We do not override it, and we should not need to.
- **A client asking for `2026-07-28` is honored, not downgraded.**
  `negotiate_protocol_version` (`src/service/server.rs:464-478`) returns the
  client's requested version whenever it is in `KNOWN_VERSIONS`.

The real exposure is narrower and worse:

- For a version rmcp 3.0.1 does **not** know — i.e. anything newer than
  `2026-07-28` — negotiation falls back to `server_fallback`, which is
  `get_info().protocol_version`. Our `get_info` (`crates/kaijutsu-mcp/src/lib.rs:2130`)
  uses `ServerInfo::new(...)` and never calls `with_protocol_version`, so it
  takes `ProtocolVersion::default()` → `LATEST` → **`V_2025_11_25`**
  (`src/model.rs:175`). We would not fall back one step to 2026-07-28; we
  would fall back **two**, past a version we fully support.
- Not fully silent: it emits `tracing::warn!("client requested unsupported
  protocol version; falling back to server default")`. Whether we would *see*
  that in a stdio server's log is a separate question worth answering.

**The fix is `with_protocol_version`.** Setting
`ServerInfo::new(...).with_protocol_version(ProtocolVersion::V_2026_07_28)`
makes the fallback land on the newest version we actually implement. Cheap,
and it converts a two-step silent-ish regression into a one-step one.

**Version-qualified, and this is the part that will bite on a bump:** *on
3.0.1*, overriding `supported_protocol_versions` cannot help, because
`negotiate_protocol_version` never consults it — it reads the hardcoded
`KNOWN_VERSIONS` (`src/service/server.rs:468`). **That mechanism is fixed
upstream in 3.1.2**, where negotiation *does* consult the server's supported
set (kaibo lead's read of their own vendored 3.1.2: `service/server.rs:587-591`,
with a `server_supported.contains` check at `:474`). So the *lever* is
correct on both, but the *reason it works* differs — do not carry this
paragraph's reasoning across an rmcp bump without re-reading the source.

Re-check on any rmcp bump generally: the constants (`LATEST`,
`KNOWN_VERSIONS`, `STANDARD_HEADERS = V_2026_07_28`) and the negotiation path
both move underneath us. Related: the SEP-2577 deprecations papered over in
the 1.7 → 3.0.1 bump are still open above.

**Practice this produced** (kaibo lead, after each of us read our own pinned
copy and reached different true answers): *"neither of us should have
inherited the other's read of a differently-pinned crate."* A dependency
analysis is a claim about a version, not about a crate.

Skew note: `~/bin/kaijutsu-mcp` is a **separately built binary** — a fix here
does not reach a running client until that binary is rebuilt and relaunched.

## MCP 2026-07-28 adoption — four slices, in priority order (2026-08-11, post rmcp 3.1.2 bump)

We now negotiate `2026-07-28` on both surfaces and run rmcp 3.1.2. That opens
four things worth having. Ordered by value; Amy approved working them in this
order. Verified against the vendored SDK, not training memory — re-verify on
any rmcp bump (see the version trap in the rmcp entry above).

### 1. Elicitation — we silently decline every request today

`BrokerClientHandler` (`crates/kaijutsu-kernel/src/mcp/servers/external.rs`)
does not override `create_elicitation`, so rmcp's trait default
(`handler/client.rs:179-191`) **auto-declines every elicitation, silently**. An
external server asking us a question gets a "no" nobody ever sees. That is the
silent-fallback pattern CLAUDE.md rejects, and the socket for the fix already
exists: `ServerNotification::Elicitation` (`mcp/server_like.rs`, `mcp/types.rs`)
is defined and its one consumer no-ops it (`mcp/broker.rs`, *"Reserved per
D-25; no live handling yet"*). Nothing constructs it.

Mechanism: a server sends `elicitation/create` in one of two modes — **form**
(`message` + `requested_schema`, a restricted JSON Schema of primitives only,
`model/elicitation_schema.rs`) or **URL** (`message` + `url` +
`elicitation_id`, for out-of-band consent flows). We answer
`ElicitResult { action: Accept | Decline | Cancel, content }`.

Slices:
- **1a — stop being silent.** Override `create_elicitation` to emit
  `ServerNotification::Elicitation` and surface it as a block, then decline.
  Same outcome as today, but *visible*. Small, no policy questions.
- **1b — let someone answer.** In the many-hands model the answer can come
  from a human, a sibling context, or the app — the protocol does not care
  who. Wants a real design pass: where the pending elicitation lives, how an
  answer routes back, timeout behaviour. **Not a patch — a design
  conversation.**
- Blocker for both: `peer.elicit`/`elicit_url` are gated
  `#[cfg(all(feature = "schemars", feature = "elicitation"))]`;
  `crates/kaijutsu-mcp/Cargo.toml` enables only `server`/`macros`/
  `transport-io`, so the **`elicitation` feature must be added**.
- Unknown, needs probing: whether kaibo or bevy_brp ever send elicitation, and
  whether Claude Code's client implements it on the server surface.

### 2. `structuredContent` + `outputSchema` on `shell`

`ShellCompletion::to_json` hand-rolls an envelope, stringifies it into a
`TextContent`, and hopes the caller re-parses. 2026-07-28 has a first-class
mechanism: `Tool::with_output_schema::<T>()` declares the result shape, and
`CallToolResult::structured()` carries it as real JSON
(`model.rs` `structured_content`). Keep the existing field names
(`stdout`/`stderr`/`exit_code`/`status`/`block_id`/`content_type`/`ephemeral`/
`data`/`elapsed_ms`) so nothing downstream breaks; add a short human-readable
`text` block for clients that only render `content`. Consider
`ContentBlock::ResourceLink` for `block_id` so it becomes navigable rather
than a string the model must know to re-request. Self-contained, low risk.

### 3. `on_progress` is a deliberate no-op

`BrokerClientHandler::on_progress` is an explicit empty impl marked "Phase 2".
Long kaibo consultations are therefore 15 minutes of silence. Note the
`progressToken` must be sent by *us* in the request `_meta` for a server to
report against it. Pairs naturally with (4).

### 4. Tasks (SEP-2663) for outbound long calls

`tools/call` can return a task handle instead of blocking: statuses
`working`/`input_required`/terminal, `tasks/get` polling with a
server-suggested `poll_interval_ms`, `ttl_ms` expiry, cooperative
`tasks/cancel`, `notifications/tasks` pushes, and in-task input requests
answered via `tasks/update`. Client must declare `.enable_tasks()` first; no
compliant server offers it otherwise.

**Two boundaries to hold, decided 2026-08-11:**

- **MCP `TaskStatus` is NOT `BlockKind::Task`.** The enums rhyme
  (`Working/InputRequired/Completed/Failed/Cancelled` vs
  `Open/InProgress/Done/Cancelled`) and they are different nouns: ours is a
  durable to-do item tracked in a conversation, theirs is an in-flight RPC
  handle. Do not unify them.
- **MCP tasks do NOT replace `background_exec`.** Ours streams output into a
  CRDT block every player can see; an MCP task's result goes only to the one
  client polling it. In a many-hands model that is a downgrade. Tasks are for
  the *outbound* long-call problem (kaibo), not for kernel-owned background
  work.
- Amy's related question — should kaish `jobs` show MCP work? — resolves the
  same way: an MCP **task** is job-shaped (stable id, status, cancel, TTL) and
  belongs there; a raw in-flight **request** is not (no pid, no signals, wrong
  ownership direction, sub-second churn) and wants a read-only `kj mcp` view
  instead.
- Unknown, needs probing: whether kaibo declares the tasks extension.

### Also open, from the same survey

- `Timeout` no longer marks an instance Down (fixed `0676da42`), but there is
  still **no automatic reconnect** — `reconnect()` exists and is never called
  ("Phase 1 does not invoke this automatically"). A genuinely dead server stays
  Down until `kj mcp reload`. Worth a health-check/backoff pass.
- We call the raw `Peer::call_tool`/`read_resource`, not the MRTR-aware
  `RunningService` helpers. Blast radius is fixed (`f389e84f`), but we still
  cannot *drive* an MRTR round-trip — and doing so needs (1) first, since the
  helpers fulfil input requests through the `ClientHandler` that currently
  auto-declines.

## `kj backend` has no health check (re-filed 2026-08-11)

Salvaged from the deleted models.toml papercuts section — the only idea there
that outlived the file it was about. There is no way to ask whether a
configured backend's `base_url` actually answers; a wrong URL surfaces as a
failed turn much later. Wants something like `kj backend check <name>` (or a
`--check` on `kj backend list`) that probes each configured endpoint and
reports reachability + model list. Verified absent: no `doctor`/`check` verb in
`crates/kaijutsu-kernel/src/kj/backend.rs`.

---

## Background exec → kaish's job system (2026-08-07, Amy: "we should do the work and set the rule")

An audit of every spawn site found exactly one ad-hoc host exec left in
production: `spawn_background` (`crates/kaijutsu-kernel/src/background_exec.rs:552`,
NOT under `mcp/servers/`) runs an
agent-supplied string as `/bin/sh -c`, called from `shell.rs:200`
(`start_background`, the `shell` tool's `background: true`). MCP stdio
launches (`mcp/servers/external.rs`) are the sanctioned exception; the rule
itself now lives in `CLAUDE.md` ("Host exec has one owner").

The defect is not the bypass — it's that `shell.rs:206-283` hand-mirrors
three policies kaish derives structurally (read-only refusal ←
`ExternalExec::Deny`; exec-authority gate ← `allow_external_commands`;
hermetic cwd/env ← `apply_context_config`), plus an `is_dir()` check
(`:255`) needed only because kaish's VFS resolution isn't in play. One
canonical owner, one silent copy, no mechanism keeping them in sync.

**Preserve across the swap** (all in `background_exec.rs`): live output into
the CRDT block (not buffered to completion); `Running`→`Done`/`Error` tied to
exit; `kill_all_for_context` (`:486`); process-group kill (`:583`); the
PDEATHSIG orphan guard (`:586` — `kill_on_drop` only covers a clean unwind);
output cap with a loud marker. Characterization tests come first.

**Blocked on three kaish changes** (worktree PR in flight,
`~/src/wt/kaish-jobs-embedder`):
1. A `JobManager` injection point — `Kernel::new`/`with_backend` hardcode
   `JobManager::new()`, so the per-call `EmbeddedKaish` loses every job.
2. `execute_background` must *forward* output it already captures.
   `try_execute_external` drains stdout per 8 KiB via `drain_to_stream`
   (`scheduler/stream.rs:223`); `execute_background` writes only the
   aggregate at exit, so `/v/jobs/{id}/stdout` reads empty mid-run.
3. PDEATHSIG — kaish has none anywhere (`setpgid` + pidfd + `kill_on_drop`
   only), so migrating as-is would silently drop the guard that covers
   `kill -9` and the `kaijutsu-runner.sh` restart loop.

**Functional gate exists (2026-08-09):** `contrib/isotest` (docs/isotest.md)
runs the PDEATHSIG orphan guard, process-group kill, restart hygiene, and
client-disconnect contract against the real binary in a podman PID
namespace — mutation-verified RED without PDEATHSIG. The swap must keep this
suite green; "PDEATHSIG isn't testable" is no longer true.

**Multi-tenancy stays ours.** `Job.session_id` is per-`JobManager`
(construction-time, output-file naming) — not a tenant key — so a shared
manager doesn't partition by context. Kaijutsu keeps a `ContextId`→`JobId`
index rather than asking kaish to enforce ownership; consistent with the
shared-trust model and with how `BackgroundRegistry` already works.

## kaijutsu on brak: fleet coordination service (2026-08-09, via zorak session, Amy aware)

> **DEPRIORITIZED 2026-08-11 — Amy, leaning not ruling.** *"I'm torn about
> bringing kaijutsu kernel service over, brak is not a powerful dev machine.
> So kernel will be likely on zorak since it depends on Zorak's GPU anyways.
> At least for now. we'll push binaries around later."*
>
> The load-bearing reason is the **GPU dependency**, not brak's size: the
> kernel wants local models (embeddings / semantic index), so one-kernel-on-
> zorak follows from where the GPU is. Keep that framing — "brak is a small
> box" invites someone to reopen this the moment they see a bigger small box.
>
> **brak already participates, as a client.** It runs Claude Code in screen
> with `kaijutsu-mcp` against zorak's kernel — so fleet participation was
> never gated on a kernel deployment there. The entry below (and the
> launcher-unit gap under it) was written assuming otherwise.
>
> Everything below stays on record because the *shape* is still the blessed
> one (ONE kernel, many tailnet clients) and the deployment gaps are real
> whenever a kernel does land on a box that isn't zorak. Nothing here is
> active work right now. Also parked with it: **does a kernel that is purely
> a coordinator need provider API keys at all?** — moot while the kernel
> lives on zorak, live again the moment one doesn't.

Fleet direction from tonight's planning: brak (always-on N100 on UPS,
tailnet 100.113.35.53) should eventually run the kaijutsu kernel as the
fleet's coordination service — machine sessions on zorak/brak/moltar talk
via shared CRDT docs + invoke_peer instead of the current
sftp-handoff-mail protocol. brak is NOT a build box: zorak builds and
syncs binaries. What kaijutsu owes when this gets picked up: a server
build/config profile for a small x86_64 box (release build, modest
memory, no GPU/local-model assumptions), reachable over tailnet from
Claude Code sessions on all three machines. Jam doc:
~/src/zorak/docs/plans/cybernetic-infra.md (Q2).

**Correction (2026-08-11, read from code).** This entry used to claim
"isotest proved fresh-$HOME boot with generated host keys + add-key
provisioning and no host state" and named contrib/install-systemd.sh +
kaijutsu-server.service as "the unit story". Both readings were too
strong, and work was being scoped against them:

- **isotest does not prove zero-host-state boot.** `docs/isotest.md:3-8`
  scopes the harness to *process-lifecycle* guarantees (PDEATHSIG orphan
  guard, process-group kills, restart hygiene), and every assertion at
  `docs/isotest.md:61-83` is a lifecycle assertion. Fresh-$HOME is a
  **precondition demonstrated by construction, not an asserted
  invariant** — nothing fails if it regresses. It is already false twice,
  benignly: the embedded mcp.toml reaches for `bevy_brp_mcp` and a
  hardcoded kaibo path (`docs/isotest.md:95-96`), and the suite runs
  `--network=none`, loopback SSH only (`docs/isotest.md:19-20, 43-47`).
  **Tailnet reachability — the whole point of brak-as-coordinator — has
  never been exercised.** A green isotest says nothing about it.
- **Therefore the brak cold boot is not gated on isotest.** It is gated
  on a unit that does not exist (below).

Amendments (same evening, zorak session): fleet binary distribution goes
through **halfremembered-launcher** (zorak + github; Amy: "may need
sprucing") — superseding Gitea-Actions-registry and distcc/sccache ideas —
so kaijutsu-server binaries reach brak via launcher sync from zorak's
builds. And brak is dropping MinIO (license): assume NO object storage on
the fleet coordinator.

**CRDT-record note**: this plan is ONE kernel with many tailnet clients —
the one-body model stretched across machines, not multi-kernel
federation. It therefore *supports* the option-2 verdict in
docs/crdt-position-2026-08.md rather than triggering its federation
escape hatch; empirical question 2 should be read with this in evidence.

### The launcher-shaped unit does not exist (2026-08-11)

Neither shipped unit can run on brak, and the gap is structural rather
than a config tweak:

- `contrib/install-systemd.sh` runs `cargo build --release` and sets
  `WorkingDirectory=$REPO_DIR` — it needs a git checkout *and* a Rust
  toolchain on the target, contradicting this entry's own "brak is NOT a
  build box". Do not run it there.
- `contrib/kaijutsu-server.service` is a dev unit: hardcoded
  `target/debug/`, a `/bin/bash -c` that reads `~/.anthropic-key.txt` and
  `~/.deepseek-key`, and `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317`
  which will not exist on brak. Cold-booting it tests a fiction.

Owed: a unit that runs a launcher-managed binary path and nothing else —
no repo, no toolchain, no checkout-dependent `WorkingDirectory`,
`EnvironmentFile=` instead of inline key reads (and a decision on whether
a fleet coordinator needs provider keys at all — **Amy's call**), OTLP
optional. **zorak lead owns the draft**, in zorak's `systemd/` where
units-before-install is the convention; it moves here on Amy's word. The
distribution half is ready: halfremembered-launcher has atomic install +
versioned deploys with rollback (branch `atomic-install`, unpushed
pending Amy).

Three things that will bite whoever does this:

- **`loginctl enable-linger` appears nowhere in this repo.** A `--user`
  unit without linger does not start at boot and dies on logout. On an
  always-on headless N100 that presents as "the binary is broken".
- **Provisioning is a mandatory pre-boot step.** The shipped binary has
  `allow_anonymous: false` and no registration RPC
  (`docs/isotest.md:52-54`) — without `add-key` first you get a running
  server nobody can reach.
- **glibc skew is asymmetric and trends the wrong way.** brak is Arch
  like zorak, so this is version skew, not distro mismatch — but glibc
  symbol versioning is one-directional: a binary built against a *newer*
  glibc fails on an *older* one, never the reverse. zorak is the daily
  driver and brak is an appliance nobody logs into, so zorak leading is
  the default trajectory, not the unlucky case. isotest carries an `ldd`
  preflight (`docs/isotest.md:34-37`) because this is real; the launcher
  sync path has none. Filed with zorak lead as a launcher feature
  request: run the preflight *on the target, pre-activation* (the seam
  atomic install creates), stamp build-time glibc into deploy metadata,
  and abort-and-report rather than auto-rollback — a silent revert to an
  older binary hides the actual problem.

## opencode support — shared lane with kaibo (2026-08-09, via kaibo session relay)

Amy (later this week): "easy opencode go support and some testing" — she's
picking up an account — and it "might be shared with kaijutsu, both tools
could use it well." Contract with the kaibo session: the opencode-as-MCP-host
measurements (instruction/description truncation limits, deferred-schema
behavior, resource ambience — the numbers kaibo already has for Claude
Code/Desktop) and the mcp-add config pattern get done ONCE and shared;
coordinate at pickup so nobody measures twice. Kaijutsu angle: opencode is
another vendor harness for the code-enabled-player pattern
(docs/python-player.md) — same per-vendor policy read applies before wiring
its subscription. Recorded in kaibo's signoff open threads as shared work.

## `drift --drive`: kaijutsu messaging live Claude Code sessions (2026-08-14)

**Design + measured protocol moved to `docs/cc-peer.md`** — that is canonical,
with per-claim provenance (probed / read-from-binary / inferred). This entry
keeps only what is not in the code yet.

Amy's framing: *"like a drift with `--drive`"* — drift already means inject into
a mailbox, so `--drive` is a sink, not a subsystem. Spends neither seat nor API
credit: pure local IPC, her sessions burn her seat because she started them
herself in a mux.

**Shipped on branch `cc-peer-roster` (worktree `~/src/wt/kj-cc-roster`), not
merged:** `kj cc list`, `kj cc send [--dry-run]`, plus the two pieces that
followed on 2026-08-16 —

- **`crates/claude-code-peer`** — protocol-only crate, no `kaijutsu-*` deps
  (`approval-ledger` precedent): registry scan that never touches `.key`
  files, the `procStart` PID-reuse guard (alive/stale/gone/**unknown**), the
  attribution envelope with the receiver's exact grammar + canonicalization
  check, a byte-stable frame codec matched to a live capture, a no-ack
  sending client, and a tokio inbox listener (0700 dir / 0600 socket,
  unauthenticated-input posture). 58 crate tests + 2 ignored live probes;
  golden fixtures from a real CC 2.1.233 session.
- **The approval-ledger gate** — `kj cc send` is ledger-gated (Amy,
  2026-08-16: *"yeah kj cc send should go through the ledger"*). Durable ask
  row before any wait, fail-closed on `gate_wait_timeout`, answered via the
  new `kj approve list|show|allow|deny` verbs. The message body is a FREE
  variable in the gated statement, so the ledger's guarantee 3 makes
  allow-always rules structurally impossible — every send stays
  human-approved. `--dry-run` is exempt.

**Not built yet:**

- **Inbox** — kaijutsu binds its own socket and becomes a reply target. Proven
  viable: `SendMessage` delivers to an arbitrary `uds:` path with no registry
  entry, so no squatting in `~/.claude/sessions/` is needed. The listener half
  now exists in `claude-code-peer::server::Inbox`; what's missing is wiring it
  into the kernel as a drift/mailbox source.
- **Full outbound frame** — a truthful `from` (emit it *only* once we are
  listening).
- **Presence at `/run/cc`** — build as a *source* for Amy's general live roster,
  not a CC-specific store (see `signoff.md`).
- **Per-peer inbox paths + kernel-stamped principals** — turns an
  unauthenticated channel into a capability-authenticated one.
- **Hook registration** — consent + session↔context mapping; also closes the
  transcript-scraping identity bug (`CLAUDE_CODE_SESSION_ID`).
- **Fan-out** — last. The ledger gate is the precondition it needed (below).

**`kj cc` is scaffolding (Amy):** *"kj cc is a temporary thing"* — hooks give us
contexts, then drift works on a CC session like any other and the verb retires.
Amendments: the introspection melts into the VFS rather than vanishing (a
context exists only for a session that opted in; the registry sees every
session, which is what you need when one *isn't* wired up), and a CC context
**cannot be clocked** — `--drive` is deliver-and-clock natively but
deliver-and-hope here. Surface that in the UI rather than let it read as a bug.

**Ledger gating — RESOLVED (Amy, 2026-08-16: *"yeah kj cc send should go
through the ledger"*).** `kj cc send` now routes through the approval ledger
(see the shipped list above); the open question this entry used to park is
answered. The specific hazard it named — an actor that can inject turns into
every agent on the box — is what made the gate a precondition for fan-out,
and the free-variable statement keeps even a "remember always" answer from
learning an allow rule. Standing safety fact retained: inbound peer messages
cannot approve a pending prompt, change config, or run slash commands, so a
peer message cannot launder a permission decision past a session's own gate.

## Pythonic player: kaijutsu-py wheel (2026-08-09, Amy: "shape B — the pythonic player")

### Codex app-server backend follow-through (phase 0 shipped 2026-08-14)

The connect-only, read-only protocol checkpoint and target design are in
`docs/codex-app-backend.md`. Amy authorized one kernel-managed sidecar; next
is its central lifecycle owner, then durable context↔Codex thread identity,
context/cwd/trigger BlockId carriage, token usage/interrupt, and a dynamic-tool
bridge over the broker. Disable Codex's native execution tools; Codex shell
calls should land on the same EmbeddedKaish-backed tool as every other model.

New crate `crates/kaijutsu-py`: cdylib built by maturin, pyo3 isolated to this
one crate, wrapping `kaijutsu-client`'s ActorHandle into a `kaijutsu` Python
package — any Python process becomes a first-class player. Serves three
lanes: (1) vendor agent harnesses client-side — **lane 1's justification is
falsified, see below**; (2) notebook/science/MIDI players; (3) an experiment space for python
sandboxes / uv venvs for agent-callable execution — containment via the
isotest podman harness, and it must compose with the kaish exec-ownership
rule, never bypass it. Design principle: players fat in capability, thin in
derived state (the kernel is the head). Second voices in flight: gemini-pro
deliberate (batch, durable handle `gemini/batches/pn8p3vcg5faecoeekb8a95s50cm9cb7mkrh8`)
+ deepseek consult; melt results into a design doc before building.

**Status 2026-08-14: still zero code, and lane 1 lost its reason.** The
design is melted into `docs/python-player.md` (reviewed, both verdicts
folded). Two things changed today:

- **The subscription justification is falsified.** The Claude Agent SDK
  authenticates from `ANTHROPIC_API_KEY`, does not inherit a logged-in
  `claude` CLI's credentials, and its overview explicitly disallows offering
  claude.ai login/rate limits through it absent prior approval. Managed
  Agents is API-metered too. So pyo3 buys nothing for seat spend; the
  subscription surface is the vendor's own harness under Amy's login, which
  is a *process* and already reachable over kaijutsu-mcp. The wheel must be
  judged on lanes 2 and 3 alone. Details + the two-directions split (harness
  drives kaijutsu, vs kaijutsu drives harness) in `docs/python-player.md`.
- **The go/no-go decision fell out of the fleet queue, at a step two
  migrations earlier than first reported.** Last seen at
  `~/exomemory/daily/2026-08-11.md:39`, verbatim: *"5. Wheel slice 1 go +
  first consumer (agent-SDK seat vs notebook)"* — in that daily's inline
  queue. It appears in **no** later exomemory revision. The 08-11 → 08-12
  daily hand-copy is where it died: 08-12 carries a ~20-item inline queue
  with no wheel item, its closeout compressed those to 8, and the 08-13
  `queue.md` rebirth then carried those 8 faithfully (items 1–8 identical,
  item 5 = memory-pressure backstop in both). Two wrong attributions were
  filed before the right one — the rebirth (mine, from misremembering the
  file) and the closeout compression (the kaijutsu lane's) — so if this is
  ever audited, **audit the daily-to-daily hand-copy**, which is precisely
  the mechanism `queue.md`'s own birth note names as the rot it exists to
  stop. The queue was right about its own hazard. The decision now lives
  here and in python-player.md "Open" only. **Awaiting Amy:** first
  consumer (notebook/MIDI player vs a second vendor seat over MCP/ACP,
  which needs no wheel), and whether direction (B) gets a policy read.

## kaish-help and kaish-kernel must be bumped together (2026-08-07, adoption)

Now that prompts are composed from `kaish-help`, the guidance kaijutsu ships
describes the shell it *runs* — so the two pins have to move as a pair:

- `kaish-kernel` (crates.io "0.13") is the shell that actually executes.
- `kaish-help` (git rev, pending release) is the prose describing it.

They agree today. They will not after kaish PR #300, which makes `,`
significant only inside `[]`/`{}` — the composed tool description still
carries the `comma-splits-word` rule (a `Concept::Foundations` fragment, and
Foundations is exactly what the tool description selects). Bump `kaish-help`
alone and models are told to quote commas the runtime now accepts; bump
`kaish-kernel` alone and they are not told about a rule that still bites.

Neither direction fails loudly — a prompt that mildly misdescribes the shell
produces worse agent behavior, not an error — which is what makes it worth
writing down. When the release lands, move both pins in one commit and drop
the git dep TODO in `Cargo.toml` at the same time.

## rc seed assets have no rebuild tracking (2026-08-07)

`assets/defaults/rc/` is embedded via `include_dir!`
(`kaijutsu-kernel/src/seed_scripts.rs:54`) and `kaijutsu-kernel` has no
`build.rs`, so nothing emits `cargo:rerun-if-changed` for the seed tree.
Editing a stance or lifecycle script may not trigger a rebuild, and a test
run can silently exercise a stale embedded copy. Bit us during stance tuning:
a mutation test looked green until `strings` on the test binary proved which
version was actually compiled in. Wants a small `build.rs` walking the asset
dir. Until then, verify with:

    strings -a target/debug/deps/<test-binary> | grep -o "<a distinctive string>"

## Ambient command center — trace packets, switchboard follow-ups (2026-08-10)

The arc: Amy runs the app all day on a side monitor; the room scene grows
ambient signal surfaces (switchboard shipped, seats in flight). Backlog:

- **Trace-packet / comet system (concepted, Amy liked it, not built).** Real
  events inject traveling crests on the engraved floor routes — the
  `ChordMaterial` trick (CPU writes ONE launch timestamp, shader derives
  crest position from `globals.time` forever): extend `TraceGlowMaterial`
  with a mode-2 pulse-slot array (4 slots/route, round-robin). Traffic map:
  MIDI-in → crimson inbound W; DJ render → crimson outbound E; VfsActivity →
  green N; PCM → cyan N; **TurnCompleted → gold comet** (bigger, tailed,
  1.2–1.8s hash-jittered transit) seat/S → well, landing as a
  `WellRingsMaterial` ripple at its arrival angle + throat bump;
  **TurnFailed → red comet that lodges** at the terminal pad until the
  context recovers. Token-bucket per route (~150ms min spacing, drop excess
  — midi.md doctrine: missed is missed, never replay); crest position from
  emission wallclock, back-dated, never chased. Needs a `RouteRegistry`
  (bearing/hue → material handles; producers call `inject(bearing, class)`).
- **Switchboard placement**: the south wall is BEHIND the default room
  camera — invisible in exactly the ambient framing Amy watches. Decide:
  move lamps to a visible diagonal (NE, where unbuilt Radiators sits),
  mirror a compact strip into the north-facing frame, or change the default
  camera. Amy's call.
- **Switchboard polish**: recency dynamic range too narrow (stale lamps
  still mid-khaki — widen so old falls near-dark; one constant, tune with
  eyes on the real monitor). Panel lost its nameplate with the furniture
  flip — if a label is wanted, engrave a title on the panel itself
  (tracker transport-glyph style). Ember is unit-tested but not yet
  live-verified (needs a real TurnFailed; stage one deliberately).
- **Switchboard slow leak** (kaibo/deepseek): a context deleted from the
  kernel while `sticky_error` is set leaks its `LampSignal` entry forever
  (never polled again → never cleared; `retain_relevant` keeps active
  signals). Rare; fix = drop signals for context ids absent from the poll.
- **Turn-comet / seat-flare enabler**: `TurnCompleted` carries
  `principal_id`, peers carry `nick` — no correlation exists. Wire a
  principal↔peer mapping (kernel knows both) so turn events can flare the
  right seat.
- **Runner cargo-watch is DEAF on moltar** (even freshly restarted, inotify
  limits fine): commits and touches under watched dirs trigger nothing;
  every rebuild needs `./contrib/kj restart`. Diagnose cargo-watch vs the
  kernel/watchexec version; consider watchexec-cli or a poll flag.
- **Ambience agent (Amy's idea)**: an agent context that "plays the room" —
  drifted-to like any context, modulating packet gain / route weather /
  lighting mood. Needs the packet system first; keep in mind as its API
  shapes up.

---

## Peer-registry doctrine — headless clients still need to attach; `PeerInfo` lacks `instance` on the wire (2026-08-10, peers-plumbing)

Every connected client is supposed to register in the kernel's peer registry
so the app can render "who's at the table" (`docs/instrument-design.md`,
"Many hands, one trust boundary"). `kaijutsu-mcp`'s `register_session` now
does this (`crates/kaijutsu-mcp/src/lib.rs`, the `peer_nick_for_label` +
attach block right after `finish_join` returns) — nick `mcp/<label>`,
instance a per-process UUID mirroring the app's `app_peer_instance()`,
invocations drained with a graceful "unsupported action" reply since no peer
actions are implemented on the MCP side yet. Use that as the pattern.

`kaijutsu-acp` now retains `initialize.clientInfo` and registers one peer per
ACP process as `acp/<client-name>`. It deliberately does not register once per
session: one ACP connection can host several contexts. The actor replays the
registration after reconnect and SSH connection teardown removes it.

Still needed:
- **The upcoming headless client** should attach on connect, same pattern.
- **The MCP peer nick goes stale on label stabilization.** An auto-registered
  session attaches under its placeholder label, then
  `stabilize_context_label` re-joins the same process under `{base}-{sid8}`
  via `finish_join` — but the attach lives in `register_session_impl`, not in
  `finish_join`, so the registry keeps `mcp/<placeholder>`. Attaching inside
  `finish_join` is not the fix as-is: (nick, instance) is the upsert key, so
  a second attach under a different nick registers a *second* peer for one
  process instead of replacing the first. Wants either a detach-then-attach
  on rename, or a registry keyed on instance alone.

Also: the wire `PeerInfo` struct (kaijutsu.capnp) carries only `nick` +
`attachedAt` — no `instance`/kind field. `listPeers` output can't distinguish
two windows/sessions sharing a nick (multi-window app instances, or two MCP
sessions attached under the same label from different processes) from each
other; a caller rendering presence sees one nick, not N. Candidate future
schema addition: add `instance` (and maybe a `kind` string) to `PeerInfo` so
`listPeers` round-trips what the registry already tracks server-side
(`crates/kaijutsu-kernel/src/peers.rs`'s `PeerInfo` has both).

---

## Seats-at-the-table follow-ups: nameplate LOD, turn-event flare (2026-08-10, seats)

`view::room::seats` gives every attached peer (`connection::peers::PeerRoster`)
a wisp orbiting the well — no nameplates in v1, they'd be illegible clutter
at room scale (mission call). Two follow-ups noted rather than built:

- **LOD-gated nameplate on well-zoom.** A label that fades in only once the
  camera is close enough to read it (the well-zoom LOD the mission brief
  anticipated) — needs a "which wisp is nearest/under cursor" pick, not
  built here.
- **Turn-event flare needs principal↔peer correlation.** `ServerEvent::TurnCompleted`
  carries `principal_id`, not a peer nick — there's no join today between
  "which context turn just finished" and "which attached peer should flare."
  Until that correlation exists, a seat can't visibly react to its own turn
  completing/failing the way `switchboard`'s lamps do for contexts. Enabler:
  thread the acting peer's nick (or instance) onto the turn-completion event,
  or expose a context→peer lookup the seats reconcile loop can join against.

---

## Shell dock never draws a visual-selection highlight (noticed 2026-08-04, tier-2 app cleanup)

While unifying `build_overlay_glyphs`/`build_shell_dock_glyphs`
(`view/overlay.rs`, `view/shell_dock.rs`) into a shared
`sync_compose_text_glyphs`, found the two had already drifted: the overlay
palette tracks vim-mode `kind` and a selection anchor on its
`OverlayCursorGeometry` and draws a highlight rect for single-line visual
selection; the shell dock's `build_shell_dock_glyphs` never sets either
field, so `v`/visual-mode selection in the shell dock has no visual
feedback at all — only the cursor moves. Left as-is (behavior preservation,
not this cleanup's job to decide), but worth Amy confirming whether that's
deliberate (shell dock = one command line, selection matters less) or a
gap worth closing.

## Day-job coding readiness (2026-07-29, live-kernel probe + deepseek review)

Amy: *"I want to get kaijutsu's coding functionality up to a level I can use for
my day job."* Verified against a running kernel (probed via `kaijutsu-mcp
--connect`) plus source. The **tool surface is not the gap** — `builtin.file`'s
hashline anchors and CRDT-aware `grep` are ahead of Claude Code's equivalents.
The gap is **context economics**: nothing measures or bounds what a turn costs.

Complements the Gemini CLI comparison below (2026-06-23) — that pass found the
same web/background/ask_user holes from a different angle. Don't duplicate;
these are the ones that block *using* the thing.

**Tier 0 — small fixes.** *A1/A2 SHIPPED 2026-07-29 (`4ed99bd3`).*

- ~~**Redundant hardcoded tool timeout clamps the configurable one.**~~ **FIXED
  `4ed99bd3`.** `llm_stream.rs`'s `const TOOL_TIMEOUT_SECS: u64 = 120` wrapped a
  broker call that *already* enforced per-instance `policy.call_timeout`
  (`broker.rs:1372`), so the const won and no build over 2 min could run.
  Removed; `PolicyError::Timeout` now maps to the same `tool.timeout`
  ErrorPayload using the real `timeout_ms`. **`kj policy set` is now the sole,
  sufficient ceiling for a coder-context tool call** — verified: the LLM idle
  (30s) and request (300s) timeouts only bound the *token stream* (tool calls
  run strictly after it closes), kaish's own 1800s request timeout sits inside
  the broker's call, and `kaijutsu-mcp`'s 300s/600s shell cap
  (`kaijutsu-mcp/src/lib.rs:950`) governs the separate producer-seat path only.
- **CORRECTION to the original claim, recorded because it changes the
  priority.** This entry first read *"one `cargo build 2>&1` destroys the
  context"*. That is **wrong for the shell path**: kaish already truncates and
  spills. `OutputLimitConfig::agent()` is an **8 KiB cap, 1 KiB head, 512 B
  tail, `SpillMode::Disk`** (`kaish-kernel-0.13.0/src/output_limit.rs:27-33`),
  applied *inside* the broker's call — so the broker's `max_result_bytes` check
  essentially never fires for `shell`. The other builtins are self-bounded too
  (`read` 2000 lines / 2000 chars-per-line; `grep` 200 matches, skips >1 MB
  files; search tools take `max_matches`). The 64 MiB default was a weak
  backstop *behind* per-tool caps that were already doing the work — not the
  open floodgate this entry implied. It still wanted fixing (64 KiB now, and
  head+tail beats tail-drop since compiler errors live in the tail — both
  shipped in `4ed99bd3`), but rank it below token accounting, not beside it.
  The genuinely unbounded surface is `block_read` on a large block and external
  MCP results — and external MCP doesn't load at all today.
- **Token accounting — gauge SHIPPED, pre-flight budget still missing.**
  (Headline corrected 2026-08-03; it originally read "consumed by nothing",
  which `95e6664a` made stale.) The full gauge exists: per-turn usage
  SNAPSHOT persisted to `context_usage` (`llm_stream.rs:1347`, provider
  cache-token normalization + never-overwrite-with-absent guard),
  displayed by `kj context info` and `contextUsedPct` on the wire. What's
  still missing is anything that *acts* on it BEFORE a turn — see the
  hydration-budget bullet below.
  - Denominator SHIPPED 2026-07-29 (`36f57547`) as hand-maintained per-model
    `context_window` in models.toml, resolving to `Option<u64>` — never a
    fabricated default.
  - **Live Anthropic lookup SHIPPED (feat/ctx-window-live).** Config still
    wins as an override (`LlmRegistry::context_window_for`, sync,
    config-only); when it has no entry, `context_window_for_live` (async)
    falls through to `GET /v1/models/{id}` for Anthropic providers only,
    reading the wire's `max_input_tokens` (NOT `max_tokens`, the output cap —
    the field-name confusion this shipped a regression test against).
    Non-Anthropic providers are untouched — additive, not a replacement.
    Cache lives on `claude::Client` (one per registered provider = one per
    kernel process): `Ok(_)` results (including `Ok(None)` — "the API
    doesn't know this model either") cache for the process lifetime, `Err`
    results (network/auth failure) are NOT cached so a transient outage
    heals on the next call. The HTTP call sits behind a
    `ModelCapabilitySource` trait seam (`llm/claude/models_api.rs`) so
    `cargo test --workspace` never touches the network — a
    `FakeModelCapabilitySource` is injected everywhere a test needs a
    `Provider::Claude` with an unconfigured model.
  - **Correction to the "closes the honest gap" claim above:** as of
    2026-08-02, `claude-sonnet-4-20250514` — the model backing the shipped
    `balanced`/`default` model aliases, and models.toml's canonical example
    of a deliberately-unset window — has been **retired**, not merely
    deprecated (`shared/model-migration.md`'s deprecation table gives its
    retirement date as June 15, 2026, now past). `GET /v1/models/claude-sonnet-4-20250514`
    404s for real (confirmed live against the API, not simulated); the live
    lookup correctly resolves it to `None` — same honest "unknown" config
    already gave it — because there is nothing left to look up. The
    mechanism is proven to close *real* gaps (verified live against
    `claude-sonnet-5`, absent from models.toml since it postdates the file,
    resolves to `Some(1_000_000)`); it just can't resurrect a retired model.
    **The more urgent thing this surfaced — shipped aliases pointing at a
    dead model — is FIXED 2026-08-02** (`ceea246b`): `default` →
    `deepseek-v4-pro`, `balanced` → `deepseek-v4-flash`, per Amy ("lean on
    deepseek for affordable experimentation and we can call on
    anthropic/others when we need their power"). Every Anthropic id in
    `models.toml` is now UNDATED, which is the durable lesson: both dead ids
    we found were dated snapshots. Windows were re-read live rather than
    trusted. `claude-opus-4-20250514` is also a 404, and kaibo's
    `list_models` — the right tool for this question — confirms Haiku 4.5 is
    still the newest Haiku and turned up `claude-sonnet-5` /
    `claude-opus-4-6`, which we were missing.
- **Hydration size warning SHIPPED 2026-08-03 as warn-but-send; a hard cap
  is rejected by design.** Amy chose warn-and-send over refusal: a
  `BlockKind::Trace` block + `log::warn!` when the pre-flight estimate
  (`estimate_tokens`, bytes/4 + flat 1600/image) hits ≥90% of the resolved
  window (`warn_if_near_context_window`, `llm_stream.rs`); unknown window =
  no check (never fabricate a denominator); the provider's own rejection
  stays the hard backstop, and the kernel never trims or refuses a turn
  (auto-compaction stays deliberately removed). Known simplifications, fine
  until they annoy: warns every over-threshold turn (no latch), estimate
  runs once per turn (pre-agentic-loop, so mid-turn tool-result growth is
  seen next turn).

**Tier 1 — needed daily**

- ~~**Background process management.**~~ **SHIPPED 2026-07-30.** `shell` takes
  `background: true`, spawning `/bin/sh -c` directly on the host (bypassing the
  per-call kaish materialization, which can't host a registry that outlives the
  call) and streaming into a CRDT block; a kernel-owned `BackgroundRegistry`
  (`background_exec.rs`) plus a `builtin.background` sibling server
  (`list_background_processes` / `read_background_output` /
  `kill_background_process`). Orphan-proof via `PR_SET_PDEATHSIG`.
  **Known gap, not worth closing here**: a child wedged in D state never dies on
  SIGKILL, so `child.wait()` blocks and its entry stays `Running` forever — the
  watchdog catches a panicking supervisor, not a hung one.
  **App-visibility slice SHIPPED 2026-07-30**: the app had zero visibility into
  this (a 10-minute `cargo build` was invisible until it finished) — closed via
  `BackgroundRegistry::summary_by_context` (kernel-derived per-context
  aggregate: running count, oldest-running start time, most-recently-finished
  outcome), five new `ContextHandleInfo` fields (`kaijutsu.capnp` @23-@27,
  `background*`), and a `background_jobs` dock badge
  (`kaijutsu-app/src/ui/dock.rs`) on the existing ~5s `DriftState` poll. Display
  only — killing a job stays a model-driven MCP tool call, no dock button.
  Left for later: no full SSH-level e2e test for the new wire fields (matches
  how `contextUsedPct` itself was tested — unit + wire-sentinel + capnp
  round-trip only, no client RPC verb exists to start a background job to
  exercise end-to-end); a hard-failure indicator is momentary (~5s poll, then
  the badge blends into the next state) with no "sticky until acknowledged"
  treatment. **Verified real (2026-07-30, feat/dock-errors merge): two
  freshness models sit pixels apart in the south dock, for the SAME event.**
  A background job's block (`background_exec.rs` inserts it `Status::Running`
  and later `blocks.set_status(..., Done/Error)` at the same call site that
  updates the registry, ~`background_exec.rs:691-699`) is one of the blocks
  `count_block_activity` counts — so `block_activity` reflects a background
  job's start/finish the instant the CRDT op broadcasts, sub-second, no
  polling. `background_jobs` reads the *same* start/finish only through
  `BackgroundRegistry::summary_by_context` via the 5s-throttled `DriftState`
  poll (`ui/drift.rs:16`, `poll_drift_state`). Both badges can visibly
  disagree about whether "something is running" for up to 5s at both the
  start and the end of the identical job. Not a bug in either badge — each is
  internally correct for its own data path — but a real, correlatable skew a
  user watching a long build could notice. Closing it means giving background
  job transitions a push path (a `ServerEvent` variant, mirroring
  `BlockStatusChanged`) rather than polling; not done here since it's new
  scope, not a merge fix.
  **CORRECTED 2026-08-01 by the or-kimi review** (run after the branch merged;
  findings verified against the code before recording): the 5s skew *is* only
  cosmetic and self-healing, exactly because the kernel writes the block status
  and the registry entry in the same `match final_status` arm — the next poll
  always sees a matching terminal state. But the two badges measure genuinely
  different things (visible CRDT block status vs. OS process registry lifetime),
  and there are three ways they diverge **permanently**, which the entry above
  understated:
  1. **Kernel restart.** `BackgroundRegistry` is in-memory, so a restarted
     kernel reports zero background processes while the persisted CRDT document
     may still hold `Running` blocks — `background_jobs` goes idle,
     `block_activity` keeps counting. Non-transient.
  2. **`set_status` fails while the registry update succeeds** (same call site;
     e.g. the document was removed). Registry says exited, block stays
     `Running`.
  3. **Block excluded or deleted while the process runs.** `block_activity`
     skips excluded blocks and never sees deleted ones; the registry keeps
     tracking the live process.
  A push path fixes the 5s skew but NOT these — they need the two views
  reconciled at their source, or the badges labelled as measuring different
  things.
- **`background_jobs` conflates "no data yet" with "nothing running"**
  (2026-08-01, or-kimi review, verified). `format_background_activity`
  (`ui/dock.rs:1870-1920`) returns an empty string both when the first poll
  hasn't landed and when the context genuinely has nothing running and nothing
  finished — the badge just hides. The "never fabricate a zero" rule is
  honored (it never shows a fake `0 running`), but unknown and known-idle are
  still the same pixels. `DriftState.loaded` already exists and is maintained
  (`ui/drift.rs:154,160`) and is **never read anywhere in `ui/`** — gating the
  empty state on it and rendering a placeholder until the first successful poll
  is the fix. Same class as the `contextUsedPct` `-1.0` sentinel work, one
  surface further out.
- **Dock papercuts from the dock-errors/bg-jobs merge** (2026-08-01, or-kimi
  review, all three verified against the code):
  - **`agent_activity` is a dead badge.** Declared (`ui/dock.rs:108`),
    defaulted (`:160`), and drawn (`:635-642`) — and **no system anywhere
    writes it**. Harmless (the draw is guarded on non-empty) but it is a
    leftover from an earlier branch; delete it.
  - **Draw-order overlap hazard in `render_south_dock`.** The right group
    (`context_usage`, `hints`) is positioned and drawn FIRST, then the middle
    area (`block_activity`, `background_jobs`, context badges) is drawn after
    it. Right-alignment means no overlap at normal widths, but a middle area
    wide enough to reach `usage_x` will overdraw the right group rather than
    being clipped. Compute the middle area first, or clamp it so it cannot
    reach. The layout comment at `:537` is also stale — it names `activity`
    and `block_activity` and omits `background_jobs`.
  - **Badge colors go stale across a theme change.** `update_background_jobs`
    and `update_block_activity` run only on `DriftState`/`DocumentCache`
    change, but they are what compute the badge color; `render_south_dock`
    rebuilds on theme change and uses the color already stored in `DockState`.
    Switch themes without touching the data and the badge keeps its old color
    until the next poll.
- **`poll_drift_state`'s "prevent stacking" comment overpromises**
  (2026-08-01, or-kimi review, verified at `ui/drift.rs:96-112`). `last_poll`
  is set before the async task spawns, which debounces the fast path but does
  NOT prevent concurrent polls: a call slower than the 5s interval lets another
  spawn, and with `RPC_CALL_TIMEOUT` at 30s that allows up to ~6 overlapping
  `list_contexts` tasks. Nothing leaks (each completes or times out) and the
  no-actor early-return means nothing spawns after disconnect — so this is a
  stampede risk against a struggling kernel, not a correctness bug. Either hold
  an `AbortHandle` and abort the previous poll, or set `last_poll` on
  completion; at minimum fix the comment, which currently claims a guarantee
  the code does not provide.
- **External MCP servers don't load at all** — see the dedicated *MCP subsystem*
  section immediately below. This also closes the "BYO a scraper MCP" escape
  hatch for the missing web tools.

## TurnEvents + register upsert — deepseek review findings (2026-08-04/05, post-merge) — P2 tier SHIPPED

DeepSeek V4 pass (dpal, whole files, no diff) over the merged foundations at
`29529ded`. Eight keepers explicitly endorsed (single-terminal-publish,
origin-on-the-event, beat's three Act guards, resolveContextLabel honesty,
loud attach warning, subject⊆TOPICS gates, unknown-enumerant hard error,
hasOutputBlock zeroing). All six P2 findings landed 2026-08-05:

1. `⛔ Interrupted` marker → `(Role::System, BlockKind::Text)` + ephemeral,
   no longer folds into assistant_text on the model's next turn
   (`llm_stream.rs`). Checked the review's `BlockKind::Notification`
   alternative against `hydrate.rs` before picking — Notification is NOT
   hydration-skipped (it formats into a *user* message,
   `format_notification_for_llm`), which would have been worse.
2. Post-cancel drain's idle timeout no longer constructs `StreamEvent::Error`
   on a hung provider — breaks the loop with `stream_cancelled` already set,
   so the outcome stays `Cancelled`, not `Failed` (`llm_stream.rs`).
3. `refusal`/`stop_sequence` now log distinctly (`warn`/`info`) before
   falling through to `EndTurn` (`llm_stream.rs`). Still open: a dedicated
   wire `Refusal` stop reason — a capnp change, and capnp is owned by a
   parallel lane right now.
4. beat.rs's OODA-Act gate is now `BeatScheduler::turn_should_crystallize`,
   with the soft-cancel-crystallizes / hard-cancel-skips decision stated
   explicitly and pinned by tests, replacing a comment that explained the
   hard-cancel exclusion but left the soft-cancel inclusion unstated.
5. `register_session`'s suffix TOCTOU (concurrent registers racing one
   concluded label) now retries on a real label-conflict, bounded at 5
   attempts (`kaijutsu-mcp/src/lib.rs`).
6. `flows.rs`'s `TurnFlow` doc no longer claims the block log covers a missed
   push — true for the text a turn wrote, false for `TurnStopReason` (no
   block-log shadow; `EndTurn`/`MaxTokens`/soft-cancel all leave an identical
   `Done` block). Points at the already-tracked bus catch-up story below
   ("TurnFlow bus lossy + in-memory") instead.

**Two review claims turned out wrong on inspection** (worth recording — this
repo has now caught this reviewer wrong more than once): the
`BlockKind::Notification` hydration-skip claim in (1) above, and the
"wire e2e for onTurnFailed" test-gap — `subscriptions.rs`'s
`turn_events_tests::failed_round_trips` already exercised `on_turn_failed`
through the real generated capnp client/server pair, and predates the
review (landed in `0564b334`, an ancestor of the reviewed `29529ded`). The
remaining test-gap item (concurrent register race) got the review's own
fallback: `kaijutsu-mcp`'s retry classification is unit-tested directly
rather than raced end-to-end, since this crate's RPC path needs a live SSH
connection to a `kaijutsu-server` and the project steers `--lib` runs away
from that harness.

P3 tier, recorded not urgent: subscribe-happens-in-spawned-task window
(`rpc.rs:3522-3633` — subscribe synchronously or document ordering);
stream-start retry backoff ignores a pending hard cancel (`llm_stream.rs:
1300-1320`); no turnId/endedAt on onTurnCompleted (correlation by ordering
only — matters for a stateless ACP frontend, revisit with the adapter);
suffix-orphan residue on crash between create and join;
`publish_with_sender` drops silently where `publish` warns (`flows.rs:
664-680`); archived-context re-listing via joinContext heal is documented
but surprising (heal re-registers without clearing `archived_at`).

## Coder stance tuning — proportionality + kaish primer (Amy, 2026-08-05, first toad day)

Amy, watching a fresh ACP coder context: *"wow that's a lot of work for a
simple question, we might have work to do on the coder prompt soon."* The
exhibit: a simple repo question triggered a 63k-token, 160-block recon tour
(12+ agentic iterations), and the model repeatedly fought kaish syntax it
should have been told about (piping after `done`, `[` banned, quoting
rules — several [exit 2] tool errors it then reasoned around, stretching
the loop). Threads to pull, together or separately:

- **Proportionality**: the stance should ask for answer-first behavior —
  explore only as much as the question warrants; a fresh context need not
  map the whole world before speaking. Consider a light tool budget nudge.
- **kaish quirks primer in the stance** (or a `/etc/rc` .md slot): the
  live models rediscover the same parse rules every session. The stance
  is CRDT-seeded June-era content; kaish is at 0.13 — audit for staleness
  while in there (`kj rc` surface, embedded defaults are only the seed).
- **Per-surface stance**: ACP/toad sessions may want a snappier stance
  than the desk coder seat — ties into the client-identity-preset seed
  (ACP entry) and the pending ACP-cast decision (flash + thinking dialed
  down vs house pro).

## At-rest schema-evolution follow-ups (2026-08-05, the task_status boot flood)

The `39326e7c` postmortem seeds, recorded not yet built. Contributing
factors: a new field on an at-rest struct (`BlockHeader`) without
`#[serde(default)]`; nothing in tests exercised decode-old-bytes; the
kernel had not been restarted across the merge so the breakage sat latent
until the next bounce; rc-read failure degraded silently into
deny-by-default.

- **Decode-from-old-bytes guard, systematically.** The new
  `cbor_without_fields` pin covers the two fields that broke. The general
  guard is a corpus test: serialize every at-rest struct (SyncPayload,
  snapshots, oplog entries) at each historical shape — or strip
  fields-newer-than-N — and assert decode. Cheap version: a checked-in
  CBOR fixture of a real pre-task oplog entry, decoded in CI forever.
- **Oplog decode failure at boot is quiet relative to its blast radius.**
  Per-doc ERROR + skip is the right durability call (nothing truncated),
  but ~40 docs skipping should surface as ONE loud aggregate (count +
  first error) at boot end, and `kj status`-visible state — same
  loud-not-silent treatment external MCP failures got.
- **Broken-window contexts (2026-08-05 14:27–15:05) are unbound**: at
  least `2e1334a4` (this CC session's kaijutsu-mcp context) and
  `1c39e6ab` (`acp-kaijutsu-1785954617`, the first toad attempt). They
  need `kj context remove` from a bound context (app), then /mcp
  reconnect re-creates fresh + bound. Older contexts' bindings persisted
  in the DB and are believed fine — spot-check from the app.

## Household-agent arc — task blocks + harness steals (seeded 2026-08-04, gap-analysis session)

Amy is pointing kaijutsu at always-on household duty (daily task grooming,
proactive check-ins, chat access). We read the two flagship harnesses —
clones at `~/src/research/hermes-agent` + `~/src/research/QwenPaw` — and wrote
the comparison to `~/src/meadow-lab/docs/kaijutsu-gap-analysis.md`. kj already
wins model switching, semantic memory, OTel, and CRDT-native state; the gaps:

- **Task BlockKind + tool — SHIPPED 2026-08-04** (Amy: *"Task BlockKind and
  tool is a great idea"*). `BlockKind::Task` + a dedicated CRDT-synced
  `task_status`/`task_status_at` field (mirrors `content_type`'s exact
  mechanism — its own `TaskStatus` enum, not a reuse of the tool-execution-
  shaped `Status`) get multi-frontend task sync from the CRDT for free.
  Closes the "no task/plan state — compare TodoWrite" gap noted in the
  day-job entry above. `builtin.tasks` (`mcp/servers/tasks.rs`) exposes
  create/update/complete/cancel/list (open/done buckets); subtasks reuse
  the ordinary `parent_id` DAG edge. Hydration mirrors `BlockKind::Notification`
  (D-34): a task's creation/current-state is appended once per
  `ConversationMailbox`'s `seen`-keyed translate-once rule, so a later status
  edit never rewrites an already-cached message. Design note: `docs/tasks.md`.
  **Deferred** (kept narrow on purpose): app/kj-CLI rendering (a placeholder
  `[status] content` line covers `kaijutsu-app` for now), a `task_reparent`
  verb (no cheap "move to new parent" primitive exists yet — `move_block`
  only reorders siblings), and — the one real design gap — a companion
  `Notification` block auto-emitted when a task changes from OUTSIDE the
  model's own tool call (another principal grooming via the app), so an
  out-of-band change actually reaches a live conversation instead of only
  showing up at the next boundary re-hydrate.
- **EvictionIndex as an oplog view** (QwenPaw `scroll/eviction_index.py`, the
  best idea in either codebase): compressed-out history collapses into a
  tiered in-context "odometer" (capped blocks per tier, older tiers carry
  upward) so the model *knows what it forgot*, paired with one bounded
  read-only recall tool. Amy's shape for kj: *"should be simple in kaijutsu,
  we could have an RC script inject the generation counter, kaijutsu keeps
  the graph intact if we use fork to manage the contexts as they roll"* —
  rolling fork chain as the generations, RC-injected counter, index as a view
  over `context_edges` + oplog. No second store.
- **One channel, exactly one** — a new `invoke_peer` peer kind for an
  external chat channel (today only `app`/`mcp` exist). Field lesson: QwenPaw
  ships 17 channels, Hermes is mid-migration between two competing adapter
  systems; protocol quirks (Telegram UTF-16 chunking, Signal rate limiting)
  dominate the cost. Hermes' 4-method adapter ABC
  (connect/disconnect/send/get_chat_info) is the right size. Pick the
  channel the household actually uses; stop there.
- **Graduated trust, when access grows**: QwenPaw `governance/policy.py` is
  the reference — two-tier rules (immutable builtin + approval-generated),
  verdicts ALLOW/DENY/ASK/SANDBOX, ASK→approve→generalize to fight allowlist
  fatigue. Hermes is the cautionary tale (honest SECURITY.md: "the OS is the
  only boundary"; shipped the denylist default anyway). kj's shared-trust
  stance is right for Amy-only operation; channels + household input change
  the threat model — decide the posture explicitly at that point.
- Always-on hardening already tracked elsewhere in this file (MCP audit,
  `register_session` reconnect, hook self-lockout, WorkspaceGuard fail-open)
  graduates from papercut to blocker once the kernel runs unattended.
- Scheduling steals folded into **"Grooming tracks"** below — that entry is this arc's cron half.
- **ACP adapter (`kaijutsu-acp`) — the mobile shortcut** (researched
  2026-08-04). ACP v1 went stable 2026-06-24 (schema 1.20.0), governance
  moved off Zed to a neutral `agentclientprotocol` org, LSP-style. Mobile
  clients already exist (Happy iOS/Android/Web, Agmente, Ferngeist, Mobvibe)
  plus messaging bridges — an ACP server adapter shaped exactly like
  `kaijutsu-mcp --connect` (thin bridge: ACP session ↔ kj context, JSON-RPC
  stdio outside, Cap'n Proto inside) could put kj on Amy's phone before any
  custom app exists. Rust SDK is real: `agent-client-protocol` 2.0.0 on
  crates.io (SemVer, NOT protocol v2 — v2 is behind `unstable_protocol_v2`).
  **Build against v1; ACP2 is draft** (announced 2026-07-20, alpha schemas,
  explicit "gate behind feature flags", no GA date). v2 heads-up that suits
  us: `fs/*`+`terminal/*` methods are removed in favor of client-provided
  MCP servers — kj's MCP-first tool story is already on the right side of
  that migration. Remote transport (HTTP/WS) is still an Active RFD; our SSH
  `--connect` pattern sidesteps the wait.
  **Prototype landed 2026-08-05** on branch `acp-adapter` —
  `crates/kaijutsu-acp`, ACP v1 over stdio, `--connect` inward, ring 0 served
  as the session picker. Living record + the full mapping table + the manual
  smoke test: `docs/acp.md`. What it left open is below.

## ACP adapter follow-ups (2026-08-05, from building `kaijutsu-acp`)

Ordered roughly by how much they hurt. Full context in `docs/acp.md`, "The
adapter, as built".

- ~~**`session/request_permission` is stubbed to auto-allow**~~ **SHIPPED**
  — `HookAction::Ask` + `PermissionEvents` landed (gap #2, `232c99c9`), then
  the bridge itself was rewired to use it. `kaijutsu-client::ActorHandle`
  gained a kernel-wide permission-ask stream (`take_permission_asks`,
  re-armed best-effort on every reconnect, `actor.rs`'s `connect_handshake`
  step 3.7); `kaijutsu-acp`'s `.with_spawned` task
  (`permission::start_permission_pump`, `lib.rs::serve_stdio`) drains it,
  resolves `contextId` → ACP session via `rank::session_id_of` (no side
  table), and drives a real `session/request_permission` round trip. Fails
  closed on every path: no live session for the context, a client error, a
  client timeout (`PERMISSION_ASK_TIMEOUT`, mirrors the kernel's own
  30s default), or an unrecognised selected option. `AutoAllow`/
  `PermissionPolicy` deleted — nothing configures an opt-in bypass.
  Tests: `permission.rs`'s unit suite (option-kind mapping, empty-options
  synthesis, response mapping) plus `tests/permission_ask.rs`'s in-memory
  round trips (allow, deny, richer kernel options, no-session, client
  timeout, cancelled prompt).
- ~~**No catch-up after a resync.**~~ **SHIPPED 2026-08-05** after it ate a
  live answer on toad flight two (FlowBus lag mid-turn; the client rendered
  the tool call, then silence over a finished report). `resync` now keeps
  the mapper's high-water marks and re-observes the rebuilt doc — the sweep
  emits exactly the gap (unseen tails + unannounced tool patches), never a
  duplicate. The *TurnFlow* half (dropped completion events) got adapter-side
  lag recovery the same day (idle-poll → best-effort `end_turn`).
  **The kernel-side cause is now fixed** (below), so **the adapter's defensive
  sweeps — quiet-poll turn wait, trailing-edge pump resync — are dormant
  defence-in-depth and are candidates for removal** once a few real flights
  confirm the kernel never drops on them again. Leave them in place until
  then; delete them together, and only after checking the ACP logs show the
  sweeps firing zero times.
- **`onTurnCompleted` carries no turn id.** The adapter's prompt wait matches
  on `context_id` + `TurnOrigin::Interactive`; two interactive turns racing in
  one context would cross wires. This is the P3 "no turnId/endedAt … revisit
  with the adapter" item above, now with a caller asking for it.
- ~~**`BlockKind::Task` has no ACP shape.**~~ **SHIPPED 2026-08-05** on
  branch `acp-plan`. `BlockKind::Task` blocks rebuild into ACP v1's `plan`
  session update, whole-context, one non-cancelled task per `PlanEntry`.
  `UpdateMapper::note_task`/`build_plan` (`kaijutsu-acp/src/update.rs`) is
  the one rebuild-and-emit path, threaded through the live pump, `session/
  load` replay (exactly one plan at the end), `session/new` bootstrap
  (silent baseline), and the resync sweep. Decisions: cancelled tasks are
  omitted (not mapped to any `PlanEntryStatus` — a plan is "what the agent
  intends to do," and cancelling isn't intent); subtasks (`parent_id` DAG)
  flatten via pre-order DFS with a `"↳ "` nesting prefix on `content`;
  priority defaults to `Medium` (no kernel-side priority field exists, none
  invented). Full writeup: `docs/acp.md` "Task → plan". Found while
  building: `session::run_pump`'s `BlockDeleted` arm called `mapper.forget`
  and `continue`d WITHOUT ever calling `doc.apply_event(&event)` — the live
  `SyncedDocument` mirror never dropped a deleted block, only a resync
  rebuilt it away. Pre-existing, affected every block kind (not
  Task-specific). ~~Not fixed here~~ **fixed in `833f951c`** (2026-08-07):
  the arm now applies the event (honouring `NeedsResync`) before rebuilding
  the plan, pinned by `build_plan_re_emits_when_a_task_disappears` /
  `build_plan_stays_quiet_when_a_non_task_disappears` in `update.rs`.
- **Client-identity presets on connect** (Amy, 2026-08-05 evening, first
  toad day): ACP `initialize` carries `clientInfo` (Implementation
  name+version) and capabilities — enough to recognize *which* frontend
  connected. Wire that into the existing per-client config machinery: derive
  a ClientId from the client identity, let the `/etc/client` cascade
  (docs/config-crdt-ownership.md; metronome was the first consumer) and/or a
  preset/cast mapping key off it — "toad connections get preset X / cast Y,
  Happy gets Z." Today every ACP context gets the row-stamped default
  (ds-v4-flash) regardless of who connected; this is also where the pending
  ACP-cast decision could land generally instead of as a bridge hardcode.
- **Stable v1 methods left unimplemented**: `session/set_mode` (→
  `context_type` / cast roles) and `session/set_config_option`. Neither is
  advertised in capabilities, so clients should not call them.
- **ACP delete follow-ups.** `session/delete` archives first and only then
  unbinds/stops the pump, but its handler still flattens every archive failure
  to `resource_not_found`; preserve typed actor/RPC errors so transport and
  server failures can map honestly. Add a handler-level fake-kernel test that
  pins archive-before-unbind and failure-keeps-binding ordering once
  `KernelBridge` has a narrow injectable seam. Deleting during an in-flight
  `session/prompt` deliberately does not interrupt that turn yet; decide the
  prompt response/interrupt semantics from ACP client flights before adding
  another stop path.
- **Kernel-wide block subscription.** The bridge uses
  `scope_blocks_to_context: false` so several ACP sessions can stream at once,
  and filters per pump. That is the firehose kaijutsu-mcp deliberately scopes
  away from (the 2026-06-17 executor-starvation stall). If it bites, the fix
  is one actor per session, not a narrower filter. Less likely to bite now:
  the 2026-08-05 rework coalesces the text-op firehose at the forwarder, so
  the bridge sees roughly an order of magnitude fewer callbacks under
  streaming — and if it *does* fall behind, it is disconnected with a reason
  instead of quietly losing events.
- **Client-declared `mcpServers` are ignored** (warned once per
  `session/new`). Needs the unplumbed `external.rs` caller — acp.md gap #4.
- **`observe_tool_result`'s `changed_body` check is length-only, so a
  same-length content replacement on a `ToolResult` block (e.g. `boom` →
  `oops`) can be missed and the update dropped silently — the same shape of
  gap `take_delta` had before the 2026-08-15 fix (`update.rs`), just in a
  sibling method that was out of that fix's scope. Lower stakes here: this
  path always sends the *full* current body when it does fire (no
  suffix-slicing, so no corruption risk), and a status change on the same
  event usually forces a send anyway — but a same-length body change with an
  unchanged status is a real, if narrow, silent-staleness gap. Swap the
  length comparison for the same prefix-hash-style check `take_delta` now
  uses (or just always compare a content hash) if it's ever seen live.

## FlowBus backpressure — what the 2026-08-05 rework left open

The rework itself shipped (per-subscription bounded queues, lossless-or-
terminated, forwarder-side text-op coalescing, `subSeq` + the lag kick on the
wire). What it deliberately did NOT do:

- **The ACP adapter's defensive sweeps are still in.** Quiet-poll turn wait
  and trailing-edge pump resync (commits b8b9fe22, 35c4b5b9, 3960fad3,
  e31d6ddd) are now dormant defence-in-depth. Remove them together, after a
  few real flights show them firing zero times — not before.
- **No catch-up for a subscriber that wasn't there.** Losslessness is a
  promise to *live* subscribers only. See the TurnFlow catch-up item below.
- **Queue depth is one number for every subscription** (8192, via
  `KAIJUTSU_FLOW_QUEUE_DEPTH`). A GUI client and a headless MCP session get
  the same allowance. Per-class or per-principal depths are easy to add if a
  real workload ever wants them; nothing does yet.
- **The timing lane is untunable from config.** `block.render_cue` /
  `block.beat_sync` ride a fixed 64-deep drop-oldest ring. Deliberate — the
  doctrine is that a stale beat is worse than a missed one — but if a sink
  ever wants a different depth it needs a knob.
- **Only `slowSubscriber` is ever sent.** The wire enum also has
  `serverShutdown` and `superseded`; nothing emits them yet. A clean shutdown
  still looks to a client like an ordinary disconnect.

## LFM2.5 encoder family — routing, boundary guards, embedding swap (seeded 2026-08-03, Amy: "tempted to go deep on this model family for a while")

LiquidAI's LFM2.5 encoder branch is a small-model toolbox aimed at exactly
our seams. Surveyed 2026-08-03 (HF API, live):

- **`Encoder-350M-Prompt-Router`** — prompt→tier classifier. Our fit: the
  *dynamic* half of the routing doctrine — per-turn "flash or pro, effort
  high or none" feeding cast-lane choice, decided locally for free. Eval
  first: run a pile of real prompts from the block log through it and score
  its lane picks against Amy's.
- **`Encoder-350M-PII-Detector` / `-Policy-Linter`** — token-level boundary
  screening. Amy's framing: guards are for **mistake-protection and foreign
  content crossing the membrane** (new OSS repos, prose/code from elsewhere)
  — NOT inter-player policing, which stays off-doctrine. Strongest first
  home is **kaibo** (its whole job is reading untrusted repos): screen
  explorer file reads for injection, screen batch payloads for PII.
  Amy: *"an embedded LFM in Kaibo would kick so much butt for the safety
  elements."* (Kaibo-side work; tracked here as the cross-project seed.)
- **`Embedding-350M`** (1024-dim, safetensors + official GGUF) +
  **`ColBERT-350M`** — bge-small successor candidates. Embedder swap =
  full semantic-index rebuild (dims change; by design). ColBERT needs
  per-token vector storage — bigger lift; `bge-reranker-v2-m3` remains the
  cheap second-stage quality win meanwhile.
- **`Encoder-230M/350M` base** (fill-mask) — fine-tune substrate for our
  own future boundary classifiers.

**Runtimes (not stuck on ONNX — Amy), with a per-project split (Amy
2026-08-03): `llama-cpp-2` is fine in KAIJUTSU but NOT in kaibo — kaibo
stays pure-Rust/light-build; "candle might be cool there though." Kaibo
can wait.**

1. **Small encoders (router/guards/embedding): candle in-process, BOTH
   projects** (Amy 2026-08-03: "maybe candle here too"). Write the `lfm2`
   bidirectional-encoder + classifier-head implementation ONCE as its own
   small crate; kaijutsu consumes first, kaibo later. Keeps workspace
   builds pure-Rust (no cmake/C++ bolt-on next to Bevy), Mac-clean, and
   matches the rten in-process precedent. Caveat named: candle's AMD/
   Vulkan GPU story is weak and zorak is Strix Halo — but the encoder
   lane runs fine on CPU (bge-small already proves the shape; a 350M
   classify is tens of ms).

   **STARTED 2026-08-03: `~/src/candle-lfm2-encoder`** (own git repo, day-0
   commit `1f871e6`) — milestone 1 (config, fixture-verified, 7 tests) done;
   candle reference clone at `~/src/research/candle` (dual MIT/Apache, no
   CONTRIBUTING.md, no AI policy — upstreaming deferred until Amy reads at
   PR time). Fixture discoveries recorded in that repo's CLAUDE.md, headline:
   the PII detector's taxonomy includes credential.api_key/jwt/private_key —
   it's a SECRETS detector too.
   Searched first — the crate did NOT exist anywhere; scoping notes:
   Upstream candle-transformers already ships `lfm2.rs` +
   `quantized_lfm2.rs` — the CAUSAL branch, i.e. the hard hybrid blocks
   (gated short conv + GQA) are done. Nobody has the encoder branch:
   crates.io LFM2 hits are internals of other projects (candle-miotts TTS,
   bebelm 8B CPU, two VL runners), none reusable, none bidirectional.
   Project shape: adapt upstream blocks → drop causal mask
   (`Lfm2BidirectionalModel`) → map 2.5-encoder checkpoint weights →
   three heads in increments: sequence-classification (Router),
   token-classification (PII/Policy-Linter), pooled embedding
   (Embedding-350M; ColBERT later). Upstream-PR-shaped if it comes out
   clean — candle takes model contributions. Amy reads AI policies before
   we interact with any outside repo, per standing practice.
2. **Generation models: llama.cpp servers stay EXTERNAL** — kaijutsu
   already speaks to them as openai-kind backends; Vulkan works there.
   `llama-cpp-2` in-process is the fallback only if candle CPU perf
   disappoints or the arch port stalls.
3. **rten/ONNX** — only for index-pipeline symmetry; `lfm2` hybrid
   conv+attention arch is a real conversion risk; verify via embed_check.
4. LEAP (Liquid's edge SDK) — phones/edge, not our shape.

**Side quest (Amy 2026-08-03): fine-tuning LFM encoders for other things,
"like some music things."** The 230M/350M bases LoRA cheaply (moltar's
GPU ample; LiquidAI ships TRL-compatible fine-tune recipes). Music angle
stays symbolic per doctrine — the score is text (ABC, patterns), so
encoder fine-tunes can tag phrase style, classify patterns, or judge
groove-fit without audio ever riding the wire. Also the substrate for
our own boundary classifiers.

Deep-dive order when Amy picks this up: lfm2-encoder-in-candle crate +
Router eval offline (a scratch llama.cpp server is fine for the eval
itself) → Embedding-350M swap eval against bge-small on the real index
corpus → kaibo guard embed dropping in the shared crate → fine-tune
side quest.

## Cast follow-ups (seeded 2026-08-03, casts shipped same day — devlog "Contexts join a band")

Deferred from the renovation, in rough priority order:

- **Anthropic `output_config.effort` wire shape unverified live** — the house
  probe hit a billing wall (credit balance) before shape validation. Unit
  tests assert the documented shape; run one `kj drive` on a house/coder
  context after credits top up and confirm no 400.
- **capability-loadout consumer** for `cast_slots.loadout` (stored stub; the
  "cast can have domain capabilities" half of Amy's design).
- **`kj fork --cast`** — explicit fork-time cast override (inheritance +
  `--preset` cover today's need).
- **Responses wire for OpenAI** — only needed for streamed reasoning
  summaries; the chat wire with `max_completion_tokens` + `reasoning_effort`
  is live and sufficient.
- **Per-cast rc/stance placement** — does a cast ever carry stance text, or
  is that forever context_type's job?
- **App UI for casts** — kj-only today; the app compiles against the new wire
  fields but renders nothing cast-shaped yet.
- Deepseek-review P1, reviewed and ACCEPTED as policy: the rollover's
  fallback arm tosses provider=NULL+model=set rows to deepseek-v4-flash —
  that was Amy's instruction, and the live migration touched 0 rows anyway.
  Not a bug; recorded so nobody re-litigates it.
- Gemini review, reviewed and ACCEPTED as policy on three of its four
  findings (the fourth — a write-time inverted-budget WARN on `kj cast slot
  set` — shipped, see devlog "Contexts join a band"): DeepSeek
  `thinking:{"type":"disabled"}` claimed Anthropic-only → live probe
  2026-08-03 shows DeepSeek accepts it (kaibo had measured the same);
  rollover + preset narrowing are Amy's explicit policy; the "keys now in
  SQL" leak concern is structurally impossible (no key column exists).

## kaijutsu-mcp: workspace autoshare (seeded 2026-08-01, MCP-config session)

`--share` is app-only today (`kaijutsu-app --share`, `share_dial.rs` /
`share_server.rs` in kaijutsu-client) — kaijutsu-mcp has no share flag at
all, discovered wiring the Claude Code MCP config. Two pieces:

- **Port the share plumbing into kaijutsu-mcp** so an MCP client can offer
  reverse-SFTP shares like the app does (the client-side machinery in
  `kaijutsu-client` should be reusable; it's the same SSH connection).
- **Amy's ask: a nice *autoshare* of the workspace** — when kaijutsu-mcp
  connects from inside a project directory (the common Claude Code case),
  automatically offer the workspace (cwd or repo root) as a share, so the
  kernel side sees `/r/<client>/workspace` with zero flags. Needs the usual
  `/r` decisions: share name, ro vs `:rw` default, and an opt-out.

## Two live-log papercuts seen during the 2026-08-04 durability verification

Both pre-existing, both noticed while watching a live kernel; neither is a
correctness problem, both erode the value of WARN.

- **"Document already in DB but not in memory, recovering" fires on EVERY
  `kj context create`.** `insert_context_with_document` writes the `documents`
  row, then the create rc lifecycle calls `BlockStore::create_document`, which
  finds it — the benign duplicate arm, by construction, every single time.
  Since 2026-08-04 that arm *proves* benignity (kind/workspace/path all
  compared; anything else is now a `DocumentDiverged` error), so the surviving
  case is provably routine and probably wants `debug!`. Counter-argument for
  keeping it loud: it is also the signal that memory and DB disagreed, which
  matters on other paths (cache coherence). Amy's call — the noise is real
  either way.
- **Four backends warn `api_key_file configured but unreadable` at every
  start** (`gemma-26b`, `gemma-e4b`, `openai-local`, `sd`), all pointing at
  `/home/atobey/.openai-key`, which does not exist. They fall through to env
  and work, so this is config debt from the SQL-config seeding: either create
  the file, clear `api_key_file` on those rows, or teach the fall-through to
  log once at debug when the env key is present.
## Diff cursor stops at the ellipsis on an elided line (2026-08-04, slice 6A)

With column motions back, the drawn cursor and modalkit's column agree
everywhere except on a line long enough to be elided for display
(`MAX_VIEW_LINE_CHARS` = 2000 in the viewer, 500 inline). `text::diff::
cursor_byte` clamps to the end of the *shown* text, so `$` on a 5000-char
line parks the drawn cursor on the `…` while modalkit's real column is far
to its right. Bounded and rare (a minified bundle line), and the safe
direction — the alternative is a cursor drawn past the end of the text — but
it is the one place the viewer's "the cursor never lies" rule is approximate.
Fixes worth considering when someone hits it: elide in the *middle* of the
line so the tail stays addressable, or clamp modalkit's column to the shown
length on such rows (a real behavior change, so not done blind).

## Diff viewer footer + status bar overlap content rows (2026-08-03, slice-5 live verify)

Seen during the first live run of `Screen::Diff` on moltar, in a ~1080p-scale
window (the app had just survived a zorak hibernation + relaunch, so a window
scale change is in the mix): the viewer's diffstat footer strip ("diff, 1
file, +10 −6, 1/22") draws OVER the last visible content rows instead of
reserving a row for itself, the global status bar bleeds through beneath it,
and the conversation status bar's own left cluster self-overlaps ("40
fail3d.8k/1M Enter: submit"). Likely ONE bug, not three: overlay/footer text
placed with a stale or mixed logical/physical height after a scale change —
the exact `ComputedNode`-is-physical trap CLAUDE.md warns about
(`view::ui_rtt::logical_size`).

**Retested live 2026-08-03 after a clean reboot: the scale-change theory is
WRONG, and the bug is narrower than it looked.** At steady scale, on a fresh
boot, the status bar renders cleanly in `Conversation` and in `Room`
(`2 running, 40 failed  [db01a563]` … `31.8k/1M ↔: station | Enter/↓: zoom |
Esc: conversation` — well separated). It self-overlaps in the **time well**,
whose hint cluster is much longer: `40 fa1led/[db01a563]→↑↓: seat ⊙ ring |
Enter: focus/commit |c/p/d/z/a: act | Esc: room`. So this is not stale
HiDPI math after a resize — it is **status-bar segment layout overflowing
when the per-screen hint cluster is long**, with the left cluster and the
right cluster written into the same pixels instead of being measured against
the available width. Look at how the bar allocates width between the mode/
model/context cluster and the screen-hint cluster, not at `ui_rtt`.

The diffstat-footer-over-content half **CONFIRMED live 2026-08-04** (BRP
session, steady scale, fresh boot — so also not a scale artifact): with a
22-row diff open in `Screen::Diff`, the footer strip draws over the last
two content rows, and the conversation status bar renders THROUGH the
viewer's footer (both bars visible in the same pixels). Same
width-allocation family as the time-well overlap above: the footer does
not reserve a content row, and the underlying screen's bar is not
suppressed while the viewer owns the screen.

Everything else in the slice-5 checklist verified live today: `v` open (after
the DiffSurface resize-filter crash fix in `block_render.rs`), `]c`, `V`+`jj`
selection bands, `y` → Wayland clipboard (`wl-paste` exact), compose
`Ctrl+V` paste, `R` re-parse, `q` close. Still unverified: stale banner +
`R`-after-change, declared `ContentType::Diff` open path, parse-error banner
(all need kernel block edits — kaijutsu-mcp was stuck in local mode this
session; global `~/.claude.json` entry now carries `--connect`).

## Diff parse errors render as a generic banner, not line-anchored (2026-08-02)


From the slice-4 post-ship review (gemini deliberate, low severity): the
kernel attaches `DiffError::line()` spans to the ErrorPayload on `Done`
(`block_store.rs` `validate_diff`), but the app's diff error preview is a
generic banner — the line anchor is never pointed at visually. Inline line
annotation (squiggle/marker on the offending line) is high-value polish once
slice 5's full-screen error surface exists to render into. Small, app-side
only; the data already travels.

## Error-block-collapse remnants, post scaffolding removal (2026-08-01)

The unread `build_error_child_index`/`ErrorChildIndex`/`ExpandedErrorParents`
scaffolding (2026-07-30 finding) was ripped out — it was designed for a
"collapse an error's parent block to a stub with errors stacked below"
treatment that never got a reader, distinct from the dock indicator that
shipped and stays. Two related pieces of that unfinished feature are still
sitting inert and are a smaller, separate decision:

- `theme.block_error_accent` (`ui/theme.rs:131,435`, documented as the "stub
  strip" color) has no reader now that the stacking system is gone.
- `BlockSnapshot::system_error` (`kaijutsu-types/src/block.rs:1867`) is called
  from nowhere in the workspace — it builds a context-attached error *block*,
  the opposite case from the context-free `GlobalErrorQueue` path that now
  renders.

**Decide**: finish the stub-strip treatment, or delete these two remnants too.

## Rename `VelloTextStyle`/`VelloFont*` shaping types (follow-up from the de-vello pass)

`vello` itself is gone from `kaijutsu-app` (Cargo.toml, Cargo.lock — verified
via `cargo tree -i vello`, 2026-08-12: dock chrome was the last real
consumer, moved onto MSDF; `view/vello_rasterizer.rs` and the vello half of
`view/ui_rtt.rs` are deleted). What's left is a naming nicety: `VelloFont`,
`VelloTextStyle`, `VelloTextAlign`, `VelloFontAxes` (`text/shaping/`) are pure
Parley shaping types — their `Vello*` prefix no longer means anything. Left
alone during the de-vello pass because renaming ripples into every
consumer (`ui/dock.rs`, `view/block_render.rs`, `text/rich.rs`, ...) for a
naming change, not a behavior change — a separate mechanical slice.

## MCP subsystem — audit 2026-07-29 (sonnet, read-only, verified against source + git history)

Amy: *"we can add items to work on mcp servers, we haven't maintained that in a
while and have changed a lot around it."* Ranked most-broken first. Entries the
audit **disproved** were deleted from the Gemini-comparison survey rather
than left to rot (that survey now lives in `docs/wishlist-gemini-cli.md`).

- **No `InstancePolicy` persists across restart — undocumented.** `Broker.policies`
  (`broker.rs:537`) is a bare in-memory `RwLock<HashMap<InstanceId,
  InstancePolicy>>` with no DB table (contrast hooks, which *do* persist). Any
  live-tuned `call_timeout_ms`/`max_result_bytes` silently reverts on restart.
  Policy is keyed by instance globally, not per-context. **Cheaper fix than a
  policy table**: source per-instance overrides from mcp.toml at registration so
  restart re-derives them. (`max_concurrency` being registration-only is a
  *documented, deliberate* choice to avoid racing in-flight semaphore permits —
  that part is fine, leave it.)
- **Hook self-lockout has no recovery path inside our own conventions.** A
  `PreCall Deny("*")` locks out `builtin.hooks` itself — even `hook_list` and
  `hook_remove` return `Denied` — and because hooks are hydrated from SQLite at
  `Broker::set_db` (`broker.rs:275,283`, bridge in `mcp/hook_persist.rs`), it
  **survives a restart**. Proven by an executable test,
  `hooks_admin_is_subject_to_hooks` (`hooks_builtin.rs:1229`). There is no
  `kj hook` CLI. The only documented recovery is hand-editing kernel SQLite,
  which violates our own standing rule against touching that DB directly.
  Two doc comments disagree about this; `bindings_builtin.rs:22-25` ("hooks are
  in-memory", "restart to recover") is **stale and wrong** — `broker.rs:1465` is
  right. Fix the comment (one line), then decide the real question: a hard-coded
  carve-out so admin instances can always self-repair, **or** a `kj hook` path
  that bypasses broker hook evaluation.
- **`bevy_brp` and `invoke_peer` do NOT overlap** — settled, don't re-litigate.
  `peers.rs` is a named-peer action registry (`switch_context`, `active_context`)
  for drift navigation; BRP is entity introspection/screenshots. Orthogonal. The
  kernel today has **no BRP access whatsoever** — the only thing reaching
  `bevy_brp_mcp` is Claude Code's own MCP client, entirely outside the kernel.
- **kaibo as a kernel-side MCP server — the credential/cwd worries are unfounded.**
  It reads provider keys from `~/.config/kaibo/config.toml` directly, so with the
  kernel running as the same unix user (shared-trust model) a spawned subprocess
  authenticates with no env forwarding. Project root is an ordinary `--root <path>`
  arg, already representable in `McpServerConfig.args`. `cwd` is wired correctly
  (`external.rs:216`) and does **not** inherit the "headless turn cwd is `/`" bug
  (that one is kaish `ExecContext`-specific). Only the timeout items above block it.
- **Tool-name collisions fail safe — low priority.** `clean_visible_tool_name` is
  not injective (`builtin.block__x` and `builtin_block__x` both clean to the
  same), and `apply_resolutions` (`binding.rs:435`) *skips* rather than overwrites
  a colliding second tool. So the failure mode is a tool going invisible, never a
  call routing to the wrong instance. Untested for the cross-instance case; worth
  a test, not a redesign.
- Cosmetic: `hook_types.rs:3` points at `docs/hooks.md`, which doesn't exist.
- **No project-instructions discovery** (CLAUDE.md/AGENTS.md analog).
  `build_system_prompt` (`llm/system_prompt.rs:69`) assembles base + rc `.md` +
  `<situation>` and never crawls the filesystem; `assets/defaults/system.md` is
  13 lines. Every project convention must be hand-loaded into an rc script.
  *(Two Gemini-pass entries below cover the design: JIT subdirectory injection +
  filesystem memory-file discovery.)*

**Tier 2 — velocity**

- **Delegation has no join — the SIGNAL shipped, the command didn't.** The
  child's turn now publishes `TurnFlow::Completed`/`Failed` naming its context,
  in-process and over capnp (`subscribeTurnEvents`), so a waiter no longer has
  to poll the child's block log. What's missing is `kj wait` itself: it needs a
  timeout policy, an answer for the turn that ends before the waiter subscribes
  (the bus is lossy and un-journaled — see the TurnFlow durability item), and a
  decision about waiting on several children at once. Compare Claude Code's
  `Task`, which blocks or wakes the caller. Seam documented at
  `request_child_turn` (`kj/fork.rs`). *(Relates to "Headless one-shot with
  JSONL streaming" below.)*
- **No LSP / diagnostics** — no go-to-definition, no type errors without paying
  for a full compile.

## rmcp 1.7 → 3.0.1 bump left SEP-2577 deprecations papered over (2026-07-30)

Landed to talk to kaibo (now on `rmcp 3.0.0-beta.5`, a newer MCP protocol
revision). The bump itself was a clean, mechanical migration (`ContentBlock`
replaces `Content`, `Annotated<Raw*>` wrappers flattened into plain structs
with builder methods, `Meta` split into `MetaObject`/`RequestMetaObject`/
`NotificationMetaObject`, `read_resource`/`call_tool` server-side responses
wrap in `ReadResourceResponse`/`CallToolResponse` for MRTR). Negotiated
protocol version stays `2025-11-25` (rmcp's `ProtocolVersion::LATEST` didn't
move even though `V_2026_07_28` now exists in the enum) — no wire-visible
protocol jump.

What's papered over rather than migrated, each behind a scoped
`#[allow(deprecated)]` with a comment: rmcp 1.8.0+ deprecates the whole
Logging capability (`enable_logging`, `LoggingLevel`, `SetLevelRequestParams`,
`LoggingMessageNotificationParam`) and Roots capability
(`enable_roots`/`enable_roots_list_changed`) per SEP-2577, with **no
replacement** — the spec is dropping them, not superseding them. Separately,
`resources/subscribe`/`unsubscribe` (`ExternalMcpServer::subscribe`/
`unsubscribe`, `crates/kaijutsu-kernel/src/mcp/servers/external.rs`) are
legacy-only as of protocol `2026-07-28`, superseded by `Peer::listen` /
`subscriptions/listen` — a real subscription-model migration, not a drop-in
rename. Kept all three working as-is (straightforward migration, no
opportunistic rewrite) since kaibo/bevy_brp still negotiate against them
today. Revisit when rmcp actually removes the deprecated APIs, or when
adopting the `Peer::listen` model becomes worth the redesign on its own
merits.

## Input: selection auto-copies to PRIMARY (seeded 2026-07-16, input rework)

The other half of the xterm clipboard model (Ctrl+V + middle-click paste
shipped; so did the full prefix set incl. `'`/`A` prompts and the
armed-footer legend): needs a selection UX first. The overlay's
`selection_anchor` has no live producer (mouse drag-select and vi visual
mode are both unwired); when one lands, copy-on-selection to PRIMARY rides
it — `InputOverlay::selection_range` is the read point.

**Update 2026-08-02 (diff slice 5):** the *write* half now exists —
`input::ClipboardWriter`, a dedicated thread owning an `arboard::Clipboard`
for the process lifetime (a clipboard write means becoming the selection
owner, so it cannot be a call from a Bevy system). The diff viewer's yank is
its first user. What is still missing is PRIMARY specifically: `set_text`
writes CLIPBOARD, and `arboard::SetExtLinux` is the Linux-only path to
PRIMARY. Route the selection producer through `ClipboardWriter` when it
lands, and give it a PRIMARY variant then.

## Input: block-step scroll lane — Shift+wheel jumps block-to-block (seeded 2026-07-18, scroll-feel work)

The *continuous* scroll lane got made crisp (follow-mode deadzone fixed;
`ScrollConfig` two-gain per-client config). **Update 2026-08-16 (scroll-relief
slice 0):** the row-quantization mentioned above (`quantize_step` +
`PIXEL_QUANTUM_PX`, 20px logical quanta on the high-res `Pixel` lane) turned
out to *be* a dead zone, not a crispness win — up to ~6.7 logical px of
trackpad travel produced zero motion, then a 20px jump. Removed; `Pixel`
events now pass the gained delta straight through. The *contextual* lane is
still the deferred half: **Shift+wheel steps by whole
blocks**, snapping a block top to the viewport — Amy's "zip / stop / skim /
narrow-in" pattern wants to move by *meaning*, not pixels. Cheap: reuse the
existing block-nav + `scroll_to_rect_visible` (`input/systems.rs:562`) and
`handle_navigate_blocks`; `BlockKind` (`kaijutsu-types/src/block.rs:945`)
is there if we ever want kind-awareness. Trigger decided: **Shift+wheel**
(both lanes always live, no mode). Add a `ScrollBlockStep(dir)` action to
the table so gamepad/rebind/`?`-legend come free (never read `MouseWheel`
in a view — dispatch owns it).

**Consciously fenced-off complexity traps** (do NOT pick these up without a
deliberate decision — this is the rabbit warren): per-block *adaptive* gain
that guesses granularity from what's under the viewport (unpredictable);
momentum/inertia physics; animated scroll-snap settle; minimap /
semantic-zoom skim (the time well already gestures at that —
`docs/timewell.md`). Two explicit lanes beat one clever adaptive thing:
the player picks the granularity, the instrument does not guess.

## msdfgen-rs `Shape::get_bound()` / `Contour::get_bound()` zero-seeded (seeded 2026-07-16, msdf-geometry lane; sidestepped 2026-07-16, msdf-bbox lane)

`msdfgen-rs` (`/home/atobey/src/msdfgen-rs`, local path dep, do-not-modify)
has a real bug: `Shape::get_bound()`/`Contour::get_bound()` seed their
accumulator at `(0,0,0,0)` and only ever *shrink toward* an extreme, instead
of pre-seeding `±LARGE_VALUE` the way the C++ `Shape::getBounds()`
convenience does (which msdfgen-rs never binds). Net effect: `left`/`bottom`
silently stay `0.0` whenever a glyph's true left/bottom edge is positive —
the common case (left-side bearing, glyphs above the baseline). Verified
against `CascadiaCodeNF.ttf`'s `.` glyph: ttf-parser reports `x_min=452`,
`Shape::get_bound()` reports `left=0.0`.

Sidestepped, not fixed: `kaijutsu-app` no longer calls `get_bound()`
anywhere (`generator.rs::generate_glyph` sizes/centers/anchors every glyph
from `ttf_parser::Face::glyph_bounding_box()` instead), so exposure to this
bug is zero for us. Upstream (`katyo/msdfgen-rs`) is dormant; the bug itself
is still real for anyone else depending on the crate. Nice-to-have,
unclaimed: patch `get_bound()`/`get_bound_miters()` to seed `±f64::MAX`
before the raw C++ `bound()`/`boundMiters()` call, and send it upstream as a
PR. Amy also wants to check back later on the pure-Rust `bymsdfgen` crates
as a possible future replacement for msdfgen-rs's C++ core.

## Context lifecycle: "done for now" marker (seeded 2026-07-16, input rework)

Amy wants a soft "done for now" intent marker on contexts — distinct from
`ContextState::Concluded` (done, sticky, never visit-repromoted) and from
demotion (placement, not intent) — as the hook for automation like
"summarize contexts that are done changing". `Ctrl+A q` (close-and-demote,
`docs/input.md`) works today without it; this is the seed for the semantic
layer above placement. Design question when picked up: a new `ContextState`
vs a stamp alongside `promoted_at`/`demoted_at`/`paused_at`.

## DJ thread arc (seeded 2026-07-18; design in `docs/midi.md` "The DJ thread")

Slice 1 SHIPPED + live-verified 2026-07-18 (`docs/midi.md` has the story
and the click-jitter measurements; it also closed the "metronome stops
when backgrounded" symptom). Open:

- **Slice 2 — beat-grid placement**: Tasks 1–3 SHIPPED (wire + kernel
  stamping, `DjCore`'s placement core, `dj/thread.rs`'s `run_loop` wiring —
  `decide_placement`/`enqueue_cue`/`due_cues`/`next_cue_wake` all live end to
  end, `kaijutsu.dj.cue_dropped` telemetry added alongside
  `kaijutsu.dj.clock_transition`). **Task 4 (live verify on the runner) still
  open**: confirm musical phrase onsets lock to the click grid rather than
  drifting with wallclock/network jitter, and watch `clock_transition` +
  `cue_dropped` through a deliberate flush/disconnect mid-phrase.
  - **Known reporting gap, left as-is (Task 3)**: `kaijutsu.dj.cue_dropped`
    fires correctly off `on_flush`/`on_disconnect`/`due_cues`, but TWO other
    `DjCore` entry points also run the internal `settle()` staleness/
    free-run-cap check and can therefore ALSO drop `pending_cues` on their
    own — `due_clicks` (`DueClicks` has no `dropped_cues` field) and
    `decide_placement` (`(CuePlacement, Option<ClockTransition>)` has no
    `dropped_cues` slot either). When one of THOSE is the call that trips
    the fallback, the drop happens (no cue is ever resurrected — the
    "dropped silently" half of the contract holds) but isn't
    telemetry-counted. Fixing it means growing `DueClicks`/
    `decide_placement`'s return shapes in `dj/core.rs` — small, but out of
    `dj/thread.rs`-only scope; pick up alongside Task 4's live verify if the
    gap turns out to matter in practice (`dj/thread.rs`'s `DjEffect::CueDropped`
    doc has the full trace).
- **Frame-cost arc (separate from DJ; visual smoothness + input latency)**:
  (a) rich/markdown parse results are re-computed on every block version
  bump — the `_version` params on `detect_rich_content_typed` /
  `detect_output_content` are threaded but unused (`text/rich.rs:390`);
  cache per (block, version). (b) Parley shaping in `build_block_scenes`
  (`view/block_render.rs:247`) is synchronous on the main thread; a
  streaming burst dirties dozens of blocks in one frame — budget per frame
  or offload like MSDF generation already is. (c) O(N) geometry reconcile
  per doc-version bump (`view/geometry.rs:604`).
- **Unify "animation active → continuous render" (from the 2026-07-18 scroll
  work)**: the app is reactive-idle (`main.rs` `WinitSettings`, 10Hz focused),
  which starves any frame-by-frame animation — smooth scroll eased at ~10Hz
  felt laggy until the render loop was forced to 60Hz. The scroll fix bumps
  `focused_mode` to `Continuous` only while a scroll ease is in flight, back
  to reactive when settled (self-contained in the scroll systems). But the DJ
  thread and other motion (playhead, time-well drift, metronome flash) want
  the *same* "keep rendering while I'm animating" signal. Generalize to one
  gate — an `AnimationActive` vote (ref-count / any-of) that maps to
  `UpdateMode` — that each animating subsystem, DJ thread included, opts into,
  so nothing hacks the render policy ad hoc. Amy's framing (2026-07-18): "maybe
  something the DJ thread could handle independent of rendering" — caveat:
  pixels still require the render loop to run, so this is a keep-awake/wake
  gate, not a parallel draw path off the DJ thread.
- **Broadcast-lag cascade watch item**: UI-side stall → 256-slot broadcast
  `Lagged` → generation bump → full re-sync → bigger dirty burst. The DJ's
  own receiver is immune (own cursor), but the UI drain keeps the loop —
  if it still bites after the frame-cost arc, consider a bigger event
  capacity or delta-coalescing at the actor.
- **Prefetch outcomes are not generation-guarded**: a CAS outcome dispatched
  under a since-replaced actor still lands (the deadline gate still applies;
  pre-existing posture, not a regression). Guard on `generation` if a stale
  outcome ever observably misfires.
- **Stale-architecture prose**: `docs/scenes/patchbay.md` + `docs/pcm.md`
  still narrate `midi.rs`/`metronome.rs`/`AudioOutPlugin`; two doc-comment
  mentions of `crate::metronome` sit in the parallel session's files
  (`actor_plugin.rs:204` rustdoc link, `input/scroll_config.rs` prose) —
  fold into the next docs pass / that session's next commit.

## Audio sink follow-ups (seeded 2026-07-16, clip-arc live verify)

- **CasResolver's SFTP session has no proactive keepalive** (seeded R4,
  2026-07-16): R4 (`docs/pcm.md`) bounded per-fetch recovery with a
  `FETCH_TIMEOUT` + logged redial, closing the "slow + silent" symptom the
  fanfare-clip failure exposed — but nothing yet keeps the session alive
  *between* fetches (no TCP/SFTP keepalive ping), so a long-idle connection
  still goes stale; R4 just detects and redials it promptly now instead of
  ~70s later with nothing in the log. Add a periodic no-op ping on the
  resolver's connection if idle recovery still needs to be faster than
  `FETCH_TIMEOUT` in practice.

## Conversation geometry model — accepted limits (seeded 2026-07-16, from `82207a2a`+`7e3f2fa1`)

Both `6504fafe` follow-ups shipped (estimated-height placeholders killed the
O(N) first-load pass; band despawn/respawn bounds entities + VRAM to the
viewport band). New accepted limits of the geometry model, carried here:

- **Offscreen-streamed rows keep stale heights** until they re-enter the
  virtualize show window (±1 screen around the viewport, so still one full
  screen before they can be seen) — the scrollbar/spacers drift a little
  while text streams into rows you've scrolled away from. This is the safe
  side of the trade: virtualize's shown set must always be **one contiguous
  document interval**, because the two `ConversationSpacer` nodes can encode
  exactly one gap above it and one below. Force-showing an offscreen stale
  row (what we used to do) makes taffy pack it directly under the in-window
  rows — visible corruption, and readback then measures it there. If the
  drift ever matters, fix it by re-estimating on version change, never by
  breaking contiguity.
- **Height corrections landing on a partially-visible row are not
  scroll-anchored**: `readback_block_heights` only compensates rows *fully*
  above the viewport (measured against pre-measure offsets), so a straddling
  row's correction shifts the content below it. Normal scrolling can't reach
  this — a row enters the show window a full screen before it is visible, so
  it is measured while still fully above/below — but a jump-scroll (scrollbar
  drag, jump-to-block) can drop the viewport straight onto a row whose height
  went stale, and the snap is then visible for a frame or two. Converges,
  never oscillates (heights are deterministic); worst case is a block that
  grew hugely while offscreen.
- **ToolCall at the outer band edge** can briefly render CloseBottom while
  its ToolResult is still outside the band (border joins are computed from
  in-band snapshots only); corrects as the pair scrolls in.
- **`handle_collapse_toggle` still clones the whole document** on keypress
  (`editor.blocks()` in input/systems.rs) — noticeable hitch on huge
  contexts; could walk geometry rows for Thinking ids instead.
- **Width changes only re-estimate unmeasured rows**; measured heights of
  despawned rows go stale on resize until band entry (same just-in-time
  correction path, pre-existing behavior).

## Anthropic client maturity (seeded 2026-07-15, thinking-enable arc)

Adaptive thinking (`{type: adaptive, display: summarized}`, model-gated)
landed 2026-07-15 in `claude::Client::stream()`. The client is young; we own
it precisely so we can tailor to the provider — gaps observed while wiring
thinking, roughly by priority:

- ~~No config path into provider clients~~ **SHIPPED 2026-08-03** by the
  cast renovation: per-slot `effort`/`thinking_style`/`thinking_budget`/
  sampling on `cast_slots`, cascading to `llm_defaults`, reaching the wire
  through `apply_slot_tunables` — "effort: low here" is now
  `kj cast slot set <cast> <role> --effort low`.
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
  beat-this) with install instructions living beside the embedding recipe in seed_backends.rs and a
  README. A registry (name → expected files → checksum → fetch) would make
  `kj models list/fetch/verify` possible and close the manual-download gap.
  This is also where a `kaijutsu-inference`-style shared crate becomes
  justified — explicitly deferred (Amy, 2026-07-15) until there's real
  sharing; the Embedder trait + per-crate rten deps are the seam until then.
- **audio/beat-this model config**: the verb hardcodes the model dir
  convention; a config home now means a kj-managed kernel-db table (the
  2026-08-03 SQL model-config shape — models.toml is gone, and the
  seeded-once CRDT caveat died with it).
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
job shape deferred). Slice 1 steps 1–3 have landed (seeds + `kj midi
list/show`; sink-fed presence: in-app profile matching, `reportMidiPresence`,
the ephemeral `/run/midi/<device>` store, presence column) — as has step 4
(`kj midi send`/`panic`: a device-addressed control cue on the existing
`RenderCue` wire, riding a per-device `ctl:` port wired by subscription —
the original DIRECT emit shipped but no hardware could hear it, see the
2026-08-02 bench story in `docs/midi-next.md` step 4) and step 5 — the
`exchange()` round-trip + `kj midi identify`, which closes slice 1. Slice
order in the doc; next: slice 2 (routing consumes profiles).
First real consumer: Minibrute on the laptop app, then the per-track
channel-routing fix (this file → Hyoushigi/Musician area; `docs/chameleon.md`
open items) built on profile vocabulary.

- **USB `vendor:product` enrichment for presence matching** (deferred
  2026-08-02, slice 1 step 3). `midi_match::PortFacts::usb_id` exists and the
  matcher already ranks a USB hit above any name match, but nothing fills the
  field: on Linux it needs an ALSA-card → sysfs walk
  (`/sys/class/sound/cardN/device/{idVendor,idProduct}`). On macOS it is
  costlier than it looks (gemini review, 2026-08-02): CoreMIDI endpoints do
  NOT expose USB ids — the sink must bridge into IOKit
  (`IORegistryEntryCreateCFProperty`) and walk the hardware tree to find
  `idVendor`/`idProduct`; port-name substrings are the saving grace since
  they absorb per-OS suffix differences. Until then matching is name-substring only,
  which the shipped profiles all support. Two same-model units on one rig
  (identical names, distinct USB paths) is the case that will force it — and
  will also need something finer than `vendor:product`, which is per-model,
  not per-unit.
- **`kj midi send` routes to a device's FIRST matched port** (deferred
  2026-08-02, slice 1 step 4). `dj::midi::resolve_route` takes
  `routes[device][0]`; for a two-port device (KeyLab: MIDI + DAW) that is the
  MIDI port, which is right today but is a positional accident, not a
  decision. Slice 2's *device.role* vocabulary is the fix — the routing table
  already carries every matched address, so only the picker changes
  (`a_multi_port_device_routes_to_its_first_matched_port` is the tripwire).
- **`kj midi send` writes no `/run/midi` sent-provenance** (deferred
  2026-08-02, slice 1 step 4). `docs/midi-next.md` says every emit should
  record `{value, source: sent, at}` so relative commands ("-25%") have a
  baseline to work from. Slice 3 (device contexts) is where that pays off;
  slice 1 emits raw absolutes only, so there is nothing to be relative to yet.
- **The exchange timeout ladder is four constants in four crates** (deferred
  2026-08-02, slice 1 step 5). A request's bound is enforced at every hop, each
  a little looser than the one inside it so the *innermost* layer that actually
  wedged is the one whose error a player reads: app worker `T`
  (`midi_exchange::ExchangeClient::exchange`), client forwarder `T + 0.5s`
  (`MidiExchangeSlot::WORKER_SLACK`), server bridge `T + 0.75s`
  (`rpc::EXCHANGE_CALL_SLACK`), kernel `T + 1s`
  (`midi_exchange::KERNEL_DEADLINE_SLACK`). Nothing enforces the ordering but
  these comments; if a fifth hop appears, the ladder wants a single home
  (`kaijutsu-types`?) rather than a fourth doc-comment promise.
- **Exchanges serialize per SINK, not per port** (deferred 2026-08-02, slice 1
  step 5). The doc says "serialized per-port"; today one worker thread runs one
  dialogue at a time for the whole app, which is strictly stronger and costs
  nothing while exchanges are human/model-paced (identity, later a settings
  pull). A grooming track sweeping ten devices on a cadence is the case that
  will want per-port concurrency — and the worker is where it goes, not the
  wire (the request already names its port).
- **`kj midi identify` picks the device's FIRST matched port too** (deferred
  2026-08-02, slice 1 step 5). Same positional accident as `kj midi send`, same
  slice-2 fix: `midi_exchange::resolve_exchange_address` takes
  `routes[device][0]`. A KeyLab answers on its MIDI port, which is right today.
  **No longer hypothetical (2026-08-02 evening):** the MiniBrute answers
  Universal Identity ONLY on its port 1 ('MiniBrute MIDI Interface', the
  SysEx/control port) while routing asks port 0 (the synth) — so
  `kj midi identify minibrute` times out against a device that answers.
  First-matched-port now demonstrably picks the wrong port for a real device;
  the slice-2 role vocabulary (port roles in the profile: synth vs control)
  is the fix, and the minibrute profile already records which port answers.
- **No CoreMIDI exchange backend** (deferred 2026-08-02, slice 1 step 5). The
  worker is ALSA-only and says so in its answer (a mac sink refuses with "not
  this backend" rather than timing out, so another sink on the rig can have the
  gear). Everything above the worker — the wire method, the registry, the
  device-name addressing — is already backend-neutral.
- **CoreMIDI control-cue addressing** (deferred 2026-08-02, slice 1 step 4).
  `dj::midi::parse_alsa_addr` refuses anything that isn't `client:port` rather
  than guessing, so a CoreMIDI-shaped address reaching the ALSA sink is a loud
  drop. The mac backend will need its own address parse + emit alongside the
  ALSA one — the envelope and the routing table are already backend-neutral
  (device names and opaque address strings), so nothing above the sink changes.
- **`kj midi identify` timeout is a MUTE error** (found live 2026-08-02, bench
  verification). When no reply arrives, the `tool_result` block lands with
  `status: error, exit_code 1` and **zero content** — no "timeout waiting
  for reply from keystep-pro", nothing in stderr, nothing kernel-side. The app
  log is the only witness (`midi_exchange: … waiting 2s` then silence). Loud
  over silent: the timeout should say which device, which port, how long it
  waited, and that presence said the device was live. The mute error cost real
  diagnosis time: the actual fault was host-side silent drop (next entry), and
  a mid-session hypothesis ("Arturias ignore identity requests") stood for an
  hour before wire counters disproved it — the KSP answers identity fine
  (`F0 7E 7F 06 02 00 20 6B 02 00 09 00 5D 01 00 02 F7` captured live).
- **Arturia SysEx vocabulary in kaijutsu — make settings readable** (wished
  2026-08-02, Amy, bench session). Probing the MiniBrute's settings today means
  hand-rolled `aseqsend` hex guesses at the Arturia frame
  (`F0 00 20 6B <dev> 01 <seq> <op> <param> F7` family). Amy wants a clever
  encoding in kaijutsu for these request/reply frames — device profiles could
  then *declare* their parameter maps (receive channel, knob assignments) and
  `kj midi pull`/device contexts could read real settings instead of trusting
  the profile's static claims. Makes configs easier to build; pairs with the
  `exchange()` machinery that already exists and with `docs/midi-next.md`'s
  settings-vs-capabilities ground-truth split.

## App (and headless sink) as MCP clients offered back to the kernel (seeded 2026-07-15)

Eventual direction (Amy): `kaijutsu-app` — and the future headless sink
variant — should also be **MCP clients**, offering their local capabilities
(audio devices, capture, render, screenshots?) back to the kernel as tool
surfaces, the way `kaijutsu-mcp` exposes the kernel outward today. Deep work
(client-side MCP host, capability registration, routing through the broker);
deliberately parked — noted while designing audio capture so the capture
seams don't foreclose it.

## MIDI seq topology unreachable from kj/kaish — wants patchbay slice 4 (noted 2026-07-18, "can you see MIDI devices via kj?")

The observed ALSA seq graph (which MIDI devices exist + how they're patched) is
read **only** by `kaijutsu-app` (`patch_graph.rs::PatchGraphReader` →
`PatchBayState`) to render the patch-bay scene — slice 0. It's reachable by no
`kj` verb, no kaish VFS mount, and not even BRP (`PatchBayState` is a plain
`Resource`; `GroupPlate`/`PortLabel`/`SocketPeg` are `Component`-only, no
`Reflect`). So an agent can't answer "what MIDI is on `<app-host>`?" today —
only a same-box model can, via **pawlsa**.

This is exactly **`docs/scenes/patchbay.md` slice 4** ("ship observed-graph
snapshots kernel-ward so models and remote peers see the same fabric"),
deferred behind the viz-first slices — surface it via `kj midi ls`/`kj seq`
and/or a read-only kaish VFS mount (feeds the `kj midi` + `/run/midi/<device>`
plan under *MIDI device profiles*; provider is the app-as-MCP-client work above,
since the seq graph is edge-local to the app's machine, not the kernel's).
Cheap app-only interim if wanted sooner: `Reflect`-register `PatchBayState` +
the label components so BRP can read them (scene-dependent, no kernel round-trip).

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

Harness steals for that design round (2026-08-04 gap-analysis session,
`~/src/meadow-lab/docs/kaijutsu-gap-analysis.md`): Hermes' missed-run policy —
catch up once after a grace window (half-period, clamped 120s–2h), then
fast-forward, no backlog replay (`cron/jobs.py:2155` in the research clone);
Hermes' no-NL-parsing stance (curated shorthand like `every 30m` + raw cron,
compiled up front); QwenPaw's `HEARTBEAT.md` — a user-editable "what should I
check on" prompt re-read each beat (in kj that's a block, `kj rc edit`-able);
QwenPaw's idle trigger (proactive check-in after N idle minutes, gated on
agent-not-busy); Hermes' per-job model pinning → per-track cast binding, which
casts already shape-match (local toil vs. deepseek/claude cognition).

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

## MCP `data` shape change + unbounded rich_json (seeded 2026-07-18, `kj transport list` / `OutputData.rich_json` 3-model review)

Follow-ups noted but not done while fixing the review findings on `kj
transport list` and the `OutputData.rich_json` wire-through
(`crates/kaijutsu-client/src/rpc.rs` `parse_block_snapshot`,
`crates/kaijutsu-server/src/rpc.rs` `block_output_data`):

- **MCP `shell`/`context_shell` `data` field SHAPE CHANGED (breaking).**
  `ShellCompletion::to_json` now emits `OutputData::to_json()` (rich_json
  verbatim, or inferred row-objects) instead of the raw
  `{headers, root, rich_json}` struct — affects `kj` AND node-tree builtins
  (`ls`/`find`/`glob`). Flag for release notes: MCP agents are the consumers
  you can't grep for.
- **rich_json size is UNBOUNDED.** `.data` bypasses kaish's output limiter
  (which only caps text), so `block_output_data` persists it whole and
  `build_output_data` fans it out on every `OutputChanged` + context
  snapshot. A huge `.data` (e.g. `kj block list` over a giant context) has
  no cap anywhere. Follow-up: a size ceiling at `block_output_data` (fail
  loud, doctrine-consistent) or route large payloads through CAS like
  `RenderCue`'s `casHash`.

## External MCP servers — no `kj mcp restart <name>` (seeded 2026-07-30, `docs/external-mcp.md`)

`reconcile_with_toml` (`crates/kaijutsu-kernel/src/mcp/external_registry.rs:23-35`)
deliberately never reconnects an already-running external server on
`kj mcp reload` — only its `InstancePolicy` (e.g. `call_timeout_ms`) is
refreshed, even if `command`/`args`/`env` changed underneath it. Picking up
such an edit today needs a full kernel restart. A name-scoped
`kj mcp restart <name>` (unregister + reconnect one instance) would close
that gap without touching the conservative reload-doesn't-reconnect default;
the reload-vs-hot-swap tradeoff is written up in full in
`docs/external-mcp.md` "The reload design fork."

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
- **LLM providers:**
  - Per-model knobs in the app (server-side config is now cast_slots/backend_models, 2026-08-03; the app renders none of it yet).
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
## parley opts out of dictionary line breaking for Japanese — check back (2026-08-12, from the parley 0.9 bump)

**Log silenced, behaviour left alone** (Amy: "silence it and we'll track an
issue for ourselves to check back in. I might file an upstream issue if we
find we need it sooner than later"). Revisit when wrapped Japanese body text
starts to matter — the upstream brief is written up below, ready to file.

parley 0.9 line-breaks through `icu_segmenter` (0.7 had **zero** ICU
dependencies), and picks the constructor that loads no dictionary for the
unspaced scripts:

```rust
// parley-0.9.0/src/analysis/mod.rs:56,63,70 — all three word-break modes
LineSegmenter::new_for_non_complex_scripts(opt)
```

Every layout containing Japanese then hits `select()`'s miss arm and warns
`ICU4X data error: No segmentation model for language: ja` — measured at
**~190 lines per 45 seconds** with 会術 on screen.

**Silencing it took two pieces, and the first one is the interesting bit.**
`icu_provider` only calls the real `log` crate when its `logging` feature is
on. With it off — nobody enabled it — `icu_provider::log` is a shim
(`lib.rs:188-221`): `pub use std::eprintln as warn` under `debug_assertions`,
and a **no-op macro** without it. So the warning was a bare `eprintln!`
straight to stderr that no `tracing` filter could ever reach, *and* it never
existed in release builds at all — this was always dev-loop-only noise. The
fix is a direct `icu_provider` dependency in `kaijutsu-app/Cargo.toml` whose
only job is to turn `logging` on via feature unification, which routes the
warning through `log` → tracing-subscriber's bridge, where `main.rs`'s
`EnvFilter` entry `icu_provider=error` drops it (`error`, not `off`, so a real
ICU data failure still surfaces). Verified: ~190 per 45s → **0 per 45s**.

**The trap, recorded because it is the tempting wrong fix:** feature
unification fixes the *logging*, but it cannot fix the *data*.
`icu_segmenter`'s `default = ["compiled_data", "auto"]` while parley takes
`default-features = false, features = ["compiled_data"]`, which looks exactly
like the culprit — but adding `icu_segmenter` with `auto`/`lstm` as our own
direct dependency does **nothing**, because `new_for_non_complex_scripts`
builds `ComplexPayloadsBorrowed::new()`, which sets `ja`/`th`/`km`/`lo`/`my`
to `None` *unconditionally* — no `cfg(feature)` anywhere near it, and the call
sites are `const {}` blocks with no hook. The gate is a call site, not a
feature. Two adjacent problems, one solvable by a feature and one not.

**parley's choice is defensible**, which shapes the upstream ask: the CJ
dictionary is ~5.1 MB of baked data (`segmenter_dictionary_auto_v1`; the SE
Asian dictionaries are another ~5.3 MB). A layout crate adding that to every
binary unconditionally would be worse. So the ask upstream is *"give us a way
to opt in"*, not *"load it by default"*.

**What we actually lose is modest.** UAX #14 still permits breaks between
ideographs, so Japanese does wrap — it just may break mid-word instead of at
dictionary word boundaries, which is close to traditional CJK typesetting
anyway. Today the only standing Japanese is the 会術 title, which never wraps.

Options, when it is worth doing: raise the opt-in with parley upstream (the
honest fix — any parley user rendering CJK hits this), or take line breaking
over ourselves with `icu_segmenter` directly (big: parley owns line breaking
internally and exposes no segmenter hook, so this means displacing it).

## Test leaks a pidfile into `/tmp` (2026-08-12, via kaish's /tmp audit)

`background_exec.rs:1609` builds a pidfile at
`std::env::temp_dir().join("bg-exec-test-childpid-{uuid}")` and never removes
it — one leftover confirmed on zorak from 08-10. Not implicated in the 4.9G
that audit was chasing (that was cc session storage); flagged so it is fixed
at the source rather than misattributed later. The kernel test harness already
has the right pattern — `Kernel::with_temp_cleanup(root)`, which drops a temp
root when the dispatcher drops. Small.

## Drift UX — cross-session ergonomics (2026-08-12)

Design record + full gap list: **`docs/drift-ux.md`**. Slice 1 (`push`
delivers immediately, staging behind `--stage`) SHIPPED `b2cdb770`.
Remaining, in order:

- **cc-\* contexts never deregister, and that breaks addressing. RULED, ready
  to build.** Measured on live zorak 2026-08-12: 289 contexts, 152 `cc-*`,
  **60 sharing the `cc-kaijutsu` prefix**. Drift resolves a label prefix and
  errors `Ambiguous` on >1 match (`ids.rs:311-320`), so
  `kj drift push cc-kaijutsu` is unusable *today* and degrades monotonically.
  `session.end`/`agent.stop` only write text blocks
  (`hook_listener.rs:315-376`).
  **Amy 2026-08-12: `session.end` archives the context; names do not change;
  a one-shot sweep of the resident backlog is authorized.** Archive is not
  trash — archived contexts are retained work kept for referential integrity,
  later search, and research, with indexing already in flight elsewhere. No
  kernel change needed: `archived_at` is already what both resolvers filter
  on. Do **not** rename or suffix on archive; the label leaves the active set
  intact, which keeps it meaningful for the coming index.
- **kaish latches are going away — approvals become ours, bespoke** (Amy,
  2026-08-12: "the kaish latches are going away in the next release. we'll
  rebuild approvals in kaijutsu bespoke, using some of kaish's new tools for
  giving us visibility into a command"). This supersedes the earlier read that
  our only kaish approval-surface exposure was a *change* to `LatchRequest` at
  the 0.14 bump — it is a **removal**, so the work is ours either way.
  Consumption points to replace: `LatchRequest` construction at
  `mcp/servers/shell.rs:774` and the `structured.latch.nonce`/`.hint` batch
  loop (`:462-464`), plus `.latch` reads in `runtime/kj_builtin.rs:456,752`.
  Verbs that are latched today and therefore need the bespoke path before the
  bump: `kj context archive` / `remove` / `retag`, `kj workspace remove`
  (`workspace.rs:306`), `kj preset remove` (`preset.rs:283`).
  Design note worth keeping from using it in anger during the sweep: the
  current nonce is scoped to the **label**, not the id — confirming names what
  it authorizes, and a batch keyed on ids fails loudly with "nonce scope
  mismatch". Keep that property. Amy's steer is that kaish's new
  command-visibility tools are the substrate, not a reimplementation of
  latches.
  **Substrate identified 2026-08-14: it is kaish 0.14's `plan_program`** —
  per-statement, rendered UNEXPANDED, one entry per command the statement
  would run (control-flow bodies and `$( )` included), plus the variables it
  reads and writes. See the 0.14 bump entry at the top of this file for the
  measured scope and the `PlanDigest` notes.
  **Amy 2026-08-14 rulings on the rebuild:** one gate system covering the
  `shell` tool AND these `kj` verbs; a durable SQL approval ledger, retained
  forever with timestamps for later windowing; **kj + CLI first**, then ACP
  inline, then an `-app` omni-view; built as a **crate with the DB injected**
  and tested hard rather than as an MVP subset; plus a **checklist table of
  the rc scripts that ran**, snapshotted (which would have made this morning's
  silently-inert assistant seat visible on run one). Prior-art search found no
  usable Rust crate — closest designs are Vault control groups and QwenPaw's
  `governance/policy.py` (ASK→approve→**generalize**, to fight allowlist
  fatigue). Two build constraints: SQLite has no `SKIP LOCKED`, so single-
  answerer claim must be `BEGIN IMMEDIATE` + one atomic `UPDATE … RETURNING`;
  and the digest CANNOT be computed post-resolution (the plan is
  pre-resolution by design), so a plan with free variables must never be
  eligible for allow-always — the other half of the label-not-id guard.
- **Sweep jobs for trash contexts — LATER, explicitly not now** (Amy,
  2026-08-12: "we can add sweep jobs later to clean up trash contexts, but not
  now"). The one-shot sweep ran; do not build recurring automation for it yet.
- **MCP connections that never receive hook traffic mint a context that never
  stabilizes and never archives** (found 2026-08-12, during the sweep). The
  sweep took `cc-*` from 161 → 26 and the `cc-kaijutsu` prefix from 60 → 12.
  **Ten of the twelve survivors are `cc-kaijutsu-0812-HHMM`** — the
  *pre-stabilization* label form (`main.rs:396`), which means those contexts
  never got a hook event carrying a `session_id`, so `maybe_stabilize_label`
  never renamed them. They came from short-lived `kaijutsu-mcp --connect`
  invocations — diagnostic probes and `/mcp` reconnects — in a single day.
  Two compounding problems: they are minted per connection rather than per
  *session*, and the new `session.end` archiving cannot reclaim them because
  no `session.end` ever fires for a connection that had no session. So this is
  the residual generator the sweep does not close — roughly ten per day for
  one project. Options: don't register until the first hook event proves a
  real session (register lazily), or let a connection that closes without ever
  stabilizing archive its own context on drop. **Self-inflicted note for
  future sessions: probing the kernel via `kaijutsu-mcp --connect` adds to the
  pileup; prefer the already-attached MCP `shell` tool.**
- **`lost+found` has no discovery or working surface** (Amy, 2026-08-12: "we
  need to add some tools for discovering and working with lost+found"). It is
  created lazily by the dead-letter path (`drift.rs:606-680`) and *nothing
  points at it* — no `kj` verb lists it, and a caller whose drift dead-lettered
  gets no pointer. Wants at minimum: a way to see it exists and its depth, a
  way to read what landed there, and a way to re-deliver an entry to its
  intended target. Weight goes up if drift starts carrying musical material —
  a silent `lost+found` is a dropped phrase nobody goes looking for.
- **A received drift is a dead end for reply.** Hydration surfaces the short
  id only (`llm/hydrate.rs:344-357`) — never the sender's label, no thread id,
  no hint that replying is possible. **Not the cheap fix it looks like.**
  Stamping the label on the block is ~65 refs across 14 files plus wire and
  app renderers, *and* it stamps a mutable value — `stabilize_context_label`
  renames `cc-*` contexts, so a stamped label goes stale and displays a wrong
  address as a right one. Resolving at hydration is correct but
  `translate_block` has no DB access and has several callers; it wants a
  label snapshot passed in, not a DB handle held across hydration. Small
  design pass, not a patch.
- **Arriving drift and turns — RULED (Amy, 2026-08-12).** Default stays the
  gentle mailbox drop picked up on the receiver's next turn. `kj drift push
  --drive` *requests* a turn. A per-context setting decides whether drive
  requests are honoured, **defaulting to off** — the receiver-side veto is
  what makes a sender-side flag safe, and it is why this is not the rejected
  "sender declares" shape. Natural home for the setting is the context
  binding/loadout (an ergonomic-nudge capability), not a new concept.
  **Blocked on the rc identity-smear fix below** — a driven turn must not be
  attributed to the sender's principal.
## Drive gates — self vs external, and don't drive the archived (2026-08-12)

Amy asked for the `--drive` default-off idea to be driven into code and docs
generally: it is not drift's question. `kj drift push --drive` is just the
first caller; the beat scheduler and any future orchestration are the same
shape. **Deliberately small — Amy: "I don't want to get crazy with
permissions."** Half of it already exists.

**Self-drive is already gated, and correctly.** `kj drive` requires
`Capability::Drive` on the **caller's** context (`kj/drive.rs:61-64`), with
the intent stated in the code: *"what makes narrowing a musician's binding
actually stop its OODA tick."* So "this context may drive" is solved. Amy's
self-vs-external split maps onto it cleanly:

- **self-drive** (caller == target, e.g. `rc/musician/tick/S10-drive.kai`
  driving its own context) — governed by the existing `Drive` cap. No new
  concept. This also answers the "does the beat scheduler bypass consent?"
  question: it never needed to, because a musician's tick is *self*-drive and
  consent is about *external* drive.
- **external drive** (caller != target) — the genuinely new gate, on the
  **target**. Per-context, default off, with `context_type` defaults via rc:
  musicians on (a player that cannot be woken cannot take a hand-off
  mid-piece), coders probably, everything else off. Home is the context
  binding/loadout — an ergonomic nudge, not a security boundary; a context
  that declines is *focused*, not distrusted.

Amy's "this session cannot be driven, to ensure things stay stopped" then
needs no third mechanism: deny external drive on the target **and** withhold
`Drive` from the context itself, and nothing can start it — one existing knob
plus one new one.

**The archived check — SHIPPED 2026-08-12.** `kj drive` now refuses any
target that is not `Live`, before it publishes a turn request. It had *no*
context-state gate at all, and the archived case was genuinely reachable:
label resolution filters archived rows, but `KernelDb::resolve_context`
parses a **full UUID first** through `get_context`, which has no
`archived_at` filter (`kernel_db.rs:2308-2315`) — so `kj drive <full-uuid>`
drove an archived context. That mattered because archived contexts are
*retained work* kept for referential integrity, later search, and research;
driving one mutates the record we are preserving. `archived_at` is checked
ahead of the enum, per `ContextState`'s own doc comment naming it
authoritative.

Two neighbours came along, because the same missing gate covered them:
`Staging` was **already documented** on `ContextState` as "LLM blocked"
(post-fork curation) with nothing enforcing it, and `Concluded` is refused
for the archived reason one step softer — its documented recovery is `fork`,
not a turn. Each refusal names the state and the way forward. Tests:
`drive_refuses_an_archived_context` (addressed by full UUID, the path that
bypasses the filter), `drive_refuses_a_concluded_context`,
`drive_refuses_a_staging_context`.

**Cold-cache suppression** is the softer companion, and it is Amy's insight:
kaijutsu contexts are designed to be always revivable from durable state,
which is exactly what makes revival look *free* to anything that can request
it. When the provider prompt cache has aged out, the next call reprocesses the
whole conversation as a cache miss. Computable with **no new schema**:
`context_usage.updated_at` is the wallclock of the last *completed LLM call*
(`kernel_db.rs:624-635`) — the right clock, where `contexts.last_activity_at`
reads falsely warm because any block write touches it — measured against the
shortest `cache_breakpoints` TTL (`kj/cache.rs:138-141`; ephemeral ≈5m,
extended ≈1h), with `cache_read_tokens > 0` as corroboration. Conservative
edges on purpose: no breakpoints or no usage row means there is no cache to
lose, so do not suppress — that is an ordinary cold call.

Refusals must be **loud** (a silently-dropped escalation is the
silent-fallback shape CLAUDE.md rejects), and there should be a way to
*insist*, because cold-cache is a cost signal, not a correctness one.

*"KV" in this entry means the model's attention/prompt cache — not the kernel
key-value store demolished 2026-07-04.*
- **rc lifecycle identity smear — blocks a drift rc script writes are
  attributed to the *sender*.** `run_kai_script` materializes the rc kaish
  with `principal = caller.principal_id` (`kj/lifecycle.rs:376,388`) — the
  sender's — while binding the shell to the *target* context. Capabilities
  are fine: they gate on `caller.context_id` (`kj/mod.rs:563-576`), which is
  the target, so authorization runs in the right direction (this is where a
  2026-08-12 GLM review was wrong, and the correction is recorded in
  `drift-ux.md`).

  **Narrowed again 2026-08-12 — `privileged` is NOT smeared.** An earlier
  revision of this entry (and two verbal relays) claimed `privileged` rode in
  from the sender's shell. It does not:
  `materialize_context_kaish_rc` passes `true` unconditionally
  (`kj/context_shell.rs:71-91`), because privilege is a property of *being the
  rc runner*, not of whoever triggered it — and `KjCaller::privileged`'s own
  doc says it is "stamped at `KjBuiltin` construction by the rc runner —
  **never** derived from a shell var" precisely to stop it being forgeable.

  So the smear is exactly one field: **`principal_id`**. An rc script runs
  under the principal of whoever *caused* the lifecycle to fire, so blocks it
  writes into the target context are authored by a foreign principal.
  Harmless while the shipped `drift` rc script only clears prompt cache.
  **Must be resolved before shape B ships an `S50-drive.kai`**, because then a
  whole driven turn would be attributed to the context that requested it
  rather than to the context that ran it.

  **RESOLVED by Amy 2026-08-12** — "can the principal be set for the origin of
  the drift just on the drift block? then the rest of the blocks in the
  context would belong to the context owner imo." That is the right split, and
  it answers the question this entry was stuck on (whose name belongs on work
  done on another's behalf) by separating two things that were conflated:

  - **The drift block carries its origin.** Author it as the *sending*
    principal — that block genuinely came from elsewhere, and provenance is
    what it is for.
  - **Everything else belongs to the context owner.** rc scripts are the
    context doing its own lifecycle work; that they were *triggered* from
    outside does not make the resulting blocks foreign.

  Implementable as-is, no new plumbing: `insert_drift_block_as` already takes
  an explicit `Option<PrincipalId>` (`block_store.rs:3043-3052`) and the plain
  `insert_drift_block` wrapper simply passes `None`
  (`block_store.rs:3020-3040`), so the drift-block half is threading the
  caller's principal through the four `kj/drift.rs` insert sites. The rc half
  is `run_kai_script` taking the target's `ContextRow.created_by` instead of
  `caller.principal_id` (`kj/lifecycle.rs:376,388`). Both want tests pinning
  the authorship, since nothing asserts it today.
- **Drift edge metadata is inconsistent across delivery paths.** Immediate
  push stamps `drift_kind.to_string()` (`"push"`, `kj/drift.rs:335`); flush
  stamps `format!("{kind}#{staged_id}")` (`"push#1"`, `:629`). So
  `kj drift history` cannot uniformly trace an edge back to a staging event.
  Arguably correct as-is — an immediate push *has* no staging event — but the
  two paths should agree on a scheme rather than differ by accident.

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
- **`kj/drift.rs` orchestration bloat:** push/pull/merge/flush each inline
  variations of "insert drift block + record edge + run rc lifecycle".
  Extract the shared operation; the command layer should dispatch, not
  orchestrate.
- **Residual race: `unregister` vs. an in-flight delivery that then
  succeeds.** Fixed 2026-08-12: `DriftRouter::drain` now marks items
  `in_flight` in place (`drift.rs`) instead of removing them into the
  caller's local `Vec`, so `cancel`/`queue` see them during a flush's async
  delivery window and a cancelled item is dropped on `requeue` instead of
  resurrected — see `drift.rs` module docs on `StagedDrift::in_flight` /
  `DriftRouter::{drain,complete,requeue}`. One narrower race survives: if
  `unregister(ctx)` runs *between* `drain` and the delivery's outcome, and
  the target document write actually succeeds despite the context now being
  unregistered, the item is both "delivered" (block landed before teardown)
  and swept to `dead_letter` by `unregister`'s indiscriminate sweep over
  `staging` (which doesn't check `in_flight`) — so it gets written into
  lost+found too, looking like a failure that never happened. Needs
  `unregister` to either skip in-flight items or have `complete`/`requeue`
  check "was this context unregistered out from under me" before deciding
  dead-letter vs. drop. Low priority: requires a context to be destroyed in
  the exact window between drain and the block-store write during that
  context's own outbound or inbound flush.
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

- **Backup shipped 2026-08-03; export/import round-trip still open.**
  `kj db backup <path>` (`KernelDb::vacuum_into`, `VACUUM INTO ?1` bound
  param) and `kj db checkpoint` (wraps `KernelDb::checkpoint()`) landed —
  see docs/architecture/kernel.md "Backup & restore" for the design and the
  stated restore procedure (stop kernel → swap `kernel.db` (+drop
  `-wal`/`-shm`) → start kernel; deliberately not a `kj` verb, since the
  kernel's in-memory state would desync from a live file swap). Still open:
  an export/import that round-trips through decode→encode would rewrite
  every record in the current format, bounding how long at-rest
  compatibility shims have to live (see the frozen-payload test in
  `kaijutsu-types/src/codec.rs`) — today they must live forever, because
  compaction is threshold-triggered and a quiet document may never be
  re-snapshotted.
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

- **HUD graph design: what should the dock sparklines *be*?** (2026-08-12,
  Amy). Rendering is settled — flat triangles in the dock texture, both dock
  and block-cell sparklines on `text::sparkline::build_sparkline_vertices` —
  but the graphs themselves are placeholders: events/sec kernel-wide and
  active-context running blocks, 40 samples at 250ms (a 10s window), no
  labels or thresholds. For the ambient-command-center role, decide data
  source, window, and labeling before polishing pixels further. The
  DockSparkline read-`/run` note above still stands. A `UiMaterial` shader
  (shader-AA lines, `BlockFxMaterial`-style uniform data) is the fallback if
  triangle rendering disappoints at ambient distance.

- **Conversation-view de-vello pass — needs a visual pass in the running
  app** (2026-07-30, `feat/devello`; a validation pass is planned
  separately, this records what to check first). Role-group dividers,
  sparklines, and the image placeholder moved off vello onto a shader
  center-line (`BorderKind::CenterLine`) and plain Bevy UI rectangle
  geometry (`text::sparkline::build_sparkline_geometry`,
  `view::block_render::spawn_segment_child`/`spawn_rect_child`); this pass
  was done without running the app (the runner was down), so unit tests
  cover the pure math but not pixels. Specifically worth eyeballing:
  - ~~Sparkline segment rotation / fill shape~~ — MOOT (2026-08-12): both
    sparkline surfaces moved off UI-node rectangles onto flat triangles in
    the block/dock texture (`build_sparkline_vertices`), with the true
    trapezoid fill; the rotated-segment and bar-tiled-fill code is deleted.
    Both surfaces live-verified 2026-08-12: dock via BRP + Amy's eyes
    (flicker gone), block-cell via a ```sparkline fence in a test context
    (renders correctly first try — likely its first-ever live render).
  - **Role divider label vertical centering + line thickness** —
    `fieldset::ROLE_LABEL_FONT_SIZE`/`ROLE_DIVIDER_THICKNESS` preserve the
    pre-shader Vello values exactly on paper; worth a glance since the
    label now goes through MSDF (a different glyph pipeline) rather than
    Vello's `draw_glyphs`.
  - ABC (`text/abc.rs`) moved off vello too, in the `msdf-music` branch —
    it now renders through MSDF glyphs + `MsdfBlockGeometry`'s flat-colored
    triangles, no vello scene at all. The music-notation merge needs its own
    visual pass — same "unit tests cover the math, not pixels" caveat applies
    to staff lines/beams/slurs/ties and glyph placement. It surfaced two
    merge-only bugs worth knowing about if something regresses further:
    `extract_msdf_blocks`'s render-world query briefly required
    `&MsdfBlockGeometry` unconditionally, which would have silently dropped
    role headers, the shell dock, the compose overlay, the editor surface,
    and time-well cards from MSDF extraction (none of them carry that
    component) — fixed to `Option<&MsdfBlockGeometry>` before it shipped.
    Separately, devello's MSDF rewrite of the `Image` placeholder label
    independently reintroduced the exact `scene_version`-derived
    `MsdfBlockGlyphs.version` bug the `msdf-music` branch had already fixed
    elsewhere (see that component's doc comment) — also fixed before it
    shipped.

- **SVG block rendering off vello — also needs a visual pass** (2026-07-30,
  `feat/svg-cpu`, branched from `feat/devello`; same "no running app during
  the change" caveat as the de-vello pass above). `text/rich.rs`'s
  `RichContentKind::Svg` now carries a parsed `usvg::Tree` instead of a
  pre-rendered vello `Scene`; `view::block_render`'s `Svg` arm rasterizes it
  via `resvg`/`tiny-skia` (`text::svg_raster`) into a straight-alpha RGBA8
  `Image`, uploaded as a child `ImageNode` (same `ContentGeometryChildren`
  despawn/respawn convention as sparkline/image). `vello_svg` is gone from
  `Cargo.toml` entirely. Round-trip pixel tests (`text::svg_raster::tests`)
  cover the
  premultiplied→straight alpha math and a real resvg render of a small
  shape, but not the on-screen result. **First live sighting (2026-08-01)
  found two real defects, both fixed on `fix/svg-cat-sizing`:**
  `fit_svg_to_box` filled the box rather than fitting into it, blowing a
  200x200 cat SVG up 4.74x to 948x948; and block cells spawned with Bevy's
  default `flex_shrink: 1.0`, so the SVG cell — whose content is an
  absolutely-positioned child, giving it a zero automatic minimum size —
  absorbed the whole scroll column's shrink pressure and collapsed to
  `ComputedNode.size = [1896, 0]`, letting its raster paint straight
  through the neighbouring blocks' text. Still worth eyeballing:
  - **Any SVG with `<text>` elements** — `SvgFontDb`'s fontdb still feeds
    `usvg::Options`, unchanged in shape, but this is the first real exercise
    of that path through the new raster (previously vello's own
    `draw_glyphs` rendered usvg's resolved outlines; now resvg/tiny-skia
    does).
  - **HiDPI crispness** — the raster is sized from the block's PHYSICAL
    pixel box (`ComputedNode` × `TextMetrics::scale_factor`) and
    re-rasterized on a DPI-only change (`BlockScene::svg_raster_physical_size`
    staleness check in `build_block_scenes`); confirm on an actual HiDPI
    display or scale-factor change that SVGs stay crisp rather than
    blurring or going stale.
  - **Malformed/unparseable SVG** — unchanged fallback (parse failure logs
    and falls through to the plain-text/markdown path), but worth a manual
    poke with genuinely broken markup to confirm it still reads as "here's
    the raw text," not a blank block.

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
- **Verify one unexercised render surface:** the unfocused-pane summary, the
  one surface on Bevy's native `Text` pipeline (`tiling_reconciler`), needs a
  multi-pane layout to eyeball. All MSDF-only surfaces (including docks,
  role borders, and — since 2026-08-12 — the North/South dock chrome
  itself) verified via build/test; the "Vello-content cell" category this
  entry used to also name is gone entirely along with vello
  (`has_vello_content`/`render_vello_scenes` no longer exist).
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

- **kaish 0.13 `--json` migration: kj's per-leaf `json: bool` fields are dead
  code via the live kaish bridge (found + confirmed pre-existing 2026-07-18,
  kaish 0.13 `--json` migration).** kj now adopts kaish 0.13's global `--json`
  (kaish's `finalize_output`/`apply_output_format` render every `ExecResult`
  from `.data`/`.output`/`.latch`; kj's own `render_json_envelope` is gone —
  `runtime/kj_builtin.rs`). Auditing every subcommand's `.data` payload turned
  up a pattern that predates this migration: `doc list`, `config
  list`/`show`, `rc list`/`show`, and `search` each declare their OWN local
  `json: bool` clap field and build a richer JSON *message* when it's true
  (e.g. search's `{matches, total, truncated}` with full match context/lines,
  vs. `.data`'s flat array of block ids). That field can only ever be `true`
  when `KjDispatcher::dispatch()` is called directly (as the per-file unit
  tests do) — `KjBuiltin::execute` has ALWAYS stripped `--json` out of the
  argv it hands to `dispatch()` (needed so leaves that don't declare `json`
  don't reject it as unrecognized), so the richer branch has never fired via
  a real shell/MCP call, before or after this migration. Under kaish's
  `--json` now, these commands emit exactly `.data` (the flat id array),
  which is unchanged from what users already saw under the old
  `render_json_envelope`'s `data` key — so nothing regressed — but the richer
  object (`search`'s match context is the one with real information loss) is
  worth either wiring for real (thread the subcommand's OWN `--json` local
  flag through as `.data` instead of a discarded message, or drop the dead
  branch + local `json` field entirely) next time one of these files is
  touched.
- **`kj config set`/`kj config edit` (write branch) and `kj rc edit`
  (content branch)/`reset`/`rm` have no `.data` on success (found 2026-07-18,
  kaish 0.13 `--json` migration).** They return a plain `KjResult::ok(msg)`.
  Under kaish's `--json`, a text-only success with no `.data`/`.output` wraps
  the human message as a JSON *string* (`"set config 'theme.toml' (7
  bytes)"`), not a structured record — matches kaish's documented contract,
  not a bug, but inconsistent with sibling verbs (`doc create`/`delete`,
  `block append`, `rc show`) that already attach a small record. A
  `{"path": ..., "bytes": N}`-shaped `ok_with_data` would bring these in
  line if a caller ever wants `kj config set ... --json` to be
  machine-parseable beyond "some JSON string came back".
- **`kj db` read window into `kernel.db` — deferred, not built (feedback 2026-07-18,
  DeepSeek tracks-discoverability).** When DeepSeek couldn't find a track-listing
  surface it fell back to sqlite and hit a wall: the kaish sandbox blocks
  `sqlite3`/`python3`/`node`, and there's no `kj db` verb — the DB is a black box
  from inside the kernel. The *track* need is now met by `kj transport list`
  (merged persisted+live roster), so this is no longer urgent. But a read-only
  `kj db tables` / `kj db schema` / `kj db dump <table>` (NOT arbitrary SQL) would
  turn other "the kernel has the answer but won't tell you" moments into a one-liner.
  Shape: reflect over the same `KernelDb` connection, emit `.data` rows; keep it a
  read (no write path) and out of the loadout's `transport`/config gates. Watch the
  standing rule — never hand out raw SQLite; go through a typed `kj`/MCP surface
  ([[feedback_no_direct_kernel_db_access]]).
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
- **kaish binder eats a literal `--json` inside trailing var-args** (found
  2026-07-17 while fixing the kj-side strip): a literal `--json` token riding
  a `trailing_var_arg`+`allow_hyphen_values` positional (e.g. `kj drift push
  dst hello --json world`) is pulled into the global `--json` flag by kaish's
  own binder *before* `ToolArgs` is built — the kj-side fix (which now reads
  the structured `args.flags`) can't see it; the token is gone by then.
  kaish-crate work (`~/src/kaish`), same family as the `local` reserved-word
  lexer footgun below.
- **`KjBuiltin` argv/stdin quirks (gemini-pro review 2026-07-04, both
  pre-existing/low):** (b) `wants_stdin_content` promotion means
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
- **Autonomous turn runaway guard:** Add a `drive_depth` cap to prevent unbounded fan-out from `--prompt` forks.
- **TurnFlow catch-up for late/reconnecting subscribers:** the *lossy* half is FIXED (2026-08-05, the FlowBus backpressure rework — a live subscriber can no longer miss an event; it is terminated with an explicit signal instead). What remains is the **catch-up** story: a client that subscribes *late*, or reconnects mid-turn, was never a subscriber when the outcome was published, so it still misses `turn.completed`/`turn.failed` and must fall back to reading the block log — which recovers *what* the turn wrote but never *why it stopped* (`TurnStopReason` has no block-log shadow; `EndTurn`, `MaxTokens`, and a soft cancel all leave the same `Done` block behind). Deliberately un-journaled (blocks are the durable record; replaying completions after a restart would announce turns nobody is waiting on), so the fix is a bounded per-context "last outcome" the subscriber reads on attach, not a journal.
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
  already created from a stale seed keeps its broken loadout and must be
  repaired from a binding-admin context. **No longer structurally blocked**
  (2026-08-11): cold start now seeds a ROOT director with admin + rc-write
  (`crates/kaijutsu-server/src/rpc.rs:2112`), so the authority to repair
  exists — what's missing is the repair path itself, not a context to run it
  from.
- **`local` is a kaish reserved word (like `set`).** `--model local` lexes as
  the `local` builtin keyword → `found ';' expected identifier`. Same class as
  the `set` reserved-word gotcha; quote it (`--model "local"`) or pass the full
  spec. Consider letting reserved words bind as plain args after a flag.
  (kaish-lexer change in `~/src/kaish`, not kaijutsu-side.) NOTE: alias
  *resolution* is now fixed — `kj context create/set --model "local"` expands
  the registry alias entry (then models.toml [model_aliases]) to its concrete `provider/model`
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
- **External shell-hang fix — one residual verification.** The 2026-06-17
  executor-starvation hang is fixed (`SubscriberHealth` reap tolerance +
  `resubscribe_blocks` + joined-context-scoped subscription; story in devlog).
  The server fix is verified live; the *client-side* scoping + resubscribe (2,3)
  ride in the MCP binary and are covered by `e2e_shell` until a session whose MCP
  binary is rebuilt confirms them in situ. Related: P3 above +
  `project_mcp_synceddocument_sync`.
- **Live-kernel follow-up: any long-lived kernel may still be serving a stale
  mcp-default model id (verified 2026-07-17).** Re-checked the "mcp-context
  default model is an invalid id" bug filed 2026-06-17: the bad ids
  (`…-20250101`, `…-20250929`) don't exist anywhere in tracked source or
  history except this doc's own bug report — `models.toml`'s
  `default_model`/aliases and `DEFAULT_MODEL` have always read the valid
  `claude-haiku-4-5-20251001` since the TOML config was introduced. But
  `/etc/config/models.toml` is CRDT-owned and seeded absent-only
  (`config_seed.rs`, `config_crdt_fs.rs:349-378`) — a kernel whose CRDT
  config predates whatever earlier fix corrected this would still be
  serving the stale value, since a context's model is baked in at creation
  time from the live registry (`rpc.rs` `create_context_inner`) and restarts
  never re-seed an already-present file. **2026-08-03: this whole failure
  class is structurally dead** — models.toml no longer exists; defaults live
  in the `llm_defaults` table. If a stale default ever recurs, the remedy is
  `kj backend default show` / `set` (or `kj backend reseed`), all live-reload.
- **`builtin.file` hardening — remaining (small; the byte→char corruption fix +
  hashline addressing shipped 2026-06-17, story in devlog +
  `project_file_tools_hashline`):** the in-context recovery affordance
  *shipped* 2026-08-01 as `kj diff` (`docs/diff.md` slice 3) — one path diffs
  disk against the CRDT document that owns it, and `--from <seq>` replays the
  journal, so "what did the agent change, and what did it used to say?" is
  answerable in the shell. Still open: (1) the post-write verification reads the
  CRDT cache, not the VFS disk, so a faulty flush is only caught by
  `flush_one`'s own error (documented in `edit.rs`); (2) `FileDocumentCache`
  CRDT-native pass-through (tracked under Persistence & Sync) would let `read`'s
  hashes anchor `/etc/rc` cleanly.
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
  manifest (pooling strategy) beside embedding_config in the kernel db. The `Embedder` trait is the seam;
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
- **In-world ring labels — still TODO, and now cheap.** "ACTIVE" / "RECENT"
  floating at each ring, per `docs/timewell.md` "The bowl, revisited". The old
  pure helpers (`card::band_label_pos`/`band_label_text` + their radius
  offset) were deleted with the labels themselves 2026-07-06 and finally with
  the ring collapse 2026-08-01 — nothing to resurrect, and only two labels to
  write. Wiring is an MSDF panel per ring (`panel::create_msdf_panel`, the
  `HorizonLabel` pattern, which already parks a label in world space), gated
  on font-asset load the same way `text::build_card_scenes` is, and —
  landmine — pass the brush explicitly to `VelloFont::layout`/
  `collect_msdf_glyphs` or the text renders black. Open question first: with
  two rings and the reading card's SPECS `band` line, is a label earning its
  clutter?
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
- **Horizon dive — front door built, room behind it isn't (2026-08-01).** The
  ring collapse made the event horizon a real place (the accretion disc on the
  room floor) and bound `h` → `Action::ActivateHorizon` in the well, but the
  handler only logs `"horizon dive: not yet built (see docs/horizon-dive.md)"`
  — and **that doc does not exist yet**; the prototype is in flight elsewhere.
  Two things to close: write/land `docs/horizon-dive.md`, and replace the stub
  arm in `time_well::scene::well_keyboard`. This is where Stage 5's
  search-at-the-horizon should surface (`docs/timewell.md`, Stage 5).
- **Two of four terrace centerpiece variants are now unreachable.**
  `assets/shaders/terrace_ring.wgsl` picks a ring's centerpiece with
  `ring_index % N_VARIANTS` (N_VARIANTS = 4: barcode, rosette, moiré dial,
  and the fourth). With two rings only variants 0 and 1 ever draw. Nothing is
  broken — `GLYPH_FORCE` already exists to audition any of them — but the
  pairing is now a *choice* rather than a rotation, and Amy hasn't picked
  which two she wants. (While in there: the material passes `ring_count` as
  `glyph.y` and the shader never reads it — a dead uniform channel that
  predates the collapse.)
- **Horizon sediment arcs.** The "+N" is a bare count. Stage 5's original
  bullet wanted per-track sediment arcs on the disc so the mass reads as
  *whose* — now genuinely cheap, since the disc is a first-class floor
  feature with the well's activity data already flowing into its material
  (`well_rings.wgsl` ripples). Wants Amy's eyes before anyone builds it.
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
  - *Passive-aging short-circuit* (gemini, design). `sync_time_well` early-exits
    on an empty join diff, so cards don't re-band as wall-clock time passes — a
    context won't drop out of RECENT (past the horizon) on idle alone until
    some *other* diff arrives. Ties to the ring-MEMBERSHIP / coarse auto-decay thread (explicit
    hot-row + coarse decay, see `signoff.md`): the band derivation likely needs a
    coarse timer independent of the block diff.
  - *Spin chaining on rapid reversal* (deepseek, medium → downgraded on code-read).
    `spin_target_to_gate` measures the short path from the accumulated *target*, not
    the eased position, so a very fast direction-reversal could feel like the ring
    keeps going before reversing. **Not a correctness bug** (resting target is the
    gate π, steps are one-card; math verified sound by both models) — a possible
    feel-tuning item only.
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
  document finds the Chameleon spec (B♭ Dorian, B♭m7–E♭7 vamp, bass chair)
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

## Archive-time summaries, written by a local model (Amy, 2026-08-03)

When a context is archived it stops changing. That makes archiving the natural
moment to generate a small summary once and keep it forever — no invalidation
problem, because the thing it summarises is frozen by definition. Good work for
a local model: it is not latency-sensitive, it happens on an explicit event
rather than a hot path, and it never needs to be redone.

Two payoffs. Browsing archived contexts stops being a list of labels you have
to remember the meaning of — a card can say what the context *was about*. And
any search over the archive gets real prose to match on instead of a title plus
keywords; the horizon dive's ranker (`docs/horizon-dive.md`, "Where a real
search plugs in") would benefit directly, as would `SemanticIndex` when it
lands behind it.

Open: where the summary lives (a field on the context handle vs. a block in the
context itself), which local model, and whether concluding/demoting should get
the same treatment or only full archival. Not needed for the dive's v1 — that
slice ranks on `label` + `keywords`, which exist today.

## kaijutsu-mcp — June 2026 SyncedDocument migration review

Surfaced by a DeepSeek (concurrency) + Gemini (architecture) review of commit
`ac5f518` (Remote backend cut over to `kaijutsu_client::SyncedDocument`). The
dropped-stdout bug and the content/exit_code completion race are fixed (poll now
does an authoritative `get_context_sync` read after terminal status); these are
the *remaining* findings, triaged.

- **Sole-writer command channel SHIPPED 2026-07-17** (`doc_task.rs`; the old
  HIGH hook-authoring-vs-resync entry is RESOLVED — sole writer, dedicated
  pushed frontier, flush→apply window closed by construction, resyncs
  coalesce, flush-failure aborts the swap).
  **SUPERSEDED 2026-08-13 by slice 3 of the CRDT-position migration**
  (`docs/crdt-position-2026-08.md`): the hook path authors over
  `authorBlock`/`completeBlock`, so the mirror has no local writer and the
  pushed frontier, the flush, and the flush-failure abort are all *deleted*
  rather than fixed. The guarantees above still hold; they are now structural.
  Keep the entry for the reasoning, not the mechanism. Remaining follow-ups
  from its kaibo review:
  - ~~**Hook-ack latency under a long fetch (watch item)**~~ — **moot.**
    Authoring no longer queues behind a resync fetch at all. The latency
    concern did not disappear, it *moved*: the hook now waits on three
    sequential RPCs instead of a local ack, bounded by `tiers::HOOK_PATH`
    under Claude Code's 5 s deadline (`af45445e`). Same 5 s ceiling, a
    different thing pressing against it.
  - **`ContextResynced` events ride `ApplyEvent` and are ignored**; the
    bridge could convert them into a direct apply_sync_state and save one
    fetch on reconnect. Optimization, unclaimed.
  - **`pending_events` on FullSync are CLEARED, not replayed** (decided:
    replay is unsafe — header setters stamp fresh local ticks, so a stale
    replay would overwrite newer data; comment on `apply_sync_state`).
  - **resubscribe-acks-before-subscription-active** leaves a narrow
    incremental-event gap; the stall resync covers it for shell state.
- **LOW — `renameContext` RPC has no structured result channel.** The 2026-07-17
  server-side handler (`kaijutsu-server/src/rpc.rs`) returns errors via
  `Promise::err` because `renameContext @29` declares no results — a caller can't
  distinguish "label taken" from "connection broken" (`conclude`/`promoteContext`
  use `(success, error)` result fields). Fine while the only caller (hook
  listener's session-suffix rename) just logs; add `-> (success :Bool, error
  :Text)` if a caller ever needs to react. (kaibo deepseek review 2026-07-17.)
- **LOW — `agent.stop` transcript read is unbounded.** `HookListener` reads the
  whole transcript JSONL (`tokio::fs::read_to_string`) to extract the last
  assistant message; long sessions reach tens of MB. Truncation applies only
  after extraction. Cap the read or reverse-scan from the tail. (kaibo deepseek
  review 2026-07-17.)
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
- **LOW — `InvokePeerRequest.params` generates an untyped MCP schema property**
  (found 2026-07-17 while closing the double-encoding entry): the field is
  `serde_json::Value`, so the derived tool schema gives `params` no `type` at
  all — which is likely why calling layers stringify objects into it (the
  double-encode `normalize_peer_params` now tolerates). Upstream fix shape:
  annotate the schema with `"type": "object"` so callers encode a real nested
  object in the first place.
- **PERF follow-up — the shell poll's authoritative read pulls the full context
  snapshot per command** (`execute_and_poll_shell`, Phase 2). Fine for short MCP
  contexts; a per-block read RPC (`actor.get_block(ctx, id)`) would avoid the
  O(blocks) transfer for large conversations.
- **TEST gaps beyond `tests/e2e_shell.rs`:** no coverage for Remote
  input tools, the hook-listener socket path, prompts, resources, or
  reconnect/resync. Add e2e cases (the harness in `e2e_shell.rs`
  generalizes).

## Testing & Tooling

- **Clippy has no floor, and a warning backlog behind it (2026-08-14).** The
  repo has no `[lints]` table, no `clippy.toml`, and no CI, so clippy is red
  only when someone runs it by hand with `-D warnings` — which is how the
  reconnect lane tripped over a `reversed_empty_ranges` error in a test whose
  reversed range was the assertion (fixed with a commented `#[allow]`,
  `b103f8b1`). Underneath it, `cargo clippy -p kaijutsu-app --all-targets`
  reports **51 warnings (31 duplicates)**, ~20 real: `field_reassign_with_default`
  in `view/time_well/scene.rs` tests, items-after-test-module, and 4 with
  machine-applicable fixes. **Amy 2026-08-14: a subagent cleans this up on the
  morning of 08-15.** Two things for whoever briefs it. (1) Do it *after* the
  in-flight lanes merge — a `--fix` sweep across three branches is how you get
  a bad merge, and the warnings are not going anywhere. (2) **A cleanup without
  a floor rots**: 20 warnings come back the same way they arrived. Land a
  workspace `[lints.clippy]` floor with the sweep, or the next lane re-files
  this entry. Which lints belong in that floor is still Amy's call — the
  cleanup is decided, the standard is not. Note `rustfmt` is disabled repo-wide
  on purpose (README "Code Style"); clippy is a separate question and the
  rustfmt rationale does not carry over.

- **russh teardown panic:** `ChannelCloseOnDrop::drop` panics with "there is no reactor running" in tests.
- **`vfs::backends::local::tests::test_normal_paths_succeed` is flaky under
  full-workspace parallelism** (found 2026-08-02 verifying the compaction
  removal). It failed once in a `cargo test --workspace` run, then passed in
  isolation and in two full `-p kaijutsu-kernel --lib` runs (worktree and
  main). So it's order- or parallelism-dependent, not a real regression —
  most likely shared temp-dir or process-CWD state between tests. Worth
  chasing: a test that fails only sometimes trains us to ignore red, which
  is the expensive failure mode. Note also that `cargo test … | tail` hides
  this — the pipe's exit status is `tail`'s, so the run reads as passing.
- **`kj` help-doc siblings: no consumer, unaudited.** `kj.md`'s command table
  was regenerated from the real clap tree 2026-07-17 (cleanup batch); the six
  siblings (`kj-cache/context/drift/fork/preset/workspace.md`) still have
  **no consumer** (only a doc-comment mention of `kj-cache.md`) and predate
  the clap migration. Decide their fate: delete, or wire as `kj <cmd> help`
  bodies — and audit against the clap tree first (start with `kj-context.md`
  + `kj-fork.md`; those commands gained the most verbs). NB `docs/kj-help`
  is a symlink into that dir — not a docs-cleanup candidate.
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

**CRDT data model:**
- `calc_order_key` calls `block_ids_ordered()` (O(N) sort) on **every** insert
  (`kaijutsu-crdt/src/block_store.rs:390`); the bench exposing it is `#[ignore]`d.
- Tombstones aren't a first-class `BlockSnapshot` field — they ride a side
  `deleted_blocks` list re-applied by hand (`block_store.rs:1637`).
- `StoreSnapshot` has a breaking-format note with no migration path ("delete
  existing databases when upgrading", `block_store.rs:1680`).

**`kj` single-source guarantee is manual** — `dispatch()` routing and
`kj_command()` schema tree must be hand-kept in sync; a subcommand added to one
but not the other is unreflectable (`kaijutsu-kernel/src/kj/mod.rs:589`).

**Types-crate layering** — `ThemeData` (~60 visual fields + `include_str!` of
`assets/defaults/theme.toml`) lives in the foundational `kaijutsu-types`
(`theme.rs:59`). Belongs in a UI/config crate.

**`kaijutsu-index`:**
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
- `is_disconnect_error` matches on the capnp error `Display` text
  (`actor.rs:1214`) — fragile; a capnp formatting change would stop triggering
  reconnect. Prefer a typed `ErrorKind::Disconnected` match.

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

## Cache & cost — decided direction (2026-06-24)

*(Promoted from the Gemini CLI survey when that wishlist moved to
`docs/wishlist-gemini-cli.md` on 2026-08-11 — unlike the survey, this is a
locked decision with concrete remaining work.)*

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
    usage capture for the EMA); side effects = the script calling builtins (drift, VFS),
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
- **`kj context list` registry/DB divergence — narrowed and partly shipped
  (2026-08-04, `register_session` upsert work).** Re-investigated while
  building `register_session`'s upsert/attach fix (was going to "heal the
  registry on attach"). Findings against current code:
  - `create_shared_kernel`'s boot-time recovery step (`rpc.rs`, "Recover
    contexts") already re-registers every context `KernelDb::list_active_contexts`
    returns into the DriftRouter on EVERY kernel start — confirmed with a
    same-process double-boot test
    (`list_contexts_recovers_live_context_after_restart`,
    `crates/kaijutsu-server/tests/context_label_resolve.rs`). So the
    original symptom described here (a live/concluded MCP context surviving
    a restart but vanishing from `kj context list`) does NOT reproduce
    against current code — it looks stale, possibly already fixed
    incidentally by unrelated work landed since 2026-07-03, or the original
    live observation involved something the synthetic restart here doesn't
    capture (e.g. a torn/non-graceful shutdown, or two server processes
    briefly both live). Flagging rather than silently deleting, per 改善 —
    if the symptom recurs, treat it as a genuinely different bug, not this
    one.
  - The one real, provable registry gap: `list_active_contexts` filters
    `WHERE archived_at IS NULL`, so an ARCHIVED context's DriftRouter entry
    does NOT survive a restart even though its KernelDb row and BlockStore
    document do. **Shipped**: `joinContext` (`rpc.rs`'s `ensure_context_joinable`)
    now heals this — re-registers from the durable KernelDb row instead of
    hard-failing with "use createContext first" — and a passing regression
    test covers it end-to-end over the wire
    (`join_context_heals_registry_for_an_archived_context_after_restart`,
    same test file): archived-context join fails before the fix, succeeds
    and reappears in `listContexts` after.

---

## `ExecResult.output` can't carry structured data past kaish's output limiter (found 2026-07-18, rich_json wire-through)

`kaish-types::ExecResult::materialize()` (`result.rs`, invoked from
`spill_if_needed` whenever `ctx.output_limit.is_enabled()` — true for every
`EmbeddedKaish`, which always runs `OutputLimitConfig::agent()`)
unconditionally clears `.output` at the end of the function, even when `.out`
already carries independent text and `materialize()` therefore never actually
consumed `.output` to build it. So `.output` cannot be used as an independent
structured side-channel alongside a human-readable `.out` message — any
builtin that sets both (as `kj` commands do: `message` for `.out`, `data` for
structured payloads) has `.output` silently dropped before
`execute_with_options` returns to the caller. Confirmed live: setting
`.output` in `kj_builtin.rs`'s `KjResult::Ok` arm produced `ExecResult
{ output: None, .. }` by the time kaish-kernel's `EmbeddedKaish` handed the
result back (regression-pinned by
`kj_output_channel_does_not_survive_the_kaish_output_limiter` in
`crates/kaijutsu-kernel/src/runtime/kj_builtin.rs`).

**Current workaround (shipped):** `kj` keeps writing only `.data` (the kaish
`$()`/for-loop sideband, which does survive materialize/spill_if_needed
intact). `crates/kaijutsu-server/src/rpc.rs`'s `block_output_data` bridges
`.data` → a rich_json-only `OutputData` at the block-persistence seam in
`execute_shell_command`, so the structured payload still reaches the block
(→ MCP `shell` tool `data`, → the app's `block.output`) even though `.output`
itself never carries it through the shell layer.

**Cleaner fix (not done):** only clear `.output` in `materialize()` when it
was actually consumed to populate `.out` (i.e. move `self.output = None;`
inside the `if .out.is_empty()` branch, or add a distinct "detach" method for
`spill_if_needed`'s disk-spill call site). Lives in the sibling `~/src/kaish`
project (published as `kaish-kernel`/`kaish-types` 0.12.0), needs a version
bump + `Cargo.lock` update in kaijutsu — worth doing if more callers want an
`.output`-native structured channel independent of `.out`.

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

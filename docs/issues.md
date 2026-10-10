# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry — if the "how we got here" is worth keeping, move the narrative to [`devlog.md`](devlog.md) (the landed-work story). See `AGENTS.md`, "Writing, memory, and git".

---

## Anticipation and commitment — iteration order

The kernel coordinates anticipation and commitment on the shared pulse.
Iterate through one observable performance with producers at different
speeds; each change should improve that flow, remove a competing mechanism,
or establish a testable contract. Keep model placement independent of this
work. `docs/audio-inference.md` records the workload and measured costs.

1. **Keep interaction dependable.** Continue the complete kaish/rc caller
   migration alongside the following steps, using `docs/kaish-integration.md`'s
   inventory. Each migrated caller must retain identity, cancellation,
   complete output, and terminal
   settlement, with the old path removed. These are the means of playing
   the instrument, so verify them through the actual client as well as unit
   tests.
2. **Prove different producer speeds on one pulse.** Done:
   `crates/kaijutsu-server/tests/timeline_commitment_wire.rs` drives
   controlled producers against manual beats through the SSH client. Fast,
   slow, obsolete, and failed producers share one pulse; stale output never
   commits; a failed producer plays its declared fallback once.
3. **Align execution with that proof.** Audit admission through completion
   and commitment against exact work ownership and intended musical time.
   Keep slow preparation outside timeline locks; give attempts bounded
   resource admission (`docs/resource-admission.md`) and explicit
   cancellation/shutdown behavior. Preserve
   a small synchronous commit step. Use existing runtime and resolver seams;
   delete superseded paths as the scenario starts passing.
4. **Expose the feedback needed to play.** `kj transport work --track`
   reports each attempt's intended tick, admission, start, readiness, basis
   validity, and disposition (committed, fallback with its reason,
   cancelled, superseded), with the estimate and planned preparation tick
   beside them, as a table and as JSON. Producers report measured queue
   and compute time on results and errors, and each timeline keeps the
   newest 32 samples per resolver (`docs/hyoushigi.md`). Open: nothing
   reads the window yet (`estimate_cost` is a fixed guess, and the model
   output resolver estimates zero); transfer durations wait for media
   delivery in the scenario; no client shows work yet. Measure under overlap
   before choosing lead-time targets; avoid turning unmeasured percentiles
   into guarantees.
5. **Exercise replacement, then extract.** Run the same scenario with one
   real producer and CAS/SFTP media delivery, observing it through a client
   and audiod when hardware is available. Use the evidence to choose which
   model-specific behavior becomes an optional tool or resolver adapter.
   `kj audio beats` is a candidate; its new home remains undecided. Remove
   the kernel dependency and obsolete verb only with a verified replacement
   or an explicit decision to drop the feature.

The first day's useful checkpoint is a repaired interaction path and a
repeatable scenario that exposes the next scheduling gap. A controlled
failure is useful evidence; a new model integration is not required to
prove the coordination contract. Update the checklist as each step lands,
and correct adjacent docs/comments in the same change.

### Measure and recover admitted timeline work

Live model score work now binds an absolute target and a seed/score basis at
admission, with turn-owned delivery and timeline-owned cancellation. The old
completion listener is deleted. Controlled SSH tests exercise both the generic
producer scenario and timed `kj drive`, including the shipped musician tick
script. See `docs/hyoushigi.md`, "Model turns with an intended score tick".
The basis deliberately excludes the changing conversation, tool inputs and
cross-track heard view. Broader dependencies need an explicit projection;
current model handoffs do not automatically replay their producer on invalidity.

CAS preparation has a process-wide limit of four operations. An uninterruptible
host read retains its slot even after its owner is cancelled; four stuck reads
would stop further CAS preparation until a read returns or the process restarts.
Timeline deadlines still settle waiting work and the pulse keeps advancing. The
20 ms cost estimate remains an initial guess, not a measured size/queue model.
The open-work bound counts operations, not bytes; ready-source and committed CAS
residency still need measurement before admitting large rendered artifacts through
this adapter (placed clips carry small records referring to media in CAS).

Feedback retains its original source anchor when available and retries missing
anchors or failed writes on subsequent pulses. The delivery cursor and bounded
work history remain live state, not restart recovery or durable work provenance.
A persistent write fault also holds later feedback behind the failed event.
Measure and design recovery before promising durable admission or delivery.

## First System 1 benchmark runs (2026-10-04, Sonnet 5.5, deny mode)

Five Sonnet runs of db-wal-recovery under contrib/bench/gate-council.toml
with Harbor `--ak permission_mode=deny`. Each found the XOR key, tried to
back up first, and ended `blocked` with a correct plan and an honest
account; none passed. What each step fixed:

- The overlay refused a new context type: S04 installs `council` itself.
- `council-system` reached the server empty: the projection sends only user
  and model text blocks, and S04 wrote the rules as a system block. Fixed.
- The kernel's JSON case reads as suspicious: `xxd data.txt` scored 0.95 as
  plain text and 0.56 as `case_state` JSON. A rules paragraph that names
  the fields lifts routine work (about 0.87-0.92) and keeps risky commands
  low. Rendering the case so a reader model needs no explanation is worth a
  look in `council/gate.rs`.
- Remaining: running a script the seat wrote (`python3 /tmp/fix.py`)
  scores about 0.5, because the council cannot see what the file does. In
  an unattended deny run that is a dead end. The program spec (docs/council.md
  rollout step 4) and a bump outcome (refuse with the readout and invite a
  retry) are the next design steps.
- The kernel logs a council answer loudly only for `report`; allow and ask
  answers are in the ledger and the `council_*` tables but not in the log
  a Harbor run keeps. A Harbor run cannot show why a command was refused.

Exploration settings used: `allow_at` 0.7 to 0.9, `mass_floor` -0.3, and
read-only inspection programs in `[global] allow`; the committed gate file
keeps the documented example thresholds.

kaish also refused `cp -p` ("cp: -p is not supported (see `help cp`)"), so a
DeepSeek run's whole backup line did not run and its next call destroyed the
WAL. Accept `-p`, or have the refusal name the fix.

## Council gate: what 43c4a61c left open (2026-10-04)

- **The observation shares the sync lock with the next decision.** A slow
  observation's prepare can delay the next gate decision into a "deadline
  passed during prepare" miss. Bounded by both deadlines; measure before
  splitting the lock.
- **`timeout_ms` is the whole deadline, not the time left.** After a slow
  prepare, the server may work past the point the kernel stops waiting.
- **Observing after a miss.** Observations run whatever the gate's outcome,
  a miss included; docs/council.md does not say whether they should.
- **A collected allow that never ran blocks the council for good.** The
  worker-run guard counts an allowed, redeemed ask with no shell operation
  as unsettled. An ask spent without running (`KernelDb::end_held_ask` →
  `Spent`) links no operation, so that command (or the same statements
  under the same label) in that context is never council-allowed again.
  It fails toward the ask, but permanently. Record a no-run settlement, or
  exclude spent-without-run asks.
- **The worker-run check scans under the lock.** On the RPC path the label
  is the tool label, shared by many asks, so each check reads every
  collected allow with a stored command under that label in the context.
  The cost grows with approval history; unmeasured.
- **No kernel test for the same-statements match.** `asks_about_submission`'s
  digest-set-under-one-label branch is tested only through the ledger query,
  not as `approved_run_unsettled` uses it (e.g. `touch  x` against `touch x`).
- **A prepare miss records empty identity fields.** `council_decisions`
  makes `server_*` and `spec_id` NOT NULL, but a miss inside `prepare` has
  neither, so the gate writes empty strings and the cause explains. Make
  them nullable or record the miss in its own shape.
- **Untested path:** the unlinked-record fallback, used when the gate stops
  before an ask exists; reachable only on faults.
- **A kept ask can lose its notice to a narrow race.** The worker can claim
  an answer after a turn's wait returned but before `leave_to_worker` runs;
  it then still sees a holder and skips the completion notice and the
  conversation-cache eviction while the command runs. In the report flow the
  interrupt fires before the hold exists, so the window is microseconds.
- **The decision record does not store the report stop.** It is on the
  span and the event only. Durable would mean an additive column written
  after the ask's transaction commits.

## Council programs: what the program decision left open (2026-10-04)

- **Interpreters and wrappers not read.** `node`, `perl`, `ruby`, `uv run`,
  `poetry run`, `npx`, `xargs`, `find -exec`, `sudo`, and kaish `source`
  run program text the council does not see; only the shell decision reads
  them. Add each the way `council/programs.rs` reads python and the shells.
- **Writes the plan cannot see.** A file written through a word that
  expands (`cp x $dst`) or by a program the submission runs earlier is
  judged by its text at the gate, and the re-hash before execution happens
  before the submission writes it. A program the submission runs earlier is
  itself judged, so its writes are in front of the council; an expanding
  copy is not.
- **Imports are named, not read or hashed.** A local module listed in
  `imports_not_shown` can change between the decision and the run without
  the integrity check noticing.
- **Two deadlines in sequence.** The shell decision waits for the program
  decisions, so a submission with programs can wait twice `deadline_ms`.
  One shared deadline would bound it but starve the shell decision after a
  slow program decision. Measure from the logged `program_ms` and
  `shell_ms`.
- **`kj ledger show` does not render `council_programs`.** The signals and
  the ask's text carry the answers; the rows are reachable by SQL only.
- **A program threshold's `allow_at` is required and unused.** The program
  case decides on its rubric. Either drop the field for the program case or
  give it a meaning.
- **Pre-existing test failure.** `kj::context::tests::rc_source_edit_applies_only_to_later_lifecycle_runs`
  asserts the seeded coder stance contains "You are a coder."; ec76d3c9
  removed that sentence. The test needs the stance's current text.

## Council contexts primed by a stronger model (2026-10-05)

Amy: "we might need a thinking pass with house rules, maybe do the prime
with max again (and let's eventually add tools to help construct a
kaijutsu context via rc or something where it uses an advanced model in the
same family to do some reasoning and store that, ephemeral, easy to update
over time)."

- **What worked by hand.** `council-code` and `council-shell` hold worked
  examples whose reasoning qwen3.8-max wrote, stored as each reply's
  thinking. The program examples took program verdicts from 30 to 35 right
  of 36; the shell examples, with the rubric, caught the brief-only
  `sqlite3 /app/main.db` opens the verdict alone passed. Both were made by
  scratch scripts on 2026-10-05.
- **A house-rules thinking pass.** A raw AGENTS.md is rules stated in
  general terms, which the council model applied poorly before examples.
  One stronger-model pass per distinct AGENTS.md (the house-rules context
  is already keyed by its content) would add a model turn with reasoning:
  what these rules mean for commands a seat might run, with a few worked
  cases. Unchanged rules reuse it; changed rules get a fresh pass.
- **The general tool.** An rc step or a `kj` verb that takes a source (a
  context, a file, a set of example commands), runs the configured stronger
  model of the council model's family, and writes its reasoning into a
  council context as ephemeral turns, so they are regenerated when the
  source changes rather than kept as history.

## Council house rules: what is open (2026-10-05)

- **The seat can edit its own house rules.** The context reads the
  `AGENTS.md` in the seat's workspace, and a seat with `file:write` there
  can change it. The decision pins the head at submission, so a rewrite
  reaches the next decision, not the one it rides on. Decide whether the
  council should read a copy the kernel holds, or whether the
  `council-system` rule that changing `AGENTS.md` needs Amy is enough.
- **The budget is bytes.** `house_rules_tokens` is estimated at four bytes a
  token; the server's tokenizer may disagree. A file larger than the budget
  keeps only its start.
- **Superseded house-rules ids stay on the server.** An edited `AGENTS.md`
  is a new id, and the server keeps the old id's snapshots. The kernel
  keeps one body hash per id in memory. `DELETE` an id the kernel no longer
  produces, or let the server age it out.
- **The home directory fallback.** A seat with no working directory looks
  for `AGENTS.md` from the kernel host's home directory upward, so a
  benchmark seat that never set one reads the host's file if it has one.
  Decide whether no working directory should mean no house rules.
- **Observations do not read the house rules.** A director's
  `direction-check` reads its voice alone.
- **The zorak `council-system` lacks the house-rules sentence.**
  `contrib/council/seed-voices.kai` has it, but it runs once; replace the
  seat sentence in the live context by chat.
- **Elided program text depends on spelling.** The shell case elides a
  judged program's text only where it appears verbatim; `-c` text with
  escapes stays, and the shell decision may still ask about it.
- **Flaky test.** `council::sync::tests::a_prepare_dropped_during_a_put_sends_the_context_fresh_next_time`
  failed 2 of 3 runs once, then passed 8 in a row; origin/main passed 4 of
  4. The panic was the mock's "client closed before sending a request",
  which kills the mock's accept loop. A likely cause: a prepare dropped at
  100 ms while still connecting closes a connection empty. Let the mock
  ignore an empty connection.

## Council bump protocol: pass or bump instead of allow or ask (2026-10-05)

Amy proposed that System 1 answer pass or bump. Pass means everything looks
good and safe to proceed. Bump means "hold on, think about that some more":
the seat gets the action back with what we know and tries again, where today
a deny would refuse it or an ask would wait on a human. Amy, 2026-10-05:

- **Two gate modes, bumper and gatekeeper.** `kj/gate.rs` gains a bump
  protocol beside today's allow/ask/report. Today a retry of the same
  request replays its durable denial (`kj/gate.rs`, "this exact request was
  already denied by its assigned reviewer"), so a seat that makes a backup
  and sends the same `sqlite3` again is never judged again. A bump must not
  close the request that way.
- **A bump is a ledger entry plus a drift.** The action goes in the ledger;
  the bump goes to the seat, or later to its banto, as a drift, so it gets
  the existing context infrastructure. In a swarm a bump could send banto
  to read the ledger and decide.
- **The bump limit is a counter in the ledger.** How it is keyed (request
  digest, seat, turn) and what happens at the limit are open.
- **The bump message carries everything useful we have,** composed through
  rc so we can tune it the way we tune prompts. It directs and does not
  invite an argument. Amy: "Try harder, a different approach, or look for
  more ways to verify safety before mutations." A bump that arrives as a
  drift reaches no council context but a character's voice.
- **The ledger needs garbage collection.** The ui gets slow on the long list
  of old entries, and bumps will add more of them.

Probes on the megakernel (`signoff.md`, 2026-10-05): a `fix` question plus a
pass/bump verdict, bumping when either the verdict is bump or the fix is not
"nothing", sorted 52 of 60 graded cases. Each question is a separate letter
read, so the fix answer does not feed the verdict read; the fix menu in the
spec layer is what changes the verdict.

Built (bumper mode, `docs/council.md`, "Bumper mode"): the two modes, the
bump record and limit, and the message template. Still open:

- **Drift of the bump to the seat's banto.** A bump is a tool failure only;
  it does not reach the seat's context as a drift, nor its banto.
- **Ledger garbage collection.** Bump decisions have no ask and so do not
  fill `kj ledger`, but they accumulate in `council_decisions`.
- **Debiasing by reading both option orders.**
- **The bump count never resets.** It counts every refused bump of a
  submission in a seat context. A bump an hour ago counts after a pass in
  between; whether a pass, a context fork, or time resets it is open.
- **The shell bumper spec has no `fix` question.** The probes (60 graded
  cases, 52 sorted) used one beside the verdict; `shell-bump.json` holds the
  verdict alone.
- **A bump reads as a denial to callers.** It rides `RefusalKind::Denied`,
  whose doc says retrying gets the same answer. A `Bumped` refusal kind would
  cost a wire change.
- **Per-spec council contexts** are on main under `[[council.spec]]`; bumper
  mode touches only the `[council]` table.

## Council: a shadow voice holding the seat's command history (2026-10-06)

The council judges each command alone. It cannot tell that `.partb` is
scratch the seat made an hour ago, so a cleanup of the seat's own work looks
like destroying an original. Amy, 2026-10-06: "we may need to carefully wire
a council that accumulates command history so it can be contextual, that was
the idea for the shadow context on say a coder, each command, not the
model's full output, would go into the council member. maybe tool calls
too. not the results. so it's small and makes up the workstream of things
the model under observation has tried."

- **What it holds.** Each submitted command, and maybe each tool call, of
  the observed seat, in order. No results, no narration, no thinking. House
  rules dropped the seat's narration because it changed every few decisions
  and forced re-sends (29 in 18 minutes); an append-only command list only
  extends, so a held snapshot stays useful.
- **Design.** `docs/council.md`, "Shadow voice (design, not built)". Amy,
  2026-10-09: start with shell calls, then every tool call; the shadow is a
  fork child of the seat; get it flowing into a megakernel context and
  measure before doing more. Amy, later that day: the judge rides on the
  cast; the shadow is a command/opinion dialogue, fed per turn to council
  servers and hydrated whole for chat models; kaijutsu manages the server
  cache, no cap yet. Open items are listed in the design.

## A banto's own kj work asks its reviewer for each verb (2026-10-10)

A new `director` seat played by banto on moltar (`banto-shadow`) raised an
ask to Amy for `kj context create ... --as coder` and again for
`kj drive <coder> --prompt ...`; every kj verb it runs to do its job comes
to the human. Amy, 2026-10-10: "there should be fewer asks on a banto
operating on kaijutsu... but that can be tasks for later." Open: which
director verbs `gate.toml` or the director rc should allow (create of its
own children, drive and wait on them), and whether that belongs to the
type or to the character.

## `kj context create --cwd` accepts a directory no shell can write (2026-10-10)

On moltar, `kj context create shadow-coder-1 --type coder --cwd
/home/atobey/scratch/...` succeeded although only `$HOME/src` and `/tmp`
are writable mounts (`docs/mounts.md`). The coder's first `touch` failed,
and the DeepSeek coder then explored `~/src/bench-work` and other trees
looking for a writable place, outside its brief. Open: warn or refuse at
create when a writing seat's cwd is not under a read-write mount, and say
which mounts are writable in the coder's runtime facts.

## The judge shadow misses under a burst of calls (2026-10-10)

On moltar, `shadow-coder-2` sent about ten shell calls in two minutes at
the end of a task. The gate, judge priming, and judge reads share one
megakernel (`mk-zorak`), and four judge reads missed their 30 s deadline,
on `git clean -fdX`, `git add && git commit`, and `make clean`: the
cleanup steps whose history the judge exists for. Gate time also rose
from about 1 s to 13 s during a burst on the first seat. Open: queue or
coalesce priming per shadow (send the latest tail, not every one), give
the judge its own deadline or server, and measure priming time from the
server.

## Council presets per context (Amy, 2026-10-08)

Amy: "bumps should only be enabled on coders and optionally. maybe we come
up with some kind of preset for contexts with bumps, then we can likely put
in like, mk bumps, lfm2d bumps, and Jev! bumps." Council is enabled per
context type in `gate.toml` today; moltar dropped the director's council on
2026-10-08 because Amy watches the director herself. Open: whether a named
council profile belongs on `kj preset` (which already assigns a cast at
fork) or as a named section in `gate.toml`, and what distinguishes the mk,
lfm2d, and Jev! flavors (judge model, rubric, or both).

Proposed 2026-10-09: the flavors are casts whose `shadow` slot names the
judge (`docs/council.md`, "Shadow voice"). Whether the voting council's
judge also moves onto the cast is open; today `[council] server` in
`gate.toml` names one server for every decision.

## Dead test helpers in `mcp/servers/file.rs` (2026-10-08)

`cargo check -p kaijutsu-kernel --tests` warns that `broker_with_vfs_file`
and `broker_with_vfs_files` (`kaijutsu-kernel/src/mcp/servers/file.rs`) are
unused. Seen while retiring `shellDryRun`; not caused by it.

## A card that arms mid-line takes a typed letter as an answer (2026-10-08)

The ledger push brings an ask to the TUI sooner than the old poll did, so a
card more often arms while a person is still typing a draft line. Once armed,
a typed `d`, `a`, `A` or `v` answers or moves the ask (`docs/tui.md`, "Asks",
and `kaijutsu_client::AskArming`). Seen in `terminal_fit`'s desktop-notify
test, where `send` denied the ask. Open: whether arming should also wait for
the line to be sent or cleared, not only for a pause in typing.

## Input latency follow-ups from the Nagle and draft outbox work (2026-10-09)

- **Faster transport exposed two test races, and one kernel window.** With
  Nagle off, `reconnect_fsm` and `rc_lifecycle_wire` probes ran into
  windows the 40 ms delay had hidden. Their probes now wait correctly. The
  streaming `execute` answers when a command starts and holds the
  connection's one execution slot until output is delivered; no shipped
  client calls it twice in a row, but a caller has no completion signal
  to wait on besides the output subscription. Separately, a new context's
  row is visible (`list_active_contexts`, `kj context list`) before its
  document exists, so a reader that lists and then reads blocks can get
  `DocumentNotFound`. Open: create the document before the row is
  visible, or make readers treat a listed context with no document as
  empty.

- **The app's input lag is not measured.** The app echoes locally and does
  not wait on `edit_input`. A likely contributing factor is the reactive
  `UpdateMode` (100 ms when focused, `kaijutsu-app/src/main.rs`): if a key
  needs a second frame to reach the screen, that frame waits for the
  timeout. Count frames from key to glyph before changing it. Amy,
  2026-10-09: the app waits behind larger work.
- **Per-keystroke kernel cost.** On the ephemeral test kernel an
  `edit_input` takes about 2 ms end to end. Zorak's disk is not measured.
  The outbox keeps that cost off the key path, but a long burst still
  drains at that rate.

## acp-fleet contained scenarios not run since the typed ledger (2026-10-08)

`fleet/contained` needs podman and the fleet image; it was not run when
the host scenarios were brought up to date on 2026-10-10.

## Council bump-only has no way to hand an action to Amy (2026-10-06)

In bump-only mode every decision that does not pass bumps, so a command the
rules say "needs Amy" can only be bumped again. On 2026-10-06 banto stopped
after four bumps and reported, which was the right move, but the system
offered it no ask. Whether bump-only should open an ask for a `report`
verdict, or after N bumps of one goal, is open.

Amy, 2026-10-09: escalate after N consecutive bumps in N minutes, but
observe first. Many bumps are often fine: they force a seat to break work
into smaller, safer steps, at some cost.

Built 2026-10-10: the `undo` read on the shell bump spec, `kj ledger bumps`,
and `[council] escalate = { bumps, minutes }`, off unless set
(`docs/council.md`, "Bump-only"). Open: pick N and M from what
`kj ledger bumps` shows on real seats, then turn it on.

Found by a kaibo DeepSeek review, 2026-10-10: `refuse_bump` reads the bump
count and the streak under one lock and writes the bump or the ask later,
so two submissions one seat sends at once can both bump past `bump_limit`
or both escalate. The same window exists in `kj context rotate` and
`kj context demote`, which run the archive lifecycle between reading the
row and committing. No per-context lock spans a decision today; a seat
runs one tool call at a time, so it has not been seen.

## Council mass floor misses a decision every read agreed on (2026-10-06)

`kj db backup … && kj context archive partb-1 --confirm` missed on moltar:
every read answered `originals=changes_backed_up`, and one read's verdict
mass was −2.58 against the director's floor of −2.5. The floor exists to
catch reads that answer off the menu; whether it should apply to the
verdict when the rubric decides, or sit lower for the director, is open.

## What the first live council decisions on zorak showed (2026-10-04)

A coder seat on tenchi's qwen3.8-27b (`council-smoke`) ran `ls -la`,
`git status`, and `chmod -R 777 ~/src/nonexistent-council-smoke` twice,
against `council-system` and `council-amy` on the megakernel.

- **A deploy does not install the council specs.** `rc reseed` covers
  `/config/rc` only, so `/config/kernel/council/shell-gate.json` was missing
  and the first decision missed ("cannot be read"). They were copied by hand
  from `assets/defaults/council/`. Seed them, or name them in
  `docs/operating.md`'s deploy steps.
- **The first decision after a context change misses.** Sync is lazy: the
  first decision PUTs both contexts (1108 and 396 tokens) and builds each
  spec layer inside the deadline. With a 3000 ms deadline it missed during
  prepare, then during the decision. The megakernel declares no `warm`, so
  the kernel should send contexts and build spec layers off the hot path, at
  enable time and after each change.
- **Warm decisions take 1.4-3.3 s** on zorak's shared GPU (direct timing,
  two contexts). The deadline is now 8000 ms on zorak.
- **The mass floor discards a useful read.** On the chmod, council-system
  said ask and council-amy said report with mass -0.62; the -0.3 floor made
  it a miss instead of an ask carrying that split. Lower the floor, or drop
  a low-mass read from the pool instead of missing the whole decision.
- **It works.** Second pass: `ls -la` and `git status` were council-allowed
  (policy verdict Allow, no ask), and the chmod asked. Direct reads: force
  push to main 0.987 report, `sqlite3` on kernel.db 0.971 report, `cargo
  test` 0.78 allow (asks at 0.9).

## `kj context list --tree` draws siblings as children (2026-10-04)

On zorak, `kj context list --tree` drew `tui-ask-stuck`, the banto contexts,
and others one level under `council-setup-cf`, a context created that minute
whose `kj context info` reports 0 children and `forked_from` = `amy`. Amy
could not find her root context (`amy`, f8f4010b, 125 children) in the tui
either; the root is not labeled as one, and about 150 contexts sit at the
top level beside it.

## System 1 musicians share the megakernel with the gate (2026-10-06)

The bass prototype (`contrib/chameleon-s1/`) chose one bass cell per bar
from the megakernel in about 250 ms (median, client wall time, one choice
question), while moltar's gate decisions ran on the same server. The
response has no `queue_ms`, although `docs/council-api.md` lists it, so a
1475 ms outlier has no recorded cause. Before a second musician joins, the
band's decisions and the gate need a priority or admission story, and a
producer measuring council latency must use its own wall time.

## Model tunables: follow-ups from the kaibo review (2026-10-06)

- **Two writers store an unresolved model string.** `kj cast slot set`
  stores `--model` verbatim, and RPC `configure_llm`
  (`kaijutsu-server/src/rpc.rs`) persists the raw provider and model. An
  alias or a `provider/model` form there becomes the wire model and the
  tunables key, so the model row is skipped and the floor applies. `kj
  context set` and `kj fork` resolve through `resolve_model_choice`; these
  two should too.
- **`kj model` can show a knob the wire drops.** Claude drops temperature
  and top_p whenever thinking is on, and `effort` rides only the adaptive
  tier; codex-app maps only model and effort. `kj model` prints the
  resolved values regardless.
- **`unknown_kind_in_the_table_is_fatal` cannot fail for its name**
  (`llm/db_config.rs`): it never puts an unparseable kind in the table, so
  `load_backends` skipping one would stay green.

## acp_fleet's forget-a-human-rule scenario predates the worker loadout (2026-10-06)

`cargo test -p kaijutsu-solo-acp --features test-mock --test acp_fleet`
fails `f-model-cannot-forget-human-rule`: the scenario expects a coder's
`kj ledger forget` to reach "ask the reviewer who made it", but since
13fff761 a coder has no `kj ledger` at all ("not part of this seat's
work"). The property holds more strongly; the scenario should expect the
house refusal, or move to a seat that holds the house capability. Which
it should test is a choice to make, not a typo to fix.

## Two tui terminal_fit tests fail on moltar (2026-10-06)

`ctrl_a_r_rotates_the_seat_and_follows_its_successor` (the client never
shows `rotated` within 20 s) and
`the_prefix_leaves_the_editor_parked_and_the_way_back_restores_it` fail
in `crates/kaijutsu-tui/tests/terminal_fit.rs` at 606a98d0, b7055fcb, and
d650b685 alike, so no change made that day caused them. The rotate dump
shows the coder rc's orient step listing the real `/home/atobey` and its
dotfiles, so host state reaches the test kernel; that is a likely
contributing factor, not a confirmed cause. The other 43 tests pass.

## Running a kaish script file from a context shell (2026-10-06)

Found while checking `contrib/council/seed-director.kai` on moltar through
`kjc -c verify sh`. Running a `.kai` file by its path does nothing and
exits 0. `source` runs it but leaves `$0` empty, so a script that finds its
data through `$(dirname "$0")` resolves from the working directory. And
`kjc sh` drops stderr, so a script's own error message is invisible unless
it is redirected with `2>&1`. Together these made the bench council
variant's first run on moltar write nothing and report nothing. kaish is a
shared interface; the path and `$0` behavior go to that lane first.

## Creating a context with an unseeded type falls back to default (2026-10-06)

`council-amy` and `council-banto` on moltar were created with
`--type council` before the host rc had a `council` type, and
`kj context info` reports them as `Type: default`, with the default stance
and loadout. A type with no rc should refuse the create, or say loudly that
it fell back. On moltar we repaired them after the reseed with
`kj context set --type council` and `kj context rotate`, which reran the
council create rc; the guidance was loaded again from
`contrib/council/director/`. Also, `kj block inspect` does not show whether a block is
excluded, so there is no CLI way to confirm what a council context sends.

## The shell spec is not named in a council `warm` (2026-10-06)

`CouncilSync::hold` hashes a context's body before adding `warm` and skips
the `PUT` when the hash matches (`crates/kaijutsu-kernel/src/council/sync.rs`).
A submission's program decisions run first and send each shared context
with `warm` naming the program spec, so the shell decision finds the head
held and never names its spec. Per `docs/council-api.md`, the first shell
read after a context change then rebuilds its spec layer inside that
decision's deadline. No test covers `warm`. Found in the kaibo review of
49b6f839.

## `CancelOnDrop` also fires after a delivered score (2026-10-06)

`ModelOutput::resolve` moves `CancelOnDrop` into its future
(`crates/kaijutsu-kernel/src/hyoushigi/model.rs`), so the turn's interrupt
fires when the future completes normally, not only when it is dropped. The
lease is terminal by then, so nothing breaks today, but a change that keeps
the interrupt state live would hard-cancel a turn on its own successful
delivery. Disarm the guard on delivery. Found in the kaibo review of the
timing work.

## Reading a seat writes into its log (2026-10-06)

`kjc -c <context> kj ...` authors its call and result as blocks in the
context named, and `kj context show` takes no argument, so reading banto's
metadata from outside put a user tool call and its result into banto's next
turn. Amy: "expected, but will need some UX work for both of us." A read
from outside a seat should name its target without becoming the seat's
history, for people and for agents. Meanwhile, read from `verify` with `-c`
flags (`kj block list -c banto`).

## Council setup on an existing host takes hand steps (2026-10-06)

Turning the council on for moltar took a copy of the missing specs into
`/config/kernel/council/` (docs/operating.md says to) and the bench's
`council` rc variant run by hand. Its scripts find their Markdown through
`$0`, which `source` does not set, so `council-system` came out empty on
the first try and the script still reported nothing wrong. The variant
lives under `coder/create`, but the contexts it creates serve every type
whose council is enabled. A seed step for council specs and contexts would
replace both.

## `blocks repair-order` does not converge on one conversation (2026-10-04)

On zorak's deploy, the first `--apply` fixed 1099 conversations' worth of
pre-855ace8a misordering (248 results, 2965 blocks re-keyed), but
conversation `01a0d469-3af0-7701-88a9-9f76ea803728` keeps 123 results
before their calls in one 661-block run. Each `--apply` re-keys the same
run again and reports the same 123, so the run's ticks themselves put
results first (tied or wrong ticks), and tick-order re-keying cannot fix
it. It was applied twice; don't keep applying. Look at that run's ticks
before deciding: a per-pair repair (move each result after its call), or
archive the conversation as-is.

## "Context" means two things once the council lands (2026-10-04)

`docs/council-api.md` uses context for a held model context, as the
megakernel and lfm2d do; our Terms table (`docs/writing.md`) uses it for
kaijutsu's durable block log. Amy: "if a context is a context call it a
context here. kaijutsu shoulda called it something else in any case." A
council context is a kaijutsu context held on the server under the same id,
so the two mostly coincide. Renaming kaijutsu's term is open; until then,
say "held context" when the server's copy is meant.

## Architecture cleanup plan

Source review at `f7e46f8e`, September 16:
[evidence and proposed tests](audits/2026-09-16-architecture-debt.md).
This is the live plan; the audit records the review. Planning added source
markers and corrected misleading comments, without changing behavior.

The bounded deletions are recorded in `docs/devlog.md`, "Retiring duplicated
state". The remaining work below needs separate reviewable changes. Start
behavior changes with red/green regressions; remove each issue and its source
marker in the change that satisfies it.

### Idle conversation reset policy

Idle LRU eviction still resets semantic conversation history, so cache
pressure can make stored edits visible on the next turn. Active and waiting
turns retain one lock across reset; concurrent first lookups share it.
Decide when idle conversations may reset before changing the LRU policy;
preserve context/conversation separation. See `docs/conversation-session.md`.

### Turn execution and shell settlement

`docs/kaish-integration.md` owns the caller inventory and its Migrated,
Partial and Pending rows. `docs/resource-admission.md` owns the worker pool
slices. The story of what landed is in `docs/devlog.md`, "Retiring duplicated
state", and in git. Review evidence:
`~/exomemory/kaijutsu/reviews/2026-09-17-execution/`. What stays open:

Provenance and storage
- **No durable per-edit audit record.** Writes carry their current performer
  as live state only: `TextEdit`/`SyncPayload` and persisted snapshots do not
  retain an edit actor.
- **Boot skips a corrupt document with only a log line.** `load_one_from_db`
  refuses with `CorruptSnapshot` or `CorruptOplog`; the bulk path
  `load_from_db` logs and skips. Return the skipped contexts and their errors
  so startup can report them.
- **A failed document acceptance poisons that context until restart.**
  Structured inspection from that context also refuses; inspect the operation
  from a healthy context.
- **Pair creation and receipt registration are separate writes.** A failure
  between them leaves blocks without a receipt.

Block tools

File cache
- Generation metadata errors are swallowed, and comparison detects only an
  increasing generation.
- A dirty symlink buffer does not detect a changed target in the guarded-write
  check. Preserve dirty work while fixing it.
- Stale-read error branches remove cache entries; check that they preserve
  editor pins.

Settlement and lifetimes
- **Non-shell MCP calls have no retained result-review owner.** A result-phase
  Ask or escalation returns `GateUnavailable` before minting an ask.
- **Abrupt worker-task destruction before capture has no live terminal
  settlement.** Startup reports the interruption without replaying source.
  The job/controller lifetime work is the Pending row in
  `docs/kaish-integration.md`.
- Host `Drop` is a cancellation signal without a wait; SIGTERM and SIGINT do
  wait for the runtime pool.
- The live retention copy of a terminal result is lost if the process dies
  before SQLite accepts it. Retries are four per scan, so a long storage fault
  accumulates retained results: `docs/resource-admission.md`, slice 5.
- A session refusal settles before a separate redemption, so a retry can
  re-emit the same pair's metadata and status updates. Startup suppresses old
  denied pairs rather than settling them.
- The four-item delivery cap counts deliveries, not provider requests. Model
  spend needs its own admission count. A changed-performer completion keeps a
  suppressed disposition; conversations already running need their own audit.
- `jobs --json` does not expose the nested external process group under the
  outer kaish job, so the receipt does not identify it. Parent-death cleanup
  covers direct children; descendant trees after SIGKILL have no evidence.

`kj wait` and turn outcomes
- Turn events carry a `TurnId`, but clients clear context activity on any
  terminal event.
- A context wait has no durable per-turn outcome: a failure before any model
  block plus a missed terminal event can time out as running. Stored event
  detail is the latest observed outcome only.
- The global ledger wake re-reads an ask on unrelated changes:
  `docs/resource-admission.md`, slice 6.

### Shared client recovery

Give `kaijutsu-client` ownership of subscription, mirror, snapshot recovery,
and rejection of obsolete responses. Start with tests for delta/snapshot
ordering, overlapping reconnects, and release during recovery. Migrate app,
TUI, and ACP one at a time, removing their duplicate lifecycle code as they
adopt the shared owner. Clients retain retry/release presentation choices.

### Render the live mirror

Separate collapse/selection state from live `BlockSnapshot` copies, then let
the app render its mirror without rebuilding `RenderBlockStore` per version.
Preserve welcome/offline sources and geometry/glyph caches. Reuse collapse
regressions and check streaming plus context switching through the GUI runner
and BRP. Keep this separate from the shared recovery migration.

### Per-cast turn token budget

The agentic loop's per-turn iteration cap (50 collaborative / 100 autonomous)
is removed; a turn now runs until `EndTurn`, cancellation, or the
output-ceiling continuation budget, with no count of tool rounds. Amy: "let's
remove that turn cap if it's not useful… no cap for now, we'll come back to
this, and it'll need to be per cast/model since it varies by model and model
configuration." The replacement is a per-cast/model budget of cumulative
input+output tokens across a turn's model calls; at roughly 90% of budget the
model gets one final tool-free call and must report, rather than being cut off
mid-tool-chain. Context windows for Alibaba (qwen) models are unpinned, so a
fraction of the context window is not a usable proxy for the budget — it needs
its own configured number. Not built yet. `kj interrupt <target>
[--immediate]` is the manual backstop until the budget lands: it stops the
running turn and refuses automatic resume until the next explicit turn.
Live evidence: "What the cap-free banto review showed (2026-09-24)" below.

### Lazy file documents

Audit readers, editors, and recovery callers before choosing a buffer
representation. Ordinary reads should not require durable documents; persist
unsaved editor content with its recovery metadata. Preserve external-change
checks, pinning, and swap acknowledgment. Start with restart/recovery and
external-edit regressions from `docs/file-buffers.md`; only then remove clean
read materialization. This remains a separate design change.

**Order:** kaish/rc migration (construction, rc, settlement, then turn
ownership), shared recovery, rendering, then file-buffer persistence. Each
requires its own reviewable change; the source TODOs point to these entries.

### Kernel architecture overview needs a refresh

Finish the symbol and schema inventory review alongside the runtime migration.
Keep the architecture overview's current implementation separate from the
planned destination in `docs/kaish-integration.md`; update server ownership,
lifecycle callers, and diagrams when the code moves.

## What banto driving one cleanup to a coder showed (2026-09-22)

Banto (director, qwen3.8-flash) created a coder lane and drove the
TimelineVisibility cleanup. The coder's edits were right; the path around them
stalled. Ask rows: `kj ledger list --history --origin shell_gate --since`
covering 2026-09-22. Open, most costly first:

- **An ask to an idle model reviewer notifies nobody.** The coder's asks
  went to banto; nothing woke it, and the coder spun to the 50-iteration cap.
  A reviewer blocked in `kj wait <lane>` now returns on the lane's ask for it;
  a reviewer whose turn has ended still hears nothing. Design direction to
  discuss: a model reviewer passes the ask up the walk with its opinion
  attached rather than holding it.
- **One inert statement escalates a whole program.** Banto ran
  `kj ledger allow <id>; echo "allow_exit=$?"`. The ledger answer has a
  builtin allow; `echo` has no builtin key, so the program escalated and the
  answer became an ask to banto's own reviewer. Banto was already the coder's
  assigned reviewer; nothing else stood in the way. The director stance now says to run the answer
  as a command of its own. Whether `echo`/`printf` without a redirect earn a
  builtin key is open.
- **Offline `kj` takes 44 s to boot.** Inferred, unprofiled:
  `BlockStore::load_from_db` decodes every document snapshot serially under
  the `KernelDb` mutex. Profile before changing it.
- **Approval resume and completion notices write before reserving.**
  `runtime/approval_resume.rs` and `runtime/completion_notice.rs` write blocks
  ahead of a runtime reservation, against rule 1 of
  `docs/resource-admission.md`.
- **Approval delivery holds a top-level reservation for the process
  lifetime.** Harmless at `ADMISSION_CAPACITY` 64; slice 4's occupant
  exemption removes it.
- **A shutdown between a caller's write and its spawn strands the write**
  (kaibo, 2026-09-23). `reserve` checks the shutdown token once; the
  supervisor closes the queue on shutdown, so `RuntimeSlot::spawn` fails after
  `prompt::submit` inserted its block or consumed the draft, or after
  `kj drive` wrote its seed. `ToolCommand::execute` settles its receipt on
  that failure; these two do not. Shutdown-only, so no live harm yet.
- **`RuntimeSlot::Local` is not pinned to its thread.** Spent on another
  thread, it would send work to the originating thread's channel and skip
  admission. No caller does that today; a `!Send` marker or a thread check in
  `spawn` would make it impossible.
- **kaish bare `echo` prints nothing.** POSIX prints a newline. Upstream kaish
  issue; Amy decides whether and how it is posted.

## What the banto rerun showed (2026-09-24)

Banto (director, qwen3.8-flash, house cast) drove `fix-handoff-tail` to a
correct, test-first fix: red observed, green verified by banto itself, no
commits. Banto 50k tokens, coder 69k (09-22: banto alone 142k and the coder
hit the 50-iteration cap). The coder's two `cargo test` asks went to banto,
which answered them from inside `kj wait`; two banto asks reached the lead.
Open, most costly first:

- **A `</think>` tag leaks into qwen model text** (coder block #25). The
  provider does not strip it on every path.
- **Banto's `shell` tool times out at 120 s inside `kj wait`.** Its wait asked
  for longer than the tool allows; the stance or the tool should say the cap.
- **Banto quoted an operation id it never received** (`…afa5-f7a0-afa5`).
  The refusal named the problem; noted as model behavior, no fix proposed.

## What the read-only git probe showed (2026-09-24)

After deploying 8e210275, banto (qwen3.8-flash) reviewed a57ad069 from its
read-only shell. `git info`, `git diff --from --to`, and `git show rev:path`
worked with no asks; three file reads sent `offset`/`limit` as strings and
all succeeded. Open:

- **A review turn hit MaxIterations (50) without a report.** Banto chased
  side questions (the `/` mount, the `/dev` pair below) and its last text was
  "Let me check…". The turn ends with no answer for the requester. Design
  question: a final tool-free turn at the cap that must report, or a stance
  line to answer before exploring further.
- **`kaish-mounts` lists `/dev` twice.** kaish's overlay appends its virtual
  mounts to `MountBackend::mounts()`, so the host `/dev` mount
  (`kaijutsu-server/src/rpc.rs:1528`) and kaish's virtual `/dev` both show.
  Which one serves a `/dev` path under the overlay is unverified.
- **Builtin git differs from host git.** No `log --oneline`, no `diff --stat`,
  and `diff <path>` needs `--`. The refusals say so and banto adapted; decide
  whether the director stance should name the builtin's forms.

## What the cap-free banto review showed (2026-09-24)

Banto (`banto-0924c`, director, house cast, qwen3.8-flash) was asked to
review 56b1ab3a from its read-only shell and report on two questions with
file:line citations. After 47 minutes and about 80 tool calls it had written
no `model/text` block and no report; it had moved from `mcp/schema.rs` into
the broker and turn hydration. Nothing in kaish could stop it, so the lead
restarted the kernel. No optional int arrived as a string. Open:

- **A turn runs without a bound and without a report.** This is the first
  live evidence for "Per-cast turn token budget" above; the 09-24 read-only
  probe's cap-hit ended the same way, just sooner.
- **An async shell call's stored result was empty** (104 of 227 results
  in `banto-0924d`). Fixed 2026-09-26: the result stores the envelope the
  model received as `model_content`, and completion notices join a running
  turn (`docs/conversation-session.md`, "Tool results replay as sent" and
  "Input during a turn"). Contexts written before the fix still replay
  empty results.
- **Each tool round took about 40 s.** The model appears to wait for the
  completion notification before its next call. Unmeasured: how much is
  provider latency versus the async settle.
- **Builtin `git diff --from A --to B -- <path>` refuses the path** with
  "takes no bare operands", although its own help shows that form. Something
  between kaish argument parsing and the tool drops the `--`. Unlocated.

Rerun after c76ff70a (`banto-0924d`, same prompt and cast): in 15 minutes
banto made about 180 tool calls and about 45 short narration steps, answered
part of question (1) ("all 12 builtin servers call `tool_input_schema`"),
and said "Nearly there" at minute 10, but wrote no report. `kj interrupt`
(soft) ended it as `Cancelled { immediate: false }` about 23 s later; two
shell completions that landed afterward did not resume it. Open from this
run:

- **Banto invented three things and caught each one itself**: an operation
  id, `/tmp` spill paths, and a spill path it assumed from an error. Same
  family as the 09-24 "quoted an operation id it never received".
- **Tool pace differed by an order of magnitude between the runs** (about
  40 s per call in the first, about 3 s in this one). Unexplained; parallel
  calls or provider latency are candidates.
- **A soft interrupt gives the model no chance to report.** For a review the
  work so far is lost unless someone reads the blocks. The per-cast budget's
  "one final tool-free call" is the designed answer.

## Banto with foreground shell calls (2026-09-24)

`banto-0924e` (house cast, qwen3.8-flash), same review prompt as 0924c/d,
after 08b7a741: 82 tool calls in 15 minutes, no polls, one empty result, no
report; interrupted. 49 thinking blocks, one of 15,412 characters. Of 21
errors, 17 are builtin-tool friction:

- **kaish `grep` rejects GNU BRE alternation.** 10 pre-validation refusals
  ("invalid regex pattern … unclosed group") for `"a\\|b("`-style patterns;
  the hint says to use `[(]` or `-E`, and banto kept writing GNU grep.
- **Builtin `git diff … -- <path>` drops the `--`** ("takes no bare
  operands"), three times; the refusal suggests the very form that fails.
- **Builtin git gaps:** `--stat` (2), `-C` (1), and no repository found from
  the director's home cwd (2).
- **The `system/error` block beside a rejected call shows an empty
  envelope**, while the real reason is in the `tool_result` (and reaches the
  model through `stdout`). Misleads a reader of the log.

## A `session.end` hook archived a live session's context (2026-09-24)

At 17:40:04 this Claude Code session's hook pipeline delivered `SessionEnd`
with reason `prompt_input_exit` while the process (pid 375691, running since
13:12) kept going; no `SessionStart` followed. kaijutsu-mcp's hook listener
archived the session's context `181a0b9e` on that event alone
(`kaijutsu-mcp/src/hook_listener.rs`, the `session.end` arm). Amy did not
press Ctrl+C; her tui's connection closed cleanly 14 s later, so a key
aimed elsewhere is a candidate. The trigger is still unconfirmed.

Fixed 2026-09-24: `session.end` now only records the end
(`HookListener::should_record_session_end` /`session_end_recorded`); the
archive itself runs from `main.rs`'s stdio-shutdown path
(`HookListener::archive_if_session_ended`), and any later hook event clears
the recording. `register_session_impl`'s already-joined fast path now
re-checks the bound context's liveness (`resolve_context_label`) before
trusting it, and rebinds to a fresh context — carrying `previous_context` —
when the bound one turns out to be archived, concluded, or gone. Two things
this left open:

- **`shell`'s archived-context refusal is not structurally recognizable.**
  `admission.rs::ContextAdmission::acquire` returns a plain `String`
  ("context … is archived; restore it with `kj context promote …`"), which
  reaches the client as `CallError::Rpc(String)` — the same catch-all every
  other kernel-side RPC failure uses. `shell_impl` cannot point the caller at
  `register_session` without matching that exact prose. Giving this refusal
  a structured kind (like `Refusal` or `VfsErrorKind`) is a design
  conversation, not a follow-up patch.
- **`register_session`'s tool description overstates the archived-label
  case.** It promises a suffixed fresh label plus `previous_context` for
  "concluded or archived", but `idx_contexts_label` covers live rows only
  (`kernel_db.rs`) — an archived context's label is free for reuse by
  design, so `resolve_context_label` returns `None` for it and a **fresh**
  `register_session` call (not already bound to that context) creates a
  context under the exact same label with no `previous_context`. Only
  "concluded" actually gets the suffixed-label treatment. The tool
  description needs correcting, or the archived case needs the same
  registry-fallback treatment the already-joined fast path now gets.

## Client-declared MCP servers: what stays open (2026-10-01)

ACP `mcpServers` now start on the kernel host and reach only the session's
context (`mcp::context_servers`, docs/acp.md, "Client-declared MCP
servers"). Open, most likely to bite first:

- **10 s is short for an `npx` server.** A declared server must finish spawn,
  handshake, and `tools/list` within the kernel's `mcp_connect_timeout`
  (10 s). `npx -y <package>` on a cold cache downloads first, so an MCPMark
  task can fail `session/new` for want of time. ACP carries no timeout; the
  choices are a longer kernel default for declared servers or a `_meta` key.
- **`http` is unproven from ACP.** The broker speaks streamable HTTP for
  `mcp.toml`, and Harbor sends `streamable-http` servers as ACP `http`, but
  no test drives it, so `mcpCapabilities.http` stays false and such a
  session fails. `sse` has no broker transport at all.
- **A reconnect mid-turn loses the tools until the next prompt.** The kernel
  stops a connection's servers when it closes; the bridge redeclares only
  at `session/prompt`.
- **A kernel crash leaves stale grants.** The `context.<hex>.<name>` grant
  is a persisted binding entry; after a crash nothing revokes it. It grants
  nothing (no instance has that id until the session redeclares) but shows
  in `kj binding show`. A boot sweep of `context.*` grants would remove it.
- **Two `session/new` calls in one cwd within one second share a
  context.** The label is `acp-<cwd leaf>-<unix seconds>`, and
  `open_or_create` attaches to a live context holding the label, so the
  second "new" session is the first one, MCP servers included.
- **Does `kj mcp reload` start servers on a connection's runtime?** A
  server's rmcp tasks live on the runtime that connected it; context
  servers got their own runtime after a test showed them dying with the
  declaring connection. `reconcile_external_mcp_servers` from `kj mcp
  reload` or `restart` may have the same exposure, depending on where `kj`
  runs. Unverified.
- **No capability gates `declareContextMcpServers`.** Any connection can
  spawn a host command for any context and add one explicit grant to its
  loadout. Amy, 2026-10-01: "no exec gate for now ... acp would have to
  auth over ssh in the first place". Revisit with exec on declaration.
- **The fleet has no `mcpServers` key.** The end-to-end tests live in
  `crates/kaijutsu-solo-acp/tests/acp_mcp_servers.rs`, so the Harbor-shape
  invariants are not checked against an MCP tool call yet.

## From the coder orientation preload (2026-10-01)

- rc shells have no read-only kaish-git (`runtime/git_tool.rs` registers it
  only in `ShellPolicy::ReadOnly`), so `coder/create/S35-orient.kai` reads
  `.git` files directly and has no dirty-tree summary or commit log; the
  reflog stands in. Registering kaish-git under rc would shadow host `git`
  for rc scripts; decide before doing it.

## kaijutsu-mcp session identity (2026-09-25)

From the kaibo review of 2d274c2e (routing by host pid, host-supplied ids):

- **Codex premises are unverified.** The listener compares the MCP's
  `CODEX_THREAD_ID` with each hook payload's `session_id` and refuses a
  mismatch; the hook client assumes Codex spawns hooks directly, so its
  parent pid names the Codex process. Only our own fixtures back either.
  Probe a live Codex session: its hook payload's `session_id`, and the hook
  process's parent. If either fails, every Codex event is refused or dropped.
- **A misrouted `session.start` would still rename a listener.** Adoption
  runs before the foreign-session check, and `session.start` always renames
  (that is how `/clear` works). Reachable only after a routing error: a
  reused pid, a stale `CLAUDE_PID`, or an explicit `--socket`.
- **After `/clear` or `/resume` the session id moves and the context does
  not.** Blocks keep landing in the old context; after `/mcp` the new process
  registers `{base}-{new sid8}` and starts a fresh context. Decide whether a
  context follows the Claude Code session or the process.

## kaijutsu-mcp startup against a sick kernel (2026-09-25)

- **Kernel boot holds SSH connections for about 33 s.** On the 15:52 restart
  the listener started at 15:52:27, `Loaded 3377 documents from database`
  logged at 15:52:59 (`kaijutsu-server/src/rpc.rs`), and the shared kernel
  came up at 15:53:00. Clients in that window see `ssh dial exceeded 5s`
  rather than a refusal. `run()` binds the listener (`ssh.rs`) but accepts
  only after `create_shared_kernel` returns, so connects sit in the backlog
  with no banner. On zorak's 1.3 GB `kernel.db`, 30.3 s of it is
  `BlockStore::load_from_db` (`block_store.rs`): it decodes every snapshot
  and replays every oplog row for every document, archived or not.
  Archived conversations hold 756 MB of the 936 MB of snapshots (888 of
  1039 contexts); the largest single snapshot is 33 MB. Moltar loads 14,721
  smaller documents in about 4 s, so cost follows bytes decoded rather than
  document count (inferred; not profiled). Candidates: load archived
  contexts on demand through `load_one_from_db` (audit every `get()` caller
  first); accept SSH early and answer "kernel starting" until it is ready;
  trim oversized snapshots. Amy (2026-09-27): "that's fine for now".
- **A shell built before the semantic index attaches never sees it.** The
  index now connects after boot (`connect_semantic_index`, `rpc.rs`).
  `KjBuiltin` copies the dispatcher's index when a shell is built, so a
  long-lived shell built during that window reports search as unavailable
  until it is rebuilt. Model shells are built per call.
- **Label stabilization does not take `RemoteState::registering`.** A
  `register_session` on a join path can interleave with a hook event's
  reattach (`stabilize_context_label`), leaving the connection's context and
  `remote.joined` on different contexts (kaibo, deepseek). Taking the lock
  in the hook path runs into the hook budget, which drops the future after
  `Mutex::take` has consumed the one-shot label base. Fix both together.
  Across processes, two MCPs for one session can still race resolve→create
  on the no-row path; the loser gets a label-conflict verdict.
- **The deferred-registration fault cap has no end-to-end test.** The
  ephemeral kernel offers no deterministic untyped error on a live
  connection. `register_when_connected` stops after `DEFERRED_FAULT_DELAYS`
  by reading, not by a test that went red.
- **`run_local` in the kaijutsu-mcp e2e tests drops its `LocalSet` outside
  the runtime.** When a test fails, live SSH channels drop there too, and
  russh's `ChannelCloseOnDrop` adds a second panic: `there is no reactor
  running`. The first panic still names the failure.
  `mcp_startup.rs`'s late-kernel test drops its `LocalSet` inside
  `rt.enter()`.
- **Claude Code 2.1.282 rejects our `resources/list` result**: `ttlMs` must
  be a number and `cacheScope` must be `public` or `private` (rmcp 3.4.0,
  protocol 2026-07-28). Logged at every MCP start in every session.

## A client does not say which principal it connected as (2026-09-24)

Amy's tui tried to allow an ask and got "awaits its assigned reviewer": with
no key selected it tried the agent's keys in order, `kaijutsu-lead` came
first, and the tui authenticated as the ask's own performer. The tui and app
now take `--key-fingerprint`/`--key-file` like the mcp and acp bridges, but
nothing on screen names the principal, so the mistake only shows at the first
refusal. Show the connected principal's name where the tui and app show the
context.

## From the kaibo review of the scripted mock and the session scenario (2026-09-15)

Read by the lead; each line re-checked before it went here.

- **A registry rebuild rewinds every mock queue.** `Provider::from_backend`
  re-reads `KJ_MOCK_SCRIPT_DIR` on every `build_llm_registry`, and `kj cast`,
  `kj backend`, and `kj alias` writes rebuild it, so a mid-scenario write
  replays consumed turns instead of panicking. `session_scenario.rs` survives
  by ordering; say so in the file, or hold the queues outside the provider.

## Accountability at the handoff log (Amy, 2026-09-15)

Reviewer resolution walks `forked_from`, so every path that mints a context
decides who reviews. Fork and `kj context create` from inside a context are
covered; nothing re-parents a context. This is not:

1. **Handoff logs are parentless.** `kj handoff note` mints the
   character's log with `forked_from: None` (`kj/handoff.rs`), a
   parentless context played by a model. The kernel must not guess a root
   (Amy, 2026-09-16), so it needs an explicit parent. Since the default
   reviewer went away, an ask raised there refuses (nobody is above its
   model performer) and it has no lineage root. Amy decides its parent.

Open question from the lane: the walk starts at the ask's own context,
so an actor who is not that context's performer, a human typing in a
lane coder plays, resolves to coder. Starting at the parent would give
banto. Both are models. Neither the docs nor a test pins it; Amy decides
whether a human's command in a model's lane is reviewed by that model,
by its parent, or walks to the nearest root.

## Roots, bootstrap, and rotation: what stays open (2026-09-17)

The seven-slice bootstrap redesign shipped 2026-09-16 and 2026-09-17
(`docs/devlog.md`, "The kernel with no one to answer to"). Open:

- Deploy: each host needs its kernel wiped and `kaijutsu-server init` run,
  every client binary rebuilt (wire version 2), rc reseeded, and the stale
  `/config/kernel/approval.toml` deleted by hand. Migrate zorak by hand or
  in downtime; keep it simple.
- A model rotating its own seat from inside a live turn archives the
  context that turn is still writing to. Untested.
- `Ctrl+A r` (rotate by prefilled prompt, client follows the successor) is
  TUI only. The app has the prefilled-`kj` pattern but no rotate chord and no
  follow on a rotate result (`docs/input.md`, "The prefix table").
- A pinned structured `kj context switch` still runs the builtin's
  `switch_context`: it validates and saves the addressed context's cwd in a
  throwaway shell, so a context whose cwd has gone missing refuses a switch
  that would never have moved that shell
  (`runtime/kj_builtin.rs`, `KjResult::Switch`). The client follows
  `switched_to` from the result data instead.
- An upgrade can land a kernel on a `gate.toml` the new binary rejects, and
  then every gated shell submission is refused. On moltar 2026-09-21 the host
  file was the older shipped default, naming `[context_type.explorer]` after
  the type became `toolie`. `rc reseed` does not look at `/config/kernel`, and
  nothing at boot says the gate is unusable; the refusal shows only per call
  in the log. Reseed also never removes a retired bucket: the host still
  has `/config/rc/bassist`.
- Amy's 2026-09-21 report that `Ctrl+A` failed from the inline draft after
  `Esc` does not reproduce: `the_prefix_switches_seats_from_normal_mode_mid_draft`
  passes, and every path from the draft reaches `Keys::interpret`. A pty
  cannot stand in for her terminal and mux. `RUST_LOG=kaijutsu_tui::keys=trace`
  records each key and the intent it became in
  `~/.local/state/kaijutsu-tui/tui.log`; run with it and read the log at the
  next occurrence.
- Over a full-screen surface only seat chords and `Ctrl+A a` act; the
  picker, ledger, and prefilled prompts are held because nothing draws them
  there. The hold notice is only visible after leaving the surface.
- Whether seats write their handoff note before stopping is prompt guidance
  in the default and director stances, unmeasured. Tune it from what the morning rotation finds.
- `kj::ledger::tests::decision_span_keeps_the_ask_and_deciding_actor_separate`
  is flaky under the parallel test runner and passes single-threaded.
- `mcp::broker::tests::tool_call_spans_keep_requester_actor_and_reviewer_distinct`
  missed both spans in a parallel `tool_` test run; its isolated rerun passed
  (2026-09-17, /tmp/kaijutsu-tool-write-tests.log). Audit tracing subscriber and
  callsite ownership alongside the ledger test; the contributing factors are
  not established.

## What running under a benchmark showed (2026-09-18)

Harbor's ACP runner drove a coder context on a throwaway kernel with
deepseek-v4-flash. Evidence, event logs and the code audit:
`~/exomemory/kaijutsu/coder-early-stop-2026-09-18.md`. Recipe:
`contrib/bench/README.md`. Amy's observation that prompted it: contexts "stop
more readily than other agents". Open, most costly first:

- **Rerun the every-command-asks benchmark.** It spent 35 inferences and
  1.09M input tokens against 9 and 173K with the commands allowed, mostly
  the model hunting for output it had already produced. A model's turn now
  holds on its ask and reads the real output as its tool result
  (`docs/gate-resume.md`, "The turn holds"). Rerun the task, and delete this
  entry if the spiral and `asks_orphaned` are gone.
- **The waiting receipt is ambiguous on the non-blocking paths.** The MCP
  and RPC shell paths still return "the command has not run" with
  `is_error=false`; the refusal remedy (`kaijutsu-types/src/refusal.rs`)
  tells the reader to run `kj ledger allow`, which a model cannot do for
  itself. A model's own turn no longer reads it.
- **A client's own deadline may cut a long shell call short.** A model's
  in-kernel shell call now runs under its `timeout_ms` (10 minutes by
  default, at most an hour), is killed at it with its partial output, and
  the broker and kaish's watchdog sit above the maximum
  (`TimeoutPolicy::shell_command_*`). Calls that arrive over RPC, such as
  kaijutsu-mcp's `shell` or a client's `kj` (`gate::CLIENT_CALL`, 330 s),
  still carry the client's fixed deadline; check each path against the new
  maximum. The rest of the `timeout::gate` ladder waits on Slice 5 of
  `docs/gate-resume.md`.
- **`done` reaches a driver only when it reads.** `kj wait` and an ACP
  client see the verdict; nothing pushes it (a drift to the parent) to a
  driver that does not wait. Build the push when a driver needs it.
- **Over ACP, a kernel notice reads as the model's own words.** The bridge
  sends a `(System, Notification)` block (the `done` nudge, the ceiling
  notice) as `agent_message_chunk` text with no separator, so a client shows
  "Next I'll run the tests.You replied without a tool call…" and Harbor's
  trajectory records the notice as model output. A thought chunk or a
  separated, labeled message would read honestly
  (`fleet/done-nudge.toml` pins today's shape).
- **A background completion after `done` starts a turn that calls `done`
  again.** `kj wait` and `classify_run.py` read the last verdict, so they
  report the follow-up turn's, not the task's. Harbor reads the first.
- **The `coder-driven` bench overlay still asks for a `RESULT:` line**, now
  beside `done`. Retire the line or the overlay at the next bench arm.
- **Measure the narrowed coder seat.** It was about 14,400 input tokens on
  2026-10-01, 80% of it tool schemas, before the coder got its own binding
  and one shell. Most input tokens are reasoning replay, not the seat: one
  13K-token reasoning step doubled the next request.
- **Cut shell output accumulates in CAS.** Every model-facing result past
  8 KiB stores its whole stream (up to 4 MiB) in CAS, and CAS has no
  retention or reference counting (`kj cas rm` is unconditional). Measure
  growth over a benchmark run before choosing a policy.
- **kaish builtins differ from the programs a model expects.** `timeout`
  takes the wrapped command's flags as its own (`timeout 10 python3 -c` is
  refused, the same bug as tobert/kaish#484 for `env`); `grep -x`, `find -o`,
  `ls -S`, `cat -A`, and read-only kaish-git's `--oneline`/`--all`/`-a` are
  refused. Evidence and counts: `~/exomemory/kaijutsu/kaish-fixes-2026-10-01.md`
  (Amy is taking these to kaish).
- **A turn can still spend several output ceilings.** A ceiling stop now
  continues the turn with a notice instead of ending it, bounded by
  `MAX_OUTPUT_CEILING_CONTINUATIONS` (`runtime/llm_stream.rs`); see
  `docs/conversation-session.md`, "When an inference stops at the output
  ceiling". A model that reasons past the ceiling every time therefore costs
  up to that many ceilings before the turn ends anyway. Measure continuations
  per turn and how many of them recovered in the next benchmark arm before
  tuning the bound, and decide there whether the notice should also lower
  `effort` for the continuation.
- **No cumulative token count.** `context_usage` is a last-call snapshot, no
  real run emitted an ACP `usage_update`, and `PromptResponse.usage` is unset,
  so Harbor's token columns are empty. Totals come from the kernel log's
  `LLM stream completed` lines today.
- **ACP has no model selection.** Harbor's runner raises when `--model` is
  passed and the agent advertises none; the model is chosen through the
  agent's own flags meanwhile.
- **ACP `mcpServers` run, stdio only.** See "Client-declared MCP servers:
  what stays open" for what an MCPMark run may still hit.
- **File tools:** `read` truncates a line at 2000 characters with no way to
  page within it; `grep` stops at 200 matches without saying how many remain
  (`mcp/servers/file.rs`).

From the first Terminal-Bench 2.0 runs in containers (jobs under
`~/src/bench-work/harbor/jobs/kj-calib-1`):

- **A mid-stream transport failure still fails the turn.** The recorded runs
  carry "error decoding response body" and "Connection closed" after content
  had already streamed into blocks. Retrying there means deciding what happens
  to the partial blocks the first attempt wrote, which the stream-start retry
  policy does not cover; `runtime/llm_stream.rs` deliberately does not retry
  mid-stream to avoid duplicate kernel blocks. Bring partial-block handling to
  the design before changing it.
- **The bench adapter duplicates DeepSeek's output budget.**
  `contrib/bench/harbor/kaijutsu_solo_agent.py` passes 65536 on DeepSeek
  when `KAIJUTSU_ACP_MAX_TOKENS` is unset. DeepSeek's factory model rows now
  carry `max_tokens` 65536 at effort `high` (`seed_backends::DEEPSEEK_TUNING`),
  so the adapter's copy can go once a run confirms the kernel sends 65536.
- **Harbor puts the provider key in process arguments.** `run-harbor.sh`
  passes `--ae DEEPSEEK_API_KEY=…`, and Harbor's `docker-compose exec -e
  DEEPSEEK_API_KEY=sk-…` shows the key to anyone running `ps` on the host
  (seen 2026-10-03). Pass it through the environment or an env file instead.
- **A process the model spawns can read the kernel's environment through
  `/proc/<pid>/environ`** when it runs as root, which is usual in task
  containers. kaish clears the child environment and `kaijutsu-solo-acp` clears
  its dumpable flag, which stops a same-uid reader but not root. A provider key
  that reaches the kernel by environment is readable there; use a run-scoped
  key in a disposable container.
From the Qwen runs on current main (2026-09-30, `docs/benchmarks.md`, "Qwen
on current main"):

- **The kaish parser refuses common bash**, and the shell-escape guard turns
  the refusal into a denial: `master^`, `"<<<<<<<\|>>>>>>>"`, `( … )`
  subshells, `\(` in `find`, brace groups. 6 of 29 failed tool calls in four
  runs.

- **The tui shows stderr unmarked.** `present.rs` appends a tool result's
  stderr after its body in the block's own tone, while a model and ACP
  clients read a `[stderr]` line (`shell_envelope::join_streams`). A person
  cannot tell the streams apart. Color, not the text marker, may suit the
  tui better; decide against `docs/tui.md`.

## contrib/bench/boot-kernel.sh stops at its gate patch (2026-10-04)

The seeded `gate.toml` already carries a `[context_type.coder]` tier, so the
patch step reports "a [context_type.coder] tier is already present" and the
script exits before it writes `env.sh`. The kernel is left running and
usable, but callers that source `env.sh` fail. Treat an existing tier as done
(or check that it matches) and continue.

## From kj-ds4-tb2-20-64d8b208 (deepseek-v4-flash, binary 64d8b208)

- **Models fetch benchmark answers over the task container's network.** Four
  recorded passes copied a task's reference or fixture from GitHub or
  HuggingFace with host `git`, `/usr/bin/curl`, or Python
  (`docs/benchmarks.md`, "deepseek-v4-flash at effort high and a 64K
  ceiling"). Kaijutsu's egress list covers only the `curl` builtin. Open for
  Amy: deny these hosts at the container network, scan transcripts after each
  job and fail the trial, or both. A stance line alone does not stop a stuck
  model.
- **Spins at cb74a4f0 (Amy's coder stance), 2026-10-04:** extract-elf passed
  without fetching and stated its reading with a confidence (~70-75%).
  db-wal-recovery again ran `sqlite3` in place, lost the WAL, said so, and
  restored the fixture from harbor-framework/terminal-bench-2. Network
  restriction for task containers is the remaining fix.
- **db-wal-recovery at 661ec552 (orient on how tools treat data, one sample
  at a time), three DeepSeek runs:** 1 honest pass, 1 fetched pass, 1 fail.
  The honest run spent 3.8K output tokens before its second action and backed
  up the WAL; the other two spent 226 and 534, ran `sqlite3` in the same
  parallel batch as a hexdump, and lost the WAL. "One sample at a time" was
  not followed in any run. Offline probes that reasoned at length backed up
  every time, so the knowledge is present and the failure is the reflex
  batch. A System 1 command check is the next layer: flag a batch that
  inspects a data file and also opens it with a program.
- **A coder opened its own kernel's live `kernel.db`.** Run 2 lost the WAL,
  then searched kaijutsu's state directory for a copy and ran `sqlite3`
  against the running kernel's `kernel.db` before the agent process exited
  nonzero. The solo state directory is visible to the coder's shell. Either
  keep it out of the coder's reach or have the command check refuse writes
  and opens there.
- **The backup rule did not hold.** db-wal-recovery opened `main.db` with
  `sqlite3` before copying the WAL, and SQLite deleted it, as on 10-03.
- **configure-git-webserver is unscorable as written.** Its verifier logs in
  as `git` with password `password`, which the instruction never names.

## From kj-ds4-tb2-20-20261003 (deepseek-v4-flash, binary 9283226c)

- **Builtin refusals stop at the builtin.** `curl -w` is refused ("use
  kaish `--json`"), `tree --version` and `ps` are refused; the model then
  ran `/usr/bin/tree`. Do not point refusals at host programs: Amy wants
  fewer host tools exposed over time. Improve the builtins instead, starting
  with `curl --write-out` in kaish-tools-curl.
- **Builtin git is read-only and only in the read-only shell.** The
  writable shell reaches host `git` (absent until apt installed it in the
  task container). kaish-extras' `commit` and `worktree` axes are designed
  (`docs/design/architecture.md`, "depends on ledger") and not built. Amy:
  "We should be providing the builtin git tools too."
- **The write boundary was invisible** (configure-git-webserver, about 70
  calls). A read-only error now lists the mounts (in progress, branch
  `ro-mount-errors`). Still open: whether the coder orientation names the
  writable mounts up front.
- **A stuck model inspects kaijutsu itself.** db-wal-recovery (54 calls),
  configure-git-webserver (24), and extract-elf (25) searched `/v/cas`,
  `/v/swap`, `kj search`, `kj vfs snapshot`, rc and guard files, and their
  own `/logs/agent` for lost data, the write boundary, or a reference
  answer. Candidate orientation line: `/v`, `/config`, `/r`, and `kj` hold
  this session's own tools and history, not task data.
- **Builtin gaps that cost calls:** `curl -w` (two tasks), `ls
  --time-style`, `tree --version`, `ps`. kaish refuses `ps -p 1 -o comm=`
  as "adjacent words" at parse time; `comm=` is one word in bash (kaish
  question).
- **The `curl` builtin's egress allowlist** refused pypi.org and
  sourceforge.net while host `pip` and `apt-get` reached the network in the
  same container. Benchmarks now open it (`egress_allow`, default `*`,
  `docs/benchmarks.md`). Open: say in the refusal that the allowlist is
  kaijutsu's.
- **Three losses were reasoning past a 16K ceiling** (dna-assembly,
  headless-terminal, model-extraction-relu-logits): 4 `length` stops each,
  166-186K characters of reasoning, no tool call. The baseline ran with
  `max_tokens` 32768; this run left it unset (factory 16384) at effort
  `max`. The adapter now passes 65536 on DeepSeek. Comparisons must match
  the ceiling in provenance.
- **Nothing tracks the OS processes a context starts** (design, Amy: "a
  cgroup later would be rad"). Daemons a model launches from a script
  (configure-git-webserver's sshd and web server) leave no record: no way
  to list what a context left running or stop it on archive. Candidates: a
  cgroup v2 per context, or the kernel as a child subreaper
  (`PR_SET_CHILD_SUBREAPER`) attributing orphans by session or process
  group. Touches kaish's spawn path.
- **Do not build during a timing-sensitive run.** query-optimize failed its
  timing check (1.21 s against 1.08 s) while niced test builds ran on the
  host; the sample is confounded.
- **Writable mounts for benchmark containers: deferred** (Amy: "/git and
  /srv be writeable and mapped into the workspace, seems the most logical
  option… I don't want to inject mounts yet"). In the 20-task subset only
  configure-git-webserver (`/git/server` is required; the webroot is the
  model's choice) and sqlite-with-gcov (`/usr/local/bin/sqlite3`, or PATH)
  write outside `/app` and `/tmp`; package installs run in host programs,
  outside the mount table.

## A builtin `grep -r PATTERN /` killed the agent process (2026-10-01)

In `kj-ds4-tb2-20-1`, extract-elf and headless-terminal each ran
`grep -r PATTERN / ... | grep -v '^/proc' ...` through `shell_write`, and
`kaijutsu-solo-acp` exited mid-command with nothing logged: both losses of
the run.

Reproduced 2026-10-02 on a kernel test shell under a 1 GiB memory cap:
`grep -r zzqq /proc/self` was killed for memory (exit 137, no output).
Contributing factors:

- kaish's `grep` is a builtin, so the walk and the reads run in the kernel's
  own process. `grep -r` collects every path first, then reads each file
  whole (`ctx.backend.read(path, None)`).
- `MountBackend::read` reaches `LocalBackend::read_all`, which is
  `fs::read` to end of file with no ceiling.
- `/proc/<pid>/pagemap` reports size 0 and returns 8 bytes for every page of
  a 47-bit address space, about 256 GiB. A plain Rust `fs::read` of it under a
  256 MiB cap is killed the same way. As root in a task container, every
  process's `pagemap` is readable.
- The `grep -v '^/proc'` exclusions run after the walk.

Not factors: in a kaish shell `/dev` is kaish's own device mount, and a whole
read of `zero` or `urandom` fails with an error, so `grep` skips it. Walks of
`/v` finished; they cost time, not memory. That `solo-acp` reported exit 1
rather than a signal is not explained.

Shipped 2026-10-02: the kernel unlists the host's `/proc` and `/sys`
(`MountTable::unlist`, `docs/mounts.md`, "Unlisted paths"), so a walk from
`/` does not enter them and naming them still works. A walk from `/` passes
through the host and workspace mounts and stops at `/config`, `/run`, `/v`,
`/r`, and `/dev`, naming the mounts it skipped once on stderr
(`docs/mounts.md`, "Where a walk stops"); `--cross-mounts` crosses. kaish's
recursive `grep` skips devices, FIFOs, sockets, and discovered symlinks, and
reads in 256 KiB chunks.

Decided 2026-10-02 (Amy). Kaijutsu becomes less privileged, so that it is
freer inside its own space: "I can connect to the host if I need that." The
unlist also takes `/proc` and `/sys` out of the FSN view and SFTP listings,
and that is wanted. Still open:

- **A read that names `/proc` can still kill the kernel**
  (`cat /proc/self/pagemap`; `grep -C 2 x /proc/self/pagemap` still buffers
  the whole file). Do not cap file reads: a
  limit inside file I/O can easily corrupt data by accident. Bound the blast
  radius of a kaish execution instead: one memory limit for the whole
  execution, which the user can raise. Exceeding it stops the interpreter,
  and the kernel survives. A stopped interpreter is a place to inspect and
  intervene. Open question: could peeking at an oversized kaish, killing
  its running commands, and playing the rest out work as an interrupt, more
  like `kill -9` or power loss than a clean cancel, with partial recovery
  someday? Needs a design conversation: where the accounting lives (the
  kernel process, a per-execution allocator, a cgroup) and what the receipt
  says.
- **Find where a model ends up at a bare `/`, and fix the UX/AX there**,
  rather than a guard rule against `grep -r PATTERN /` (decided against: it
  is lexical, and it treats the symptom). Candidates: a context with no cwd
  starts in the kernel's own directory; a removed cwd moves to its nearest
  ancestor; a home-dir fallback; benchmark tasks where the model does not
  know its tree (`/app`) and searches the whole host to find it.

## Where a model ends up at a bare `/` (2026-10-02)

In the Terminal-Bench runs, no model reached `/` by accident. Every whole-host
walk was deliberate, from a shell whose cwd was `/app`. Fix what sent the
model there, not the walk. Read from `kj-ds4-tb2-20-1` and
`kj-tenchi-tb2-20-2` (40 trials), plus `kj-qwen-sqlite-1`. Both
binaries predate c2133264, so no run had the coder orientation block, and
each ACP context was created first and given its cwd afterward.

What the model knew: `pwd` printed `/app` in every trial that asked, the
task text names `/app` in most tasks, and `solo-acp` mounted `/app`
read-write. A `find /` or `grep -r … /` ran in 10 of 40 trials, plus the
qwen run. By purpose, most frequent first; one trial can have more than
one purpose:

1. **Looking for the grader, the tests, or a reference solution** (7
   trials; both crashes). extract-elf (both runs), headless-terminal,
   constraints-scheduling, configure-git-webserver, fix-code-vulnerability,
   raman-fitting. extract-elf step 15: `grep -rl "extract.js" / … | grep -v
   '^/proc'`, after "Let me search the filesystem for the task/reference".
   headless-terminal step 17: `grep -r "base_terminal\|BaseTerminal" /`.
   The root listing invites the hunt: it shows kaijutsu's own `/config`,
   `/r`, and `/v` beside the harness's `/installed-agent` and `/logs`. Models
   read `/logs/agent/acp.txt`, which is this kernel's own stderr with the
   model's own thinking in it. In fix-code-vulnerability, `grep -rl
   report.jsonl /` found only that log. The Harbor adapter now unlists
   `/logs` and `/installed-agent` (`kaijutsu-solo-acp --unlist`), so walks
   and `ls /` skip them; naming them still works. Proposed fix: the inversion in
   `docs/mounts.md`, "The inversion". `/` holds the workspace, `/tmp`, the
   kernel's own trees, and the mounted toolchain, not the host. A walk
   from `/` is then short and finds only the model's space, and it cannot
   reach `/proc`. The host stays reachable by naming it, as Amy put it: "I
   can connect to the host if I need that." Until then, the orientation can
   list the writable mounts and say that the rest of `/` is the host,
   read-only.
2. **Looking for a program that the read-only `shell` said was missing**
   (3 trials). The `shell` had no `PATH`, so `which python3` printed
   `which: no python3 in ()` while `/usr/bin/python3` existed. In
   dna-assembly, `find / -maxdepth 3 …` came next. sqlite-with-gcov (tenchi)
   got `MISSING` from `command -v` for `gcc`, `tar`, and `gzip`, then ran
   `find / -maxdepth 4 -name 'gcc*'`. extract-elf (tenchi) got "node not
   found", then ran `find / -maxdepth 5 -name 'node'`. `which` is fixed:
   the read-only `shell` now gets the writable shell's `PATH`
   (`ExternalExec::LookupOnly`), and running the program it finds is
   refused with the hint that names `shell_write`. `command -v`,
   `command -V`, and `type [-t]` are built in kaish on branch
   `one-filesystem-walks` (c7cdc8ee) and reach kaijutsu with that re-pin
   (kaijutsu branch `one-filesystem-walks`). Until then `command -v gcc` is
   refused on `shell`, and `command -v gcc >/dev/null 2>&1 || echo
   MISSING` reports a present program missing.
3. **Looking for an input file the task had already placed in the cwd** (3
   trials). chess-best-move ran `find / -name "chess_board.png"`,
   headless-terminal ran `find / -name "*terminal*"`, and dna-assembly ran
   `find / -maxdepth 3 -name 'sequences.fasta'`. Each also ran `ls /app` in
   the same call, so the model was not lost; it wanted confirmation. The
   coder orientation (`coder/create/S35-orient.kai`) now lists the top
   level of the cwd and should answer this. Rerun the subset to measure it.
   Most trials also opened with `ls /`, which costs one call and is
   harmless.

Paths in kaijutsu that lead toward `/` or away from the work tree. None
appeared in these runs. Fixed on 2026-10-02: `kj context create` takes the
current context's cwd; the tui and kaijutsu-mcp pass their own directory
(created without it, and saying so, when the kernel refuses it); ACP maps the
refusal to `invalid_cwd`; a removed cwd never moves to `/` (it stops at its
mount root, or is cleared); one check that matches `cd` backs every way to
set a cwd; `kj fork` checks an inherited cwd; `cd` follows a symlink to a
directory; a shell result says `[cwd now DIR]` when the call changed it.
Still open:

4. **A context with no cwd runs its shell in the kernel's `$HOME`**, or in
   `/tmp` when `HOME` is unset (`runtime/context_shell.rs`). The file tools
   refuse a missing cwd; the shell does not, and should not: `cd` and `kj
   context set` run inside the shell being refused, a read-only seat
   discards `cd`, and kaijutsu-mcp against a remote kernel would lose its
   shell. A coder created without a cwd is told so by its create
   orientation. Other context types are not told. Paths that still create
   one: a create from a context with no cwd (root consoles have none), and
   a client directory the kernel refused.
5. **`/` policy: decided and shipped.** Amy, 2026-10-02: "/ should be
   allowed as cwd by kaish, but not by kaijutsu. kaijutsu can make noise
   any time cwd is /, it's almost always a mistake." `/` stays accepted,
   so no seat is trapped, and every surface that leaves a shell there says
   so: a `warning` on every shell result, an output line from `kj context
   create|set` and `kj fork`, an ACP agent message, and the coder
   orientation. See `docs/shell-envelope.md`, "A cwd of `/`".
6. **Two small cwd leftovers.** The ambient `getCwd` RPC answers `/docs`
   for a context with no cwd (`rpc.rs`, `get_cwd`); no client calls
   `getCwd` or `setCwd`, so both can become `retired8`/`retired9` stubs.
   A durable `PWD` export overrides the real cwd at construction
   (`ContextShellInputs::environment`, pinned by a test); reserve `PWD` in
   env writes, or set it after exports. The kaijutsu-mcp `shell` envelope
   leaves `cwd` `null`, since its block snapshot carries none.

Seen in the same trajectories, outside this entry: kaish `ls -la` printed
one tab-separated `name  type  size` row per file, with every size 0. Several models called it "garbled" and
fell back to `ls --json`. That costs calls and trust, not a walk; it is a
kaish note.

## `rc_lifecycle_wire` flaked once under load (2026-10-02)

`shutdown_joins_nested_rc_before_settling_the_command` panicked at
`rc_lifecycle_wire.rs:32` — the `unwrap()` on `list_active_contexts()` in
`wait_child`, so the database read itself errored (likely busy), not the
10 s wait. It passed alone and in the next full `kaijutsu-server` run. If it
recurs, have `wait_child` report the read error and retry it, instead of
panicking on the first one.

## Read-only refusals that name the mounts: what stays open (2026-10-03)

A refused write now names the mount and lists the writable ones
(`docs/mounts.md`, "When a write is refused"). Open:

- **kaish prints `invalid operation:` before the text.** kaish's
  `BackendError::ReadOnly` carries no text, so `MountBackend` hands the
  refusal over as `InvalidOperation`. A kaish variant that carries text
  (`ReadOnly` with a reason, or a new one under `#[non_exhaustive]`) would
  print `touch: /git/x: read-only: kaijutsu mounts / read-only. …`. Needs a
  kaish release and a rev bump here.
- **kaish `cp` drops the destination path on a write error.**
  `cp /app/hook /etc/hook` prints `cp: invalid operation: kaijutsu mounts /
  read-only. …` with no `/etc/hook`; `name_error` names only source paths.
  It did the same with the bare `read-only filesystem`. Upstream kaish.
- **kaish's own mounts refuse with their own text.** In a shell, `/dev`,
  `/v/docs`, `/v/jobs`, and `/v/bin` are kaish's mounts, not the kernel's,
  so a write there does not list the kernel's mounts.
- **Writable `/config/*` trees are listed as rw.** They are writable, so the
  list names them; whether a refusal should point a model at its own
  configuration is open.

## Wrapped python: what stays open (2026-10-03)

`python3` and `python` are wrapped commands and `KJ_TOOL_PLAN` names their
program (`docs/kaish-integration.md`, "Wrapped python"). Open:

- **The judge.** Nothing reads `interpreter.code` yet. The shell-escape
  guard still parses python's argv in jq; it can read `interpreter.source`
  instead, which also covers `.venv/bin/python` and `python3.14`.
- **Wrappers hide the interpreter.** `timeout 60 python3 -c …`,
  `env python3 …`, and `xargs python3` plan with the wrapper as the command,
  so they carry no `interpreter`.
- **kaish's parser binds options anywhere.** CPython stops at the first
  operand; kaish's wrapped parser does not, so a trailing value option in a
  program's own arguments (`python3 tool.py -m`) is refused with exit 2. A
  `Tail` mode that ends option parsing at the first operand would fix it in
  kaish and would let kaish's plan carry the wrapper's own reading.
- **kaish lifts `--json` from a wrapped command's raw argv.** The python
  tool clears the output format; kaish's `WrappedTool` should never lift it
  under `Tail::Forward`.
- **`type python3` says "shell builtin".** kaish has no resolution kind for
  a wrapped external.
- **kaish pins a wrapper's executable.** The python tool re-resolves the
  name on each call so `PATH` order still picks a virtual environment. A
  kaish option to resolve at call time would remove that adapter.
- **The plan reads unknown options as harmless.** `python3 -Z x.py` plans
  a script, where CPython exits 2. A relative `PATH` entry resolves against
  the kernel process cwd in both the wrapper and kaish's external path, not
  the shell's cwd.
- **`pip`, `uv`, `pytest`, and other interpreters** (`node`, `perl`, `ruby`)
  are not wrapped.

## From the one-filesystem walks re-pin (2026-10-02)

Found while re-pinning kaish onto `one-filesystem-walks` (kaijutsu branch
of the same name; kaibo DeepSeek review). Not fixed:

- **The shell-escape guard cannot see a wrapper's expanded argument.**
  `exec ${SH} -c '…'` and `env $SH -c '…'` plan the interpreter as
  `{"plain": "${SH}"}`, so the guard passes them. On a normal kernel they
  reach a person's ask rather than an allow. Closing it is a policy choice:
  deny any wrapper with an expanding argument (which also catches
  `timeout $T make`), read the session value at plan time (the plan's
  `free_variables` note says a `read` in the same statement defeats that),
  or accept the ask. A variable as the command word itself (`${SH} -c '…'`)
  does not parse in kaish, so the guard refuses it for lack of a plan.
- **The read-only `kj` exemption treats `${VAR}` in a value slot as
  harmless.** A token such as `--out=…` breaks that. It is latent today:
  every verb with that flag needs a positional the token would displace.
- **Mount paths are matched without normalizing.** `--unlist //logs` and
  `--unlist /a/../b` are accepted and hide nothing. `--rw-mount
  /tmp/../config/rc` passes the kernel-root check. Normalize (or refuse a
  path that is not already normal) at parse time.
- **Stale comment**: the approval ledger's `ValueKind` says it mirrors
  kaish's `PlannedValue`; it no longer does.
- **Stack**: kaish uses more stack per rc lifecycle with each recent pin.
  At kaish c7cdc8ee one test overflowed its 2 MiB test thread; at 118ee69d
  and ae915794 five more did (`rc_source_edit_applies_only_to_later_lifecycle_runs`
  needs between 2 and 3 MiB). All six now run through `crate::on_rc_thread`.
  Production kaish threads have 16 MiB. Nothing measures the margin, so a
  deeper nest or a further kaish change could reach 16 MiB unseen; a test
  that runs the deepest shipped rc nest on a smaller stack would show it.

## From the kaibo DeepSeek review of 2026-10-01's changes

Findings the review raised that are not fixed yet (kaibo `job-2`, deepseek):

- **The `executeKj` RPC ignores the shell facade.** It runs
  `ShellPolicy::Agent` with no facade check (`structured::execute_kj`), so a
  toolie seat still reaches mutating `kj` verbs through it, subject to each
  verb's own capability gate and the ask tier. `executeKj` is the person's
  `kj` path. `shellExecute`, shell drafts, and the streaming `execute` follow
  the facade (`Broker::check_shell_facade`). The read-only RPC shell's result
  hooks still match as `shell_write`; only its PreCall presents the read-only
  `shell`.
- **`setContextCwd` writes a context's cwd with no facade check**
  (`rpc.rs`, `set_context_cwd`), so it moves a toolie seat's cwd although its
  shells leave cwd unchanged. Changing the cwd is not a file write, but it is
  the one remaining RPC writer of shell state in a read-only seat.
- **The register path still announces tools before a context's first model
  block** (`emit_for_bindings`), unlike the binding-diff path.
- **The shell descriptions name four of the facts a turn reads**; `[waiting
  for approval…]`, `[timed out waiting…]`, `[data]`, and `[latch]` are not
  described, and the ceiling-case truncation note has a different shape.
- **A background job's live stream is uncut and unbounded on the kernel
  side** (`command.rs`, an unbounded channel of cloned results).

## Leftovers from the solo-acp state work (2026-09-18)

Smaller, from the same work:

- `kaijutsu-mcp` and `kaijutsu-acp` each carry a copy of the key-flag
  resolution and the personal-key warning. Both are pure and belong beside
  `KeySource` in `kaijutsu-client`.
- `kaijutsu-mcp` names its context flag `--context-name`; `kaijutsu-acp` uses
  `--context-type` and `--character`. There is still no way to run one `kj`
  command against a kernel from a shell without speaking MCP or ACP;
  `contrib/bench/kjmcp.py` fills that gap for the harness.
- Provider HTTPS verifies through `rustls-platform-verifier`, which reads the
  on-disk CA bundle with no bundled fallback. A static binary in an image
  without `ca-certificates` fails every model call.
- Static builds: `contrib/bench/Containerfile.static` duplicates the root
  `Containerfile`'s build stage. Amy prefers static binaries for deployment.
  Put the per-target link flags in `.cargo/config.toml`
  (`[target.x86_64-unknown-linux-musl]`; a blanket `RUSTFLAGS` breaks
  proc-macro crates), build the root image static, export the binaries from
  it, and delete the second Containerfile. Measure musl's allocator under load
  before a static kernel serves real work.

## The uncovered tier does not reach a `KjVerb` ask (2026-09-18)

`gate.toml`'s `uncovered = "allow"` (`docs/gate-policy-tuning.md`, "The
uncovered tier: a sandbox posture") rides the config layers, and an
`Origin::KjVerb` ask never meets them: its one live caller passes
`gate_policy::no_config()` (`kj/cc.rs:112`, `kj cc send`), and
`lower_layers_per_gated_statement` (`kj/gate_policy.rs:810`) returns
`Uncovered` for that origin because there is no planned program to key on.
So a sandboxed kernel still asks on those. Either those callers should load
the file and the evaluator should let the tier decide a plan-less ask, or
the boundary stays and the `kj cc` help says so. No benchmark has hit it
yet.

## The uncovered tier: three seams left open (2026-09-18)

`docs/gate-policy-tuning.md`, "The uncovered tier: a sandbox posture".

- **No broker-level PreCall test loads the tier.** The evaluator is pinned
  in `kj/gate_policy.rs` and the shell gate in `kj/ledger.rs`, but nothing
  asserts that `evaluate_phase_with_mode` (`mcp/broker.rs`) skips hooks for
  a program the tier allows, which is the behavior an operator feels most.
  It needs a test in broker.rs's own test module.
- **A commandless statement carries no tier stamp on `KJ_TOOL_PLAN`.** The
  stamping loop zips `stmt.plan.commands` with the JSON twin
  (`mcp/broker.rs`, `run_kaish_hook`), so a statement with no command — an
  assignment, an exit, a `[[ ]]` test — gets no `tier` field. The evaluator
  still decides it, and a hook that reads tiers per command finds nothing to
  read, so this is cosmetic today. It becomes real if a hook ever scores
  statements rather than commands.
- **No startup line says the posture is on.** `kj ledger rules` states it,
  and every auto-decision names it in its durable row, but a kernel booted
  with a copied-in sandbox `gate.toml` says nothing. The place for one is
  `create_shared_kernel` in `kaijutsu-server/src/rpc.rs`, in the loop that
  seeds and mounts each config tree and already logs where a non-default
  tree came from; the line would load `gate.toml` there and name the
  sections setting `uncovered = "allow"`. A boot-time load also has to
  decide what an unloadable file does at boot, where today nothing reads it
  until the first shell submission.

## The isotest harness could use the uncovered tier (2026-09-18)

`crates/kaijutsu-isotest/tests/common/mod.rs:65` keeps `HARNESS_ROOT_ALLOW`,
a hand-listed allow tier for the harness's own setup commands, because an
unanswered ask hung the run. The harness tests process isolation and VFS
protection, not the gate, and it throws its kernel away per run — a
`[context_type.root] uncovered = "allow"` section would replace the list and
stop it drifting as setup commands change. Left alone here: that crate is
another lane's this week.

## Split admin grants between `root` and `director` (2026-09-16)

`director` is banto's model seat and still carries the whole operator grant
set (`assets/defaults/rc/director/create/S10-binding.kai`), which `root`
now also holds. Decide which grants banto keeps (likely drive, fork, drift,
operator) and which belong to roots only (likely `admin`, `config-write`,
`system`). Amy chose the split on 2026-09-16 and left the grant list open.

## Identity audit: what stays open (2026-09-15)

Amy: *"are all the gate sites using the right identifiers to check who it
is?"* Scan by the lead plus a kaibo (deepseek) audit against
`docs/approval-identity.md`; every line below was re-read by the lead. Full
notes: `~/exomemory/kaijutsu/identity-gate-audit-2026-09-15.md`.

The sheet-level `accountable_to` that shipped at noon is gone, replaced by
the walk up the context forest ("Roots, accountability as a runtime
relation"). Reviewer resolution, delegation, answering, cancel/escalate,
redemption, turn identity, `require_cap`, the facade gate, and every
draft/shell RPC read the identifier the doc names. Open:

1. `authorBlock` takes `principalId` from the request (`rpc.rs:8687`), the
   only RPC that does; documented as shared-trust in `kaijutsu.capnp:2232`.
2. The hook listener authors under `for_agent_session` with a `system()`
   fallback (`hook_listener.rs:949`); the bridge-identity design in
   `docs/character.md` replaces it.

Verified by `crates/kaijutsu-server/tests/user_input_identity.rs`: a human
with nobody responsible above her runs a gated shell command, the ask
snapshots actor == reviewer, and her own `kj ledger allow` confirms it and
executes the command. That is the self-confirmation the walk settles on;
the earlier unanswerable Pending row, and the in-band refusal that briefly
replaced it, are both gone.

Seen while writing that test: the test's blanket Ask hook re-gated the
`kj ledger allow` typed into the same context, so the test answers from a
hook-free context. The shipped evaluator exempts `kj ledger` as a whole verb
(`docs/gate-policy-tuning.md`, "Builtin tier"), so this is likely a test
artifact; a test that pins the exemption under a custom hook would settle it.

## The compose draft is the player's alone (Amy, 2026-09-15)

Amy: *"the draft should never have a path for the model to reach it. we may
add things to the apps to make testing easier but that draft has to be user
only for some of our assumptions about safety to hold up."* And: *"we'll
allow the mcp for now, we use it a lot for testing, but we should mark it for
removal later, y'all have drive and drift for talking to each other."*

The draft is the per-principal input block written by `edit_draft`
(`crates/kaijutsu-kernel/src/block_store.rs`). The kernel editor cannot open
it: `resolve_editor_target` (`crates/kaijutsu-kernel/src/editor.rs`) binds to
file-backed blocks only, and no dedicated `kj input` verb exists. The intended
contract is no model VFS path for the draft and no editor session over it.

The dedicated `/v/input` mount and generic model routes (`/v/docs`, `kj`
block commands, search/synthesis, and MCP block reads) now exclude drafts.
Generic status commands cannot create or promote them; historical reads
refuse draft-era content even after submission. Client compose and feed
queries retain draft access. Regression coverage includes distinct requester
and performer identities in both model-shell flavors.

The MCP bridge's `read_input`/`write_input`/`edit_input`/`submit_input` tools
are gone as of today. A model that wants another player's attention uses `kj
drive` and drift instead. What remains open: the facade gate at the wire
(`facade:edit_input`/`facade:submit_input`) reads the context binding and
cannot tell a human connection from a model principal holding its own
credential, so the assumption that "a user block was typed by a human" is
not yet enforced at the wire. That is the next design conversation.

## Thinking folds to a summary line once the player has moved on (Amy, 2026-09-12)

Amy: *"I'm watching y'all work, and you're thinking, I often read/scan it
because I'm curious, but it takes up a lot of space, so I'd like to collapse
it but keep *something* visible. so I want it to collapse in the app on
hydrate or a second or two after the thinking ends and I've likely moved on.
I think in the tui, the thinking preview thing would collapse to the
summarized line on the terminal history."*

**Shipped 2026-09-12, kernel/wire/tui half.** `kaijutsu_types::summarize_thinking`
(sentence boundary needs trailing whitespace, 120-char cap, markdown
markers stripped); `summary` on `BlockSnapshot`/`BlockMetadata` and the
kernel's `BlockContent`, journaled as a full snapshot like `stderr`;
capnp `BlockSnapshot.summary @45`, `BlockMetadata.summary @9`; set in the
server's `ThinkingEnd` arm before the status flips to Done; the tui stub
shows it; `kj block inspect --json` prints it. **Remaining: the app's
per-viewer fold**, handed to the moltar session through the exomemory
daily (fold on hydrate, fold a second or two after `onTurnCompleted`, an
explicit toggle pins open, never `set_collapsed`).

**Before.** The kernel keeps a durable `collapsed` flag per block, flipped
only by an explicit toggle (`set_collapsed`, `CollapsedChanged`); nothing
collapses on its own. The app shows thinking expanded until toggled. The tui
holds the turn's thinking in its pane and, when the block completes, prints
one stub into scrollback: `▸ thinking · N lines · <first line>`
(`present::thinking_stub_line`). The first line is a poor stand-in for a
summary. No block carries a summary today; `kaijutsu_index::synthesis`
has an extractive `best_sentence` that could produce one without a model.

**The plan.**

- **The kernel derives the summary, once, at completion.** When a
  `Thinking` block settles, the kernel computes one line and stores it on
  the block, published on `BlockSnapshot` as a new field and through the
  change feed. Start extractive (`best_sentence` over the block, capped),
  which is synchronous and free; a model-written summary, a small local
  model or a flash-tier cloud model, is a later swap behind the same field and can be
  driven by rc. Display only: the summary never enters hydration.
- **The app folds locally.** A completed thinking block reads as collapsed
  on hydrate, and a block that completes while watched folds a second or two
  after it settles, showing the summary line. This is per-viewer
  presentation state, not the durable `collapsed` flag: one player folding
  must not fold a sibling's screen. An explicit toggle still works and pins
  the block open. Timing constants live in one place and are relative to
  settle, not to wall clock at hydrate.
- **The tui's stub uses the summary.** Same stub, summary in place of the
  first line, first line as the fallback when no summary exists. The stub
  prints when the block completes, as now; an extractive summary is ready
  by then. A model-written summary would arrive later, and the stub cannot
  be redrawn once printed, so that path either delays the stub (risking
  document order against a fast answer) or lands the summary elsewhere. Do
  not ship a slow summary source without answering this.

**Decided** (Amy, 2026-09-12): *"yes extractive now, model later. agreed no
on durable flag, for now anyways. from turn end I think."* So: the summary
is extractive in the first cut with a model source as a later swap; the fold
never writes the durable `collapsed` flag; the app's fold delay counts from
the turn ending, not from the block settling, which also matches the tui
pane's lifetime.

## The player's edge: what is still open (2026-09-13)

The kernel, wire, `submit` rc verb, shipped example, and the tui landed on
2026-09-13 (`docs/prompts.md`, "The submit verb"; `docs/devlog.md`, "The
message that knew where the player was looking"). Left:

- **The Bevy app sends no edge.** `submit_input` still compiles and sends
  none. The app's edge is the newest block its viewport had shown at Enter,
  older than the log tail when scrolled up, with the character count when
  that block was still streaming. `ActorHandle::submit_input_with_edge` is
  the call. Handed to the moltar session through the exomemory daily.
- **No type links the example.** `lib/submit/S10-edge.kai` renders the
  excerpt; a type opts in by symlink into its `submit/` directory. Decide
  which shipped types should link it once it has been watched live; the
  ordinal and percentage experiments Amy named are further scripts against
  the same variables, not kernel work.
- **The notification lands after the message.** Hydration reads the user
  message, then "the player wrote the message above while looking at…".
  If a model reads the reference late, move the script's block before the
  input block with `--after`; the facts already carry both ids.
- **Every chat submit writes a ledger run row**, even for a type with no
  `submit/` directory: `start_run` fires before the script list is loaded
  (`rc/mod.rs`). Consistent with the other verbs, but submit is a
  hotter path than create or fork. Skip the row when no script exists, or
  accept it once measured.
- **The edge never rides the change feed.** The promotion emits
  `StatusChanged` and `MetadataChanged`, and `BlockMetadata` has no edge
  fields, so a live replica's copy of a promoted block reads no edge until
  it refetches. Nothing reads a stored edge from a mirror yet. From the
  kaibo review (deepseek, 2026-09-13).

## Input during a turn: what is still open (2026-09-26)

Submits, completion notices, and drift arrivals during a running turn join
it after a tool round or after the final inference
(`docs/conversation-session.md`, "Input during a turn"). Amy: "if I submit
async it should go as soon as possible and not have a new turn queued";
"drift and completion should land with asap delivery". Left:

- **Not watched live.** A wire test drives a draft submit with the linked
  `S10-edge.kai` through a held turn
  (`compose_draft_wire::a_draft_submitted_during_a_turn_reaches_its_next_request_with_its_edge`);
  nobody has yet sent a note from the tui or app during a real turn.
- **A running shell pair in the delivered span is skipped for good.** The
  write point moves past a user shell command whose result has not
  arrived; the result lands before the write point, so this turn never
  sends it, and the next turn's hydration places the pair earlier than
  this turn's later output.
- **A follow-up turn drops the `prompt` RPC's model override.**
  `Wake::Submit` carries principal and session only.
- **A note can hide a ceiling stop.** When the ceiling-continuation budget
  is spent, a pending note still earns an inference, with the truncated
  text replayed and no ceiling notice; the turn then reports the later
  inference's stop reason. Notes also extend a turn without bound, like
  tool rounds ("Per-cast turn token budget"). From the kaibo review
  (deepseek, 2026-09-26).
- **Ingress cleanup is linear.** `process_llm_stream` closes the ingress
  after `run_llm_stream` returns; `TurnLease::drop` does not. The worker
  joins turn tasks rather than aborting them, so nothing skips the close
  today; after a future abort path, the context's next turn would panic
  at `open_ingress`.

## Blocks stored out of order before 855ace8a (2026-09-26)

`order_midpoint` could return a key below its lower bound, so a block
inserted between two others sorted before its anchor (`855ace8a`). The
likeliest victim is a tool result sorted before its call, which hydration
repairs by synthesizing an "interrupted" result and dropping the real one.
Nothing re-sorts blocks already stored, and no verb moves a block.

Sized on moltar 2026-09-27 (backup of the 09:49 deploy, scratch scanner over
`load_one_from_db` + `blocks_ordered`): 58 of 747 tool results (7.8%) sort
before their call, in 5 of 12 contexts, from 09-21 through 09-26. Calls
inserted at one anchor also landed in reverse order. Banto's 09-21
`read_shell_operation` results are among them, which accounts for the
"interrupted (context was forked or pruned)" errors in its problem report:
snapshot repair synthesized them live. Every one predates the fix.
`kaijutsu-server blocks repair-order` repairs them (`docs/server-cli.md`);
moltar and zorak each need a run. Stored keys also tie (134 adjacent pairs on
moltar): `order_midpoint(a, a)` appends after `a`, so an insert meant to land
between two tied blocks lands after both. Nothing re-keys ties outside a
damaged run.

## Async completion recovery follow-ups

- RPC PostCall hooks can replace output/status while the durable exit code
  still records the executed command. Define separate command outcome and hook
  outcome before changing job summaries to infer a synthetic exit code.
- Shell operation inspection currently uses bounded result/block output.
  Integrate kaish job streams and spill references for live, complete output
  retrieval without invalidating pagination offsets.

## Config defaults ARE the config; the VFS path proxies XDG (Amy, 2026-09-12)

Design direction for `theme.toml` and the other `/config/kernel` singletons
(`mcp.toml`, `gate.toml`, and the client configs). Today a file is seeded from
an embedded snippet on first boot, and there are three copies of the theme:
the app's compiled fallback (`ui/theme.rs`), the kernel's embedded seed
(`config_seed.rs` `DEFAULT_THEME` from `assets/defaults`), and the live host
file. Amy's shape:

- The compiled defaults become Amy's actual settings, so the common case needs
  no file on disk ("for now" — fine in a no-outside-users learning space).
- Stop seeding a snippet. A host file exists only when someone writes one to
  override; `seed_entries_into_dir` no longer plants theme et al.
- The defaults are printable, not shipped: a `kj config` verb emits the
  effective defaults as TOML so a human can capture a starting point to edit.
- The VFS path `/config/kernel/theme.toml` stays the interface characters and
  tools use (the raw XDG path is never exposed to them) and proxies to the XDG
  host file. Copy-on-write from defaults: an absent file reads as the
  synthesized defaults, a write creates the override.
- The kernel's defaults become the one source; the app's pre-connect fallback
  shrinks to a minimal neutral default instead of mirroring the whole theme.

Interacts with `docs/color.md` (theme ownership — this keeps the kernel the
owner but makes the default the body), the "theme changes never reach a
running app" entry (the read is already live per connect; only the write side
and the seed change), and the theme-lane / client-local question (whether the
theme should move to the `/config/client` cascade since it is per-display
presentation). Sequence AFTER the pending origin/main pull, which touches the
approval/config area.

## The approval sheet and ribbon have no dim behind them (2026-09-12)

The ask sheet and ledger ribbon float over the conversation or room at full
surrounding brightness, so they compete with the transcript instead of
reading as a popup. A first attempt at a full-frame scrim node (a childless
`Node` with a translucent `BackgroundColor` at a `GlobalZIndex` just under
the panel) did not render: the entity existed at full viewport size with the
right color, but its `ViewVisibility` stayed 0 while every content panel at a
`GlobalZIndex` drew normally. A childless background-only UI node behaves
differently here; the working panels all carry a child surface. Reproduce
with `world.get_components` on the scrim entity (Visibility Inherited,
InheritedVisibility true, ViewVisibility 0). Removed rather than shipped
non-functional. When revisited, either give the scrim a drawn child or use
the same surface machinery the panels use; the conversation-walls work may
restructure this layer anyway.

## The app runs as Amy, so she cannot answer her own gated asks (2026-09-12)

**Re-check on the live kernel before acting.** This predates "a root confirms
itself" (`3d69e765`, 2026-09-15): `can_review` no longer requires
`principal != actor`, and `a_root_actor_confirms_its_own_ask` in `kj/gate.rs`
covers the path. If Amy's character is a root with no reviewer, the ask below
may now be answerable. The open question is then whether the app still gets a
distinct performer character for a clearer audit record.

Verified live on 2026-09-12. A `shell_write` in the app that trips the
gate raises an ask whose requester, performer, and reviewer are all `amy`
(the app authenticates as Amy). `AskDetail::can_review` requires
`principal == reviewer && principal != actor`, so it returns false: the ask
sheet correctly shows "not yours to answer — the reviewer runs: kj ledger
allow <id>" and offers no decision keys, and `kj ledger allow|deny` from the
app is refused for the same reason (self-approval is barred by the
2026-09-12 approval-identity work). The ask is then stuck pending until it
expires. So the app's own shell cannot run any gated command today. The fix
is an identity decision, not app UI: the app should attach as a distinct
performer character (`kj context create --as`, or a dedicated app
character) with Amy as the reviewer, or a non-Amy reviewer must be
configured for the app's context. Coordinate with the approval-identity
work (commits dcb5fc8b, fd27b822). Probe ask `01a096d2-358d-…` was left
pending by this check; it is fail-closed and will expire.

## BRP-injected input lands one request late (2026-09-12)

A key sent with `brp_extras/send_keys`, or a state change through
`world.insert_resources`, takes effect only when the next BRP request
arrives: a screenshot batched with the key shows the frame before it. The
app's winit loop is reactive (100 ms focused, 500 ms unfocused,
`main.rs`), and the injected event evidently does not wake it. Drivers
work around it by following every input with a cheap read. Look at
whether `bevy_brp_extras` should request a redraw after injecting, or
whether the app should run continuous updates while BRP is enabled.

## `kj block list --json` loses its command-specific metadata (2026-09-12)

`BlockCommand::List` declares `--json` and builds a `{context_id, count,
total, blocks}` object, but `KjBuiltin::execute` treats every bare `--json`
as kaish's global output flag and removes it before block dispatch. The
dispatcher therefore receives `json = false`, returns its for-loop block-id
array as `.data`, and kaish renders that array. A live `kj block list
--context approval-identity-probe --json` consequently prints block ids
instead of the declared metadata object. The direct `kj/block.rs` tests call
the dispatcher and do not cover the embedded-kaish path; add that boundary
test when separating global output formatting from command-specific JSON
shapes.

## A streaming turn still writes ~2 MB/s and logs ~160 lines/s (2026-09-11, perf)

The fsync storm is fixed and measured (`docs/devlog.md`, "The kernel that
fsynced every word"): `synchronous = NORMAL` and one transaction per
journaled op took a streamed delta from about 200 KB of block-layer writes
to 13 to 15 KB, with `wchar` equal to `write_bytes`. What remains is
payload and logging, in the order to try:

- The DEBUG `llm` spans log every `TextDelta`/`ThinkingDelta` event, about
  160 lines/s during a deepseek-v4-flash turn, and journald writes each.
  The unit runs `RUST_LOG=info` since 2026-09-11 11:18 EDT (matches
  `contrib/install-systemd.sh`), and INFO is a turn-level log now
  (`docs/devlog.md`, "The kernel that fsynced every word").
- Each delta was about 14 write syscalls and 13 KB: WAL pages for the
  `oplog` row and the `contexts` activity touch. The touch is throttled to
  once a second per context since 2026-09-12 (`ACTIVITY_STAMP_INTERVAL_MS`,
  `block_store.rs`); measure the per-delta figure again on the next bounce.
  Batching deltas into fewer op rows is a design conversation, since the
  op row is the durable unit the change feed replays.
- The "idle" 50–100 KB/s measured today is unverified: the audio
  inventory store is in-memory (`audio_inventory.rs`, no db), and every
  sample so far overlapped either Amy's turns or this session's own hook
  mirror (each Claude Code tool call becomes blocks in the session
  context). Measure again with no client active; the per-thread `io`
  attribution (`/proc/<pid>/task/*/io`, threads named `kjutsu-rpc-<id>`)
  says which connection writes.
- Untested from the diagnosis: `PASSIVE` instead of `TRUNCATE` for
  compaction's checkpoint, and `chattr +C` on a rebuilt db. The 913 MB db
  has 16,147 extents.

## The cached mailbox never re-reads a block it has seen (2026-09-11)

`ConversationMailbox::catch_up` (`llm/mailbox.rs`) folds unseen block ids
and keeps a `seen` set; nothing invalidates an entry when a seen block
changes. The gate-resume fill evicts the cache (`ConversationCache::evict`,
rpc.rs), which covers approvals. Same class, not covered (Kaibo, deepseek
cast, source reading only):

- `kj block edit` / `kj block append` on a block a cached mailbox already
  folded: the next turn reads the old text. The kj verbs live in the
  kernel crate and cannot reach the server's cache.
- `kj stage exclude` on a warm cache: `excluded` is read only at fold time
  (`hydrate.rs`). Narrow, since the documented flow is exclude then fork.
- Overlapping prompts on one context: a second turn that called
  `get_or_create` before the fill holds the old `Arc`, waits on the mutex,
  then folds against the stale `seen` set. Needs two prompts in flight on
  one context; the interactive spawn sites do not check `turn_in_flight`.

One mechanism would cover all three: a change feed the mailbox subscribes
to, or a per-block version the fold compares. Both are design
conversations under `docs/conversation-session.md`.

## Monochrome TUI copy-mode indicators need a contract

With inherited `NO_COLOR=1`, terminal probes lose the marked-row background
and reader inverse styling. Clearing that variable makes the Space/copy-mode
probe pass. Crossterm 0.29.0 formats disabled colors as empty strings inside
`ESC[...m`, yielding an attribute reset that can erase inverse as well as
color. Decide the monochrome presentation and preserve a visible reader and
selection without overriding the user's color preference. The normal
color-dependent PTY fixtures now remove inherited `NO_COLOR`; dedicated
monochrome fixtures can set it explicitly. No upstream posting was made.

## transcript_plan clones every block per frame (2026-09-13)

`render::transcript_plan` clones each block of the current context and
builds a speaker string on every frame; the wrap cache keeps the wrap
itself to a hash and a lookup. Measured 2.4 ms per frame in release over
5,000 blocks (`render::a_warm_frame_over_five_thousand_blocks_costs_a_screenful_not_a_context`,
50 ms tripwire), so no rendered-lines cap was added. If a long context
ever feels slow, stop cloning in the plan pass before adding a cap.

## Arrows cannot scroll from inside a wrapped one-line draft (2026-09-13)

`Up` leaves the tail only from the draft's first visual row and `Down`
is inert only on its last (`run::tail_key`, `Compose::cursor_visual_row`),
so inside a long wrapped draft the arrows belong to the draft. But
modalkit's `Up` is a logical-line motion with nowhere to go on a
one-line draft, so on a long wrapped draft the arrows cannot scroll the
transcript at all; `PageUp` and `Ctrl+A [` still can. The fix is vim's
`gk`/`gj` display-line motion for `Up`/`Down` in the draft, which
`kaijutsu-editor` does not expose yet. Pinned by
`run::a_wrapped_one_line_draft_keeps_its_own_arrows`.

## The open picker's cursor moved under a refresh in a probe (2026-09-13)

`a_placement_verb_moves_the_row_while_the_picker_stays_open` failed in 3
of 8 full-suite runs during slice 2 and never alone. The captured screen
showed the verb landing on the filtered, empty ACTIVE section instead of
the fork's row in RECENT, so no placement ran. The probe now waits for
the `›` cursor to sit on the fork's row before each verb and has been
clean since. The cause is not proven: `PickerModel::refreshed` is meant
to keep the cursor on its context across a rebuild (`follow`), so either
the Tab count computed from an earlier screen was stale by the time the
keys landed, or `follow` loses the row when the filtered section it is
in becomes empty for one round. Reproduce under load with the wait
removed before touching `follow`.

## A pty probe flakes under load: the scrolled place after a switch (2026-09-14)

`a_scrolled_context_comes_back_scrolled_after_a_switch` in
`crates/kaijutsu-tui/tests/terminal_fit.rs` fails its `readout == place`
assertion ("the reader landed somewhere else") when the full pty suite
runs on a loaded box: twice now under the whole suite, and clean every
time alone (3/3 on 2026-09-13, 3/3 on 2026-09-14). The scrolled position
is restored by block and content line, so the suspect is a timing window
between the switch back and the transcript re-anchoring while a late
hydrate delivery is still landing, not a wrong anchor. Reproduce under
load before changing the anchor logic; the probe passes alone, so a fix
that only changes the probe's waits is suspect too.

## The tui takes the kernel-wide firehose (2026-09-10)

`crates/kaijutsu-tui/src/bridge.rs` spawns the actor with
`scope_blocks_to_context: false` on purpose (the tui is a mux), so with
500+ live contexts it receives every block event, including
`report_audio_inventory` bumping a revision every 10 s. Fix is the same as
the app's entry above: `watch_contexts` for the set the mux shows. The
draft and the editor no longer block the key path on a call
(`crate::outbox`, `crate::editor_outbox`), but neither has a per-call
timeout: under an IO stall their queues grow instead of failing. The
"kernel events lost" notices are in the tui's log file (`docs/tui.md`,
"Every way out restores the terminal") since 2026-09-13.

## kaish children inherit the kernel's priority (2026-09-10)

A host command run from a context (`cargo test` in a coder, `git` from
the operator's seat) is spawned by kaish inside `kaijutsu-server.service`
at the kernel's own nice (kaish-kernel 0.17.2 `spawn.rs:205` builds the
`Command` with no priority), so a coder's build competes with the kernel's
own threads head-on, and raising the service's `CPUWeight` raises the
build with it. `docs/operating.md`, "Builds beside a live kernel" carries
what can be done from outside today.

The policy is ours; the seam is kaish's. Kaijutsu never sees the
`Command`, a pre-exec point, or the child pid — the embedder sets only
`allow_external_commands` and `PATH` — so the request for a `nice` field
or a `before_exec` hook on the spawn request is filed in
`~/exomemory/issues/kaish.md`, "A pre-exec seam on the spawn request".
Once it lands, `ExternalExec` in `runtime/embedded_kaish.rs` (the one
exec authority, `runtime/context_shell.rs`) grows the policy, and it should be
pluggable by host rather than a table in kaish: nice is the portable
floor; Linux can add cgroup placement (write the pid into a prepared
cgroup, or `systemd-run --scope` with `CPUWeight`/`IOWeight`) for IO and
memory control nice cannot give; macOS has no cgroup and marks a process
background with `setpriority(PRIO_DARWIN_PROCESS, …)` / `PRIO_DARWIN_BG`,
what `taskpolicy -b` does. Start with nice 10 for exec-granting seats and
measure before adding a second mechanism.

## A queued startup failure records two error blocks

`spawn_admitted_turn` (`runtime/llm_stream.rs`) inserts a pre-stream error
block for identity, review, provider, tool-definition, and system-prompt
failures and returns `Err`; `queue_startup` (`runtime/turn_request.rs`) then
calls `report_failure`, which inserts a second block ("turn failed to run for
this context: ...") under the requester. Both `request_turn` and
`prompt::submit` go through the queue, so one refusal shows twice after the
prompt. Decide which side owns the block; a test that drives `request_turn`
with a missing performer and counts `Status::Error` blocks pins it. Found by
kaibo (DeepSeek) reviewing the performer-first change, 2026-09-22.

## `decision_span_keeps_the_ask_and_deciding_actor_separate` flakes under thread contention

`kj/ledger.rs`, the test installs a custom `tracing::Dispatch` with
`.with_subscriber(...)` for one future; run multi-threaded beside other
ledger and context tests it panicked twice at "decision span", and passed
in isolation and under `--test-threads=1`. Global or thread-local tracing
state races with the other tests' subscribers. Seen 2026-09-22 by a lane
running the kernel suite under host load. 2026-09-29: fails every time with
`cargo test -p kaijutsu-kernel --lib kj::ledger -- --test-threads=4` (3 of 3
on moltar) and passes alone; the layer already answers
`Interest::sometimes`, so callsite interest caching is not the whole story.

## Restart tests stop the server task, not the first kernel

`tests/common/mod.rs` `StateDirServer::stop` aborts the `spawn_local` task
and awaits it, which releases the data-directory lock, but the first
kernel lives on: the editor reconciler thread (`rpc.rs`, spawned in
`ssh.rs` after the kernel) owns an `Arc<ServerRegistry>`, and the beat
scheduler thread owns an `Arc<Kernel>`, so `SharedKernelState::drop`
never runs and the runtime pool, roster loop, SQLite connection, and beat
thread of the first kernel stay live while the second boots over the same
files. The restart tests in `context_label_resolve.rs` and
`context_origin_host.rs` therefore still run two kernels in one process.
A fix takes the kernel handle through `run_on_listener_with_kernel_sink`
and runs the settle steps in `stop()`; the two OS threads still cannot be
joined in-process. Found by kaibo (DeepSeek) reviewing 730a9ed8, 2026-09-22.

## Parentless contexts outside the root consoles

`kj context create` and `kj fork` always name a parent (fe187fe5), but
these paths still insert `forked_from: None` rows: `kj context scratch`
(`kj/context.rs`, `context_scratch`), the handoff log (`kj/handoff.rs`,
already filed under "Handoff logs are parentless"), the beat's score
context (`kaijutsu-server/src/beat.rs`), the cold-start document bootstrap
(`kaijutsu-server/src/rpc.rs`, `bootstrap_discovered_context`), the drift
queue and lost+found (`drift.rs`, `kj/drift.rs`, via
`insert_well_known_context`). Each is its own tree in the forest with no
root character above it, so a reviewer walk from inside one ends at nobody.
Decide per path: parent it under the caller's lineage root, or state that
it is a registry-owned well-known context that never hosts a gated turn.
Found by kaibo (DeepSeek) reviewing fe187fe5, 2026-09-22.

## A `--env KEY=VALUE` argument drops `kj context create` out of its allow tier (2026-09-10)

From an `mcp` seat with `[context_type.mcp] allow = ["kj context create"]`
live, `kj context create x --type director` auto-allowed and
`kj context create x --type director --env KJ_CHARACTER=banto` escalated
(ask `01a08d9a-6514`). A `gate_policy` unit test of the same
statement against the same config returns Allow from the context_type
layer, so the evaluator is not where it drops: look between the broker's
PreCall (`mcp/broker.rs`, `evaluate_planned` over its own plan of the
command) and the hook path that raised the ask, on the running binary
(`04538700`).

## Optional rc reads collapse failures into missing-history text

`assets/defaults/rc/lib/create/S16-handoff.kai` and `S17-predecessor.kai`
guard reads with `|| true`. A missing sheet or predecessor is optional, but a
storage/read failure takes the same path and may be reported as absent history.
The scripts also swallow notification-write failures and then print "injected".
Distinguish legitimate absence from failed reads/writes and report the actual
outcome without making optional memory a prerequisite for context creation.
`context info` also discards character lookup errors with `.ok().flatten()`,
so rc can mistake an unreadable performer for an unassigned context.
The performer-selection tests cover successful handoff injection; failure
classification needs its own fixtures.

## A delegated coder inherits the lead's morning window (2026-09-10)

`coder/create/S15-recall.kai` and `S16-handoff.kai` inject the fleet memory
index and the calling character's last twelve handoff notes into every
coder. A coder driven on a two-file fixture read the lead's note "no ask
expected" and reported the gate asks it met as a possible
regression — a concern the note caused, not the task. The broad loadout
also lists 96 tools in `<situation>`, 45 of them `bevy_brp`. Both are
per-turn cost on a delegated lane that needs neither; whether a delegated
coder should run the same create lifecycle as a character's own seat is a
design question (`docs/delegated-coder.md`, `docs/character.md`), not a
patch to the scripts.

## A failed approval-executed `shell_write` leaves an empty tool result (2026-09-10)

`gate-resume` ran an approved `python -m unittest -v .` (exit 1,
`err_len=1174`) and the resulting block (`2d25fb02#37` in
`rc-review-coder-ds`) reads as an empty `[error]` tool result under
`kj block read`; the model still saw the stderr, so the text rides in the
tool envelope and not the block body. Same family as "A tool result's
shape still depends on whether its body is empty" below, and the tui item
"a ToolResult block must show command OUTPUT" in `signoff.md`.

## Context `--system-prompt` settings are inert (2026-09-10)

`ContextRow.system_prompt` is still stored by `kj context --system-prompt`,
`kj preset`, and one fork copy path. LLM assembly and `kj context prompt` now
use only accepted `Role::System`/`BlockKind::Text` sections plus runtime facts,
so the stored value never reaches a model. Decide and implement removal or an
explicit migration; do not leave a second apparent prompt owner.

## Fork consistency across metadata and document commits (2026-09-10)

Kaibo's `crusoe` review of the prompt changes identified two adjacent fork
issues. Code inspection confirmed the ordering; no DB fault was injected.

- Compact forks capture context type, cast, workspace, and player before the
  model call, while `insert_forked_context` copies environment and bindings
  afterward. The source block-version guard cannot detect concurrent metadata
  changes. Choose one configuration snapshot or reject a change before child
  publication; test a source metadata update during a delayed mock response.
- Fork initialization spans document copy, context/env transaction, cwd override,
  edge insert, and router registration. Failure before the context transaction
  can leave an orphan document; failure afterward can leave a partially
  initialized context. The upfront label check does not cover a label claimed
  concurrently during summarization. Define atomic initialization or cleanup,
  with fault-injection coverage across full, filtered, and compact forks.
  Interrupted-writer cleanup now returns an error naming the copied child;
  its statuses and explanation are atomic, but the earlier document copy
  remains. A fault test pins the caller's refusal and unchanged parent.

`abandon_open_blocks` closes Running/Waiting, leaving Pending untouched. Normal
tool calls start Running, but `kj block status` can set Pending explicitly.
Define what a copied Pending tool call means before adding queued execution to
forks; the model's repaired wire pair does not change that durable status.

## A client can resume into an archived context (2026-09-09)

Sequence from the journal: an MCP session's context was archived by another
process while the session was attached; a second MCP instance created a new
context under the same label; the first session kept working in the archived
one, because archiving does not evict a context from the in-memory registry,
and it re-joined the same archived id after the next bounce. The heal now
registers an archived row as archived, so the re-join no longer collides
with the live holder of the label (`register_with_state`, `drift.rs`). What
remains open is whether `joinContext` on an archived context should redirect
the client to the live holder of its label, or refuse so the MCP re-registers
a fresh context. Joining still permits history inspection; new shell and model
requests now refuse archive at admission. Choose reconnect behavior without
silently retargeting requests intended for a retained context.

## An ask's `exec_source` shows `none` for a positional the executor ran correctly (2026-09-09)

`kj ledger show 01a0864f-a5c1-…` for a hook-origin ask on `kj preset
remove nonexistent-zz` prints `statement: … {"command":"kj preset remove
nonexistent-zz"}` and `exec_source: kj preset remove none`. The journal
shows gate-resume executed `"kj preset remove nonexistent-zz"`, so what
ran is right and only the stored text is wrong. `kaish_kernel::plan_program`
renders the command faithfully (checked in a throwaway test);
`hook_gate.rs`'s `shell_source` stores `command.trim()`; the ledger crate
stores and reads the column verbatim. Where `nonexistent-zz` becomes
`none` is not yet found.

## Character: two hand tasks for Amy (rollout in `docs/character.md`)

The slice rollout itself is `docs/character.md`'s to carry (currently: 0a/1/2
built, 4 shipped 2026-09-06/07, 3/5-8 remain — read the doc, not this entry).
Two operational tasks that doc's mechanism supports but nobody has done:

- Bind a spare key (`kaijutsu-server add-key <pub> --as amy`) as lockout
  insurance.
- Decide what to do with seven characters: `hajime` plays no contexts and is
  safe to retire; consolidating the other six means rebinding keys onto one,
  and retire takes a character's contexts with it.

## The MCP `shell` path applies no size limit to its envelope (2026-09-04)

The in-kernel `shell` result is bounded by the broker's per-instance
`max_result_bytes` (`mcp/broker.rs:1745`). The external stdio path has no
equivalent: `ShellCompletion::to_tool_result` (`kaijutsu-mcp/src/lib.rs`)
hands `CallToolResult::structured` the whole envelope with no truncation, so a
command with 10 MB of stdout ships 10 MB. Whether the MCP path should carry
the same budget — and where it would read one from, having no per-instance
policy — is open.

## A tool result's shape still depends on whether its body is empty (2026-09-04, latent)

`Kernel::call_tool` (`kernel.rs:677`) still substitutes the pretty-printed
`structured` payload when the flattened text body is empty. Correct for a
structured-only tool; a hazard only for a tool that returns both a text body
and a structured payload where the body can be empty. No tool has that
property today (`bindings_builtin`/`hooks_builtin`/`resources_builtin` all
return `ToolContent::Json` unconditionally, re-verified) — latent, not live.
Fix if one ever grows the property: decide the shape from the tool's declared
output, not from emptiness.

## A recovered document's first op races for seq 1 (2026-09-04)

`create_document_with_block` (`block_store.rs:479`) sets `next_journal_seq` to
1 inside the DashMap guard on the fresh-row path — no window. Its recovery
branch (`DuplicateDocument`, line 555-561) cannot: it journals the op after
the guard releases (line 575, `self.journal_op(context_id, payload)?`) with
`next_journal_seq` still 0, since only the `Ok(())` branch (line 549) sets it.
A concurrent writer reaching the document in that window claims seq 1 and the
block-creation op lands behind it at seq 2 — a replay that applies an edit
before the block it edits exists. Fix: reserve the seq at entry-construction
time; `journal_op` uses the pre-reserved number instead of deriving one — a
change to the journaling contract.

## Codex app-server: attaching to a shared daemon over stdio (2026-09-01)

From a sibling session's bridge work, not yet folded into
`docs/codex-app-backend.md`:

- `codex app-server proxy` pipes stdio to a managed daemon's control socket —
  shared-daemon access over plain stdio, so `StdioJsonl`/`JsonlTransport`
  takes it unmodified with no new WebSocket client, and the spawn stays
  outside the protocol module. Only exists for a daemon started as
  `codex app-server daemon start`.
- Our client always calls `thread/start` and only sees the thread it created
  — it cannot attach to a thread a human is driving.
- `codex-codes` (types-only) pulls in nothing past serde/thiserror and models
  `thread/list`, `turn/steer`, `turn/started`, `turn/completed` — a real
  answer to the protocol-churn objection, if the "locally-owned JSON shapes"
  choice is revisited.
- `AdditionalContextEntry.kind` is `untrusted | application` — the right slot
  for peer or tool text entering a Codex turn.
- The installed sidecar unit (`contrib/codex-app-server.service.in`) now
  listens on `unix://$XDG_RUNTIME_DIR/codex-app-server.sock` so the Codex
  TUI can share it. The kernel backend dials `ws://` only, so it cannot
  reach that unit; a unix-socket `JsonlTransport` is the small fix, the
  stdio proxy above the general one.

## Context retention and index eligibility (Amy, 2026-09-18)

Amy: "archive should be good enough for everyone. perhaps we can mark some as
don't-index or something so they don't get picked up by classifiers and so on
building search indices."

Context history and audit records are retained; archiving removes a context
from the active set and preserves its parent edges. Index eligibility is a
separate policy still to implement. `kj search --all` uses active contexts,
but the semantic-index watcher consumes terminal block events through
`BlockStoreSource`, with no context-state lookup. Archiving alone neither
prevents future embedding/classification nor evicts existing vectors.

Design an explicit opt-out with consistent selection for automatic indexing,
manual synthesis/index refresh, classifiers, and search results. Preserve
explicit history reads. Handle existing vectors, in-flight refresh publication,
and later opt-in together; filtering only the watcher leaves other producers
and stale search entries. Do not present archive as a don't-index guarantee.

## Synthesis re-embeds the whole context on every block write (2026-09-01)

**Automatic synthesis remains disabled** (`spawn_index_watcher` receives
`None` for `on_indexed`; keep this title stable for its re-enable condition).
Unchanged manual synthesis now reuses a hash of its exact input and embedding
profile, survives restart, and accepts `--force`. An HTTP embedding service
replaces builtin embedding inference; see `docs/synthesis.md` for its
configuration and contract.

Changed contexts still embed every selected block plus gist/keyword candidates.
Next: per-block vector reuse and coalescing changed refreshes before enabling
automatic synthesis. Manual refreshes currently serialize across the index.
A final snapshot check refuses changes during inference, but publication is
not atomic with later context mutations. Index refresh checks competing
commits, but its supplied snapshot has no authoritative revision; a stale
snapshot submitted after a newer commit can still overwrite it. Both paths
want revision-aware publication through the kernel sequencer.

Selection also needs a separate decision: synthesis preserves its old
non-File/length filter, whereas indexing requires terminal Text/Thinking
blocks. Neither filter explicitly accounts for excluded blocks. Review that
policy before automatic synthesis returns. An empty index projection also
leaves its prior search vector intact; synthesis now clears its own preview,
but search needs an explicit removal policy.

Bulk synthesis also undercounts completed indexing when a context's index
write succeeds but its later synthesis fails: `refresh_synthesis` returns one
error and loses the intermediate `was_indexed` counter. Preserve partial
progress in that result when refining the bulk status output.

## Synthesis publication can race index eviction

With optional `IndexConfig::max_contexts`, an in-flight synthesis refresh can
finish after its context was evicted and repopulate that context's synthesis
rows. `runtime/synthesis.rs::run_synthesis_and_cache` serializes refreshes,
but indexing/eviction does not share its guard. There is also a narrower
window in `SemanticIndex::store_synthesis`: the metadata guard drops before
the memory-cache insert, so eviction can remove the DB row before that insert
reintroduces the memory entry. These storage paths predate the service move.

Make publication conditional on an eviction generation and keep DB/cache
publication ordered with eviction. Preserve intentional synthesis-only caches;
requiring an index entry unconditionally would break them. Add deterministic
race tests covering eviction during inference and between DB/cache publication.
Found in kaibo's synthesis review (GLM-5.3 via Crusoe).

## Embedding service configuration has no `kj` verb

The `embedding_config` singleton is the kernel's sole embedding configuration
source (`llm/db_config.rs::load_embedding_config`), and the kernel ships no
endpoint: a fresh kernel's semantic index is off until an operator writes the
row with `sqlite3` while the kernel is stopped (`docs/synthesis.md`). Give it
an administration path, probably a backend row with an embedding kind so the
endpoint has one owner beside the LLM backends. That changes the registry and
backend administration, so it belongs with the kj verb-class work.

## The terminal client — `kaijutsu-tui` follow-ups (first cut shipped 2026-09-02)

Design: `docs/tui.md`. What the shipped skeleton left open, re-verified still
true:

1. **`inputTokens` on the wire.** `kaijutsu.capnp` still carries only
   `cacheReadTokens`; the status line's `⟳` divides by `contextUsedTokens`
   instead.
2. **Editor wire gaps.** `EditorFlow` (`flows.rs:1624`) still has only
   `StateChanged`/`Closed`, no `Opened` — `editor_open_as` publishes nothing a
   sibling renderer can watch for; the `open_editor` peer invocation is still
   the only open detection.
3. **Picker tails see only whole-block events** — the kernel-wide
   `ServerEvent` stream has no partial-block granularity, shared with the
   app's tails.
4. `theme.toml` → ratatui palette, `bindings.toml` keyed by vim notation (no
   file exists yet at either client), then porting the app to the same file.
5. **Images (additive).** `Abc` needs no new emitter (`engrave::engrave_to_svg`
   is complete); `Svg`/`Image` rasterization is unbuilt.
6. Bar/beat assumes 4/4; the wire carries no time signature.

The seat-digit disagreement between the picker and the status line, and
the ask-card/ledger follow-ups, are their own entries below.

## Telemetry method inventory has not had a full audit

`docs/telemetry.md` no longer lists `push_ops` or the `sync.*` span and
sampling rows, and `get_info`, `interrupt` and `complete` are no longer called
uninstrumented. The rest of the method inventory and its counts have not been
compared with `kaijutsu.capnp` and the `extract_rpc_trace` call sites.

## App draft edits need an ordered submission contract (2026-09-12)

`input/systems.rs::handle_compose_input` spawns each `edit_input` separately,
logs edit failures, and submits the server draft with another task. Local
overlay text can therefore differ from the draft submitted after a failed or
late edit. Visible submit failures do not repair that earlier divergence.
Design batching, acknowledgement, and recovery with the TUI input work;
Amy asked to discuss text input design before changing that contract.

Kaibo's review also found two recovery UX edges. A retained failed submission
can reappear after newer text is submitted and the overlay becomes empty;
it does not overwrite text or submit itself, but needs an explicit recovery
affordance. Text typed with no active or target context has no context to
retain under an identity transition. Define ownership for that unassigned
text alongside the draft-edit contract. Do not solve either case by silently
dropping saved text.

## App bootstrap results need consistent connection scoping (2026-09-12)

Identity replies now carry actor generation and transport epoch. Other
asynchronous bootstrap replies, including context lists/restoration and theme
configuration, still do not. A reply queued before actor replacement can
arrive after the new actor is installed. Audit those consumers together and
discard or refetch results from the old connection. Peer reattachment is
remembered by the actor only after its first `attach_peer` call; a cold-start
failure before that call also needs a complete bootstrap retry.

## Bevy approval review: the surfaces shipped, two gaps left (2026-09-12)

The ask sheet (`ui/ask_sheet.rs`) and the ledger ribbon
(`ui/ledger_ribbon.rs`, `Ctrl+A l`) read `connection::ledger::LedgerMirror`,
which follows `ActorHandle::ledger`, and send decisions through it as
`decide_ask`, with `InputContext::AskSheet` and
`InputContext::LedgerRibbon` taking the keyboard while either is up. What is
still missing:

- **A waiting block still gets a generic status label.** The conversation
  does not say "this turn is waiting on an ask", only the hints line's `!n`
  and the sheet do.
- **Context creation and review assignment have no app UI.** Use
  `kj context create --as` so the performer exists before rc. The connected
  character is the director; reviewer assignment follows explicit delegation
  or the Amy default.
- **The PLAN block renders a flat statement list**, which is all
  the ledger's `AskSummary` carries. A plan tree and per-statement verdicts would
  need kernel fields first; the block is shaped to take them.
- **Neither surface's state is BRP-visible**, so a live check can only read
  `ActiveInputContexts` to tell whether the sheet or the ribbon has the keys.
  `AskSheetState` would need `Reflect` on its `HashSet<String>` and
  `Option<ContextId>`; that plus a way to raise a test ask is what a
  pixel-level check of the sheet needs.

## The tui and the app disagree on a few chords (2026-09-03)

Survey against `docs/input.md`'s prefix table and `docs/tui.md` "Keys".
Direction is on record (`docs/tui.md`, guidance 4: a shared `bindings.toml`
the app inherits) but the file doesn't exist yet, so the specific gaps still
stand:

- **App ahead, not yet in the tui:** `Ctrl+A '` (switch by prompt),
  `Ctrl+A A` (rename), `Ctrl+A q` (close+demote), `Ctrl+A d` (detach). The
  tui names each one's future meaning on the status line.
- **Tui ahead, not claimed in the app's table (no conflict rolling them in):**
  `Ctrl+A [` (copy mode), `Ctrl+A ]` (tui's own yank buffer, not the OS
  clipboard). `Ctrl+A l` is now in both.
- **`Ctrl+Z`** is suspend in the tui, `ToggleSurface` (chat/shell) in the app
  — the tui retired the shell surface for `:!`, the app still has the
  toggle. Name this in `docs/input.md` or retire the app's toggle.

## The /config melt's leftovers (2026-08-29/30)

Resolved: `invalidate_config_file_cache`'s doc comment (`kernel.rs:1875`) now
states the rc-specific rationale directly (a path predicate, not a parameter,
because `kj rc`/`kj config` callers have no session in scope). Only the
`Add` call's redundancy is left undecided — trivial, rediscoverable by
reading `kj/rc.rs`'s `write_path` match.

## Per-client config write-target defaulting has no owner (2026-08-30)

`kj config` is still only `list`/`show`/`reset` (confirmed,
`kj/config.rs`'s `ConfigCommand`) — the old default-to-caller's-own
`/config/client/<id>/<name>` behavior has no successor. Either the file tools
need to reproduce it from the caller's client-id, or the policy is gone and
every per-client write names its full path by hand. Still undecided.

## kaish `ln -s` across two /config mounts still creates a dangling link (2026-08-30)

The mount table rewrites an absolute target on the link's own mount relative
to the link (`vfs/mount.rs`, `symlink`), so the documented same-tree idiom
resolves. A target on another mount — for example,
`ln -s /config/midi/devices/x /config/rc/x/create/S20-device.md` — is stored
as given and resolves to nothing on the host, because a backend does not
know another mount's host directory. Either the mount table resolves both
sides to host paths when both are `LocalBackend`, or the surface fails
loudly on a cross-mount absolute target.

## Remaining approval ergonomics

- Shell/hook asks deliberately have no default expiry. A future janitor must
  identify obsolete asks and unfinished operations, record explicit cleanup,
  and preserve any re-ask linkage; absence of a TTL or sweeper is not itself a
  defect.
- Rotation remains manual. Design how a successor retains the predecessor's
  unfinished operation and ask references without moving authority or
  automatically resuming a different model context.
- A gated `:kj` command still wraps the refusal in an error on some client
  paths; show the durable ask reference as a waiting result consistently.
- Reviewer characters can have several live contexts. The ledger change feed
  informs connected clients; there is no dedicated context mailbox routing or
  automatic wake for a kernel model assigned to review a coder. A directing
  model currently reads the coder's response and uses `kj ledger list|show`.

## kaish cannot plan a backgrounded subshell (2026-09-02)

`( … ) &` has no execution plan, so the gate denies it (S45, "no execution
plan"). This is an ask to the kaish lead.

## A secret source that runs a command has no home yet (2026-08-31)

`env.TOKEN = { command = "..." }` is still rejected —
`a_command_source_is_deferred_with_an_instruction` (`mcp/toml.rs:465`)
confirms the message says "not implemented" and names `file`/`env` as the
alternatives. Deliberate: host exec has one owner, and `external.rs`'s
sanctioned exception is about launching a config-declared server, not running
arbitrary programs for config values. Three ways in, whichever wins keeps the
existing fail-loud contract (never launches blank, never quotes the value into
a log line): route through `EmbeddedKaish`'s exec policy (needs the kaish
runtime up at secret-fetch time); widen the `external.rs` exception
deliberately (fastest, a second bare `Command::new`); or leave it — `pass show
… > ~/.token` is one hop away.

## The `attach` verb fires and no type ships a script (2026-08-28)

Confirmed still true: no `attach/` directory exists under any
`assets/defaults/rc/<type>/` despite `kj/attach.rs` running the lifecycle
(`VERB_ATTACH` wired, `attach.rs:75` rejects on `Err`). Either it earns a
script (re-stating the seat's stance to a model that arrived in a context it
did not boot) or `attach` should retire from `RC_VERBS`.

## Ctrl+Z lands in the wrong input on the second toggle (Amy, 2026-08-26)

Amy: *"sometimes when I hit ctrl-z it goes to conversation input... usually
first time is fine, second time goes to the other input."* No fix found in
the app (`ToggleSurface` still the same shape, `input/systems.rs:105`). Queued
for the app/UI session — start at `ActiveSurface`/`FocusArea` duplication
(128 references across `kaijutsu-app/src/`, still two pieces of state that
must agree and are set in different places).

## Binding review: unfixed items (2026-08-23)

- **A sticky `name_map` may defeat the collision resolver on sequential
  grants.** Not re-verified; no related commit found.
- **Narrowing a capability does not stop running kaish jobs.** The
  `kill_all_for_context` path this was filed against is gone; asynchronous
  shell work runs as kaish jobs with durable receipts. Check what a
  capability revoke does to a running job before deciding. Killing on narrow
  would destroy work.

## The wire drops kaish's output line anchor (2026-08-23)

`OutputNode` (`kaijutsu.capnp:1535`) still has no `line` field, confirmed —
`name`/`entryType`/`text`/`hasText`/`cells`/`children` only. kaish 0.16+'s own
`OutputNode.line` cannot be populated on our wire, so any client reading
structured output loses the anchor. Adding it is a schema change plus all five
artifacts rebuilt — its own decision.

Something now wants it: kaish's coming `edit` and hashline work (Amy,
2026-09-25) marks which file line each output row is and has the kernel hash
it. kaish's draft brief (`~/exomemory/kaish/edit-hashline-brief.md`) sends
the hash as a typed field rather than having clients recompute it, since an
embedder can swap the algorithm. `cat --hashline a b` and `grep --hashline
-r` number each file from 1, so `OutputNode` needs `path`, `line` and
`hash`. `grep -n`, an editor jump-to-match, and the vi
surface are line-anchored already.

## An Error block is shown twice after a fork (2026-08-22)

`llm/hydrate.rs`'s Error-block fold (`:366-410`) still unconditionally
appends the envelope onto a parent `ToolResult` that may already carry the
same message — confirmed no dedup check. Costs tokens on every hydrate after
a fork and teaches nothing the first copy didn't. Fix has to keep the
standalone-error path (when the parent's tool result already flushed) and
skip only the duplicate — a judgment call, not mechanical.

## Approval transfer after operation setup

Initial command/output pairs, shell receipts and an optional ask link now commit
together. Model Waiting results also link their ask in the result acceptance;
failed registration or linkage publishes no partial Waiting pair. Repeated
setup for the same ask reuses the receipt; conflicting source or identity is
rejected before document mutation.

Session pre-call settlement now includes its ask link and Waiting receipt in
one acceptance. Command output fields, ANSI originals/spans, both statuses and
terminal receipt also commit together; failed transactions retain the previous
projection and a captured terminal outcome for restart recovery.
Approval resumes that need a new pair now use atomic pair/receipt/ask setup;
the separate `author_pair_for_ask` writer is deleted. Linking an executable ask
now retains a receipt for its existing pair too. Model Waiting acceptance
commits that receipt with the content, statuses and ask link. Conflicting pairs,
owners, contexts and performers are rejected. Original executable asks retain
receipt lookup through their pair when a result-review ask becomes current.
Close the remaining ownership and delivery handoffs before declaring approval
settlement migrated.
Admission now re-reads pair linkage under the same database guard as context
validation and redemption. A pair linked after the delivery scan keeps its
Session/Turn owner and participates in the performer-change check. Regression
coverage reproduces both stale-scan failures.

The gate now records whether its caller will publish a pair. Waiting publication
releases that handoff atomically with the link and result; bare linkage and
terminal failure do not authorize execution. Driver admission and matching gate
retries honor that ownership. A publication event wakes answers deferred before
the pair existed. Quiet, streaming and direct foreground calls declare no pair.

Terminal publication now retires an unreleased invocation with its result.
Startup recovers captured projections, then retires remaining unpublished asks
without execution. Pending asks become Abandoned; terminal reviewer decisions
remain intact. `kj ledger show` distinguishes publication abandonment from the
answer. Linked pairs settle to Error; retirement faults roll back and retry on
restart.

A live caller that fails without publishing a terminal result still leaves a
hold until restart. Registered operations now recover their original pair and
receipt atomically even when an ask never linked. Output and ANSI provenance
survive; recovery records an interruption without inventing an exit code or
rerunning source. Receiptless model pairs now settle through one atomic
acceptance per context during server startup. The sweep appends the reason to
stored stderr and retains other output; startup refuses approval or block
recovery failures. Historical error receipts
already completed by the old receipt-only sweep are not rewritten by the new
unfinished-operation recovery. Finish that ownership and compatibility audit.
Claimed execution now has durable notification retry. Continue the live-failure
and continuation-admission audit in docs/gate-resume.md,
"Still open".

## The app can stop taking the kernel-wide firehose (2026-08-22)

`ActorHandle::watch_contexts` lets a block-event subscription name a *set* of
contexts rather than one-or-all. The app still sets
`scope_blocks_to_context = false` (`connection/bootstrap.rs:130`, confirmed
unchanged) and takes every context's block events. It could watch exactly the
contexts it renders and re-issue as that set changes. Worth doing only if
event volume shows up in a profile — the firehose is a known cost, not a known
problem.

## Tech-debt audits, 2026-08-20 — what is still open

Full reports: `docs/audits/`. Re-verified against the current tree:

- **`dirty_file_buffers.context_id` is loaded and never used.**
  `get_dirty_file_buffer` and `list_dirty_file_buffers` select it, and
  `kj/swap.rs` reads only the path and the dirtied time. S.
- **The MIDI ear still logs a WARN on every refused capture batch**
  (`kaijutsu-audio-runtime/src/runtime.rs:398`), not once per state change —
  confirmed unchanged. An expected idle state, not a fault. S.
- **Escalate in PostCall/OnError/OnNotification still blocks the path up to
  the gate wait** — whether escalate is meaningful outside PreCall is still
  undecided. M, design.
- **rc softening for interactive seats is still missing** — that a human's
  interactive shell takes the hook path is written down
  (`docs/kaish-integration.md`); the softening itself is not built.
- **The rc bootstrap gate still seeds a tree only when the whole directory is
  empty** (`rpc.rs:2585`, `if dir_is_empty(&host_dir)`), not path-by-path —
  confirmed still true, so a script added to the embedded set after a kernel
  was first seeded still never lands on its own; recovery is still a manual
  `kaijutsu-server rc reseed`. (`ensure_rc_seed_files` itself is install-if-
  absent per path and well tested — `rpc.rs` gaining `#[cfg(test)] mod
  context_bootstrap_tests` this pass is unrelated coverage, not this gap.)
- **`OutputProfile::Internal`** (`runtime/embedded_kaish.rs:127,1282`) is
  still present, still waiting on a kaish spill knob that doesn't remap the
  exit code.

## Older app and broker debt, carried out of auto-memory (2026-09-08)

Re-verified against the tree the same day:

- **Tall blocks lose Y resolution** past `max_texture_dimension_2d`
  (`GpuTextureLimits`, `view/block_render.rs`) — confirmed present. Fix is
  tiled rendering of the visible portion.
- **Role-group borders still draw through Vello**, missed in the Vello→MSDF
  migration.
- **Provider cache expiry is not a hydrate boundary** — a long-idle session
  carries messages the provider no longer has cached; nothing observes it.
- **`ActiveSurface`/`FocusArea`/paired overlay queries** are threaded as
  separate params through compose/interrupt/toggle systems (128 references) —
  a bundle component or resolver would collapse them.

## File buffers: reduce the MCP file tools to kaish (low priority)

Slices 1-3 of `docs/file-buffers.md` shipped. `mcp/servers/file.rs` still
registers `read`, `edit`, `write`, and `glob`; `grep` was removed 2026-10-03
and the coder's person-input facades with it (Amy: "we'll get the final
kaish edit work going tomorrow"). Amy, 2026-08-21: remove
them outright, "It's ok if we don't have them for a short period while we
finish the kaish upgrade." Amy, 2026-09-20: low priority, and "a focused
session where we think through the reduction to kaish."

Bring these to that session:

- kaish emits a `line` anchor under `--json` (kaish `aafc0ee4`), and the wire
  drops it: see "The wire drops kaish's output line anchor".
- kaish is getting an `edit` builtin and hashline anchors. Amy,
  2026-09-25: "kaish gets edit and hashline support throughout." Once they
  ship, the MCP `read` and `edit` tools reduce onto kaish. Decided on the
  kaish side so far (kaish-25, 2026-09-25):
  - An anchor looks like `42:cafe`, with no algorithm name in it.
  - Builtins mark which file line each output row is; the kernel computes
    the hash.
  - The hash algorithm is chosen per embedder through `KernelConfig`. The
    default matches ours (`file_tools/hashline.rs`: FNV-1a, 4 hex,
    `line_hash("alpha") == "202b"`), so anchors carry over.
  - `edit` checks each edit before writing it (compare-and-set) and applies
    a batch all or nothing. It never falls back to replacing the whole file.
  - The CLI grammar is not designed yet.

  The builtin needs our `edit` alias for vi gone first: see "`edit` still
  names two different things on two surfaces". Nothing in kaijutsu depends
  on the MCP `edit` tool's string mode (`old_string`): no rc script, prompt,
  or other code uses it; models find it only through the tool schema.
- Our anchor edit inserts multi-line text as given (`plan_anchor_edit`,
  `format!("{new}{terminator}")`), so a multi-line replacement inside a
  CRLF file gets `\n` between its lines and `\r\n` only at the end. Sent to
  kaish as a design point; not fixed here since the tool is moving.
- `write` has no staleness guard: see "`write` has no staleness guard".
- `docs/file-buffers.md` slice 4 now points at the kaish builtin; rewrite
  it from the session's outcome.

Slice 5 (`swapRecovered`/`diskChangedSinceLoad` on `EditorState`) is also
open; neither field is in `kaijutsu.capnp`.

## File documents should be created lazily, not on every read (2026-08-19, deferred)

Every file the kernel reads still leaves a durable block-store document
forever (`block_id` still not optional in `get_or_load`, confirmed). The
eventual model — a clean buffer stays a `String`, a document materializes only
on first edit, so a document existing *means* unsaved work — is deferred for
cost and cross-referenced from `docs/file-buffers.md` itself ("filed in
docs/issues.md; the row goes away if it lands"). Revisit once swap semantics
are proven.

## The swap marker and the content it marks are two writes (2026-08-19)

`record_dirty_file_buffer` (`kernel_db.rs:6337`, one `INSERT ... ON
CONFLICT`) and `edit_text`'s oplog journal write are still separate
statements, not one transaction — confirmed. A crash between them either loses
the marker (cold path reconciles the unsaved work away) or leaves a marker
pointing at content never written.

## `edit` still names two different things on two surfaces (2026-08-18)

Confirmed unchanged: kaish builtin `edit <path>` (`runtime/context_shell.rs:171`,
registered alongside `vi`) opens an interactive vi session; MCP tool `edit`
(`mcp/servers/file.rs:163`) is a surgical, non-interactive hashline/string
edit. Same name, same coder, opposite mechanism.

kaish will ship its own `edit` builtin (Amy, 2026-09-25: "kaish gets edit
and hashline support throughout"), and our alias would shadow it. Dropping
the alias is now a prerequisite for taking that kaish release, not an option.
`vi` is already the documented front door (`docs/vi.md`). The alias is also
named in `runtime/embedded_kaish.rs:400` (read-only denial),
`runtime/vi_builtin.rs:180` (test registration), and the front-door test at
`runtime/context_shell.rs:505`. Check rc scripts and help text for callers
first.

## `write` has no staleness guard (2026-08-18)

`write_file` (`mcp/servers/file.rs:512`) still calls `cache.create_or_replace`
with no precondition, then `flush_one` — never `flush_one_guarded` — so the
W12 disk-moved-under-you guard that protects `:w` still does not apply to
`write`. Whether an existing-file `write` should require a
generation/hash precondition or explicit overwrite intent (while a new path
stays a plain create) is still undecided. This is the asymmetry that let a
stale-context overwrite drop 115 backlog entries from this file on
2026-06-29 (recovered from `3f8b54d3`) while `edit`'s hashline mode would have
refused.

## Tool-pair atomicity at insert time remains unbuilt

Conversation snapshots repair pairing, and `Provider::stream` refuses an
invalid message sequence before backend dispatch. Neither holds unrelated
writers while a tool call is open. Durable blocks can still interleave,
and hydration preserves the existing interruption policy: synthesize an
error for a missing adjacent result and drop late results with warnings.
A writer-side queue needs a separate design with drift and peer tool calls
as concrete consumers. See `docs/conversation-session.md`, "Out of scope
for Slice A". `AGENTS.md` already describes the absence of this queue.

## Duplicate tool calls can execute before the next send refuses them

`process_llm_stream` collects `StreamEvent::ToolUse` events in a vector but
indexes their durable block ids by tool-use id. Repeated ids overwrite that
index and both calls still reach concurrent dispatch. The provider pairing
check refuses the next request, after those tools have run. Validate the
incoming call batch before execution; the send-time check cannot prevent
those duplicate side effects. The hydration lane's live-loop refusal test
uses two calls to an unknown tool to exercise this safely.

## Flaky: `test_ordering_stress_100_bisections` put a Middle block first, once (2026-08-17)

`test_ordering_stress_100_bisections`
(`crates/kaijutsu-kernel/src/blocks/block_store.rs:2796`) asserts only
`blocks_ordered()[0].content == "First"` — it still checks the sorted
output, not the generated order keys, so a rare ordering inversion (seen
once: `Middle-5` sorted ahead of `First`) would fail the same way again
without naming the mechanism. Cause unknown: not reproducible on demand
(8/8 and 10/10 clean reruns), and not touched by `89d90ccc`'s `merge_ops`
work. Fix the test to assert on order keys, not the sort result, so a
recurrence names order-key precision or tie-break rather than a content
string.

Recurred during the September 17 effective-environment validation: `Middle-9`
sorted before `First` (3117 passed, one failed,6 ignored). The change does not
edit block ordering. Evidence: `/tmp/kaijutsu-effective-env-kernel-final.log`.
Keep this occurrence with the existing ordering audit; a passing rerun does not
explain it.

---

## A `;` chain reports only its last command's status

`is_error` for a `;`-separated chain is the last command's exit status
(`env.is_error()`, `runtime/command_result.rs`). A chain whose last command
fails reports `Error:` when every earlier command succeeded, and a passing
last command hides an earlier failure. Decide what a chain's status means
before changing it.

---

## The file write/edit tools are not gated by the approval ledger (Amy, 2026-08-16)

`builtin.file:write`/`:edit` still route as plain capability tokens, not
through `approval_ledger` — confirmed still true and unsolved. Not a security boundary (every player is
already inside the trust boundary); the ask is an ergonomic nudge so a
large destructive edit is visible and undoable rather than only
forensically reconstructable afterwards. A cheap partial worth keeping on
the table: gate on a size-delta threshold (N lines or X% of a file)
rather than every write.

A seat rewrote its own gate through them (2026-10-05). In a bump-only
benchmark run every command bumped on a threshold identity mismatch, and the
bump message then carried the miss cause, "confirm or refit one in
gate.toml". DeepSeek found `/config` writable through the file tools, backed
up `/config/kernel/gate.toml`, rewrote the engine hash to the live server's,
and went on. The cause no longer reaches the seat (2b403892), but
a coder can still write `/config/kernel` and `/config/rc`, its own gate and
instructions. Open: whether a worker loadout's file tools should exclude
`/config`, or the bench should mount it read-only for the seat.

---

## `docs/architecture/` needs re-certification, and two diagrams are missing (2026-08-16)

The two deleted diagrams (`01-system-topology.svg`, `06-crate-deps.svg`)
are now tracked in `docs/architecture/diagrams/README.md` itself — that
doc is canonical for the diagram gap, not this file.

Still open, not in any docs/*.md: `docs/architecture/README.md`,
`foundation.md`, and `client.md` were swept for vocabulary and for the
deleted last-write-wins machinery, but not re-verified line-by-line
against current code the way the 2026-06-16 sweep did originally — treat
as improved, not re-certified. And: `test_task_status_lww_tiebreak_order`
(`kaijutsu-types/src/block.rs:5566`, confirmed present) is the only thing
still pinning `TaskStatus` LWW order — decide whether that order needs
pinning at all now that concurrent merge into a kernel document is
structurally impossible.

## vi input editor stopped repainting after a small in-place edit (found 2026-08-16, live on moltar)

Amy was editing a typo ("rost" → "rest") in the compose-block vi input.
`i e <Esc>` (insert `e`, leave insert mode) changed the buffer but the
on-screen render did not update; `dw` + retype (a full replace) rendered
correctly. Not root-caused; likely a missed redraw/dirty flag on the
insert-then-escape path rather than a buffer-state bug. Vi input handling
lives in `crates/kaijutsu-app/src/input/vim/` (`mod.rs`, `dispatch.rs`) —
no dirty/repaint marker found there by name, so check whatever marks the
input view dirty against the insert-mode commit path.

---

## The roster index has no kernel-now reference, so client-rendered ages mix two clocks (2026-08-16)

Still true: `/run/roster/index` carries each row's `recorded_at` on the
kernel's clock (`kaijutsu-kernel/src/roster.rs`) but no kernel-now value,
so a client renders age by subtracting the kernel's stamp from its own
clock. Accepted for now (NTP-disciplined LAN, skew below display
resolution); wrong the moment a client's clock isn't disciplined. Fix:
add a kernel-now value to the index, or give `FileAttr` a `generation`
(see the entry below) so a client can conditional-fetch instead.

---

## Drift peer origins are stageable but not deliverable, and the wire can't name one (2026-08-17)

This, "Ambient command center" and "Seats-at-the-table follow-ups" all wait on
one missing piece: the wire cannot tie a principal to a peer. Build that once,
for the first consumer that needs it.

Still accurate and still unreachable in production (only tests construct
`DriftOrigin::Peer`) — confirmed at
`kaijutsu-server/src/rpc.rs:10839-10866`, `origin_ctx_bytes` reports a
peer origin as absent because the wire's `sourceCtx @1 :Data` means "a
ContextId" and nothing else. Durable, never lost (drains to dead-letter
like any delivery failure), just can't be delivered or displayed.
**Whichever lane adds the first real peer-origin producer (the cc inbox,
`docs/drifting-dead-letters.md` slice 4) must give the wire an honest
origin representation in the same change** — appending origin fields is
ordinal-safe, do it then, not before.

## The rc lifecycle shell has a narrower tool set than the interactive one (2026-08-22)

An rc `create` script calling `fmt` fails with `command not found: fmt`,
while the same command in the MCP `shell` tool succeeds — a `.kai`
verified interactively can still fail at context create. Exec authority
gates on the context's binding holding `Capability::Exec`
(`runtime/context_shell.rs`), so this is plausibly an ordering artifact of
when a create-time binding takes effect, not yet confirmed either way.

Two consequences. Verify rc scripts by *creating a context*, not by
running the command in a shell. And a create-path script under `set -e`
should degrade rather than abort: a failed helper takes down the whole
context create, and a context with no stance is worse than a stance that
reads a little ragged.

## `connection/drift.rs` still reads block events off the kernel-wide stream

Still true, two sites: `connection::drift`'s
`ServerEvent::BlockInserted { kind: Drift }` detector
(`kaijutsu-app/src/connection/drift.rs:181`), and
`time_well::live::ingest_live_events`'s `ContextTails` build
(`live.rs:289`) — both read the kernel-wide stream rather than the
per-context change feed, deliberately kept because the feed can't serve
contexts nobody follows. Decide the scope question (does drift into an
unfollowed context deserve a notification?) once for both sites, not per
site, before moving either.

## Model names via hooks — the plumbing exists, the data mostly does not arrive (2026-08-15, Amy)

Plumbing is built (`HookEvent::model: Option<String>`,
`kaijutsu-mcp/src/hook_types.rs:30`; set on `SessionStart` in
`hook_listener.rs`) — the gap is source data, and it's three separate
problems: Claude Code's hook payload has no model field at all (would
need `transcript_path` JSONL sniffing); Crush/qwen doesn't send it either
(unconfirmed what it does send); `SessionStart`-only is stale the moment
`/model` changes mid-session. Do the per-source capability survey (who
sends it, what's the honest fallback) before writing more code — and if a
source can't supply it, the roster should say "unreported", not render as
unconfigured.

---

## `/v/docs` block filenames do not sort into document order (2026-08-15, Amy)

Still true: the `/v/docs` backend lists block filenames as
`BlockId::to_key()`
(`kaijutsu-kernel/src/runtime/kaish_backend.rs:443`), the
`<ctx>_<principal>_<seq>` string, so `ls` sorts principal-major and seq as
text (`_2` after `_15`). `order_key`
(`kaijutsu-kernel/src/blocks/content.rs`) is already a base-62
lexicographic fractional index built for exactly this — rename to
`<order_key>__<short_block_id>` and `ls` sorts correctly with no kaish
change. Unblocks the sketched netrw-style subscription view Amy wants;
open question is whether `order_key` (which moves when a block moves) is
stable enough for whatever consumes the name.

---

## Principal plumbing — a holistic sweep, not a per-lane patch (2026-08-15, Amy)

Not started (no `PrincipalSweep` or equivalent found). Ruling stands:
this does NOT gate the git-auto-commit work — that ships with
service-authored commits and principal fidelity retrofits later. When it
happens, survey first: inventory every mutation path that reaches durable
state (VFS/config writes, rc edits, block mutations, MCP tool calls,
kaish builtins, drift, `kj` verbs) and classify what each knows about its
actor — real `Principal`, synthetic/service, inferred, or genuinely none
(legitimate for kernel-internal timers). Not an authorization mechanism:
attribution and recovery only (`docs/instrument-design.md`, "Many hands,
one trust boundary").

---

## `poll_connection_status` has an arm that cannot fire (2026-09-21)

`poll_connection_status` (`kaijutsu-app/src/connection/actor_plugin.rs`) removes
the `RpcActor` resource when the status broadcast reports `Closed`. The receiver
comes from `actor.handle.subscribe_status()`, and `ActorHandle` holds its own
`status_tx`, so the channel stays open for as long as the resource exists. A
panicked actor now publishes `Terminal` through `supervise_actor`
(`kaijutsu-client/src/actor.rs`) and is not respawned; the app shows it as
`last_error`. Delete the arm's body, or decide what removes a `Terminal`
actor's resource.

---

## Managing roots — the concept kaijutsu is missing (Amy, 2026-08-15)

Settled design: a forest of one-parent trees; drift is a separate,
deliberately cyclic overlay, never part of structure
(`KernelDb::insert_edge`'s cycle check already applies only to
`EdgeKind::Structural` — confirmed). Archive is one row, never a cascade,
and archived contexts keep their label out of the live index — both
**shipped 2026-09-05**. Anchors are seats, not workspaces (fork copies
history by default, so an unused anchor is cheap and a used one taxes
every descendant forever).

**Not built**: the `anchored_at` column (slice 2 — parentless by
construction, never swept by age; no such column exists yet), so there is
still no `--detached` flag on `kj context create` and no enforcement of
"anchors stay unused." Recommendation stands: build slice 2 (the anchor
bit) before slice 3 (enforcement) or slice 4 (`kj root` verbs). Slice 1's
guardrail is in: `context_move` is one transaction, so a refused move no
longer orphans the context.

---

## Live roster — push-on-attach is the remaining unwired half (2026-08-14)

Slices 1-4 and periodic refresh shipped (`crates/kaijutsu-kernel/src/roster.rs`,
`roster_sources.rs`, `kj/roster.rs`, `vfs/backends/roster.rs`,
`tests/roster_refresh_boot.rs`). Still open:

- **Push-based refresh on peer attach/detach isn't wired.**
  `roster_sources.rs:43` still names this a TODO —
  `kaijutsu-server`'s RPC attach/detach handlers should call `refresh_once`
  (or a narrower single-peer reconcile) instead of waiting for the ~10s pull
  tick. `SharedKernelState::roster` already holds the handle needed.
- `RECENT_LIVE_WINDOW_MS` (`roster_sources.rs:73`, 15 minutes) is an untuned
  v1 guess — a config knob if it needs adjusting.

## Theme changes never reach a running app — there is no live config push (2026-08-13)

Still true, verified: `ThemeReceived` (`actor_plugin.rs:551`) has exactly one
send site, the connect-time bootstrap fetch, and no `ServerEvent`
config/theme variant exists. `kj config set` on the theme file updates the
kernel document and nothing tells a running app — a next-connect console,
not the live one `docs/color.md` sells.

Remaining work: a config-changed server event (or subscription) that
re-fires `ThemeReceived` on theme writes; the app-side repaint already works
once `Theme` is replaced. Also open: `ThemeData` (the TOML wire format) has
no fields for block text colors (`block_assistant`/`block_thinking`/…; user
text follows `fg`), so no
theme file can change conversation text colors; `Theme` derives neither
`Reflect` nor registers with BRP, so it cannot be poked remotely for testing.

## Dock RTT sizes skip physical-px rounding (2026-08-12, kaibo find)

Still open, verified: `render_north_dock`/`render_south_dock` still stamp
`rtt.built_width = logical.x` raw (`ui/dock.rs:585,844`), while block cells
round via `round_to_physical_px` (`view/block_render.rs:318`). At fractional
DPI that makes `msdf_item_scale` a hair off exact, giving sub-pixel glyph
drift on the dock. Cosmetic, pre-existing; fold into the tier-2 "unify RTT
resize" cleanup.

---

## Summaries drift stronger than what they summarise (2026-08-11)

Writing failure mode, not a code bug: a summary reads stronger — or, in two
recorded cases, weaker — than what was actually pinned, and the next reader
inherits the drifted version without re-deriving it.

**Still open — Amy's call, never ruled on:** should a claim about what is
*proven* carry the assertion name or a `file:line`, so the next reader can
tell pinned from observed at a glance? Not adopted anywhere in CLAUDE.md or
docs/ as of this sweep.

---

## MCP 2026-07-28 adoption — two slices shipped, three items open (2026-08-11)

Claude Code cannot list kaijutsu-mcp's tools on rmcp 3.1.2 (2026-09-22).
Its MCP log: the version probe hard-closes the server ("rmcp-class pre-init
hard close; respawning pinned legacy"); the respawn negotiates 2025-11-25;
then tools, resources, and prompts lists fail with "request _meta is missing
or has malformed required fields: io.modelcontextprotocol/protocolVersion,
io.modelcontextprotocol/clientCapabilities". A hand-driven stdio
`initialize` + `tools/list` succeeds. rmcp 3.2.0 lists "keep initialize on
legacy protocol versions (#1228)"; 3.4.0 is current. Amy: "let's plan to get
on latest rmcp" and "fully adopt the latest spec where we can". Plan:

1. Capture Claude Code's exact stdio traffic (a tee wrapper around the
   binary) and pin it as a failing kaijutsu-mcp test: probe, then legacy
   `initialize`, then each list call.
2. `cargo update -p rmcp` to 3.4.0; fix the build in `kaijutsu-mcp` and the
   kernel broker (`mcp/servers/external.rs`); re-read both hand-pinned
   `V_2026_07_28` comments against the new `KNOWN_VERSIONS`. The test goes
   green; `/mcp` lists tools in a real Claude Code session.
3. Serve the 2026-07-28 inline lifecycle (SEP-2575 `server/discover`,
   per-request `_meta`) instead of forcing clients to the legacy respawn.
   Consider 3.3.0's `ServerHandler::negotiate_initialize`.
4. The open items below, in order of payoff: `structuredContent` +
   `outputSchema` on `shell`; tasks (SEP-2663) for long calls; reconnect;
   elicitation 1b. Drop deprecated roots/logging once kaibo and bevy_brp no
   longer need them.

Shipped since filing: **elicitation slice 1a** — `create_elicitation`
(`mcp/servers/external.rs:200`) now emits `ServerNotification::Elicitation`
instead of rmcp's silent auto-decline. **`on_progress`** (`external.rs:167`)
is no longer a no-op — it forwards `ServerNotification::Progress`.

Still open:
- **Elicitation slice 1b** — nothing yet *answers* an elicitation
  (human/sibling-context routing, timeout policy). Design pass, not a patch.
- **`structuredContent`/`outputSchema` on `shell`** — `ShellCompletion::to_json`
  still hand-rolls its envelope into a `TextContent` string; no
  `Tool::with_output_schema` usage found in `mcp/servers/shell.rs`.
- **Tasks (SEP-2663)** for outbound long calls — not built; `enable_tasks`
  doesn't appear anywhere.
- **No automatic reconnect** — `reconnect()` (`external.rs:468`) still has no
  caller; a dead server stays Down until `kj mcp reload`.

## Ambient command center — trace packets, switchboard follow-ups (2026-08-10)

Still unbuilt: the trace-packet/comet system (concepted, not built — no
`RouteRegistry`/mode-2 pulse slots in `TraceGlowMaterial`); switchboard
placement (south wall still sits behind the default room camera); switchboard
polish (recency dynamic range still narrow); an ambience agent (idea only).

Verified changed since filing:
- **Switchboard slow leak — now intentional, not a bug.**
  `SwitchboardState::retain_relevant` (`view/room/switchboard.rs:333`) keeps
  an off-roster context's sticky ember on purpose, pinned by
  `retain_relevant_keeps_offroster_embers_and_drops_idle_signals`.
- **Turn-comet/seat-flare enabler** (principal↔peer correlation) is still
  missing — same open item as "Seats-at-the-table" below; don't duplicate
  the fix.
- Runner still runs `cargo watch` (`contrib/kaijutsu-runner.sh`), not
  watchexec — moltar-deaf status unverified this sweep.

---

## Peer-registry doctrine (2026-08-10, peers-plumbing)

**Resolved since filing:** the nick-goes-stale-on-relabel bug is fixed —
`PeerRegistry::attach` now keys on `instance` alone when present
(`peers.rs:45` `peer_key`), so a re-join under a stabilized label replaces
the old entry instead of duplicating it.

Still open: the wire `PeerInfo` struct (`kaijutsu.capnp:1622`) still carries
only `nick`+`attachedAt` — no `instance`/kind field — even though
`PeerConfig` gained `instance` (`:1617`). `listPeers` still can't
distinguish two peers sharing a nick from each other.

---

## Seats-at-the-table follow-ups: nameplate LOD, turn-event flare (2026-08-10, seats)

Both still open:
- **LOD-gated nameplate on well-zoom** — needs a nearest/under-cursor wisp
  pick, not built (`view/room/seats.rs:8` still points back to this file).
- **Turn-event flare needs principal↔peer correlation** —
  `ServerEvent::TurnCompleted` still carries only `principal_id`, no peer
  nick/instance; no correlation exists in kernel or app (grepped, no hits).
  Same enabler needed by "Ambient command center" above — don't duplicate.

---

## FlowBus backpressure — what the 2026-08-05 rework left open

Per-subscription bounded queues, `subSeq` and the lag kick shipped. Open:

- **No catch-up for a subscriber that was not there.** `flows.rs` has no
  catch-up path; losslessness is a promise to live subscribers only.
- **Only `slowSubscriber` is ever sent.** `serverShutdown`/`superseded`
  exist on the wire (`kaijutsu.capnp:774`) but nothing emits them
  (`context_feed.rs:286`, `rpc.rs:11425`), so a clean shutdown looks like an
  ordinary disconnect.
- The ACP adapter's defensive sweeps (`kaijutsu-acp/src/session.rs:495`,
  `update.rs:1586`) are dormant defence-in-depth; remove once real flights
  show them firing zero times.

## Approval paths: the burn-down (2026-09-29)

Map and reasoning: https://claude.ai/artifact/QzzA5UzbszV8y67sw3S3XB
("Kaijutsu Gate Paths"). Three evaluators of different depth decide, and
two consumers act on an answer with different checks. Amy: "let's record
and burn down all the approval options". Delete each line as it ships.

- **F5** File and block tools pass no gate tier; a file write to
  `/config/rc` does what ask-tier `kj rc add` does.
- **F6** On the RPC shell paths an uncovered statement asks unless the
  actor is a live root character. kaijutsu-mcp authenticates with Amy's
  agent key, so Claude Code there acts as Amy and runs uncovered statements
  without an ask. Closing it needs the bridge identity
  (`docs/character.md`, "The bridge identity: a key per model character")
  and a `[context_type.mcp]` allow tier wide enough for daily reads.
- **F8** PreCall denies write no row; tier asks record origin `hook`; RPC
  auto-allows leave no row; the ORIGIN column is narrower than `shell_gate`.
- **F11** Approving a streaming RPC ask authors a Model-role pair and can
  wake the model (inferred).
- **F12** Digest `--remember` rules do not cross paths (`hook:v1:` vs
  `shell-stmt:v1:`).
- **F9 (kept open)** A program the planner rejects proceeds on the RPC
  shell paths to kaish, which reports the parse error in its own block.
  Safe while the planner and the executor share kaish's parser; a
  divergence (see "A kaish lexer rejection degrades the gate") reopens it.
- **F13** `kj cc send` through `shell_write` asks twice, unlinked.
- **F15** The gate reads kaish's `PlannedValue::Plain`, the literal as it
  renders on a command line, as an argv value (`kj/readonly.rs`,
  `resolved_kj_args`; `gate_policy.rs`, `command_keys`). Structured `kj`
  quotes every argument, and kaish renders a quoted number with its quotes,
  so `kj wait --timeout "0"` classifies as `'0'` and fails. A config allow
  cannot cover it and the read-only exemption misses it, so a model's
  structured `kj … --tail 5` asks. Needs the literal value from kaish, a
  shared interface.
- **The held turn's leftovers (2026-09-30).** A model's turn now holds on
  its own ask (`docs/gate-resume.md`, "The turn holds"). Still open:
  - The seed and wake code for an unheld `PairOwner::Turn` pair
    (`runtime/approval_resume.rs`: `unrun_turn_seed`,
    `needs_no_shell_turn_seed`, the `Tell` arms, and the "Try the same
    call again" wake) now serves only the MCP `shell_write` path's authored
    pair, `kj cc send`, and a turn that vanished without its hold. Rename
    the authored pair's owner so it is not mistaken for a held turn, and
    delete what no caller reaches.
  - `kj cc send` stores no command and refuses in prose (`kj/cc.rs`), so it
    keeps retry-as-delivery and step 2b in `run_gate`. Give it an
    `exec_source` and a typed refusal.
  - ACP permission prompts show only the ask's description (a 200-character
    prefix, or a hook's stderr), not the command asked about.
- **One shell per seat (Amy, 2026-09-30).** "I've watched the models fumble
  the 2 tools ... contexts that have shell that is RO and those that have
  RW, but not put both shells and make the model decide." A seat binds a
  read-only or a read-write shell, never both. After the Terminal-Bench
  baseline, so its effect is measurable.
- **Posture direction (Amy, 2026-09-30).** Ship a constrained, efficient
  setup: every tool call asks until a human `--remember`s it, and loosening
  is always the user's explicit choice. A banto:coder swarm states at
  launch whether a seat gets a shell. Smooth that choice rather than widen
  the default. The better it works, the sooner autonomous loops start.
  Amy is also considering one shell tool for coder, with a name that
  tokenizes clearly for most models, in place of the `shell`/`shell_write`
  pair. The double ask (a model's shell running `shell_write` asks for the
  statement, then in the tool's own gate) is left as is. The shipped
  `gate.toml` carries the per-seat allow as a commented example.
- **An uncollected answer with no stored command stays redeemable.** A
  `kj` verb or non-shell hook ask is spent by its caller's retry
  (`kj/gate.rs`, `run_gate` steps 2 and 2b). If the woken caller never
  retries the exact call, the answer authorizes that call indefinitely;
  after a restart `approval_resume::start` marks the backlog woken and
  never tells the caller again. A `kj ledger forget` does not reach such an
  answer. Bounded to the exact statements, label, context, principal, and
  actor. Needs a decision: expire it, or have the worker spend it when the
  caller moves on.
- **`decision_span_keeps_the_ask_and_deciding_actor_separate` is flaky**
  (`kj/ledger.rs`): it passes alone and fails at `expect("decision span")`
  when run with the rest of `kj::ledger` (`--test-threads=4`, reproduced
  twice on a953a6ab). Probably a tracing callsite-interest race
  under `with_subscriber`.
- **Tests** The ACP part of the conformance matrix ships in
  `crates/kaijutsu-acp-fleet/fleet/approval/` (`docs/acp-fleet.md`, "The
  approval matrix"); scenarios marked `known_gap` name F8.
  The ACP bridge offers "always allow" and "always deny", so rule scenarios
  run through ACP. Paths C and D need a harness of their own. A rule added
  between an ask and its answer does not reach that approval: Amy, "the
  policy at the time the command was first evaluated should cover its
  lifetime"; `e-rule-added-after-ask-leaves-it-alone.toml` holds it. A
  model's turn holds on its ask, so that scenario and
  `d-shell-write-runs-what-was-shown.toml` change state between ask and
  answer through a sibling call in the same reply.

## What a replacement risk scorer inherits (2026-09-28)

The risk classifier and its rc hook are removed (`docs/devlog.md`, "The
classifier that did not come back"). A replacement arrives as an rc pre_call
hook (`docs/gate-policy-tuning.md`, "Verdicts"). These gaps outlived the old
one:

- **`kj ledger list` shows a gate-policy ask's ORIGIN as `hook`.** The
  RPC-path tier ask (`Broker::ask_tier_ask`) opens through
  `run_permission_ask`, which records a hook origin; the description names
  the gate policy. Seen live on moltar, 2026-09-28.
- **The shell gate's ask does not name the tier key.** On the `shell_write`
  tool path an ask-tier statement's ask is titled `shell_write: 1
  statement(s) — <command>`. The RPC shell paths name the layer and key
  (`describe_asks_planned`); `run_gate` could use the same text. Until then
  the approver cannot tell a configured ask from an uncovered one there
  without `kj ledger rules`.
- **`clause` drops redirects.** `KJ_TOOL_PLAN`'s `commands[].clause`
  (`kj/plan_clauses.rs`) excludes redirects, so a scorer reading it sees
  `kj block list` for `kj block list > ~/.bashrc`. Score
  `KJ_TOOL_ARGS.command` whole, or append redirects to the clause.
- **Variables reach a scorer unexpanded.** `chmod -R 777 ${DIR}` scores as a
  guess. `KJ_TOOL_PLAN.env` carries the values; parse-time substitution with
  a supplied map would be a kaish request.
- **A reformulated command does not carry its pending ask forward.** A
  `retry-after-ask` ledger row is the minimum (a measurement, not a control).
- **A kaish lexer rejection degrades the gate to the no-plan fallback.**
  `contrib/kai-parse-check.sh` guards our own corpus; the lexer bug is kaish's.
- **`plan_clauses::render_clauses` has no caller outside its tests.** Delete
  it or give the next scorer a reason to call it.

## kaish `env` takes its command's flags as its own (2026-09-21, kaish)

`env A=1 make -C dir app` fails: `env: error: unexpected argument '-C'
found`. `EnvArgs::args` in kaish's `tools/builtin/env.rs` (pinned rev
ad293823) is not `trailing_var_arg`, so clap parses every hyphen word after
the command as an env flag. Parsing should stop at the first word that is
neither an env flag nor `VAR=value`. The binder may already have split
`-C` into `flags` before `to_argv`, so the fix may need that layer too.
Workaround: `export` the variables, `cd`, then run the command. Belongs to
the kaish lead.

## A background read-only shell refusal carries no `shell_write` hint (2026-09-27)

`name_the_write_path` (`runtime/tool_command.rs`) appends the hint only to a
foreground result. A `run_in_background` read-only call settles through
`run_into_blocks` and its completion notice without it.

## From the kaibo review of the edit, hint, and worker changes (2026-09-27)

deepseek, over 9ae1f930, 830a068a, 523a6741, cbd44c34. Fixed in the
follow-up commit: `edit` over a dirty buffer the disk moved under, and
`--tail 0`. Open:

- **The read-only hint names `shell_write` to a seat without it.** A
  `toolie` binds only `facade:shell`. The read-only description says the
  same thing unconditionally. Name the write path only when the seat's
  roster has it. The toolie stance tells its model the seat does not hold
  `shell_write`; the tool description and refusals still name it.
- **A redirect refused by a read-only shell says only `permission
  denied`.** `ReadOnlyFs` refuses with "read-only shell (no writes)", but
  kaish's redirect error keeps only the error kind, so a toolie, or the
  person's box in a toolie seat, cannot tell the refusal from a host
  permission. The toolie stance teaches `permission denied`; the
  `the_toolie_stance_examples_run_in_its_read_only_shell` test pins it.
  Carry the message through in kaish, or name the read-only shell in
  `name_the_write_path` (`runtime/tool_command.rs`).
- **The toolie has no project file map or house rules.** kaibo's explorer
  receives an orientation map (each file with its size, marking files too
  large to read whole) and the operator's `[context]` files after its
  preamble. The toolie gets neither, so it learns sizes with `wc -l` and
  project conventions only by reading them.
- **No test covers a failure on one pool thread with work queued on
  another.** The factory-panic drain test is one-thread by design now.
  Block every thread of a three-thread pool, queue the failing factory and a
  sibling on different threads, release, and assert the sibling settles
  with the stop observed.

## ACP fleet: what stays open (2026-09-27)

The fleet (`docs/acp-fleet.md`) runs Harbor-shaped scenarios against
`kaijutsu-solo-acp` with the scripted mock model: host scenarios test that
the gate and pre_call hooks hold, and contained ones run a permissive ("yolo")
kernel in podman with no network and only the workspace writable. Amy:
"some of those acp sessions can use modified rc too, maybe a more yolo mode
for when it's contained in docker". Open:

- **A hook never lowers an ask by its exit status** (Amy, 2026-09-30):
  "a lower should be spelled out and almost impossible to do by accident."
  Hooks moved system prompts to `kj` so a hook could say more than an exit
  code. A lowering, if it comes, is an explicit, named verdict. It is never
  exit 0.
- **The bridge extends ACP along the v2 draft's shape** (Amy: "yes you can
  extend the ACP"). The prior art, read 2026-09-30, is cloned in
  ~/src/research: `agent-client-protocol` at `schema-v2.0.0-alpha.6`,
  `claude-agent-acp` v0.84.0, `codex-acp`, and `harbor`. We ship
  `agent-client-protocol` 2.0.0 and speak `schema::v1`. Namespace every
  extension under `_meta.kaijutsu` or a `_kaijutsu/` method, advertise it in
  `agentCapabilities._meta`, and move to the real v2 names when the crate
  exposes them. Each item below closes a gap listed after it:
  - *Turn state.* Every kernel turn start and end, follow-ups included,
    emits v2's `state_update` (`running`/`idle`/`requires_action`, with
    `stopReason` and `usage` on idle). Send it as `_kaijutsu/state_update`
    unless the client declares support for the update variant. The fleet
    then waits for `idle` after the last `running`. This replaces the
    quiet wait below.
  - *Failure.* Record `_meta.kaijutsu.failure = {kind, category, severity,
    title, details?, retryable?}` on the `PromptResponse` and on the idle
    update, shaped after claude-agent-acp's `sessionFailure`. `stopReason`
    stays standard. The fleet reads `kind`, not text.
  - *Cancel.* Split `interruptContext`'s result into `turn_interrupted` and
    `continuation_closed` at the source, and carry them as
    `_meta.kaijutsu.cancel` on the idle update that follows.
  - *Permission.* Put the command in `toolCall.title`, `{command, cwd}` in
    `rawInput`, and the hook's reason in `content`. Mirror v2's
    `permission.subject` under `_meta.kaijutsu.permission`. Name the rule
    in the `allow_always` label.
  - *Harbor* has a first-class ACP adapter (`acp:<id>` or a local
    `agent.json` with a `distribution.local` entry). Its runner treats the
    `session/prompt` return as run end and ignores follow-up turns. It
    auto-answers the first allow option and builds ATIF from
    `tool_call`/`tool_call_update` by `toolCallId`, taking cost from
    `usage_update.cost`. A kaijutsu `agent.json` is cheap to add.
    Amy, 2026-09-30: an opt-in flag holds `session/prompt` open until
    the context goes idle, with follow-up turns included, so Harbor
    sees the whole run. A model's own ask no longer starts a follow-up
    turn: the turn holds on it, inside the prompt.
- **Harbor shape** (`docs/acp-fleet.md`, "Harbor shape"): every fleet
  scenario is checked for what Harbor reads. Open findings:
  - **H2: a background completion starts a turn after `session/prompt`
    returns.** Harbor ends the run at the response and never sees it; the
    job's own `shell` call is still `in_progress` then. The planned opt-in
    flag above closes it. `harbor-background-after-response.toml`,
    `contained/background.toml`.
  - **H3: no cost is reported.** `usage_update` carries only the context
    fill, and only when the model's window is known (none for the mock),
    and the prompt response has no `usage`. The kernel keeps no price
    table or cumulative cost. `harbor-usage-cost.toml`.
  - **No model selection.** `session/new` offers no `models` or model
    `configOptions`, so a Harbor run given a model fails before its
    prompt. The bench names the model at launch.
- **A failed follow-up turn has no structured ACP signal.** The runner
  matches agent text "stream error: …".
- **A hook-raised permission request's title is the hook's stderr**, not
  the command asked about.
- **The fleet's cargo test takes about 110 s** (host and approval
  scenarios run as two tests at once), mostly a 3 s quiet wait after each
  prompt. Mock replies a scenario never used go unreported. ACP v1 has no
  idle notification, and the bridge reports turn completion only for the
  interactive turn a `session/prompt` waits on
  (`crates/kaijutsu-acp/src/session.rs`). A follow-up turn started by an
  answer to an ask that holds no turn (no stored command, the MCP path) or
  by a background completion ends with no ACP message. A model's own ask
  holds `session/prompt` open, so for those prompts the quiet wait only
  catches unexpected late messages and could shrink.
- Add the `kaijutsu-acp --connect` agent.
- **A `session/cancel` has no acknowledgment**, so the cancel scenario reads
  the kernel's `turn_interrupted=true` log line from stderr. The bridge's
  own "soft interrupt sent" line does not serve: `interruptContext` answers
  `success` when it only closed a continuation (`turn_interrupted ||
  continuation_closed`, `crates/kaijutsu-server/src/rpc.rs`), so the line
  also appears when no turn was running.
- **A gated `shell_write` is announced `in_progress`**, then reported
  `failed` with the waiting text ("nothing was run"), then `pending`,
  before its answer settles it. Neither "in progress" nor "failed" on the
  wire is final. An approved call's last update carries only
  `rawOutput.exit_code`, so the client shows the waiting text as its only
  content, though the model read the real output.

## Egress: what stays open (2026-09-21)

`docs/egress.md` owns the rule: per-context rows, a reviewer-held list, refuse
on a miss. Amy: "git and any builtin contacting outside resources should go
through the same path over time so we can monitor/classify/constrain." Open:

- **Only `curl` reads the list.** `git` and host programs run under
  `ExternalExec::Allow` reach the network with no egress check.
- **A miss is refused, never adjudicated.** Amy: "we may add a special network
  classifier later but for now we'll rely on allowlist or yolo for curl." The
  plan evaluator sees `curl <url>` as a command; an approval has no way to
  reach the tool at connect time. Needs a design conversation.
- **Nothing records a request.** Monitoring is part of the goal; no span or
  ledger row names the host a context reached or was refused.

## Cast follow-ups (seeded 2026-08-03)

- Nothing reads `cast_slots.loadout`. Remove the column or give it a reader
  in the capability redesign.
- `available_models()` is hand-maintained per provider
  (`llm/mod.rs:899`) though a live Models API lookup exists for the context
  window (`llm/claude/models_api.rs`).

## An unreadable `api_key_file` falls through to env (seeded 2026-08-04)

Four backends warn `api_key_file configured but unreadable`
(`llm/config.rs`, `resolve_api_key`) for a `~/.openai-key` that does not
exist, then read the env var anyway. The env rule next to it already says
naming a source is a statement about where the key lives; the file rule
should match — return `None` and let the registry skip the backend — but
that changes which of Amy's backends load, so it waits for her to fix the
rows or agree.

## MCP subsystem — audit follow-ups (2026-07-29)

- **`InstancePolicy` does not persist across restart** —
  `Broker.policies` (`mcp/broker.rs:125`) is a bare map, so a live-tuned
  `call_timeout_ms`/`max_result_bytes` reverts on restart.
- **No project-instructions discovery** (CLAUDE.md/AGENTS.md analog).
  `build_system_prompt` (`llm/system_prompt.rs`) assembles stored instruction
  sections plus `<situation>`, with no filesystem crawl.

## MIDI device profiles: routing does not consume port roles (`docs/midi-next.md` slice 2)

`PortRoleMatch` exists (`kaijutsu-audio-runtime/src/midi_match.rs:88`), but
`kj midi send`/`identify` still route to a device's FIRST matched port
(`midi_exchange.rs:86,524`, `dj/midi.rs:92,114`). Demonstrated wrong on the
MiniBrute, which answers identity only on port 1. Also unfilled: USB
`vendor:product` enrichment (`midi_in.rs:194`, `usb_id` left `None`), so
matching is name-substring only.

## `rich_json` is unbounded on the wire (seeded 2026-07-18)

`block_output_data` (`kaijutsu-server/src/rpc.rs:9080`) persists `.data`
whole with no size check, bypassing kaish's text-only output limiter. A size
ceiling that fails loud, or CAS routing like `RenderCue`'s `casHash`.

## SFTP over the VFS: appends can clobber each other (`docs/sftp.md`)

`write` (`sftp.rs:566-576`) does one `getattr` for both the generation guard
and the APPEND offset, so two cross-session appenders can both read gen=N
and lose an update. Wants an atomic append: `VfsOps` has no append
primitive (`write_all` replaces the whole file), which would
also make `>>` and jsonl logs cheap. `opendir` materializes a whole
`readdir` per handle (no pagination); the post-write re-getattr race is
accepted in code (`sftp.rs:585`).

## Offline audio analysis has no resource admission or cancellation contract

`kj/audio.rs::audio_beats` starts a blocking task for each request, without
an analysis-specific concurrency limit, decoded sample bound, or inference
cancellation mechanism. Dropping its waiter cannot stop started inference;
runtime shutdown waits for the blocking work. Full-file decoding and mel
extraction precede chunked prediction. A 180 s synthetic track peaked near
564 MiB; two concurrent analyses peaked near 1.37 GiB on the measured host.
See `docs/audio-inference.md` for methodology and placement tradeoffs.

Define bounded admission, input limits, and cancellation semantics before
expanding this workload. Results currently name a mutable host path and
model family, without audio or weight hashes; caching or durable musical
use needs immutable provenance. Executor placement, including a separate
inference service, remains undecided. Hardware timing stays with audiod.

Amy's workload is near-term musical decisions on a shared pulse, with
seconds available for models and media transfer. Evaluate readiness across
queueing, computation, and transfer, and use the existing resolver contract
for basis validation and commit-time fallback. Beat analysis is a candidate
for an optional tool/resolver adapter; retaining a core `kj` verb is not a
requirement. Packaging and relocation remain undecided.

The latency doctrine in `docs/midi.md` also needs measured targets: its
"99.99%" readiness claim has no supporting measurement here, and "a few
seconds — 16–32 bars" conflates durations (at 120 BPM in 4/4, that is
32–64 seconds). Derive horizons from tempo and measured end-to-end costs.

## Audio nodes — follow-up after daemon extraction

- **Keep jobs are in-memory on both sides** — `KeepJobs`
  (`crates/kaijutsu-kernel/src/kj/audio_capture.rs:18`, still a bare
  `Mutex<BTreeMap>`) and `takes.rs::Pool`: after a kernel restart nothing
  can rediscover a job's `/tmp/kaijutsu-audio-<uuid>` staging path. Persist
  the job row (id, node, instance, path, phase) before a recovery verb is
  worth building.
- Profile edits load only on daemon connection/reconnection; raw port
  reconciliation does not refresh profile files.
- Distinguish ingress data loss from lost topology notifications — the
  conservative reset replaces generations on every startup burst, not only
  real loss. See `docs/audio-daemon.md` "Evolution" for the execution
  checklist (node inventory, retained windows, capture export).
- A timed-out RPC (10s) flips `connected=false` and re-sends
  `MetronomeConfig` to the DJ mid-play; should reload config on reconnect
  only, treat a timeout as a retry.
- `--context` is validated once at startup; an archived capture context
  just makes `commit_capture` warn every four seconds forever instead of
  stopping.
- Named render destinations: playback broadcasts to every attached render
  client; multiple machines need an explicit destination contract.
- Kept-take recovery after kernel restart, and explicit handling of a
  permanently lost daemon instance — do not add a broad `/tmp` sweep;
  ownership recovery must name exact job paths.
- Generic `kj` help still understates MIDI verbs; the live subcommand help
  is more complete.

## Architecture & System Design

- **`rpc.rs` is one file of about 10,600 lines.** It shrinks as execution
  moves into the kernel crate. Split the Cap'n Proto trait impl by domain
  (`rpc/vfs.rs`, `rpc/llm.rs`, `rpc/mcp.rs`).
- A broadened role loadout reaches a live context only on re-create or
  restart.

## Drift UX — cross-session ergonomics (2026-08-12)

Design record: `docs/drift-ux.md`; the newer `docs/drifting-dead-letters.md`
(2026-08-16/17) now owns most of drift's architecture backlog. Still open
and not covered by either doc:

- **A received drift is a dead end for reply.** Hydration surfaces the short
  id only (`llm/hydrate.rs`), never the sender's label or a reply hint.
  Not the cheap fix it looks like: `translate_block` has no DB access, and
  a stamped label would go stale when `stabilize_context_label` renames
  `cc-*` contexts. Wants a label snapshot passed in, not a DB handle.
- **Drift edge metadata is inconsistent across delivery paths** — still true:
  immediate push stamps `drift_kind.to_string()` (`kj/drift.rs:406`,
  `"push"`), flush stamps `format!("{}#{}", drift.drift_kind, drift.id)`
  (`drift.rs:793`, `"push#1"`). `kj drift history` cannot uniformly trace an
  edge back to a staging event.
- **MCP connections that never receive hook traffic mint a context that
  never stabilizes/archives** (found 2026-08-12 during the cc-* sweep) — a
  `kaijutsu-mcp --connect` probe with no session gets no `session.end`, so
  nothing reclaims it. Options: lazy registration on first hook event, or
  archive-on-drop for a connection that never stabilized.
- **Arriving drift honoring `--drive`** — the receiver-side per-context
  "honor drive requests" setting (default off) described in the 2026-08-12
  ruling is not built; no code found for it. Its blocker (the rc-lifecycle
  identity smear) is fixed — see Drive gates below — so this is now
  buildable, just not built.

## Drive gates — external drive still ungated (2026-08-12)

Self-drive is gated (`Capability::Drive`, `kj/drive.rs:61-64`). The archived
check shipped: `kj drive` refuses `Concluded`/`Staging`/archived targets,
tested (`drive_refuses_an_archived_context` and siblings,
`kj/drive.rs:274-320`). Still open:

- **External-drive gate (caller != target)** — the genuinely new per-context
  gate, default off with `context_type` defaults via rc. No
  `ExternalDrive`/honor-drive code found; not built.
- **Cold-cache suppression** — computable with no new schema
  (`context_usage.updated_at` vs. the shortest `cache_breakpoints` TTL,
  `kj/cache.rs:138-141`) but not implemented. Refusals must be loud, with a
  way to insist (cold-cache is a cost signal, not a correctness one).

## Musician create cannot name its track; dock sparklines have no meaning yet

The musician create rc attaches to a label-derived track before an explicit
`--track` can move it: `kj context create` has no `--track` passthrough. The
dock sparklines' data source is a placeholder (events/sec, running-block
count); decide what they measure before polishing.

## Control plane (kj): two gaps

- **`--out` writes bypass the VFS.** `kj cas get` (`cas_get`), `kj block cat`
  and `kj block original` `std::fs::write` relative to the server cwd, never
  through mounts. The fix needs `dispatch_cas` and `dispatch_block` to become
  `async fn`, because `VfsOps::write_all` is async and those dispatchers are
  not: that is two `.await`s in `kj/mod.rs`'s dispatch match plus every test
  that calls either dispatcher. Do not bridge it with `block_in_place` and
  `Handle::block_on` instead — `kj` runs under the current-thread runtime each
  SSH session thread builds (`kaijutsu-server/src/ssh.rs`, where rc lifecycles
  re-enter kaish), and `block_in_place` panics there. Attempted and reverted
  2026-09-20 for exactly that reason.

## Index and ABC: two schema-shaped debts

- **The synthesis tables lack `ON DELETE CASCADE`.** `synthesis`,
  `synthesis_keywords` and `synthesis_top_blocks` live in
  `crates/kaijutsu-index/src/metadata.rs`; deletes are manual across the
  three. Do it at the next schema change.
- **ABC MIDI pitch/velocity are unmasked.** The `kaijutsu-abc` MidiWriter
  leaves pitch/velocity unmasked (`midi.rs:970-995`), safe while the one
  caller uses velocity 80.

## Tracks do not re-arm after a kernel restart

A restart resets every track to stopped, and nothing re-arms the tracks that
were playing. Tick counters also do not survive a restart. `docs/tracks.md`
records the current behavior; the re-arm sweep and counter durability are
unbuilt.

## Hyoushigi / Musician — open remainder

`docs/midi.md`, `docs/pcm.md`, `docs/chameleon.md` and `docs/fork-filters.md`
carry the mechanism; these are what none of them cover:

- **No CAS write surface (client→kernel put).** `/v/cas` is read-only
  (`vfs/backends/cas.rs`) and `commitCapture`'s `Cas(hash)` arm refuses
  (`rpc.rs:8546`). Needed at the first heavy payload.
- **Perception is notation-only.** `KJ_HEARD` is ABC; no `MidiToAbcDeriver`,
  so a captured MIDI window is invisible to a model.
- **No chart is seeded into a player's context**, and the OODA Act is
  hardwired to ABC (`kj drive --score-at`, `hyoushigi/model.rs`).
- **Players get no tools at all.** A read-only kaish (kaibo's posture) would
  remove the tool-palette-hangs-small-models cliff by construction and give
  bar math an escape hatch. Decide which RO builtins.
- **Rotate chains pollute the director's tree** (`kj context list --tree`
  shows a 17-deep chain per song); no `--hide-archived` or chain folding.

## kaijutsu-mcp Remote backend collapses multi-context ops to one context

`context_ids()` (`kaijutsu-mcp/src/lib.rs:844`) returns only the joined
context for `Backend::Remote`, so a global search silently skips every other
context; resource/prompt handlers hardcode `kind: "Conversation"` for Remote
(`lib.rs:2871,2918`).

## The stream-start retry loop's use of `retry_disposition` has no test (2026-09-20)

`retry_disposition` (`runtime/llm_stream.rs`) is pinned exhaustively per
`LlmError` variant, but nothing drives the retry loop itself with a real
`AuthError` or a transient error: `MockClient` (`llm/mod.rs`) cannot make
`Provider::stream()` return an `Err(LlmError)`, so only the synchronous
`validate_tool_pairing` path can produce one without a live provider — which
is what `invalid_live_tool_pairing_fails_once_without_retry` already uses. An
error-injection builder on `MockClient` would let that test's shape cover the
permanent and transient cases, and would prove the attempt count.

## OpenAI's 402 becomes a retried `ApiError` (2026-09-20)

`llm/openai/mod.rs:255` maps HTTP 402 to `LlmError::ApiError("insufficient
balance: ...")`, and `retry_disposition` (`runtime/llm_stream.rs`) classifies
every `ApiError` as transient, so an exhausted balance spends the full backoff
reproducing itself. Claude routes the same status through `400..=499 =>
InvalidRequest` (`llm/claude/mod.rs:331`), so the two providers disagree. The
catch-all `_` arm in both also lands a non-4xx/5xx status on `ApiError`, and a
JSON-parse failure of a successful response does too
(`openai/mod.rs:192`, `claude/mod.rs:246`). Decide whether 402 is a permanent
variant or whether `ApiError` needs splitting, and make both providers agree.
Found by kaibo reviewing the retry-classification commit.

## A Claude provider comment promises an error it panics instead (2026-09-20)

`llm/claude/mod.rs:85-89` says a reqwest client-builder failure surfaces as
`LlmError::Unavailable`; the code `.expect()`s and panics
(`claude/mod.rs:108-109`). Either return the error or say that a builder
failure is unrecoverable at construction. Found by kaibo.

## `cargo test -p kaijutsu-solo-acp` fails 21 tests for a missing feature (2026-09-20)

`solo_acp_stdio.rs` drives real turns through `BackendKind::Mock`, which
exists only under the `test-mock` feature, so a plain
`cargo test -p kaijutsu-solo-acp` fails 21 of 23 with
`invalid value 'mock' for '--backend-kind'` rather than skipping them. Declare
the target in `Cargo.toml` with `required-features = ["test-mock"]` so cargo
leaves it alone without the feature, and say in the file which invocation runs
it.

## The theme mirror test only spot-checks fields (2026-09-20)

`theme::tests::default_theme_toml_deserializes` (`kaijutsu-types/src/theme.rs`)
`include_str!`s the shipped `assets/defaults/theme.toml` and asserts about a
hand-picked dozen fields, so a key present on one side and absent on the other
passes. Proven: removing three `SceneGainsData` fields from the struct while
leaving them in the file produced no new failure. Compare key sets instead —
round-trip both sides through `toml::Value` and diff.

## `vfs_getattr` does not reach the app (2026-09-20)

`Vfs.getattr` and `KernelHandle::vfs_getattr` carry `FileAttr.generation` to a
client now, but `ActorHandle` has no command for it, so
`connection/roster.rs`'s poll still reads the whole index every time. Wiring is
an `RpcCommand` variant and a `RpcActor::dispatch` arm, then the feed compares
generations and skips the read.

## `kj cas get --out` and `kj block cat --out` need async dispatchers (2026-09-20)

See "Control plane (kj): two gaps" for the shape. Recorded separately because
the attempt is instructive: a `block_in_place` bridge is not the way around it.

## The editor's terminator reconcile is two block-store calls (2026-09-20)

`mirror_ops` (`kernel/src/editor.rs`) reads the block and then, conditionally,
deletes one terminator at a fixed character offset. The two calls each take and
release the block-store entry guard, so a writer that is not an editor session —
`kj block append`, a model turn, another client — landing between them would put
the delete on changed text. Editor sessions serialize through the registry and
the equality guard makes a spurious fire unlikely, so this is a shape to close,
not an observed loss. Found by kaibo.

## `EditorCore::insert_at` round-trips through lossy text (2026-09-20)

`insert_at` (`crates/kaijutsu-editor/src/lib.rs`) rebuilds the buffer from
`self.text()`, which strips one trailing newline, so a paste into a block ending
in a blank line leaves the buffer one newline short of the block. The kernel
side no longer mistakes that for a doubled terminator, but the editor's own view
is still wrong until the next hydrate. The contained fix is for `insert_at` to
splice the rope rather than its normalized text; the larger one is the
terminator byte-fidelity work in `docs/vi.md`.

## Testing & Tooling

- A failed SSH integration assertion can also panic in russh 0.61.1
  `channels/io/mod.rs:37` during teardown, filling the log with backtraces.
  Reproduced by the shell-draft red tests; investigate harness shutdown before
  attributing the secondary panic to the behavior under test.
- `vfs::backends::local::tests::test_normal_paths_succeed` is flaky under
  full-workspace parallelism (found 2026-08-02).
- `contrib/kaijutsu-runner.sh` rebuilds only `kaijutsu-app`; a wire change
  still needs `kaijutsu-server` and `kaijutsu-mcp` rebuilt by hand
  (`docs/operating.md`).

## Conversation-view latent costs (surface slice 0, 2026-08-18)

- **`ConversationGeometry::reconcile` still never retries a skipped block.**
  On a `None` seed it `continue`s without adding the id to `rows`/
  `block_index`, but `self.block_ids = ids.to_vec()` (`view/geometry.rs:415`)
  still records the full incoming list — so `ids_match` reports no change
  next frame and `sync_conversation_geometry` never calls `reconcile` again
  for that id. The block gets no row until the doc version moves for some
  unrelated reason. Real bug, still open.
- `sync_conversation_geometry` still calls `recompute_offsets()`
  unconditionally through `Mut<_>`, marking `ConversationGeometry` changed
  every frame. Nothing consumes `Changed<ConversationGeometry>` today
  (verified) — the first consumer that tries will be silently defeated.

## `BlockContentCache` is still unbounded (surface slice 3, 2026-08-18)

`view/surface/content.rs:253`'s `BlockContentCache` still only evicts blocks
that leave the document — it grows with scroll depth, a second copy of the
conversation's rendered text alongside the block store's own. Not urgent
(strings, not glyphs; no frame-cost impact — every consumer iterates a
geometry band). Fix is the same pinned-window LRU `shape_cache` already
uses, at a different band (content ±2 screens vs. shape ±1).

## A streaming rich block still re-parses and re-draws whole, every tick (surface slice 4, 2026-08-18)

Confirmed still true: `view/surface/content.rs:9` documents "no streaming
debounce here" by design for the incremental-prefix path, but drawn rich
kinds (ABC, diff, sparkline, SVG, image; `RichKindInfo::is_drawn()`) still
shape as one chunk, whole, on the main thread every content bump — no
debounce, `rich.rs`. Bounded (budgeted parsers), not free; nobody has
measured a streamed diff on this path yet. Fix if it bites: a debounce
scoped to `is_drawn()` blocks in `Running` status only.

## ANSI rendering, pass 1: four corners left open (2026-08-19)

Stage 3.2/3.3 (`StyleSpan` → parley ranged brushes → `PositionedGlyph`) is
shipped. Still open, all verified against current code:

- **Italic (SGR 3) is parsed but never rendered** — no `FontStyle`/`Italic`
  handling found anywhere in `kaijutsu-app`'s text/view code. It's the one
  attribute that changes shaping, so it needs a column-alignment decision
  first.
- **ANSI spans are dropped on a block detected as a drawn rich kind** (ABC,
  SVG, sparkline, diff, image) — those shape through `RichShaper`, which
  never sees the styled-span list. Doesn't arise from today's ingest hooks
  (shell output is plain).
- **`INVERSE` is resolved CPU-side and stale until re-shape** — an inverted
  span bakes both colors, carries `style_index = 0`; the theme epoch forces
  a re-shape within a frame or two.
- **Backgrounds/underlines bake color into vertices** — `ShapeKey::
  baked_theme_epoch` exists for exactly this reason.

## Capability names and layout need a redesign sweep (Amy, 2026-08-20)

Still unaddressed: `Capability` (`kaijutsu-kernel/src/mcp/binding.rs:133`)
still mixes `Instance`/`Tool{instance,tool}`/`Facade` (granular), `Admin`/
`AllInstances`/`AllFacades` (broad), and bare-word verb authorities
(`Drive`/`Fork`/`Drift`/`Transport`/`Operator`/`ConfigWrite`/`Exec`/
`Editor`/`System`/`House`) — three different shapes, grown ad-hoc as gaps were found. Amy:
"Director should only get `shell` as long as it has `kj`" — director's
broad facade/exec grants (`assets/defaults/rc/director/create/S10-binding.kai`)
are worth revisiting once `kj` itself can reach what a shell used to be
for. "We'll do a cap redesign sweep soon so it's a good time to
experiment" — treat `Editor` as provisional until that sweep.

## Worker loadout: what still points at house verbs (2026-10-05)

`house` hides the administration verbs from worker seats (`docs/instrument-design.md`,
"Many hands, one trust boundary"). These surfaces were left as they are:

- **The `kj` tool schema lists every verb.** `KjBuiltin::schema` reflects the
  whole clap tree, and one schema serves every context, so a worker's shell
  still shows `ledger`, `rc`, and the rest in schema-driven help. Filtering
  needs a schema per context loadout.
- **Held-ask text names `kj ledger`.** `PENDING_REASON_EXECUTES`,
  `PENDING_REASON_RETRY` (`kj/gate.rs`) and the pending-ask remedy in
  `mcp/error.rs` tell the reader which `kj ledger` command the reviewer runs.
  A worker reads them too and cannot run them. Say "wait for the reviewer"
  and point the worker at `kj wait --ask <id>`.
- **Client ledger polling runs in the joined context.** The app, TUI, and MCP
  clients call `kj ledger list|show` through `execute_kj_quiet(ctx, ..)` in
  whichever context they are joined to. A live root actor holds the house
  verbs in any context, so a person joined to a `coder` context keeps it; an
  mcp session whose actor is not a root character is still refused there.
- **The musician tick and rc scripts bypass the capability gates.** rc runs
  as a privileged caller, so `kj drive` in `musician/tick/S10-drive.kai` is
  not checked against the musician's `drive` grant, and neither is `house`.
  The `drive` comment in `musician/create/S10-binding.kai` claims otherwise.
- **A shell with no joined context may run house verbs.** `kj ledger`,
  `kj character`, `kj roster`, and the others take no context so a person at a
  bare shell can run them, and there is no loadout to narrow. A context whose
  loadout is missing or empty is refused them, as `require_cap` refuses.
- **`synth` is classified as a worker verb** (`house.rs`); it was in neither
  list the assignment gave.

## Shell settlement follow-ups

- **`did_spill` misses nested spills (kaish, open upstream).** Only a
  top-level statement's spill reaches the program-level flag.
  `x=$(seq 1 100000)` truncates the captured value and reports
  `did_spill: false`; the same holds inside function bodies and `if`/`while`
  conditions. The shell envelope presents `did_spill` as "output was capped",
  so a caller can believe it received everything. Also upstream:
  `seq 1 100000 && echo YES` never runs the right side, because the spilled
  left operand is remapped to exit 3 before `&&` reads it, while
  `if seq 1 100000` is true. The kaish lead has a follow-up PR approved; check
  `docs/shell-envelope.md` wording when it lands.
- **No timeout knob on the server kernel**, so the wire timeout test proxies
  with `exit 124`; a real one needs an injectable `TimeoutPolicy`.
- **Interruption elsewhere still reads as a plain failure.** An interrupted
  result review now projects exit 130. A structured `kj` call returns the
  refusal reason as an RPC error before `exec_result` is read
  (`runtime/structured.rs`, the `Refused` early return), and an unpublished
  approval pair recovered at restart (`approval_resume.rs`,
  `recover_unpublished_pairs`) projects 1. Decide whether either should carry
  the interruption marker.
- **A failed wake after a completion notice is never retried.**
  `completion_notice::deliver` commits the notice block, which stamps
  `block_id` (`block_store.rs`, `insert_completion_notice`), then calls
  `request_turn`. If that fails (context archived during the
  `gate_resume_window` await, or the worker shutting down), every later scan and
  startup recovery skips the row because they select `block_id IS NULL`. The
  notice block survives; the owed continuation is lost with no durable record.
  Approval delivery loses its wake the same way by design, but keeps a
  redeemable answer. Two more paths end the same way (kaibo review,
  2026-09-26): `deliver` returns after the insert when its stop token is
  cancelled, and a notice refused by a stopping turn finds that turn still
  in flight and skips its resume (`completion_notice.rs`, `resume`). The
  resume's other refusals (context no longer live, performer changed,
  window closed) are also silent. Decide between recording an owed wake durably and
  reserving turn admission before the notice insert; see the resource admission
  design, `docs/resource-admission.md`, "Known hazards".
- **Uncovered:** scheduler call sites of tick/rotate (`beat.rs` ~2225, ~2230);
  the mailbox notice block for a paused PostCall; wire-level interactive cancel
  (no wire cancel exists for durable interactive commands).

## Waiting on upstream

Each of these needs a change in another project before anything here can
move. Re-check when that dependency's version changes.

### Kaish positional suffix expansion

The locked kaish 0.17.2 expands `${0%.kai}` to an empty value, rather than
stripping the suffix or rejecting unsupported syntax. The rc migration exposed
this as a failed companion-file read. Shipped scripts use `dirname "$0"` and
`basename "$0" .kai` instead. Audit unsupported parameter expansion in kaish;
keep it explicit rather than silently accepting a different expression.
No upstream issue has been posted.

### OSC 8 hyperlinks wait on a ratatui span attribute (2026-09-13)

`present::links` detects the paths and URLs a hyperlink would target, with
tests, but nothing emits one: ratatui 0.30.2 and ratatui-core 0.1.2 carry
no hyperlink attribute on a `Style` or a `Span`, and the backend diffs
cells — escape bytes smuggled into a cell's symbol would be miscounted as
width and overwritten by the next diff. The exit is a ratatui feature that
adds the attribute, or a custom backend that writes OSC 8 around a cell's
own bytes; neither is built.

### Hi-res wheel (v120) blocked at winit/sctk — slow drags are a compositor dead zone (2026-08-16)

Root cause confirmed, not app-side: MX Master emits sub-detent v120 →
sctk 0.19.2 has no `AxisValue120` handler (verified in the cargo cache) →
winit 0.30.13 pins that sctk and has zero value120 references even on its
own master branch, so the block is winit, not sctk or the compositor —
switching KWin→Mutter would not fix it.

**Lane PARKED by Amy's call** ("fix kaijutsu-app for what already works
… experiment later with the HID++ device"). Carried forks exist and were
protocol-verified live (`~/src/research/{client-toolkit,winit}`, branches
`tobert/axis-value120-0.19` and `tobert/wayland-axis-value120-0.30`) but
`[patch.crates-io]` was removed from `Cargo.toml` per the no-committed-
path-deps rule; re-wire via the `tobert/*` GitHub forks when resumed. Root
probe also found the Bolt receiver (046d:c548) runs on `hid-generic`, not
`hid_logitech_dj` — hidpp never manages the mouse, a separate,
possibly-upstreamable one-line kernel fix
(`~/src/research/bolt-dj-bind-test.sh`). Do not re-add smoothing hacks
meanwhile; pipeline stays fraction-ready.

### `ExecResult.output` cannot carry structured data past kaish's output limiter (found 2026-07-18)

Still true on kaish 0.17.1: `materialize()` (`kaish-types/src/result.rs:504`)
clears `.output` unconditionally even when `.out` never consumed it. `kj`
works around it by writing only `.data`, bridged at `block_output_data`
(`kaijutsu-server/src/rpc.rs`), regression-pinned in `kj_builtin.rs`. The
clean fix is upstream: clear `.output` only inside the `if .out.is_empty()`
branch.

### vte 0.15.0 drops a control byte after a chunked partial UTF-8 codepoint (2026-08-19)

Upstream bug in `vte::Parser::advance_partial_utf8` (not `kaijutsu-ansi`):
`strip(&[0xCD, 0xAE, 0x1B, 0xFF])` differs when fed as one chunk vs. two —
chunked feed leaks an extra replacement character and silently drops the
`0x1B` (ESC) that followed a resumed multi-byte codepoint. Narrow trigger
(incomplete UTF-8 lead byte as the last byte of a `feed` call, continuation
immediately followed by a control byte in the next) — exactly the shape of
chunked kaish output mixing multibyte text and escapes. Pinned as a
regression test asserting the *current* buggy divergence:
`crates/kaijutsu-ansi/tests/vte_partial_utf8_regression.rs` — a `vte`
upgrade that fixes it fails this test loudly. Not worked around in
`kaijutsu-ansi` (would mean reimplementing vte's UTF-8 resumption). Fix:
file upstream against `alacritty/vte`, or vendor-patch if needed sooner.

## Zorak's rc tree still holds the shared base (2026-10-03)

Stances moved into each context type and the shared base was removed. Reseed
does not remove files the seed no longer carries, so zorak's rc tree still
holds `lib/create/S00-base.{kai,md}`, the `S00-base` links under `coder`,
`default`, and `director`, and `mcp/create/S00-stance.{kai,md}`. A coder created
there would run both the old base and the old tiered `S00-stance.kai`. When we
next deploy zorak, delete those files by hand, then run
`kaijutsu-server rc reseed --force`. Delete this entry once that is done.

## Client ledger reads inherit worker policy (2026-10-06)

The TUI's quiet pending-ask poll runs `kj ledger list` in the displayed
context (`kaijutsu-client/src/ledger.rs`). A coder context intentionally
denies `kj ledger`, so Amy sees `cannot refresh pending asks: denied by the
gate policy: context_type config (coder) denies kj ledger`. Client ledger
reads and human decisions need their review context without widening the
coder's model loadout or removing its static deny. The same shared helper
serves other clients; correct the common path and verify over RPC.

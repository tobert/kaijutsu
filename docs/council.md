# The council: System 1 in the gate

**Status: built, off by default.** The gate consults the council on both
paths when `gate.toml` enables it for a context type, with a second decision
for each program a submission runs; report-stop is built but not wired (`docs/issues.md`, "Council gate: what 43c4a61c left open"). The wire contract is `docs/council-api.md`
(with `docs/council-api.openapi.yaml`); outside source material is
`docs/council-dossier.md`.

The council is a fast model check on each shell submission whose statements
the static gate rules leave uncovered. It reads the submission after several
kaijutsu contexts (Amy's own words, the system rules, and later more) and
answers allow, ask, or report, with a probability for each, per context and
pooled. A high-confidence allow runs the submission; anything else goes to
the ledger as it does today. It is enforcing from its first deployment, and
every decision is recorded so we can watch it.

It is what lets kaijutsu coders serve our swarms the way kaibo does: an
autonomous seat with System 1 in the hot path for safety checks and
monitoring, instead of an ask on every uncovered statement.

## The flow

```text
submission -> gate_policy::evaluate        (docs/gate-policy-tuning.md)
  any Deny             -> refused, as today
  all Allow            -> runs, as today
  any Ask (a key)      -> ledger, as today; the council reads it for the record only
  Uncovered remain     -> PreCall hooks run, as today
     a hook denies or asks -> as today; the council never lowers a hook's ask
     hooks continue        -> one council decision for the submission, under a deadline
        allow   pooled p(allow) >= allow_at, every read's verdict mass >= mass_floor,
                with require_agree every read's top answer is allow,
                no control-text hit, identity matches its threshold
                -> the Uncovered statements become council allows; the submission runs
                   with a durable auto-decision row naming the council
        ask     -> ledger ask, with the council's answer attached
        report  -> ledger ask, plus a council report event; an autonomous seat's
                   running turn is interrupted
        miss    -> ledger ask; the row records why the council did not answer
```

Amy, 2026-10-03: "static rules fire first, if an explicit allow allows
something, that's it. same for deny. they fall through to system 1. if it is
high confidence on allow, we allow. if it says ask we go to the ledger. when
it's report we'll keep it informational for now, logging / etc.. our traces
should record the confidences too btw."

- **One decision per submission.** The gate composes per program: every
  statement must be allowed for a submission to run, and it is never
  partly applied (`kj/gate_policy.rs`, module doc). The council therefore
  reads the whole submission once, and its allow covers the Uncovered
  statements in it. A submission that also holds a static ask still asks.
- **The hooks run first.** A council allow placed in broker PreCall's fast
  path would skip every PreCall hook, the shell-escape guard included
  (`mcp/broker.rs`, `evaluate_phase_with_mode`). The council therefore runs
  after the hooks let the call continue, and it can lower only an ask the
  Uncovered tier would have opened. A hook's own ask stands.
- **The declared fallback is the ledger's ask.** A council that is down, slow
  past its deadline, answers outside the schema, or answers with low mass
  leaves the submission where it is today, and the miss is recorded with its
  cause. A human decides. The megakernel's guide asks clients to "fail open,
  loudly"; for a gate, failing to the ask is that.
- **A control-text hit forces the ask.** Any `signals.control_text` entry
  means the submission spells a model's control token. That is suspicious in
  a shell statement, so the decision is recorded and the gate asks, whatever
  the probabilities say.
- **Report stops an autonomous seat and keeps its ask.** A report opens the
  ask like any other and, when the seat's turn is autonomous, stops that
  turn with `Kernel::interrupt_context_keeping_asks`: no further model
  calls, and the ask stays redeemable. An allow runs the stored command once
  in the approval worker; a deny runs nothing. The seat stays stopped until
  someone drives it again. Amy, 2026-10-04: "report should keep the ask
  redeemable; it will still come to an ask and we will likely have false
  positives sometimes." After the ask exists, the kernel publishes the
  `council.report` event, which names the submission, the seat, the
  per-context answers, and `council.report_stop` (`not_autonomous`,
  `nothing_running`, `interrupted`, `failed`, or `no_ask`); the
  `council.decide` span carries the same value. The decision record does
  not store it.
  A turn is autonomous when the kernel drove it (`TurnOrigin::Autonomous`)
  or when its requester is not a live root character, so a swarm seat
  driven over ACP counts. A person at the keyboard keeps the ordinary ask.
  Amy, 2026-10-04: "report should stop autonomous seats".
- **The council never lowers a static ask and never overrides a deny.** A
  static `ask` key stays firm (`assets/defaults/gate.toml`: "ask is firm — no
  lower layer can allow it"). The council's answer on it is recorded, which
  gives us data to fit on.

## Where it plugs in

The evaluator is synchronous and runs while holding the kernel database lock
(`kj/gate.rs`, `run_gate_recorded`; `mcp/broker.rs`, `program_policy`), and
the RPC paths evaluate a submission three times when it escalates: PreCall's
fast path, the ask's description, and `run_gate_recorded`. The council call
is none of those:

- **An async step after evaluation, outside the lock.** It takes the
  evaluation, asks the council when Uncovered statements remain, and turns
  them into council verdicts, a new layer beside the existing ones
  (`kj/gate_policy.rs`, `Layer`), so `auto_reason` can name it.
- **PreCall's fast path never consults it.** A council allow there would
  skip the hooks. The fast path keeps today's Allow, Deny, and Escalate.
- **Two consumers, each running it once.**
  - On the RPC shell paths, in `ask_tier_ask`, before `run_permission_ask`.
    That ask is `Origin::Hook` with the hook id `gate policy`
    (`GATE_POLICY_SUBJECT`), and the council's verdict must ride into the
    gate's own evaluation through the ask spec; today `ask_tier_ask` passes
    only a description, and `run_gate_recorded` evaluates again from scratch.
  - On the `shell_write` tool path, in `run_gate` for `Origin::ShellGate`.
    PreCall's hooks have already finished there: the tool runs, and so
    reaches the gate, only when they continue.
- **The gate-policy ask and the shell gate only.** A hook's own ask (any
  other hook id) stands. `Origin::HookResult` reviews a captured result, and
  `Origin::KjVerb` carries no plan; the council sees neither. The origin alone
  cannot tell the gate-policy ask from a hook's (`docs/issues.md`, "What a
  replacement risk scorer inherits"), so the filter reads the hook id.
- **A held or open ask is never council-allowed.** A submission whose digest
  already has an ask that is open, or answered and held for the approval
  worker, takes the Escalate path, so `run_gate_recorded`'s single-use guard
  still decides it. On the Allow path a held answer is not spent
  (`kj/gate.rs`, step 2b), so a council allow on a retry would run the
  command beside the worker's run. A council verdict never revives or
  bypasses an earlier ask.
- **The read-only `shell` tool is unchanged.** It enforces its policy
  structurally and runs no gate (`mcp/servers/shell.rs`); the council does
  not run there.
- **Roots are unchanged.** On the RPC paths a live root character's
  Uncovered statements run without an ask today (`ask_tier_ask`); the council
  does not run for them. On the tool path roots still ask, and the council
  runs for them like any seat.
- **It replaces the sandbox posture for coder seats.** A section with
  `uncovered = "allow"` never reaches Uncovered, so the council never sees
  it. A coder seat that gets the council goes back to `uncovered = "ask"`,
  and System 1 decides what runs.
- **The deadline fits both paths, but they hold different things.** On the
  tool path the call holds a runtime slot, the broker's per-instance permit,
  and the shell call's timeout while it waits; the RPC paths call the hooks
  before any of those.

On the RPC paths a tier allow leaves no ledger row today; a council allow
leaves one there. That is new on purpose: every council decision is on the
record.

## The record

Every council decision is a durable row, allow or not. That is a schema
change: `approval_signals` holds one label and score per source
(`approval-ledger/src/schema.rs`), and an ask's signals start empty
(`kj/gate.rs`, `build_ask`).

- A `council_decisions` table keyed by the submission's request id holds the
  spec id, the server identity, the pool, the threshold applied, the deadline,
  the outcome (allow, ask, report, miss, and a miss's cause), and the
  timings.
- A child table holds one row per read: the context id, the snapshot read,
  the head the kernel expected, and the verdict's mass and confidence.
- A child table holds one row per read and option: the option, its log
  probability, and its probability.
- `house_rules_head` names the head of the house-rules context the decision
  read, NULL when it read none (see "House rules").
- `SignalSourceKind` gains `council`, so an ask carries the pooled verdict as
  a signal a human sees in `kj ledger show`.
- Traces carry the same numbers as span attributes (`docs/telemetry.md`),
  and the kernel sends its `traceparent` with each decision.

## Council contexts are kaijutsu contexts

A council context is a kaijutsu context held on the council server under its
own context id. The server keeps its snapshots; kaijutsu keeps the identity,
the blocks, and the history. Below, "held context" means the server's copy.

Amy: "I am thinking 3 context feeds for the adjudicator contexts: a shadow
context that has my prompts + additional ones I put in or banto drifts in.
system rules (shared by all contexts), and maybe 1 or 2 more if we find good
reason. These let us tune in real time." Then, 2026-10-04: "each character
perhaps has a council voice, and we compose them as needed, so when banto
runs a coder it would compose system, mine, and its own voice. Copying would
work like context forking; we have tools for filtering and eventually
compacting."

| Context | Holds | Built by |
|---|---|---|
| system rules (`council-system`) | the house rules every seat shares | Amy and banto, by chat |
| a character's council voice (`council-<character>`) | that character's guidance: Amy's words in `council-amy`, banto's directions in `council-banto` | forking from that character's working contexts with fork filters, then chat, drift, and exclusions |

- **A decision composes voices along the reviewer chain.** It starts at the
  submitting seat's reviewer (`docs/approval-identity.md`, reviewer
  resolution) and climbs to the first live root character, collecting each
  character's `council-<character>` context. A coder banto runs for Amy
  reads `council-system`, `council-amy`, and `council-banto`. Several coders
  under one banto share `council-banto`. A character with no voice context
  is skipped and the skip is recorded; it is not a miss.
- **A root's voice votes; a director's voice observes, for now.** Amy's voice
  and the system rules are pooled into the verdict. A model director's voice
  is read under its own `direction-check` spec ("is this action following my
  directions?"), after the gate has decided, off the hot path, and recorded
  with the decision. It never enters the pool or changes the outcome. Amy:
  "It's an experiment; I'm unsure where it'll in authority." Giving it a
  vote is a later choice, made from the recorded answers.
- **A voice is built like a fork, not fed.** Nothing copies prompts into a
  voice automatically. A voice context is made and refreshed the way a fork
  is: copy from the character's working contexts through fork filters
  (`docs/fork-filters.md`), shape it with `kj stage exclude`, and later
  compact it. A director may also drift into its own voice as it goes, so
  the projection reads drift blocks in voice contexts.
- **The chain follows the reviewer, for now.** Amy: "reviewer is fine for
  now". Recording which context drives a coder is the alternative if the
  reviewer and the director come apart. Above the first reviewer, the climb
  walks the submitting context's fork tree the way `kj ledger escalate`
  does, taking each responsible character it has not seen, until a live
  root.
- **`council-system` is reserved.** A character named `system` would collide
  with it, so the chain walk fails loudly on one.
- **Voices count against the server's limit.** The system context plus the
  voting voices must fit `identity.limits.contexts_per_decision` (8 on the
  megakernel); a longer chain is a miss naming the limit, not a silent cut.
- **A worked example keeps its thinking.** A model's finished thinking rides
  on its next reply as that turn's `reasoning` (council API 0.2.2), so a
  council context can hold examples the way a stronger model reasoned them.
  The council answers with an empty thinking region, so these examples are
  how it learns the reasoning. A probe on the megakernel (2026-10-05) put
  eight programs judged by qwen3.8-max, with its reasoning, in a code
  context: the program verdicts went from 30 to 35 right of 36.
- **Tuning is chat.** Amy switches to a council context and talks to it.
  `kj stage exclude` removes a block from what the council reads, the same
  way it shapes a fork. No special UI.
- **A change reaches the server as a whole-context `PUT`.** The kernel
  projects the context (its framing as the system message, its blocks as
  turns, excluded blocks left out, and a model's thinking as its reply's
  `reasoning`) and sends it with `If-Match` on the head
  it last saw and `warm` naming the gate's specs. The server feeds only what
  changed, then rebuilds the spec layer, so the next decision starts from a
  held snapshot.
- **Snapshots are a budget.** A server extends a context only from a turn
  marked `snap`, so a `PUT` re-feeds every turn since the last one, and each
  snapshot costs `identity.limits.snapshot_bytes` (on the megakernel about
  112 MiB of recurrent state). The projection marks `snap` at boundaries we
  expect to keep, such as every few guidance blocks and each handoff, and
  reads the cost of a `PUT` with `dry_run` before choosing more.
- **After a restart, the next decision sends each context it reads again.**
  Nothing is sent at boot. The `PUT` is idempotent; a server with `park`
  answers it from disk.
- **A context's id is resolved from its label at decision time.** A label
  that resolves to nothing is a council miss with that cause, not a gate
  refusal, so a missing context cannot stop every seat.

## House rules

The council reads the house rules of the workspace the proposing seat works
in, so it judges a command by the rules the people there wrote down. The
context holds those rules and nothing the seat said or did.

- **On with `[council] house_rules = true`.** Off by default. Every
  decision, the shell decision and each program decision, reads it after
  `[council] contexts` and the voting voices. Observations do not. The old
  keys `seat` and `seat_tokens` fail the parse with a message naming
  `house_rules` and `house_rules_tokens`.
- **What it holds.** The first `AGENTS.md` found walking up from the seat's
  working directory (its home directory when it has none), read through the
  kernel VFS and cut to `house_rules_tokens` (default 2000), keeping its
  start. Tokens are estimated at four bytes each. A file that cannot be read
  or is not UTF-8 is skipped with a warning. The body is one user turn that
  names the file's path and carries its text, marked `snap`. The seat's
  prompts, narration, thinking, and tool calls never reach it.
- **No file, no context.** A seat with no `AGENTS.md` above its working
  directory has no house-rules context, and the decision reads one fewer
  context. Terminal-bench task containers ship no `AGENTS.md`, so a
  benchmark decision reads only its configured contexts.
- **Its shape.** The label is `house-rules`, in the `reads` log field and in
  miss causes. A fixed system message says it holds the house rules of the
  workspace the proposing seat works in, from its `AGENTS.md`, and grants no
  permission beyond them.
- **It votes.** It is pooled with the other reads. With `require_agree`,
  every read must top allow on its own, so the rules cannot turn another
  read's ask into an allow.
- **Shared by content.** Its id is a UUIDv5 over the sha256 of the projected
  body, in a namespace fixed in `council/sync.rs`. Every seat under one
  `AGENTS.md` shares one held context and its snapshots, and an edited file
  is a new id. The kernel sends a body only when it changed from the last one
  it sent under that id, so a seat's work sends nothing. The server keeps
  the snapshots of an id the file no longer produces; `docs/issues.md`,
  "Council house rules: what is open", records that.
- **It is pinned and counted.** The decision pins it with `at`, like every
  context, so it reads the rules as of the submission. `[council] contexts`,
  the voting voices, and the house-rules context together must fit
  `identity.limits.contexts_per_decision`; a miss names the limit.
- **It is ephemeral on the server.** When the server lists the `persist`
  capability (council API 0.2.5), the kernel sends a house-rules context with
  `persist: false`: it sends the context again after a restart, so the
  server need not park it to disk. `council-system` and the voices stay
  persistent. Amy, 2026-10-05: "we will have councils over time that are
  ephemeral in nature and our hardware is limited." A capability the kernel
  does not know is ignored, so a server can add one without breaking older
  kernels.
- **The record.** `council_decisions.house_rules_head` names the head the
  decision read, NULL when it read none. The info-level outcome line and the
  `council.decide` span carry `house_rules_head`, or `none`.
- **Why not the seat's own account.** The seat's brief and narration were
  read here before. Narration changed every few decisions, so the kernel
  re-sent the context often (29 times in one 18-minute benchmark run), and
  council decisions took 11 to 14 seconds. A probe on the megakernel with an
  `originals`/`network` rubric showed that the shell rubric with
  `council-shell` catches the dangerous case, `sqlite3 main.db ".tables"` on
  a WAL SQLite would delete, without the seat's account. Amy, 2026-10-05:
  "'the seat' is a terrible name and I think we got that wrong. It can stop
  with AGENTS.md. It should not get a running dialog from the model."

## Shadow voice (design, not built)

A shadow voice is a council context that holds what one seat has done to the
world: each outward call it made, in order, with no results, narration, or
thinking. The
council judges each command alone today, so a cleanup of scratch the seat
made an hour ago looks like destroying an original (`.partb`, 2026-10-06).
The shadow gives it that history. Amy, 2026-10-06: "each command, not the
model's full output, would go into the council member. maybe tool calls
too. not the results. so it's small and makes up the workstream of things
the model under observation has tried." It starts with `shell_write` and
grows with the gate.

- **Contexts and casts, not `gate.toml`.** Amy, 2026-10-09: "I want to do
  less in gate.toml over time and move to more configurable and
  transparent setups with casts and contexts." A shadow needs no
  `gate.toml` entry: its context_type's rc says what it does, and the
  seat's cast says which model plays it.
- **Two types: `judge` and `bump`.** A `judge` gives its opinion of each
  command. A `bump` answers proceed or bump, as bumper mode does today,
  with the seat's history. Each type's rc holds its framing and its
  question. Both start in recording mode: the answer is written to the
  dialogue and the record and changes no decision. Moving bumper mode
  from `[council]` onto a `bump` shadow that acts comes after the
  recordings show the history helps.
- **One child per type, a fork child of the seat.** A shadow is a
  kaijutsu context of context_type `judge` or `bump` whose fork parent is
  the observed seat, so it shows in the seat's tree and archiving the seat
  archives it. It inherits no blocks and has no performer; the kernel, not
  a seat turn, asks its model. The kernel finds a shadow by its structural
  fork edge and its context_type, never by its label. The label is
  `<type>-<seat label>`; it does not start with `council-`, so the
  reviewer-chain walk never takes it for a character's voice. Each child
  keeps its own dialogue, so a judge sees only its own earlier opinions.
- **The cast picks the model.** A context already resolves its model from
  the cast slot named by its context_type (`model_resolution.rs`), and a
  shadow inherits the seat's cast. A cast with a `judge` slot gives the
  seat a judge, pointing at the megakernel, lfm2d, or Jev through
  OpenRouter; a cast with no such slot gives none. "mk bumps", "lfm2d
  bumps", and "Jev bumps" become casts. A seat with no cast of its own
  uses its performer's default cast (`kj character set coder --cast
  mk-judge`), so every seat a `coder` character plays gets the judge with
  nothing to turn on per seat; `kj context create --cast` pins another.
- **Outward actions only.** Council time is limited on our hardware, so a
  shadow records what the seat does to the world, not what it reads. The
  gate already draws that line: the read-only `shell` tool runs no gate,
  and `shell_write` does. A shadow records every call the gate evaluates
  for its seat, whatever the gate decides, including calls an allow rule
  passes (`mkdir .partb` is the history that makes a later `rm -rf .partb`
  routine). A tool that comes under the gate later enters the shadow with
  no change here.
- **The gate writes the shadow.** It has the call when it evaluates and
  the outcome when it decides, so it is the one writer: the call before
  the decision, the answer after it (`council/shadow.rs`, called from
  `run_gate`). The gate evaluates each call once; a call sent again after
  a bump is a new call and a new turn. A shadow write that fails is logged
  at error level and never changes the gate's decision.
- **A dialogue: the call, then the answer.** Each call is a user text turn
  holding the tool name and its input, such as `shell_write: rm -rf
  .partb`, cut to a budget. The shadow's answer comes back as the model
  turn after it, with what the gate did (ran, bumped with its flavor,
  asked, refused). A council server answers with option probabilities,
  not prose, so the kernel writes its answer as text from the top options.
- **Each model is fed the way its API works.** The stored dialogue is the
  same; hydration differs by backend.
  - A council server (megakernel, lfm2d) gets each call as it arrives: the
    kernel `PUT`s the new tail with `warm` off the hot path, so the held
    snapshot is primed when the question comes.
  - A chat model (Jev) is hydrated whole for each request: the dialogue is
    rendered as one script-like transcript in a single prompt, with the
    question about its last call.
- **Append-only keeps a council server cheap.** `snap` falls on every
  eighth turn by position, so each new turn re-feeds at most eight turns.
  Each snapshot costs `snapshot_bytes`, about 112 MiB on the megakernel.
  Kaijutsu manages the bulk of that cache itself, choosing what to keep,
  park, and drop, rather than leaving it to the server's eviction. No cap
  or compaction until measurements call for one.
- **The record.** Each judge answer is a `council_observations` row on
  the gate's decision: the choice, each question's answer from the
  shadow's read, the head it read, and the time it took. Comparing its
  choice with the decision's outcome shows where the history changed the
  answer.
- **Open.** Whether feeding its own past answers back anchors a model; the
  record shows it. How a council spec names the case when the call is
  already the shadow's last turn. How a shadow is switched from recording
  to acting, on the context or in its type.
- **Out of scope.** A human's commands on the RPC shell paths make no
  tool-call block and get no shadow. Drift is an edge between players; the
  shadow is written by the kernel and is not drift.


### Shadow build order

Each step lands with its tests, and its line here changes to say what
shipped. A fresh session resumes at the first step not marked done.

1. **Done: the judge child and its dialogue,** on the `shell_write` tool
   path. A seat whose cast has a `judge` slot gets one `judge` fork child
   on the first call the gate evaluates; each call is a user turn
   (`shell_write: <command>`, cut at 800 bytes) and the outcome is the
   model turn (`ran`, `asked the reviewer`, `bumped (<flavor>)`,
   `refused`, `not run: the gate was unavailable`). Tests in
   `council/gate_e2e.rs`, `a_judged_seat_records_…` and the two after it.
   1b. **The RPC shell path.** Seats that reach the gate through
   `shell_pre_call_hooks` (MCP clients) are not recorded yet. ACP seats
   run kernel model turns, so their `shell_write` calls are recorded.
   1c. **Done: archive with the seat.** The shipped `coder` archive script
   (`rc/coder/archive/S10-shadows.kai`) runs `kj context archive
   --children --type judge --confirm`, so a seat's judge shadows are
   archived with it (`docs/kaish-integration.md`, the `archive` verb).
   1d. **Done: the stance.** A new shadow runs the `judge` type's `create`
   lifecycle (`rc/judge/create/S00-stance.md`). Its system instruction
   blocks are the shadow's framing, followed by a runtime fact naming the
   seat. A shadow with no stance is never sent; priming fails and names
   `rc reseed`. Synthesized 2026-10-09 from the council-shell examples: the
   seat's own earlier work is routine to remove, data it did not create
   is an original, a resend after a bump is a new attempt, and earlier
   answers are not repeated for their own sake.
2. **Done: priming a council server.** A `judge` slot whose backend has
   kind `mk` names the server by its `base_url` (the megakernel serves
   `/council/v1` at the same address). After each recorded call, a task
   of its own `PUT`s the shadow whole (`CouncilSync::prime_shadow`), under
   its stance (1d), warming the seat's shell
   spec from `gate.toml`; a seat type with no council there is not primed
   (logged). Priming has its own lock and a 30 s timeout, so it never
   holds up a decision. A judge on any other backend is not primed. Test:
   `a_judge_on_a_council_server_is_primed_with_each_call`.
3. **Done: asking and recording.** After the council decides a judged
   seat's call, a task of its own reads `[council] contexts`, the shell
   spec's own contexts, and the house rules when they are on, then the
   shadow, on the judge's server, under the decision's spec and case,
   with a 30 s deadline (`shadow::spawn_judge`). Voices are not read. The
   answer is a `council_observations` row on the decision, with
   `voice_label` `judge-<seat>`, and it is added to the call's outcome
   turn as `; judge: <choice> (p=<pooled p>)`. A call a static rule
   decided is recorded but not judged. The judge never changes the
   decision. Test: `a_judge_answers_after_the_decision_and_its_answer_is_recorded`.
   Still open: whether the judge should read the voices.
4. **Measure on moltar.** Wired 2026-10-09: backend `mk-zorak`, cast
   `mk-judge` (one `judge` slot), and a `coder` character whose default
   cast is `mk-judge`. A smoke seat played by `coder` ran `mkdir` then
   `rmdir`: priming took 1.6 s and 4.2 s, the gate's council answered
   proceed at 0.992 and 0.953, and the judge with the history answered
   proceed at 0.994 and 0.967. First real seats, 2026-10-10: a DeepSeek
   banto (`banto-shadow`) ran two DeepSeek coders `--as coder`, judge on
   `mk-zorak`. Over 29 judged decisions the judge answered 27, all
   `proceed`, and agreed with every gate pass; the gate bumped two as
   `unjudged` (misses) where the judge, with the history, said proceed.
   The judge's two misses were priming that did not finish in its 30 s.
   Gate time rose from 1.1 s to 12.9 s and judge time from 1.5 s to
   18 s on the first seat, which ran more calls in a burst; the gate reads
   no history, so the rise is the shared server's load, not the shadow's
   length. Still to do: a seat that does something the gate should doubt,
   priming time and snapshot count from the server, and more seats.
5. **Jev hydration.** A chat-model judge gets the whole dialogue as one
   script-like transcript per request. Compare it with the megakernel on
   the same seats.
6. **`bump` recording, then acting.** A `bump` type records proceed or bump
   with the history. Moving bumper mode off `[council]` waits for step 4
   and 6 numbers.
## Specs and cases

The gate holds one spec per kind of case: the shell spec and the program
spec. `gate.toml` refuses a second spec of one case. The shell spec is the example in `docs/council-api.md`, "A
decision, end to end", without its `text` question while the megakernel
does not declare `describe`; it lives at `/config/kernel/council/shell-gate.json`.
The kernel computes each spec's id itself (RFC 8785, then sha256), `POST`s
the spec at boot and whenever the file changes, and treats a server that
answers with a different id as a fault. A spec change is a new spec id and
new thresholds.

The case's `state` is a JSON object: the submitted command whole, the
`KJ_TOOL_PLAN` statements and clauses, from the kernel the seat's context
type and working directory, and, when the submission runs programs, their
decisions' outcomes (`programs`, see "Programs").

- **Programs are cases of their own.** A submission that runs program text
  gets a decision on each program under the program spec, beside the shell
  decision. It is council-allowed only when the shell decision and every
  program decision allow. Amy: "we will go a bit further like pulling out
  python programs to be evaluated on their own". The next section has the
  rules.
- **Known plan gaps reach the council.** `docs/issues.md`, "What a
  replacement risk scorer inherits", lists them: `clause` drops redirects,
  variables arrive unexpanded. The state carries the submitted command whole
  beside the clauses for that reason.

## Programs

`python3 /tmp/fix.py` tells the shell spec nothing about what `fix.py`
does, so the council reads the file too (`council/programs.rs`).

- **What counts as a program.** python (`python3`, `python`, a versioned
  name, or a path ending in one): a script operand, `-c CODE`, a heredoc or
  a `<` file on stdin, and `-m NAME` when `NAME.py` or `NAME/__main__.py`
  exists beside the cwd. `bash`, `sh`, `dash`, `zsh`, `ksh`: a script
  operand, `-c CODE`, or a heredoc. A command named by a path (`./fix.sh`)
  when the file starts with `#!`. Each of these behind `env`, `timeout`,
  `nice`, `nohup`, or `time`. `python3 -m pytest` with no local `pytest.py`
  is library code: no program decision.
- **Reading.** Paths resolve against the seat's cwd through the kernel VFS,
  the mount table the shell sees. A file is read whole up to 16 KiB and
  must be UTF-8; the record keeps the sha256 of the exact bytes. Local
  imports are not followed: top-level `import x` and `from x import` names
  that resolve to `x.py` or `x/__init__.py` beside the program are listed
  as `imports_not_shown`.
- **Not judged means not council-allowed.** A program whose text is not
  known before the statement runs gets no decision, and the submission
  asks: a word that expands at run time (`python3 $script`), stdin the
  submission does not show (`curl ... | bash`), an expanding heredoc, a
  relative path after a `cd` in the same submission, a file the
  submission writes before it runs it, a missing, non-UTF-8, or larger
  file, a ninth program, and a `gate.toml` with no program spec. One write
  still says what runs: a `cat > FILE` heredoc to the file the submission
  then runs; its text is the program.
- **The shell decision does not judge the text again.** A program that
  gets its own decision appears in the shell case as
  `<program judged separately>`: in `command`, each statement's
  `rendered`, and its clauses. The shell decision judges the invocation,
  and the program decision judges the text. This covers `-c` text, a
  heredoc, and a `cat > FILE` heredoc; a file operand's text was never in
  the shell case. Text the submission spells differently, such as with
  escapes, stays as written.
- **The program case.** `state` holds `invocation` (the `command` that runs the
  program, its `statement`, and the whole `submission`), `path`,
  `language` (`python`, `shell`, or `shebang`), `context_type`, `cwd`,
  `imports_not_shown`, and `program`, the text, last.
- **The program spec decides on its rubric.** `program-gate.json` asks
  `originals` (`reads`, `changes_routine`, `changes`; opening data with a
  program that can write to it counts as changing it), `network` (`none`,
  `packages`, `other`), and `verdict`, all choice questions: the megakernel
  does not answer `text`. A program decision allows when the pooled
  `originals` is `reads` or `changes_routine` and the pooled `network` is
  `none` or `packages`, and, with `require_agree`, every read's own answers
  are too. Otherwise a `verdict` argmax of `report` reports and anything
  else asks. Probes on the megakernel (2026-10-04) found per-question
  confidence low everywhere while the argmax sorted twelve sample programs
  correctly; p(allow) did not. The threshold for the program spec supplies
  the identity and the mass floor; its `allow_at` is not used. Verdict mass
  on program text ran about -0.3 to -1.0, so a program threshold needs a
  looser floor than the shell's; the bench uses -1.5, an exploration value.
- **The program decisions run first.** They run together, each under its
  own `deadline_ms`; then the shell decision runs under its own, with their
  outcomes in its case. A submission with programs waits at most two
  deadlines. A program decision took about 0.9 to 1.6 s on the megakernel,
  so a deadline under 2000 ms misses. Each submission logs its program,
  shell, and total time at info.
- **The shell decision reads the programs' outcomes.** Its case gains
  `programs`, last: per program, its `statement`, `language`, `path`
  (null for text the submission carries), `outcome` (`allow`, `ask`,
  `report`, `miss`, or `unread`), a `cause` for a miss or an unread
  program, and the rubric's `originals` and `network`. On the megakernel
  with the bench's rules, shell p(allow) on a backup plus an allowed
  program went from 0.82 without the field to 0.98 with it, and a program
  that asked or reported took the shell decision from allow (0.97) to ask
  or report. The same facts as an extra council context did not help (0.86)
  and cost a `PUT`. The combine rule is unchanged: the submission is
  council-allowed only when the shell decision and every program decision
  allow.
- **The record.** Each program decision is a `council_decisions` row linked
  to the same ask. `council_programs` hangs off the shell decision, one row
  per program: the command, the language, the path and sha256 judged, the
  imports not shown, and either the program decision or the reason there is
  none. The ask carries one signal per decision; a program's label reads
  like `ask: originals=changes, network=none`. Its text names each program
  decision, each program not judged, and, when the shell decision allowed,
  the program that held the submission. A program decision's report stops
  an autonomous seat like a shell report. Every decision logs at info with
  its spec, outcome, and p(allow).
- **The judged script is the script that runs.** A council allow carries
  each judged file's path and sha256 to the execution seam
  (`runtime/command.rs`, `capture_command`), on the tool path and the RPC
  paths alike. Right before kaish executes, the kernel reads each file
  again; if any differs, the command is rejected and nothing runs: "the
  script changed after the council judged it; send the command again."

## Thresholds

- **A threshold belongs to its spec.** Each `[[council.threshold]]` names
  one spec and gives its `allow_at` and `mass_floor`; a second threshold for
  one spec fails the parse. The council server's identity (`weight_hash`,
  `engine`, `tokenizer_hash`, `template`) is recorded with every decision
  and on its span, but does not select a threshold. Amy, 2026-10-05: "let's
  drop this pinning business against mk, we're not that precise for this."
  Pinning made every decision a miss each time the megakernel moved its
  engine, three times in one day. A threshold that still names the identity
  fields fails the parse with a message saying to delete them.
- **Contexts steer the probabilities across a stable threshold.** That is
  what tuning by chat does. We record the context heads and the server
  identity with every decision, so the effect of a change is measured, not
  assumed, and a refit can read which server produced each number.
- **The starting threshold is a guess.** `allow_at = 0.98` with a mass
  floor is where we start; the recorded decisions are what we fit the next
  one from.
- **An allow needs every read to agree.** `require_agree` defaults to true:
  the pooled answer allows only when every context's top answer is allow.
  Amy, 2026-10-04: "require agree yes". A spec's own `require_agree`
  overrides it.
- **The gate never narrows options.** Narrowing changes the distribution, so
  the gate always reads the spec's full option set.

## Configuration

The council needs new sections in `/config/kernel/gate.toml`. Today the file
refuses unknown fields and an unparseable file refuses every gated
submission (`kj/gate_policy.rs`, `GateToml`), so the parser change ships
before any host file gains these lines.

```toml
[council]
server = "http://zorak:8090"
contexts = ["council-system"]  # read for every decision
voices = true                  # compose council-<character> along the reviewer chain
house_rules = true             # read the AGENTS.md above the seat's cwd; off by default
house_rules_tokens = 2000      # the house-rules context's budget
pool = { method = "loglinear", weights = "mass" }
deadline_ms = 700              # sent as timeout_ms

[[council.spec]]
name = "shell-gate"            # /config/kernel/council/shell-gate.json
case = "shell"

[[council.spec]]
name = "program-gate"          # /config/kernel/council/program-gate.json
case = "program"
contexts = ["council-code"]    # read by this spec's decisions only, after [council] contexts

[[council.threshold]]
spec = "shell-gate"
allow_at = 0.98                # pooled p(allow)
mass_floor = -0.05             # each read's verdict mass, a log probability

[context_type.coder.council]
enabled = true

[context_type.director.council]
enabled = true
contexts = ["council-amy", "council-banto"]
```

A context type's `contexts` replaces `[council] contexts` for that type.
Omitting it keeps the shared list. The replacement must hold at least one
distinct, non-empty label and cannot repeat a spec's own context. Specs,
thresholds, worked examples, voices, and house rules keep their shared
settings. Explicitly listing `council-banto` makes it a voting read; this
does not enable reviewer-chain voices or their observations.

To chat with a council context, give it `facade:edit_input` and
`facade:submit_input`, a live model performer with a distinct reviewer,
and a model. Its system instruction can ask it to think about submitted
guidance and acknowledge it briefly. The council server supplies decisions;
the context's regular model supplies these chat replies. Finished thinking
and replies reach the next council decision through the same projection
as its user guidance. A context with no binding denies compose input.

A spec's `require_agree` overrides `[council] require_agree` for its own
decisions; the bump-only bench turns it off for programs. A decision that
answers and does not allow logs each context's own answers at info, in its
outcome line's `reads` field (`label: question=choice ...`, reads joined by
`; `), so a run's log names the context that dissented.

A spec's `contexts` are read by that spec's decisions alone, after
`[council] contexts` and before the voting voices. A label already in
`[council] contexts`, or listed twice, fails the parse. A code context with
worked examples belongs to the program spec; the shell decision does not
need it.

The council's settings live only here. One `deadline_ms` bounds the whole
decision, preparing the contexts and spec included: the kernel sends it as
`timeout_ms` and stops waiting when it passes, and the miss names the phase
it passed in. A 404 naming the spec makes the next decision post the spec
again; a 404 naming a context sends that context again.

## Servers

- **Megakernel first.** Qwen3.8-Flash-Next on zorak has given more
  consistent reads than lfm2d. Amy, 2026-10-04: "mk seems to be overall
  better". It serves `/council/v1/` (megakernel `docs/council-v1.md`) with
  the `leave_one_out` capability; context records are not parked yet, so
  the kernel sends every council context again after the server restarts.
  Long runs against it on zorak take the heavy lock
  (`flock -w 900 ~/.cache/zorak-heavy.lock`).
- **lfm2d second,** behind the same port once it serves `/council/v1/`.
- **Jev through OpenRouter, for comparison only.** It speaks the Decisions
  API, which our contract includes, but it holds no contexts, so a
  comparison renders each context into `state` and pays for it per request.

The kernel talks to every server through one port with one implementation
of the wire contract. It recomputes each pooled answer from the reads'
`logprobs` and `mass` and checks it against the server's within 1e-9; a
mismatch is a miss, never an answer.

The council server sits inside our one trust boundary (`docs/instrument-design.md`,
"Many hands, one trust boundary"), on the tailnet, with no auth of its own.
It holds Amy's words and the house rules, so it stays there.

## Why a kernel port and not an rc hook

| | Kernel port | rc pre_call hook |
|---|---|---|
| Allow | the gate auto-allows with a durable row | a hook's exit 0 only proceeds; on the RPC paths `ask_tier_ask` opens the ask anyway, and nothing lets a hook lower one |
| Context sync | before each decision the kernel reads the council contexts and sends a changed one | a second mechanism outside the kernel |
| Records | decision rows, traces, and the report event are kernel facts | a hook writes `kj ledger signal add` rows |
| Egress | the kernel's own client | every seat's hook needs the server in its egress list |

The council decides an ask, and the gate is "the one place that authority
lives" (`docs/writing.md`, Terms). Model execution stays on the council
server; the kernel coordinates contexts, deadlines, thresholds, and records.
On 2026-09-28 Amy expected the replacement to come "still via rc mostly"; the
reason to differ is that a hook cannot lower an ask, and teaching hooks to
would put an authority decision in a script.

## Rollout, smallest first

1. **The contract.** These docs, the OpenAPI file, and a conformance suite
   both servers run. The megakernel serves `/council/v1/`.
2. **The gate, enforcing.** The parser change, the kernel port, the record
   tables, one spec, the system-rules context and Amy's voice sent before a
   decision that reads them, traces, the report event, and a report interrupting an
   autonomous seat's turn. Deployed on zorak against
   the megakernel, with coder seats moved back to `uncovered = "ask"`.
3. **Live contexts.** Change-feed sync with `warm`, so tuning by chat lands
   on the next decision. Re-read recent decisions after a context changes and
   record which flipped and which context moved them.
4. **Programs.** The program spec and the second decision. Built; the
   bench (`contrib/bench/gate-council.toml`) declares it.
5. **lfm2d and Jev.** lfm2d behind the port; a Jev comparison run.
6. **System 2 and loud reports.** A reasoning reviewer after the ledger:
   it reads an open ask (first those where the council disagreed, `agree`
   false or a high `spread`) and attaches its judgment to the ask as advice,
   off the hot path. Amy, 2026-10-04: "system 2 after the ledger". Report
   alerts past logging come with it.

## Bumper mode

`[council] mode = "bumper"` changes what the council's answer does. The
default, `mode = "gatekeeper"`, is everything above. Amy, 2026-10-05:
System 1 answers pass or bump. Pass is "everything looks good and safe to
proceed". Bump is "hold on, think about that some more": the seat gets the
action refused with guidance and tries again, with no human in the loop.

- **The spec.** The shell spec's `verdict` choice has a pass option named
  `proceed`; every other option is a bump flavor, and its `means` text is the
  guidance. `assets/defaults/council/shell-bump.json` holds `proceed`,
  `try_harder` (the goal is fine, but it needs more care first), and
  `do_less` (it reaches past what the task needs). A spec with no `proceed`
  option, or with no other option, is a miss with that cause. The program
  spec keeps its rubric and its `allow` option.
- **Pass or bump.** A spec that declares the rubric questions (`originals`
  and `network`) decides by them, whatever its case: the seeded
  `shell-bump.json` does, so a shell decision passes on an allowing pooled
  rubric, and a rubric that holds it names its flavor, such as
  `originals=changes`. A bump-only db-wal run on 2026-10-05 lost its WAL to a
  first-batch `sqlite3 /app/main.db "SELECT ..."` the verdict alone passed at
  p(proceed) 0.988; in a probe the rubric with a `council-shell` context of
  worked examples answered `originals=changes` there. A shell spec without
  the rubric passes when pooled p(proceed) is at or
  above the threshold's `allow_at` and, with `require_agree`, every read's
  top answer is `proceed`. Otherwise it bumps, with the pooled argmax among
  the other options as its flavor. A program decision passes on an allowing
  rubric; otherwise it bumps, and its flavor names the rubric answers that
  held it (`originals=changes`, `network=other`). The submission runs only
  when the shell decision and every program decision pass.
- **`originals` saves backups for databases and system state.** A change
  is `changes_routine` when it touches ordinary files (source, scratch,
  build output, the seat's own work), when a backup exists, or when the
  command keeps history (`kj context archive`). `changes` touches a
  database, service, or system state, such as a `.db` file or anything
  under `/var/lib` or `/etc`, with no backup. The rubric used to ask about
  data the seat "did not create", which the council cannot see: a
  director's `rm -rf` of its own scratch and an archive of its own lane
  were bumped on 2026-10-06. Amy: "ease things so it's a bit less obsessed
  with backups except when doing things like messing with .db files or
  /var/lib or similar." A shell spec that names any rubric question must
  carry the whole rubric with its passing options, or the decision is a
  miss naming what is missing; a host seeded before the rename misses
  rather than bumping every routine change.
- **`undo` reads whether it can be taken back.** The shell spec asks it
  beside the rubric: `reversible`, `normal` (it cannot be taken back, but it
  is a normal step whose changes the work expects), or `irreversible`. Amy,
  2026-10-10: reversibility, and "situation normal; not reversible but this
  is a normal operation with expected changes", not a broad risk read. It
  never decides and has no mass floor; its pooled answer is in the record,
  and a bump shows it to the seat through `{undo}`.
- **The fallback does not change.** A control-text hit, a miss, and a
  program that was not judged leave the ordinary ask (except in bump-only
  mode, below).
- **A bump opens no ask.** The call fails with the bump message, nothing
  runs, and no permission request is made. The council decision is recorded
  with outcome `bump` and its flavor, and links to no ask. Because no ask
  or durable denial exists, the same submission sent again is decided again
  by the council.
- **The limit.** `bump_limit` (default 3) is how many times one submission
  of one seat context is bumped. The count is the number of bump decisions
  with no ask for that context and submission digest
  (`approval_ledger::council::list_bump_flavors`), a program's bump counting
  for its submission. When the count is at the limit, the next would-be bump
  is an ordinary ask. Its description and a council signal carry the count
  and the flavors.
- **The message.** `/config/kernel/council/bump.md`, seeded from
  `assets/defaults/council/bump.md`, is read at each bump, so a change shows
  on the next one. It fills `{guidance}`, `{flavor}`, `{attempt}`,
  `{limit}`, and `{undo}` (the council's `undo` read as a sentence with a
  leading space, or nothing when it was not read). The default directs and does not invite argument. A template
  that cannot be read is an error in the log and in the message, and the
  built-in text is used.
- **The record.** `council.decide` and the info line carry `outcome = bump`,
  `bump_flavor`, and `p_proceed`.
- **Reading bumps back.** `kj ledger bumps` lists each seat's streak, the
  bumps in a row since its last submission the gate did not refuse as a
  bump (one that ran or asked), with their flavors
  and the `undo` read of the last one; `--context` lists one seat's judged
  submissions, newest first, with a digest prefix that shows a resend. A
  submission is bumped when its shell decision links no ask and did not
  pass (`approval_ledger::council::list_council_submissions`).

### Bump-only

`[council] mode = "bump-only"` is bumper mode with no ask, except an
escalation when `escalate` is set (below). The
benchmarks run it while the ask side is tuned. Amy, 2026-10-05: "I was
intending to run the benchmarks with bump only, we have a lot of tuning to
do on the more ask/deny side."

- **Every decision that does not pass bumps, saying why.** The shell
  decision is read first, then each program decision. A miss bumps with
  flavor `unjudged` ("the council could not judge it: send it again, or
  write it more plainly.", or "could not judge it in time" past the
  deadline). A miss often comes from one read's mass falling just under the
  floor while every read agreed, so sending it again unchanged is a fair
  answer. The
  cause stays in the record and the log: a DeepSeek seat that read "refit
  one in gate.toml" in a bump rewrote its own gate (2026-10-05). A control-text hit, the only ask a
  bumper mode can produce, bumps with flavor `control_text`. A program the
  council could not read bumps with flavor `unread` and names the program.
- **There is no limit.** The message counts `attempt N of ∞`. The default
  template does not forbid sending the same command again: a miss is often
  worth one more try. Amy: "even an accidental bump can be
  worked around, and it still forces the thinking and intent we want." A
  `bump_limit` with `bump-only` fails the parse. A miss is recorded with
  outcome `miss`, so it does not raise the attempt count.
- **Escalation after a streak.** `escalate = { bumps = N, minutes = M }`
  in `[council]` (bumper or bump-only; off when unset) turns the would-be
  bump that makes N in a row for one seat within M minutes into an ordinary
  ask to the seat's reviewer. The streak counts the submissions the gate
  refused as bumps (`seat_bump` in the record), across submissions; any
  submission that was not refused ends it, whether it ran or asked, and so
  does the escalated ask. A gate fault after the council decided is not a
  bump. In bumper mode the bump limit asks first for one submission sent
  again, so a streak longer than `bump_limit` needs different submissions.
  The ask's description and a council signal name the streak and its
  flavors. Two submissions decided at once can both read the same streak;
  see `docs/issues.md`. Amy, 2026-10-09: "escalate after N consecutive
  bumps in N minutes, but observe first"; `kj ledger bumps` is the reading
  to pick N and M from.

## Open questions for Amy

- **The ask's wait in a swarm.** A swarm seat whose submission the council
  sends to the ledger waits on a human. Is that the behavior we want, or
  should the seat get the refusal back and choose another command?

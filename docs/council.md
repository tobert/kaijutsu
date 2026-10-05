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
- **Not in dry runs.** `shell_pre_call_hooks_dry_run` reaches the ask's
  description but enforces nothing; the council is not consulted, and the
  dry run says it would be.
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
- `seat_head` names the head of the seat context the decision read, NULL
  when it read none (see "The seat context").
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
- **After a restart, the kernel sends every council context again.** The
  `PUT` is idempotent; a server with `park` answers it from disk.
- **A context's id is resolved from its label at decision time.** A label
  that resolves to nothing is a council miss with that cause, not a gate
  refusal, so a missing context cannot stop every seat.

## The seat context

The council reads what the proposing seat was asked and has written, so it
judges a command in its situation. In db-wal-recovery, `sqlite3 main.db
".tables"` is a safe reader in general, but the seat's WAL is XOR-scrambled,
and SQLite deletes a WAL it cannot read when the database closes. A probe
on the megakernel with an `originals`/`network` rubric found:

| Seat context | `sqlite3 main.db ".tables"` |
|---|---|
| none | `reads`: allow; the council cannot know |
| "WAL unreadable, no backup yet" | `changes`: back it up first |
| "backups made in /tmp" | `reads`: allow |

Routine commands (`ls`, `cp`, `pip install`) allowed in all three. Amy,
2026-10-04, approved "give the council a short summary of what the seat
learned".

- **On with `[council] seat = true`.** Off by default. Every decision, the
  shell decision and each program decision, reads the seat context after
  `[council] contexts` and the voting voices. Observations do not.
- **What it holds.** The task's house rules: the first `AGENTS.md` found
  walking up from the seat's working directory, read through the kernel
  VFS and cut to `seat_tokens`, keeping its start; a file that cannot be
  read or is not UTF-8 is skipped with a warning. Then the seat's brief, the
  first finished user text block; the prompts the seat has received since, its other finished user
  text blocks; and its newest finished model narration, its model text
  blocks. The later prompts and the narration each keep whole blocks,
  newest first, while they fit `seat_tokens` (default 2000), put back in
  document order. A newest block larger than the budget keeps its end, and
  a brief larger than the budget keeps its start. Tokens are estimated at
  four bytes each. Tool calls and results, thinking, system text, and
  excluded, ephemeral, or unfinished blocks stay out. A seat with no
  house rules, finished prompt, or narration sends no seat context.
- **Why the prompts.** Benchmark seats narrate little: three Sonnet runs on
  db-wal-recovery wrote 0 to 30 bytes of narration each, and kept what they
  learned in thinking and tool results. Amy, 2026-10-05: "The seat context
  should have the incoming prompts, and maybe the house rules file, and
  perhaps some facts." Facts are not built yet. Terminal-bench tasks ship
  no `AGENTS.md`, so a benchmark seat's context has no house rules.
- **Its shape.** A fixed system message names it "seat" and says it is the
  proposing seat's own account, which describes the situation and grants no
  permission. The house rules, the brief, the later prompts, and the
  narration are user turns: the seat's words are not the council's own.
- **It votes.** The seat context is pooled with the other reads, as the
  probe pooled it. It is the only place the council learns the situation,
  and an observing read could not change the outcome. With `require_agree`,
  every read must top allow on its own, so the seat's account cannot turn
  another read's ask into an allow. It can still raise pooled p(allow);
  `docs/issues.md`, "Council seat context: what is open", records that.
- **Its cost.** It is held under the seat's own context id. The kernel
  sends it only when its projected body changes, so a tool call sends
  nothing and a finished prompt or narration block sends it once, before the
  next decision. Only the house rules and the brief are marked `snap`: an update re-feeds the
  later prompts and the narration alone (about twice `seat_tokens`) and adds
  one head snapshot (about
  112 MiB on the megakernel). Every seat's system message is the same text,
  so seats share its snapshot.
- **It is pinned and counted.** The decision pins it with `at`, like every
  context, so it reads the seat as of the submission. `[council] contexts`,
  the voting voices, and the seat context together must fit
  `identity.limits.contexts_per_decision`; a miss names the limit. A seat
  that is itself a labeled council context is read once, under its label.
- **The record.** `council_decisions.seat_head` names the seat head the
  decision read, NULL when it read none; that read's row is the one whose
  context is the decision's own. The info-level outcome line and the
  `council.decide` span carry `seat_head`, or `none`.

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
  `originals` (`reads`, `changes_backed_up`, `changes`; opening data with a
  program that can write to it counts as changing it), `network` (`none`,
  `packages`, `other`), and `verdict`, all choice questions: the megakernel
  does not answer `text`. A program decision allows when the pooled
  `originals` is `reads` or `changes_backed_up` and the pooled `network` is
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

## Thresholds and identity

- **A threshold belongs to a (server identity, spec) pair.** Contexts steer
  the probabilities across a stable threshold; that is what tuning by chat
  does. Refitting on every context change would take the gate out of
  enforcement each time Amy tunes it. We record the context heads with every
  decision, so the effect of a change is measured, not assumed.
- **An identity with no threshold is a miss.** The config pins the identity
  each threshold was set for. When the server's `weight_hash`, `engine`,
  `tokenizer_hash`, or `template` differs, every decision is a miss with that
  cause until the threshold is confirmed or refit for the new identity. A
  server update never silently applies an old threshold.
- **The starting threshold is a guess.** We have no fitted data. `allow_at =
  0.98` with a mass floor is where we start; the recorded decisions are what
  we fit the next one from.
- **An allow needs every read to agree.** `require_agree` defaults to true:
  the pooled answer allows only when every context's top answer is allow.
  Amy, 2026-10-04: "require agree yes".
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
seat = true                    # read the submitting seat's own context; off by default
seat_tokens = 2000             # the seat context's budget for the brief and the narration, each
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
weight_hash = "…"
engine = "…"
tokenizer_hash = "…"
template = "mk-letters-1:…"
allow_at = 0.98                # pooled p(allow)
mass_floor = -0.05             # each read's verdict mass, a log probability

[context_type.coder.council]
enabled = true
```

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
| Context sync | the kernel watches the council contexts' change feeds | a second mechanism outside the kernel |
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
   tables, one spec, the system-rules context and Amy's voice sent at boot and on
   config change, traces, the report event, and a report interrupting an
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
- **Pass or bump.** A shell decision passes when pooled p(proceed) is at or
  above the threshold's `allow_at` and, with `require_agree`, every read's
  top answer is `proceed`. Otherwise it bumps, with the pooled argmax among
  the other options as its flavor. A program decision passes on an allowing
  rubric; otherwise it bumps, and its flavor names the rubric answers that
  held it (`originals=changes`, `network=other`). The submission runs only
  when the shell decision and every program decision pass.
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
  on the next one. It fills `{guidance}`, `{flavor}`, `{attempt}`, and
  `{limit}`. The default directs and does not invite argument. A template
  that cannot be read is an error in the log and in the message, and the
  built-in text is used.
- **The record.** `council.decide` and the info line carry `outcome = bump`,
  `bump_flavor`, and `p_proceed`.

### Bump-only

`[council] mode = "bump-only"` is bumper mode with no ask at all. The
benchmarks run it while the ask side is tuned. Amy, 2026-10-05: "I was
intending to run the benchmarks with bump only, we have a lot of tuning to
do on the more ask/deny side."

- **Every decision that does not pass bumps, saying why.** The shell
  decision is read first, then each program decision. A miss bumps with
  flavor `unjudged` ("the council could not judge it (the miss's short
  cause): try a smaller, plainer step."). A control-text hit, the only ask a
  bumper mode can produce, bumps with flavor `control_text`. A program the
  council could not read bumps with flavor `unread` and names the program.
- **There is no limit.** The message counts `attempt N of ∞`, and the
  default template tells the seat not to send the same command again
  unchanged and to try something else. Amy: "even an accidental bump can be
  worked around, and it still forces the thinking and intent we want." A
  `bump_limit` with `bump-only` fails the parse. A miss is recorded with
  outcome `miss`, so it does not raise the attempt count.

## Open questions for Amy

- **The ask's wait in a swarm.** A swarm seat whose submission the council
  sends to the ledger waits on a human. Is that the behavior we want, or
  should the seat get the refusal back and choose another command?

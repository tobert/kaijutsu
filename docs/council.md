# The council: System 1 in the gate

**Status: design, not built.** The wire contract is `docs/council-api.md`
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
- **Report stops an autonomous seat.** A report opens the ask like any
  other, publishes a kernel event that names the submission, the seat, and
  the per-context answers, and, when the seat's turn is autonomous,
  interrupts that turn the way `kj interrupt` does (`Kernel::interrupt_context`).
  The seat stays stopped until someone answers the ask or drives it again.
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
reason. These let us tune in real time."

| Context | Holds | Fed by |
|---|---|---|
| voice | Amy's prompts, and guidance she adds; one context shared by every seat | the kernel copying her prompts; Amy |
| system rules | the house rules every seat shares | Amy and banto, by chat |
| the reviewer's seat | the judgment of the director that reviews this coder, such as banto | that director, writing text blocks in it |
| (later) | one more, when evidence asks for it | |

- **One voice context for every seat.** It is one set of snapshots and one
  fit. Amy, 2026-10-04: "one global voice context is fine".
- **A director has its own seat, shared by the coders it reviews.** Banto's
  view reaches the council as a context of its own, not as drifts into Amy's
  voice, so the reads keep Amy's words and banto's apart and `leave_one_out`
  can say which of them moved a decision. Amy, 2026-10-04: "I think we'd give
  banto its own seat on the council" and "if banto has several coders they
  would share that banto's council seat". The director writes that context's
  text blocks itself; the projection reads them like any other.
- **The seat follows the ask's reviewer.** Every ask already resolves a
  reviewer (`docs/approval-identity.md`, reviewer resolution). When that
  reviewer is a model character, the decision adds the context labeled
  `council-<character>`, so every coder banto reviews reads `council-banto`.
  When the reviewer is a live root character (Amy at her own root), the
  voice context already speaks for her and no seat is added. A model
  reviewer with no seat context is recorded on the decision and read
  without one; it is not a miss.
- **Tuning is chat.** Amy switches to a council context and talks to it.
  `kj stage exclude` removes a block from what the council reads, the same
  way it shapes a fork. No special UI.
- **A change reaches the server as a whole-context `PUT`.** The kernel
  projects the context (its framing as the system message, its blocks as
  turns, excluded blocks left out) and sends it with `If-Match` on the head
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

## Specs and cases

The gate holds one spec per kind of case: the shell spec now, the program
spec later. The shell spec is the example in `docs/council-api.md`, "A
decision, end to end"; it lives at `/config/kernel/council/shell-gate.json`.
The kernel computes each spec's id itself (RFC 8785, then sha256), `POST`s
the spec at boot and whenever the file changes, and treats a server that
answers with a different id as a fault. A spec change is a new spec id and
new thresholds.

The case's `state` is a JSON object: the submitted command whole, the
`KJ_TOOL_PLAN` statements and clauses, and from the kernel the seat's context
type and working directory.

- **Programs are cases of their own.** A submission that runs program text
  (`python3 -c`, a heredoc, a script file; `KJ_TOOL_PLAN`'s `interpreter`
  names the source) gets a second decision on the program under a program
  spec. It runs only when both allow. Amy: "we will go a bit further like
  pulling out python programs to be evaluated on their own".
- **Known plan gaps reach the council.** `docs/issues.md`, "What a
  replacement risk scorer inherits", lists them: `clause` drops redirects,
  variables arrive unexpanded. The state carries the submitted command whole
  beside the clauses for that reason.

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
contexts = ["voice", "system-rules"]
reviewer_seat = true           # add council-<reviewer> when the reviewer is a model
pool = { method = "loglinear", weights = "mass" }
deadline_ms = 700              # sent as timeout_ms

[[council.spec]]
name = "shell-gate"            # /config/kernel/council/shell-gate.json
case = "shell"                 # later: "program"

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

The council's settings live only here. The kernel sends `deadline_ms` as
`timeout_ms` and stops waiting at the same deadline.

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
   tables, one spec, the voice and system-rules contexts sent at boot and on
   config change, traces, the report event, and a report interrupting an
   autonomous seat's turn. Deployed on zorak against
   the megakernel, with coder seats moved back to `uncovered = "ask"`.
3. **Live contexts.** Change-feed sync with `warm`, so tuning by chat lands
   on the next decision. Re-read recent decisions after a context changes and
   record which flipped and which context moved them.
4. **Programs.** The program spec and the second decision.
5. **lfm2d and Jev.** lfm2d behind the port; a Jev comparison run.
6. **System 2 and loud reports.** A reasoning reviewer after the ledger:
   it reads an open ask (first those where the council disagreed, `agree`
   false or a high `spread`) and attaches its judgment to the ask as advice,
   off the hot path. Amy, 2026-10-04: "system 2 after the ledger". Report
   alerts past logging come with it.

## Open questions for Amy

- **The ask's wait in a swarm.** A swarm seat whose submission the council
  sends to the ledger waits on a human. Is that the behavior we want, or
  should the seat get the refusal back and choose another command?

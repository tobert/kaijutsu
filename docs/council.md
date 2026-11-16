# The council: System 1 in the gate

**Status: built, off by default.** The gate consults the council on both
paths when `gate.toml` enables it for a seat's performer, with a second decision
for each program a submission runs. The wire contract is council v2 (`docs/council-backend.md`),
and the kernel uses `gogi-proto` for its types, spec ids, and pooling math.

The council is a fast model check on each shell submission whose statements
the static gate rules leave uncovered. It reads the submission after assigned
kaijutsu contexts and answers allow, ask, or report, with a probability for each,
per context and pooled. A high-confidence allow runs the submission; anything else goes to
the ledger.

## The flow

```text
submission -> gate_policy::evaluate        (docs/gate-policy-tuning.md)
  any Deny             -> refused
  all Allow            -> runs
  any Ask (a key)      -> ledger; the council reads it for the record only
  Uncovered remain     -> PreCall hooks run
     a hook denies or asks -> the council never lowers a hook's ask
     hooks continue        -> one council decision for the submission, under a deadline
        allow   pooled p(allow) >= allow_at, every read's verdict mass >= mass_floor,
                with require_agree every read's top answer is allow,
                no control-text hit, identity matches its threshold
                -> Uncovered statements become council allows; submission runs
                   with a durable auto-decision row naming the council
        ask     -> ledger ask, with the council's answer attached
        report  -> ledger ask, plus a council report event; an autonomous seat's
                   running turn is interrupted
        miss    -> ledger ask; records why the council did not answer
```

- **One decision per submission.** Every statement must be allowed for a submission to run. The council reads the whole submission once, and its allow covers the Uncovered statements.
- **The hooks run first.** The council runs after hooks let the call continue, and it can lower only an ask the Uncovered tier would have opened. A hook's own ask stands.
- **Declared fallback.** A council that is down, slow past deadline, answers outside schema, or has low mass leaves the submission where it is today, recorded as a miss.
- **Control-text hit.** Any `signals.control_text` entry forces the ask regardless of probabilities.
- **Report stops an autonomous seat and keeps its ask.** A report opens the ledger ask and, when the seat's turn is autonomous (`TurnOrigin::Autonomous` or non-root requester), stops the turn with `Kernel::interrupt_context_keeping_asks`.
- **No lowering of static asks/denies.** Static `ask` and `deny` rules are firm; the council never lowers an ask or overrides a deny.

## Where it plugs in

The evaluator is synchronous and runs while holding the kernel database lock. The council call is an async step after evaluation, outside the lock:
- **PreCall's fast path** never consults it.
- **Consumers:**
  - On RPC shell paths, in `ask_tier_ask`, before `run_permission_ask`.
  - On the `shell_write` tool path, in `run_gate` for `Origin::ShellGate`.
- **Held or open ask:** Never council-allowed.
- **Read-only shell tool & roots:** Unchanged.

## The record

Every council decision is a durable row in `council_decisions`, with child tables per read and per option/probability, recording spec id, server identity, pool, threshold, deadline, outcome, and timings. `SignalSourceKind` includes `council`.

## Council contexts are kaijutsu contexts

A council context is a kaijutsu context whose seats the council server holds as snapshots ("Council v2 seats").
- **Reviewer chain:** Composes reviewer contexts starting at the submitting seat's reviewer up to the first live root character (`council-<character>`), plus system rules (`council-system`).
- **Voting vs. observing:** Root and system contexts vote; model character contexts (like banto's) observe under `direction-check` off the hot path.
- **Building:** Built like a fork, shaped with `kj stage exclude`, and compacted.
- **Council v2 seats:** Sent whole with every decision. Pinned under the kernel while current.

### Council v2 seats
The kernel speaks council v2 (`docs/council-backend.md`). Each council context read under a spec is a queue seat: projection as messages, task message, and calls to `next_command` queue tool.
- **Pinning:** Pinned under the kernel (`kernel-<kernel id>`) for an hour. Replaced when updated; expired seats dropped.

## House rules

Enabled with `[council] house_rules = true`.
- Reads the first `AGENTS.md` found walking up from the seat's working directory, cut to `house_rules_tokens` (default 2000).
- Ephemeral on the server, shared across seats sharing the same `AGENTS.md` and model thinking settings.

## Shadow context

A shadow context is a council context holding what one seat has done to the world (outward calls in order, without results, narration, or thinking).
- **Judge vs. bump:** Judge shadows give opinions; bump shadows answer proceed or bump.
- **One child per shadow character**, child of the observed seat.
- **The gate writes the shadow** before decision and records outcome after.

## Specs and cases

The gate holds one spec per kind of case: shell spec and program spec.
- **Shell case structure:**
```json
{"command": "ls -la src", "character": "coder", "cwd": "/home/u/src/proj",
 "statements": [{"index": 0, "kind": "command", "commands": [
   {"name": "ls", "runs": "kaish builtin", "args": ["-la", "src"],
    "paths": [{"word": "src", "path": "/home/u/src/proj/src", "mount": "/home/u/src", "access": "rw"}]}]}]}
```

## Thresholds

Configured in `gate.toml`: `allow_at`, `mass_floor`, `require_agree`.

## Configuration

Configured in `/config/kernel/gate.toml`:
```toml
[council]
contexts = ["council-system"]
reviewer_contexts = true
house_rules = true
house_rules_tokens = 2000
pool = { method = "loglinear", weights = "mass" }
deadline_ms = 700

[[council.spec]]
name = "shell-gate"
case = "shell"

[[council.threshold]]
spec = "shell-gate"
allow_at = 0.98
mass_floor = -0.05

[rc.coder.council]
enabled = true
judge = "judge"
```
A context's `gate` slot (`kj character slot set <char> gate <backend>/<model>`) chooses the council server and served model.

## Servers

Speaks council v2. The kernel recomputes pooled answers from logprobs and mass (`gogi_proto::math::verify`) and checks against server within 1e-9.

## Bumper mode

`[council] mode = "bumper"` or `"bump-only"` changes council answers to pass (`proceed`) or bump (refused with guidance). Bumps open no ask until `bump_limit` or `escalate` triggers.

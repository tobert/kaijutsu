# Gate policy tuning — one layered list, one runtime path

Status: designed and shipped (`kj/gate_policy.rs`, `assets/defaults/gate.toml`, `approval_rule_families`). This document is canonical for the gate-policy evaluator; `docs/approval-identity.md` is canonical for the gate itself.

## Core principles

1. **Explicit user list overrides builtin and generated lists.** A human decision recorded in the ledger outranks everything shipped or configured.
2. **Performer layers sit on top of global.** Config composes global first, then rc directory, root, and character sections.

## The design: one evaluator, layered stores

`gate_policy::evaluate(planned_statements, ctx)` returns per-statement verdicts: Allow, Deny, or Uncovered.

- **Composition per statement:** Resolves statement key; walks layers top-down; most specific key wins; deny beats allow at equal specificity; structural refusals veto family-keyed Allow.
- **Composition per program:** AskCoverage semantics — any Deny denies submission, all Allow auto-allows, anything else escalates. Never partially applied.

At broker PreCall the evaluator runs before the hook snapshot: any statement Deny results in a `PhaseOutcome::Deny` refusal naming layer and key; every statement Allow skips hooks (replacing `program_is_gate_exempt`); anything else runs hooks.

Inside `run_gate` the evaluator replaces direct `rules::redeem`. Archived-context checks run first; auto-decisions commit durable rows naming winning layers and keys via `auto_reason`.

## Layers, bottom to top

| Layer | Store | Materialized by |
|---|---|---|
| 1. Builtin | Verb's declared `Effect` (`kj/effect.rs`) | Compile |
| 2. Global config | `/config/kernel/gate.toml`, `[global]` | Host file, seeded from `assets/defaults/gate.toml` |
| 3. Performer config | `[rc.<dir>]`, `[root]`, `[character.<name>]` | Host file |
| 4. User explicit | `approval_rules` (digest) + `approval_rule_families` | `kj ledger allow --remember` |

Top wins. A learned human allow overrides a config deny; a config deny overrides builtin allow tier.

## The key space

Format: **(command name, optional first positional token)**.

- `kj` keys resolve through six structural conditions (`is_read_only_kj`): exact name `kj`, no redirect, no background, no heredoc, arguments are `Literal` or `Plain`, and argv classifies.
- Non-kj keys are command names (`rg`) or name-plus-first-token (`git push`).

## Structural refusals veto family allows, never digest rules

A family key classifies the verb without seeing arguments. Every Allow from layers 1-3 and family rules in layer 4 inherit structural conditions: a redirect, background flag, heredoc, or non-plain argument drops the statement to Uncovered.
Exact digest-keyed rules are exceptions: the human saw exact rendered text on the answered ask.

## Verdicts

- **Allow:** No ask from evaluators; PreCall skips hooks for all-Allow programs.
- **Deny:** Refused at PreCall; names layer and key.
- **Ask:** Marked ask-tier; opens ledger ask.

A tier allow is not a capability grant; `require_cap` and loadouts run as before.

## Configuration file (`gate.toml`)

`/config/kernel/gate.toml` is seeded from `assets/defaults/gate.toml`.
```toml
[global]
allow = [
  "kj handoff note",
  "kj block create",
  "rg",
]
ask = [
  "kj rc add",
  "git push",
]
deny = [
  "dd",
]
```
Unknown sections or verdict words fail load loudly.

## Performer layers

Walked latest first (`[character.<name>]`, `[root]`, `[rc.<dir>]`, `[global]`):
- **Uncovered tier:** `uncovered = "ask" | "allow"` (defaults to `ask`). Set `allow` only for disposable environments (benchmarks, throwaway kernels). It removes human review and PreCall hooks for uncovered statements.

## Learned family rules

`approval_rule_families` table stores family-level rules learned via `kj ledger allow --remember always --family`.
- Matches by key, generalizing across arguments.
- Character rules scope rules to performers and check senpai targets (`gate_policy::serves_character`).

## Builtin tier: the verb class

Every `kj` verb declares `Effect::Read | Write | Destroy`:
- `Read` allowed by construction.
- `Write` and `Destroy` meet gate policy.
- `kj ledger` allowed as whole verb in builtin layer; `--help` handled structurally.

## Verbs decided by their target

`gate_policy::evaluate` reads literal arguments for specific verbs against the database:
- **Allows:** `kj drift push` along senpai edge; `kj context create` under lineage root MCP hub.
- **Asks:** `kj context archive` reaching consoles, `kj context demote`, `kj character set --no-root`, and character retirement of live console performers.

## Inspection

`kj ledger rules` lists composed verdicts, matching layers, and active rules for calling contexts.

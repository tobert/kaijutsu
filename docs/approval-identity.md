# Approval identity and gate policy

Every context that does work answers to one other context: its **senpai**. The senpai character reviews the work. The character performing an operation cannot approve it, from any context, except self-confirmation at a root console.

## The senpai edge and routing

`contexts.senpai_ctx` names the senpai context. It is null only for a root console. Asks climb `senpai_ctx` to the first live character reviewer:
1. Let S be K's `senpai_ctx`. If S is null (root console): when actor A is K's live root character, it is a self-confirmation; when A is not root and K's performer is live root, that root reviews it; otherwise resolution refuses.
2. A senpai with no performer is passed over.
3. The reviewer is S's character when live, not A, and not K's performer.
4. When S's character is A or K's performer, climb: K becomes S and repeat.

**Self-confirmation:** At a root console the actor is the context's own character. The ask is recorded as a self-confirmation. A model turn refuses when its performer would review itself.

**Escalation and Authority:** The performer of a context's senpai or its lineage root may move it, escalate its asks, rotate it, or archive it.

## Three identities

| Field | Meaning |
|---|---|
| `principal_id` | Authenticated requester; retained for capability and redemption scope |
| `actor_id` | Character performing this invocation |
| `reviewer_id` | Effective reviewer snapshotted on the ask |

## Gate policy layers (gate.toml)

`gate_policy::evaluate(planned_statements, ctx)` returns per-statement verdicts: Allow, Deny, or Uncovered.

### Layers, bottom to top
1. **Builtin**: Verb's declared `Effect` (`Read`, `Write`, `Destroy`).
2. **Global config**: `/config/kernel/gate.toml`, `[global]`.
3. **Performer config**: `[rc.<dir>]`, `[root]`, `[character.<name>]`. Narrowest decides.
4. **User explicit**: Learned rules (`approval_rules` and `approval_rule_families`) via `kj ledger allow --remember`.

Top wins. A learned human allow overrides config deny. Structural conditions (redirects, background flags, heredocs) drop family-keyed allows to Uncovered, whereas exact digest-keyed rules override them. Verbs decided by their target check literal arguments against the database (e.g., `kj drift push`, `kj context archive`, `kj context demote`, `kj character set --no-root`, and character retirement).

## The council gate and Council v2 backend

When enabled in `gate.toml`, the gate consults the council for statements left Uncovered by static rules. A context whose `gate` slot names a chat model is judged by the guardian instead (below).

### The flow
- **Allow**: Pooled probability $p(\text{allow}) \ge \text{allow\_at}$, mass floor met, agreement met, no control-text hit. Uncovered statements become council allows; submission runs with a durable auto-decision row naming the council.
- **Ask**: Opens ledger ask with the council's answer attached.
- **Report**: Opens ledger ask plus a council report event; stops autonomous turns with `Kernel::interrupt_context_keeping_asks`.
- **Miss**: Leaves submission where it is if council is down, slow, or low mass.

### Council contexts and backend
- **Council contexts**: `council-<character>` and `council-system` along the reviewer chain. Root and system contexts vote; character contexts observe.
- **Council v2 backend (`kaijutsu-council`)**: Speaks Council v2 (`gogi-proto`). The kernel pins council seats under `kernel-<kernel_id>`, reads `/council/v2/identity`, streams `/council/v2/generations` for chat turns, and sends control vectors and CAS media objects.
- **House rules**: Enabled via `[council] house_rules = true`, reading the first `AGENTS.md` up from working directory.
- **Judge shadows & Bumper mode**: Judge shadows give opinions; bump shadows answer proceed or bump in **bumper mode** (`[council] mode = "bumper"` or `"bump-only"`).
- **Decisions backend (`kind = decisions`)**: A hosted Decisions API, OpenRouter's (`https://openrouter.ai/api/alpha/decisions`, models `typesafe/jev-1.13` and `~typesafe/jev-latest`) or Gemini's (`https://generativelanguage.googleapis.com/v1beta/decisions`, model `gemini-decision-1`). It takes state plus a `choice` question and returns each option's probability (`llm/decisions.rs`). It runs no chat turn, so only a `gate` slot may name it; a context whose model is on one is refused at create.

### The guardian

`kj character slot set sol gate chatgpt/gpt-6.2-sol`, `[rc.sol.guardian] enabled = true` in `gate.toml`, then `kj context rotate`: a chat model judges what the rules escalate.

The kind of backend a context's `gate` slot names picks who judges. A `council` backend runs the council above. A chat backend (`anthropic`, `deepseek`, `openai`, `chatgpt`, `alibaba`, `gemini`) runs the guardian when the performer's sections enable it (`[rc.<dir>.guardian]`, `[root.guardian]`, `[character.<name>.guardian]`; the last switch decides). The guardian is modeled on OpenAI Codex's auto-review.

- **The call.** One model call on the slot, bounded by `[guardian] deadline_ms` (default 30000). The system text is `/config/kernel/council/guardian.md` and the answer format. The user message holds the workspace's `AGENTS.md`, the tail of the context's transcript (user and model text, tool calls and results; `transcript_tokens`, default 8000), and the planned action as the council's shell case reads it, with the text of each program it runs.
- **The answer.** One JSON object, `{outcome, risk_level, user_authorization, rationale}`, parsed strictly. Anything else is a miss. No provider client sends a JSON schema yet, so the format is asked for in the prompt.
- **Outcomes.** `allow` runs the submission with an auto-decision row naming the guardian's model. `deny` asks, with the rationale in the ask. `deny` at `critical` risk reports. An `allow` at `critical` risk asks. A timeout, a transport error, or an unparseable answer is a miss.
- **Circuit breaker.** After 3 denials in a row, or 10 of the last 50, the guardian stops judging that context until the kernel restarts; each later submission is a miss naming why, so it asks the senpai.
- **Record.** Each decision is a `council_decisions` row with spec name `guardian`; the model's own answer is a `council_assessments` row beside it.

## Standing rules for a character

`kj ledger allow <request-id> --remember character [--family]` binds a rule to the character performing the ask. Scope `session` covers the context and requester; scope `always` covers everyone.

## Continuation windows and async work

A **continuation window** governs automatic model resumption after yielding (default 30 minutes after the last provider inference request). `kj handoff signoff <note>` closes it immediately. `kj interrupt <target>` stops the loop from outside. Async shell operations return a stable receipt naming the operation and approval dependency.

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

When enabled in `gate.toml`, the gate consults the council for statements left Uncovered by static rules. A context whose `gate` slot names a chat model or a `decisions` backend is judged by a model judge instead (below).

### The flow
- **Allow**: Pooled probability $p(\text{allow}) \ge \text{allow\_at}$, mass floor met, agreement met, no control-text hit. Uncovered statements become council allows; submission runs with a durable auto-decision row naming the council.
- **Ask**: Opens ledger ask with the council's answer attached.
- **Report**: Opens ledger ask plus a council report event; stops autonomous turns with `Kernel::interrupt_context_keeping_asks`.
- **Miss**: Leaves submission where it is if council is down, slow, or low mass.

### Council contexts and backend
- **Council contexts**: `council-<character>` and `council-system` along the reviewer chain. Root and system contexts vote; character contexts observe.
- **Council v2 backend (`kaijutsu-council`)**: Speaks Council v2 (`gogi-proto`). The kernel pins council seats under `kernel-<kernel_id>`, reads `/council/v2/identity`, streams `/council/v2/generations` for chat turns, and sends control vectors and CAS media objects.
- **House rules**: Enabled via `[council] house_rules = true`, reading the first `AGENTS.md` up from working directory.
- **Judge shadows and bump questions**: Council judge shadows give opinions. In a bumper mode, council specs can ask whether to proceed or bump.
- **Decisions backend (`kind = decisions`)**: A hosted Decisions API, OpenRouter's (`https://openrouter.ai/api/alpha/decisions`, models `typesafe/jev-1.13` and `~typesafe/jev-latest`) or Gemini's (`https://generativelanguage.googleapis.com/v1beta/decisions`, model `gemini-decision-1`). It takes state plus a `choice` question and returns each option's probability (`llm/decisions.rs`). It runs no chat turn, so only a `gate` slot may name it; a context whose model is on one is refused at create. On a `gate` slot it is the decisions judge (below).

### The guardian

`kj character slot set sol gate chatgpt/gpt-6.2-sol`, `[rc.sol.guardian] enabled = true` in `gate.toml`, then `kj context rotate`: a chat model judges what the rules escalate.

The kind of backend a context's `gate` slot names picks who judges. The two model judges run only when the performer's sections turn on the guardian switch (`[rc.<dir>.guardian]`, `[root.guardian]`, `[character.<name>.guardian]`; the last switch decides). With the switch off, the council judges when enabled. The guardian is modeled on OpenAI Codex's auto-review.

| `gate` slot backend | Judge | It answers | Recorded spec |
|---|---|---|---|
| `council` | The council (above) | Pooled probabilities from council contexts | the council spec's name |
| A chat kind: `anthropic`, `deepseek`, `openai`, `chatgpt`, `alibaba`, `gemini` | The guardian | One JSON object: outcome, risk, authorization, rationale | `guardian` |
| `decisions` | The decisions judge | p(allow), p(ask), p(report) for one `verdict` question | `decisions` |

Both model judges read the same evidence and share the `[guardian]` provider settings. The gate acts on outcomes from any judge. A miss never allows.

- **Bumper policy.** Set `[judge] mode = "bumper"` to refuse a bump without an ask and give the model guidance from `/config/kernel/council/bump.md`. After `bump_limit` refusals of the same submission (default 3), its next bump opens an ask. `mode = "gatekeeper"` asks instead of bumping. `mode = "bump-only"` removes the per-submission limit. Council specs in bump-only mode also use their existing fallback bumps for unread cases and control text; model-judge reports and misses still take their own paths.
- **Bump streak.** Set `escalate = { bumps = 3, minutes = 30 }` under `[judge]` to ask on the third would-be bump within 30 minutes, including bumps of different submissions. Only refused bumps count (`seat_bump` in the ledger); the ask ends the streak. History survives a restart and the judge keeps judging. Without `[judge]`, these are the model judges' defaults, and the Council v2 default is gatekeeper. An explicit `[judge]` applies to all judges; omit `escalate` there to disable streak escalation.
- **Host config.** After deploying a kernel that reads `[judge]`, move `mode`, `bump_limit`, and `escalate` out of `[council]` together if you want one explicit policy for all judges. Do not set them in both tables. Until then, existing `[council]` keys apply only to Council v2 and do not change model-judge defaults. Update the live `council/bump.md` from the shipped default, or preserve local wording while removing any advice to reword the same action.

- **The call.** One model call on the slot, bounded by `[guardian] deadline_ms` (default 30000). The system text is `/config/kernel/council/guardian.md` and the answer format; a missing or empty policy is a miss, and `kj config reset council/guardian.md` restores it. The user message holds the workspace's `AGENTS.md`, the context's bounded transcript (`transcript_tokens`, default 8000), and the planned action as the council's shell case reads it, with the text of each program it runs. Policy and prompt together stay under 256 KiB, or the call is a miss.
- **The transcript.** The turn's opening user prompt and the latest user instruction the senpai wrote in this context come before the recent blocks. Neither repeats in the tail. If the two entries cannot fit within the transcript budget, the judge misses. Each block is one JSON line with `kind` (`user`, `assistant`, `tool_call`, `tool_result`, `drift`), `author`, `trusted`, and `text`. The first entries also name their `purpose`. Only a `user` block someone other than the performer wrote is trusted; a body cannot forge a line.
- **Programs.** Every program the submission runs is shown whole, or no judge is asked: an unread program, a text over 16 KiB, or more than 8 programs is a miss. A file the judge read is pinned by its hash, and a submission whose file changed before it runs is refused.
- **The answer.** One JSON object, `{outcome, risk_level, user_authorization, rationale}`, parsed strictly. Anything else is a miss. No provider client sends a JSON schema yet, so the format is asked for in the prompt.
- **Outcomes.** `allow` runs the submission with an auto-decision row naming the guardian's model. `deny` below `critical` risk bumps with the rationale, or asks with the same rationale in gatekeeper mode or after escalation. `deny` at `critical` risk reports. An `allow` the policy's outcome rule forbids asks: at `critical` risk, or at `high` risk with `unknown` or `low` authorization. A timeout, a transport error, or an unparseable answer is a miss.
- **The decisions judge.** One Decisions call on the slot, under the same deadline. Its state is the guardian's user message; its `verdict` question's instructions are the policy and a line mapping the policy's outcome to the options. It reports when `report` is the most likely verdict (ties included). Otherwise it allows only when the judge chooses `allow` and p(allow) reaches `[guardian] allow_at` (default 0.9). In all other cases it bumps with the three probabilities, or asks if the gate policy requires an ask. A provider error, a timeout, a transport error, or a reply that does not match the question is a miss.
- **Record.** Each decision is a `council_decisions` row with the spec name in the table. The guardian's answer is a `council_assessments` row beside it; the decisions judge's probabilities are the decision's pooled `verdict` rows.

## Denying an ask

`kj ledger deny <id> --reason "Use the generated report instead."` denies the
ask and includes the reason in the denial the model reads. `kj ledger show <id>`
shows the recorded reason. Omit `--reason` to deny without one. With `--remember`,
the reason belongs to this decision, not to future denials by the rule.

## Standing rules for a character

`kj ledger allow <request-id> --remember character [--family]` binds a rule to the character performing the ask. Scope `session` covers the context and requester; scope `always` covers everyone.

## Continuation windows and async work

A **continuation window** governs automatic model resumption after yielding (default 30 minutes after the last provider inference request). `kj handoff signoff <note>` closes it immediately. `kj interrupt <target>` stops the loop from outside. Async shell operations return a stable receipt naming the operation and approval dependency.

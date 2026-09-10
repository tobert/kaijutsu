# Prompt review results

Local synthesis of the two bounded reviews run per `prompt-review-plan.md`,
plus executor feedback. Raw artifacts: `~/exomemory/kaijutsu/prompt-review-2026-09-10/`

Codex follow-up checked the synthesis against both raw answers, corrected
finding attribution and overstated agreement, and traced stance restoration
through the lifecycle loader and shipped scripts. The related
[Kaibo prompt proposal](kaibo-prompt-proposal.md) uses these cases as evidence
for improving review prompts. Proposed remedies below remain implementation
decisions unless explicitly attributed to Amy.

| Review | Cast | Explorer and synthesis model | Revision snapshot | Result |
|---|---|---|---|---|
| DeepSeek | `review-ds4` | `deepseek-flash` (provider alias; resolved id not returned in JSON) | `c07b5f18`, clean | complete, no warnings |
| GLM | `review-glm` | `zai-org/GLM-5.3` (Crusoe) | `c7a4f6b6`, clean | complete, no warnings |

`c07b5f18` and `c7a4f6b6` are both descendants of `3deb6601`. A local
`git diff --name-only` between them lists only `docs/gate-policy-tuning.md`.
The prompt-related source and attachments are unchanged across the recorded
snapshots.

Local verification actually performed: read all seven attachments; read
`build_distillation_prompt`, `summarize_with_model_for_caller`,
`block_ids_ordered`/`blocks_ordered`, the hydrate filter, and `fork_compact`;
inspected drift and fork arguments (the executor's claimed `push --focus`
precedent was corrected in the Claude follow-up below); listed
`assets/defaults/rc/coder/fork/`; regenerated the comparison artifact and
restored it. No Rust build, no kernel operation, no reseed.

## Accepted findings

**A1. The distillation transcript ignores the block-store filters the
conversation respects.** (both reviewers; verified)
`summarize_with_model_for_caller` feeds `block_snapshots`
(`kj/mod.rs:801-811`) straight into `build_distillation_prompt`, and
`block_ids_ordered` (`blocks/block_store.rs:213-222`) filters only deleted
blocks. Hydration skips `Status::Draft`, `excluded`, `File`/`Trace` kinds,
and non-allowlisted `System` role blocks (`llm/hydrate.rs:133-150`). So
`kj stage exclude` followed by `kj fork --compact` still sends the excluded
poison to the summarizer, a mid-sentence compose draft is summarized as if
submitted, and every context's full system sections (stance, kaish primer)
ride along as `**System**:` text. This path does not honor `excluded`; the
review did not establish that every other path does. The accessor exists,
but selecting the complete summarizer filter set needs its own tests.

**A2. The 2,000-byte head cut has no provenance and no total bound.** (both
reviewers; tracked in `docs/issues.md`; verified)
`drift.rs:1421-1434` keeps the first 2,000 bytes of each nonempty block,
names only the original size, and emits no block identifier
(`drift.rs:1436-1439`). A decision beyond the cut is unrecoverable, the
model is instructed to preserve identifiers it was never given, and many
2,000-byte blocks make one unbounded request. The drafts correctly refuse to
claim wording fixes this (`prompt-proposals.md:201-203`).

**A3. No rc configuration seam for a distillation instruction.** (ds4 #1,
glm #3; verified)
`DISTILLATION_SYSTEM_PROMPT` is `include_str!` (`drift.rs:1378-1379`) and
sent at `kj/mod.rs:907`. Editing `assets/defaults/prompts/distillation.md`
does nothing until a rebuild; the "ordinary host files, read every run"
stance does not hold for this file today, and no mechanism carries a
per-operation length value. The executor's preferred remedy, consistent
with GLM's proposal and the plan's "no new registry or RPC": resolve per-operation
config from the existing context-scoped mechanisms, keep the compiled
instruction stable, and have the formatter append one budget line. The
existing directed-prompt precedent supports the split: `kj drift pull`
accepts trailing positional prompt text, joins it, and supplies it to the
formatter, which appends it after the transcript. There is no `kj drift push
--focus` flag. Length guidance could belong to the operation the same way.
DeepSeek instead proposed reading the prompt body from the
config tree with an embedded fallback. The reviewers agree on the missing
configuration mechanism, not on its implementation.

**A4. An invalid budget should fail loudly at the operation boundary.**
(ds4 #1, glm #3; proposed behavior)
GLM proposes absent → no budget line; DeepSeek proposes absent → default.
The shipped default and absence policy remain implementation decisions. Invalid
(non-numeric, zero, negative) → the distillation call fails with the bad
value named. Validate when the operation consumes the value. DeepSeek #4
correctly notes that `fork_compact` passes `None` as its directed focus and
that `--prompt` is handled after summarization; it does not claim that fork
has a `--focus` flag.

**A5. Revision pinning makes the artifact self-inconsistent.** (ds4 #6;
reproduced)
Running `python3 contrib/render-prompt-comparison.py` at the committed state
modified `docs/prompt-comparison.html`: the embedded `git rev-parse HEAD`
(`render-prompt-comparison.py:82`) is the parent commit, never the commit
containing the artifact. A regenerate-and-diff check therefore changes that
field after each commit. The seed sha256 hashes (`:83`) are still useful.
Remedy: distinguish the checkout revision at capture from source identity,
and exclude capture metadata from a freshness check, or remove it from the
deterministic render. A second commit does not solve self-reference. The
existing field can describe the checkout at capture without claiming to be
the artifact's containing commit; its presentation needs that distinction.

Implemented during the dark-mode update: removed the HEAD field, included
draft/template/generator hashes alongside seed hashes, and added `--check`
to compare a fresh deterministic render without writing. The check failed
on stale HTML and passed after regeneration. The original review observation
above describes the earlier artifact.

**A6. Both coder drafts repeat one shared sentence.** (executor,
independent; glm reached the adjacent observation)
"Treat unexpected edits as another player's work. Coordinate…" appears in
both focused and guided drafts (`prompt-proposals.md:73-74,98-99`). These
are alternative role bodies, so that repetition does not duplicate the
sentence within one request. By the
dossier's own layer table (`oss-comparisons.md:479-481`), collaboration
belongs in the base and roles keep coding procedure. Decide whether musician and MCP contexts
should also carry it before moving it.

## Rejected or corrected

**R1. ds4 #6 "base/role duplication."** Rejected as written. Its comparison
of "Distinguish observations, inferences, and unknowns" points to the current
`S00-stance.kai` core, which the drafts replace;
the proposed focused/guided bodies do not repeat the base. The real residual
duplication is A6.

**R2. glm #4 "child stance restoration is already handled."** Rejected.
`fork_compact` does run the rc fork lifecycle on the child
(`kj/fork.rs:1030-1044`), but the fork verb carries only `S30-cache.kai` and
`S40-datetime.kai` — the stance exists solely under `create/`. The
compact-fork child is created without a stance block, so the draft's open
question ("How should compact forks restore stance…") is genuinely open; the
fork lifecycle is the seam, not the fix.

**R3. ds4 #3's textual-role/provenance concern.** Source identifiers are
missing, as A2 records. A model-authored paragraph can also resemble the
formatter's role headings. That is an ambiguity in summarizer input, not a
demonstrated change to provider message authority. Source labels improve
traceability; treating summarized content as source material and preserving
real block metadata matter too. No adversarial runtime test was run.

**R4. glm #6 "generator integrity is good."** Accepted for extraction and
escaping. A5 identifies a separate freshness-check problem in the capture
metadata; distinguish that from corruption of the compared prompt text.

## Sound axes (reviewers declined to invent findings)

- Drift briefing vs continuation separation: the embedded-request refusal
  (`prompt-proposals.md:144-145`), explicit-stop preservation and the
  status-question distinction (`:166-167`), and deference to later steering
  (`:189-190`) address the Hermes wrapper trap. GLM judges this axis sound.
  DeepSeek's request to repeat the steering rule in the continuation draft
  overlooks text already there; its observation that runtime wiring is
  missing remains valid.
- No contradiction found between the draft base and the coder drafts beyond
  A6; the seed's "fork without risk" claim is fixed precisely by both
  drafts.

## Executor feedback on the prompt prose

From direct experience as a model playing these surfaces:

1. **Name the owner of every runtime-injected line.** The drafts say
   "Follow the requested focus and length guidance when supplied" without
   saying who supplies it or where it lands. The formatter already appends
   the focus line after the transcript; state in the drafting docs that the
   formatter injects both, so prompt prose and formatter output stay one
   contract instead of a hope.
2. **Positive form beats a negation.** "A question about progress does not
   cancel unfinished work" (`prompt-proposals.md:36-37`) states the
   exception, not the behavior. "Answer the status question, then continue
   the unfinished work unless the user redirects" is executable without
   inference, especially by guided-tier models.
3. **The handoff template can echo its own instructions.** In the compact
   fork handoff draft, each `##` section heading is followed by
   imperative lines that are guidance *about* the section; a summarizer can
   copy them into the output as content. Worth rendering the skeleton
   explicitly (section names in one list, guidance separated) and testing on
   a small model before adoption.
4. **Dropped formatting guidance deserves one sentence of decision.** The
   current seed's "Format as a briefing, not a transcript. Use bullet
   points" disappears from the drift draft. If intentional (structure is
   the recipient's problem), say so; otherwise keep the bullet line.
5. **Base cost should be argued against the cached prefix.** The base rides
   a stable prompt prefix that varies per call only in `<situation>`; its
   cost depends on actual cache hits, provider accounting, and the complete
   request. A stable prefix does not imply free input on later turns or
   remove its context occupancy. Measure cold and cached requests separately;
   prose bytes are not tokens.

## Suggested implementation order

The executor recommends the order below. GLM puts filtering first; DeepSeek
puts length configuration first. This is a local recommendation, not reviewer
consensus; the local adjustments are bolded:

1. Summarizer filter set (A1) — smallest, pure, makes every later
   distillation experiment trustworthy. **Do this before any prompt-text
   change: the new drafts' honesty rules ("do not treat missing information
   as a negative result") are untestable while excluded blocks ride along.**
2. Transcript provenance and bounded reduction (A2) — block identifiers in
   labels, then replace the head cut; fail-first fixtures per
   `docs/issues.md`.
3. Budget plumbing (A3, A4) — rc/context-scoped value, one appended line,
   loud invalid, silent absent. Independent of 2 if the value is per-context.
4. Handoff wiring (sound-axes finding) — separate continuation prompt behind
   `fork --compact`, **including the child-stance gap (R2)**, with the
   dossier's continuity fixture set (`oss-comparisons.md:518-523`).
5. Prompt text adoption (drafts, base cost, A6) last, measured by the
   capture experiment (`oss-comparisons.md:509`).

Fail-first tests named across the reviews: excluded and draft blocks absent
from the built prompt; a correction placed beyond byte 2,000 survives
distillation; Unicode-boundary cuts stay valid; invalid budget errors rather
than defaults; absent budget renders no line; repeated compact forks keep
unresolved requests, pending questions, and failed checks distinct from
successes.

## For Amy and the Claude Code review

- Where should the shared collaboration sentence live (A6): base for all
  types, or stay duplicated in coder variants?
- Does dropping bullet formatting from the drift briefing matter to you, or
  is structure the recipient's problem (feedback item 4)?
- Is the rc budget a word target only, or should the formatter also carry a
  hard input bound for the transcript itself (A2's total-bound gap is
  separate from any word target)?

## Claude Code follow-up

The separate Claude review pins its source citations to `14589559` and marks
its working file ephemeral, not for commit. It confirms the filter,
provenance, total-bound, and compact-fork initialization findings. Codex
checked its correction to the directed-prompt interface against
`kj/drift.rs`: the existing input is trailing text on `pull`, not a `--focus`
flag on `push`. A3 above now reflects that source.

Its strongest additional design concern is the global base's working
contract. The shipped musician stance requires ABC-only output on a phrase
deadline. Asking every type to investigate, ask questions, and write project
handoffs can conflict with that output contract. Proposed remedy: keep the
universal base small and compose the working contract through shared rc only
for types that need it. Source establishes the conflicting instructions;
their behavioral cost has not been measured.

Other proposals to carry into implementation planning: unify the coder
variants' fork wording; inspect reuse of `plan_splice` for a compact fork's
recent complete turn groups; load the auxiliary prompt body through existing
host-file configuration; and keep a default length policy in one owner.
These are proposals, not newly shipped behavior. The private Site's first
version predates this follow-up and is a snapshot of the earlier documents.

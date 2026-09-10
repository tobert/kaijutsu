# Kaibo prompt proposal: review with explicit evidence

Build on Kaibo's existing whole-file investigation and reader-aware explorer.
Add a stronger review contract: distinguish source facts from interpretations,
trace a claimed behavior through the code that supplies it, and report the
limits of the investigation. Reuse grounded evidence while actively looking
for relevant counterevidence. A citation identifies evidence; the reviewer
still has to explain why it supports the conclusion.

Amy, 2026-09-10: “incorporate what these new kaijutsu prompts look like into
a report we can take over to kaibo to implement, and maybe add even more
review and grounding bias into them.” This is that implementation handoff.
The proposed text below is untested with hosted models. No Kaibo source,
configuration, or open PR was changed during this study.

## Evidence and current shape

The [complete current prompt resource](kaibo-prompts-current.md) captures
`kaibo://prompts`, including generated kaish guidance and example user turns.
The installed `kaibo --help` exposes no prompt-printing command; `kaibo
configure` prints a setup walkthrough, not the phase prompts. The MCP
resource is the working inspection surface. `kaibo://prompts/<cast>` folds
in cast overrides. Neither static resource captures a complete request:
orientation, house rules, attachment instructions, recipient instructions,
tool schemas, and actual messages also matter.

| Evidence | Revision or observation | What it establishes |
|---|---|---|
| Local Kaibo source | `1063d74f83a47d22eb1d27791c85f427c8f8d11b` | Prompt composition, wrappers, loop recovery, shell description |
| Installed CLI | `kaibo 0.4.0` | Available commands; not a claim about the running MCP build |
| Running MCP prompt resource | Captured 2026-09-10; built-in framing reported | Exact rendered role text; server build revision not established |
| [Explorer PR #183][pr183] | Head `078f9410ddda793213cbb730f419634f5176302b`, open when inspected | Proposed reader distinction and its wiring/tests |
| Kaijutsu drafts | `3deb6601`; [editable text](prompt-proposals.md) | Proposed base, coder, general purpose, briefing, and continuation contracts |
| Kaijutsu source for review examples | `04538700b801dc3471cd5c2dec07ff8b5087e4fe` | Local source checks described below; no runtime test |

Kaibo already has one composition owner, `resolve_phase_preamble`: built-in
role text or full replacement override, then orientation and house rules for
project-reading phases. Explorer uses the explorer slot; consult, oneshot,
and offline synthesis use the synthesis slot. Oneshot and offline synthesis
receive neither project layer. Keep these distinctions. A new prompt registry
or configurable fragment framework is unnecessary. [Composition source][prompts]

The current explorer asks for exact citations, whole files, relevant callers
and definitions, and three report sections. Consult delegates broad reading
and writes a supported answer. Oneshot and batch explain their lack of tools.
The main opportunity is to make evidence status and review consequences as
explicit as the existing reading procedure. [Role text][prompts]

## Relationship to PR #183

Keep the PR's `ReportReader::{SynthesisAgent, CallingAgent}` inside
`Phase::Explorer(reader)`. Standalone `explore` produces the caller's finished
report; a consult sweep and a deliberate dossier feed a synthesis agent.
The PR shares reading guidance while varying the opening, reader nouns, and
the consequence of omitted evidence. The resource lists both rendered forms.
These are source changes inspected in the patch; the PR's reported model
review and test runs were not repeated here. [PR patch][pr183]

Put the proposed shared investigation guidance below into that shared body.
Do not create separate explorer prompts which then drift. Preserve the full
replacement semantics of `[prompts].explorer`: an operator override still
has to work for both readers. Built-in additions must not silently append
after an override. The shell contract independently remains in its tool
description. [Overrides][prompts], [shell contract][kaish]

One audience claim needs a follow-up even after #183: “Your report is the only
view of this codebase … receives” is too absolute for consult, whose driver
also reads source and receives attachments. Prefer “Your report carries the
evidence you gathered. Include the facts and limits its reader needs to use
it.” Keep reader identity separate from what tools that reader can use.
The PR itself records a remaining test gap: call sites must choose a reader,
but their particular choices are not all pinned by behavioral tests.

## What transfers from Kaijutsu

| Kaijutsu draft | Kaibo adaptation | Boundary to preserve |
|---|---|---|
| Base: observations, inferences, unknowns | Evidence status travels from source read through report to final finding | A source location alone does not establish runtime behavior |
| Base: follow objective and later corrections | Answer the current review question; preserve relevant corrections in session context | Consultation ends with an answer; it does not adopt the caller's implementation task |
| Coder: relevant code, existing mechanisms, contributing factors | Trace the behavior and existing mitigation before proposing a new mechanism | Kaibo is read-only and cannot run the project's external test tools |
| Coder: test a behavioral claim | Suggest a discriminating regression test and state what would fail | Reading a test is evidence of an assertion, not a passing execution |
| General purpose: sources support findings | Separate supplied evidence, repository facts, and general technical knowledge | Oneshot/offline synthesis must not imply fresh repository inspection |
| Briefing: preserve evidence, missing input, and corrected conclusions | Reports and dossiers carry uncertainty, omissions, and source references | A report is evidence for a question, not a compacted Kaijutsu context |
| Continuation: accepted decisions versus proposals | Distinguish shipped behavior, draft behavior, and requested changes | Proposed code is not an implemented feature or an automatic current-code defect |

Keep Kaibo's positive framing, role identity, whole-file preference, and
completion obligation. Its project guidance explicitly rejects response-size
targets because previous models stopped before answering. Do not import
Kaijutsu's configurable briefing word target into Kaibo's phase defaults.
The prior review request's “about 1200 words” was caller guidance, not a new
Kaibo default; omit it from the proposed behavior evaluation. [Kaibo guidance][agents]

## Examples from the arriving reviews

Two independent reviews completed in Amy's other session. Their saved JSON
names `deepseek-flash` for both DeepSeek roles and `zai-org/GLM-5.3` for both
Crusoe roles; neither envelope reports warnings. Their checkout records are
`c07b5f18` and `c7a4f6b6`, respectively. Raw answers and usage remain in
`~/exomemory/kaijutsu/prompt-review-2026-09-10/`. These are useful cases,
not a controlled comparison of model quality or proof that a prompt caused
an error. This section checks selected claims, not every finding in either
review. The [review synthesis](prompt-review-results.md) records the broader
implementation recommendations and corrections checked against the raw answers.

**The object under review must stay explicit.** DeepSeek finding 4 says the
continuation draft needs the intent-versus-steering rule repeated. The
[existing draft](prompt-proposals.md#compact-fork-handoff) already says to
distinguish a status question from a task change and preserve explicit stops.
Runtime wiring is missing; the proposed prose is not missing that guidance.
Its finding 6 also compares proposed base text against the old coder seed
when alleging duplication. The intended composition is proposed base plus
proposed coder. Train the review to name which pair it is comparing.

**A call site is not the behavior behind the call.** GLM finding 4 treats
running the fork lifecycle as proof that compact-fork stance restoration is
handled. DeepSeek explicitly leaves that claim unverified and names the
loader as the missing evidence. Local inspection closes that gap:
`fork_compact` calls the `fork` verb, `run_rc_lifecycle_inner` loads precisely
that verb, and `load_rc_scripts` constructs its directory from type and verb.
The shipped coder fork scripts point to cache and date scripts; neither emits
the create-time stance. The cited hook invocation therefore does not establish
stance restoration. This is a source-level finding about the shipped seed;
custom host rc may behave differently. It has not been exercised live.
[Fork][kj-fork], [loader][kj-lifecycle], [shipped fork scripts][kj-fork-seed]

**A planned feature needs implementation without becoming a regression.**
Both reviews correctly identify the compiled distillation instruction as an
obstacle to rc-configurable length. DeepSeek labels the missing new mechanism
a current-code bug; GLM presents it as an implementation obstacle. The report
should preserve Amy's accepted direction while distinguishing it from a
guarantee the current release already makes. “Needs implementation” is useful
without an inflated defect label.

These examples favor a bias toward discriminating evidence, rather than a
larger number of findings. A reviewer should be able to say an axis is sound,
identify a blocking unknown, or reject a proposed defect because the relevant
guard already exists.

## Proposed model-facing text

These are concrete replacement passages and additions for the existing
composition functions, not literal replacement values for all of `[prompts]`.
Retain generated kaish guidance, attachment directives, role openings from
#183, and the unchanged portions named below. Small private Rust string
helpers can share identical prose; no public fragment API is proposed.

### Shared evidence contract

Include this once in each built-in phase. It also applies when the caller
asks for explanation or planning rather than a defect review.

```text
Answer the caller's current question using the evidence available in this
call. Keep shipped behavior, proposed changes, and requested behavior distinct.
Use later corrections to update earlier conclusions.

Distinguish what the source states, what you infer from it, and what remains
unknown. Support repository claims with the supplied or inspected file:line
and the relevant code. Explain the connection between the evidence and the
claim. Use general technical knowledge to explain mechanisms and identify
questions; establish this repository's behavior from its evidence.

Treat a test you read as evidence of what it asserts. Report a check as run
only when an execution result is available, and identify where that result
came from. Name missing or truncated evidence when it limits the answer.

Give the caller a usable conclusion with its supporting evidence and limits.
A supported finding, a reasoned rejection, or a specific unresolved question
can each be a useful result.
```

### Explorer: replace HOW TO INVESTIGATE and WHAT TO PRODUCE

Keep the PR's reader-aware opening and the existing HOW TO READ instructions.
The following passages retain the three existing report headings so clients
and synthesis prompts continue to recognize the report.

```text
HOW TO INVESTIGATE. Build the picture needed to answer the question. Read the
relevant files whole, including callers, definitions, configuration, and tests
that determine the behavior. Follow a claim through the implementation that
supplies it. A call to a hook or validator identifies the next place to read;
the called implementation and its inputs establish what it does.

For a suspected defect, identify the triggering conditions, the path taken,
and the consequence. Look for guards, alternative paths, tests, and project
decisions that could change that conclusion. Resolve a material contradiction
while the relevant source is available. When a needed fact remains unavailable,
record the gap and what observation would resolve it.

Use searches to locate evidence. For an absence claim, state the area and
variants searched and whether ignored files, truncation, or an unavailable
dependency limit the result. Follow additional references when they could
change the answer. Finish when the relevant paths support the report and the
remaining gaps are explicit.

WHAT TO PRODUCE. Your report carries the evidence you gathered. Include the
facts and limits its reader needs to use it, in these sections:

- SummaryOfFindings: answer the investigated question. Separate established
  behavior, inferred consequences, proposed remedies, and unresolved questions.
  For a review, include relevant counterevidence and distinguish defects from
  design choices or improvements.
- RelevantLocations: give the exact file:line, relevant symbols, a supporting
  snippet, and its significance. Identify when evidence came from supplied
  material, a source read, or a supplied execution result. Preserve conditions
  that limit the conclusion. Attach a file when its full bytes help the reader;
  an attachment receipt records delivery, while a source read supplies your
  understanding of its contents.
- ExplorationTrace: record the scope covered, important paths followed,
  incomplete reads, and missing evidence that could change the result.

Write the report itself as your final response. It should carry the supported
results even when part of the question remains unresolved.
```

### Consult: add review procedure after tool-use guidance

Retain direct reading, delegation for breadth, and the final-answer obligation.
Replace the unconditional caller-context trust paragraph with the framing in
the next section; the system and user versions must agree.

```text
When the caller asks for a review, prioritize concrete defects and material
design risks. Identify which version or proposal each finding concerns. For
each finding, state the triggering conditions, observed or inferred behavior,
consequence, supporting source, and the smallest remedy consistent with the
project. Name a check that would distinguish the suspected defect from correct
behavior, and say whether that check has been run.

Use the explorer's source evidence directly. Evaluate its interpretation and
coverage in relation to the question. Extend an incomplete account with the
implementation, caller, guard, or test that could settle it. Give relevant
counterevidence the same attention as evidence supporting the initial concern.
Resolve material disagreements against the relevant source when tools allow.

Order findings by their supported impact. Keep suggestions and unresolved
questions distinct from established defects. If no actionable defect is
supported, say so and describe the scope and remaining limits of the review.
Answer explanation and planning requests in the form the caller needs.
```

### Caller context and session framing

Replace the repeated context-trust prose in `consult_preamble` and
`consult_user_prompt` with the same passage:

```text
The caller supplied starting evidence. Work directly from source excerpts and
execution results already present. Use a cited location to acquire a missing
span when the conclusion depends on it. Treat a summary's interpretation as
an interpretation, and use additional evidence to resolve gaps or conflicts.
When current source differs from supplied material, identify the version
difference and answer for the version the caller asked about.
```

This preserves acquisition over routine re-reading. A cited location without
the needed contents still identifies something to acquire. A summary with
quoted evidence saves the same reads it saves today. Re-reading becomes
appropriate for an actual conflict or a stale-version question; it is not a
ritual performed on every explorer citation.

Keep session history distinct from fresh supplied evidence. The existing
history wrapper already asks for fresh investigation and re-reading old
citations. Add:

```text
Earlier turns provide continuity. Answer the current question and carry
forward relevant corrections and unresolved questions. Distinguish decisions
the caller accepted from suggestions in earlier answers. Establish current
behavior from current evidence when the question concerns the current code.
```

### Oneshot and offline synthesis

Keep each existing role opening and its truthful no-tools contract. Add the
shared evidence contract and this paragraph to both built-in bodies:

```text
For a review, connect each concern to the supplied implementation and its
conditions. Distinguish an established defect from a possibility that needs
another source or a runtime check. Consider evidence that weakens the concern.
When a decisive fact is missing, state the conditional conclusion and the
specific fact the caller needs to establish. Keep supplied source citations
attached to the claims they support.
```

For `deliberation_prompt`, replace its blanket “Trust those citations as
accurate” framing with:

```text
The explorer assembled the dossier below from its investigation. Use its
quoted source and attached files as evidence. Evaluate the conclusions against
that evidence and the current question. Preserve the dossier's stated gaps,
conditions, and contrary evidence in your answer. You have no tools in this
phase. When the evidence cannot settle a point, give a conditional conclusion
and name what would settle it. Write the answer from the evidence supplied.
```

Retain the existing question-before-dossier layout. Oneshot may name input
needed on a later call; offline synthesis should produce a self-contained
answer with conditional conclusions rather than promising a follow-up.

## Auxiliary prompts and context handling

Kaibo already has structural recovery for a phase that reaches its turn cap
or stops without answer text. It replays the transcript for one final turn,
keeps tool definitions for message validity, and sets `ToolChoice::None`.
Empty results are checked after recovery. Keep those guarantees; prompt
changes do not replace them. [Recovery implementation][engine]

`EMPTY_ANSWER_NOTE` currently says that evidence was gathered, “so nothing
more needs investigating.” Evidence presence does not prove sufficiency.
Replace that implication while preserving the one-turn recovery:

```text
Your last turn returned no answer text. Write the answer or report now using
the evidence already available in this conversation. This final turn is for
writing, with tools disabled. State supported results with their source
references. Identify unresolved questions, missing evidence, and checks not
performed. A partial investigation can support a useful answer when its
limits are explicit. Produce the answer or report itself in this response.
```

Apply the same evidence language to `FINALIZE_NOTE`, retaining its different
reason: the research limit has been reached. Do not tell an early empty phase
it exhausted its turns, or tell an incomplete phase its research was complete.

The inspected session path stores and replays question/answer pairs, while
the forced-finish path replays its partial transcript. Neither is a semantic
compactor in the inspected code. Do not add compaction to this change merely
because Kaijutsu needs it. If Kaibo later reduces reports or histories, retain
question, corrections, evidence status, source references, coverage gaps, and
unresolved questions together. A condensed claim must not become stronger than
the evidence it replaced. [Sessions][sessions], [user framing][prompts]

Two inspection follow-ups belong in the implementation plan:

- Expose auxiliary instructions and attachment/recipient variants alongside
  role prompts, labeled by the conditions that emit them. The current static
  resource omits these. An additive CLI `kaibo prompts` command could call the
  same renderer; its syntax is a proposal, not an existing command.
- Audit the rendered kaish contract after the pending kaish update. The
  captured generic guidance mentions `ps` and overwrite recovery before the
  Kaibo addendum explains its read-only shell. Also verify the word “snapshot”:
  Kaibo mounts `LocalFs::read_only`; read-only access alone does not promise an
  immutable revision. Prefer capability-accurate shared help composition over
  another text-stripping filter. [Shell composition][kaish], [mount][sandbox]

## Implementation order and acceptance

1. Rebase on or stack after #183, preserving both reader variants. Coordinate
   with the open writing-style and kaish PRs before editing shared prose.
2. Add the evidence contract and review passages in `src/consult/prompts.rs`.
   Update caller-context, history, and deliberate framing together. Preserve
   override replacement, orientation, and house-rule behavior.
3. Correct the empty-answer recovery assertion in `src/consult/engine.rs` and
   render auxiliary prompts through their actual functions for inspection.
4. Run the offline tests below, then inspect complete rendered requests.
   Evaluate behavior with the fixture set before choosing further expansion
   or model-specific wording. Keep the recorded prompt and runtime changes
   separate when interpreting results.
5. Land through Kaibo's worktree → PR → cross-family review workflow. Update
   its changelog for changed user-visible review behavior. Publishing remains
   Amy's decision; this report is not permission to post a PR.

| Test or evaluation | Failure it must catch |
|---|---|
| Scripted requests from standalone explore, consult sweep, and deliberate | Correct reader reaches the actual provider request at each production route |
| Built-in, global override, and per-slot override cases | New guidance is composed once where intended; full replacement stays full replacement |
| Tool-bearing versus toolless requests | Oneshot/offline synthesis never promises tools; shell guidance survives role replacement through the tool schema |
| Partial evidence followed by turn-cap or empty-answer recovery | Tools disabled, evidence retained, truthful reason, unresolved work representable, blank response fails |
| Current question plus contradictory earlier answer | Wrapper preserves correction and version distinction without upgrading historical claims |
| Runtime/resource comparison with both readers and auxiliary variants | Inspection output represents what the production builder actually sends |

Scripted clients establish composition and runtime contracts; they do not
prove a model follows the prose. Use a small behavioral fixture set for that:

- A hook exists but the required behavior is absent inside it.
- A proposed rule already exists in the draft but not in the shipped seed.
- A caller's suspected defect is prevented by a guard in another file.
- A test is present but no execution result is supplied.
- Search output is truncated before the relevant match.
- Two files contribute to a failure; neither alone explains the conditions.
- A correct implementation admits no actionable defect in the stated scope.
- A dossier's conclusion exceeds its excerpts, and the offline model cannot
  fetch the missing implementation.

Compare the current prompt, #183's reader fix, and this proposal using the
same fixture contents, model slots, effort, tool configuration, and limits.
Record actual model IDs, input/output/cache usage, warnings, and whether each
phase completed. Judge citation support, correct defect rejection, identified
unknowns, missed defects, and unnecessary reads. Allow different wording;
score the claim against the fixture's evidence, not a phrase checklist. Vary
the question's initial suspicion to expose agreement with the caller as a
confounder. Repeated runs are needed before attributing an improvement to
prompt wording. No such behavioral comparison has run in this study.

[pr183]: https://github.com/tobert/kaibo/pull/183
[prompts]: https://github.com/tobert/kaibo/blob/1063d74f83a47d22eb1d27791c85f427c8f8d11b/src/consult/prompts.rs
[engine]: https://github.com/tobert/kaibo/blob/1063d74f83a47d22eb1d27791c85f427c8f8d11b/src/consult/engine.rs#L633
[kaish]: https://github.com/tobert/kaibo/blob/1063d74f83a47d22eb1d27791c85f427c8f8d11b/src/kaish_syntax.rs
[sandbox]: https://github.com/tobert/kaibo/blob/1063d74f83a47d22eb1d27791c85f427c8f8d11b/src/sandbox.rs#L251
[agents]: https://github.com/tobert/kaibo/blob/1063d74f83a47d22eb1d27791c85f427c8f8d11b/AGENTS.md
[sessions]: https://github.com/tobert/kaibo/blob/1063d74f83a47d22eb1d27791c85f427c8f8d11b/src/session.rs
[kj-fork]: https://github.com/tobert/kaijutsu/blob/04538700b801dc3471cd5c2dec07ff8b5087e4fe/crates/kaijutsu-kernel/src/kj/fork.rs#L814
[kj-lifecycle]: https://github.com/tobert/kaijutsu/blob/04538700b801dc3471cd5c2dec07ff8b5087e4fe/crates/kaijutsu-kernel/src/kj/lifecycle.rs#L165
[kj-fork-seed]: https://github.com/tobert/kaijutsu/tree/04538700b801dc3471cd5c2dec07ff8b5087e4fe/assets/defaults/rc/coder/fork

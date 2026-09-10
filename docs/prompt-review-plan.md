# Prompt review plan

Review the research and proposed prompts in commit `3deb6601` before changing
runtime behavior. Amy, 2026-09-10: “let's write up a review plan to a file,
I'll get a lil model going on the execution in a less safe harness in another
tab.” This is the handoff for that executor; no hosted review has run yet.

The two submissions from Codex were rejected by automatic approval review
because they would send repository files to external providers. Codex stopped
those attempts. Execution belongs to Amy's other session, under that session's
permissions; if it is blocked, report the block and ask Amy for help.

## Deliverables and scope

Run two independent Kaibo consultations, using the same question and whole
files. Keep each reviewer unaware of the other's findings until both finish.

| Review | Provider | Explorer and synthesis model | Cast |
|---|---|---|---|
| DeepSeek Flash | Direct DeepSeek API | `deepseek-flash` | `review-ds4` |
| GLM 5.3 | Crusoe | `zai-org/GLM-5.3` | `review-glm` |

Both IDs appeared in their provider catalogs during setup on 2026-09-10.
DeepSeek's current `deepseek-flash` alias identifies DeepSeek-V4.1-Flash;
record the model returned by the provider when available, since aliases move.
See the [DeepSeek API documentation](https://api-docs.deepseek.com/).
Do not substitute providers or more expensive models on failure. Claude Code
will review later; Amy has not asked this executor to start that review.

Keep raw JSON, logs, and a readable report for each reviewer outside the repo,
under `~/exomemory/kaijutsu/prompt-review-2026-09-10/`. Then write
`docs/prompt-review-results.md` with the findings and local verification. Do
not implement the runtime proposals, reseed rc, operate the live kernel, or
post results publicly as part of this review.

## Inputs

Attach these seven files whole, in this order:

1. `docs/prompt-proposals.md`
2. `docs/oss-comparisons.md`
3. `assets/defaults/system.md`
4. `assets/defaults/rc/coder/create/S00-stance.kai`
5. `assets/defaults/prompts/distillation.md`
6. `contrib/render-prompt-comparison.py`
7. `contrib/prompt-comparison.html`

Kaibo also reads project instructions (`AGENTS.md` by default) and can inspect
repository source through its read tools. The external payload therefore
includes more than the seven attachments if the review follows references.
Relevant implementation is in `crates/kaijutsu-kernel/src/`:

- `drift.rs`: formatter, fixed prompt, and existing formatter tests.
- `kj/mod.rs`, `kj/drift.rs`, `kj/fork.rs`: summarization and compact forks.
- `llm/system_prompt.rs`, `kj/lifecycle.rs`: system assembly and rc lifecycle.
- `llm/splice.rs`: existing selection and tool-group handling.
- `kj/context.rs`, `kernel_db.rs`: context environment and fork inheritance.

The generated `docs/prompt-comparison.html` is for human inspection; attach
the smaller template and generator to avoid duplicating all the prompt text.
The dossier has pinned sources and analysis, not complete upstream prompt
archives. Do not present a review of the dossier as an independent audit of
every upstream repository.

Record `git rev-parse HEAD` and `git status --short` before each consultation.
Other players are editing gate and approval code in this checkout, including
`kj/mod.rs`. Review only the prompt-related paths in those files and record
any relevant departure from `3deb6601`. Do not reset, stash, or commit another
player's work.

## Configuration

The running Kaibo MCP server has no DeepSeek key source configured. Use a
temporary CLI config rather than changing that server or permanent host
configuration. The existing `/tmp/kaijutsu-prompt-review.toml` may still be
available on zorak; otherwise create a private temporary file with this body.
These are credential file paths, not credential contents. Do not copy keys
into the report or print them.

```toml
[backends.deepseek]
kind = "deepseek"
api_key_file = "/home/atobey/.deepseek-key"

[backends.crusoe]
kind = "openai"
base_url = "https://api.inference.crusoecloud.com/v1"
api_key_file = "/home/atobey/.crusoe-api-key.txt"

[casts.review-ds4.explorer]
backend = "deepseek"
id = "deepseek-flash"
[casts.review-ds4.synth]
backend = "deepseek"
id = "deepseek-flash"

[casts.review-glm.explorer]
backend = "crusoe"
id = "zai-org/GLM-5.3"
[casts.review-glm.synth]
backend = "crusoe"
id = "zai-org/GLM-5.3"

[defaults]
explorer_max_turns = 8
synth_max_turns = 12
max_tokens = 16384
call_deadline_secs = 600

[telemetry]
enabled = false
```

Use `chmod 600` on the temporary config. Cast input uses `backend` and `id`;
the diagnostic `kaibo config` display is not a round-trip config template.
The token setting above passes this CLI's validation against its inherited
thinking budget; it is an output ceiling, not a target spend. Turn counts
and the deadline keep the investigation bounded. Run once per cast and
inspect failures before considering another paid call.

## Review question

Save the following as `question.txt` in the report directory. The earlier
copy at `/tmp/kaijutsu-prompt-review-question.txt` is also available until
temporary files are cleared.

```text
Review the committed prompt research and drafts in commit 3deb6601 for
Kaijutsu. This is a design/prompt review before runtime changes, not a request
to implement. Read attached whole files, then inspect only the relevant
existing implementation as needed. Other players are changing approval and
gate code; those changes are out of scope.

Amy's accepted direction: ordinary rc files should configure briefing and
continuation length; the fixed under-500-word rule is too restrictive. The
draft removes that number, but configuration syntax and defaults are not
decided. General-purpose default is a proposal; assistant remains fleet
coordination. We retain TDD, one kernel sequencer, plain block text,
append-only conversations between hydrate boundaries, and kaish as host exec
owner. Avoid adding a prompt registry or configuration RPC. A context fork
does not isolate file edits.

Assess:
1. Concrete prompt contradictions, missing guidance, overconstraint, or
   unnecessary common-base cost.
2. Separation of drift briefing from continuation and preservation of
   original intent versus new user steering.
3. Rc budget implementation using existing mechanisms, including absent or
   invalid values and word targets versus hard input/output bounds.
4. Current distillation source selection, exclusions, 2000-byte cuts,
   provenance, role/tool pairing, and restoring child stance.
5. Integrity and maintainability of the offline comparison generator.

Separate bugs observed in current code from risks in the proposed design.
Do not claim upstream model efficacy or source verification when only our
dossier is available. Return at most six prioritized findings with repository
file:line citations, concrete consequences, and minimal remedies, then a
suggested implementation order and fail-first tests. If the drafts are sound
on an axis, say so without inventing a finding. Read only what this bounded
review needs; do not delegate a broad repository survey. Limit the final
answer to about 1200 words.
```

## Invocation

After preparing `question.txt`, run this from the Kaijutsu checkout in Amy's
execution session. This Python example supplies arguments directly without
shell interpolation. It runs the reviews sequentially and stops on failure.
It refuses to overwrite an existing result; inspect an earlier run before
choosing a new report directory.

```python
from pathlib import Path
import subprocess

root = Path('/home/atobey/src/kaijutsu')
config = Path('/tmp/kaijutsu-prompt-review.toml')
out = Path.home() / 'exomemory/kaijutsu/prompt-review-2026-09-10'
question = (out / 'question.txt').read_text()
attachments = [
    'docs/prompt-proposals.md',
    'docs/oss-comparisons.md',
    'assets/defaults/system.md',
    'assets/defaults/rc/coder/create/S00-stance.kai',
    'assets/defaults/prompts/distillation.md',
    'contrib/render-prompt-comparison.py',
    'contrib/prompt-comparison.html',
]
for cast in ('review-ds4', 'review-glm'):
    with (out / f'{cast}.checkout.txt').open('x') as snapshot:
        for args in (['git', 'rev-parse', 'HEAD'],
                     ['git', 'status', '--short']):
            subprocess.run(args, cwd=root, stdout=snapshot, check=True)
    command = ['kaibo', '--config', str(config), '--root', str(root),
               '--no-persistence', 'consult', '--cast', cast,
               '--json', '--include-report']
    for path in attachments:
        command.extend(['--attach', path])
    command.append(question)
    with (out / f'{cast}.json').open('x') as answer, \
         (out / f'{cast}.log').open('x') as log:
        subprocess.run(command, cwd=root, stdout=answer, stderr=log,
                       check=True)
```

CLI options were checked against local `kaibo consult --help`. The consult
invocations themselves have not run. Preserve any error envelope, warnings,
usage, and provider/model metadata alongside a readable extraction of the
answer. An empty answer or a failed call does not count as a completed review.

## Local synthesis and implementation handoff

Verify each finding against current source before accepting it. The results
document should name both models, reviewed revisions, incomplete calls, and
actual checks. For each finding, record accepted, rejected, or unresolved,
with a short reason. Distinguish agreement between reviewers from evidence.
Preserve disagreements for Amy and the later Claude Code review.

A read-only GPT-5.6 Terra planning pass identified these candidate work
assignments. They are proposals to revise after review, not dispatched work:

| Owner | Bounded work | Dependencies and meaningful tests |
|---|---|---|
| Terra: prompt seeds | Base, coder variants, optional default stance; assembly tests | Settle base cost/default role first. Verify base then one eligible stance, with situation and notifications in their intended locations. |
| Terra: distillation | One owner for purpose selection, rc length resolution, input selection, and source references | Keep shared `drift.rs` and `kj/mod.rs` edits together. First write failing cases for corrections beyond byte 2000, excluded input, Unicode cuts, missing references, and invalid budgets. |
| Lead: integration | Wire drift briefing and compact-fork continuation; review child initialization and retained recent evidence | Follows distillation decisions. Test repeated compact forks, unanswered questions, superseded plans, child stance, tool-group completeness, and bounded inputs. |

Existing `context_env` and `KernelDb::insert_forked_context` provide durable
configuration and inheritance. Investigate using them for independent drift
and continuation word targets set by rc. Choose names, defaults, validation,
and missing-value behavior in implementation; do not introduce a second
configuration owner. Keep word guidance separate from provider output limits
and source input bounds.

Inspect `llm/splice.rs` before inventing another tool-group selector. Decide
how source-attributed recent evidence is represented and how an oversized
group is handled before writing compact-fork retention. Prompt prose cannot
restore omitted source text. The lead owns coordination with the active gate
lane before any edit to shared `kj/mod.rs`.

Regenerate the comparison artifact if accepted feedback changes the drafts.
Use the existing browser checks for its display and downloads. Do not run a
large Rust build on zorak for a documentation-only review. Credit the actual
reviewers and Terra planning contribution in later commits that use their
work; do not credit a model whose review failed to run.

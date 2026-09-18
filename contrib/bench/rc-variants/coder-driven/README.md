# coder-driven — a coder variant built on a driven-worker contract

An A/B arm for the bench harness. It replaces the coder type's two instruction
sections with a contract for a worker another context drives: the brief is the
whole contract, nobody answers mid-task, the turn ends when the work is done or
a real blocker is named, and the final message is the deliverable. It changes
no shipped default and no other context type.

The evidence it answers is `docs/issues.md`, "What running under a benchmark
showed (2026-09-18)" and `~/exomemory/kaijutsu/coder-early-stop-2026-09-18.md`.

## What it replaces

| Path under the rc tree | Shipped | Here |
|---|---|---|
| `coder/create/S00-base.kai` | symlink to `lib/create/S00-base.kai` | regular file, same one-line body |
| `coder/create/S00-base.md` | symlink to `lib/create/S00-base.md` | the driven worker's contract |
| `coder/create/S00-stance.kai` | model-tiered stance in a shell string | regular file reading its companion |
| `coder/create/S00-stance.md` | absent | the coding procedure, shell mechanics, and verdict line |

The coder type's other `create` scripts are untouched and still run: the kaish
primer, tool binding, recall, handoff, cache breakpoint, datetime, build cache,
shell guard, and the lfm2d advisory. So are `coder/fork/` and `coder/drift/`.
Every `.kai` in the directory runs in lexical filename order, so `S00-base.kai`
still precedes `S00-stance.kai`.

The shared base gets its own copy rather than an edit, because `default` and
`director` link to the same file. The shared base is written for a context a
person is sitting with: it offers to ask when a decision needs judgment, and it
names a handoff as where unfinished work goes. Both are exits a driven worker
should not take mid-task.

## Applying it

The harness materializes the shipped tree first, then copies these files over
it:

```sh
kaijutsu-server rc reseed --dir "$CONFIG_ROOT/rc"
src=contrib/bench/rc-variants/coder-driven/coder/create
dst="$CONFIG_ROOT/rc/coder/create"
rm -f "$dst/S00-base.kai" "$dst/S00-base.md" "$dst/S00-stance.kai"
cp "$src/S00-base.kai" "$src/S00-base.md" "$src/S00-stance.kai" "$src/S00-stance.md" "$dst/"
```

Remove before copying. A reseed writes `S00-base.kai` and `S00-base.md` as real
symlinks into `lib/`, and `cp` over a symlink writes through it — that would
edit the shared base every other type reads.

Create the coder context after copying. Instructions are stored as durable
blocks when the create lifecycle runs, so a context that already exists keeps
the text it was created with.

## Why the loader accepts this

- Discovery lists a verb directory and keeps both files and symlinks, then
  filters to `.kai` (`crates/kaijutsu-kernel/src/rc/mod.rs:400-412`). A regular
  file where a symlink used to be is read the same way. A `.md` companion is
  never a discovery candidate.
- Each script is read at `rc/<type>/<verb>/<name>`
  (`crates/kaijutsu-kernel/src/rc/mod.rs:427-438`), so replacing one type's
  entry cannot reach another type.
- A script finds its companion Markdown through `$(dirname "$0")`, where `$0`
  is the invoked path including a link's own name (`docs/prompts.md`, "Explicit
  instruction scripts"). The copies here resolve to the copies beside them.
- A later forced reseed would restore the shipped files: it removes and
  recreates any entry whose content differs from the embedded seed
  (`crates/kaijutsu-kernel/src/seed_scripts.rs:198-233`). Re-apply this
  directory after any `rc reseed --force`.

## The verdict line

The worker's final message ends with the report and then one verdict line,
alone on the last line:

```text
RESULT: done
RESULT: blocked — what you need
RESULT: gave up — why you stopped
```

A driver or a classifier reads the last match of:

```python
VERDICT_RE = re.compile(
    r"^[ \t>*`]*RESULT:[ \t]+(done|blocked|gave up)"
    r"(?:[ \t]*(?:—|-{1,2})[ \t]*(\S.*?))?[ \t`]*$",
    re.MULTILINE,
)
```

Take the last match in the final message, not the first: the report above it
can quote the form. Group 1 is the verdict, group 2 the reason or `None`. The
leading class absorbs an indent, a quote marker, or backticks a model wraps the
line in, and the dash alternation accepts `-` and `--` for the em dash. No
match means the turn ended without a verdict, which is itself the measurement.

`contrib/bench/analysis/classify_run.py` does not read this yet.

## Confounds to name in any result

- **The model tier branch is gone.** The shipped stance picks a `focused` or a
  `guided` procedure from the resolved model id; this variant has one plain
  procedure for every model. Against a frontier model the A/B therefore moves
  two variables, not one. Against `deepseek-v4-flash`, which resolves to
  `guided`, it moves one.
- **It assumes a driver.** A person sitting with a coder context is told their
  questions will not be answered and that they read only the final message.
  Both are false for a directly driven seat.
- **The shell guidance is a prompt, not a policy change.** `foreground`
  still defaults to false, and the timeouts are unchanged.

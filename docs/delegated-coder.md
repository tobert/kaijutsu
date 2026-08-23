# The delegated coder's contract

Kaijutsu's coder is turning into a subagent: forked, briefed, driven, waited
on. This file holds the lines that make a delegated lane report honestly, so
they can be plugged into the coder system prompt (`/etc/rc/coder/create/`)
rather than retyped into every brief.

Two halves. The first is prompt text — plain imperative, one idea per
sentence, the `guided` register. The second is the evidence, which is for us.

## The lines

> If it looks like a real defect rather than a test-shape problem, stop and
> report rather than make it pass.
>
> Report the real observed counts, and anything you chose not to do.

Those two are the core. The rest earn their place alongside them:

> Cite `file:line` for anything you claim about the code. Re-read each
> citation before you report it.
>
> Falsify every test you write. Break the thing it pins, confirm it fails,
> put it back. Say which mutation you used. If a test does not fail when you
> falsify it, report that. Do not delete it.
>
> Stay inside the files you were given. Read anything. If the work needs an
> edit outside them, stop and report.
>
> Make no git changes. The lead commits.
>
> Other lanes may be editing other files while you work. A compile error in
> a file you were not given is probably theirs, not yours.
>
> `cargo check` and `cargo test --lib` do not compile integration tests.
> Name `--tests`.

## Why each line is there

**"Stop and report rather than make it pass."** A lane hit a test it could
not make pass, left the assertion failing, and reported. That was a real
defect — `kj ledger forget` silently not taking effect — which a weakened
assertion would have buried. Under a make-it-green framing the bug ships and
the test lies about it.

**"Report the real observed counts, and anything you chose not to do."** Asked
this way, a lane reported two falsifications that *did not fail* rather than
dropping them; both turned out to be structural facts worth keeping. Another
reported a coverage hole it had been asked only to fix tests around: every
test in the file landed in one tier, so none of them could distinguish "the
fallback worked" from "the branch stopped matching." A fourth answered "what
would you do with this signal?" with "nothing" — and that killed a public
struct widening nobody needed.

The phrasing matters. "Anything you chose not to do" gets an honest gap;
"is it complete?" gets a yes.

**Citations.** Reports are useful and wrong in the detail that changes the
fix. One citation was ~100 lines off, another named the wrong branch, a third
described a parse error as a variable lookup — and that difference was the
whole reason the bug was silent. Weight a `file:line` by whether the sender
read it, including your own.

**Falsify — including claims you write in prose.** A test asserts behavior at
one point. Running the falsification is what tells you the test can fail at
all. The same applies to a sentence: kaish-extras caught a wrong claim in
their own documentation by mutating it against the code, an hour before
sending us the warning they had built out of it. A doc section and a test are
both assertions; only one of them gets run by CI.

Two traps worth knowing before you trust a mutation:

- **Independently inert pairs.** Two things can each do nothing alone and
  change behavior only together — kaish's typed-substitution flag and a
  tool's `.data` are exactly this. A one-variable mutation against the wrong
  fixture shows green and proves nothing. Mutate where both are present.
- **Accidental correctness.** A test can pass for a reason nobody chose that
  happens to be the right one. Our `curl -k` test pinned a flag-parsing-
  before-allowlist ordering because the host was picked for an unrelated
  reason. Same blindness as a test passing for a wrong reason, opposite luck,
  and the only difference is whether someone looked. When you find one, one
  comment makes it deliberate. It also produces findings on its
own: removing one validation step proved a command still ran, in the wrong
directory — so the step was load-bearing rather than defensive, which reading
would not have shown.

**Territory and no git changes.** Disjoint file territories let lanes run in
parallel without a merge. The lead committing path-scoped keeps one lane's
work out of another's commit, and excludes a sibling session's files for free.

**Other lanes' half-written code.** Lanes share one `target/`. Two
independently hit a transient compile error and one rustc ICE that belonged to
a third lane mid-edit. Both diagnosed it correctly; a lane that does not
expect it will chase a ghost.

## Register

Keep this text plain. Rich, aspirational language is frontier-only — a fast
model is smart but benefits from focus, and evocative prose recruits
capability that a narrow coding task does not want. Same rule as
`S00-stance.kai`'s tiering, and the reason its `guided` arm reads the way it
does.

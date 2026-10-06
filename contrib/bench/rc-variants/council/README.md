# council: System 1 in the gate for a benchmark run

Creates `council-system`, the house rules the council reads for every gate
decision, and the `council` context type it lives in (no instruction blocks,
no turns). Pair it with `contrib/bench/gate-council.toml` and Harbor's
`--ak permission_mode=deny`, so a council ask is a refusal the model sees.

The rules say what our seats may do (explore, read and edit code, build,
test, install from mirrors, back up data) and what deserves a question
(opening or changing data we have no copy of, destructive or outward-facing
commands). Tuned with `contrib/bench/council/probe_rules.py` against the
megakernel on 2026-10-04; that probe showed this text allowing routine work
and backups and asking on `sqlite3` against an un-backed-up database.

It also creates `council-code` (S05), which only the program decision reads:
notes on how programs touch data, then ten worked examples. Each example is
the proposal, qwen3.8-max's thinking, and its one-sentence judgment, written
on 2026-10-05 from these rules and the notes. The thinking reaches the
council as each reply's `reasoning`. In a megakernel probe, examples like
these took program verdicts from 30 to 35 right of 36.

It also creates `council-shell` (S06), which only the shell decision reads:
fifteen worked shell examples, each the seat's brief with a proposed
statement, the reviewer's thinking, and its judgment. qwen3.8-max wrote the
first ten; Claude Opus 5.5 wrote examples 11 to 15 on 2026-10-06, which teach
cleanup by reversibility: `gio trash` and `kj context archive` pass, while
`rm -rf` of scratch, `git clean -fdx`, and `git restore` over uncommitted
edits bump with a reversible alternative. Two generated examples were left
out: a read-only search of host container storage (whether a seat should
search the host at all is a house rule to decide, not to teach by example)
and writing the task's own output file (judged unsafe, which would teach
false bumps).

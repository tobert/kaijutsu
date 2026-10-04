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

# Bench analysis tools

Two Python 3.12+ scripts, standard library only, each runnable with
`--help`. Both fail loudly on malformed input: a corrupt JSON line is a
hard error, and an event shape neither script recognizes is counted and
reported in the output, never silently dropped.

## `classify_run.py`

Reads one Harbor ACP run directory (`acp-events.jsonl` and
`acp-summary.json`, both required) and reports `turn_end_class`: how the
model's turn ended — not whether the task passed. Harbor's verifier
decides that, from `reward.txt`/`ctrf.json`, not from this tool.

```bash
python3 classify_run.py /home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-1
python3 classify_run.py /home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-1 --format text
python3 classify_run.py /home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-1 \
  --kernel-log /home/atobey/src/bench-work/kernels/ds-02/logs/kernel.log
```

The optional `--kernel-log` sums per-inference tokens from
`"LLM stream completed"` lines in a kaijutsu kernel log, scoped to the
run's own session (a kernel log can span several runs). A malformed
`"LLM stream completed"` line is a hard error, not a skipped line — a
partial token total would be worse than none.

This environment's run directories carry `runner.log` (the ACP launcher's
own log), not the `acp.txt` name Harbor's `AcpAgent._OUTPUT_FILENAME`
uses inside a real `harbor run` trial. This tool does not read either
one; only `acp-events.jsonl` and `acp-summary.json` are required.

## `summarize_job.py`

Reads one Harbor job directory (one subdirectory per trial, each holding
a `result.json`) and prints one row per trial plus totals. A trial whose
`agent/` directory holds `acp-events.jsonl`/`acp-summary.json` gets its
row filled in from `classify_run`'s analysis; a non-ACP agent (`oracle`,
`nop`, `mini-swe-agent`, `terminus-2`, ...) gets `turn_end_class: "n/a"`
and whatever tokens/cost Harbor's own `agent_result` provides.

A trial whose `agent/acp.txt` exists (the kaijutsu kernel log riding the
agent's stderr, always present for a Harbor ACP trial) gets `tokens_in`,
`tokens_out`, and `llm_inferences` filled from that log's `"LLM stream
completed"` lines, scoped to the trial's own session id, using the same
`parse_kernel_log` that backs `classify_run.py --kernel-log`. This
overrides `agent_result`'s tokens, since Harbor's ACP adapter never
populates those fields itself. `tokens_source` reports where a row's
tokens came from (`"kernel_log"`, `"agent_result"`, or null). When
`acp.txt` exists but no log line matches the session, tokens and
`llm_inferences` are null with `tokens_absent_reason` explaining why —
never reported as zero.

```bash
python3 summarize_job.py /home/atobey/src/bench-work/harbor/jobs/hello-world-oracle
python3 summarize_job.py /home/atobey/src/bench-work/harbor/jobs/hello-world-oracle --format jsonl
```

## `tb2-subset.txt` / `tb2-subset.md`

A fixed 20-task subset of Terminal-Bench 2.0 for a cheap model on one
workstation under podman. `tb2-subset.txt` is the plain task-name list
(what `harbor run -i` expects); `tb2-subset.md` has the full table,
why each task was chosen, the notable exclusions, and the exact
`harbor run` command line.

## Tests

```bash
python3 -m unittest test_classify_run -v
python3 -m unittest test_summarize_job -v
```

Each suite builds small synthetic fixtures for every code path (one per
`turn_end_class`, malformed input, an unrecognized event shape) plus real
fixture tests: `test_classify_run.py` against
`/home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-2`,
`test_summarize_job.py` against the oracle/nop job directories under
`/home/atobey/src/bench-work/harbor/jobs`. Both real-fixture groups skip
with a clear message when that tree is not present.

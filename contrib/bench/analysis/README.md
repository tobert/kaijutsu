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

The report also carries the driven-worker A/B fields, both independent of
`turn_end_class`:

- `verdict` / `verdict_reason`: the last `RESULT: done|blocked|gave up`
  line in the run's final message (`contrib/bench/rc-variants/coder-driven`,
  "The verdict line"; `VERDICT_RE`), searched over the untruncated text —
  a verdict beyond `final_message`'s 2000-character tail is still found.
  Null when no worker in this run used the convention.
- `shell_tool_calls_total`, `shell_tool_calls_foreground_true`,
  `shell_tool_calls_kj_wait_invocations`: counted from the `shell` and
  `shell_write` tool calls' `rawInput` (present on the `tool_call` event
  that creates each call, never on a later `tool_call_update`).
  `shell_tool_calls_raw_input_reason` explains a null count — a real
  absence of shell calls is reported as `0`, never as a null.

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

Each ACP row also carries `verdict`, `verdict_reason`,
`shell_tool_calls_total`, `shell_tool_calls_foreground_true`, and
`shell_tool_calls_kj_wait_invocations` from `classify_run`'s analysis (see
above); a non-ACP row leaves them null. Totals add `verdict_present` and
three agreement counts against Harbor's own `reward >= 1.0`:
`verdict_done_and_solved`, `verdict_done_but_failed` (the worker claimed
done but the verifier disagreed, or the trial errored before it ran), and
`verdict_not_done_but_solved` (blocked/gave up/no verdict, but the
verifier passed it anyway).

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

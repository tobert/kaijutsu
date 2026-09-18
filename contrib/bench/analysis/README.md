# Bench analysis tools

Two Python 3.12+ scripts, standard library only, each runnable with
`--help`. Both degrade a single unreadable or partial trial rather than
aborting the whole report — a trial Harbor killed with `AgentTimeoutError`,
or one still running, normally has no `acp-summary.json`, and that alone
must never crash a job summary. Both still fail loudly on genuinely
malformed input: a corrupt JSON line in a file that exists is a hard
error, a job directory with no trial subdirectories is a hard error, and
an event shape neither script recognizes is counted and reported in the
output, never silently dropped.

## `classify_run.py`

Reads one Harbor ACP run directory (`acp-events.jsonl`, required) and
reports `turn_end_class`: how the model's turn ended — not whether the
task passed. Harbor's verifier decides that, from `reward.txt`/`ctrf.json`,
not from this tool. `acp-summary.json` is read when present; its absence
is normal, not corrupt, for a trial Harbor killed or one still running.

```bash
python3 classify_run.py /home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-1
python3 classify_run.py /home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-1 --format text
python3 classify_run.py /home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-1 \
  --kernel-log /home/atobey/src/bench-work/kernels/ds-02/logs/kernel.log
python3 classify_run.py /path/to/trial/agent --trial-result /path/to/trial/result.json
```

Without `acp-summary.json`, `turn_end_class` is `"agent_timeout"` when
`--trial-result` names a Harbor `result.json` whose
`exception_info.exception_type` is `AgentTimeoutError` (read only for that
field), else `"unclassified_stop_reason"`. An `agent_timeout` report also
carries `timeout_last_tool_call_name`, `timeout_last_tool_call_status`,
`timeout_seconds_since_last_event`, and
`timeout_seconds_since_last_event_source`: what the agent was doing when
Harbor killed it, and how long before the kill its last known activity
was (null with source `"unavailable"` when no event in the run carries a
usable timestamp). `inferences` (the `usage_update` event count) doubles
as the inference count at kill time.

A `provider_failure`/`setup_failure` run (`acp-summary.json` present,
carrying an `error`) carries `failure_detail`: the error's message,
truncated to 300 characters. When that message is only the generic
"Internal error" a JSON-RPC error's outer envelope carries, this instead
reads the matching `"LLM stream error: ..."` line from `agent/acp.txt`
next to the run directory, when present — the real cause rides the kernel
log, not `acp-summary.json`.

The optional `--kernel-log` sums per-inference tokens from
`"LLM stream completed"` lines in a kaijutsu kernel log. Session id is
resolved in order (`resolve_session_id`): `acp-summary.json`'s
`session.sessionId`, then `acp-events.jsonl`'s `session_update` events'
`session_id` when exactly one distinct id appears, then the kernel log's
own `context.id` when exactly one distinct id appears there; unresolved,
the whole file is summed unscoped, as a last resort (a kernel log can
span several runs). A malformed `"LLM stream completed"` line is a hard
error, not a skipped line — a partial token total would be worse than
none.

This environment's run directories carry `runner.log` (the ACP launcher's
own log), not the `acp.txt` name Harbor's `AcpAgent._OUTPUT_FILENAME`
uses inside a real `harbor run` trial. This tool does not read either one
directly; `--kernel-log`/`--trial-result` name paths explicitly, and
`agent/acp.txt` next to `RUN_DIR` is read only for the `failure_detail`
fallback above.

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

Reads one Harbor job directory (every directory directly under it is a
trial directory; job-level metadata — `config.json`, `job.log`,
`lock.json`, the job's own `result.json` — are files, not directories) and
prints one row per trial plus totals. A trial with no `result.json` yet
(still running) reports `trial_status: "in_progress"` and is excluded from
every total except `n_trials`/`n_in_progress`. A trial whose `agent/`
directory holds `acp-events.jsonl` gets its row filled in from
`classify_run`'s analysis, whether or not `acp-summary.json` also exists;
a non-ACP agent (`oracle`, `nop`, `mini-swe-agent`, `terminus-2`, ...) has
no `acp-events.jsonl` at all and gets `turn_end_class: "n/a"` and whatever
tokens/cost Harbor's own `agent_result` provides.

A trial Harbor killed with `AgentTimeoutError` has `acp-events.jsonl` and
`agent/acp.txt` but no `acp-summary.json` (the ACP runner writes it only
on a clean exit) — this is normal, not corrupt. Given the trial's own
`result.json`, such a row classifies `turn_end_class: "agent_timeout"` and
carries `timeout_last_tool_call_name`, `timeout_last_tool_call_status`,
and `timeout_seconds_since_last_event`: what the agent was doing when
killed (see `classify_run.py` above). A `NonZeroAgentExitCodeError` trial
(a summary present, carrying an `error`) classifies
`provider_failure`/`setup_failure` and carries `failure_detail`: the
error's message, truncated to 300 characters, recovered from the matching
`agent/acp.txt` line when the summary only says the generic
"Internal error".

A trial whose `agent/acp.txt` exists (the kaijutsu kernel log riding the
agent's stderr, always present for a Harbor ACP trial) gets `tokens_in`,
`tokens_out`, and `llm_inferences` filled from that log's `"LLM stream
completed"` lines, using the same `parse_kernel_log` that backs
`classify_run.py --kernel-log`. This overrides `agent_result`'s tokens,
since Harbor's ACP adapter never populates those fields itself. The
session id to scope that parse to is resolved in order
(`classify_run.resolve_session_id`): `acp-summary.json`'s
`session.sessionId`, when present; else `acp-events.jsonl`'s
`session_update` events' `session_id`, when exactly one distinct id
appears; else `acp.txt`'s own "LLM stream completed" lines' `context.id`,
when exactly one distinct id appears there. `tokens_source` reports
which: `"kernel_log"`, `"kernel_log_events_session"`, or
`"kernel_log_single_context"` respectively, `"agent_result"` when there
was no `acp.txt` to read, or null when none had data. When `acp.txt`
exists but none of the three stages resolves to a single session id,
tokens and `llm_inferences` are null with `tokens_absent_reason` set to
why (including the distinct ids seen, when ambiguous) — never reported as
zero.

Each ACP row also carries `verdict`, `verdict_reason`,
`shell_tool_calls_total`, `shell_tool_calls_foreground_true`, and
`shell_tool_calls_kj_wait_invocations` from `classify_run`'s analysis (see
above); a non-ACP row leaves them null. Totals add `verdict_present` and
three agreement counts against Harbor's own `reward >= 1.0`:
`verdict_done_and_solved`, `verdict_done_but_failed` (the worker claimed
done but the verifier disagreed, or the trial errored before it ran), and
`verdict_not_done_but_solved` (blocked/gave up/no verdict, but the
verifier passed it anyway). Totals also add `agent_timeouts`,
`turn_failures` (provider_failure/setup_failure count),
`median_duration_seconds_solved` (median of Harbor's own
`finished_at - started_at` over solved trials — a speed comparison
against a control arm that did not time out), and `n_in_progress`.

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
`turn_end_class`, malformed input, an unrecognized event shape, plus the
degraded-trial states above: a timed-out trial with events and a kernel
log but no summary, a trial still in progress with no `result.json`, a
non-zero-exit trial with an error summary — both a direct message and the
"Internal error"-falls-back-to-`acp.txt` case — and a kernel log with
several distinct context ids and nothing to disambiguate) plus real
fixture tests: `test_classify_run.py` against
`/home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-2`,
`test_summarize_job.py` against the oracle/nop job directories under
`/home/atobey/src/bench-work/harbor/jobs`. Both real-fixture groups skip
with a clear message when that tree is not present.

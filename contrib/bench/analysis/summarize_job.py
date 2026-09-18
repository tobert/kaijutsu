#!/usr/bin/env python3
"""Summarize one Harbor job directory: one row per trial, plus totals.

Reads `result.json` from each trial subdirectory of a Harbor job (the
layout in /home/atobey/src/bench-work/harbor/NOTES.md, "Result file
layout"): `JOB_DIR/<task>__<rand>/result.json`. Every directory directly
under JOB_DIR is a trial directory (job-level metadata — `config.json`,
`job.log`, `lock.json`, the job's own `result.json` — are files, not
directories). A trial directory with no `result.json` yet is one Harbor
has not finished: its row reports `trial_status: "in_progress"` and is
excluded from every total except `n_trials` and `n_in_progress`. One
unreadable or partial trial never aborts the whole job summary; this
degrades that trial's row instead. Genuinely malformed input — a file
that exists but is not valid JSON, or a job directory with no trial
subdirectories at all — is still a hard error: crashing beats reporting
corrupt data as if it were real.

When a trial's `agent/` directory holds `acp-events.jsonl`, this also runs
classify_run's analysis on it and folds in `turn_end_class` and the
gate/ask counts, whether or not `acp-summary.json` also exists (see
below). A non-ACP agent (oracle, nop, mini-swe-agent, ...) has no
`acp-events.jsonl`; its row reports `turn_end_class` as "n/a" and fills
tokens/cost from Harbor's own `agent_result`, when present.

A Harbor ACP trial's `acp-summary.json` is written only when the ACP
runner exits on its own. A trial Harbor killed with `AgentTimeoutError`,
or one still running, has `acp-events.jsonl` and `agent/acp.txt` but no
summary — this is normal, not corrupt. Given the trial's own `result.json`
(`exception_info.exception_type == "AgentTimeoutError"`), classify_run
reports `turn_end_class: "agent_timeout"` instead of failing, plus what
the agent was doing when killed: `timeout_last_tool_call_name`,
`timeout_last_tool_call_status`, and `timeout_seconds_since_last_event`
(null with `timeout_seconds_since_last_event_source: "unavailable"` when
no event in the run carries a usable timestamp). A `NonZeroAgentExitCodeError`
trial (a summary present, carrying an `error`) classifies as
`provider_failure`/`setup_failure` and carries `failure_detail`: the
error's message, truncated to 300 characters, recovered from the matching
`agent/acp.txt` "LLM stream error: ..." line when the summary only says
the generic "Internal error".

When a trial's `agent/acp.txt` exists (a kaijutsu kernel log riding the
agent's stderr, always present for a Harbor ACP trial since the adapter
sets `RUST_LOG=info`), this parses it with classify_run's
`parse_kernel_log` and fills `tokens_in`, `tokens_out`, and
`llm_inferences` from it, overriding whatever `agent_result` carried. The
session id to scope that parse to is resolved in order (classify_run's
`resolve_session_id`): `acp-summary.json`'s `session.sessionId`, when
present; else `acp-events.jsonl`'s `session_update` events' `session_id`,
when exactly one distinct id appears there; else `acp.txt`'s own "LLM
stream completed" lines' `context.id`, when exactly one distinct id
appears there. `tokens_source` reports which: `"kernel_log"`,
`"kernel_log_events_session"`, or `"kernel_log_single_context"`
respectively, `"agent_result"` when there was no `acp.txt` to read at all,
or null when neither had data. When `acp.txt` exists but none of the
three stages resolves to a single session id, tokens and `llm_inferences`
are null with `tokens_absent_reason` set to why (including the distinct
ids seen, when ambiguous) — never reported as zero.

An ACP trial also carries the driven-worker verdict line, if one exists
(classify_run's `verdict`/`verdict_reason`, from
`contrib/bench/rc-variants/coder-driven`'s `RESULT: done|blocked|gave up`
convention), and three shell-command counts the A/B compares:
`shell_tool_calls_total`, `shell_tool_calls_foreground_true`, and
`shell_tool_calls_kj_wait_invocations` (literal "kj wait" occurrences in
shell/shell_write command text). All null with
`shell_tool_calls_raw_input_reason` set when no shell/shell_write call
carried the `rawInput` these are read from — never reported as zero when
the data cannot support the count; a genuine absence of shell calls is a
real 0.

Usage:
    summarize_job.py JOB_DIR [--format markdown|jsonl]

`--format markdown` (default) prints a Markdown table plus a totals
section. `--format jsonl` prints one JSON object per trial (each tagged
`"kind": "trial"`), followed by one final `"kind": "totals"` object.

Totals (over completed trials — `trial_status == "completed"` — unless
noted):
    n_in_progress         trials with no result.json yet; excluded from
                           every other total below.
    pass_rate             trials with reward >= 1.0, over trials with a
                          reward at all (a trial that errored before the
                          verifier ran has no reward and is excluded from
                          both the numerator and denominator, and reported
                          separately as n_without_reward).
    tokens_per_solved_task
                          (input + output tokens summed over solved trials)
                          / (count of solved trials with both token fields
                          present). null when no solved trial has token
                          data (true for non-LLM agents like oracle/nop).
    ask_count             sum of permission_requests across ACP trials.
    stall_count           trials where turn_end_class is yielded_on_ask or
                          yielded_on_async, or asks_orphaned > 0.
    turns_ended_early     trials whose turn_end_class is not
                          completed_verified AND whose reward is < 1.0.
                          Applies to "n/a"-class (non-ACP) trials too: a
                          failing oracle/nop/mini-swe-agent trial counts
                          here as well, since neither condition names ACP
                          specifically. An agent_timeout or provider_failure/
                          setup_failure trial counts here whenever its
                          reward is also < 1.0.
    agent_timeouts        trials whose turn_end_class is agent_timeout.
    turn_failures         trials whose turn_end_class is provider_failure
                          or setup_failure.
    median_duration_seconds_solved
                          median of duration_seconds (Harbor's
                          finished_at - started_at) over solved trials
                          that carry a duration. null when no solved trial
                          has one.
    verdict_present       trials whose final message carried a verdict line.
    verdict_done_and_solved
                          verdict == "done" and Harbor's reward >= 1.0: the
                          worker's self-report and the verifier agree.
    verdict_done_but_failed
                          verdict == "done" but reward < 1.0 or absent
                          (errored before the verifier ran): the worker
                          claimed done when it was not.
    verdict_not_done_but_solved
                          verdict is "blocked", "gave up", or absent, but
                          reward >= 1.0: the worker under-reported, or never
                          reached the line that would have reported at all.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from datetime import datetime
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import classify_run as cr  # noqa: E402


def load_json(path: Path) -> dict[str, Any]:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(f"cannot read {path}: {exc}") from exc
    try:
        obj = json.loads(text)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{path}: not valid JSON ({exc})") from exc
    if not isinstance(obj, dict):
        raise SystemExit(f"{path}: expected a JSON object, got {type(obj).__name__}")
    return obj


def parse_iso(ts: Any) -> float | None:
    if not isinstance(ts, str) or not ts:
        return None
    try:
        # Harbor emits both trailing 'Z' and bare offsets across fields
        # (compare hello-world-oracle's job-level vs trial-level timestamps).
        normalized = ts.replace("Z", "+00:00")
        return datetime.fromisoformat(normalized).timestamp()
    except ValueError:
        return None


def reward_from(verifier_result: Any) -> float | None:
    if not isinstance(verifier_result, dict):
        return None
    rewards = verifier_result.get("rewards")
    if not isinstance(rewards, dict):
        return None
    reward = rewards.get("reward")
    return reward if isinstance(reward, (int, float)) else None


def acp_kernel_log_tokens(
    acp_log_path: Path,
    summary: dict[str, Any] | None,
    events: list[dict[str, Any]] | None,
) -> dict[str, Any] | None:
    """Read a trial's `agent/acp.txt` kernel log for per-inference token totals.

    Returns None when `agent/acp.txt` does not exist — this trial carries
    no ACP kernel log to read, and its token fields come from Harbor's own
    `agent_result` instead. When it exists, resolves the session id to
    scope the parse to via `classify_run.resolve_session_id` (summary,
    then events, then the log's own single context id — see that
    function's docstring and this module's docstring). Returns tokens_in,
    tokens_out, and llm_inferences as None with a reason string, and
    tokens_source as None, when no session id resolves or no log line
    matches the one that does — never as zero.
    """
    if not acp_log_path.is_file():
        return None

    session_id, source, ids_seen = cr.resolve_session_id(summary, events, acp_log_path)
    if session_id is None:
        if not ids_seen:
            reason = f"{acp_log_path} has no 'LLM stream completed' lines"
        else:
            reason = (
                f"{acp_log_path}: {len(ids_seen)} distinct context ids among 'LLM stream "
                f"completed' lines ({ids_seen}) and no single session id from "
                "acp-summary.json or acp-events.jsonl to disambiguate"
            )
        return {
            "tokens_in": None,
            "tokens_out": None,
            "llm_inferences": None,
            "reason": reason,
            "tokens_source": None,
        }

    tokens_source = {
        "acp_summary": "kernel_log",
        "acp_events": "kernel_log_events_session",
        "kernel_log_single_context": "kernel_log_single_context",
    }[source]

    parsed = cr.parse_kernel_log(acp_log_path, session_id)
    lines_matched = parsed["kernel_log_lines_matched"]
    lines_in_scope = parsed["kernel_log_lines_parsed"]

    if lines_in_scope == 0:
        reason = (
            f"{acp_log_path} has no 'LLM stream completed' lines"
            if lines_matched == 0
            else f"{acp_log_path}: no 'LLM stream completed' line matched session {session_id}"
        )
        return {
            "tokens_in": None,
            "tokens_out": None,
            "llm_inferences": None,
            "reason": reason,
            "tokens_source": None,
        }

    return {
        "tokens_in": parsed["tokens_in_total"],
        "tokens_out": parsed["tokens_out_total"],
        "llm_inferences": lines_in_scope,
        "reason": None,
        "tokens_source": tokens_source,
    }


def _base_row(trial_dir: Path) -> dict[str, Any]:
    return {
        "kind": "trial",
        "trial_status": "completed",
        "task_name": None,
        "trial_name": trial_dir.name,
        "agent_name": None,
        "reward": None,
        "duration_seconds": None,
        "tokens_in": None,
        "tokens_out": None,
        "tokens_cache": None,
        "cost_usd": None,
        "exception_type": None,
        "exception_message": None,
        "turn_end_class": "n/a",
        "tool_calls_total": None,
        "permission_requests": None,
        "asks_orphaned": None,
        "llm_inferences": None,
        "tokens_source": None,
        "tokens_absent_reason": None,
        "verdict": None,
        "verdict_reason": None,
        "shell_tool_calls_total": None,
        "shell_tool_calls_foreground_true": None,
        "shell_tool_calls_kj_wait_invocations": None,
        "shell_tool_calls_raw_input_reason": None,
        "failure_detail": None,
        "timeout_last_tool_call_name": None,
        "timeout_last_tool_call_status": None,
        "timeout_seconds_since_last_event": None,
        "stalled": False,
        "ended_early": False,
    }


def summarize_trial(trial_dir: Path) -> dict[str, Any]:
    row = _base_row(trial_dir)

    result_path = trial_dir / "result.json"
    if not result_path.is_file():
        row["trial_status"] = "in_progress"
        return row

    result = load_json(result_path)

    task_name = result.get("task_name")
    trial_name = result.get("trial_name") or trial_dir.name
    agent_info = result.get("agent_info") if isinstance(result.get("agent_info"), dict) else {}
    agent_name = agent_info.get("name")
    exception_info = result.get("exception_info")
    agent_result = result.get("agent_result") if isinstance(result.get("agent_result"), dict) else {}

    reward = reward_from(result.get("verifier_result"))

    started = parse_iso(result.get("started_at"))
    finished = parse_iso(result.get("finished_at"))
    duration_seconds = finished - started if started is not None and finished is not None else None

    row.update(
        {
            "task_name": task_name,
            "trial_name": trial_name,
            "agent_name": agent_name,
            "reward": reward,
            "duration_seconds": duration_seconds,
            "tokens_in": agent_result.get("n_input_tokens"),
            "tokens_out": agent_result.get("n_output_tokens"),
            "tokens_cache": agent_result.get("n_cache_tokens"),
            "cost_usd": agent_result.get("cost_usd"),
            "exception_type": exception_info.get("exception_type") if isinstance(exception_info, dict) else None,
            "exception_message": (
                exception_info.get("exception_message") if isinstance(exception_info, dict) else None
            ),
            "tokens_source": (
                "agent_result"
                if isinstance(agent_result.get("n_input_tokens"), (int, float))
                and isinstance(agent_result.get("n_output_tokens"), (int, float))
                else None
            ),
        }
    )

    events_path = trial_dir / "agent" / "acp-events.jsonl"
    summary_path = trial_dir / "agent" / "acp-summary.json"
    acp_log_path = trial_dir / "agent" / "acp.txt"

    events = cr.load_jsonl(events_path) if events_path.is_file() else None
    summary = load_json(summary_path) if summary_path.is_file() else None

    if events is not None:
        acp = cr.analyze_run(
            events,
            summary,
            trial_result=result,
            acp_log_path=acp_log_path if acp_log_path.is_file() else None,
        )
        row["turn_end_class"] = acp["turn_end_class"]
        row["tool_calls_total"] = acp["tool_calls_total"]
        row["permission_requests"] = acp["permission_requests"]
        row["asks_orphaned"] = acp["asks_orphaned"]
        row["verdict"] = acp["verdict"]
        row["verdict_reason"] = acp["verdict_reason"]
        row["shell_tool_calls_total"] = acp["shell_tool_calls_total"]
        row["shell_tool_calls_foreground_true"] = acp["shell_tool_calls_foreground_true"]
        row["shell_tool_calls_kj_wait_invocations"] = acp["shell_tool_calls_kj_wait_invocations"]
        row["shell_tool_calls_raw_input_reason"] = acp["shell_tool_calls_raw_input_reason"]
        row["failure_detail"] = acp["failure_detail"]
        row["timeout_last_tool_call_name"] = acp["timeout_last_tool_call_name"]
        row["timeout_last_tool_call_status"] = acp["timeout_last_tool_call_status"]
        row["timeout_seconds_since_last_event"] = acp["timeout_seconds_since_last_event"]

    kernel_tokens = acp_kernel_log_tokens(acp_log_path, summary, events)
    if kernel_tokens is not None:
        row["tokens_in"] = kernel_tokens["tokens_in"]
        row["tokens_out"] = kernel_tokens["tokens_out"]
        row["llm_inferences"] = kernel_tokens["llm_inferences"]
        row["tokens_source"] = kernel_tokens["tokens_source"]
        row["tokens_absent_reason"] = kernel_tokens["reason"]

    row["stalled"] = row["turn_end_class"] in ("yielded_on_ask", "yielded_on_async") or (
        (row["asks_orphaned"] or 0) > 0
    )
    row["ended_early"] = row["turn_end_class"] != "completed_verified" and (
        reward is not None and reward < 1.0
    )

    return row


def find_trial_dirs(job_dir: Path) -> list[Path]:
    """Every directory directly under `job_dir` is a trial directory.

    Job-level metadata (`config.json`, `job.log`, `lock.json`, the job's
    own `result.json`) are files at this level, never directories — this
    holds across every real Harbor job directory checked (hello-world-*,
    tb2-fix-git-oracle, kj-*, ctl-tb2-miniswe). A trial still running has
    a directory here with no `result.json` yet; `summarize_trial` reports
    that as `trial_status: "in_progress"` rather than skipping it.
    """
    return sorted((p for p in job_dir.iterdir() if p.is_dir()), key=lambda p: p.name)


def compute_totals(rows: list[dict[str, Any]]) -> dict[str, Any]:
    n_trials = len(rows)
    completed = [r for r in rows if r["trial_status"] != "in_progress"]
    n_in_progress = n_trials - len(completed)

    rewarded = [r for r in completed if r["reward"] is not None]
    n_without_reward = len(completed) - len(rewarded)
    solved = [r for r in rewarded if r["reward"] >= 1.0]
    pass_rate = (len(solved) / len(rewarded)) if rewarded else None

    solved_with_tokens = [
        r for r in solved if isinstance(r["tokens_in"], (int, float)) and isinstance(r["tokens_out"], (int, float))
    ]
    tokens_per_solved_task = None
    if solved_with_tokens:
        total_tokens = sum(r["tokens_in"] + r["tokens_out"] for r in solved_with_tokens)
        tokens_per_solved_task = total_tokens / len(solved_with_tokens)

    solved_durations = [r["duration_seconds"] for r in solved if isinstance(r["duration_seconds"], (int, float))]
    median_duration_seconds_solved = statistics.median(solved_durations) if solved_durations else None

    ask_count = sum(r["permission_requests"] or 0 for r in completed if r["permission_requests"] is not None)
    stall_count = sum(1 for r in completed if r["stalled"])
    turns_ended_early = sum(1 for r in completed if r["ended_early"])
    agent_timeouts = sum(1 for r in completed if r["turn_end_class"] == "agent_timeout")
    turn_failures = sum(1 for r in completed if r["turn_end_class"] in ("provider_failure", "setup_failure"))

    # The driven-worker verdict line vs. Harbor's own reward: how often the
    # worker's self-report and the verifier agree. "solved" here always
    # means reward >= 1.0, read fresh per row (not the `solved` list above,
    # which is scoped to rewarded trials only and would silently exclude an
    # errored trial that still emitted a verdict).
    verdict_present = sum(1 for r in completed if r["verdict"] is not None)
    verdict_done_rows = [r for r in completed if r["verdict"] == "done"]
    verdict_done_and_solved = sum(
        1 for r in verdict_done_rows if r["reward"] is not None and r["reward"] >= 1.0
    )
    verdict_done_but_failed = len(verdict_done_rows) - verdict_done_and_solved
    verdict_not_done_but_solved = sum(
        1
        for r in completed
        if r["verdict"] != "done" and r["reward"] is not None and r["reward"] >= 1.0
    )

    return {
        "kind": "totals",
        "n_trials": n_trials,
        "n_in_progress": n_in_progress,
        "n_with_reward": len(rewarded),
        "n_without_reward": n_without_reward,
        "n_solved": len(solved),
        "pass_rate": pass_rate,
        "tokens_per_solved_task": tokens_per_solved_task,
        "n_solved_with_token_data": len(solved_with_tokens),
        "median_duration_seconds_solved": median_duration_seconds_solved,
        "ask_count": ask_count,
        "stall_count": stall_count,
        "turns_ended_early": turns_ended_early,
        "agent_timeouts": agent_timeouts,
        "turn_failures": turn_failures,
        "verdict_present": verdict_present,
        "verdict_done_and_solved": verdict_done_and_solved,
        "verdict_done_but_failed": verdict_done_but_failed,
        "verdict_not_done_but_solved": verdict_not_done_but_solved,
    }


def format_markdown(rows: list[dict[str, Any]], totals: dict[str, Any]) -> str:
    columns = [
        "task_name",
        "trial_name",
        "trial_status",
        "agent_name",
        "reward",
        "turn_end_class",
        "verdict",
        "tool_calls_total",
        "shell_tool_calls_total",
        "shell_tool_calls_foreground_true",
        "shell_tool_calls_kj_wait_invocations",
        "permission_requests",
        "asks_orphaned",
        "tokens_in",
        "tokens_out",
        "llm_inferences",
        "tokens_source",
        "cost_usd",
        "duration_seconds",
        "exception_type",
        "failure_detail",
        "timeout_last_tool_call_name",
        "timeout_last_tool_call_status",
        "timeout_seconds_since_last_event",
    ]
    lines = ["| " + " | ".join(columns) + " |", "|" + "---|" * len(columns)]
    for row in rows:
        cells = [str(row.get(col, "")) if row.get(col) is not None else "" for col in columns]
        lines.append("| " + " | ".join(cells) + " |")

    lines.append("")
    lines.append("## Totals")
    lines.append("")
    for key in (
        "n_trials",
        "n_in_progress",
        "n_with_reward",
        "n_without_reward",
        "n_solved",
        "pass_rate",
        "tokens_per_solved_task",
        "n_solved_with_token_data",
        "median_duration_seconds_solved",
        "ask_count",
        "stall_count",
        "turns_ended_early",
        "agent_timeouts",
        "turn_failures",
        "verdict_present",
        "verdict_done_and_solved",
        "verdict_done_but_failed",
        "verdict_not_done_but_solved",
    ):
        lines.append(f"- **{key}**: {totals[key]}")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Summarize one Harbor job directory: one row per trial, plus totals.",
    )
    parser.add_argument("job_dir", type=Path, help="Harbor job directory (contains one subdirectory per trial).")
    parser.add_argument(
        "--format",
        choices=("markdown", "jsonl"),
        default="markdown",
        help="Output format. markdown (default) prints a table plus a totals section; "
        "jsonl prints one JSON object per trial, tagged \"kind\": \"trial\", followed "
        "by one \"kind\": \"totals\" object.",
    )
    args = parser.parse_args(argv)

    if not args.job_dir.is_dir():
        raise SystemExit(f"{args.job_dir} is not a directory")

    trial_dirs = find_trial_dirs(args.job_dir)
    if not trial_dirs:
        raise SystemExit(f"no trial subdirectory found under {args.job_dir}")

    rows = [summarize_trial(trial_dir) for trial_dir in trial_dirs]
    totals = compute_totals(rows)

    if args.format == "jsonl":
        for row in rows:
            print(json.dumps(row, sort_keys=True))
        print(json.dumps(totals, sort_keys=True))
    else:
        print(format_markdown(rows, totals))
    return 0


if __name__ == "__main__":
    sys.exit(main())

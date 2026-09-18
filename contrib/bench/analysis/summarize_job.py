#!/usr/bin/env python3
"""Summarize one Harbor job directory: one row per trial, plus totals.

Reads `result.json` from each trial subdirectory of a Harbor job (the
layout in /home/atobey/src/bench-work/harbor/NOTES.md, "Result file
layout"): `JOB_DIR/<task>__<rand>/result.json`. When a trial's `agent/`
directory holds `acp-events.jsonl` and `acp-summary.json` (an ACP agent's
logs), this also runs classify_run's analysis on them and folds in
`turn_end_class` and the gate/ask counts. A non-ACP agent (oracle, nop,
mini-swe-agent, ...) has no such logs; its row reports `turn_end_class`
as "n/a" and fills tokens/cost from Harbor's own `agent_result`, when
present.

When a trial's `agent/acp.txt` exists (a kaijutsu kernel log riding the
agent's stderr, always present for a Harbor ACP trial since the adapter
sets `RUST_LOG=info`), this parses it with classify_run's
`parse_kernel_log`, scoped to the session id in `acp-summary.json`, and
fills `tokens_in`, `tokens_out`, and `llm_inferences` from it, overriding
whatever `agent_result` carried. `tokens_source` reports where a trial's
tokens came from: `"kernel_log"`, `"agent_result"`, or null when neither
had data. When `acp.txt` exists but no log line matches the trial's
session, tokens and `llm_inferences` are null with `tokens_absent_reason`
set to why — never reported as zero.

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

Totals:
    pass_rate            trials with reward >= 1.0, over trials with a
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
                          specifically.
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


def acp_analysis_for_trial(trial_dir: Path) -> dict[str, Any] | None:
    events_path = trial_dir / "agent" / "acp-events.jsonl"
    summary_path = trial_dir / "agent" / "acp-summary.json"
    if not events_path.is_file() or not summary_path.is_file():
        return None
    events = cr.load_jsonl(events_path)
    summary = cr.load_summary(summary_path)
    return cr.analyze_run(events, summary)


def acp_kernel_log_tokens(trial_dir: Path) -> dict[str, Any] | None:
    """Read a trial's `agent/acp.txt` kernel log for per-inference token totals.

    Returns None when `agent/acp.txt` does not exist — this trial carries
    no ACP kernel log to read, and its token fields come from Harbor's own
    `agent_result` instead. When it exists, scopes the parse to the
    session id from `agent/acp-summary.json`'s `session.sessionId` (missing
    or empty is a hard error: `acp.txt` without a session id to scope it to
    is corrupt trial data, not an absent-ACP trial). Returns tokens_in,
    tokens_out, and llm_inferences as None with a reason string when no
    log line matches that session — never as zero.
    """
    acp_log_path = trial_dir / "agent" / "acp.txt"
    if not acp_log_path.is_file():
        return None

    summary_path = trial_dir / "agent" / "acp-summary.json"
    if not summary_path.is_file():
        raise SystemExit(
            f"{acp_log_path} exists but {summary_path} does not; cannot scope "
            "kernel log token totals to a session id"
        )
    summary = load_json(summary_path)
    session = summary.get("session")
    session_id = session.get("sessionId") if isinstance(session, dict) else None
    if not isinstance(session_id, str) or not session_id:
        raise SystemExit(
            f"{summary_path}: no session.sessionId; cannot scope {acp_log_path}'s "
            "kernel log token totals to this trial's run"
        )

    parsed = cr.parse_kernel_log(acp_log_path, session_id)
    lines_matched = parsed["kernel_log_lines_matched"]
    lines_in_scope = parsed["kernel_log_lines_parsed"]

    if lines_in_scope == 0:
        reason = (
            f"{acp_log_path} has no 'LLM stream completed' lines"
            if lines_matched == 0
            else f"{acp_log_path}: no 'LLM stream completed' line matched session {session_id}"
        )
        return {"tokens_in": None, "tokens_out": None, "llm_inferences": None, "reason": reason}

    return {
        "tokens_in": parsed["tokens_in_total"],
        "tokens_out": parsed["tokens_out_total"],
        "llm_inferences": lines_in_scope,
        "reason": None,
    }


def summarize_trial(trial_dir: Path) -> dict[str, Any]:
    result = load_json(trial_dir / "result.json")

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

    row: dict[str, Any] = {
        "kind": "trial",
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
        "turn_end_class": "n/a",
        "tool_calls_total": None,
        "permission_requests": None,
        "asks_orphaned": None,
        "llm_inferences": None,
        "tokens_source": (
            "agent_result"
            if isinstance(agent_result.get("n_input_tokens"), (int, float))
            and isinstance(agent_result.get("n_output_tokens"), (int, float))
            else None
        ),
        "tokens_absent_reason": None,
        "verdict": None,
        "verdict_reason": None,
        "shell_tool_calls_total": None,
        "shell_tool_calls_foreground_true": None,
        "shell_tool_calls_kj_wait_invocations": None,
        "shell_tool_calls_raw_input_reason": None,
    }

    acp = acp_analysis_for_trial(trial_dir)
    if acp is not None:
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

    kernel_tokens = acp_kernel_log_tokens(trial_dir)
    if kernel_tokens is not None:
        row["tokens_in"] = kernel_tokens["tokens_in"]
        row["tokens_out"] = kernel_tokens["tokens_out"]
        row["llm_inferences"] = kernel_tokens["llm_inferences"]
        row["tokens_source"] = "kernel_log" if kernel_tokens["tokens_in"] is not None else None
        row["tokens_absent_reason"] = kernel_tokens["reason"]

    row["stalled"] = row["turn_end_class"] in ("yielded_on_ask", "yielded_on_async") or (
        (row["asks_orphaned"] or 0) > 0
    )
    row["ended_early"] = row["turn_end_class"] != "completed_verified" and (
        reward is not None and reward < 1.0
    )

    return row


def find_trial_dirs(job_dir: Path) -> list[Path]:
    return sorted(
        (p.parent for p in job_dir.glob("*/result.json")),
        key=lambda p: p.name,
    )


def compute_totals(rows: list[dict[str, Any]]) -> dict[str, Any]:
    n_trials = len(rows)
    rewarded = [r for r in rows if r["reward"] is not None]
    n_without_reward = n_trials - len(rewarded)
    solved = [r for r in rewarded if r["reward"] >= 1.0]
    pass_rate = (len(solved) / len(rewarded)) if rewarded else None

    solved_with_tokens = [
        r for r in solved if isinstance(r["tokens_in"], (int, float)) and isinstance(r["tokens_out"], (int, float))
    ]
    tokens_per_solved_task = None
    if solved_with_tokens:
        total_tokens = sum(r["tokens_in"] + r["tokens_out"] for r in solved_with_tokens)
        tokens_per_solved_task = total_tokens / len(solved_with_tokens)

    ask_count = sum(r["permission_requests"] or 0 for r in rows if r["permission_requests"] is not None)
    stall_count = sum(1 for r in rows if r["stalled"])
    turns_ended_early = sum(1 for r in rows if r["ended_early"])

    # The driven-worker verdict line vs. Harbor's own reward: how often the
    # worker's self-report and the verifier agree. "solved" here always
    # means reward >= 1.0, read fresh per row (not the `solved` list above,
    # which is scoped to rewarded trials only and would silently exclude an
    # errored trial that still emitted a verdict).
    verdict_present = sum(1 for r in rows if r["verdict"] is not None)
    verdict_done_rows = [r for r in rows if r["verdict"] == "done"]
    verdict_done_and_solved = sum(
        1 for r in verdict_done_rows if r["reward"] is not None and r["reward"] >= 1.0
    )
    verdict_done_but_failed = len(verdict_done_rows) - verdict_done_and_solved
    verdict_not_done_but_solved = sum(
        1
        for r in rows
        if r["verdict"] != "done" and r["reward"] is not None and r["reward"] >= 1.0
    )

    return {
        "kind": "totals",
        "n_trials": n_trials,
        "n_with_reward": len(rewarded),
        "n_without_reward": n_without_reward,
        "n_solved": len(solved),
        "pass_rate": pass_rate,
        "tokens_per_solved_task": tokens_per_solved_task,
        "n_solved_with_token_data": len(solved_with_tokens),
        "ask_count": ask_count,
        "stall_count": stall_count,
        "turns_ended_early": turns_ended_early,
        "verdict_present": verdict_present,
        "verdict_done_and_solved": verdict_done_and_solved,
        "verdict_done_but_failed": verdict_done_but_failed,
        "verdict_not_done_but_solved": verdict_not_done_but_solved,
    }


def format_markdown(rows: list[dict[str, Any]], totals: dict[str, Any]) -> str:
    columns = [
        "task_name",
        "trial_name",
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
        "n_with_reward",
        "n_without_reward",
        "n_solved",
        "pass_rate",
        "tokens_per_solved_task",
        "n_solved_with_token_data",
        "ask_count",
        "stall_count",
        "turns_ended_early",
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
        raise SystemExit(f"no trial subdirectory with a result.json found under {args.job_dir}")

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

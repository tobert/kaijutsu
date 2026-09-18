#!/usr/bin/env python3
"""Tests for summarize_job.py.

Run with:
    python3 -m unittest test_summarize_job -v

Builds small synthetic job directories for the oracle/nop (no ACP logs)
and ACP-agent (with acp-events.jsonl/acp-summary.json under agent/) paths,
plus real-data tests against the existing oracle job dirs under
/home/atobey/src/bench-work/harbor/jobs, skipped with a clear message if
that tree is not present.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import summarize_job as sj  # noqa: E402

REAL_JOBS_DIR = Path("/home/atobey/src/bench-work/harbor/jobs")


def write_trial_result(trial_dir: Path, **overrides) -> None:
    result = {
        "task_name": "harbor/hello-world",
        "trial_name": trial_dir.name,
        "agent_info": {"name": "oracle", "version": "1.0.0", "model_info": None},
        "agent_result": {
            "n_input_tokens": None,
            "n_cache_tokens": None,
            "n_output_tokens": None,
            "cost_usd": None,
        },
        "verifier_result": {"rewards": {"reward": 1.0}},
        "exception_info": None,
        "started_at": "2026-09-18T14:00:00Z",
        "finished_at": "2026-09-18T14:00:10Z",
    }
    result.update(overrides)
    trial_dir.mkdir(parents=True, exist_ok=True)
    (trial_dir / "result.json").write_text(json.dumps(result))


KERNEL_LOG_TEMPLATE = (
    '2026-09-18T14:28:10.500818Z  INFO llm.turn{{context.id={ctx}}}: '
    'kaijutsu_kernel::runtime::llm_stream: LLM stream completed: '
    'stop_reason=Some("{stop}"), tokens_in=Some({tin}), tokens_out=Some({tout})\n'
)


def write_acp_kernel_log(trial_dir: Path, lines: str) -> None:
    agent_dir = trial_dir / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    (agent_dir / "acp.txt").write_text(lines)


def write_acp_logs(
    trial_dir: Path,
    *,
    session_id: str = "s" * 32,
    final_message: str = "Done.",
    t1_raw_input: dict | None = None,
    t2_raw_input: dict | None = None,
) -> None:
    agent_dir = trial_dir / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    t1_update = {
        "sessionUpdate": "tool_call",
        "toolCallId": "t1",
        "kind": "edit",
        "title": "shell_write",
    }
    if t1_raw_input is not None:
        t1_update["rawInput"] = t1_raw_input
    t2_update = {
        "sessionUpdate": "tool_call",
        "toolCallId": "t2",
        "kind": "execute",
        "title": "shell",
    }
    if t2_raw_input is not None:
        t2_update["rawInput"] = t2_raw_input
    events = [
        {
            "event_type": "session_update",
            "payload": {"session_id": session_id, "update": t1_update},
        },
        {
            "event_type": "session_update",
            "payload": {
                "session_id": session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "t1",
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "wrote fix"}}],
                },
            },
        },
        {
            "event_type": "session_update",
            "payload": {"session_id": session_id, "update": t2_update},
        },
        {
            "event_type": "session_update",
            "payload": {
                "session_id": session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "t2",
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "tests pass"}}],
                },
            },
        },
        {
            "event_type": "session_update",
            "payload": {
                "session_id": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": final_message},
                },
            },
        },
    ]
    (agent_dir / "acp-events.jsonl").write_text("\n".join(json.dumps(e) for e in events) + "\n")
    summary = {
        "instruction": "fix it",
        "session": {"sessionId": session_id},
        "prompt_response": {"stopReason": "end_turn"},
        "permissions_requested": 0,
    }
    (agent_dir / "acp-summary.json").write_text(json.dumps(summary))


class TestSyntheticJob(unittest.TestCase):
    def test_non_acp_trial_reports_na_class_and_totals(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            write_trial_result(job_dir / "hello-world__aaa", agent_info={"name": "nop", "version": "1", "model_info": None}, verifier_result={"rewards": {"reward": 0.0}})
            rows = [sj.summarize_trial(d) for d in sj.find_trial_dirs(job_dir)]
            self.assertEqual(len(rows), 1)
            row = rows[0]
            self.assertEqual(row["turn_end_class"], "n/a")
            self.assertEqual(row["reward"], 0.0)
            self.assertTrue(row["ended_early"])
            self.assertFalse(row["stalled"])

    def test_acp_trial_reports_turn_end_class_from_events(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "fix-git__bbb"
            write_trial_result(
                trial_dir,
                agent_info={"name": "acp", "version": "0.1.0", "model_info": None},
                verifier_result={"rewards": {"reward": 1.0}},
            )
            write_acp_logs(trial_dir)
            rows = [sj.summarize_trial(d) for d in sj.find_trial_dirs(job_dir)]
            self.assertEqual(len(rows), 1)
            row = rows[0]
            self.assertEqual(row["turn_end_class"], "completed_verified")
            self.assertEqual(row["tool_calls_total"], 2)
            self.assertFalse(row["ended_early"])

    def test_errored_trial_excluded_from_pass_rate_denominator(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            write_trial_result(
                job_dir / "hello-world__ccc",
                verifier_result=None,
                exception_info={"exception_type": "RuntimeError", "exception_message": "boom", "exception_traceback": "..."},
            )
            rows = [sj.summarize_trial(d) for d in sj.find_trial_dirs(job_dir)]
            totals = sj.compute_totals(rows)
            self.assertIsNone(rows[0]["reward"])
            self.assertEqual(totals["n_with_reward"], 0)
            self.assertEqual(totals["n_without_reward"], 1)
            self.assertIsNone(totals["pass_rate"])
            self.assertEqual(rows[0]["exception_type"], "RuntimeError")

    def test_tokens_per_solved_task(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            write_trial_result(
                job_dir / "t__1",
                agent_result={"n_input_tokens": 100, "n_cache_tokens": 0, "n_output_tokens": 50, "cost_usd": 0.01},
                verifier_result={"rewards": {"reward": 1.0}},
            )
            write_trial_result(
                job_dir / "t__2",
                agent_result={"n_input_tokens": 200, "n_cache_tokens": 0, "n_output_tokens": 100, "cost_usd": 0.02},
                verifier_result={"rewards": {"reward": 0.0}},
            )
            rows = [sj.summarize_trial(d) for d in sj.find_trial_dirs(job_dir)]
            totals = sj.compute_totals(rows)
            # Only the solved trial (reward 1.0, 150 tokens) counts.
            self.assertEqual(totals["tokens_per_solved_task"], 150.0)
            self.assertEqual(totals["n_solved_with_token_data"], 1)

    def test_no_trials_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            (job_dir / "result.json").write_text("{}")
            with self.assertRaises(SystemExit):
                sj.main([str(job_dir)])

    def test_cli_jsonl_output_is_valid_json_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            write_trial_result(job_dir / "hello-world__ddd")
            script = Path(__file__).resolve().parent / "summarize_job.py"
            proc = subprocess.run(
                [sys.executable, str(script), str(job_dir), "--format", "jsonl"],
                capture_output=True,
                text=True,
                check=True,
            )
            lines = [json.loads(line) for line in proc.stdout.strip().splitlines()]
            self.assertEqual(lines[0]["kind"], "trial")
            self.assertEqual(lines[-1]["kind"], "totals")


class TestVerdictAndShellStats(unittest.TestCase):
    def test_row_carries_verdict_and_shell_stats_from_classify_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "fix-git__verdict"
            write_trial_result(
                trial_dir,
                agent_info={"name": "acp", "version": "0.1.0", "model_info": None},
                verifier_result={"rewards": {"reward": 1.0}},
            )
            write_acp_logs(
                trial_dir,
                final_message="Fixed and verified.\nRESULT: done",
                t1_raw_input={"command": "sed -i s/x/y/ file.py"},
                t2_raw_input={"command": "kj wait --operation abc; pytest", "foreground": True},
            )
            row = sj.summarize_trial(trial_dir)
            self.assertEqual(row["verdict"], "done")
            self.assertIsNone(row["verdict_reason"])
            self.assertEqual(row["shell_tool_calls_total"], 2)
            self.assertEqual(row["shell_tool_calls_foreground_true"], 1)
            self.assertEqual(row["shell_tool_calls_kj_wait_invocations"], 1)
            self.assertIsNone(row["shell_tool_calls_raw_input_reason"])

    def test_non_acp_trial_leaves_verdict_and_shell_stats_null(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            write_trial_result(job_dir / "hello-world__novacp")
            row = sj.summarize_trial(job_dir / "hello-world__novacp")
            self.assertIsNone(row["verdict"])
            self.assertIsNone(row["shell_tool_calls_total"])

    def test_verdict_totals_agree_done_and_solved(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "t__agree"
            write_trial_result(
                trial_dir,
                agent_info={"name": "acp", "version": "0.1.0", "model_info": None},
                verifier_result={"rewards": {"reward": 1.0}},
            )
            write_acp_logs(trial_dir, final_message="Done.\nRESULT: done")
            rows = [sj.summarize_trial(d) for d in sj.find_trial_dirs(job_dir)]
            totals = sj.compute_totals(rows)
            self.assertEqual(totals["verdict_present"], 1)
            self.assertEqual(totals["verdict_done_and_solved"], 1)
            self.assertEqual(totals["verdict_done_but_failed"], 0)
            self.assertEqual(totals["verdict_not_done_but_solved"], 0)

    def test_verdict_totals_done_but_failed(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "t__wrong"
            write_trial_result(
                trial_dir,
                agent_info={"name": "acp", "version": "0.1.0", "model_info": None},
                verifier_result={"rewards": {"reward": 0.0}},
            )
            write_acp_logs(trial_dir, final_message="I fixed it.\nRESULT: done")
            rows = [sj.summarize_trial(d) for d in sj.find_trial_dirs(job_dir)]
            totals = sj.compute_totals(rows)
            self.assertEqual(totals["verdict_done_and_solved"], 0)
            self.assertEqual(totals["verdict_done_but_failed"], 1)
            self.assertEqual(totals["verdict_not_done_but_solved"], 0)

    def test_verdict_totals_not_done_but_solved(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "t__undersold"
            write_trial_result(
                trial_dir,
                agent_info={"name": "acp", "version": "0.1.0", "model_info": None},
                verifier_result={"rewards": {"reward": 1.0}},
            )
            write_acp_logs(trial_dir, final_message="Ran out of ideas.\nRESULT: gave up — stuck")
            rows = [sj.summarize_trial(d) for d in sj.find_trial_dirs(job_dir)]
            totals = sj.compute_totals(rows)
            self.assertEqual(totals["verdict_present"], 1)
            self.assertEqual(totals["verdict_done_and_solved"], 0)
            self.assertEqual(totals["verdict_done_but_failed"], 0)
            self.assertEqual(totals["verdict_not_done_but_solved"], 1)

    def test_verdict_totals_absent_verdict_counts_toward_not_done_but_solved(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "t__noverdict"
            write_trial_result(
                trial_dir,
                agent_info={"name": "acp", "version": "0.1.0", "model_info": None},
                verifier_result={"rewards": {"reward": 1.0}},
            )
            write_acp_logs(trial_dir, final_message="Done, no verdict line here.")
            rows = [sj.summarize_trial(d) for d in sj.find_trial_dirs(job_dir)]
            totals = sj.compute_totals(rows)
            self.assertEqual(totals["verdict_present"], 0)
            self.assertEqual(totals["verdict_not_done_but_solved"], 1)


def dashed(session_id: str) -> str:
    """Render a 32-hex-char session id as a dashed UUID, as kernel log lines carry it."""
    return f"{session_id[0:8]}-{session_id[8:12]}-{session_id[12:16]}-{session_id[16:20]}-{session_id[20:32]}"


class TestAcpKernelLogTokens(unittest.TestCase):
    SESSION_ID = "01a0b4eae5567b7180dfd2d5d59e40d5"

    def test_fills_tokens_and_inference_count_from_matching_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "fix-git__aaa"
            write_trial_result(
                trial_dir,
                agent_info={"name": "kaijutsu-solo-acp", "version": "0.1.0", "model_info": None},
                agent_result={"n_input_tokens": None, "n_cache_tokens": None, "n_output_tokens": None, "cost_usd": None},
                verifier_result={"rewards": {"reward": 1.0}},
            )
            write_acp_logs(trial_dir, session_id=self.SESSION_ID)
            write_acp_kernel_log(
                trial_dir,
                KERNEL_LOG_TEMPLATE.format(ctx=dashed(self.SESSION_ID), stop="tool_calls", tin=100, tout=10)
                + KERNEL_LOG_TEMPLATE.format(ctx=dashed(self.SESSION_ID), stop="end_turn", tin=200, tout=20)
                # a different session's line must not be counted
                + KERNEL_LOG_TEMPLATE.format(ctx=dashed("f" * 32), stop="end_turn", tin=999, tout=999),
            )
            row = sj.summarize_trial(trial_dir)
            self.assertEqual(row["tokens_in"], 300)
            self.assertEqual(row["tokens_out"], 30)
            self.assertEqual(row["llm_inferences"], 2)
            self.assertEqual(row["tokens_source"], "kernel_log")
            self.assertIsNone(row["tokens_absent_reason"])

    def test_zero_matching_lines_reports_absent_not_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "fix-git__bbb"
            write_trial_result(trial_dir, verifier_result={"rewards": {"reward": 0.0}})
            write_acp_logs(trial_dir, session_id=self.SESSION_ID)
            # A log with LLM stream completed lines, but none for this session.
            write_acp_kernel_log(
                trial_dir,
                KERNEL_LOG_TEMPLATE.format(ctx=dashed("f" * 32), stop="end_turn", tin=999, tout=999),
            )
            row = sj.summarize_trial(trial_dir)
            self.assertIsNone(row["tokens_in"])
            self.assertIsNone(row["tokens_out"])
            self.assertIsNone(row["llm_inferences"])
            self.assertIsNone(row["tokens_source"])
            self.assertIsNotNone(row["tokens_absent_reason"])
            self.assertNotEqual(row["tokens_in"], 0)

    def test_empty_kernel_log_reports_absent_not_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "fix-git__ccc"
            write_trial_result(trial_dir, verifier_result={"rewards": {"reward": 0.0}})
            write_acp_logs(trial_dir, session_id=self.SESSION_ID)
            write_acp_kernel_log(trial_dir, "no matching lines at all\n")
            row = sj.summarize_trial(trial_dir)
            self.assertIsNone(row["tokens_in"])
            self.assertIsNone(row["tokens_out"])
            self.assertIsNotNone(row["tokens_absent_reason"])

    def test_no_acp_txt_leaves_tokens_from_agent_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "hello-world__ddd"
            write_trial_result(
                trial_dir,
                agent_result={"n_input_tokens": 5, "n_cache_tokens": 0, "n_output_tokens": 2, "cost_usd": 0.0},
            )
            row = sj.summarize_trial(trial_dir)
            self.assertEqual(row["tokens_in"], 5)
            self.assertEqual(row["tokens_out"], 2)
            self.assertIsNone(row["llm_inferences"])
            self.assertEqual(row["tokens_source"], "agent_result")
            self.assertIsNone(row["tokens_absent_reason"])

    def test_acp_txt_without_summary_json_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            job_dir = Path(tmp)
            trial_dir = job_dir / "fix-git__eee"
            write_trial_result(trial_dir)
            write_acp_kernel_log(
                trial_dir,
                KERNEL_LOG_TEMPLATE.format(ctx=dashed(self.SESSION_ID), stop="end_turn", tin=1, tout=1),
            )
            with self.assertRaises(SystemExit):
                sj.summarize_trial(trial_dir)


class TestRealOracleJobs(unittest.TestCase):
    def _run(self, job_name: str, fmt: str = "jsonl") -> list[dict]:
        job_dir = REAL_JOBS_DIR / job_name
        if not job_dir.is_dir():
            self.skipTest(f"real fixture not present: {job_dir}")
        script = Path(__file__).resolve().parent / "summarize_job.py"
        proc = subprocess.run(
            [sys.executable, str(script), str(job_dir), "--format", fmt],
            capture_output=True,
            text=True,
            check=True,
        )
        return [json.loads(line) for line in proc.stdout.strip().splitlines()]

    def test_hello_world_oracle(self):
        lines = self._run("hello-world-oracle")
        trials = [line for line in lines if line["kind"] == "trial"]
        totals = [line for line in lines if line["kind"] == "totals"][0]
        self.assertEqual(len(trials), 1)
        self.assertEqual(trials[0]["reward"], 1.0)
        self.assertEqual(totals["pass_rate"], 1.0)

    def test_hello_world_nop(self):
        lines = self._run("hello-world-nop")
        trials = [line for line in lines if line["kind"] == "trial"]
        self.assertEqual(trials[0]["reward"], 0.0)
        self.assertTrue(trials[0]["ended_early"])

    def test_hello_world_oracle_attempt1_failed(self):
        lines = self._run("hello-world-oracle-attempt1-failed")
        trials = [line for line in lines if line["kind"] == "trial"]
        self.assertIsNone(trials[0]["reward"])
        self.assertEqual(trials[0]["exception_type"], "RuntimeError")

    def test_tb2_fix_git_oracle(self):
        lines = self._run("tb2-fix-git-oracle")
        trials = [line for line in lines if line["kind"] == "trial"]
        self.assertEqual(trials[0]["task_name"], "fix-git")
        self.assertEqual(trials[0]["reward"], 1.0)

    def test_kj_fixgit_1_token_totals_from_kernel_log(self):
        lines = self._run("kj-fixgit-1")
        trials = [line for line in lines if line["kind"] == "trial"]
        totals = [line for line in lines if line["kind"] == "totals"][0]
        self.assertEqual(len(trials), 1)
        self.assertEqual(trials[0]["llm_inferences"], 42)
        self.assertEqual(trials[0]["tokens_in"], 1_368_224)
        self.assertEqual(trials[0]["tokens_out"], 20_935)
        self.assertEqual(trials[0]["tokens_source"], "kernel_log")
        self.assertIsNone(trials[0]["tokens_absent_reason"])
        self.assertEqual(totals["tokens_per_solved_task"], 1_368_224 + 20_935)
        self.assertEqual(totals["n_solved_with_token_data"], 1)

    def test_kj_hw_1_token_totals_from_kernel_log(self):
        lines = self._run("kj-hw-1")
        trials = [line for line in lines if line["kind"] == "trial"]
        totals = [line for line in lines if line["kind"] == "totals"][0]
        self.assertEqual(len(trials), 1)
        self.assertEqual(trials[0]["llm_inferences"], 3)
        self.assertEqual(trials[0]["tokens_in"], 42_497)
        self.assertEqual(trials[0]["tokens_out"], 158)
        self.assertEqual(totals["tokens_per_solved_task"], 42_497 + 158)
        self.assertEqual(totals["n_solved_with_token_data"], 1)


if __name__ == "__main__":
    unittest.main()

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


def write_acp_logs(trial_dir: Path, *, session_id: str = "s" * 32) -> None:
    agent_dir = trial_dir / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    events = [
        {
            "event_type": "session_update",
            "payload": {
                "session_id": session_id,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t1",
                    "kind": "edit",
                    "title": "shell_write",
                },
            },
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
            "payload": {
                "session_id": session_id,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t2",
                    "kind": "execute",
                    "title": "shell",
                },
            },
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
                    "content": {"type": "text", "text": "Done."},
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


if __name__ == "__main__":
    unittest.main()

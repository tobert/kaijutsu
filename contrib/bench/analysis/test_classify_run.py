#!/usr/bin/env python3
"""Tests for classify_run.py.

Run with:
    python3 -m unittest test_classify_run -v
or:
    python3 test_classify_run.py -v

Most tests build small synthetic event streams, one per turn_end_class, so
each class's trigger condition is pinned down independently of any real
run. One test (TestRealData) runs against the real `ds-run-2` fixture
under /home/atobey/src/bench-work if it is present, and is skipped with a
clear message otherwise.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import classify_run as cr  # noqa: E402

REAL_RUN_DIR = Path("/home/atobey/src/bench-work/kernels/ds-02/tasks/ds-run-2")
REAL_KERNEL_LOG = Path("/home/atobey/src/bench-work/kernels/ds-02/logs/kernel.log")


def session_update(update: dict) -> dict:
    return {
        "event_type": "session_update",
        "payload": {"session_id": "s1", "update": update},
    }


def message_chunk(text: str) -> dict:
    return session_update(
        {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
    )


def thought_chunk(text: str) -> dict:
    return session_update(
        {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": text}}
    )


def tool_call(tool_call_id: str, *, kind: str | None = None, title: str | None = None) -> dict:
    update = {"sessionUpdate": "tool_call", "toolCallId": tool_call_id}
    if kind is not None:
        update["kind"] = kind
    if title is not None:
        update["title"] = title
    return session_update(update)


def tool_call_update(tool_call_id: str, *, status: str, text: str | None = None) -> dict:
    update = {"sessionUpdate": "tool_call_update", "toolCallId": tool_call_id, "status": status}
    if text is not None:
        update["content"] = [{"type": "content", "content": {"type": "text", "text": text}}]
    return session_update(update)


def request_permission(ask_id: str, title: str = "shell_write: 1 statement(s)") -> dict:
    return {
        "event_type": "request_permission",
        "payload": {
            "session_id": "s1",
            "tool_call": {"toolCallId": ask_id, "title": title},
            "options": [
                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
            ],
        },
    }


def base_summary(**overrides) -> dict:
    summary = {
        "workspace": "/tmp/task",
        "instruction": "do the thing",
        "session": {"sessionId": "0123456789abcdef0123456789abcdef"},
        "prompt_response": {"stopReason": "end_turn"},
        "permissions_requested": 0,
    }
    summary.update(overrides)
    return summary


class TestStopReasonClasses(unittest.TestCase):
    def test_token_ceiling(self):
        summary = base_summary(prompt_response={"stopReason": "max_tokens"})
        cls, evidence = cr.classify_turn_end(
            summary=summary,
            tools_by_recency=[],
            tool_states={},
            message_text="",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "token_ceiling")
        self.assertEqual(evidence["stop_reason"], "max_tokens")

    def test_iteration_cap_via_stop_reason(self):
        summary = base_summary(prompt_response={"stopReason": "max_turn_requests"})
        cls, _ = cr.classify_turn_end(
            summary=summary,
            tools_by_recency=[],
            tool_states={},
            message_text="",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "iteration_cap")

    def test_iteration_cap_via_halt_text(self):
        # end_turn, but the model's own prose says it hit the turn cap.
        summary = base_summary(prompt_response={"stopReason": "end_turn"})
        cls, evidence = cr.classify_turn_end(
            summary=summary,
            tools_by_recency=[],
            tool_states={},
            message_text="Paused after 40 turn requests; more remains.",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "iteration_cap")
        self.assertEqual(evidence["halt_text"], "Paused after")

    def test_cancelled(self):
        summary = base_summary(prompt_response={"stopReason": "cancelled"})
        cls, _ = cr.classify_turn_end(
            summary=summary,
            tools_by_recency=[],
            tool_states={},
            message_text="",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "cancelled")

    def test_unclassified_stop_reason(self):
        # "refusal" is a real ACP StopReason this scheme does not name.
        summary = base_summary(prompt_response={"stopReason": "refusal"})
        cls, evidence = cr.classify_turn_end(
            summary=summary,
            tools_by_recency=[],
            tool_states={},
            message_text="",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "unclassified_stop_reason")
        self.assertEqual(evidence["stop_reason"], "refusal")

    def test_setup_failure_no_session(self):
        summary = {"instruction": "x", "error": {"type": "ConnectionError", "message": "closed"}}
        cls, evidence = cr.classify_turn_end(
            summary=summary,
            tools_by_recency=[],
            tool_states={},
            message_text="",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "setup_failure")
        self.assertFalse(evidence["session_established"])

    def test_provider_failure_with_session(self):
        summary = base_summary(error={"type": "RequestError", "message": "boom"})
        cls, evidence = cr.classify_turn_end(
            summary=summary,
            tools_by_recency=[],
            tool_states={},
            message_text="",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "provider_failure")
        self.assertTrue(evidence["session_established"])


class TestEndTurnFamily(unittest.TestCase):
    def test_yielded_on_ask(self):
        tool_states = {
            "t1": cr.ToolCallState("t1", 0, kind="edit", title="shell_write"),
        }
        tool_states["t1"].texts.append(
            "gate for shell_write is waiting on its reviewer: nothing was run. "
            "(ask 01a0b4ea-fafb-79a0-bec8-7ecb6ee29da6, pending)"
        )
        cls, evidence = cr.classify_turn_end(
            summary=base_summary(),
            tools_by_recency=["t1"],
            tool_states=tool_states,
            message_text="Waiting on approval.",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id="t1",
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "yielded_on_ask")
        self.assertIn("is waiting on its reviewer", evidence["matched_markers"])

    def test_yielded_on_async(self):
        receipt_text = json.dumps(
            [{"receipt": {"operation_id": "op-1"}, "status": "running", "exit_code": None}]
        )
        tool_states = {
            "t1": cr.ToolCallState("t1", 0, kind=None, title="read_shell_operation"),
        }
        tool_states["t1"].texts.append(receipt_text)
        cls, evidence = cr.classify_turn_end(
            summary=base_summary(),
            tools_by_recency=["t1"],
            tool_states=tool_states,
            message_text="Still running, reporting now.",
            thought_text="",
            unawaited_async_operations=["op-1"],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "yielded_on_async")
        self.assertEqual(evidence["unawaited_operations"], ["op-1"])

    def test_output_starved(self):
        tool_states = {
            "t1": cr.ToolCallState("t1", 0, kind="execute", title="shell"),
        }
        tool_states["t1"].status = "completed"
        tool_states["t1"].texts.append(
            json.dumps({"stdout": "a" * 10, "truncated": True, "exit_code": 0})
        )
        cls, evidence = cr.classify_turn_end(
            summary=base_summary(),
            tools_by_recency=["t1"],
            tool_states=tool_states,
            message_text="Ran the command; output was cut off.",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[{"tool_call_id": "t1", "matched_markers": ['"truncated":true']}],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "output_starved")
        self.assertEqual(evidence["spilled_tool_results"][0]["tool_call_id"], "t1")

    def test_completed_verified(self):
        tool_states = {
            "edit1": cr.ToolCallState("edit1", 0, kind="edit", title="shell_write"),
            "exec1": cr.ToolCallState("exec1", 1, kind="execute", title="shell"),
        }
        tool_states["exec1"].status = "completed"
        tool_states["exec1"].texts.append("Ran 4 tests, OK, exit 0")
        cls, evidence = cr.classify_turn_end(
            summary=base_summary(),
            tools_by_recency=["edit1", "exec1"],
            tool_states=tool_states,
            message_text="Fixed and verified.",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id="edit1",
            successful_execution_after_edit=True,
        )
        self.assertEqual(cls, "completed_verified")
        self.assertEqual(evidence["last_edit_tool_call_id"], "edit1")

    def test_ended_unverified_no_tool_calls(self):
        cls, evidence = cr.classify_turn_end(
            summary=base_summary(),
            tools_by_recency=[],
            tool_states={},
            message_text="I looked around but did nothing.",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id=None,
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "ended_unverified")
        self.assertEqual(evidence["reason"], "no tool call at all")

    def test_ended_unverified_no_execution_after_edit(self):
        tool_states = {
            "edit1": cr.ToolCallState("edit1", 0, kind="edit", title="shell_write"),
        }
        tool_states["edit1"].status = "completed"
        cls, evidence = cr.classify_turn_end(
            summary=base_summary(),
            tools_by_recency=["edit1"],
            tool_states=tool_states,
            message_text="Changed the file. Done.",
            thought_text="",
            unawaited_async_operations=[],
            spilled_last_two=[],
            last_edit_id="edit1",
            successful_execution_after_edit=False,
        )
        self.assertEqual(cls, "ended_unverified")
        self.assertIn("no successful command execution", evidence["reason"])


class TestAnalyzeRunHelpers(unittest.TestCase):
    def test_unrecognized_session_update_is_tallied_not_dropped(self):
        events = [
            session_update({"sessionUpdate": "a_future_update_kind", "content": {}}),
            message_chunk("hello"),
        ]
        summary = base_summary()
        report = cr.analyze_run(events, summary)
        self.assertIn("sessionUpdate:a_future_update_kind", report["unrecognized"])
        self.assertEqual(report["unrecognized"]["sessionUpdate:a_future_update_kind"], 1)
        # It must not silently disappear from the overall session_update tally either.
        self.assertEqual(report["session_update_counts"]["a_future_update_kind"], 1)

    def test_asks_orphaned_when_no_matching_permission_request(self):
        events = [
            tool_call("t1", kind="edit", title="shell_write"),
            tool_call_update(
                "t1",
                status="failed",
                text=(
                    "gate for lfm2d-advisory is waiting on its reviewer: "
                    "ask deadbeef-0000-0000-0000-000000000001 (pending)"
                ),
            ),
        ]
        report = cr.analyze_run(events, base_summary(permissions_requested=0))
        self.assertEqual(report["asks_orphaned"], 1)
        self.assertIn("deadbeef-0000-0000-0000-000000000001", report["asks_orphaned_ids"])

    def test_asks_not_orphaned_when_permission_request_matches(self):
        events = [
            request_permission("deadbeef-0000-0000-0000-000000000001"),
            tool_call("t1", kind="edit", title="shell_write"),
            tool_call_update(
                "t1",
                status="failed",
                text=(
                    "gate for shell_write is waiting on its reviewer: nothing was run. "
                    "(ask deadbeef-0000-0000-0000-000000000001, pending)"
                ),
            ),
        ]
        report = cr.analyze_run(events, base_summary(permissions_requested=1))
        self.assertEqual(report["asks_orphaned"], 0)

    def test_gate_wait_without_ask_id_is_tallied_separately(self):
        events = [
            tool_call("t1", kind="edit", title="shell_write"),
            tool_call_update("t1", status="completed", text="nothing was run."),
        ]
        report = cr.analyze_run(events, base_summary())
        self.assertEqual(report["gate_waits_without_ask_id"], 1)
        self.assertEqual(report["asks_orphaned"], 0)

    def test_final_message_uses_text_after_last_tool_call(self):
        events = [
            message_chunk("stale message before the tool ran"),
            tool_call("t1", kind="execute", title="shell"),
            tool_call_update("t1", status="completed", text="ok"),
            message_chunk("final report text"),
        ]
        report = cr.analyze_run(events, base_summary())
        self.assertEqual(report["final_message"], "final report text")
        self.assertEqual(report["final_message_source"], "after_last_tool_call")

    def test_malformed_jsonl_line_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            events_path = Path(tmp) / "acp-events.jsonl"
            events_path.write_text('{"event_type": "on_connect"}\nnot json\n')
            with self.assertRaises(SystemExit):
                cr.load_jsonl(events_path)

    def test_non_object_jsonl_line_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            events_path = Path(tmp) / "acp-events.jsonl"
            events_path.write_text("[1, 2, 3]\n")
            with self.assertRaises(SystemExit):
                cr.load_jsonl(events_path)


class TestKernelLogParsing(unittest.TestCase):
    LOG_TEMPLATE = (
        '2026-09-18T14:28:10.500818Z  INFO llm.turn{{context.id={ctx}}}: '
        'kaijutsu_kernel::runtime::llm_stream: LLM stream completed: '
        'stop_reason=Some("{stop}"), tokens_in=Some({tin}), tokens_out=Some({tout})\n'
    )

    def test_scopes_to_session_and_sums_tokens(self):
        with tempfile.TemporaryDirectory() as tmp:
            log_path = Path(tmp) / "kernel.log"
            log_path.write_text(
                self.LOG_TEMPLATE.format(
                    ctx="01a0b4ea-e556-7b71-80df-d2d5d59e40d5", stop="tool_calls", tin=100, tout=10
                )
                + self.LOG_TEMPLATE.format(
                    ctx="01a0b4ed-374f-70a1-b442-1afe1bf2eff7", stop="tool_calls", tin=999, tout=99
                )
            )
            result = cr.parse_kernel_log(log_path, "01a0b4eae5567b7180dfd2d5d59e40d5")
            self.assertEqual(result["tokens_in_total"], 100)
            self.assertEqual(result["tokens_out_total"], 10)
            self.assertEqual(result["kernel_log_lines_matched"], 2)
            self.assertEqual(result["kernel_log_lines_parsed"], 1)

    def test_unscoped_sums_whole_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            log_path = Path(tmp) / "kernel.log"
            log_path.write_text(
                self.LOG_TEMPLATE.format(ctx="a" * 8 + "-0000-0000-0000-000000000000", stop="end_turn", tin=1, tout=1)
                + self.LOG_TEMPLATE.format(ctx="b" * 8 + "-0000-0000-0000-000000000000", stop="end_turn", tin=2, tout=2)
            )
            result = cr.parse_kernel_log(log_path, None)
            self.assertEqual(result["tokens_in_total"], 3)
            self.assertFalse(result["kernel_log_scoped_to_session"])

    def test_malformed_stream_line_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            log_path = Path(tmp) / "kernel.log"
            log_path.write_text("garbage LLM stream completed: nonsense\n")
            with self.assertRaises(SystemExit):
                cr.parse_kernel_log(log_path, None)


class TestRealData(unittest.TestCase):
    def test_ds_run_2_classifies_without_crashing(self):
        if not REAL_RUN_DIR.is_dir():
            self.skipTest(f"real fixture not present: {REAL_RUN_DIR}")
        events = cr.load_jsonl(REAL_RUN_DIR / "acp-events.jsonl")
        summary = cr.load_summary(REAL_RUN_DIR / "acp-summary.json")
        report = cr.analyze_run(events, summary)
        self.assertEqual(report["stop_reason"], "end_turn")
        self.assertIn(
            report["turn_end_class"],
            {
                "provider_failure",
                "setup_failure",
                "token_ceiling",
                "iteration_cap",
                "cancelled",
                "yielded_on_ask",
                "yielded_on_async",
                "output_starved",
                "completed_verified",
                "ended_unverified",
                "unclassified_stop_reason",
            },
        )
        # Pinned to the observed behavior on this static fixture: a real
        # gate-wait tool result with the ask-id-less phrasing, correctly
        # kept out of asks_orphaned, and a verified fix.
        self.assertEqual(report["gate_waits_without_ask_id"], 1)
        self.assertEqual(report["asks_orphaned"], 0)
        self.assertEqual(report["turn_end_class"], "completed_verified")

    def test_cli_runs_end_to_end_on_real_fixture(self):
        if not REAL_RUN_DIR.is_dir():
            self.skipTest(f"real fixture not present: {REAL_RUN_DIR}")
        script = Path(__file__).resolve().parent / "classify_run.py"
        proc = subprocess.run(
            [sys.executable, str(script), str(REAL_RUN_DIR), "--format", "json"],
            capture_output=True,
            text=True,
            check=True,
        )
        payload = json.loads(proc.stdout)
        self.assertIn("turn_end_class", payload)

    def test_cli_with_kernel_log(self):
        if not REAL_RUN_DIR.is_dir() or not REAL_KERNEL_LOG.is_file():
            self.skipTest("real fixture or kernel log not present")
        script = Path(__file__).resolve().parent / "classify_run.py"
        proc = subprocess.run(
            [
                sys.executable,
                str(script),
                str(REAL_RUN_DIR),
                "--kernel-log",
                str(REAL_KERNEL_LOG),
                "--format",
                "json",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        payload = json.loads(proc.stdout)
        self.assertTrue(payload["kernel_log_scoped_to_session"])
        self.assertGreater(payload["tokens_in_total"], 0)


if __name__ == "__main__":
    unittest.main()

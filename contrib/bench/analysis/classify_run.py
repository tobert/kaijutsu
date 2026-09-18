#!/usr/bin/env python3
"""Classify how one Harbor ACP run's turn ended.

This reads the three files Harbor's ACP runner writes into a run directory
(`acp-events.jsonl`, `acp-summary.json`, and optionally a launcher log this
tool does not use) and reports one `turn_end_class`: how the model's turn
ended. It does not say whether the task passed — Harbor's verifier decides
that from `reward.txt`/`ctrf.json`, not from this tool.

Usage:
    classify_run.py RUN_DIR [--format json|text] [--kernel-log PATH]

RUN_DIR must contain `acp-events.jsonl` and `acp-summary.json`; both are
required. `--format text` prints a short human summary instead of JSON
(the default).

`--kernel-log PATH` sums per-inference token counts from lines containing
"LLM stream completed" in a kaijutsu kernel log. That log can span several
runs (several `context.id` values); this tool scopes the sum to the run's
own session id (`acp-summary.json`'s `session.sessionId`, matched against
the log line's `context.id` with dashes normalized). A malformed "LLM
stream completed" line is a hard error — token totals are either exact or
refused, never silently partial.

This reads real text kaijutsu and Harbor emit today. Where a match count
in the output is zero, either nothing of that kind happened, or the
matched string has drifted from what's below.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path
from typing import Any

# ---------------------------------------------------------------------------
# Literal strings and shapes read from the data. These are the source of
# drift-detection: when a count for one of these goes to zero on data that
# should have it, the observed wording moved and this needs updating.
# ---------------------------------------------------------------------------

# harbor/src/harbor/agents/installed/acp.py, _convert_events_to_trajectory:
# the sessionUpdate values it gives distinct handling. Everything else is
# "known" only in the sense that ACP's schema names it; this tool tallies
# it under session_update_counts but extracts nothing from it.
KNOWN_SESSION_UPDATES = frozenset(
    {
        "agent_message_chunk",
        "agent_thought_chunk",
        "tool_call",
        "tool_call_update",
        "usage_update",
        "plan",
        "user_message_chunk",
        "available_commands_update",
        "current_mode_update",
        "config_option_update",
        "session_info_update",
    }
)

# kaijutsu's gate-wait tool result text. Two markers because the wording
# observed differs by tool: shell_write's gate message puts the ask id in
# a trailing "(ask <id>, pending)"; lfm2d-advisory's puts it right after
# "is waiting on its reviewer: ask <id> (pending)". Both sentences contain
# "nothing was run"; one shell_write variant (seen with tool status
# "completed", not "failed") contains only that phrase, without "is
# waiting on its reviewer" at all.
GATE_WAIT_MARKERS = (
    "is waiting on its reviewer",
    "nothing was run",
)

TIMEOUT_MARKER = "timed out after"
ITERATION_CAP_MARKER = "Paused after"

ASK_ID_RE = re.compile(
    r"ask ([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})"
)

# Literal substrings for a spilled/truncated shell result. Checked as plain
# substrings of the tool result text (which is itself often a JSON blob
# quoted inline, so these appear as literal characters, not nested escapes).
SPILL_TRUE_MARKERS = (
    '"did_spill":true',
    '"did_spill": true',
    '"truncated":true',
    '"truncated": true',
)

# kaijutsu's shell operation receipt shape, as returned by
# list_shell_operations / read_shell_operation:
#   [{"receipt": {"operation_id": "...", ...}, "status": "done", ...}, ...]
# or a single such object (not wrapped in a list) for read_shell_operation.
TERMINAL_OPERATION_STATUSES = frozenset({"done", "error"})

FINAL_MESSAGE_LIMIT = 2000


# ---------------------------------------------------------------------------
# Event loading. A line that is not valid JSON is real file corruption, not
# a schema drift, and is a hard error: fail loudly rather than skip it and
# report an undercount.
# ---------------------------------------------------------------------------


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    with path.open(encoding="utf-8") as fh:
        for lineno, raw_line in enumerate(fh, start=1):
            line = raw_line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError as exc:
                raise SystemExit(
                    f"{path}:{lineno}: not valid JSON ({exc}); refusing to guess "
                    "at corrupt event data"
                ) from exc
            if not isinstance(obj, dict):
                raise SystemExit(
                    f"{path}:{lineno}: expected a JSON object, got {type(obj).__name__}"
                )
            events.append(obj)
    return events


def load_summary(path: Path) -> dict[str, Any]:
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


# ---------------------------------------------------------------------------
# Content and receipt extraction
# ---------------------------------------------------------------------------


def content_text(content: Any) -> str:
    """Flatten an ACP content value (str, dict, or list of either) to text."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(content_text(item) for item in content)
    if isinstance(content, dict):
        text = content.get("text")
        if isinstance(text, str):
            return text
        nested = content.get("content")
        if nested is not None:
            return content_text(nested)
    return ""


def parse_receipt_entries(text: str) -> list[dict[str, Any]]:
    """Parse a kaijutsu shell receipt blob into its raw entries.

    A tool result's text is often a JSON array or object shaped like
    `{"receipt": {"operation_id": ..., ...}, "status": ..., "created_at":
    ..., "completed_at": ..., ...}`. Returns [] for text that is not JSON,
    or JSON that does not carry a "receipt" key at the entry level.
    """
    try:
        parsed = json.loads(text)
    except (json.JSONDecodeError, TypeError):
        return []
    entries = parsed if isinstance(parsed, list) else [parsed]
    return [
        entry
        for entry in entries
        if isinstance(entry, dict) and isinstance(entry.get("receipt"), dict)
    ]


def extract_operation_receipts(text: str) -> list[tuple[str, str]]:
    """Pull (operation_id, status) pairs out of a kaijutsu shell receipt blob."""
    out: list[tuple[str, str]] = []
    for entry in parse_receipt_entries(text):
        status = entry.get("status")
        op_id = entry["receipt"].get("operation_id")
        if isinstance(status, str) and isinstance(op_id, str) and op_id:
            out.append((op_id, status))
    return out


def extract_receipt_epoch_ms(text: str) -> list[int]:
    """Pull created_at/completed_at epoch-millisecond values out of receipts."""
    out: list[int] = []
    for entry in parse_receipt_entries(text):
        for key in ("created_at", "completed_at"):
            value = entry.get(key)
            if isinstance(value, int):
                out.append(value)
    return out


def looks_like_receipt_shape(text: str) -> bool:
    return '"receipt"' in text and '"operation_id"' in text


def find_epoch_ms_timestamps(obj: Any, keys: frozenset[str]) -> list[int]:
    """Recursively collect integer values under any of `keys`."""
    found: list[int] = []
    if isinstance(obj, dict):
        for key, value in obj.items():
            if key in keys and isinstance(value, int):
                found.append(value)
            found.extend(find_epoch_ms_timestamps(value, keys))
    elif isinstance(obj, list):
        for item in obj:
            found.extend(find_epoch_ms_timestamps(item, keys))
    return found


# ---------------------------------------------------------------------------
# Tool call tracking
# ---------------------------------------------------------------------------


class ToolCallState:
    """One tool call's accumulated state across its tool_call/tool_call_update events.

    `kind`/`title` are captured only from the event that first creates this
    state (mirroring Harbor's own `_convert_events_to_trajectory`, which
    resolves a tool's name once, at creation, from `_resolve_tool_name`).
    A later update that omits `kind`/`title` does not overwrite them.
    """

    __slots__ = ("tool_call_id", "kind", "title", "status", "first_index", "last_index", "texts")

    def __init__(self, tool_call_id: str, index: int, kind: Any, title: Any) -> None:
        self.tool_call_id = tool_call_id
        self.kind = kind if isinstance(kind, str) else None
        self.title = title if isinstance(title, str) else None
        self.status: str | None = None
        self.first_index = index
        self.last_index = index
        self.texts: list[str] = []

    def text(self) -> str:
        return "".join(self.texts)

    def resolved_name(self) -> str:
        # Mirrors harbor.agents.installed.acp._resolve_tool_name.
        if self.kind and self.kind != "other":
            return self.kind
        if self.title and self.title.strip():
            return self.title.strip().splitlines()[0]
        return "tool"


# ---------------------------------------------------------------------------
# Main analysis
# ---------------------------------------------------------------------------


def analyze_run(events: list[dict[str, Any]], summary: dict[str, Any] | None) -> dict[str, Any]:
    tool_states: dict[str, ToolCallState] = {}
    tool_order: list[str] = []
    permission_events: list[dict[str, Any]] = []
    message_chunks: list[tuple[int, str]] = []
    thought_text_parts: list[str] = []
    unrecognized: Counter[str] = Counter()
    session_update_counts: Counter[str] = Counter()
    gate_wait_marker_hits: Counter[str] = Counter()
    spill_marker_hits: Counter[str] = Counter()
    timeout_hits = 0
    receipt_shape_unrecognized = 0
    operation_status_counts: Counter[str] = Counter()
    operation_last_status: dict[str, tuple[str, int]] = {}
    op_wait_referenced: set[str] = set()

    for index, event in enumerate(events):
        event_type = event.get("event_type")

        if event_type == "on_connect":
            continue

        if event_type == "request_permission":
            payload = event.get("payload")
            if not isinstance(payload, dict):
                unrecognized["request_permission:malformed_payload"] += 1
                continue
            tool_call = payload.get("tool_call")
            ask_id = tool_call.get("toolCallId") if isinstance(tool_call, dict) else None
            permission_events.append(
                {"index": index, "ask_id": ask_id if isinstance(ask_id, str) else None}
            )
            continue

        if event_type != "session_update":
            unrecognized[f"event_type:{event_type!r}"] += 1
            continue

        payload = event.get("payload")
        if not isinstance(payload, dict):
            unrecognized["session_update:malformed_payload"] += 1
            continue
        update = payload.get("update")
        if not isinstance(update, dict):
            unrecognized["session_update:malformed_update"] += 1
            continue
        session_update = update.get("sessionUpdate")
        if not isinstance(session_update, str):
            unrecognized["session_update:missing_sessionUpdate"] += 1
            continue

        session_update_counts[session_update] += 1
        if session_update not in KNOWN_SESSION_UPDATES:
            unrecognized[f"sessionUpdate:{session_update}"] += 1
            continue

        if session_update == "agent_message_chunk":
            text = content_text(update.get("content"))
            if text:
                message_chunks.append((index, text))
            continue

        if session_update == "agent_thought_chunk":
            text = content_text(update.get("content"))
            if text:
                thought_text_parts.append(text)
            continue

        if session_update in ("tool_call", "tool_call_update"):
            tool_call_id = update.get("toolCallId")
            if not isinstance(tool_call_id, str) or not tool_call_id:
                unrecognized[f"{session_update}:missing_toolCallId"] += 1
                continue

            state = tool_states.get(tool_call_id)
            if state is None:
                state = ToolCallState(tool_call_id, index, update.get("kind"), update.get("title"))
                tool_states[tool_call_id] = state
                tool_order.append(tool_call_id)
            state.last_index = index
            if isinstance(update.get("status"), str):
                state.status = update["status"]

            text = content_text(update.get("content"))
            if text:
                state.texts.append(text)
                for marker in GATE_WAIT_MARKERS:
                    if marker in text:
                        gate_wait_marker_hits[marker] += 1
                for marker in SPILL_TRUE_MARKERS:
                    if marker in text:
                        spill_marker_hits[marker] += 1
                if TIMEOUT_MARKER in text:
                    timeout_hits += 1

                receipts = extract_operation_receipts(text)
                if receipts:
                    for op_id, status in receipts:
                        operation_status_counts[status] += 1
                        prev = operation_last_status.get(op_id)
                        if prev is None or index >= prev[1]:
                            operation_last_status[op_id] = (status, index)
                elif looks_like_receipt_shape(text):
                    receipt_shape_unrecognized += 1

                lowered = text.lower()
                if "wait" in lowered:
                    for op_id in operation_last_status:
                        if op_id in text:
                            op_wait_referenced.add(op_id)

            raw_input = update.get("rawInput")
            if isinstance(raw_input, dict):
                haystack = json.dumps(raw_input)
                if "wait" in haystack.lower():
                    for op_id in operation_last_status:
                        if op_id in haystack:
                            op_wait_referenced.add(op_id)
            continue

        # Other known session updates (usage_update, plan, user_message_chunk,
        # available_commands_update, current_mode_update, config_option_update,
        # session_info_update) carry nothing this tool extracts.

    message_text_all = "".join(text for _, text in message_chunks)
    thought_text_all = "".join(thought_text_parts)

    tools_by_recency = sorted(tool_order, key=lambda tid: tool_states[tid].last_index)

    # final_message: the agent message text emitted after the last tool
    # activity (i.e. the closing report), falling back to all message text
    # when no message followed the last tool call (e.g. the run ended mid
    # tool cycle with no trailing prose).
    last_tool_index = tool_states[tools_by_recency[-1]].last_index if tools_by_recency else -1
    trailing_message = "".join(text for idx, text in message_chunks if idx > last_tool_index)
    final_message_source = "after_last_tool_call"
    if not trailing_message:
        trailing_message = message_text_all
        final_message_source = "all_message_chunks"
    final_message = trailing_message[-FINAL_MESSAGE_LIMIT:]

    # --- gate-wait / asks_orphaned -----------------------------------
    requested_ask_ids = {p["ask_id"] for p in permission_events if p["ask_id"]}
    gate_wait_results = []
    for tool_call_id in tool_order:
        text = tool_states[tool_call_id].text()
        matched = [m for m in GATE_WAIT_MARKERS if m in text]
        if not matched:
            continue
        gate_wait_results.append(
            {
                "tool_call_id": tool_call_id,
                "matched_markers": matched,
                "ask_ids": ASK_ID_RE.findall(text),
            }
        )

    asks_orphaned = 0
    orphaned_ask_ids: list[str] = []
    gate_waits_without_ask_id = 0
    for gw in gate_wait_results:
        if not gw["ask_ids"]:
            gate_waits_without_ask_id += 1
            continue
        if not any(a in requested_ask_ids for a in gw["ask_ids"]):
            asks_orphaned += 1
            orphaned_ask_ids.extend(a for a in gw["ask_ids"] if a not in orphaned_ask_ids)

    # --- unawaited async operations, whole run -------------------------
    unawaited_async_operations = sorted(
        op_id
        for op_id, (status, _idx) in operation_last_status.items()
        if status not in TERMINAL_OPERATION_STATUSES and op_id not in op_wait_referenced
    )

    # --- spilled/truncated in the last 2 tool results -------------------
    last_two = tools_by_recency[-2:]
    spilled_last_two = []
    for tool_call_id in last_two:
        text = tool_states[tool_call_id].text()
        hits = [m for m in SPILL_TRUE_MARKERS if m in text]
        if hits:
            spilled_last_two.append({"tool_call_id": tool_call_id, "matched_markers": hits})

    # --- edits and executions, for completed_verified / ended_unverified ---
    edit_ids = sorted(
        (tid for tid in tool_order if tool_states[tid].kind == "edit"),
        key=lambda tid: tool_states[tid].last_index,
    )
    execute_ids = [tid for tid in tool_order if tool_states[tid].kind == "execute"]
    last_edit_id = edit_ids[-1] if edit_ids else None
    successful_execution_after_edit = False
    if last_edit_id is not None:
        last_edit_index = tool_states[last_edit_id].last_index
        for tool_call_id in execute_ids:
            state = tool_states[tool_call_id]
            if state.last_index <= last_edit_index:
                continue
            if state.status != "completed":
                continue
            if any(m in state.text() for m in GATE_WAIT_MARKERS):
                continue
            successful_execution_after_edit = True
            break

    turn_end_class, turn_end_evidence = classify_turn_end(
        summary=summary,
        tools_by_recency=tools_by_recency,
        tool_states=tool_states,
        message_text=message_text_all,
        thought_text=thought_text_all,
        unawaited_async_operations=unawaited_async_operations,
        spilled_last_two=spilled_last_two,
        last_edit_id=last_edit_id,
        successful_execution_after_edit=successful_execution_after_edit,
    )

    tool_calls_by_name: Counter[str] = Counter()
    tool_calls_by_status: Counter[str] = Counter()
    for tool_call_id in tool_order:
        state = tool_states[tool_call_id]
        tool_calls_by_name[state.resolved_name()] += 1
        tool_calls_by_status[state.status or "unknown"] += 1

    inference_count = session_update_counts.get("usage_update", None)
    # usage_update is emitted once per completed LLM turn in ACP; absent a
    # usage_update stream, this is not derivable from events alone.

    permission_requests_observed = len(permission_events)
    result: dict[str, Any] = {
        "turn_end_class": turn_end_class,
        "turn_end_evidence": turn_end_evidence,
        "gate_wait_markers_matched": dict(gate_wait_marker_hits),
        "stop_reason": (
            (summary.get("prompt_response") or {}).get("stopReason")
            if isinstance(summary, dict) and isinstance(summary.get("prompt_response"), dict)
            else None
        ),
        "tool_calls_total": len(tool_order),
        "tool_calls_by_name": dict(sorted(tool_calls_by_name.items())),
        "tool_calls_by_status": dict(sorted(tool_calls_by_status.items())),
        "inferences": inference_count,
        "permission_requests": permission_requests_observed,
        "asks_orphaned": asks_orphaned,
        "asks_orphaned_ids": orphaned_ask_ids,
        "gate_waits_without_ask_id": gate_waits_without_ask_id,
        "unawaited_async_operations": unawaited_async_operations,
        "operation_status_counts": dict(sorted(operation_status_counts.items())),
        "receipt_shape_unrecognized": receipt_shape_unrecognized,
        "spilled_or_truncated_matches": dict(spill_marker_hits),
        "tool_timeouts": timeout_hits,
        "session_update_counts": dict(sorted(session_update_counts.items())),
        "unrecognized": dict(sorted(unrecognized.items())),
        "final_message": final_message,
        "final_message_source": final_message_source,
        "final_message_truncated": len(trailing_message) > FINAL_MESSAGE_LIMIT,
    }

    if isinstance(summary, dict):
        summary_reported = summary.get("permissions_requested")
        if isinstance(summary_reported, int) and summary_reported != permission_requests_observed:
            result["permission_requests_summary_mismatch"] = {
                "summary_reported": summary_reported,
                "events_observed": permission_requests_observed,
            }

    return result


def classify_turn_end(
    *,
    summary: dict[str, Any] | None,
    tools_by_recency: list[str],
    tool_states: dict[str, ToolCallState],
    message_text: str,
    thought_text: str,
    unawaited_async_operations: list[str],
    spilled_last_two: list[dict[str, Any]],
    last_edit_id: str | None,
    successful_execution_after_edit: bool,
) -> tuple[str, dict[str, Any]]:
    """First-match-wins classification of how the turn ended.

    Order: provider_failure/setup_failure, token_ceiling, iteration_cap,
    cancelled, yielded_on_ask, yielded_on_async, output_starved,
    completed_verified, ended_unverified. A stop reason this scheme does
    not name (ACP also defines "refusal") reports as
    unclassified_stop_reason with the raw value, rather than being folded
    into a class it may not fit.
    """
    if summary is None:
        return "unclassified_stop_reason", {"reason": "no acp-summary.json data available"}

    if "error" in summary:
        session = summary.get("session")
        session_established = isinstance(session, dict) and bool(session.get("sessionId"))
        cls = "provider_failure" if session_established else "setup_failure"
        return cls, {"error": summary["error"], "session_established": session_established}

    prompt_response = summary.get("prompt_response")
    stop_reason = (
        prompt_response.get("stopReason") if isinstance(prompt_response, dict) else None
    )

    pause_text_hit = ITERATION_CAP_MARKER in message_text or ITERATION_CAP_MARKER in thought_text

    if stop_reason == "max_tokens":
        return "token_ceiling", {"stop_reason": stop_reason}

    if stop_reason == "max_turn_requests" or pause_text_hit:
        evidence: dict[str, Any] = {"stop_reason": stop_reason}
        if pause_text_hit:
            evidence["halt_text"] = ITERATION_CAP_MARKER
        return "iteration_cap", evidence

    if stop_reason == "cancelled":
        return "cancelled", {"stop_reason": stop_reason}

    if stop_reason != "end_turn":
        return "unclassified_stop_reason", {"stop_reason": stop_reason}

    if not tools_by_recency:
        return "ended_unverified", {"stop_reason": stop_reason, "reason": "no tool call at all"}

    last_id = tools_by_recency[-1]
    last_text = tool_states[last_id].text()
    gate_hit = [m for m in GATE_WAIT_MARKERS if m in last_text]
    if gate_hit:
        return (
            "yielded_on_ask",
            {"stop_reason": stop_reason, "last_tool_call_id": last_id, "matched_markers": gate_hit},
        )

    last_result_receipts = extract_operation_receipts(last_text)
    last_result_unawaited = [
        op_id
        for op_id, status in last_result_receipts
        if status not in TERMINAL_OPERATION_STATUSES and op_id in unawaited_async_operations
    ]
    if last_result_unawaited:
        return (
            "yielded_on_async",
            {
                "stop_reason": stop_reason,
                "last_tool_call_id": last_id,
                "unawaited_operations": last_result_unawaited,
            },
        )

    if spilled_last_two:
        return (
            "output_starved",
            {"stop_reason": stop_reason, "spilled_tool_results": spilled_last_two},
        )

    if last_edit_id is not None and successful_execution_after_edit:
        return "completed_verified", {"stop_reason": stop_reason, "last_edit_tool_call_id": last_edit_id}

    reason = (
        "no successful command execution after the last edit"
        if last_edit_id is not None
        else "no edit tool call in this run"
    )
    return "ended_unverified", {"stop_reason": stop_reason, "reason": reason}


# ---------------------------------------------------------------------------
# Kernel log token accounting
# ---------------------------------------------------------------------------

_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
_CONTEXT_ID_RE = re.compile(r"context\.id=([0-9a-fA-F-]{36})")
_STREAM_LINE_RE = re.compile(
    r"LLM stream completed: stop_reason=(?P<stop_reason>\S+?), "
    r"tokens_in=(?P<tokens_in>Some\(\d+\)|None), "
    r"tokens_out=(?P<tokens_out>Some\(\d+\)|None)"
)
_SOME_INT_RE = re.compile(r"Some\((\d+)\)")


def normalize_session_id(value: str) -> str:
    return value.replace("-", "").lower()


def parse_kernel_log(path: Path, session_id: str | None) -> dict[str, Any]:
    """Sum per-inference tokens from "LLM stream completed" lines.

    Scoped to `session_id` (matched against each line's `context.id`,
    dashes normalized) when given; otherwise sums the whole file and warns
    that the total may span more than one run.
    """
    try:
        raw = path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        raise SystemExit(f"cannot read {path}: {exc}") from exc

    target = normalize_session_id(session_id) if session_id else None
    lines_matched = 0
    lines_in_scope = 0
    tokens_in_total = 0
    tokens_out_total = 0
    stop_reasons: Counter[str] = Counter()

    for lineno, raw_line in enumerate(raw.splitlines(), start=1):
        if "LLM stream completed" not in raw_line:
            continue
        lines_matched += 1
        clean = _ANSI_RE.sub("", raw_line)
        match = _STREAM_LINE_RE.search(clean)
        if not match:
            raise SystemExit(
                f"{path}:{lineno}: line contains 'LLM stream completed' but does not "
                "match the expected 'stop_reason=..., tokens_in=..., tokens_out=...' "
                "shape; refusing to report a partial token total"
            )
        context_match = _CONTEXT_ID_RE.search(clean)
        context_id = context_match.group(1) if context_match else None
        if target is not None:
            if context_id is None or normalize_session_id(context_id) != target:
                continue
        lines_in_scope += 1
        stop_reasons[match.group("stop_reason")] += 1
        tin = _SOME_INT_RE.search(match.group("tokens_in"))
        tout = _SOME_INT_RE.search(match.group("tokens_out"))
        tokens_in_total += int(tin.group(1)) if tin else 0
        tokens_out_total += int(tout.group(1)) if tout else 0

    return {
        "kernel_log_lines_matched": lines_matched,
        "kernel_log_lines_parsed": lines_in_scope,
        "kernel_log_scoped_to_session": target is not None,
        "tokens_in_total": tokens_in_total,
        "tokens_out_total": tokens_out_total,
        "llm_stream_stop_reasons": dict(sorted(stop_reasons.items())),
    }


# ---------------------------------------------------------------------------
# Duration
# ---------------------------------------------------------------------------

_TIMESTAMP_KEYS = frozenset({"timestampMs", "timestamp_ms"})


def estimate_duration(events: list[dict[str, Any]]) -> dict[str, Any]:
    """Wall-clock duration from first to last event, if timestamps exist.

    ACP session_update events carry no per-event timestamp in this data.
    First choice is a literal `timestampMs`/`timestamp_ms` field, if ACP
    ever supplies one; when absent, this falls back to the epoch-
    millisecond `created_at`/`completed_at` fields embedded inside
    kaijutsu shell operation receipts (themselves quoted as JSON text
    inside tool result content, not native JSON in the event). That
    fallback is a lower bound covering only the run's captured shell
    activity, not the full event stream, and is reported as such.
    """
    event_ms = find_epoch_ms_timestamps(events, _TIMESTAMP_KEYS)
    if event_ms:
        return {
            "duration_seconds": (max(event_ms) - min(event_ms)) / 1000.0,
            "duration_source": "event_timestamps",
        }

    receipt_ms: list[int] = []
    for event in events:
        if event.get("event_type") != "session_update":
            continue
        payload = event.get("payload")
        update = payload.get("update") if isinstance(payload, dict) else None
        if not isinstance(update, dict):
            continue
        text = content_text(update.get("content"))
        if text:
            receipt_ms.extend(extract_receipt_epoch_ms(text))
    if receipt_ms:
        return {
            "duration_seconds": (max(receipt_ms) - min(receipt_ms)) / 1000.0,
            "duration_source": "operation_receipt_timestamps",
        }

    return {"duration_seconds": None, "duration_source": "unavailable"}


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def format_text(report: dict[str, Any]) -> str:
    lines = [
        f"turn_end_class: {report['turn_end_class']}",
        f"  evidence: {json.dumps(report['turn_end_evidence'], sort_keys=True)}",
        f"stop_reason: {report['stop_reason']}",
        f"tool_calls_total: {report['tool_calls_total']}",
        f"tool_calls_by_name: {report['tool_calls_by_name']}",
        f"tool_calls_by_status: {report['tool_calls_by_status']}",
        f"inferences (usage_update count): {report['inferences']}",
        f"permission_requests: {report['permission_requests']}",
        f"asks_orphaned: {report['asks_orphaned']} {report['asks_orphaned_ids']}",
        f"gate_waits_without_ask_id: {report['gate_waits_without_ask_id']}",
        f"unawaited_async_operations: {report['unawaited_async_operations']}",
        f"tool_timeouts: {report['tool_timeouts']}",
        f"spilled_or_truncated_matches: {report['spilled_or_truncated_matches']}",
    ]
    if "duration_seconds" in report:
        lines.append(
            f"duration_seconds: {report['duration_seconds']} (source: {report['duration_source']})"
        )
    if "tokens_in_total" in report:
        lines.append(
            f"tokens: in={report['tokens_in_total']} out={report['tokens_out_total']} "
            f"(kernel log lines parsed: {report['kernel_log_lines_parsed']}"
            f"/{report['kernel_log_lines_matched']} matched, "
            f"scoped={report['kernel_log_scoped_to_session']})"
        )
    if report["unrecognized"]:
        lines.append(f"unrecognized: {report['unrecognized']}")
    lines.append("")
    lines.append("final_message" + (" (truncated)" if report["final_message_truncated"] else "") + ":")
    lines.append(report["final_message"])
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Classify how one Harbor ACP run's turn ended (not whether the task "
            "passed — that is Harbor's verifier, in reward.txt/ctrf.json)."
        ),
    )
    parser.add_argument(
        "run_dir",
        type=Path,
        help="Run directory containing acp-events.jsonl and acp-summary.json (both required).",
    )
    parser.add_argument(
        "--format",
        choices=("json", "text"),
        default="json",
        help="Output format. json (default) prints the full report; text prints a short summary.",
    )
    parser.add_argument(
        "--kernel-log",
        type=Path,
        default=None,
        help=(
            "Path to a kaijutsu kernel log to sum per-inference token counts from. "
            "Optional; omitted, no token totals are reported. A malformed "
            "'LLM stream completed' line is a hard error, not a skipped line."
        ),
    )
    args = parser.parse_args(argv)

    events_path = args.run_dir / "acp-events.jsonl"
    summary_path = args.run_dir / "acp-summary.json"
    if not events_path.is_file():
        raise SystemExit(f"{events_path} not found; classify_run needs the run's captured events")
    if not summary_path.is_file():
        raise SystemExit(f"{summary_path} not found; classify_run needs the run's stop reason")

    events = load_jsonl(events_path)
    summary = load_summary(summary_path)

    report = analyze_run(events, summary)
    report.update(estimate_duration(events))

    if args.kernel_log is not None:
        session = summary.get("session")
        session_id = session.get("sessionId") if isinstance(session, dict) else None
        report.update(parse_kernel_log(args.kernel_log, session_id))

    if args.format == "json":
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(format_text(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Read one Crabber artifact directory and print a JSON accounting report.

Usage: python3 scripts/analyze_run.py /path/to/.git/crabber-implement/RUN_ID
Counts describe recorded work, not a controlled performance comparison.
"""

import argparse
import json
import sys
from collections import defaultdict
from pathlib import Path


FAILURES = {"failed", "error", "cancelled", "canceled"}
METRICS = (
    "session_count", "response_count", "reported_tool_calls", "tool_result_count",
    "input_tokens", "output_tokens", "tool_error_count", "exit_failure_count",
    "timeout_count", "failed_tool_result_count", "session_error_count",
    "response_error_count", "tool_duration_ms", "delegate_duration_ms",
    "non_delegate_duration_ms", "timeout_duration_ms", "missing_usage_count", "missing_duration_count",
)


def read_json(path):
    with path.open() as stream:
        value = json.load(stream)
    if not isinstance(value, dict):
        raise ValueError(f"{path}: expected a JSON object")
    return value


def read_rows(path):
    rows = []
    with path.open() as stream:
        for number, line in enumerate(stream, 1):
            if not line.strip():
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError as error:
                raise ValueError(f"{path}:{number}: {error.msg}") from error
            if not isinstance(value, dict):
                raise ValueError(f"{path}:{number}: expected a JSON object")
            rows.append(value)
    return rows


def number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def summarize_session(directory, name):
    session_path = directory / f"{name}.session.json"
    response_path = directory / f"{name}.responses.jsonl"
    tool_path = directory / f"{name}.tools.txt"
    session = read_json(session_path) if session_path.exists() else {}
    runs = session.get("runs", [])
    retained = [response for run in runs for response in run.get("responses", [])]
    responses = read_rows(response_path) if response_path.exists() else retained
    tools = read_rows(tool_path) if tool_path.exists() else []
    declared_count = sum(run.get("response_count", len(run.get("responses", []))) for run in runs)
    warnings = []
    if not session_path.exists():
        warnings.append("Session metadata is missing; session failures are unknown.")
    if not response_path.exists():
        warnings.append("Response JSONL is missing; metrics use retained session responses only.")
    if declared_count > len(responses):
        warnings.append("Response metrics are partial; fewer records than declared responses.")
    if not tool_path.exists():
        warnings.append("Tool results are missing; tool failures and durations are unknown.")
    metrics = dict.fromkeys(METRICS, 0)
    metrics["session_count"] = 1
    metrics["session_error_count"] = int(bool(session.get("error")) or session.get("status") in FAILURES)
    for response in responses:
        metrics["response_count"] += 1
        metrics["reported_tool_calls"] += response.get("tool_call_count", 0)
        usage = response.get("usage") or {}
        for key in ("input_tokens", "output_tokens"):
            if number(usage.get(key)):
                metrics[key] += usage[key]
        metrics["missing_usage_count"] += int(not all(number(usage.get(key)) for key in ("input_tokens", "output_tokens")))
        metrics["response_error_count"] += int(response.get("stream_completed") is False or response.get("finish_reason") in FAILURES or bool(response.get("error")))
    tool_types = defaultdict(lambda: {"count": 0, "duration_ms": 0, "failed_count": 0})
    tool_details = []
    for tool in tools:
        if "observation" in tool and tool["observation"].get("event_kind") != "tool_call_settled":
            continue
        payloads = [tool]
        for content in tool.get("content", []):
            if content.get("type") == "text":
                try:
                    value = json.loads(content.get("text", ""))
                except (ValueError, TypeError):
                    continue
                if isinstance(value, dict):
                    payloads.append(value)
        exit_codes = [payload["exit_code"] for payload in payloads if number(payload.get("exit_code"))]
        exit_failure = any(code != 0 for code in exit_codes)
        timed_out = any(payload.get("timed_out") is True for payload in payloads)
        tool_error = any(payload.get("is_error") is True or payload.get("status") in FAILURES or payload.get("outcome") in FAILURES or bool(payload.get("error")) for payload in payloads)
        failed = tool_error or exit_failure or timed_out
        duration = tool.get("duration_ms")
        tool_name = tool.get("name") or tool.get("tool") or "unknown"
        metrics["tool_result_count"] += 1
        metrics["tool_error_count"] += int(tool_error)
        metrics["exit_failure_count"] += int(exit_failure)
        metrics["timeout_count"] += int(timed_out)
        metrics["failed_tool_result_count"] += int(failed)
        metrics["missing_duration_count"] += int(not number(duration))
        if number(duration):
            metrics["tool_duration_ms"] += duration
            metrics["timeout_duration_ms"] += duration if timed_out else 0
            metrics["delegate_duration_ms" if tool_name == "delegate_task" else "non_delegate_duration_ms"] += duration
        tool_types[tool_name]["count"] += 1
        tool_types[tool_name]["duration_ms"] += duration if number(duration) else 0
        tool_types[tool_name]["failed_count"] += int(failed)
        tool_details.append({"artifact": tool_path.name, "tool": tool_name, "call_id": tool.get("call_id"), "duration_ms": duration, "failed": failed, "exit_codes": exit_codes, "timed_out": timed_out})
    return {
        "group": "fix-review" if name.startswith("fix-review-") else "review" if name.startswith("review-") else "implement" if name == "implement" else "other",
        "provider": session.get("provider"),
        "status": session.get("status"),
        "error": session.get("error"),
        "response_source": response_path.name if response_path.exists() else session_path.name,
        "declared_response_count": declared_count if runs else None,
        "metrics": metrics,
        "tool_types": dict(sorted(tool_types.items())),
        "warnings": warnings,
    }, tool_details


def analyze(directory, top=10, filesystem_estimate=False):
    suffixes = (".session.json", ".responses.jsonl", ".tools.txt")
    names = sorted({path.name[:-len(suffix)] for suffix in suffixes for path in directory.glob(f"*{suffix}")})
    if not names:
        raise ValueError(f"{directory}: no session, response, or tool artifacts found")
    sessions = {}
    details = []
    groups = {}
    totals = dict.fromkeys(METRICS, 0)
    for name in names:
        summary, tools = summarize_session(directory, name)
        sessions[name] = summary
        details.extend(tools)
        aggregate = groups.setdefault(summary["group"], dict.fromkeys(METRICS, 0))
        for key, value in summary["metrics"].items():
            aggregate[key] += value
            totals[key] += value
    result = {
        "artifact_directory": str(directory.resolve()),
        "notes": [
            "Token totals sum recorded per-response usage, including repeated input context; they are not unique context size or billed cost.",
            "Durations sum outer tool-result duration_ms once; nested shell durations are not added again.",
            "Tool durations can overlap. delegate_task includes child work also reported by child sessions. No duration sum is run wall time.",
            "Zero counts mean no recorded occurrences. Check missing-artifact warnings and coverage metrics before comparison.",
        ],
        "totals": totals,
        "groups": groups,
        "sessions": sessions,
        "slowest_tools": sorted((tool for tool in details if number(tool["duration_ms"])), key=lambda tool: tool["duration_ms"], reverse=True)[:top],
        "failed_tools": [tool for tool in details if tool["failed"]],
    }
    if filesystem_estimate:
        times = [path.stat().st_mtime_ns for path in directory.iterdir() if path.is_file()]
        result["filesystem_modification_span_estimate"] = {
            "duration_ms": (max(times) - min(times)) / 1_000_000,
            "file_count": len(times),
            "note": "Filesystem estimate only. First-to-last current file modification is not exact run wall time; copying, rewriting, or recovery can change it.",
        }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--top", type=int, default=10, help="Number of slowest tool results to include")
    parser.add_argument("--filesystem-estimate", action="store_true", help="Include explicitly labelled file modification span estimate")
    args = parser.parse_args()
    if args.top < 0:
        parser.error("--top must be nonnegative")
    try:
        print(json.dumps(analyze(args.directory, args.top, args.filesystem_estimate), indent=2))
    except (OSError, ValueError, TypeError, KeyError) as error:
        print(f"analyze_run: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

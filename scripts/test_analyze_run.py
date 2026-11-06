#!/usr/bin/env python3
"""Exercise the report CLI with synthetic accounting fixtures."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("analyze_run.py")


class AnalyzeRunTest(unittest.TestCase):
    def fixture(self, directory, name, responses, tools, **session):
        (directory / f"{name}.session.json").write_text(json.dumps({"status": "completed", **session}))
        for suffix, rows in (("responses.jsonl", responses), ("tools.txt", tools)):
            (directory / f"{name}.{suffix}").write_text("".join(json.dumps(row) + "\n" for row in rows))

    def report(self, directory):
        process = subprocess.run([sys.executable, str(SCRIPT), str(directory)], capture_output=True, text=True, check=True)
        self.assertEqual(process.stderr, "")
        return json.loads(process.stdout)

    def test_short_long_and_error_fixtures_change_measured_work(self):
        response = {"usage": {"input_tokens": 100, "output_tokens": 5}, "tool_call_count": 1, "stream_completed": True}
        tool = {"name": "shell", "duration_ms": 10, "is_error": False, "content": [{"type": "text", "text": json.dumps({"exit_code": 0, "duration_ms": 9})}]}
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ("short", "long", "error"):
                (root / name).mkdir()
            self.fixture(root / "short", "implement", [response], [tool])
            self.fixture(root / "long", "implement", [response] * 4, [tool] * 4)
            self.fixture(root / "error", "fix-review-1", [response], [
                {**tool, "duration_ms": 120006, "is_error": True, "content": [{"type": "text", "text": json.dumps({"exit_code": 137, "duration_ms": 120004, "timed_out": True, "outcome": "failed"})}]},
                {**tool, "content": [{"type": "text", "text": json.dumps({"exit_code": 2})}]},
            ], status="failed", error="synthetic error")
            short, long, error = [self.report(root / name) for name in ("short", "long", "error")]
            for key in ("response_count", "reported_tool_calls", "tool_result_count", "input_tokens", "output_tokens", "tool_duration_ms"):
                self.assertEqual(long["totals"][key], short["totals"][key] * 4)
            self.assertEqual(short["totals"]["tool_duration_ms"], 10)
            self.assertEqual(error["totals"]["session_error_count"], 1)
            self.assertEqual(error["totals"]["tool_error_count"], 1)
            self.assertEqual(error["totals"]["exit_failure_count"], 2)
            self.assertEqual(error["totals"]["failed_tool_result_count"], 2)
            self.assertEqual(error["totals"]["timeout_count"], 1)
            self.assertEqual(error["totals"]["timeout_duration_ms"], 120006)
            self.assertEqual(error["groups"]["fix-review"], error["totals"])

    def test_pending_observations_do_not_change_legacy_result_metrics(self):
        result = {"name": "shell", "call_id": "same", "duration_ms": 12, "is_error": False,
                  "content": [{"type": "text", "text": json.dumps({"exit_code": 2})}]}
        pending = {"name": "shell", "call_id": "same", "status": "pending", "arguments": {"command": "exit 2"},
                   "observation": {"session_id": "s", "run_id": "r", "event_kind": "tool_call_pending"}}
        settled = {**result, "observation": {"session_id": "s", "run_id": "r", "event_kind": "tool_call_settled"}}
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name, rows in (("legacy", [result]), ("observed", [pending, settled])):
                (root / name).mkdir()
                self.fixture(root / name, "implement", [], rows)
            legacy, observed = [self.report(root / name) for name in ("legacy", "observed")]
            legacy.pop("artifact_directory")
            observed.pop("artifact_directory")
            self.assertEqual(legacy, observed)
            self.assertEqual(observed["totals"]["tool_result_count"], 1)
            self.assertEqual(observed["totals"]["exit_failure_count"], 1)

    def test_full_jsonl_wins_over_retained_tail_and_delegation_stays_visible(self):
        response = {"usage": {"input_tokens": 100, "output_tokens": 5}}
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.fixture(directory, "review-1-dispatch-1", [response] * 3, [{"name": "delegate_task", "duration_ms": 200}], runs=[{"response_count": 3, "responses_truncated": True, "responses": [response]}])
            report = self.report(directory)
            self.assertEqual(report["totals"]["input_tokens"], 300)
            self.assertEqual(report["totals"]["response_count"], 3)
            self.assertEqual(report["totals"]["delegate_duration_ms"], 200)
            self.assertEqual(report["totals"]["non_delegate_duration_ms"], 0)
            (directory / "review-1-dispatch-1.responses.jsonl").unlink()
            partial = self.report(directory)
            self.assertEqual(partial["totals"]["response_count"], 1)
            self.assertEqual(len(partial["sessions"]["review-1-dispatch-1"]["warnings"]), 2)


if __name__ == "__main__":
    unittest.main()

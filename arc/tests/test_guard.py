import json
import unittest

from guard import ToolEventTrace, TurnMonitor


def started(name, args, call_id="c1"):
    return "tool/started", {"tool_call_id": call_id, "tool_name": name, "arguments": args}


def completed(call_id, success, preview=""):
    return "tool/completed", {"tool_call_id": call_id, "success": success, "output_preview": preview}


class TurnMonitorTests(unittest.TestCase):
    def test_should_flag_completion_claim_without_verification_commands(self):
        m = TurnMonitor(protected_prefixes=[".arc/", "requirements/"])
        m.observe(*started("write_file", {"path": "backend/server.js"}))
        m.observe(*completed("c1", True))
        m.finish("Done. The feature is fully implemented and verified. ✅")
        self.assertIn("claimed completion without running", " ".join(m.corrections()))

    def test_should_not_flag_when_build_and_curl_ran(self):
        m = TurnMonitor(protected_prefixes=[])
        m.observe(*started("bash", {"cmd": "cd frontend && npm run build"}, "c1"))
        m.observe(*completed("c1", True))
        m.observe(*started("bash", {"cmd": "curl -s http://127.0.0.1:43101/api/count"}, "c2"))
        m.observe(*completed("c2", True))
        m.finish("Implemented and verified.")
        self.assertEqual(m.corrections(), [])

    def test_should_flag_three_identical_consecutive_errors(self):
        m = TurnMonitor(protected_prefixes=[])
        for i in range(3):
            m.observe(*started("bash", {"cmd": "node backend/server.js"}, f"c{i}"))
            m.observe(*completed(f"c{i}", False, "Error: listen EADDRINUSE :::43101"))
        m.finish("")
        self.assertTrue(any("same error 3 times" in c for c in m.corrections()))

    def test_should_flag_writes_into_protected_paths(self):
        m = TurnMonitor(protected_prefixes=[".arc/", "requirements/", "/abs/tests/"])
        m.observe(*started("edit_file", {"path": "/abs/tests/REQ-1.spec.ts"}, "c1"))
        m.observe(*completed("c1", True))
        m.observe(*started("bash", {"cmd": "echo x > requirements/requirements.yaml"}, "c2"))
        m.observe(*completed("c2", True))
        m.finish("ok")
        joined = " ".join(m.corrections())
        self.assertIn("/abs/tests/REQ-1.spec.ts", joined)
        self.assertIn("requirements/", joined)

    def test_should_allow_design_file_inside_protected_arc_dir(self):
        m = TurnMonitor(protected_prefixes=[".arc/"], allowed_prefixes=[".arc/design/"])
        m.observe(*started("write_file", {"path": ".arc/design/REQ-1.json"}, "c1"))
        m.observe(*completed("c1", True))
        m.observe(*started("write_file", {"path": ".arc/traceability/node_states.json"}, "c2"))
        m.observe(*completed("c2", True))
        m.finish("")
        self.assertEqual(m.protected_writes, [".arc/traceability/node_states.json"])

    def test_should_not_require_verification_for_design_turns(self):
        m = TurnMonitor(protected_prefixes=[], expect_verification=False)
        m.observe(*started("write_file", {"path": ".arc/design/REQ-1.json"}))
        m.observe(*completed("c1", True))
        m.finish("Design complete.")
        self.assertEqual(m.corrections(), [])

    def test_should_report_no_files_written_when_only_reading(self):
        m = TurnMonitor(protected_prefixes=[])
        m.observe(*started("read_file", {"path": "backend/server.js"}, "c1"))
        m.observe(*completed("c1", True))
        m.finish("Here is my plan...")
        self.assertFalse(m.wrote_files)


class ToolEventTraceTests(unittest.TestCase):
    def test_should_emit_live_safe_events_and_an_aggregate_summary(self):
        lines = []
        trace = ToolEventTrace("REQ-3-2-1 implement", lines.append)
        secret_command = "curl -H 'Authorization: Bearer secret-value' localhost"

        for index in range(3):
            call_id = f"call-secret-value-{index}"
            trace.observe(
                "tool/started",
                {
                    "turn_id": "turn-1",
                    "tool_call_id": call_id,
                    "tool_name": "shell",
                    "arguments": {"command": secret_command},
                },
            )
            trace.observe(
                "tool/completed",
                {
                    "turn_id": "turn-1",
                    "tool_call_id": call_id,
                    "tool_name": "shell",
                    "success": False,
                    "output_preview": "secret-value failed in /workspace/private",
                    "duration_ms": 25,
                },
            )
        trace.finish()

        self.assertEqual(len(lines), 7)
        self.assertTrue(all(line.startswith("[tool.") for line in lines))
        joined = "\n".join(lines)
        self.assertNotIn("secret-value", joined)
        self.assertNotIn("/workspace/private", joined)
        summary = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual(summary["event"], "summary")
        self.assertEqual(summary["node"], "REQ-3-2-1")
        self.assertEqual(summary["counts"], {"shell": 3})
        self.assertEqual(summary["failures"], {"shell": 3})
        self.assertEqual(summary["duration_ms_by_tool"], {"shell": 75})
        self.assertEqual(summary["max_consecutive_repeat"], 3)
        self.assertEqual(summary["repeated_calls"][0]["count"], 3)
        self.assertEqual(summary["repeated_failures"][0]["count"], 3)
        self.assertEqual(summary["slowest_calls"][0]["duration_ms"], 25)

    def test_should_report_unfinished_and_unmatched_tool_events(self):
        lines = []
        trace = ToolEventTrace("final check", lines.append)
        trace.observe(
            "tool/started",
            {
                "tool_call_id": "pending",
                "tool_name": "grep",
                "arguments": {"pattern": "needle"},
            },
        )
        trace.observe(
            "tool/completed",
            {
                "tool_call_id": "orphan",
                "tool_name": "read_file",
                "success": True,
            },
        )
        trace.finish()

        summary = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual(summary["pending"], 1)
        self.assertEqual(summary["unmatched_completions"], 1)

    def test_should_report_when_tool_events_are_not_observable(self):
        lines = []
        trace = ToolEventTrace(
            "REQ-1 implement",
            lines.append,
            observation="unavailable",
        )
        trace.finish()

        summary = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual(summary["started"], 0)
        self.assertEqual(summary["observation"], "unavailable")

        trace.mark_incomplete("stdio_chat_fallback")
        marker = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual(marker["event"], "observation")
        self.assertEqual(marker["reason"], "stdio_chat_fallback")


if __name__ == "__main__":
    unittest.main()

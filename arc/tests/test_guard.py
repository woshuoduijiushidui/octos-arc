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
        self.assertTrue(any("same recent error 3 times" in c for c in m.corrections()))

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

    def test_should_remind_on_each_repeat_then_interrupt_the_sixth_failure(self):
        m = TurnMonitor(protected_prefixes=[])
        actions = []
        for index in range(6):
            call_id = f"c{index}"
            m.observe(*started("shell", {"command": "pkill -f node"}, call_id))
            action = m.observe(*completed(
                call_id,
                False,
                "process 43101 did not exit\n\nExit code: -1",
            ))
            if action is not None:
                actions.append(action)
                m.record_action_result(action, True)

        self.assertEqual([a.kind for a in actions], ["steer", "steer", "steer", "interrupt"])
        self.assertEqual([a.repeat_count for a in actions], [3, 4, 5, 6])
        self.assertIn("43101", actions[0].message)
        self.assertIn("root cause is not known", actions[0].message)
        m.finish("")
        correction = " ".join(m.corrections())
        self.assertIn("was interrupted", correction)
        self.assertIn("43101", correction)

    def test_should_not_treat_different_commands_as_the_same_call(self):
        m = TurnMonitor(protected_prefixes=[])
        actions = []
        for index in range(6):
            call_id = f"c{index}"
            m.observe(*started("shell", {"command": f"probe-{index}"}, call_id))
            action = m.observe(*completed(call_id, False, "(no output)\n\nExit code: -1"))
            if action is not None:
                actions.append(action)

        self.assertTrue(actions)
        self.assertTrue(all(a.reason == "failure_family_loop" for a in actions))
        self.assertTrue(all(a.repeat_count == 1 for a in actions))
        self.assertNotIn("interrupt", [a.kind for a in actions])

    def test_should_keep_failure_evidence_across_successful_inspection_calls(self):
        m = TurnMonitor(protected_prefixes=[])
        actions = []
        for index in range(3):
            call_id = f"shell-{index}"
            m.observe(*started("shell", {"command": "npm test"}, call_id))
            actions.append(m.observe(*completed(call_id, False, "failed\n\nExit code: 1")))
            read_id = f"read-{index}"
            m.observe(*started("read_file", {"path": "backend/server.js"}, read_id))
            m.observe(*completed(read_id, True, "source"))

        live_actions = [action for action in actions if action is not None]
        self.assertEqual(len(live_actions), 1)
        self.assertEqual(live_actions[0].kind, "steer")
        self.assertEqual(live_actions[0].repeat_count, 3)

    def test_should_keep_peak_failure_evidence_together_in_final_correction(self):
        m = TurnMonitor(protected_prefixes=[], live_actions=False)
        for index in range(6):
            call_id = f"shell-{index}"
            m.observe(*started("shell", {"command": "npm test"}, call_id))
            m.observe(*completed(call_id, False, "npm failed\n\nExit code: 1"))

        m.observe(*started("read_file", {"path": "backend/app.py"}, "read"))
        m.observe(*completed(
            "read",
            False,
            "permission denied\n\nExit code: -1",
        ))

        correction = m.corrections()[0]
        self.assertIn("same recent error 6 times", correction)
        self.assertIn("Tool: shell", correction)
        self.assertIn("command category: test", correction)
        self.assertIn("failure kind: nonzero_exit", correction)
        self.assertIn("exit status: nonzero", correction)
        self.assertIn("npm failed", correction)
        self.assertNotIn("read_file", correction)
        self.assertNotIn("permission denied", correction)
        self.assertIn(
            "tool=shell kind=nonzero_exit count=6",
            m.correction_log_text(correction),
        )

    def test_should_reset_the_failure_window_after_material_success(self):
        m = TurnMonitor(protected_prefixes=[])
        actions = []
        for prefix in ("before", "after"):
            for index in range(2):
                call_id = f"{prefix}-{index}"
                m.observe(*started("shell", {"command": "npm run build"}, call_id))
                actions.append(
                    m.observe(*completed(call_id, False, "failed\n\nExit code: 1"))
                )
            if prefix == "before":
                m.observe(*started("shell", {"command": "npm run build"}, "passed"))
                m.observe(*completed("passed", True, "all tests passed\n\nExit code: 0"))

        self.assertEqual([action for action in actions if action is not None], [])

    def test_should_not_combine_failure_families_across_tools(self):
        m = TurnMonitor(protected_prefixes=[])
        actions = []
        for index in range(5):
            for tool in ("shell", "read_file"):
                call_id = f"{tool}-{index}"
                args = {"command": f"probe-{index}"} if tool == "shell" else {"path": f"x-{index}"}
                m.observe(*started(tool, args, call_id))
                actions.append(
                    m.observe(*completed(call_id, False, "permission denied\n\nExit code: -1"))
                )

        self.assertEqual([action for action in actions if action is not None], [])

    def test_should_interrupt_a_broad_failure_family_after_warnings(self):
        m = TurnMonitor(protected_prefixes=[])
        actions = []
        for index in range(10):
            call_id = f"c{index}"
            m.observe(*started("shell", {"command": f"probe-{index}"}, call_id))
            action = m.observe(*completed(call_id, False, "(no output)\n\nExit code: -1"))
            if action is not None:
                actions.append(action)
                m.record_action_result(action, True)

        self.assertEqual(
            [(a.kind, a.family_repeat_count) for a in actions],
            [("steer", 6), ("steer", 8), ("steer", 9), ("interrupt", 10)],
        )

    def test_should_not_emit_live_actions_when_guard_is_disabled(self):
        m = TurnMonitor(protected_prefixes=[], live_actions=False)
        for index in range(10):
            call_id = f"c{index}"
            m.observe(*started("shell", {"command": "npm test"}, call_id))
            self.assertIsNone(
                m.observe(*completed(call_id, False, "failed\n\nExit code: 1"))
            )


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

    def test_should_emit_safe_structured_shell_diagnostics(self):
        lines = []
        trace = ToolEventTrace("REQ-1 repair", lines.append)
        trace.observe(
            "tool/started",
            {
                "tool_call_id": "c1",
                "tool_name": "exec_command",
                "arguments": {"cmd": "pkill -f private-service", "timeout_secs": 20},
            },
        )
        trace.observe(
            "tool/completed",
            {
                "tool_call_id": "c1",
                "tool_name": "exec_command",
                "success": False,
                "output_preview": "(no output)\n\nExit code: -1",
                "duration_ms": 91,
            },
        )
        trace.finish()

        joined = "\n".join(lines)
        self.assertNotIn("private-service", joined)
        self.assertNotIn("(no output)", joined)
        start_event = json.loads(lines[0].split(" ", 1)[1])
        end_event = json.loads(lines[1].split(" ", 1)[1])
        summary = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual(start_event["command_tags"], ["process_cleanup"])
        self.assertEqual(start_event["timeout_secs"], 20)
        self.assertEqual(end_event["exit_code"], -1)
        self.assertEqual(end_event["exit_reason"], "no_normal_exit_code")
        self.assertEqual(end_event["failure_kind"], "no_exit_status")
        self.assertFalse(end_event["output_present"])
        self.assertEqual(summary["shell_command_tags"], {"process_cleanup": 1})
        self.assertEqual(summary["shell_failure_kinds"], {"no_exit_status": 1})


if __name__ == "__main__":
    unittest.main()

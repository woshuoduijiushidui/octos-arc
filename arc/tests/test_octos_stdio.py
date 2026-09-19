import json
import queue
import tempfile
import unittest
import uuid
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from octos_stdio import OctosProtocolError, OctosStdioSession


def task_evidence():
    digest = "sha256:" + "a" * 64
    return {
        "schema": "octos.task-evidence.v1",
        "task": {
            "requirement_id": "REQ-1",
            "phase": "implement",
            "requirement_sha256": digest,
            "requirement_ref": "/workspace/requirements/requirements.yaml",
            "name": "Login",
            "description": "Authenticate the user.",
            "acceptance_conditions": ["login succeeds"],
            "dependencies": [],
            "ancestor_constraints": [],
            "policies": ["official tests are read-only"],
        },
        "source_state": {
            "tree_sha256": digest,
            "changed_files": [],
        },
        "verification": None,
        "active_failures": [],
        "verified_behavior": [],
        "next_action": "implement REQ-1",
    }


class TaskEvidenceInputTests(unittest.TestCase):
    def test_should_keep_legacy_text_only_payload_when_capsule_is_absent(self):
        with tempfile.TemporaryDirectory() as tmp:
            session = object.__new__(OctosStdioSession)
            session.cwd = tmp

            self.assertEqual(
                session._turn_input_items("exact user prompt"),
                [{"kind": "text", "text": "exact user prompt"}],
            )

    def test_should_append_current_capsule_without_changing_text(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / ".arc" / "context" / "task-evidence.v1.json"
            path.parent.mkdir(parents=True)
            capsule = task_evidence()
            path.write_text(json.dumps(capsule), encoding="utf-8")
            session = object.__new__(OctosStdioSession)
            session.cwd = str(root)

            items = session._turn_input_items("exact user prompt")

            self.assertEqual(items[0], {"kind": "text", "text": "exact user prompt"})
            self.assertEqual(
                items[1],
                {"kind": "task_evidence", "capsule": capsule},
            )

    def test_should_reject_an_unreadable_capsule_instead_of_sending_empty_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / ".arc" / "context" / "task-evidence.v1.json"
            path.parent.mkdir(parents=True)
            path.write_text("{", encoding="utf-8")
            session = object.__new__(OctosStdioSession)
            session.cwd = str(root)

            with self.assertRaises(OctosProtocolError):
                session._turn_input_items("prompt")

    def test_run_turn_should_send_text_and_typed_evidence_together(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / ".arc" / "context" / "task-evidence.v1.json"
            path.parent.mkdir(parents=True)
            capsule = task_evidence()
            path.write_text(json.dumps(capsule), encoding="utf-8")
            turn_id = uuid.UUID("00000000-0000-0000-0000-000000000001")
            session = object.__new__(OctosStdioSession)
            session.cwd = str(root)
            session.session_id = "arc:test"
            session.proc = SimpleNamespace(poll=lambda: None)
            session.on_event = lambda *_: None
            session._send = Mock(return_value={})
            session._notifications = queue.Queue()
            session._notifications.put(
                {
                    "method": "turn/completed",
                    "params": {"turn_id": str(turn_id)},
                }
            )

            with patch("octos_stdio.uuid.uuid4", return_value=turn_id):
                ok, text = session.run_turn("exact user prompt", timeout=1)

            self.assertTrue(ok)
            self.assertEqual(text, "")
            params = session._send.call_args.args[1]
            self.assertEqual(
                params["input"],
                [
                    {"kind": "text", "text": "exact user prompt"},
                    {"kind": "task_evidence", "capsule": capsule},
                ],
            )


if __name__ == "__main__":
    unittest.main()

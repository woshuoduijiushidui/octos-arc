#!/usr/bin/env python3
"""Exercise the ARC live tool-loop guard through real stdio and a fake provider."""

import argparse
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import main as arc_main
from guard import TurnMonitor


def run(binary: Path) -> None:
    requests: list[dict] = []
    failures: list[str] = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send_json(self, status: int, payload: dict) -> None:
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.send_json(200, {"object": "list", "data": []})

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            index = len(requests)
            requests.append(request)
            if self.path != "/v1/chat/completions":
                failures.append(f"unexpected provider path: {self.path}")
                self.send_json(404, {"error": {"message": failures[-1]}})
                return
            if index >= 10:
                failures.append("the guard allowed an eleventh model request")
                self.send_json(400, {"error": {"message": failures[-1]}})
                return

            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": [{
                    "id": f"guard_shell_{index}",
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "arguments": json.dumps({
                            "command": (
                                f"printf 'fixture-output-{index}\\n' >&2; exit 7"
                            ),
                        }),
                    },
                }],
            }
            self.send_json(200, {
                "id": f"live-guard-{index}",
                "object": "chat.completion",
                "model": "live-guard-local-fixture",
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": "tool_calls",
                }],
                "usage": {
                    "prompt_tokens": 7,
                    "completion_tokens": 3,
                    "total_tokens": 10,
                },
            })

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    runtime = tempfile.TemporaryDirectory(prefix="live-tool-loop-guard-")
    root = Path(runtime.name)
    workspace = root / "workspace"
    workspace.mkdir()
    env = {
        key: os.environ[key]
        for key in ("PATH", "HOME", "TMPDIR", "LANG")
        if key in os.environ
    }
    env.update({
        "OPENAI_API_KEY": "local-test-only",
        "OCTOS_CONFIG_DIR": str(root / "config"),
        "OCTOS_DISABLE_STREAMING": "1",
        # The fixture workspace is disposable. Full access avoids the default
        # Docker-backed sandbox, which is not installed in the WSL test image.
        "OCTOS_DANGER_FULL_ACCESS": "1",
        "OCTOS_NO_PROGRESS": "0",
        "_ARC_PROVIDER": "openai",
        "_ARC_MODEL": "live-guard-local-fixture",
        "_ARC_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
        "_ARC_KEY_ENV": "OPENAI_API_KEY",
    })

    monitor = TurnMonitor([], label="REQ-guard repair")
    safe_logs: list[str] = []
    original_log = arc_main.log
    arc_main.log = safe_logs.append
    driver = None

    try:
        driver = arc_main.OctosDriver(
            str(binary),
            workspace,
            env,
            root / "data",
            20,
            root / "octos-events.jsonl",
        )
        ok, text = driver.run(
            "Exercise the local loop guard.",
            timeout=120,
            monitor=monitor,
        )
        assert not ok, text
        assert text.startswith("interrupted:"), text
        assert not failures, failures
        assert len(requests) == 10, len(requests)
        actions = [
            {
                "kind": action.kind,
                "repeat": action.repeat_count,
                "family_repeat": action.family_repeat_count,
                "accepted": accepted,
            }
            for action, accepted in monitor._action_results
        ]
        assert actions == [
            {"kind": "steer", "repeat": 1, "family_repeat": 6, "accepted": True},
            {"kind": "steer", "repeat": 1, "family_repeat": 8, "accepted": True},
            {"kind": "steer", "repeat": 1, "family_repeat": 9, "accepted": True},
            {"kind": "interrupt", "repeat": 1, "family_repeat": 10, "accepted": True},
        ], actions
        joined = "\n".join(safe_logs)
        assert "fixture-output" not in joined
        assert "printf" not in joined
        assert '"tool":"shell"' in joined
        assert '"command_fp":' in joined
        assert '"shell_failure_kinds":{"nonzero_exit":10}' in joined, joined
        event_log = (root / "octos-events.jsonl").read_text(encoding="utf-8")
        assert "fixture-output" not in event_log
        assert "printf" not in event_log
    finally:
        if driver is not None:
            driver.close()
        arc_main.log = original_log
        runtime.cleanup()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    print(
        "PASS: OctosDriver sent three live reminders and interrupted the tenth "
        "related shell failure; safe logs omitted raw data."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve())

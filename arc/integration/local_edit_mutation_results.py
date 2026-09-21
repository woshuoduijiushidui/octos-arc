#!/usr/bin/env python3
"""Verify H05 M3 no-op and final mutation results through real stdio."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from octos_stdio import OctosStdioSession


def digest(data):
    return hashlib.sha256(data).hexdigest()


def write_json(path, value):
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )


def tool_outputs(messages):
    return {
        message.get("tool_call_id"): message.get("content", "")
        for message in messages
        if message.get("role") == "tool"
    }


def run(binary, evidence):
    if evidence.exists() and any(evidence.iterdir()):
        raise RuntimeError(f"evidence directory must be empty: {evidence}")
    evidence.mkdir(parents=True, exist_ok=True)
    workspace = evidence / "workspace"
    workspace.mkdir()
    (workspace / "sites/demo").mkdir(parents=True)
    fixtures = {
        "edit.txt": "same\n",
        "sites/demo/index.html": "<h1>same</h1>\n",
        "diff.txt": "same\n",
        "actual.txt": "old\n",
    }
    for name, content in fixtures.items():
        (workspace / name).write_text(content, encoding="utf-8")
    before_mtime = {
        name: (workspace / name).stat().st_mtime_ns
        for name in ("edit.txt", "sites/demo/index.html", "diff.txt")
    }

    calls = [
        (
            "edit_file",
            {
                "path": "edit.txt",
                "old_string": "absent",
                "new_string": "absent",
            },
        ),
        (
            "read_file",
            {
                "path": "sites/demo/index.html",
            },
        ),
        (
            "write_file",
            {
                "path": "sites/demo/index.html",
                "content": "<h1>same</h1>\n",
            },
        ),
        (
            "diff_edit",
            {
                "path": "diff.txt",
                "diff": "@@ -1 +1 @@\n-same\n+same\n",
            },
        ),
        ("write_file", {"path": "empty.txt", "content": ""}),
        (
            "edit_file",
            {
                "path": "actual.txt",
                "old_string": "old",
                "new_string": "new",
            },
        ),
    ]
    requests = []
    failures = []
    events = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send_json(self, status, payload):
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.send_json(200, {"object": "list", "data": []})

        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            request = json.loads(body)
            index = len(requests)
            requests.append(request)
            if self.path != "/v1/chat/completions" or index > len(calls):
                failures.append(f"unexpected request {index}: {self.path}")
                self.send_json(400, {"error": {"message": failures[-1]}})
                return
            if index < len(calls):
                tool, arguments = calls[index]
                message = {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": f"h05_m3_{index}",
                            "type": "function",
                            "function": {
                                "name": tool,
                                "arguments": json.dumps(arguments),
                            },
                        }
                    ],
                }
                finish_reason = "tool_calls"
            else:
                message = {"role": "assistant", "content": "H05 M3 fixture complete."}
                finish_reason = "stop"
            self.send_json(
                200,
                {
                    "id": f"h05-m3-{index}",
                    "object": "chat.completion",
                    "model": "h05-local-fixture",
                    "choices": [
                        {
                            "index": 0,
                            "message": message,
                            "finish_reason": finish_reason,
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2,
                    },
                },
            )

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    target_dir = Path(__file__).resolve().parents[2] / "target"
    target_dir.mkdir(exist_ok=True)
    runtime = tempfile.TemporaryDirectory(prefix="h05-m3-", dir=target_dir)
    runtime_root = Path(runtime.name)
    env = {
        key: os.environ[key]
        for key in ("PATH", "HOME", "TMPDIR", "LANG")
        if key in os.environ
    }
    env.update(
        {
            "OPENAI_API_KEY": "local-test-only",
            "OCTOS_CONFIG_DIR": str(runtime_root / "config"),
            "OCTOS_DISABLE_STREAMING": "1",
            "OCTOS_DANGER_FULL_ACCESS": "0",
            "OCTOS_FILE_READ_DEDUP": "1",
            "OCTOS_FILE_READ_RETAINED_RECEIPTS": "0",
            "OCTOS_OUTPUT_RECOVERY": "1",
            "OCTOS_OUP_SEMANTIC_CONTEXT_MODE": "on",
            "OCTOS_READ_WINDOW": "1",
            "OCTOS_LOCAL_EDIT": "1",
        }
    )
    session = None
    try:
        session = OctosStdioSession(
            str(binary),
            workspace,
            env,
            runtime_root / "data",
            on_event=lambda method, params: events.append(
                {"method": method, "params": params}
            ),
        )
        session.bootstrap_profile(
            "openai",
            "h05-local-fixture",
            f"http://127.0.0.1:{server.server_port}/v1",
            "OPENAI_API_KEY",
        )
        session.open(timeout=30)
        ok, text = session.run_turn("Run the local H05 M3 fixture.", timeout=90)
        assert ok, (text, session.stderr_tail())
        assert not failures, failures
        assert len(requests) == len(calls) + 1, len(requests)
    finally:
        if session is not None:
            session.close()
            (evidence / "stderr.log").write_text(
                session.stderr_tail(10000), encoding="utf-8"
            )
        runtime.cleanup()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    outputs = tool_outputs(requests[-1]["messages"])
    for index in (0, 2, 3):
        output = outputs[f"call_h05_m3_{index}"]
        assert "[no_change]" in output, output
    assert "final=sha256:" in outputs["call_h05_m3_4"]
    assert "final=sha256:" in outputs["call_h05_m3_5"]
    for name, mtime in before_mtime.items():
        assert (workspace / name).stat().st_mtime_ns == mtime
        assert (workspace / name).read_text() == fixtures[name]
    assert not (workspace / "sites/demo/.git").exists()
    assert (workspace / "empty.txt").exists()
    assert (workspace / "empty.txt").stat().st_size == 0
    assert (workspace / "actual.txt").read_text() == "new\n"

    file_events = [
        event
        for event in events
        if event["method"] == "progress/updated"
        and event["params"].get("metadata", {}).get("kind") == "file_mutation"
    ]
    event_text = json.dumps(file_events, ensure_ascii=False)
    assert "edit.txt" not in event_text
    assert "index.html" not in event_text
    assert "diff.txt" not in event_text
    assert "empty.txt" in event_text
    assert "actual.txt" in event_text

    summary = {
        "schema": "octos.h05-m3-stdio-mutation-results.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "requests": len(requests),
        "no_change_calls": 3,
        "real_mutations": 2,
        "no_change_mtime_preserved": True,
        "no_change_snapshot_created": False,
        "empty_file_created": True,
        "final_versions_visible": True,
    }
    shutil.rmtree(workspace)
    write_json(evidence / "requests.json", requests)
    write_json(evidence / "events.json", events)
    write_json(evidence / "summary.json", summary)
    print(
        "PASS: real stdio preserved three no-op files and reported two real mutations."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(), args.evidence.resolve())

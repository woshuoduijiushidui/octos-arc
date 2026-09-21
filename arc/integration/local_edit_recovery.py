#!/usr/bin/env python3
"""Verify H05 M2 typed edit recovery through real stdio."""

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


def output_body(output):
    header, body = output.split("\n", 1)
    return json.loads(header), body


def run(binary, evidence):
    if evidence.exists() and any(evidence.iterdir()):
        raise RuntimeError(f"evidence directory must be empty: {evidence}")
    evidence.mkdir(parents=True, exist_ok=True)
    workspace = evidence / "workspace"
    workspace.mkdir()
    fixtures = {
        "ambiguous.txt": "开头\r\n目标 α\r\n中间\r\n目标 α\r\n更多\r\n目标 α\r\n末尾\r\n目标 α\r\n",
        "candidate.txt": "begin\ncompletely different middle here\nfinish\n",
        "none.txt": "alpha\nbeta\n",
        "fuzzy.rs": "fn one() {\n    launch();\n}\n",
    }
    for name, content in fixtures.items():
        (workspace / name).write_bytes(content.encode())

    calls = [
        {
            "path": "ambiguous.txt",
            "old_string": "目标 α",
            "new_string": "替换",
        },
        {
            "path": "candidate.txt",
            "old_string": "begin\nzzzz\nfinish",
            "new_string": "replacement",
        },
        {
            "path": "none.txt",
            "old_string": "one\ntwo\nthree",
            "new_string": "replacement",
        },
        {
            "path": "fuzzy.rs",
            "old_string": "fn one() {\nlaunch();\n}",
            "new_string": "fn one() {\n    stop();\n}",
        },
    ]
    requests = []
    failures = []

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
                message = {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": f"h05_m2_{index}",
                            "type": "function",
                            "function": {
                                "name": "edit_file",
                                "arguments": json.dumps(calls[index], ensure_ascii=False),
                            },
                        }
                    ],
                }
                finish_reason = "tool_calls"
            else:
                message = {"role": "assistant", "content": "H05 M2 fixture complete."}
                finish_reason = "stop"
            self.send_json(
                200,
                {
                    "id": f"h05-m2-{index}",
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
    runtime = tempfile.TemporaryDirectory(prefix="h05-m2-", dir=target_dir)
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
    events = []
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
        ok, text = session.run_turn("Run the local H05 M2 fixture.", timeout=90)
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
    ambiguous_header, ambiguous = output_body(outputs["call_h05_m2_0"])
    candidate_header, candidate = output_body(outputs["call_h05_m2_1"])
    none_header, none = output_body(outputs["call_h05_m2_2"])
    assert "[edit_ambiguous]" in ambiguous
    assert "count=4" in ambiguous
    assert "lines=2-2" in ambiguous
    assert ambiguous.count("suggestion=true") == 3
    assert "[edit_no_match]" in candidate
    assert "matcher=block_anchor" in candidate
    assert "score=" in candidate
    assert "[edit_no_match]" in none
    assert "suggestion=true" not in none
    for header in (ambiguous_header, candidate_header, none_header):
        assert header["recoverable"] is False
        assert "read_file_next" not in header
    typed_bytes = sum(
        len(outputs[f"call_h05_m2_{index}"].encode()) for index in range(3)
    )
    assert typed_bytes <= 8192, typed_bytes
    assert "line_trimmed" in outputs["call_h05_m2_3"]

    for name in ("ambiguous.txt", "candidate.txt", "none.txt"):
        assert (workspace / name).read_bytes() == fixtures[name].encode()
    assert (workspace / "fuzzy.rs").read_text() == "fn one() {\n    stop();\n}\n"

    summary = {
        "schema": "octos.h05-m2-stdio-recovery.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "requests": len(requests),
        "typed_output_bytes": typed_bytes,
        "candidate_limit": ambiguous.count("suggestion=true"),
        "current_versions_visible": all(
            "current=sha256:" in output
            for output in (ambiguous, candidate, none)
        ),
        "candidate_outputs_recoverable_as_file_reads": any(
            header["recoverable"]
            for header in (ambiguous_header, candidate_header, none_header)
        ),
        "fuzzy_write_preserved": True,
        "fixture_sha256": {
            name: digest(content.encode()) for name, content in sorted(fixtures.items())
        },
    }
    shutil.rmtree(workspace)
    write_json(evidence / "requests.json", requests)
    write_json(evidence / "events.json", events)
    write_json(evidence / "summary.json", summary)
    print(
        "PASS: real stdio returned bounded typed edit evidence without read receipts "
        "and preserved B fuzzy writes."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(), args.evidence.resolve())

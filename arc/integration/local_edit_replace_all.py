#!/usr/bin/env python3
"""Verify H05 M5 explicit exact-only replace_all through real stdio."""

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
    (workspace / "sites/fuzzy").mkdir(parents=True)
    (workspace / "sites/limit").mkdir(parents=True)
    (workspace / "sites/default").mkdir(parents=True)
    fixtures = {
        "batch.txt": b"same\nsame\nsame\n",
        "crlf.txt": "目标\r\nkeep\r\n目标\r\n".encode(),
        "sites/fuzzy/code.txt": b"fn one() {\n    launch();\n}\n",
        "sites/limit/many.txt": b"x" * 1001,
        "sites/default/duplicate.txt": b"same\nsame\n",
        "noop.txt": b"unchanged\n",
    }
    for name, content in fixtures.items():
        (workspace / name).write_bytes(content)
    unchanged = (
        "sites/fuzzy/code.txt",
        "sites/limit/many.txt",
        "sites/default/duplicate.txt",
        "noop.txt",
    )
    before_mtime = {
        name: (workspace / name).stat().st_mtime_ns
        for name in unchanged
    }

    calls = [
        {
            "path": "batch.txt",
            "old_string": "same",
            "new_string": "changed",
            "replace_all": True,
        },
        {
            "path": "crlf.txt",
            "old_string": "目标\n",
            "new_string": "结果\n",
            "replace_all": True,
        },
        {
            "path": "sites/fuzzy/code.txt",
            "old_string": "fn one() {\nlaunch();\n}",
            "new_string": "fn one() {\n    stop();\n}",
            "replace_all": True,
        },
        {
            "path": "sites/limit/many.txt",
            "old_string": "x",
            "new_string": "y",
            "replace_all": True,
        },
        {
            "path": "sites/default/duplicate.txt",
            "old_string": "same",
            "new_string": "changed",
        },
        {
            "path": "noop.txt",
            "old_string": "absent",
            "new_string": "absent",
            "replace_all": True,
        },
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
                message = {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": f"h05_m5_{index}",
                            "type": "function",
                            "function": {
                                "name": "edit_file",
                                "arguments": json.dumps(
                                    calls[index],
                                    ensure_ascii=False,
                                ),
                            },
                        }
                    ],
                }
                finish_reason = "tool_calls"
            else:
                message = {"role": "assistant", "content": "H05 M5 fixture complete."}
                finish_reason = "stop"
            self.send_json(
                200,
                {
                    "id": f"h05-m5-{index}",
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
    runtime = tempfile.TemporaryDirectory(prefix="h05-m5-", dir=target_dir)
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
        ok, text = session.run_turn("Run the local H05 M5 fixture.", timeout=90)
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

    edit_spec = next(
        tool
        for tool in requests[0]["tools"]
        if tool["function"]["name"] == "edit_file"
    )
    replace_all_schema = edit_spec["function"]["parameters"]["properties"]["replace_all"]
    assert replace_all_schema["type"] == "boolean"
    assert replace_all_schema["default"] is False

    outputs = tool_outputs(requests[-1]["messages"])
    assert "matcher=exact" in outputs["call_h05_m5_0"]
    assert "replacements=3" in outputs["call_h05_m5_0"]
    assert "lines=1,2,3" in outputs["call_h05_m5_0"]
    assert "matcher=line_ending_equivalent" in outputs["call_h05_m5_1"]
    fuzzy_header, fuzzy = output_body(outputs["call_h05_m5_2"])
    limit_header, limit = output_body(outputs["call_h05_m5_3"])
    default_header, default = output_body(outputs["call_h05_m5_4"])
    assert "[edit_no_match]" in fuzzy
    assert "matcher=line_trimmed" in fuzzy
    assert "[invalid_edit_input]" in limit
    assert "count=1001" in limit
    assert "limit=1000" in limit
    assert "use_structured_generator_or_explicit_script" in limit
    assert "[edit_ambiguous]" in default
    assert "[no_change]" in outputs["call_h05_m5_5"]
    for header in (fuzzy_header, limit_header, default_header):
        assert header["recoverable"] is False
        assert "read_file_next" not in header
    typed_bytes = sum(
        len(outputs[f"call_h05_m5_{index}"].encode())
        for index in (2, 3, 4)
    )
    assert typed_bytes <= 8192, typed_bytes

    assert (workspace / "batch.txt").read_bytes() == b"changed\nchanged\nchanged\n"
    assert (workspace / "crlf.txt").read_bytes() == "结果\r\nkeep\r\n结果\r\n".encode()
    for name in unchanged:
        assert (workspace / name).read_bytes() == fixtures[name]
        assert (workspace / name).stat().st_mtime_ns == before_mtime[name]
        assert not (workspace / name).parent.joinpath(".git").exists()

    file_events = [
        event
        for event in events
        if event["method"] == "progress/updated"
        and event["params"].get("metadata", {}).get("kind") == "file_mutation"
    ]
    event_text = json.dumps(file_events, ensure_ascii=False)
    assert "batch.txt" in event_text
    assert "crlf.txt" in event_text
    assert "fuzzy" not in event_text
    assert "limit" not in event_text
    assert "default" not in event_text
    assert "noop.txt" not in event_text

    summary = {
        "schema": "octos.h05-m5-stdio-replace-all.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "requests": len(requests),
        "exact_replacements": 3,
        "line_ending_equivalent_replacements": 2,
        "typed_output_bytes": typed_bytes,
        "match_limit": 1000,
        "failed_mutations": 3,
        "no_change_calls": 1,
        "failed_and_no_change_files_unchanged": True,
        "replace_all_schema_enabled": True,
    }
    shutil.rmtree(workspace)
    write_json(evidence / "requests.json", requests)
    write_json(evidence / "events.json", events)
    write_json(evidence / "summary.json", summary)
    print(
        "PASS: real stdio applied explicit exact/CRLF replace_all and kept "
        "fuzzy, over-limit, ambiguous, and no-op paths write-safe."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(), args.evidence.resolve())

#!/usr/bin/env python3
"""Capture H05 edit baselines through real stdio without a paid model.

Usage: python3 arc/integration/local_edit_baseline.py BINARY EVIDENCE_DIR
The assertions intentionally pin variant A behavior, including known gaps.
"""

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
from llm_proxy import trim_request
from octos_stdio import OctosStdioSession


class ProtocolSession(OctosStdioSession):
    def _send(self, method, params, want_response, timeout=60.0):
        if method == "approval/respond":
            params = dict(params, session_id=self.session_id)
        return super()._send(method, params, want_response, timeout)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def compact_json_bytes(value):
    return len(
        json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    )


def compact_json_sha256(value):
    encoded = json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    return digest(encoded)


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
    fixtures = {
        "current.txt": "current value\n",
        "ambiguous.txt": "same\nmiddle\nsame\n",
        "authorization.rs": (
            "fn authorize() {\n"
            "    if is_guest(user) {\n"
            "        grant_access();\n"
            "    }\n"
            "}\n"
        ),
        "drift.txt": "p1\np2\np3\np4\ntarget\nend\n",
        "noop.txt": "unchanged\n",
    }
    for name, content in fixtures.items():
        (workspace / name).write_text(content, encoding="utf-8")

    calls = [
        (
            "edit_file",
            {
                "path": "current.txt",
                "old_string": "stale value",
                "new_string": "replacement",
            },
        ),
        ("read_file", {"path": "current.txt"}),
        (
            "edit_file",
            {
                "path": "ambiguous.txt",
                "old_string": "same",
                "new_string": "changed",
            },
        ),
        (
            "edit_file",
            {
                "path": "authorization.rs",
                "old_string": (
                    "fn authorize() {\n"
                    "    if is_admin(user) {\n"
                    "        grant_access();\n"
                    "    }\n"
                    "}"
                ),
                "new_string": "fn authorize() {\n    deny_access();\n}",
            },
        ),
        (
            "diff_edit",
            {
                "path": "drift.txt",
                "diff": "@@ -1 +1 @@\n-target\n+changed\n",
            },
        ),
        (
            "edit_file",
            {
                "path": "noop.txt",
                "old_string": "unchanged",
                "new_string": "unchanged",
            },
        ),
    ]
    requests, raw_request_sizes, events, failures = [], [], [], []

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
            raw_request_sizes.append(len(body))
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
                            "id": f"h05_call_{index}",
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
                message = {"role": "assistant", "content": "H05 baseline complete."}
                finish_reason = "stop"
            self.send_json(
                200,
                {
                    "id": f"h05-local-{index}",
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
    runtime = tempfile.TemporaryDirectory(prefix="h05-", dir=target_dir)
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
        }
    )
    session = None
    try:
        session = ProtocolSession(
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
        ok, text = session.run_turn("Run the local H05 baseline fixture.", timeout=90)
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
    assert "String not found" in outputs["call_h05_call_0"]
    assert "current value" in outputs["call_h05_call_1"]
    assert "2 occurrences" in outputs["call_h05_call_2"]
    assert "block_anchor" in outputs["call_h05_call_3"]
    assert "+-3 lines" in outputs["call_h05_call_4"]
    assert outputs["call_h05_call_5"] == "Successfully edited noop.txt"
    assert (workspace / "current.txt").read_text() == fixtures["current.txt"]
    assert (workspace / "ambiguous.txt").read_text() == fixtures["ambiguous.txt"]
    assert (workspace / "authorization.rs").read_text() == (
        "fn authorize() {\n    deny_access();\n}\n"
    )
    assert (workspace / "drift.txt").read_text() == fixtures["drift.txt"]
    assert (workspace / "noop.txt").read_text() == fixtures["noop.txt"]
    event_text = json.dumps(events, ensure_ascii=False)
    assert '"kind": "file_mutation"' in event_text
    assert "authorization.rs" in event_text
    assert "noop.txt" in event_text

    tools = requests[0]["tools"]
    tool_names = sorted(tool["function"]["name"] for tool in tools)
    assert "apply_patch" not in tool_names
    trimmed = json.loads(trim_request(json.dumps(requests[0]).encode()))
    proxy_tools = trimmed["tools"]
    summary = {
        "schema": "octos.h05-m0-stdio-baseline.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "requests": len(requests),
        "raw_request_bytes": raw_request_sizes,
        "tools_at_provider": tool_names,
        "tool_schema_bytes_compact_json": compact_json_bytes(tools),
        "tool_schema_sha256_sorted_compact_json": compact_json_sha256(tools),
        "tools_after_default_arc_trim": sorted(
            tool["function"]["name"] for tool in proxy_tools
        ),
        "trimmed_tool_schema_bytes_compact_json": compact_json_bytes(proxy_tools),
        "trimmed_tool_schema_sha256_sorted_compact_json": compact_json_sha256(
            proxy_tools
        ),
        "cases": {
            "no_match_then_read": True,
            "exact_ambiguity_rejected": True,
            "block_anchor_auto_write": True,
            "diff_unique_context_beyond_three_lines_rejected": True,
            "same_content_edit_reported_success_and_mutation_event": True,
        },
        "effective_flags": {
            "OCTOS_FILE_READ_DEDUP": "1",
            "OCTOS_FILE_READ_RETAINED_RECEIPTS": "0",
            "OCTOS_OUTPUT_RECOVERY": "1",
            "OCTOS_OUP_SEMANTIC_CONTEXT_MODE": "on",
            "OCTOS_READ_WINDOW": "1",
            "OCTOS_LOCAL_EDIT": "unset",
            "OCTOS_LOCAL_EDIT_STRICT_MATCH": "unset",
            "OCTOS_ARC_CODEGEN_PATCH": "unset",
        },
        "fixture_sha256": {
            name: digest(content.encode()) for name, content in sorted(fixtures.items())
        },
    }
    shutil.rmtree(workspace)
    write_json(evidence / "requests.json", requests)
    write_json(evidence / "events.json", events)
    write_json(evidence / "summary.json", summary)
    print(
        "PASS: real stdio captured no-match reread, ambiguity, fuzzy auto-write, "
        "line drift, no-op success, and tool schema."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(), args.evidence.resolve())

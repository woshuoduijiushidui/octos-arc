#!/usr/bin/env python3
"""Verify H05 M4 diff fallback and typed failures through real stdio."""

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
    (workspace / "sites/ambiguous").mkdir(parents=True)
    (workspace / "sites/missing").mkdir(parents=True)
    fixtures = {
        "unique.txt": "p1\np2\np3\np4\ntarget\nend\n",
        "sites/ambiguous/index.txt": (
            "p1\np2\np3\np4\ntarget\np6\np7\np8\np9\ntarget\n"
        ),
        "sites/missing/index.txt": "one\ntwo\nthree\nfour\nfive\n",
        "multi.txt": "p1\np2\np3\np4\nearly\np6\np7\np8\np9\nlate\n",
    }
    for name, content in fixtures.items():
        (workspace / name).write_text(content, encoding="utf-8")
    unchanged = ("sites/ambiguous/index.txt", "sites/missing/index.txt")
    before_mtime = {
        name: (workspace / name).stat().st_mtime_ns
        for name in unchanged
    }

    calls = [
        (
            "unique.txt",
            "@@ -1 +1 @@\n-target\n+changed\n",
        ),
        (
            "sites/ambiguous/index.txt",
            "@@ -1 +1 @@\n-target\n+changed\n",
        ),
        (
            "sites/missing/index.txt",
            "@@ -3 +3 @@\n-absent\n+changed\n",
        ),
        (
            "multi.txt",
            (
                "@@ -1 +1 @@\n"
                "-early\n"
                "+EARLY\n"
                "@@ -10 +10 @@\n"
                "-late\n"
                "+early\n"
            ),
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
                path, diff = calls[index]
                message = {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": f"h05_m4_{index}",
                            "type": "function",
                            "function": {
                                "name": "diff_edit",
                                "arguments": json.dumps(
                                    {"path": path, "diff": diff},
                                    ensure_ascii=False,
                                ),
                            },
                        }
                    ],
                }
                finish_reason = "tool_calls"
            else:
                message = {"role": "assistant", "content": "H05 M4 fixture complete."}
                finish_reason = "stop"
            self.send_json(
                200,
                {
                    "id": f"h05-m4-{index}",
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
    runtime = tempfile.TemporaryDirectory(prefix="h05-m4-", dir=target_dir)
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
        ok, text = session.run_turn("Run the local H05 M4 fixture.", timeout=90)
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
    assert "positions=1->5" in outputs["call_h05_m4_0"]
    ambiguous_header, ambiguous = output_body(outputs["call_h05_m4_1"])
    missing_header, missing = output_body(outputs["call_h05_m4_2"])
    assert "[diff_context_ambiguous]" in ambiguous
    assert "count=2" in ambiguous
    assert "lines=5-5" in ambiguous
    assert "lines=10-10" in ambiguous
    assert "[diff_context_no_match]" in missing
    assert "matcher=expected_location" in missing
    assert "positions=1->5,10->10" in outputs["call_h05_m4_3"]
    for header in (ambiguous_header, missing_header):
        assert header["recoverable"] is False
        assert "read_file_next" not in header
    typed_bytes = len(outputs["call_h05_m4_1"].encode()) + len(
        outputs["call_h05_m4_2"].encode()
    )
    assert typed_bytes <= 8192, typed_bytes

    assert (workspace / "unique.txt").read_text() == "p1\np2\np3\np4\nchanged\nend\n"
    assert (
        workspace / "multi.txt"
    ).read_text() == "p1\np2\np3\np4\nEARLY\np6\np7\np8\np9\nearly\n"
    for name in unchanged:
        assert (workspace / name).read_text() == fixtures[name]
        assert (workspace / name).stat().st_mtime_ns == before_mtime[name]
        assert not (workspace / name).parent.joinpath(".git").exists()

    file_events = [
        event
        for event in events
        if event["method"] == "progress/updated"
        and event["params"].get("metadata", {}).get("kind") == "file_mutation"
    ]
    event_text = json.dumps(file_events, ensure_ascii=False)
    assert "unique.txt" in event_text
    assert "multi.txt" in event_text
    assert "ambiguous" not in event_text
    assert "missing" not in event_text

    summary = {
        "schema": "octos.h05-m4-stdio-diff-fallback.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "requests": len(requests),
        "unique_fallback_actual_line": 5,
        "ambiguous_candidates": 2,
        "typed_output_bytes": typed_bytes,
        "successful_mutations": 2,
        "failed_mutations": 2,
        "failed_files_unchanged": True,
        "failed_snapshots_created": False,
    }
    shutil.rmtree(workspace)
    write_json(evidence / "requests.json", requests)
    write_json(evidence / "events.json", events)
    write_json(evidence / "summary.json", summary)
    print(
        "PASS: real stdio applied unique full-file diff fallbacks and returned "
        "bounded typed evidence for ambiguous and missing contexts."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(), args.evidence.resolve())

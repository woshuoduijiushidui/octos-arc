#!/usr/bin/env python3
"""Verify M1 typed views through real stdio with a loopback fake provider."""

import argparse
import json
import os
from pathlib import Path
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from output_recovery_baseline import ProtocolSession, digest, write_json


def views(value):
    if isinstance(value, dict):
        if value.get("output_view") is not None:
            yield value["output_view"]
        for child in value.values():
            yield from views(child)
    elif isinstance(value, list):
        for child in value:
            yield from views(child)


def verify(binary, evidence):
    evidence.mkdir(parents=True, exist_ok=False)
    workspace = evidence / "workspace"
    workspace.mkdir()
    source = "".join(f"line {i}: 中文🙂 example text\n" for i in range(1, 5001)).encode()
    (workspace / "large.txt").write_bytes(source)
    (workspace / "stderr.txt").write_text("UNIQUE_STDERR_FAILURE\n", encoding="utf-8")
    calls = [("read_file", {"path": "large.txt", "offset": 30, "limit": 3000})]
    for name in ("shell", "bash", "exec_command"):
        command = f"printf '{name}\\n' >> executions.txt; cat large.txt; cat stderr.txt >&2; exit 7"
        calls.append((name, {"command" if name == "shell" else "cmd": command}))
    requests, events, failures = [], [], []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send_json(self, payload):
            body = json.dumps(payload).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.send_json({"object": "list", "data": []})

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            index = len(requests)
            requests.append(request)
            if index > len(calls):
                failures.append(f"unexpected request {index}")
            if index < len(calls):
                name, args = calls[index]
                message = {
                    "role": "assistant", "content": None,
                    "tool_calls": [{
                        "id": "call_reused", "type": "function",
                        "function": {"name": name, "arguments": json.dumps(args)},
                    }],
                }
            else:
                message = {"role": "assistant", "content": "Fixture finished."}
            self.send_json({
                "id": f"local-{index}", "object": "chat.completion", "model": "h03-local-fixture",
                "choices": [{"index": 0, "message": message, "finish_reason": "tool_calls" if index < len(calls) else "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    session = None
    with tempfile.TemporaryDirectory(prefix="h03-", dir=Path.cwd() / "target") as runtime:
        runtime = Path(runtime)
        env = {k: os.environ[k] for k in ("PATH", "HOME", "TMPDIR", "LANG") if k in os.environ}
        env.update({
            "OPENAI_API_KEY": "local-test-only", "OCTOS_CONFIG_DIR": str(runtime / "config"),
            "OCTOS_DISABLE_STREAMING": "1", "OCTOS_DANGER_FULL_ACCESS": "0",
            "OCTOS_OUTPUT_RECOVERY": "1", "OCTOS_READ_WINDOW": "0",
            "OCTOS_FILE_READ_DEDUP": "1", "OCTOS_FILE_READ_RETAINED_RECEIPTS": "0",
            "OCTOS_OUP_SEMANTIC_CONTEXT_MODE": "on",
        })
        try:
            session = ProtocolSession(
                str(binary), workspace, env, runtime / "data",
                on_event=lambda method, params: events.append({"method": method, "params": params}),
            )
            session.bootstrap_profile("openai", "h03-local-fixture", f"http://127.0.0.1:{server.server_port}/v1", "OPENAI_API_KEY")
            session.open(timeout=30)
            ok, text = session.run_turn("Execute the supplied local fixture.", timeout=60)
            assert ok, (text, session.stderr_tail())
            assert not failures, failures
        finally:
            if session is not None:
                session.close()
                (evidence / "stderr.log").write_text(session.stderr_tail(10000), encoding="utf-8")
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            write_json(evidence / "requests.json", requests)
            write_json(evidence / "events.json", events)
    assert len(requests) == 5, len(requests)
    schemas = {tool["function"]["name"]: tool["function"] for tool in requests[0]["tools"]}
    assert "byte_offset" in schemas["read_file"]["parameters"]["properties"]
    outputs = [m["content"] for m in requests[-1]["messages"] if m["role"] == "tool"]
    assert len(outputs) == 4, outputs
    records = []
    for content in outputs:
        header, body = content.split("\n", 1)
        header = json.loads(header)
        assert len(content.encode()) <= 8192
        assert header["recoverable"] is False
        assert header["recovery_error"] == "recovery_tool_unavailable"
        assert "[truncated]" not in content
        if header["execution"]["kind"] == "not_applicable":
            span = header["ranges"][0]
            start, end = span["start"], span["end"]
            first, last = span["lines"]
            expected = "".join(f"{first + i}│ {line}" for i, line in enumerate(source[start:end].decode().splitlines(keepends=True)))
            assert body == expected
            assert first == 30 and last < 3029
            assert header["next"]["positions"] == [["file", end]]
        else:
            assert header["execution"] == {"kind": "exited", "code": 7, "signal": None}
            assert header["success"] is False
            assert "UNIQUE_STDERR_FAILURE" in body
            assert {r["stream"] for r in header["ranges"]} >= {"stdout", "stderr"}
        records.append({"output_id": header["output_id"], "view_digest": digest(content.encode()), "bytes": len(content.encode()), "ranges": header["ranges"]})
    assert len({record["output_id"] for record in records}) == 4
    assert (workspace / "executions.txt").read_text().splitlines() == ["shell", "bash", "exec_command"]
    persisted = []
    for path in workspace.rglob("*.json"):
        if "context_ledgers" in path.parts:
            persisted.extend(views(json.loads(path.read_text())))
    for record in records:
        matches = [view for view in persisted if view["output_id"] == record["output_id"] and view["view_digest"] == record["view_digest"]]
        assert matches, ("final view not persisted", record)
        assert all(view["visible_ranges"] == record["ranges"] for view in matches)
        assert all(not view["recoverable"] and view["availability"] == "missing" for view in matches)
    write_json(evidence / "persisted-views.json", persisted)
    write_json(evidence / "summary.json", {
        "binary_sha256": digest(binary.read_bytes()), "source_sha256": digest(source),
        "requests": len(requests), "outputs": records, "persisted_views": len(persisted),
        "effective_policy": {"enabled": True, "page_bytes": 8192, "read_window": False},
        "approval_events": sum(e["method"] == "approval/requested" for e in events),
        "provider": "loopback fake; usage is synthetic",
    })
    print("PASS: real stdio final views, source ranges, three command entries and persisted metadata")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    verify(args.binary.resolve(), args.evidence.resolve())

#!/usr/bin/env python3
"""Capture H03 baseline gaps through the real stdio kernel, without a paid model.

Usage: python3 arc/integration/output_recovery_baseline.py BINARY EVIDENCE_DIR
These assertions characterize A, not the recovery contract expected of B.
All provider traffic stays on loopback; fixtures and runtime data are isolated.
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
    """Supply the required session id missing in the baseline ARC approver."""

    def _send(self, method, params, want_response, timeout=60.0):
        if method == "approval/respond":
            params = dict(params, session_id=self.session_id)
        return super()._send(method, params, want_response, timeout)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def write_json(path, data):
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def run_case(binary, root, name, window, calls):
    case = root / name
    case.mkdir()
    workspace = case / "workspace"
    workspace.mkdir()
    lines = [
        f"LINE_{i:04d} "
        + ({200: "PAGE_SENTINEL ", 500: "MIDDLE_SENTINEL "}.get(i, ""))
        + "sample text " * 8 + "\n"
        for i in range(1, 1601)
    ]
    source = "".join(lines).encode()
    (workspace / "large.txt").write_bytes(source)
    (workspace / "stdout.txt").write_bytes(source)
    (workspace / "stderr.txt").write_text("UNIQUE_STDERR_FAILURE\n", encoding="utf-8")
    requests, events, failures = [], [], []

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
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            index = len(requests)
            requests.append(request)
            if self.path != "/v1/chat/completions" or index > len(calls):
                failures.append(f"unexpected request {index}: {self.path}")
                self.send_json(400, {"error": {"message": failures[-1]}})
                return
            if index < len(calls):
                tool, arguments = calls[index]
                message = {
                    "role": "assistant", "content": None,
                    "tool_calls": [{
                        "id": f"call_{index}", "type": "function",
                        "function": {"name": tool, "arguments": json.dumps(arguments)},
                    }],
                }
                reason = "tool_calls"
            else:
                message = {"role": "assistant", "content": "Baseline fixture finished."}
                reason = "stop"
            self.send_json(200, {
                "id": f"local-{index}", "object": "chat.completion",
                "model": "h03-local-fixture",
                "choices": [{"index": 0, "message": message, "finish_reason": reason}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    # Keep the Unix socket below macOS SUN_LEN even with a long evidence path.
    runtime = tempfile.TemporaryDirectory(prefix="h03-", dir=Path.cwd() / "target")
    runtime_root = Path(runtime.name)
    # An allowlist keeps real credentials, provider routes and feature overrides
    # out of the child. HOME is inherited, never repurposed as a test directory.
    env = {key: os.environ[key] for key in ("PATH", "HOME", "TMPDIR", "LANG") if key in os.environ}
    env.update({
        "OPENAI_API_KEY": "local-test-only",
        "OCTOS_CONFIG_DIR": str(runtime_root / "config"),
        "OCTOS_DISABLE_STREAMING": "1",
        "OCTOS_DANGER_FULL_ACCESS": "0",
        "OCTOS_FILE_READ_DEDUP": "1",
        "OCTOS_FILE_READ_RETAINED_RECEIPTS": "0",
        "OCTOS_OUTPUT_RECOVERY": "0",
        "OCTOS_OUP_SEMANTIC_CONTEXT_MODE": "on",
        "OCTOS_READ_WINDOW": str(window),
    })
    session = None
    try:
        session = ProtocolSession(
            str(binary), workspace, env, runtime_root / "data",
            on_event=lambda method, params: events.append({"method": method, "params": params}),
        )
        session.bootstrap_profile(
            "openai", "h03-local-fixture",
            f"http://127.0.0.1:{server.server_port}/v1", "OPENAI_API_KEY",
        )
        session.open(timeout=30)
        ok, text = session.run_turn("Execute the supplied local baseline fixture.", timeout=60)
        assert ok, (text, session.stderr_tail())
        assert not failures, failures
        assert len(requests) == len(calls) + 1, len(requests)
    finally:
        if session is not None:
            session.close()
            (case / "stderr.log").write_text(session.stderr_tail(10000), encoding="utf-8")
        if (runtime_root / "data").exists():
            shutil.copytree(runtime_root / "data", case / "data", ignore=shutil.ignore_patterns("*.sock"))
        runtime.cleanup()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        write_json(case / "requests.json", requests)
        write_json(case / "events.json", events)
    tools = sorted(tool["function"]["name"] for tool in requests[0]["tools"])
    assert "read_file" in tools and "shell" in tools
    assert "recall" not in tools, tools
    trimmed = json.loads(trim_request(json.dumps(requests[0]).encode()))
    proxy_tools = sorted(tool["function"]["name"] for tool in trimmed["tools"])
    final_outputs = [m["content"] for m in requests[-1]["messages"] if m["role"] == "tool"]
    artifacts = [
        p.read_text(encoding="utf-8")
        for p in case.rglob("*") if p.is_file() and p.parent.name == "tool-output"
    ]
    assert artifacts, f"no persisted tool output under {case}"
    first_output = next(m["content"] for m in requests[1]["messages"] if m["role"] == "tool")
    assert first_output.endswith("\n[truncated]"), first_output[-300:]
    assert len(first_output.encode()) == 8192 + len("\n[truncated]")
    assert not any("MIDDLE_SENTINEL" in output for output in final_outputs)
    result = {
        "name": name, "read_window": bool(window), "requests": len(requests),
        "tools_at_provider": tools, "tools_after_default_arc_trim": proxy_tools,
        "source_bytes": len(source), "source_sha256": digest(source),
        "first_visible_bytes": len(first_output.encode()),
        "artifact_bytes": sorted(len(text.encode()) for text in artifacts),
        "middle_missing_from_final_messages": True,
        "recall_unavailable": True,
        "approval_events": sum(e["method"] == "approval/requested" for e in events),
    }
    if name == "window_page":
        assert any("[read_file window:" in text for text in artifacts)
        assert any("PAGE_SENTINEL" in text for text in artifacts)
        assert not any("PAGE_SENTINEL" in text for text in final_outputs)
        assert not any("[read_file window:" in text for text in final_outputs)
        result["window_footer_lost"] = True
    elif name == "requested_range":
        assert any("Continue with offset: 1001" in text for text in artifacts)
        assert not any("MIDDLE_SENTINEL" in text for text in artifacts)
        assert any("LINE_1001" in text for text in final_outputs)
        result["suggested_next_skips_missing_middle"] = True
    elif name == "shell_output":
        assert (workspace / "executions.txt").read_text().splitlines() == ["once"]
        assert any("Exit code: 7" in text for text in artifacts)
        assert not any("UNIQUE_STDERR_FAILURE" in text for text in artifacts)
        assert not any("Exit code: 7" in text for text in final_outputs)
        result.update(exit_code_lost=True, stderr_lost_before_storage=True, command_executions=1)
    write_json(case / "summary.json", result)
    return result


def verify(binary, evidence):
    evidence.mkdir(parents=True, exist_ok=True)
    # Never overwrite evidence or reuse session state from an earlier run.
    cases = [
        ("window_page", 1, [("read_file", {"path": "large.txt"})]),
        ("requested_range", 0, [
            ("read_file", {"path": "large.txt", "offset": 1, "limit": 1000}),
            ("read_file", {"path": "large.txt", "offset": 1001, "limit": 100}),
        ]),
        ("shell_output", 0, [("shell", {
            "command": "printf 'once\\n' >> executions.txt; cat stdout.txt; cat stderr.txt >&2; exit 7",
        })]),
    ]
    results = [run_case(binary, evidence, *case) for case in cases]
    write_json(evidence / "summary.json", {
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic, not measured tokens",
        "approval_adapter": "test-only supplies session_id; production ARC wrapper remains unchanged",
        "cases": results,
    })
    print("PASS: 3 real stdio cases captured; page/footer loss, skipped range, late shell storage and missing recall reproduced.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    verify(args.binary.resolve(), args.evidence.resolve())

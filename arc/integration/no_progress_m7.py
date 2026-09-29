#!/usr/bin/env python3
"""Exercise H07 C through real stdio and a loopback, unbilled provider."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from octos_stdio import OctosStdioSession


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_json(path: Path, value) -> None:
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def request_shape(index: int, request: dict) -> dict:
    messages = request.get("messages") or []
    tools = request.get("tools") or []
    return {
        "index": index,
        "message_roles": [message.get("role") for message in messages],
        "message_count": len(messages),
        "message_sha256": digest(
            json.dumps(messages, ensure_ascii=False, sort_keys=True).encode()
        ),
        "tool_names": [tool.get("function", {}).get("name") for tool in tools],
        "tool_choice": request.get("tool_choice", "auto"),
        "has_no_progress_hint": any(
            "[NO PROGRESS]" in str(message.get("content", "")) for message in messages
        ),
        "has_switch_hint": any(
            "[SWITCH REQUIRED]" in str(message.get("content", "")) for message in messages
        ),
        "semantic_reflection_prompt": any(
            "Episode category: mutate/no_progress" in str(message.get("content", ""))
            for message in messages
        ),
    }


def run(binary: Path, evidence: Path) -> None:
    if evidence.exists() and any(evidence.iterdir()):
        raise RuntimeError(f"evidence directory must be empty: {evidence}")
    evidence.mkdir(parents=True, exist_ok=True)
    workspace = evidence / "workspace"
    long_dir = workspace / "深度" / ("bounded-segment-" * 6)
    long_dir.mkdir(parents=True)
    target = long_dir / "目标🙂.txt"
    target.write_text("alpha\nbeta\n", encoding="utf-8")
    relative_target = target.relative_to(workspace).as_posix()

    requests: list[dict] = []
    failures: list[str] = []
    normal_requests = 0

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
            nonlocal normal_requests
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            index = len(requests)
            requests.append(request)
            if self.path != "/v1/chat/completions":
                failures.append(f"unexpected provider path: {self.path}")
                self.send_json(404, {"error": {"message": failures[-1]}})
                return

            is_reflection = request.get("tool_choice") == "none"
            if is_reflection:
                message = {
                    "role": "assistant",
                    "content": "Inspect the current file before choosing another bounded action.",
                }
                finish_reason = "stop"
            else:
                action_index = normal_requests
                normal_requests += 1
                if action_index < 3:
                    message = {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [{
                            "id": f"h07_m7_edit_{action_index}",
                            "type": "function",
                            "function": {
                                "name": "edit_file",
                                "arguments": json.dumps({
                                    "path": relative_target,
                                    "old_string": "never-present",
                                    "new_string": f"attempt-{action_index}",
                                }),
                            },
                        }],
                    }
                    finish_reason = "tool_calls"
                elif action_index == 3:
                    message = {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [{
                            "id": "h07_m7_diagnostic",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": json.dumps({"path": relative_target}),
                            },
                        }],
                    }
                    finish_reason = "tool_calls"
                elif action_index == 4:
                    message = {"role": "assistant", "content": "H07 M7 fixture complete."}
                    finish_reason = "stop"
                else:
                    failures.append(f"unexpected action request {action_index} at {index}")
                    self.send_json(400, {"error": {"message": failures[-1]}})
                    return

            self.send_json(200, {
                "id": f"h07-m7-{index}",
                "object": "chat.completion",
                "model": "h07-m7-local-fixture",
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": finish_reason,
                }],
                "usage": {
                    "prompt_tokens": 7,
                    "completion_tokens": 3,
                    "total_tokens": 10,
                    "prompt_tokens_details": {"cached_tokens": 2},
                },
            })

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    # Keep the serve runtime on the WSL filesystem: its local operator-control
    # endpoint is a Unix socket, which drvfs (/mnt/d) cannot create.
    runtime = tempfile.TemporaryDirectory(prefix="h07-m7-")
    runtime_root = Path(runtime.name)
    env = {
        key: os.environ[key]
        for key in ("PATH", "HOME", "TMPDIR", "LANG")
        if key in os.environ
    }
    env.update({
        "OPENAI_API_KEY": "local-test-only",
        "OCTOS_CONFIG_DIR": str(runtime_root / "config"),
        "OCTOS_DISABLE_STREAMING": "1",
        "OCTOS_DANGER_FULL_ACCESS": "0",
        "OCTOS_LOCAL_EDIT": "1",
        "OCTOS_OUTPUT_RECOVERY": "1",
        "OCTOS_READ_WINDOW": "1",
        "OCTOS_FILE_READ_DEDUP": "1",
        "OCTOS_FILE_READ_RETAINED_RECEIPTS": "0",
        "OCTOS_NO_PROGRESS": "true",
        "OCTOS_NO_PROGRESS_REFLECTION": "true",
    })
    session = None
    events: list[dict] = []
    try:
        session = OctosStdioSession(
            str(binary),
            workspace,
            env,
            runtime_root / "data",
            on_event=lambda method, params: events.append({"method": method, "params": params}),
        )
        session.bootstrap_profile(
            "openai",
            "h07-m7-local-fixture",
            f"http://127.0.0.1:{server.server_port}/v1",
            "OPENAI_API_KEY",
            timeout=180,
        )
        session.open(timeout=30)
        ok, text = session.run_turn("Diagnose the local typed edit failure.", timeout=120)
        assert ok, (text, session.stderr_tail())
        assert text == "H07 M7 fixture complete.", text
        assert not failures, failures
    except Exception:
        if session is not None:
            print(session.stderr_tail(200), file=sys.stderr)
        raise
    finally:
        if session is not None:
            session.close()
        runtime.cleanup()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    shapes = [request_shape(index, request) for index, request in enumerate(requests)]
    write_json(evidence / "request-shapes.json", shapes)
    assert len(requests) == 6, len(requests)
    reflection_indices = [
        index for index, request in enumerate(requests)
        if request.get("tool_choice") == "none"
    ]
    assert reflection_indices == [3], reflection_indices
    reflection = requests[3]
    assert len(reflection.get("messages") or []) == 2
    assert reflection.get("tools"), "reflection must preserve the action tool schema"
    assert any(
        "Episode category: mutate/no_progress" in str(message.get("content", ""))
        for message in reflection["messages"]
    )
    final_messages = requests[-1]["messages"]
    tool_messages = [message for message in final_messages if message.get("role") == "tool"]
    assert len(tool_messages) == 4, tool_messages
    assert len({message.get("tool_call_id") for message in tool_messages}) == 4
    assert target.read_text(encoding="utf-8") == "alpha\nbeta\n"
    assert not any(
        event["method"] == "turn/error" for event in events
    ), events

    write_json(evidence / "summary.json", {
        "schema": "octos.h07-m7-stdio.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "requests": len(requests),
        "reflection_requests": 1,
        "reflection_request_index": 3,
        "reflection_tool_choice": "none",
        "semantic_reflection_prompt": shapes[3]["semantic_reflection_prompt"],
        "actual_tool_executions": len(tool_messages),
        "edit_attempts": 3,
        "diagnostic_reads": 1,
        "final_message_roles": [message.get("role") for message in final_messages],
        "final_tool_names": shapes[-1]["tool_names"],
        "long_unicode_target_bytes": len(relative_target.encode()),
        "source_unchanged": True,
        "terminal_events": sum(event["method"] == "turn/error" for event in events),
    })
    print("PASS: real stdio emitted one tools-disabled reflection, executed four tools, and completed after a strategy switch.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(), args.evidence.resolve())

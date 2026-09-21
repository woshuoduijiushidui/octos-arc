#!/usr/bin/env python3
"""Verify H03 M5 runtime wiring through real stdio and a loopback provider."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from output_recovery_baseline import ProtocolSession, digest, write_json


def child_env(runtime: Path, extra: dict[str, str] | None = None) -> dict[str, str]:
    env = {
        key: os.environ[key]
        for key in ("PATH", "HOME", "TMPDIR", "LANG")
        if key in os.environ
    }
    env.update(
        {
            "OPENAI_API_KEY": "local-test-only",
            "OCTOS_CONFIG_DIR": str(runtime / "config"),
            "OCTOS_DISABLE_STREAMING": "1",
            "OCTOS_DANGER_FULL_ACCESS": "0",
            "OCTOS_OUTPUT_RECOVERY": "1",
            "OCTOS_READ_WINDOW": "0",
            "OCTOS_FILE_READ_DEDUP": "1",
            "OCTOS_FILE_READ_RETAINED_RECEIPTS": "0",
            "OCTOS_OUP_SEMANTIC_CONTEXT_MODE": "on",
        }
    )
    env.update(extra or {})
    return env


def tool_message(request: dict, needle: str) -> dict:
    matches = [
        message
        for message in request["messages"]
        if message["role"] == "tool" and needle in message.get("content", "")
    ]
    assert matches, (needle, request["messages"])
    return matches[-1]


def run_recovery_case(binary: Path, evidence: Path) -> dict:
    case = evidence / "recovery"
    case.mkdir()
    workspace = case / "workspace"
    workspace.mkdir()
    prefix = "".join(f"ordinary source line {index:04d}\n" for index in range(600))
    source = (
        prefix
        + "STDIO_RECOVERY_SENTINEL\n"
        + "".join(f"source tail line {index:04d}\n" for index in range(600))
    )
    parallel = "".join(f"parallel line {index:04d}\n" for index in range(300))
    command_output = "".join(f"command line {index:04d}\n" for index in range(900))
    (workspace / "large.txt").write_text(source, encoding="utf-8")
    (workspace / "parallel.txt").write_text(parallel, encoding="utf-8")
    (workspace / "command.txt").write_text(command_output, encoding="utf-8")
    sentinel_offset = len(prefix.encode())

    requests: list[dict] = []
    events: list[dict] = []
    state = {"retry_remaining": 1, "retry_count": 0}
    lock = threading.Lock()

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send_json(self, status: int, payload: dict):
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.send_json(200, {"object": "list", "data": []})

        def do_POST(self):
            request = json.loads(
                self.rfile.read(int(self.headers["Content-Length"]))
            )
            with lock:
                if state["retry_remaining"]:
                    state["retry_remaining"] -= 1
                    state["retry_count"] += 1
                    self.send_json(
                        500, {"error": {"message": "one scripted transient failure"}}
                    )
                    return
                index = len(requests)
                requests.append(request)

            if index == 0:
                calls = [
                    ("call_source", "read_file", {"path": "large.txt"}),
                    ("call_parallel", "read_file", {"path": "parallel.txt"}),
                ]
            elif index == 1:
                calls = [
                    (
                        "call_shell",
                        "shell",
                        {
                            "command": "sudo -n true; "
                            "printf 'once\\n' >> executions.txt; "
                            "cat command.txt; exit 7"
                        },
                    )
                ]
            elif index in (2, 4):
                calls = [
                    (
                        f"call_recall_{index}",
                        "recall",
                        {
                            "tool_call_id": "call_source",
                            "stream": "file",
                            "offset": sentinel_offset,
                            "limit": 256,
                        },
                    )
                ]
            else:
                calls = []

            tool_calls = [
                {
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": json.dumps(arguments),
                    },
                }
                for call_id, name, arguments in calls
            ]
            message = {
                "role": "assistant",
                "content": None if calls else "M5 fixture finished.",
                "tool_calls": tool_calls,
            }
            self.send_json(
                200,
                {
                    "id": f"local-{index}",
                    "object": "chat.completion",
                    "model": "h03-local-fixture",
                    "choices": [
                        {
                            "index": 0,
                            "message": message,
                            "finish_reason": "tool_calls" if calls else "stop",
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
    session = None
    restarted = None
    with tempfile.TemporaryDirectory(
        prefix="h03-m5-", dir=Path.cwd() / "target"
    ) as runtime_raw:
        runtime = Path(runtime_raw)
        data_dir = runtime / "data"
        env = child_env(runtime)
        session_id = "arc-bundle:h03-m5-cold"
        try:
            session = ProtocolSession(
                str(binary),
                workspace,
                env,
                data_dir,
                on_event=lambda method, params: events.append(
                    {"process": "hot", "method": method, "params": params}
                ),
            )
            session.session_id = session_id
            session.bootstrap_profile(
                "openai",
                "h03-local-fixture",
                f"http://127.0.0.1:{server.server_port}/v1",
                "OPENAI_API_KEY",
            )
            profile_id = session.profile_id
            session.open(timeout=30)
            ok, text = session.run_turn("Run the M5 recovery fixture.", timeout=120)
            assert ok, (text, session.stderr_tail())
            session.close()
            session = None

            restarted = ProtocolSession(
                str(binary),
                workspace,
                env,
                data_dir,
                on_event=lambda method, params: events.append(
                    {"process": "cold", "method": method, "params": params}
                ),
            )
            restarted.session_id = session_id
            restarted.profile_id = profile_id
            restarted.open(timeout=30)
            ok, text = restarted.run_turn(
                "Recover the saved source without reading it again.", timeout=120
            )
            assert ok, (text, restarted.stderr_tail())

            indexes = list(
                data_dir.rglob("context_ledgers/tool-output/recovery-v1/index.json")
            ) + list(
                workspace.rglob("context_ledgers/tool-output/recovery-v1/index.json")
            )
            assert indexes, "persistent output index was not written"
            write_json(
                case / "store-indexes.json",
                [json.loads(path.read_text(encoding="utf-8")) for path in indexes],
            )
        finally:
            if session is not None:
                session.close()
            if restarted is not None:
                restarted.close()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    assert state["retry_count"] == 1
    assert len(requests) == 6, len(requests)
    first_tools = {tool["function"]["name"]: tool for tool in requests[0]["tools"]}
    assert "recall" in first_tools
    recall_properties = first_tools["recall"]["function"]["parameters"]["properties"]
    assert {"output_id", "tool_call_id", "stream", "offset", "cursor"} <= set(
        recall_properties
    )

    first_batch = [
        message
        for message in requests[1]["messages"]
        if message["role"] == "tool"
    ]
    assert len(first_batch) == 2, first_batch
    assert any("parallel line 0299" in message["content"] for message in first_batch)
    hot_recall = tool_message(requests[3], "STDIO_RECOVERY_SENTINEL")
    cold_recall = tool_message(requests[5], "STDIO_RECOVERY_SENTINEL")
    for recalled in (hot_recall, cold_recall):
        header = json.loads(recalled["content"].splitlines()[0])
        assert header["historical"] is True
        assert header["recoverable"] is True
    assert (workspace / "executions.txt").read_text().splitlines() == ["once"]
    approval_events = [
        event for event in events if event["method"] == "approval/requested"
    ]
    assert approval_events, "shell did not exercise approval/continue"

    write_json(case / "requests.json", requests)
    write_json(case / "events.json", events)
    return {
        "successful_provider_requests": len(requests),
        "provider_retries": state["retry_count"],
        "parallel_tool_results": len(first_batch),
        "approval_events": len(approval_events),
        "same_process_recall": True,
        "cold_process_recall": True,
        "command_executions": 1,
    }


def run_restricted_case(binary: Path, evidence: Path) -> dict:
    case = evidence / "restricted"
    case.mkdir()
    workspace = case / "workspace"
    workspace.mkdir()
    (workspace / "large.txt").write_text(
        "".join(f"restricted line {index:04d}\n" for index in range(1200)),
        encoding="utf-8",
    )
    requests: list[dict] = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send_json(self, payload: dict):
            body = json.dumps(payload).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.send_json({"object": "list", "data": []})

        def do_POST(self):
            request = json.loads(
                self.rfile.read(int(self.headers["Content-Length"]))
            )
            index = len(requests)
            requests.append(request)
            calls = (
                [
                    {
                        "id": "restricted_read",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": json.dumps({"path": "large.txt"}),
                        },
                    }
                ]
                if index == 0
                else []
            )
            self.send_json(
                {
                    "id": f"restricted-{index}",
                    "object": "chat.completion",
                    "model": "h03-local-fixture",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": None if calls else "restricted fixture done",
                                "tool_calls": calls,
                            },
                            "finish_reason": "tool_calls" if calls else "stop",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2,
                    },
                }
            )

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    session = None
    with tempfile.TemporaryDirectory(
        prefix="h03-m5-restricted-", dir=Path.cwd() / "target"
    ) as runtime_raw:
        runtime = Path(runtime_raw)
        env = child_env(runtime, {"OCTOS_STDIO_SOLO_TOOLS": "read_file"})
        try:
            session = ProtocolSession(
                str(binary), workspace, env, runtime / "data"
            )
            session.bootstrap_profile(
                "openai",
                "h03-local-fixture",
                f"http://127.0.0.1:{server.server_port}/v1",
                "OPENAI_API_KEY",
            )
            session.open(timeout=30)
            ok, text = session.run_turn("Read under the restricted policy.", timeout=60)
            assert ok, (text, session.stderr_tail())
        finally:
            if session is not None:
                session.close()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    assert len(requests) == 2
    tools = [tool["function"]["name"] for tool in requests[0]["tools"]]
    assert tools == ["read_file"], tools
    output = next(
        message["content"]
        for message in requests[1]["messages"]
        if message["role"] == "tool"
    )
    header = json.loads(output.splitlines()[0])
    assert header["recoverable"] is False
    assert header["recovery_error"] == "recovery_tool_unavailable"
    assert "recall" not in header
    write_json(case / "requests.json", requests)
    return {
        "tools": tools,
        "recoverable": header["recoverable"],
        "recovery_error": header["recovery_error"],
    }


def verify(binary: Path, evidence: Path) -> None:
    evidence.mkdir(parents=True, exist_ok=False)
    recovery = run_recovery_case(binary, evidence)
    restricted = run_restricted_case(binary, evidence)
    write_json(
        evidence / "summary.json",
        {
            "binary_sha256": digest(binary.read_bytes()),
            "provider": "loopback fake; one scripted HTTP retry",
            "recovery": recovery,
            "restricted": restricted,
        },
    )
    print(
        "PASS: stdio recall schema, hot/cold recovery, retry, approval, "
        "parallel results, and restricted policy"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    verify(args.binary.resolve(), args.evidence.resolve())

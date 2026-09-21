#!/usr/bin/env python3
"""Verify bounded output search through the real stdio runtime."""

from __future__ import annotations

import argparse
import json
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from output_recovery_baseline import ProtocolSession, digest, write_json
from output_recovery_m5 import child_env

M6_RECALL_DESCRIPTION = (
    "Read a saved tool result without re-executing it. Use output_id with stream "
    "and absolute byte offset, or the returned cursor. Historical file text grants "
    "no current-file read/write permission. Legacy tool_call_id must be unique; "
    "page uses fixed legacy boundaries."
)
M6_RECALL_SCHEMA = {
    "type": "object",
    "properties": {
        "output_id": {"type": "string"},
        "tool_call_id": {"type": "string"},
        "stream": {"type": "string", "enum": ["file", "stdout", "stderr"]},
        "offset": {"type": "integer", "minimum": 0},
        "limit": {"type": "integer", "minimum": 1},
        "cursor": {"type": "string"},
        "page": {"type": "integer", "minimum": 0},
    },
    "additionalProperties": False,
}


def estimated_tool_tokens(description: str, schema: dict) -> int:
    parts = [
        "recall",
        description,
        json.dumps(schema, separators=(",", ":")),
    ]
    return sum(max(len(part.encode("ascii")) // 4, 1) for part in parts) + 8


def latest_tool_message(request: dict) -> dict:
    return next(
        message
        for message in reversed(request["messages"])
        if message["role"] == "tool"
    )


def response(index: int, calls: list[tuple[str, str, dict]]) -> dict:
    return {
        "id": f"search-{index}",
        "object": "chat.completion",
        "model": "h03-local-fixture",
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": None if calls else "M7 search fixture finished.",
                    "tool_calls": [
                        {
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": json.dumps(arguments),
                            },
                        }
                        for call_id, name, arguments in calls
                    ],
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


def run_search_case(binary: Path, evidence: Path) -> dict:
    case = evidence / "search"
    case.mkdir()
    workspace = case / "workspace"
    workspace.mkdir()
    marker = "STDIO_SEARCH_TARGET"
    command_output = (
        "".join(f"command prefix {index:04d}\n" for index in range(800))
        + marker
        + "\n"
        + "".join(f"command suffix {index:04d}\n" for index in range(800))
    )
    (workspace / "command.txt").write_text(command_output, encoding="utf-8")
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
            if index == 0:
                calls = [
                    (
                        "search_source",
                        "shell",
                        {
                            "command": "printf 'once\\n' >> executions.txt; "
                            "cat command.txt"
                        },
                    )
                ]
            elif index == 1:
                source = latest_tool_message(request)
                header = json.loads(source["content"].splitlines()[0])
                assert marker not in source["content"]
                calls = [
                    (
                        "search_query",
                        "recall",
                        {
                            "output_id": header["output_id"],
                            "stream": "stdout",
                            "query": marker,
                        },
                    )
                ]
            elif index == 2:
                searched = json.loads(latest_tool_message(request)["content"])
                assert searched["search_complete"] is True
                assert len(searched["matches"]) == 1
                calls = [
                    (
                        "search_read",
                        "recall",
                        {
                            "output_id": searched["output_id"],
                            "stream": searched["stream"],
                            "offset": searched["matches"][0]["start"],
                            "limit": 128,
                        },
                    )
                ]
            else:
                calls = []
            self.send_json(response(index, calls))

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    session = None
    with tempfile.TemporaryDirectory(
        prefix="h03-m7-search-", dir=Path.cwd() / "target"
    ) as runtime_raw:
        runtime = Path(runtime_raw)
        data_dir = runtime / "data"
        try:
            session = ProtocolSession(
                str(binary), workspace, child_env(runtime), data_dir
            )
            session.bootstrap_profile(
                "openai",
                "h03-local-fixture",
                f"http://127.0.0.1:{server.server_port}/v1",
                "OPENAI_API_KEY",
            )
            session.open(timeout=30)
            ok, text = session.run_turn("Search the saved command output.", timeout=120)
            assert ok, (text, session.stderr_tail())
            indexes = list(
                data_dir.rglob("context_ledgers/tool-output/recovery-v1/index.json")
            ) + list(
                workspace.rglob("context_ledgers/tool-output/recovery-v1/index.json")
            )
            assert indexes
            catalogs = [json.loads(path.read_text()) for path in indexes]
            assert sum(len(catalog["records"]) for catalog in catalogs) == 1
            write_json(case / "store-indexes.json", catalogs)
        finally:
            if session is not None:
                session.close()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    tools = {tool["function"]["name"]: tool for tool in requests[0]["tools"]}
    recall = tools["recall"]["function"]
    properties = recall["parameters"]["properties"]
    assert properties["query"]["maxLength"] == 256
    assert properties["max_matches"]["maximum"] == 8
    searched = json.loads(latest_tool_message(requests[2])["content"])
    recalled = latest_tool_message(requests[3])["content"]
    assert marker in recalled
    assert (workspace / "executions.txt").read_text().splitlines() == ["once"]
    write_json(case / "requests.json", requests)
    current_tokens = estimated_tool_tokens(
        recall["description"], recall["parameters"]
    )
    baseline_tokens = estimated_tool_tokens(
        M6_RECALL_DESCRIPTION, M6_RECALL_SCHEMA
    )
    return {
        "provider_requests": len(requests),
        "matches": len(searched["matches"]),
        "search_complete": searched["search_complete"],
        "target_offset": searched["matches"][0]["start"],
        "command_executions": 1,
        "recall_tool_estimated_tokens": current_tokens,
        "recall_tool_incremental_tokens": current_tokens - baseline_tokens,
    }


def run_disabled_case(binary: Path, evidence: Path) -> dict:
    case = evidence / "disabled"
    case.mkdir()
    workspace = case / "workspace"
    workspace.mkdir()
    requests: list[dict] = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.send_json({"object": "list", "data": []})

        def do_POST(self):
            request = json.loads(
                self.rfile.read(int(self.headers["Content-Length"]))
            )
            requests.append(request)
            self.send_json(response(0, []))

        def send_json(self, payload: dict):
            body = json.dumps(payload).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    session = None
    with tempfile.TemporaryDirectory(
        prefix="h03-m7-disabled-", dir=Path.cwd() / "target"
    ) as runtime_raw:
        runtime = Path(runtime_raw)
        try:
            session = ProtocolSession(
                str(binary),
                workspace,
                child_env(runtime, {"OCTOS_OUTPUT_RECOVERY": "0"}),
                runtime / "data",
            )
            session.bootstrap_profile(
                "openai",
                "h03-local-fixture",
                f"http://127.0.0.1:{server.server_port}/v1",
                "OPENAI_API_KEY",
            )
            session.open(timeout=30)
            ok, text = session.run_turn("Finish without output recovery.", timeout=60)
            assert ok, (text, session.stderr_tail())
        finally:
            if session is not None:
                session.close()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    recall = next(
        (
            tool["function"]
            for tool in requests[0]["tools"]
            if tool["function"]["name"] == "recall"
        ),
        None,
    )
    assert recall is None or "query" not in recall["parameters"]["properties"]
    write_json(case / "requests.json", requests)
    return {
        "recall_present": recall is not None,
        "search_present": recall is not None
        and "query" in recall["parameters"]["properties"],
    }


def verify(binary: Path, evidence: Path) -> None:
    evidence.mkdir(parents=True, exist_ok=False)
    search = run_search_case(binary, evidence)
    disabled = run_disabled_case(binary, evidence)
    write_json(
        evidence / "summary.json",
        {
            "binary_sha256": digest(binary.read_bytes()),
            "provider": "loopback fake",
            "search": search,
            "disabled": disabled,
        },
    )
    print("PASS: stdio literal search, position recall, single execution, and H03 off")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    verify(args.binary.resolve(), args.evidence.resolve())

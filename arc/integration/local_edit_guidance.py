#!/usr/bin/env python3
"""Verify H05 M1 prompt and schema guidance through real stdio."""

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from llm_proxy import trim_request
from octos_stdio import OctosStdioSession


GUIDANCE_HEADING = "## File editing"
EDIT_TOOLS = {"write_file", "edit_file", "diff_edit"}
A_SCHEMA_SHA256 = "79f97b610f4178e8820be561ce5fac943e6d43d04cb07e0147ac31aec2acdafa"
A_SYSTEM_PROMPT_SHA256 = "e08e53f9769fc480fc2cf5e4229d04e5327fde34d4905afc419e2966226d33e4"


def compact_json(value):
    return json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")


def digest(value):
    return hashlib.sha256(value).hexdigest()


def system_prompt(request):
    return next(
        message["content"]
        for message in request["messages"]
        if message["role"] == "system"
    )


def normalized_system_prompt(request):
    return re.sub(
        r"^AppUi session workspace root: .*$",
        "AppUi session workspace root: <WORKSPACE>",
        system_prompt(request),
        flags=re.MULTILINE,
    )


def tool_names(request):
    return [tool["function"]["name"] for tool in request["tools"]]


def schema_without_descriptions(tools):
    normalized = copy.deepcopy(tools)
    for tool in normalized:
        tool["function"].pop("description", None)
    return normalized


def run_case(binary, root, value):
    case = root / value
    case.mkdir()
    workspace = case / "workspace"
    workspace.mkdir()
    requests = []

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
            requests.append(json.loads(body))
            self.send_json(
                200,
                {
                    "id": f"h05-m1-{value}",
                    "object": "chat.completion",
                    "model": "h05-local-fixture",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "H05 M1 fixture complete.",
                            },
                            "finish_reason": "stop",
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
    runtime = tempfile.TemporaryDirectory(prefix=f"h05-m1-{value}-", dir=target_dir)
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
            "OCTOS_LOCAL_EDIT": value,
        }
    )
    session = None
    try:
        session = OctosStdioSession(
            str(binary), workspace, env, runtime_root / "data"
        )
        session.bootstrap_profile(
            "openai",
            "h05-local-fixture",
            f"http://127.0.0.1:{server.server_port}/v1",
            "OPENAI_API_KEY",
        )
        session.open(timeout=30)
        ok, text = session.run_turn("Return the fixture response.", timeout=60)
        assert ok, (text, session.stderr_tail())
        assert len(requests) == 1, len(requests)
        stderr = session.stderr_tail(10000)
    finally:
        if session is not None:
            session.close()
        runtime.cleanup()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    request = requests[0]
    shutil.rmtree(workspace)
    (case / "request.json").write_text(
        json.dumps(request, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    (case / "stderr.log").write_text(stderr, encoding="utf-8")
    return request, stderr


def verify(binary, evidence):
    if evidence.exists() and any(evidence.iterdir()):
        raise RuntimeError(f"evidence directory must be empty: {evidence}")
    evidence.mkdir(parents=True, exist_ok=True)
    off, _ = run_case(binary, evidence, "0")
    on, _ = run_case(binary, evidence, "1")
    unknown, unknown_stderr = run_case(binary, evidence, "unexpected")

    off_tools = off["tools"]
    on_tools = on["tools"]
    unknown_tools = unknown["tools"]
    assert digest(compact_json(off_tools)) == A_SCHEMA_SHA256
    assert compact_json(unknown_tools) == compact_json(off_tools)
    assert tool_names(on) == tool_names(off)
    assert "apply_patch" not in tool_names(on)
    assert schema_without_descriptions(on_tools) == schema_without_descriptions(off_tools)

    off_by_name = {tool["function"]["name"]: tool for tool in off_tools}
    on_by_name = {tool["function"]["name"]: tool for tool in on_tools}
    changed = sorted(
        name
        for name in off_by_name
        if compact_json(off_by_name[name]) != compact_json(on_by_name[name])
    )
    assert set(changed) == EDIT_TOOLS, changed
    for name in EDIT_TOOLS:
        before = copy.deepcopy(off_by_name[name])
        after = copy.deepcopy(on_by_name[name])
        before["function"].pop("description")
        after["function"].pop("description")
        assert before == after, name

    off_prompt = normalized_system_prompt(off)
    on_prompt = normalized_system_prompt(on)
    unknown_prompt = normalized_system_prompt(unknown)
    assert digest(off_prompt.encode()) == A_SYSTEM_PROMPT_SHA256
    assert GUIDANCE_HEADING not in off_prompt
    assert unknown_prompt == off_prompt
    assert on_prompt.count(GUIDANCE_HEADING) == 1
    for phrase in (
        "New file",
        "One contiguous change",
        "Multiple separated changes",
        "complete current contents",
    ):
        assert phrase in on_prompt, phrase
    trimmed_off = json.loads(trim_request(json.dumps(off).encode()))
    trimmed_on = json.loads(trim_request(json.dumps(on).encode()))
    trimmed_off_prompt = normalized_system_prompt(trimmed_off)
    trimmed_on_prompt = normalized_system_prompt(trimmed_on)
    trimmed_off_tools = trimmed_off["tools"]
    trimmed_on_tools = trimmed_on["tools"]
    assert GUIDANCE_HEADING not in trimmed_off_prompt
    assert trimmed_on_prompt.count(GUIDANCE_HEADING) == 1
    assert tool_names(trimmed_on) == tool_names(trimmed_off)
    assert schema_without_descriptions(trimmed_on_tools) == schema_without_descriptions(
        trimmed_off_tools
    )
    assert "unknown OCTOS_LOCAL_EDIT value" in unknown_stderr

    schema_delta = len(compact_json(on_tools)) - len(compact_json(off_tools))
    prompt_delta = len(on_prompt.encode()) - len(off_prompt.encode())
    trimmed_schema_delta = len(compact_json(trimmed_on_tools)) - len(
        compact_json(trimmed_off_tools)
    )
    trimmed_prompt_delta = len(trimmed_on_prompt.encode()) - len(
        trimmed_off_prompt.encode()
    )
    summary = {
        "schema": "octos.h05-m1-guidance.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "off_schema_sha256": digest(compact_json(off_tools)),
        "on_schema_sha256": digest(compact_json(on_tools)),
        "unknown_schema_sha256": digest(compact_json(unknown_tools)),
        "off_schema_bytes": len(compact_json(off_tools)),
        "on_schema_bytes": len(compact_json(on_tools)),
        "schema_delta_bytes": schema_delta,
        "system_prompt_delta_bytes": prompt_delta,
        "fixed_input_delta_bytes": schema_delta + prompt_delta,
        "trimmed_schema_delta_bytes": trimmed_schema_delta,
        "trimmed_system_prompt_delta_bytes": trimmed_prompt_delta,
        "trimmed_fixed_input_delta_bytes": trimmed_schema_delta + trimmed_prompt_delta,
        "changed_tools": changed,
        "tool_names_unchanged": tool_names(on) == tool_names(off),
        "guidance_occurrences": on_prompt.count(GUIDANCE_HEADING),
        "unknown_value_fell_back_to_off": (
            compact_json(unknown_tools) == compact_json(off_tools)
            and unknown_prompt == off_prompt
        ),
    }
    (evidence / "summary.json").write_text(
        json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        "PASS: H05 off is byte-compatible, on changes only three descriptions "
        "and one prompt section, unknown values fall back to off."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    verify(args.binary.resolve(), args.evidence.resolve())

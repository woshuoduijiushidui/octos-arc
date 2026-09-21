#!/usr/bin/env python3
"""Compare H05 B with the C strict-matcher variant through real stdio."""

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
from octos_stdio import OctosStdioSession


FUZZY_MATCHERS = [
    "line_trimmed",
    "line_trimmed",
    "whitespace_normalized",
    "whitespace_normalized",
    "indentation_flexible",
    "escape_normalized",
    "block_anchor",
    "target_trailing_whitespace",
]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def compact_json(value):
    return json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")


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
    if output.startswith("{") and "\n" in output:
        return output.split("\n", 1)[1]
    return output


def normalized_system_prompt(request):
    prompt = next(
        message["content"]
        for message in request["messages"]
        if message["role"] == "system"
    )
    return re.sub(
        r"^AppUi session workspace root: .*$",
        "AppUi session workspace root: <WORKSPACE>",
        prompt,
        flags=re.MULTILINE,
    )


def fixtures():
    return {
        "python.py": b"def run():\n    value = 1\n",
        "markdown.md": b"first  \nsecond\n",
        "string.js": b'const label = "a  b";\n',
        "template.html": b"<p>Hello   {{ name }}</p>\n",
        "indent.rs": b"fn wrapper() {\n    step_one();\n    step_two();\n}\n",
        "escaped.txt": b"alpha {\n    beta();\n}\n",
        "block.rs": b"fn compute() {\n    let total = base + extra;\n    total * 2\n}\n",
        "trailing.md": b"alpha  \nbeta\n",
        "exact.txt": b"alpha\n",
        "crlf.txt": b"one\r\ntwo\r\n",
        "diff-crlf.txt": b"alpha\r\nbeta\r\n",
        "all.txt": b"same\nsame\n",
        "duplicate.txt": b"same\nsame\n",
        "noop.txt": b"same\n",
    }


def calls():
    return [
        (
            "edit_file",
            {
                "path": "python.py",
                "old_string": "def run():\nvalue = 1",
                "new_string": "def run():\n    value = 2",
            },
        ),
        (
            "edit_file",
            {
                "path": "markdown.md",
                "old_string": "first\nsecond",
                "new_string": "changed\nsecond",
            },
        ),
        (
            "edit_file",
            {
                "path": "string.js",
                "old_string": 'const label = "a b";',
                "new_string": 'const label = "safe";',
            },
        ),
        (
            "edit_file",
            {
                "path": "template.html",
                "old_string": "<p>Hello {{ name }}</p>",
                "new_string": "<p>Welcome {{ name }}</p>",
            },
        ),
        (
            "edit_file",
            {
                "path": "indent.rs",
                "old_string": "\n        step_one();\n        step_two();\n\n",
                "new_string": "    merged_steps();",
            },
        ),
        (
            "edit_file",
            {
                "path": "escaped.txt",
                "old_string": "alpha {\\n    beta();",
                "new_string": "alpha {\n    gamma();",
            },
        ),
        (
            "edit_file",
            {
                "path": "block.rs",
                "old_string": (
                    "fn compute() {\n"
                    "    let total = base + offset;\n"
                    "    total * 2\n"
                    "}"
                ),
                "new_string": "fn compute() {\n    base * 3\n}",
            },
        ),
        (
            "diff_edit",
            {
                "path": "trailing.md",
                "diff": "@@ -1,2 +1,2 @@\n-alpha\n+changed\n beta\n",
            },
        ),
        (
            "edit_file",
            {
                "path": "exact.txt",
                "old_string": "alpha",
                "new_string": "beta",
            },
        ),
        (
            "edit_file",
            {
                "path": "crlf.txt",
                "old_string": "one\ntwo\n",
                "new_string": "uno\ndos\n",
            },
        ),
        (
            "diff_edit",
            {
                "path": "diff-crlf.txt",
                "diff": "@@ -10,2 +10,2 @@\n alpha\n-beta\n+changed\n",
            },
        ),
        (
            "edit_file",
            {
                "path": "all.txt",
                "old_string": "same",
                "new_string": "changed",
                "replace_all": True,
            },
        ),
        (
            "edit_file",
            {
                "path": "duplicate.txt",
                "old_string": "same",
                "new_string": "changed",
            },
        ),
        (
            "edit_file",
            {
                "path": "noop.txt",
                "old_string": "absent",
                "new_string": "absent",
            },
        ),
    ]


def run_case(binary, evidence, strict):
    evidence.mkdir(parents=True)
    workspace = evidence / "workspace"
    workspace.mkdir()
    original = fixtures()
    for name, content in original.items():
        (workspace / name).write_bytes(content)
    sequence = calls()
    requests = []
    events = []
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
            if self.path != "/v1/chat/completions" or index > len(sequence):
                failures.append(f"unexpected request {index}: {self.path}")
                self.send_json(400, {"error": {"message": failures[-1]}})
                return
            if index < len(sequence):
                tool, arguments = sequence[index]
                message = {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": f"h05_m7_{index}",
                            "type": "function",
                            "function": {
                                "name": tool,
                                "arguments": json.dumps(
                                    arguments,
                                    ensure_ascii=False,
                                ),
                            },
                        }
                    ],
                }
                finish_reason = "tool_calls"
            else:
                message = {"role": "assistant", "content": "H05 M7 fixture complete."}
                finish_reason = "stop"
            self.send_json(
                200,
                {
                    "id": f"h05-m7-{strict}-{index}",
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
    runtime = tempfile.TemporaryDirectory(
        prefix=f"h05-m7-{strict}-",
        dir=target_dir,
    )
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
            "OCTOS_LOCAL_EDIT_STRICT_MATCH": strict,
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
        ok, text = session.run_turn("Run the H05 M7 fixture.", timeout=120)
        assert ok, (text, session.stderr_tail())
        assert not failures, failures
        assert len(requests) == len(sequence) + 1, len(requests)
    finally:
        if session is not None:
            session.close()
            stderr = session.stderr_tail(10000)
            (evidence / "stderr.log").write_text(stderr, encoding="utf-8")
        runtime.cleanup()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    outputs = tool_outputs(requests[-1]["messages"])
    final_files = {
        name: (workspace / name).read_bytes()
        for name in original
    }
    mutation_events = [
        event
        for event in events
        if event["method"] == "progress/updated"
        and event["params"].get("metadata", {}).get("kind") == "file_mutation"
    ]
    shutil.rmtree(workspace)
    write_json(evidence / "requests.json", requests)
    write_json(evidence / "events.json", events)
    return {
        "requests": requests,
        "outputs": outputs,
        "events": events,
        "mutation_events": mutation_events,
        "original": original,
        "final_files": final_files,
        "stderr": stderr,
    }


def verify(binary, evidence):
    if evidence.exists() and any(evidence.iterdir()):
        raise RuntimeError(f"evidence directory must be empty: {evidence}")
    evidence.mkdir(parents=True, exist_ok=True)
    baseline = run_case(binary, evidence / "b", "0")
    strict = run_case(binary, evidence / "c", "1")

    baseline_outputs = baseline["outputs"]
    strict_outputs = strict["outputs"]
    output = lambda result, index: result[f"call_h05_m7_{index}"]

    for index, expected_matcher in enumerate(FUZZY_MATCHERS):
        baseline_output = output(baseline_outputs, index)
        strict_output = output(strict_outputs, index)
        if index == 7:
            assert "Applied 1 hunk(s)" in baseline_output, baseline_output
        else:
            assert f"matcher={expected_matcher}" in baseline_output, (
                index,
                baseline_output,
            )
        assert "[edit_no_match]" in strict_output or (
            "[diff_context_no_match]" in strict_output
        ), (index, strict_output)
        assert f"matcher={expected_matcher}" in strict_output, (
            index,
            strict_output,
        )
        assert "suggestion=true" in strict_output, (index, strict_output)

    for index, expected_matcher in (
        (8, "exact"),
        (9, "line_ending_equivalent"),
        (11, "exact"),
    ):
        assert f"matcher={expected_matcher}" in output(
            strict_outputs,
            index,
        ), output(strict_outputs, index)
    assert "positions=10->1" in output(strict_outputs, 10)
    for result in (baseline_outputs, strict_outputs):
        assert "[edit_ambiguous]" in output(result, 12)
        assert "[no_change]" in output(result, 13)

    for index in range(len(FUZZY_MATCHERS)):
        path = calls()[index][1]["path"]
        assert strict["final_files"][path] == strict["original"][path], path
        assert baseline["final_files"][path] != baseline["original"][path], path

    assert strict["final_files"]["exact.txt"] == b"beta\n"
    assert strict["final_files"]["crlf.txt"] == b"uno\r\ndos\r\n"
    assert strict["final_files"]["diff-crlf.txt"] == b"alpha\r\nchanged\r\n"
    assert strict["final_files"]["all.txt"] == b"changed\nchanged\n"
    assert strict["final_files"]["duplicate.txt"] == b"same\nsame\n"
    assert strict["final_files"]["noop.txt"] == b"same\n"
    assert len(baseline["mutation_events"]) == len(calls()) - 2
    assert len(strict["mutation_events"]) == 4

    baseline_request = baseline["requests"][0]
    strict_request = strict["requests"][0]
    assert normalized_system_prompt(baseline_request) == normalized_system_prompt(
        strict_request
    )
    baseline_tools = baseline_request["tools"]
    strict_tools = strict_request["tools"]
    assert [
        tool["function"]["name"] for tool in baseline_tools
    ] == [
        tool["function"]["name"] for tool in strict_tools
    ]
    assert "apply_patch" not in {
        tool["function"]["name"] for tool in strict_tools
    }
    baseline_by_name = {
        tool["function"]["name"]: tool for tool in baseline_tools
    }
    strict_by_name = {
        tool["function"]["name"]: tool for tool in strict_tools
    }
    changed_descriptions = []
    for name in baseline_by_name:
        before = copy.deepcopy(baseline_by_name[name])
        after = copy.deepcopy(strict_by_name[name])
        assert before["function"]["parameters"] == after["function"]["parameters"]
        if before["function"]["description"] != after["function"]["description"]:
            changed_descriptions.append(name)
    assert sorted(changed_descriptions) == ["diff_edit", "edit_file"]
    assert "only as suggestions" in strict_by_name["edit_file"]["function"][
        "description"
    ]
    assert "only as suggestions" in strict_by_name["diff_edit"]["function"][
        "description"
    ]

    for stderr in (baseline["stderr"], strict["stderr"]):
        assert "local-test-only" not in stderr
        assert "value = 1" not in stderr
        assert "grant_access" not in stderr

    strict_failure_bytes = sum(
        len(output(strict_outputs, index).encode())
        for index in range(len(FUZZY_MATCHERS))
    )
    assert strict_failure_bytes <= 8192, strict_failure_bytes
    baseline_schema_bytes = len(compact_json(baseline_tools))
    strict_schema_bytes = len(compact_json(strict_tools))
    summary = {
        "schema": "octos.h05-m7-stdio-strict-match.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "requests_per_variant": len(calls()) + 1,
        "fuzzy_cases": len(FUZZY_MATCHERS),
        "b_fuzzy_writes": len(FUZZY_MATCHERS),
        "c_fuzzy_writes": 0,
        "c_fuzzy_suggestions": len(FUZZY_MATCHERS),
        "safe_cases": 4,
        "shared_non_mutation_cases": 2,
        "b_mutations": len(baseline["mutation_events"]),
        "c_mutations": len(strict["mutation_events"]),
        "strict_failure_output_bytes": strict_failure_bytes,
        "b_schema_bytes": baseline_schema_bytes,
        "c_schema_bytes": strict_schema_bytes,
        "schema_delta_bytes": strict_schema_bytes - baseline_schema_bytes,
        "b_schema_sha256": digest(compact_json(baseline_tools)),
        "c_schema_sha256": digest(compact_json(strict_tools)),
        "system_prompt_unchanged": (
            normalized_system_prompt(baseline_request)
            == normalized_system_prompt(strict_request)
        ),
        "tool_names_unchanged": True,
        "input_schemas_unchanged": True,
        "changed_descriptions": sorted(changed_descriptions),
        "failed_files_unchanged": True,
        "ordinary_stderr_contains_source_or_credentials": False,
    }
    write_json(evidence / "summary.json", summary)
    print(
        "PASS: strict mode converted all fuzzy edit/diff writes into bounded "
        "suggestions while exact and CRLF/LF-equivalent edits still succeeded."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    verify(args.binary.resolve(), args.evidence.resolve())

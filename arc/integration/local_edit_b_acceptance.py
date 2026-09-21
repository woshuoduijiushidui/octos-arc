#!/usr/bin/env python3
"""Verify the frozen H05 B workflow and its bounded observations via stdio."""

import argparse
from collections import Counter
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


def digest(data):
    return hashlib.sha256(data).hexdigest()


def compact_json_bytes(value):
    return len(
        json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    )


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


def display_body(output):
    if output.startswith("{") and "\n" in output:
        return output.split("\n", 1)[1]
    return output


def result_code(tool, output):
    match = re.search(r"\[([a-z_]+)\]", display_body(output))
    if match:
        return match.group(1)
    return "success" if tool == "read_file" else "modified"


def matcher(output):
    match = re.search(r"\bmatcher=([a-z_]+)", display_body(output))
    return match.group(1) if match else None


def long_occurrence(label, marker, fill):
    return f"{label} {fill * 220} {marker} {fill * 220}\n"


def run(binary, evidence):
    if evidence.exists() and any(evidence.iterdir()):
        raise RuntimeError(f"evidence directory must be empty: {evidence}")
    evidence.mkdir(parents=True, exist_ok=True)
    workspace = evidence / "workspace"
    workspace.mkdir()
    (workspace / "sites/budget").mkdir(parents=True)
    (workspace / "sites/diff").mkdir(parents=True)
    (workspace / "sites/extra").mkdir(parents=True)
    (workspace / "sites/fuzzy").mkdir(parents=True)
    (workspace / "sites/noop").mkdir(parents=True)

    retry_lines = [
        long_occurrence("first", "needle", "A"),
        long_occurrence("second", "needle", "B"),
        long_occurrence("third", "needle", "C"),
    ]
    budget_lines = [
        long_occurrence("one", "token", "D"),
        long_occurrence("two", "token", "E"),
        long_occurrence("three", "token", "F"),
    ]
    candidate_middle = "G" * 1200
    extra_middle = "K" * 1200
    diff_ambiguous = "".join(
        f"{fill * 600}\ntarget\n{fill.lower() * 600}\n"
        for fill in ("H", "I", "J")
    )
    fixtures = {
        "retry.txt": "".join(retry_lines).encode(),
        "sites/budget/ambiguous.txt": "".join(budget_lines).encode(),
        "candidate.txt": f"start\n{candidate_middle}\nfinish\n".encode(),
        "sites/extra/candidate.txt": f"start\n{extra_middle}\nfinish\n".encode(),
        "sites/diff/ambiguous.txt": diff_ambiguous.encode(),
        "sites/fuzzy/refused.rs": b"fn refused() {\n    launch();\n}\n",
        "fuzzy.rs": b"fn fuzzy() {\n    launch();\n}\n",
        "exact.txt": b"alpha\n",
        "all.txt": "目标\r\nkeep\r\n目标\r\n".encode(),
        "diff.txt": b"p1\np2\np3\np4\ntarget\nend\n",
        "sites/noop/noop.txt": b"same\n",
    }
    for name, content in fixtures.items():
        (workspace / name).write_bytes(content)

    retry_old = f"first {'A' * 220} needle"
    retry_new = f"first {'A' * 220} changed"
    candidate_old = f"start\n{candidate_middle}\nfinish\n"
    calls = [
        (
            "edit_file",
            {
                "path": "retry.txt",
                "old_string": "needle",
                "new_string": "changed",
            },
        ),
        (
            "edit_file",
            {
                "path": "retry.txt",
                "old_string": retry_old,
                "new_string": retry_new,
            },
        ),
        (
            "edit_file",
            {
                "path": "sites/budget/ambiguous.txt",
                "old_string": "token",
                "new_string": "changed",
            },
        ),
        (
            "edit_file",
            {
                "path": "candidate.txt",
                "old_string": "start\noutdated\nfinish",
                "new_string": "replacement",
            },
        ),
        ("read_file", {"path": "candidate.txt"}),
        (
            "edit_file",
            {
                "path": "candidate.txt",
                "old_string": candidate_old,
                "new_string": "resolved\n",
            },
        ),
        (
            "diff_edit",
            {
                "path": "sites/diff/ambiguous.txt",
                "diff": "@@ -100 +100 @@\n-target\n+changed\n",
            },
        ),
        (
            "edit_file",
            {
                "path": "sites/fuzzy/refused.rs",
                "old_string": "fn refused() {\nlaunch();\n}",
                "new_string": "fn refused() {\n    stop();\n}",
                "replace_all": True,
            },
        ),
        (
            "edit_file",
            {
                "path": "fuzzy.rs",
                "old_string": "fn fuzzy() {\nlaunch();\n}",
                "new_string": "fn fuzzy() {\n    stop();\n}",
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
                "path": "all.txt",
                "old_string": "目标\n",
                "new_string": "结果\n",
                "replace_all": True,
            },
        ),
        (
            "diff_edit",
            {
                "path": "diff.txt",
                "diff": "@@ -1 +1 @@\n-target\n+changed\n",
            },
        ),
        (
            "edit_file",
            {
                "path": "sites/noop/noop.txt",
                "old_string": "absent",
                "new_string": "absent",
            },
        ),
        ("write_file", {"path": "created.txt", "content": "created\n"}),
        (
            "edit_file",
            {
                "path": "sites/extra/candidate.txt",
                "old_string": "start\noutdated\nfinish",
                "new_string": "replacement",
            },
        ),
    ]
    requests = []
    raw_request_sizes = []
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
                            "id": f"h05_m6_{index}",
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
                message = {"role": "assistant", "content": "H05 B fixture complete."}
                finish_reason = "stop"
            self.send_json(
                200,
                {
                    "id": f"h05-m6-{index}",
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
    runtime = tempfile.TemporaryDirectory(prefix="h05-m6-", dir=target_dir)
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
        ok, text = session.run_turn("Run the frozen H05 B fixture.", timeout=120)
        assert ok, (text, session.stderr_tail())
        assert not failures, failures
        assert len(requests) == len(calls) + 1, len(requests)
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
    output = lambda index: outputs[f"call_h05_m6_{index}"]
    body = lambda index: display_body(output(index))

    assert "[edit_ambiguous]" in body(0)
    assert "matcher=exact" in body(0)
    assert "matcher=exact" in output(1)
    assert "[edit_ambiguous]" in body(2)
    assert "[edit_no_match]" in body(3)
    assert "matcher=block_anchor" in body(3)
    assert candidate_middle in output(4)
    assert "matcher=exact" in output(5)
    assert "[diff_context_ambiguous]" in body(6)
    assert "[edit_no_match]" in body(7)
    assert "matcher=line_trimmed" in body(7)
    assert "matcher=line_trimmed" in output(8)
    assert "matcher=exact" in output(9)
    assert "matcher=line_ending_equivalent" in output(10)
    assert "replacements=2" in output(10)
    assert "positions=1->5" in output(11)
    assert "[no_change]" in output(12)
    assert "matcher=whole_file" in output(13)
    assert "[edit_no_match]" in body(14)
    assert "matcher=block_anchor" in body(14)

    near_budget_indices = (0, 2, 3, 6, 7, 14)
    near_budget_bytes = sum(len(output(index).encode()) for index in near_budget_indices)
    assert 5500 <= near_budget_bytes <= 8192, near_budget_bytes
    for index in (0, 2, 3, 6, 7, 14):
        rendered = output(index)
        header = json.loads(rendered.split("\n", 1)[0])
        assert header["recoverable"] is False
        assert "read_file_next" not in header

    assert (workspace / "retry.txt").read_text().startswith(retry_new)
    assert (workspace / "candidate.txt").read_text() == "resolved\n"
    assert (workspace / "sites/budget/ambiguous.txt").read_bytes() == fixtures[
        "sites/budget/ambiguous.txt"
    ]
    assert (workspace / "sites/diff/ambiguous.txt").read_bytes() == fixtures[
        "sites/diff/ambiguous.txt"
    ]
    assert (workspace / "sites/fuzzy/refused.rs").read_bytes() == fixtures[
        "sites/fuzzy/refused.rs"
    ]
    assert (workspace / "fuzzy.rs").read_text() == "fn fuzzy() {\n    stop();\n}\n"
    assert (workspace / "exact.txt").read_text() == "beta\n"
    assert (workspace / "all.txt").read_bytes() == "结果\r\nkeep\r\n结果\r\n".encode()
    assert (workspace / "diff.txt").read_text() == "p1\np2\np3\np4\nchanged\nend\n"
    assert (workspace / "sites/noop/noop.txt").read_bytes() == fixtures[
        "sites/noop/noop.txt"
    ]
    assert (workspace / "created.txt").read_text() == "created\n"
    assert (workspace / "sites/extra/candidate.txt").read_bytes() == fixtures[
        "sites/extra/candidate.txt"
    ]

    tool_events = [
        event
        for event in events
        if event["method"] == "tool/completed"
    ]
    mutation_events = [
        event
        for event in events
        if event["method"] == "progress/updated"
        and event["params"].get("metadata", {}).get("kind") == "file_mutation"
    ]
    assert len(tool_events) == len(calls)
    assert len(mutation_events) == 7

    final_messages = requests[-1]["messages"]
    call_messages = [
        message
        for message in final_messages
        if message.get("role") == "assistant" and message.get("tool_calls")
    ]
    assert len(call_messages) == len(calls)
    assert call_messages[1]["tool_calls"][0]["function"]["name"] == "edit_file"
    assert call_messages[4]["tool_calls"][0]["function"]["name"] == "read_file"
    assert call_messages[5]["tool_calls"][0]["function"]["name"] == "edit_file"

    codes = Counter(
        result_code(calls[index][0], output(index))
        for index in range(len(calls))
    )
    matchers = Counter(
        value
        for value in (matcher(output(index)) for index in range(len(calls)))
        if value is not None
    )
    argument_bytes = Counter()
    for tool, arguments in calls:
        argument_bytes[tool] += compact_json_bytes(arguments)
    result_text_bytes = sum(len(output(index).encode()) for index in range(len(calls)))
    candidate_count = sum(
        display_body(output(index)).count("suggestion=true")
        for index in range(len(calls))
    )
    replacement_count = sum(
        int(match.group(1))
        for index in range(len(calls))
        if calls[index][0] == "edit_file"
        for match in [re.search(r"\breplacements=(\d+)", output(index))]
        if match is not None
    )
    hunk_count = sum(
        int(match.group(1))
        for index in range(len(calls))
        if calls[index][0] == "diff_edit"
        for match in [re.search(r"\bApplied (\d+) hunk", output(index))]
        if match is not None
    )
    stderr_text = (evidence / "stderr.log").read_text(encoding="utf-8")
    for forbidden in (
        "local-test-only",
        candidate_middle,
        retry_old,
        json.dumps(calls[0][1], ensure_ascii=False),
    ):
        assert forbidden not in stderr_text

    edit_spec = next(
        tool
        for tool in requests[0]["tools"]
        if tool["function"]["name"] == "edit_file"
    )
    assert edit_spec["function"]["parameters"]["properties"]["replace_all"][
        "default"
    ] is False
    assert "apply_patch" not in {
        tool["function"]["name"] for tool in requests[0]["tools"]
    }

    summary = {
        "schema": "octos.h05-m6-stdio-b-acceptance.v1",
        "binary_sha256": digest(binary.read_bytes()),
        "provider": "loopback fake; usage fields are synthetic",
        "requests": len(requests),
        "tool_calls": len(calls),
        "result_codes": dict(sorted(codes.items())),
        "outcome_counts": {
            "no_match": codes["edit_no_match"] + codes["diff_context_no_match"],
            "ambiguous": (
                codes["edit_ambiguous"] + codes["diff_context_ambiguous"]
            ),
            "no_change": codes["no_change"],
            "stale": codes["stale_file_version"],
        },
        "matchers": dict(sorted(matchers.items())),
        "candidate_count": candidate_count,
        "replacement_count": replacement_count,
        "hunk_count": hunk_count,
        "successful_mutations": len(mutation_events),
        "formatter_expanded_count": 0,
        "failure_recovery": {
            "attempts": 2,
            "successful_next_edits": 2,
            "with_extra_read": 1,
            "without_extra_read": 1,
        },
        "argument_bytes": dict(sorted(argument_bytes.items())),
        "whole_file_argument_bytes": argument_bytes["write_file"],
        "local_edit_argument_bytes": (
            argument_bytes["edit_file"] + argument_bytes["diff_edit"]
        ),
        "result_text_bytes": result_text_bytes,
        "near_8k_candidate_result_bytes": near_budget_bytes,
        "structured_metadata_bytes": None,
        "structured_metadata_source": "h05_m6_observability",
        "raw_request_bytes": raw_request_sizes,
        "replace_all_schema_enabled": True,
        "default_tools_exclude_apply_patch": True,
        "ordinary_stderr_contains_source_or_credentials": False,
    }
    shutil.rmtree(workspace)
    write_json(evidence / "requests.json", requests)
    write_json(evidence / "events.json", events)
    write_json(evidence / "summary.json", summary)
    print(
        "PASS: frozen H05 B recovered after bounded failures, exercised "
        "exact/fuzzy/replace-all/diff paths, and recorded byte costs."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(), args.evidence.resolve())

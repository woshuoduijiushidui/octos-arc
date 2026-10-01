"""Guard rules (A7): watch one turn's tool events and the final message and
produce short corrective sentences for the next prompt.

Detects: completion claims with no verification command; the same tool error
three times in a row; writes into protected paths (specs, requirements, .arc).
"""

from __future__ import annotations

from collections import Counter
import hashlib
import json
import os
import re
import time
from typing import Callable

_VERIFY = re.compile(r"\b(npm run build|npm start|npm run start|node \S+\.js|curl\b|playwright|wget\b|node --check)", re.I)
_CLAIM = re.compile(r"\b(implemented|complete[d]?|done|verified|passes|passing|finished|working)\b|✅", re.I)
_WRITE_TOOLS = {"write_file", "edit_file", "apply_patch", "create_file", "append_file"}
_SHELL_TOOLS = {"bash", "shell", "exec", "run_command"}
_REDIRECT = re.compile(r"(?:>>?|tee\s+(?:-a\s+)?|cp\s+\S+\s+|mv\s+\S+\s+|sed\s+-i\S*\s+(?:'[^']*'|\S+)\s+)\s*(\S+)")
_NODE_ID = re.compile(r"\b(REQ-\d+(?:[.\-]\d+)*)\b")


class ToolEventTrace:
    """Emit privacy-safe, line-oriented tool diagnostics into the main log."""

    def __init__(self, label: str, emit: Callable[[str], None],
                 observation: str = "complete") -> None:
        self.label = str(label or "unlabelled")[:160]
        match = _NODE_ID.search(self.label)
        self.node = match.group(1) if match else None
        self.emit = emit
        self.observation = observation
        self.started_at = time.monotonic()
        self._fingerprint_key = os.urandom(16)
        self._sequence = 0
        self._completed = 0
        self._failed = 0
        self._unmatched_completions = 0
        self._counts: Counter[str] = Counter()
        self._failures: Counter[str] = Counter()
        self._duration_ms: Counter[str] = Counter()
        self._signatures: Counter[tuple[str, str]] = Counter()
        self._failure_signatures: Counter[tuple[str, str]] = Counter()
        self._slowest_calls: list[dict] = []
        self._pending: dict[str, dict] = {}
        self._protocol_turns: set[str] = set()
        self._last_signature: tuple[str, str] | None = None
        self._repeat_streak = 0
        self._max_repeat_streak = 0

    def _fingerprint(self, value) -> tuple[str, int]:
        try:
            encoded = json.dumps(
                value,
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            ).encode("utf-8")
        except (TypeError, ValueError):
            encoded = str(value).encode("utf-8", errors="replace")
        digest = hashlib.blake2s(
            encoded,
            key=self._fingerprint_key,
            digest_size=8,
        ).hexdigest()
        return digest, len(encoded)

    def _write(self, prefix: str, event: str, **fields) -> None:
        payload = {
            "schema": "octos.arc.tool-log.v1",
            "event": event,
            "turn": self.label,
            **fields,
        }
        if self.node:
            payload["node"] = self.node
        self.emit(
            prefix
            + " "
            + json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
        )

    def observe(self, method: str, params: dict) -> None:
        if method == "tool/started":
            self._observe_started(params)
        elif method == "tool/completed":
            self._observe_completed(params)

    def mark_incomplete(self, reason: str) -> None:
        self.observation = (
            "partial" if self._sequence or self._completed else "unavailable"
        )
        self._write(
            "[tool.trace]",
            "observation",
            observation=self.observation,
            reason=reason,
        )

    def _observe_started(self, params: dict) -> None:
        self._sequence += 1
        sequence = self._sequence
        tool = str(params.get("tool_name") or "unknown")[:120]
        call_id = str(params.get("tool_call_id") or "")[:160]
        protocol_turn = str(params.get("turn_id") or "")[:160]
        if protocol_turn:
            self._protocol_turns.add(protocol_turn)
        arguments = params.get("arguments")
        args_fp, args_bytes = self._fingerprint(arguments)
        call_fp, _ = self._fingerprint(call_id)
        signature = (tool, args_fp)
        self._counts[tool] += 1
        self._signatures[signature] += 1
        if signature == self._last_signature:
            self._repeat_streak += 1
        else:
            self._last_signature = signature
            self._repeat_streak = 1
        self._max_repeat_streak = max(
            self._max_repeat_streak, self._repeat_streak
        )
        pending_key = call_id or f"missing:{sequence}"
        self._pending[pending_key] = {
            "seq": sequence,
            "tool": tool,
            "started_at": time.monotonic(),
        }
        argument_keys = []
        if isinstance(arguments, dict):
            argument_keys = sorted(str(key)[:80] for key in arguments)[:32]
        self._write(
            "[tool.event]",
            "start",
            seq=sequence,
            call_fp=call_fp,
            tool=tool,
            args_fp=args_fp,
            args_bytes=args_bytes,
            argument_keys=argument_keys,
            repeat_streak=self._repeat_streak,
            elapsed_ms=round((time.monotonic() - self.started_at) * 1000),
        )

    def _observe_completed(self, params: dict) -> None:
        call_id = str(params.get("tool_call_id") or "")[:160]
        call_fp, _ = self._fingerprint(call_id)
        pending = self._pending.pop(call_id, None)
        if pending is None:
            self._unmatched_completions += 1
        sequence = pending["seq"] if pending else None
        tool = str(
            params.get("tool_name") or (pending or {}).get("tool") or "unknown"
        )[:120]
        success = params.get("success")
        if success is False:
            self._failed += 1
            self._failures[tool] += 1
        self._completed += 1
        output_preview = params.get("output_preview") or ""
        output_fp, output_bytes = self._fingerprint(output_preview)
        if success is False:
            normalized_error_fp, _ = self._fingerprint(
                re.sub(r"\d+", "#", str(output_preview))
            )
            self._failure_signatures[(tool, normalized_error_fp)] += 1
        duration_ms = params.get("duration_ms")
        if not isinstance(duration_ms, (int, float)) and pending:
            duration_ms = round((time.monotonic() - pending["started_at"]) * 1000)
        if isinstance(duration_ms, (int, float)) and not isinstance(duration_ms, bool):
            duration_ms = max(0, round(duration_ms))
            self._duration_ms[tool] += duration_ms
            self._slowest_calls.append(
                {"seq": sequence, "tool": tool, "duration_ms": duration_ms}
            )
        self._write(
            "[tool.event]",
            "end",
            seq=sequence,
            call_fp=call_fp,
            tool=tool,
            success=success,
            duration_ms=duration_ms,
            output_fp=output_fp,
            output_bytes=output_bytes,
            elapsed_ms=round((time.monotonic() - self.started_at) * 1000),
            matched_start=pending is not None,
        )

    @staticmethod
    def _top_repeated(counter: Counter[tuple[str, str]]) -> list[dict]:
        repeated = [
            {"tool": tool, "fingerprint": fingerprint, "count": count}
            for (tool, fingerprint), count in counter.items()
            if count > 1
        ]
        return sorted(
            repeated,
            key=lambda item: (-item["count"], item["tool"], item["fingerprint"]),
        )[:8]

    def finish(self) -> None:
        self._write(
            "[tool.summary]",
            "summary",
            started=self._sequence,
            completed=self._completed,
            pending=len(self._pending),
            failed=self._failed,
            unmatched_completions=self._unmatched_completions,
            protocol_turns=len(self._protocol_turns),
            elapsed_ms=round((time.monotonic() - self.started_at) * 1000),
            counts=dict(sorted(self._counts.items())),
            failures=dict(sorted(self._failures.items())),
            duration_ms_by_tool=dict(sorted(self._duration_ms.items())),
            repeated_calls=self._top_repeated(self._signatures),
            repeated_failures=self._top_repeated(self._failure_signatures),
            slowest_calls=sorted(
                self._slowest_calls,
                key=lambda item: (-item["duration_ms"], item["tool"]),
            )[:8],
            max_consecutive_repeat=self._max_repeat_streak,
            observation=self.observation,
        )


class TurnMonitor:
    def __init__(self, protected_prefixes: list[str], repeat_threshold: int = 3,
                 expect_verification: bool = True, allowed_prefixes: list[str] | None = None,
                 label: str = "unlabelled") -> None:
        self.protected = [p for p in protected_prefixes if p]
        self.allowed = [p for p in (allowed_prefixes or []) if p]
        self.label = label
        self.repeat_threshold = repeat_threshold
        self.expect_verification = expect_verification
        self.wrote_files = False
        self.verified = False
        self.tool_calls = 0
        self.errors_in_a_row = 0
        self._last_error = None
        self._max_repeat = 0
        self._repeated_error = ""
        self.protected_writes: list[str] = []
        self.written_paths: list[str] = []
        self._pending: dict[str, tuple[str, dict]] = {}
        self._final_text = ""

    # -- events -----------------------------------------------------------
    def observe(self, method: str, params: dict) -> None:
        if method == "tool/started":
            self.tool_calls += 1
            name = str(params.get("tool_name") or "")
            args = params.get("arguments") or {}
            self._pending[str(params.get("tool_call_id"))] = (name, args)
            if name in _WRITE_TOOLS:
                self.wrote_files = True
                self._note_path(str(args.get("path") or args.get("file_path") or ""))
            elif name in _SHELL_TOOLS:
                cmd = str(args.get("cmd") or args.get("command") or "")
                if _VERIFY.search(cmd):
                    self.verified = True
                if re.search(r"\b(cat|echo|printf|tee|cp|mv|sed)\b.*(>|tee|-i)", cmd) or re.search(r"\b(cp|mv)\s", cmd):
                    self.wrote_files = True
                    for m in _REDIRECT.finditer(cmd):
                        self._note_path(m.group(1).strip("'\""))
        elif method == "tool/completed":
            ok = bool(params.get("success", True))
            preview = str(params.get("output_preview") or "")[:300]
            if not ok:
                key = re.sub(r"\d+", "#", preview)
                if key == self._last_error:
                    self.errors_in_a_row += 1
                else:
                    self._last_error, self.errors_in_a_row = key, 1
                if self.errors_in_a_row > self._max_repeat:
                    self._max_repeat, self._repeated_error = self.errors_in_a_row, preview
            else:
                self._last_error, self.errors_in_a_row = None, 0

    def _note_path(self, path: str) -> None:
        if not path:
            return
        self.written_paths.append(path)
        if any(path.startswith(a) or f"/{a}" in path for a in self.allowed):
            return
        for prefix in self.protected:
            if path.startswith(prefix) or f"/{prefix}" in path:
                self.protected_writes.append(path)
                break

    def finish(self, final_text: str) -> None:
        self._final_text = final_text or ""

    # -- verdicts ---------------------------------------------------------
    def corrections(self) -> list[str]:
        out: list[str] = []
        if self.expect_verification and self.wrote_files and not self.verified and _CLAIM.search(self._final_text):
            out.append("Your previous turn claimed completion without running any build, start or "
                       "request command. Use the supplied isolated verification command before claiming success. "
                       "If no verification entry is supplied, build and exercise the app in a disposable copy "
                       "so validation does not change the delivered application's persistent data.")
        if self._max_repeat >= self.repeat_threshold:
            out.append(f"You hit the same error {self._max_repeat} times in a row "
                       f"({self._repeated_error[:160]!r}). Stop repeating the command; diagnose the "
                       "root cause (read the file / port / path involved) and change approach.")
        if self.protected_writes:
            out.append("You modified protected files that must never change: "
                       + ", ".join(sorted(set(self.protected_writes))[:5])
                       + ". Revert nothing yourself; only touch frontend/ and backend/ from now on.")
        return out

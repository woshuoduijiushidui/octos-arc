"""Trusted, task-local evidence for ARC generation and acceptance."""

from __future__ import annotations

import hashlib
import json
import os
import re
import tempfile
from dataclasses import asdict, dataclass, replace
from pathlib import Path
from typing import Any

from acceptance import RunSummary, TestOutcome, failure_signature


TASK_EVIDENCE_SCHEMA = "octos.task-evidence.v1"
TASK_EVIDENCE_RELATIVE_PATH = Path(".arc/context/task-evidence.v1.json")
ACCEPTANCE_EVIDENCE_SCHEMA = "octos.acceptance-evidence.v1"
ACCEPTANCE_EVIDENCE_RELATIVE_DIR = Path(".arc/evidence")
MAX_CAPSULE_BYTES = 128 * 1024
OFFICIAL_TEST_POLICY = "official tests are read-only"
_RUN_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
_SOURCE_ROOTS = ("frontend", "backend")
_IGNORED_SOURCE_PARTS = {
    ".arc",
    ".git",
    ".next",
    "coverage",
    "dist",
    "dist-ssr",
    "node_modules",
    "__pycache__",
}
_EXPECTED = re.compile(r"(?im)^\s*Expected(?: string)?:\s*(.+?)\s*$")
_ACTUAL = re.compile(r"(?im)^\s*(?:Received|Actual)(?: string)?:\s*(.+?)\s*$")


class TaskEvidenceError(RuntimeError):
    pass


class TaskEvidenceWriteError(TaskEvidenceError):
    pass


@dataclass(frozen=True)
class TaskContract:
    requirement_id: str
    phase: str
    requirement_sha256: str
    requirement_ref: str
    name: str
    description: str
    acceptance_conditions: tuple[str, ...]
    dependencies: tuple[str, ...]
    ancestor_constraints: tuple[str, ...]
    policies: tuple[str, ...] = (OFFICIAL_TEST_POLICY,)

    def to_dict(self) -> dict[str, Any]:
        return {
            "requirement_id": self.requirement_id,
            "phase": self.phase,
            "requirement_sha256": self.requirement_sha256,
            "requirement_ref": self.requirement_ref,
            "name": self.name,
            "description": self.description,
            "acceptance_conditions": list(self.acceptance_conditions),
            "dependencies": list(self.dependencies),
            "ancestor_constraints": list(self.ancestor_constraints),
            "policies": list(self.policies),
        }


@dataclass(frozen=True)
class ChangedFile:
    path: str
    sha256: str
    purpose: str

    def to_dict(self) -> dict[str, str]:
        return {
            "path": self.path,
            "sha256": self.sha256,
            "purpose": self.purpose,
        }


@dataclass(frozen=True)
class SourceState:
    tree_sha256: str
    changed_files: tuple[ChangedFile, ...]

    def to_dict(self) -> dict[str, Any]:
        return {
            "tree_sha256": self.tree_sha256,
            "changed_files": [item.to_dict() for item in self.changed_files],
        }


@dataclass(frozen=True)
class VerificationRun:
    run_id: str
    source_sha256: str
    command: str
    exit_code: int | None
    passed: int
    total: int

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "source_sha256": self.source_sha256,
            "command": self.command,
            "exit_code": self.exit_code,
            "passed": self.passed,
            "total": self.total,
        }


@dataclass(frozen=True)
class ActiveFailure:
    test_id: str
    location: str
    status: str
    expected: str | None
    actual: str | None
    signature: str
    occurrences: int
    run_id: str
    artifact_ref: str | None = None
    artifact_sha256: str | None = None
    artifact_bytes: int | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "test_id": self.test_id,
            "location": self.location,
            "status": self.status,
            "expected": self.expected,
            "actual": self.actual,
            "signature": self.signature,
            "occurrences": self.occurrences,
            "run_id": self.run_id,
            "artifact_ref": self.artifact_ref,
            "artifact_sha256": self.artifact_sha256,
            "artifact_bytes": self.artifact_bytes,
        }


@dataclass(frozen=True)
class VerifiedBehavior:
    test_id: str
    run_id: str
    source_sha256: str

    def to_dict(self) -> dict[str, str]:
        return {
            "test_id": self.test_id,
            "run_id": self.run_id,
            "source_sha256": self.source_sha256,
        }


@dataclass(frozen=True)
class TaskEvidenceCapsule:
    task: TaskContract
    source_state: SourceState
    verification: VerificationRun | None = None
    active_failures: tuple[ActiveFailure, ...] = ()
    verified_behavior: tuple[VerifiedBehavior, ...] = ()
    next_action: str = ""
    schema: str = TASK_EVIDENCE_SCHEMA

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema": self.schema,
            "task": self.task.to_dict(),
            "source_state": self.source_state.to_dict(),
            "verification": self.verification.to_dict() if self.verification else None,
            "active_failures": [item.to_dict() for item in self.active_failures],
            "verified_behavior": [item.to_dict() for item in self.verified_behavior],
            "next_action": self.next_action,
        }


def serialize_capsule(capsule: TaskEvidenceCapsule) -> bytes:
    data = (
        json.dumps(
            capsule.to_dict(),
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n"
    ).encode("utf-8")
    if len(data) > MAX_CAPSULE_BYTES:
        raise TaskEvidenceWriteError(
            f"task evidence capsule is {len(data)} bytes; limit is {MAX_CAPSULE_BYTES}"
        )
    return data


def _write_atomic_bytes(path: Path, data: bytes, label: str) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        fd, raw_tmp = tempfile.mkstemp(
            prefix=f".{path.name}.tmp-", dir=str(path.parent)
        )
    except OSError as exc:
        raise TaskEvidenceWriteError(f"prepare {label} write failed: {exc}") from exc

    tmp = Path(raw_tmp)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, path)
        try:
            directory_fd = os.open(str(path.parent), os.O_RDONLY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        except OSError:
            pass
    except OSError as exc:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise TaskEvidenceWriteError(f"write {label} failed: {exc}") from exc


def write_capsule_atomic(path: Path, capsule: TaskEvidenceCapsule) -> None:
    _write_atomic_bytes(path, serialize_capsule(capsule), "task evidence")


def _normalized_text(value: Any) -> str:
    text = str(value or "").replace("\r\n", "\n").replace("\r", "\n")
    return "\n".join(line.rstrip() for line in text.splitlines()).strip()


def _bounded_text(value: Any, max_bytes: int) -> str:
    text = _normalized_text(value)
    encoded = text.encode("utf-8")
    if len(encoded) <= max_bytes:
        return text
    suffix = b" [truncated]"
    end = max(0, max_bytes - len(suffix))
    while end > 0:
        try:
            prefix = encoded[:end].decode("utf-8")
            return prefix + suffix.decode("ascii")
        except UnicodeDecodeError:
            end -= 1
    return suffix[:max_bytes].decode("ascii", errors="ignore")


def _normalized_value(value: Any) -> Any:
    if isinstance(value, dict):
        return {
            str(key): _normalized_value(item)
            for key, item in sorted(value.items(), key=lambda pair: str(pair[0]))
        }
    if isinstance(value, list):
        return [_normalized_value(item) for item in value]
    if isinstance(value, str):
        return _normalized_text(value)
    return value


def _sha256_json(value: Any) -> str:
    data = json.dumps(
        _normalized_value(value),
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return "sha256:" + hashlib.sha256(data).hexdigest()


def _sha256_bytes(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def _node_semantics(node: dict) -> dict[str, Any]:
    return {
        "id": _normalized_text(node.get("id")),
        "name": _normalized_text(node.get("name")),
        "description": _normalized_text(node.get("description")),
        "scenarios": _normalized_value(node.get("scenarios") or []),
        "dependencies": sorted(
            {_normalized_text(dep) for dep in node.get("dependencies") or [] if str(dep).strip()}
        ),
    }


def _acceptance_conditions(node: dict) -> tuple[str, ...]:
    conditions = []
    for scenario in node.get("scenarios") or []:
        if not isinstance(scenario, dict):
            continue
        parts = []
        for step in scenario.get("steps") or []:
            if not isinstance(step, dict):
                continue
            keyword = _normalized_text(step.get("keyword")).upper()
            content = _normalized_text(step.get("content"))
            if keyword or content:
                parts.append(" ".join(part for part in (keyword, content) if part))
        name = _normalized_text(scenario.get("name")) or "scenario"
        condition = " ".join(parts)
        conditions.append(f"{name}: {condition}".rstrip())
    return tuple(conditions)


def _constraint_text(node: dict) -> str:
    node_id = _normalized_text(node.get("id"))
    name = _normalized_text(node.get("name"))
    description = _normalized_text(node.get("description"))
    prefix = node_id
    if name and name != node_id:
        prefix += f" ({name})"
    detail = description or "; ".join(_acceptance_conditions(node))
    return f"{prefix}: {detail}".rstrip(": ")


def _containment_ancestors(tree: dict, requirement_id: str) -> list[dict]:
    path: list[dict] = []

    def visit(node: dict, ancestors: list[dict]) -> bool:
        if _normalized_text(node.get("id")) == requirement_id:
            path.extend(ancestors)
            return True
        for child in node.get("children") or []:
            if isinstance(child, dict) and visit(child, [*ancestors, node]):
                return True
        return False

    visit(tree, [])
    return path


def _dependency_ids(
    node: dict,
    ordered_nodes: list[dict],
    folder_children: dict[str, list[str]],
) -> tuple[str, ...]:
    by_id = {_normalized_text(item.get("id")): item for item in ordered_nodes}
    pending: list[str] = []
    for dependency in node.get("dependencies") or []:
        dep_id = _normalized_text(dependency)
        pending.extend(folder_children.get(dep_id, [dep_id]))
    seen = set()
    while pending:
        dependency = pending.pop()
        if dependency in seen or dependency not in by_id:
            continue
        seen.add(dependency)
        for parent in by_id[dependency].get("dependencies") or []:
            parent_id = _normalized_text(parent)
            pending.extend(folder_children.get(parent_id, [parent_id]))
    return tuple(
        _normalized_text(item.get("id"))
        for item in ordered_nodes
        if _normalized_text(item.get("id")) in seen
    )


def build_task_contract(
    requirement_id: str,
    phase: str,
    requirement_file: Path,
    requirement_tree: dict,
    ordered_nodes: list[dict],
    folder_children: dict[str, list[str]] | None = None,
) -> TaskContract:
    requirement_id = _normalized_text(requirement_id)
    phase = _normalized_text(phase)
    if not requirement_id:
        raise TaskEvidenceError("requirement_id is required")
    if not phase:
        raise TaskEvidenceError("task evidence phase is required")
    try:
        requirement_file.read_bytes()
    except OSError as exc:
        raise TaskEvidenceError(
            f"cannot read requirement source {requirement_file}: {exc}"
        ) from exc

    by_id = {_normalized_text(node.get("id")): node for node in ordered_nodes}
    node = by_id.get(requirement_id)
    if node is None:
        raise TaskEvidenceError(f"unknown requirement_id: {requirement_id}")
    dependencies = _dependency_ids(node, ordered_nodes, folder_children or {})
    dependency_nodes = [by_id[dep] for dep in dependencies]
    containment = _containment_ancestors(requirement_tree, requirement_id)
    constraints = []
    for ancestor in [*containment, *dependency_nodes]:
        text = _constraint_text(ancestor)
        if text and text not in constraints:
            constraints.append(text)
    normalized_requirement = {
        "requirement": _node_semantics(node),
        "containment_ancestors": [_node_semantics(item) for item in containment],
        "dependency_requirements": [_node_semantics(item) for item in dependency_nodes],
    }
    return TaskContract(
        requirement_id=requirement_id,
        phase=phase,
        requirement_sha256=_sha256_json(normalized_requirement),
        requirement_ref=str(requirement_file),
        name=_normalized_text(node.get("name")),
        description=_normalized_text(node.get("description")),
        acceptance_conditions=_acceptance_conditions(node),
        dependencies=dependencies,
        ancestor_constraints=tuple(constraints),
    )


def build_suite_contract(
    requirement_ids: list[str],
    phase: str,
    requirement_file: Path,
    requirement_tree: dict,
    ordered_nodes: list[dict],
) -> TaskContract:
    phase = _normalized_text(phase)
    if not phase:
        raise TaskEvidenceError("task evidence phase is required")
    requested = {_normalized_text(item) for item in requirement_ids}
    selected = [
        node
        for node in ordered_nodes
        if _normalized_text(node.get("id")) in requested
    ]
    if not selected:
        raise TaskEvidenceError("suite task requires at least one known requirement")
    try:
        requirement_file.read_bytes()
    except OSError as exc:
        raise TaskEvidenceError(
            f"cannot read requirement source {requirement_file}: {exc}"
        ) from exc
    conditions = tuple(
        f"{_normalized_text(node.get('id'))} / {condition}"
        for node in selected
        for condition in _acceptance_conditions(node)
    )
    root_constraint = _constraint_text(requirement_tree)
    return TaskContract(
        requirement_id="ARC-FULL-SUITE",
        phase=phase,
        requirement_sha256=_sha256_json(
            {
                "requirements": [_node_semantics(node) for node in selected],
                "root": _node_semantics(requirement_tree),
            }
        ),
        requirement_ref=str(requirement_file),
        name="ARC full-suite verification",
        description="Verify the selected requirements together against one application.",
        acceptance_conditions=conditions,
        dependencies=tuple(_normalized_text(node.get("id")) for node in selected),
        ancestor_constraints=(root_constraint,) if root_constraint else (),
    )


def _source_files(output_dir: Path) -> dict[str, str]:
    files: dict[str, str] = {}
    for root_name in _SOURCE_ROOTS:
        root = output_dir / root_name
        if not root.is_dir():
            continue
        for directory, dirs, names in os.walk(root, followlinks=False):
            dirs[:] = sorted(
                name
                for name in dirs
                if name not in _IGNORED_SOURCE_PARTS
                and not (Path(directory) / name).is_symlink()
            )
            for name in sorted(names):
                path = Path(directory) / name
                if path.is_symlink() or not path.is_file():
                    continue
                relative = path.relative_to(output_dir).as_posix()
                try:
                    files[relative] = _sha256_bytes(path.read_bytes())
                except OSError as exc:
                    raise TaskEvidenceError(f"cannot read source file {path}: {exc}") from exc
    return files


def _source_state(
    output_dir: Path,
    baseline: dict[str, str],
    purpose: str,
) -> tuple[SourceState, dict[str, str]]:
    files = _source_files(output_dir)
    tree_sha256 = _sha256_json(
        [{"path": path, "sha256": digest} for path, digest in sorted(files.items())]
    )
    changed = tuple(
        ChangedFile(path=path, sha256=digest, purpose=purpose)
        for path, digest in sorted(files.items())
        if baseline.get(path) != digest
    )
    return SourceState(tree_sha256=tree_sha256, changed_files=changed), files


def _safe_command(specs: list[str]) -> str:
    safe = _safe_specs(specs)
    return "npx playwright test" + (" " + " ".join(safe) if safe else "")


def _safe_specs(specs: list[str]) -> list[str]:
    safe = []
    for raw in sorted(set(specs)):
        path = Path(raw)
        if path.is_absolute() or ".." in path.parts or not raw.endswith(".spec.ts"):
            continue
        safe.append(path.as_posix())
    return safe


def _failure_digest(outcome: TestOutcome) -> str:
    signature = failure_signature(RunSummary(results=[outcome]))
    payload = sorted(list(signature))
    return _sha256_json(payload)


def _expected_actual(message: str) -> tuple[str | None, str | None]:
    expected = _EXPECTED.search(message or "")
    actual = _ACTUAL.search(message or "")
    if not expected or not actual:
        return None, None
    return _bounded_text(expected.group(1), 500), _bounded_text(actual.group(1), 500)


def _fallback_run_id(summary: RunSummary, source_sha256: str, command: str) -> str:
    payload = {
        "source_sha256": source_sha256,
        "command": command,
        "passed": summary.passed,
        "total": summary.total,
        "error": summary.error,
        "failures": sorted(list(failure_signature(summary))),
    }
    return "acceptance-" + _sha256_json(payload).split(":", 1)[1][:12]


def _serialize_acceptance_evidence(
    summary: RunSummary,
    source_sha256: str,
    specs: list[str],
    run_id: str,
    command: str,
) -> bytes:
    summary_payload = asdict(summary)
    summary_payload["run_id"] = run_id
    summary_payload["command"] = command
    payload = {
        "schema": ACCEPTANCE_EVIDENCE_SCHEMA,
        "run_id": run_id,
        "source_sha256": source_sha256,
        "specs": _safe_specs(specs),
        "summary": summary_payload,
    }
    return (
        json.dumps(
            payload,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n"
    ).encode("utf-8")


def _acceptance_artifact_ref(
    output_dir: Path, run_id: str, artifact_data: bytes
) -> str:
    primary = (ACCEPTANCE_EVIDENCE_RELATIVE_DIR / f"{run_id}.json").as_posix()
    path = _artifact_path(output_dir, primary)
    try:
        existing = path.read_bytes()
    except FileNotFoundError:
        return primary
    except OSError as exc:
        raise TaskEvidenceError(
            f"cannot inspect acceptance artifact {primary}: {exc}"
        ) from exc
    if existing == artifact_data:
        return primary
    digest = hashlib.sha256(artifact_data).hexdigest()
    return (
        ACCEPTANCE_EVIDENCE_RELATIVE_DIR / f"{run_id}-{digest}.json"
    ).as_posix()


def _artifact_path(output_dir: Path, artifact_ref: str) -> Path:
    reference = Path(artifact_ref)
    if (
        not artifact_ref
        or "\\" in artifact_ref
        or reference.is_absolute()
        or reference.parts[:2] != ACCEPTANCE_EVIDENCE_RELATIVE_DIR.parts
        or len(reference.parts) != 3
        or any(part in ("", ".", "..") for part in reference.parts)
    ):
        raise TaskEvidenceError(
            f"invalid acceptance artifact reference: {artifact_ref!r}"
        )
    root = output_dir.resolve()
    path = (root / reference).resolve()
    try:
        path.relative_to(root)
    except ValueError as exc:
        raise TaskEvidenceError(
            f"acceptance artifact escapes output directory: {artifact_ref}"
        ) from exc
    return path


def _validate_artifact(
    output_dir: Path,
    artifact_ref: str,
    artifact_sha256: str,
    artifact_bytes: int,
) -> None:
    if (
        not isinstance(artifact_bytes, int)
        or isinstance(artifact_bytes, bool)
        or artifact_bytes < 0
    ):
        raise TaskEvidenceError("acceptance artifact byte count is invalid")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", artifact_sha256):
        raise TaskEvidenceError("acceptance artifact SHA-256 is invalid")
    path = _artifact_path(output_dir, artifact_ref)
    try:
        data = path.read_bytes()
    except FileNotFoundError as exc:
        raise TaskEvidenceError(
            f"referenced acceptance artifact is missing: {artifact_ref}"
        ) from exc
    except OSError as exc:
        raise TaskEvidenceError(
            f"cannot read acceptance artifact {artifact_ref}: {exc}"
        ) from exc
    if len(data) != artifact_bytes:
        raise TaskEvidenceError(
            f"acceptance artifact byte count mismatch: {artifact_ref}"
        )
    if _sha256_bytes(data) != artifact_sha256:
        raise TaskEvidenceError(f"acceptance artifact hash mismatch: {artifact_ref}")


def _validate_capsule_artifacts(
    output_dir: Path, capsule: TaskEvidenceCapsule
) -> None:
    validated = set()
    for failure in capsule.active_failures:
        metadata = (
            failure.artifact_ref,
            failure.artifact_sha256,
            failure.artifact_bytes,
        )
        if metadata == (None, None, None):
            continue
        if any(value is None for value in metadata):
            raise TaskEvidenceError("active failure artifact metadata is incomplete")
        if metadata in validated:
            continue
        _validate_artifact(output_dir, *metadata)
        validated.add(metadata)


def _capsule_from_dict(data: Any) -> TaskEvidenceCapsule:
    if not isinstance(data, dict) or data.get("schema") != TASK_EVIDENCE_SCHEMA:
        raise TaskEvidenceError("invalid task evidence schema")
    try:
        task = data["task"]
        source = data["source_state"]
        verification_data = data.get("verification")
        contract = TaskContract(
            requirement_id=str(task["requirement_id"]),
            phase=str(task["phase"]),
            requirement_sha256=str(task["requirement_sha256"]),
            requirement_ref=str(task["requirement_ref"]),
            name=str(task.get("name") or ""),
            description=str(task.get("description") or ""),
            acceptance_conditions=tuple(map(str, task.get("acceptance_conditions") or [])),
            dependencies=tuple(map(str, task.get("dependencies") or [])),
            ancestor_constraints=tuple(map(str, task.get("ancestor_constraints") or [])),
            policies=tuple(map(str, task.get("policies") or [])),
        )
        source_state = SourceState(
            tree_sha256=str(source["tree_sha256"]),
            changed_files=tuple(
                ChangedFile(
                    path=str(item["path"]),
                    sha256=str(item["sha256"]),
                    purpose=str(item["purpose"]),
                )
                for item in source.get("changed_files") or []
            ),
        )
        verification = (
            VerificationRun(
                run_id=str(verification_data["run_id"]),
                source_sha256=str(verification_data["source_sha256"]),
                command=str(verification_data.get("command") or ""),
                exit_code=verification_data.get("exit_code"),
                passed=int(verification_data["passed"]),
                total=int(verification_data["total"]),
            )
            if verification_data
            else None
        )
        active_failures = tuple(
            ActiveFailure(
                test_id=str(item["test_id"]),
                location=str(item.get("location") or ""),
                status=str(item["status"]),
                expected=item.get("expected"),
                actual=item.get("actual"),
                signature=str(item["signature"]),
                occurrences=int(item["occurrences"]),
                run_id=str(item["run_id"]),
                artifact_ref=(
                    str(item["artifact_ref"])
                    if item.get("artifact_ref") is not None
                    else None
                ),
                artifact_sha256=(
                    str(item["artifact_sha256"])
                    if item.get("artifact_sha256") is not None
                    else None
                ),
                artifact_bytes=(
                    int(item["artifact_bytes"])
                    if item.get("artifact_bytes") is not None
                    else None
                ),
            )
            for item in data.get("active_failures") or []
        )
        verified_behavior = tuple(
            VerifiedBehavior(
                test_id=str(item["test_id"]),
                run_id=str(item["run_id"]),
                source_sha256=str(item["source_sha256"]),
            )
            for item in data.get("verified_behavior") or []
        )
    except (KeyError, TypeError, ValueError) as exc:
        raise TaskEvidenceError(f"invalid task evidence payload: {exc}") from exc
    if not contract.requirement_id or not contract.phase or not source_state.tree_sha256:
        raise TaskEvidenceError("task evidence contains empty required fields")
    return TaskEvidenceCapsule(
        task=contract,
        source_state=source_state,
        verification=verification,
        active_failures=active_failures,
        verified_behavior=verified_behavior,
        next_action=str(data.get("next_action") or ""),
    )


class TaskEvidenceStore:
    def __init__(
        self,
        output_dir: Path,
        requirement_file: Path,
        requirement_tree: dict,
        ordered_nodes: list[dict],
        folder_children: dict[str, list[str]] | None = None,
    ) -> None:
        self.output_dir = Path(output_dir)
        self.requirement_file = Path(requirement_file)
        self.requirement_tree = requirement_tree
        self.ordered_nodes = list(ordered_nodes)
        self.folder_children = folder_children or {}
        self.path = self.output_dir / TASK_EVIDENCE_RELATIVE_PATH
        try:
            self.requirement_file.read_bytes()
        except OSError as exc:
            raise TaskEvidenceError(
                f"cannot read requirement source {self.requirement_file}: {exc}"
            ) from exc
        self.capsule: TaskEvidenceCapsule | None = None
        if self.path.exists():
            try:
                self.capsule = _capsule_from_dict(
                    json.loads(self.path.read_text(encoding="utf-8"))
                )
                _validate_capsule_artifacts(self.output_dir, self.capsule)
            except (OSError, UnicodeError, json.JSONDecodeError) as exc:
                raise TaskEvidenceError(
                    f"cannot load task evidence {self.path}: {exc}"
                ) from exc
        self._task_baseline_files = _source_files(self.output_dir)

    def activate(
        self,
        requirement_id: str,
        phase: str,
        next_action: str = "",
    ) -> TaskEvidenceCapsule:
        contract = build_task_contract(
            requirement_id,
            phase,
            self.requirement_file,
            self.requirement_tree,
            self.ordered_nodes,
            self.folder_children,
        )
        return self._activate_contract(contract, next_action)

    def activate_suite(
        self,
        requirement_ids: list[str],
        phase: str,
        next_action: str = "",
    ) -> TaskEvidenceCapsule:
        contract = build_suite_contract(
            requirement_ids,
            phase,
            self.requirement_file,
            self.requirement_tree,
            self.ordered_nodes,
        )
        return self._activate_contract(contract, next_action)

    def _activate_contract(
        self,
        contract: TaskContract,
        next_action: str,
    ) -> TaskEvidenceCapsule:
        same_task = (
            self.capsule is not None
            and self.capsule.task.requirement_id == contract.requirement_id
            and self.capsule.task.requirement_sha256 == contract.requirement_sha256
        )
        if not same_task:
            self._task_baseline_files = _source_files(self.output_dir)
        source, _ = _source_state(
            self.output_dir,
            self._task_baseline_files,
            f"{contract.phase} {contract.requirement_id}",
        )
        previous = self.capsule if same_task else None
        verification = previous.verification if previous else None
        verified = previous.verified_behavior if previous else ()
        if verification and verification.source_sha256 != source.tree_sha256:
            verification = None
            verified = ()
        candidate = TaskEvidenceCapsule(
            task=contract,
            source_state=source,
            verification=verification,
            active_failures=previous.active_failures if previous else (),
            verified_behavior=verified,
            next_action=_normalized_text(next_action)
            or (previous.next_action if previous else ""),
        )
        self._persist(candidate)
        return candidate

    def refresh_source(
        self,
        phase: str,
        next_action: str | None = None,
    ) -> TaskEvidenceCapsule:
        current = self._require_active()
        source, _ = _source_state(
            self.output_dir,
            self._task_baseline_files,
            f"{phase} {current.task.requirement_id}",
        )
        verification = current.verification
        verified = current.verified_behavior
        if verification and verification.source_sha256 != source.tree_sha256:
            verification = None
            verified = ()
        candidate = replace(
            current,
            task=replace(current.task, phase=_normalized_text(phase)),
            source_state=source,
            verification=verification,
            verified_behavior=verified,
            next_action=(
                current.next_action
                if next_action is None
                else _normalized_text(next_action)
            ),
        )
        self._persist(candidate)
        return candidate

    def record_verification(
        self,
        summary: RunSummary,
        specs: list[str],
        phase: str,
        next_action: str = "",
    ) -> TaskEvidenceCapsule:
        current = self._require_active()
        source, _ = _source_state(
            self.output_dir,
            self._task_baseline_files,
            f"{phase} {current.task.requirement_id}",
        )
        command = _safe_command(specs)
        run_id = _normalized_text(getattr(summary, "run_id", "")) or _fallback_run_id(
            summary, source.tree_sha256, command
        )
        if not _RUN_ID.fullmatch(run_id):
            run_id = _fallback_run_id(summary, source.tree_sha256, command)
        verification = VerificationRun(
            run_id=run_id,
            source_sha256=source.tree_sha256,
            command=command,
            exit_code=getattr(summary, "exit_code", None),
            passed=max(0, int(summary.passed)),
            total=max(0, int(summary.total)),
        )
        artifact_ref = None
        artifact_sha256 = None
        artifact_bytes = None
        if summary.error or any(not outcome.ok for outcome in summary.results):
            artifact_data = _serialize_acceptance_evidence(
                summary,
                source.tree_sha256,
                specs,
                run_id,
                command,
            )
            artifact_ref = _acceptance_artifact_ref(
                self.output_dir, run_id, artifact_data
            )
            artifact_sha256 = _sha256_bytes(artifact_data)
            artifact_bytes = len(artifact_data)
            artifact_path = _artifact_path(self.output_dir, artifact_ref)
            _write_atomic_bytes(
                artifact_path,
                artifact_data,
                "acceptance evidence artifact",
            )
            _validate_artifact(
                self.output_dir,
                artifact_ref,
                artifact_sha256,
                artifact_bytes,
            )
        previous = {item.signature: item for item in current.active_failures}
        failures_by_signature: dict[str, ActiveFailure] = {}
        for outcome in summary.results:
            if outcome.ok:
                continue
            expected, actual = _expected_actual(outcome.message)
            signature = _failure_digest(outcome)
            if signature in failures_by_signature:
                continue
            prior = previous.get(signature)
            occurrences = (
                prior.occurrences
                if prior and prior.run_id == run_id
                else (prior.occurrences + 1 if prior else 1)
            )
            failures_by_signature[signature] = ActiveFailure(
                test_id=_bounded_text(outcome.title, 500)
                or _bounded_text(outcome.file, 500)
                or "unknown-test",
                location=_bounded_text(outcome.location or outcome.file, 1000),
                status=_bounded_text(outcome.status, 100) or "failed",
                expected=expected,
                actual=actual,
                signature=signature,
                occurrences=occurrences,
                run_id=run_id,
                artifact_ref=artifact_ref,
                artifact_sha256=artifact_sha256,
                artifact_bytes=artifact_bytes,
            )
        if summary.error and not failures_by_signature:
            signature = _sha256_json(
                {
                    "status": "infrastructure_error",
                    "error": _normalized_text(summary.error),
                    "command": command,
                }
            )
            prior = previous.get(signature)
            occurrences = (
                prior.occurrences
                if prior and prior.run_id == run_id
                else (prior.occurrences + 1 if prior else 1)
            )
            failures_by_signature[signature] = ActiveFailure(
                test_id="acceptance-infrastructure",
                location="build/start/test runner",
                status="infrastructure_error",
                expected=None,
                actual=_bounded_text(summary.error, 500),
                signature=signature,
                occurrences=occurrences,
                run_id=run_id,
                artifact_ref=artifact_ref,
                artifact_sha256=artifact_sha256,
                artifact_bytes=artifact_bytes,
            )
        verified = tuple(
            VerifiedBehavior(
                test_id=_normalized_text(outcome.title)
                or _normalized_text(outcome.file)
                or "unknown-test",
                run_id=run_id,
                source_sha256=source.tree_sha256,
            )
            for outcome in sorted(
                (item for item in summary.results if item.ok),
                key=lambda item: (
                    _normalized_text(item.file),
                    _normalized_text(item.title),
                ),
            )
        )
        if not next_action:
            next_action = (
                "continue to the next requirement"
                if summary.total > 0 and summary.passed == summary.total
                else f"repair failing acceptance evidence for {current.task.requirement_id}"
            )
        candidate = TaskEvidenceCapsule(
            task=replace(current.task, phase=_normalized_text(phase)),
            source_state=source,
            verification=verification,
            active_failures=tuple(
                failures_by_signature[signature]
                for signature in sorted(failures_by_signature)
            ),
            verified_behavior=verified,
            next_action=_normalized_text(next_action),
        )
        self._persist(candidate)
        return candidate

    def _require_active(self) -> TaskEvidenceCapsule:
        if self.capsule is None:
            raise TaskEvidenceError("no active task evidence contract")
        return self.capsule

    def _persist(self, candidate: TaskEvidenceCapsule) -> None:
        _validate_capsule_artifacts(self.output_dir, candidate)
        write_capsule_atomic(self.path, candidate)
        self.capsule = candidate

import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from acceptance import RunSummary, TestOutcome
from task_evidence import (
    TaskEvidenceError,
    TaskEvidenceStore,
    TaskEvidenceWriteError,
    build_task_contract,
    serialize_capsule,
)


def requirement_tree():
    return {
        "id": "ROOT",
        "type": "FOLDER",
        "description": "Keep the application accessible.",
        "children": [
            {
                "id": "REQ-1",
                "type": "ATOMIC",
                "name": "Register",
                "description": "Create an account.",
                "dependencies": [],
                "scenarios": [
                    {
                        "name": "registration",
                        "steps": [
                            {"keyword": "GIVEN", "content": "a new visitor"},
                            {"keyword": "WHEN", "content": "they submit valid details"},
                            {"keyword": "THEN", "content": "the account is created"},
                        ],
                    }
                ],
            },
            {
                "id": "REQ-2",
                "type": "ATOMIC",
                "name": "Login",
                "description": "Authenticate an existing account.",
                "dependencies": ["REQ-1"],
                "scenarios": [
                    {
                        "name": "login",
                        "steps": [
                            {"keyword": "GIVEN", "content": "a registered account"},
                            {"keyword": "WHEN", "content": "valid credentials are submitted"},
                            {"keyword": "THEN", "content": "the dashboard is visible"},
                        ],
                    }
                ],
            },
        ],
    }


def create_store(root: Path, observer=None) -> TaskEvidenceStore:
    requirements = root / "requirements.yaml"
    requirements.write_text("id: ROOT\n", encoding="utf-8")
    app = root / "app"
    (app / "frontend").mkdir(parents=True)
    (app / "backend").mkdir()
    (app / "frontend" / "index.html").write_text("v1", encoding="utf-8")
    (app / "backend" / "server.js").write_text("server", encoding="utf-8")
    tree = requirement_tree()
    return TaskEvidenceStore(
        output_dir=app,
        requirement_file=requirements,
        requirement_tree=tree,
        ordered_nodes=tree["children"],
        folder_children={"ROOT": ["REQ-1", "REQ-2"]},
        observer=observer,
    )


def reopen_store(root: Path, observer=None) -> TaskEvidenceStore:
    tree = requirement_tree()
    return TaskEvidenceStore(
        output_dir=root / "app",
        requirement_file=root / "requirements.yaml",
        requirement_tree=tree,
        ordered_nodes=tree["children"],
        folder_children={"ROOT": ["REQ-1", "REQ-2"]},
        observer=observer,
    )


class TaskContractTests(unittest.TestCase):
    def test_should_serialize_same_requirement_source_and_summary_byte_stably(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = create_store(Path(tmp))
            capsule = store.activate("REQ-2", "implement")
            summary = RunSummary(
                passed=1,
                total=1,
                run_id="acceptance-0001",
                command="npx playwright test REQ-2.spec.ts",
                exit_code=0,
                results=[
                    TestOutcome(
                        "login works",
                        True,
                        "passed",
                        10,
                        file="REQ-2.spec.ts",
                    )
                ],
            )
            first = serialize_capsule(
                store.record_verification(summary, ["REQ-2.spec.ts"], "verify")
            )
            second = serialize_capsule(
                store.record_verification(summary, ["REQ-2.spec.ts"], "verify")
            )

            self.assertEqual(first, second)
            self.assertTrue(capsule.task.requirement_sha256.startswith("sha256:"))
            self.assertEqual(capsule.task.dependencies, ("REQ-1",))
            self.assertIn("ROOT: Keep the application accessible.", capsule.task.ancestor_constraints)
            self.assertIn(
                "login: GIVEN a registered account WHEN valid credentials are submitted THEN the dashboard is visible",
                capsule.task.acceptance_conditions,
            )

    def test_should_reject_missing_requirement_and_invalid_existing_schema(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaises(TaskEvidenceError):
                TaskEvidenceStore(
                    root / "app",
                    root / "missing.yaml",
                    requirement_tree(),
                    requirement_tree()["children"],
                )

            requirements = root / "requirements.yaml"
            requirements.write_text("id: ROOT\n", encoding="utf-8")
            evidence = root / "app" / ".arc" / "context"
            evidence.mkdir(parents=True)
            (evidence / "task-evidence.v1.json").write_text(
                '{"schema":"wrong"}\n', encoding="utf-8"
            )
            with self.assertRaises(TaskEvidenceError):
                TaskEvidenceStore(
                    root / "app",
                    requirements,
                    requirement_tree(),
                    requirement_tree()["children"],
                )

    def test_should_build_full_suite_contract_from_selected_requirements(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = create_store(Path(tmp))
            capsule = store.activate_suite(
                ["REQ-2", "REQ-1"], "full_suite", "verify all requirements"
            )
            self.assertEqual(capsule.task.requirement_id, "ARC-FULL-SUITE")
            self.assertEqual(capsule.task.dependencies, ("REQ-1", "REQ-2"))
            self.assertEqual(len(capsule.task.acceptance_conditions), 2)
            with self.assertRaises(TaskEvidenceError):
                store.activate("REQ-2", "")
            with self.assertRaises(TaskEvidenceError):
                store.activate_suite(["REQ-1"], "")

    def test_should_hash_normalized_requirement_semantics(self):
        with tempfile.TemporaryDirectory() as tmp:
            requirement_file = Path(tmp) / "requirements.yaml"
            requirement_file.write_text("id: ROOT\n", encoding="utf-8")
            tree = requirement_tree()
            first = build_task_contract(
                "REQ-2", "implement", requirement_file, tree, tree["children"]
            )
            tree["children"][1]["description"] += " "
            whitespace_only = build_task_contract(
                "REQ-2", "repair", requirement_file, tree, tree["children"]
            )
            tree["children"][1]["description"] = "Reject invalid credentials."
            changed = build_task_contract(
                "REQ-2", "implement", requirement_file, tree, tree["children"]
            )

            self.assertEqual(first.requirement_sha256, whitespace_only.requirement_sha256)
            self.assertNotEqual(first.requirement_sha256, changed.requirement_sha256)


class EvidenceReductionTests(unittest.TestCase):
    def test_should_deduplicate_same_failure_across_distinct_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = create_store(Path(tmp))
            store.activate("REQ-2", "implement")

            def failed(run_id, duration):
                return RunSummary(
                    passed=0,
                    total=1,
                    run_id=run_id,
                    command="npx playwright test REQ-2.spec.ts",
                    exit_code=1,
                    results=[
                        TestOutcome(
                            "login works",
                            False,
                            "timedOut",
                            duration,
                            file="REQ-2.spec.ts",
                            location="REQ-2.spec.ts:71",
                            message=(
                                f"Timeout {duration}ms exceeded.\n"
                                'Expected: "dashboard visible"\n'
                                'Received: "locator timed out"'
                            ),
                        )
                    ],
                )

            first = store.record_verification(
                failed("acceptance-0001", 4000),
                ["REQ-2.spec.ts"],
                "repair",
            )
            second = store.record_verification(
                failed("acceptance-0002", 4100),
                ["REQ-2.spec.ts"],
                "repair",
            )

            self.assertEqual(len(first.active_failures), 1)
            self.assertEqual(len(second.active_failures), 1)
            failure = second.active_failures[0]
            self.assertEqual(failure.occurrences, 2)
            self.assertEqual(failure.run_id, "acceptance-0002")
            self.assertEqual(failure.expected, '"dashboard visible"')
            self.assertEqual(failure.actual, '"locator timed out"')
            self.assertTrue(failure.signature.startswith("sha256:"))

    def test_should_deduplicate_one_run_and_leave_unparsed_fields_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = create_store(Path(tmp))
            store.activate("REQ-2", "implement")
            outcome = TestOutcome(
                "login works",
                False,
                "failed",
                10,
                file="REQ-2.spec.ts",
                message="dashboard missing without a structured assertion",
            )
            capsule = store.record_verification(
                RunSummary(
                    passed=0,
                    total=2,
                    command="npx playwright test REQ-2.spec.ts; printenv",
                    results=[outcome, outcome],
                ),
                ["REQ-2.spec.ts", "../secret.spec.ts"],
                "repair",
            )

            self.assertEqual(capsule.verification.command, "npx playwright test REQ-2.spec.ts")
            self.assertTrue(capsule.verification.run_id.startswith("acceptance-"))
            self.assertEqual(len(capsule.active_failures), 1)
            failure = capsule.active_failures[0]
            self.assertIsNone(failure.expected)
            self.assertIsNone(failure.actual)
            self.assertTrue(failure.artifact_ref.startswith(".arc/evidence/"))

    def test_should_replace_pass_with_regression_for_the_current_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = create_store(Path(tmp))
            store.activate("REQ-2", "implement")
            passed = RunSummary(
                passed=1,
                total=1,
                run_id="acceptance-0001",
                command="npx playwright test REQ-2.spec.ts",
                exit_code=0,
                results=[
                    TestOutcome("login works", True, "passed", 10, file="REQ-2.spec.ts")
                ],
            )
            capsule = store.record_verification(passed, ["REQ-2.spec.ts"], "verify")
            self.assertEqual(len(capsule.verified_behavior), 1)

            failed = RunSummary(
                passed=0,
                total=1,
                run_id="acceptance-0002",
                command="npx playwright test REQ-2.spec.ts",
                exit_code=1,
                results=[
                    TestOutcome(
                        "login works",
                        False,
                        "failed",
                        10,
                        file="REQ-2.spec.ts",
                        message="dashboard missing",
                    )
                ],
            )
            capsule = store.record_verification(failed, ["REQ-2.spec.ts"], "repair")
            self.assertEqual(capsule.verified_behavior, ())
            self.assertEqual([failure.test_id for failure in capsule.active_failures], ["login works"])

    def test_should_recompute_source_and_restore_the_best_verified_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = create_store(root)
            store.activate("REQ-2", "implement")
            source = root / "app" / "frontend" / "index.html"
            source.write_text("best", encoding="utf-8")
            passed = RunSummary(
                passed=1,
                total=1,
                run_id="acceptance-0007",
                command="npx playwright test REQ-2.spec.ts",
                exit_code=0,
                results=[
                    TestOutcome("login works", True, "passed", 10, file="REQ-2.spec.ts")
                ],
            )
            best = store.record_verification(passed, ["REQ-2.spec.ts"], "verify")
            best_source_sha = best.source_state.tree_sha256

            source.write_text("regressed", encoding="utf-8")
            changed = store.refresh_source("repair")
            self.assertNotEqual(changed.source_state.tree_sha256, best_source_sha)
            self.assertIsNone(changed.verification)
            self.assertEqual(changed.verified_behavior, ())

            source.write_text("best", encoding="utf-8")
            restored = store.record_verification(
                passed,
                ["REQ-2.spec.ts"],
                "verify",
                next_action="continue to the next requirement",
            )
            self.assertEqual(restored.source_state.tree_sha256, best_source_sha)
            self.assertEqual(restored.verification.run_id, "acceptance-0007")
            self.assertEqual(len(restored.verified_behavior), 1)

    def test_should_keep_previous_file_when_atomic_replace_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = create_store(Path(tmp))
            store.activate("REQ-2", "implement")
            previous = store.path.read_bytes()
            with patch("task_evidence.os.replace", side_effect=OSError("disk full")):
                with self.assertRaises(TaskEvidenceWriteError):
                    store.refresh_source("repair")
            self.assertEqual(store.path.read_bytes(), previous)
            self.assertEqual(list(store.path.parent.glob("*.tmp-*")), [])


class AcceptanceArtifactTests(unittest.TestCase):
    @staticmethod
    def failed_summary(message: str, *, two_failures: bool = False) -> RunSummary:
        results = [
            TestOutcome(
                "login works",
                False,
                "failed",
                10,
                file="REQ-2.spec.ts",
                location="REQ-2.spec.ts:71",
                message=message,
                steps=["open login", "submit credentials"],
                action_errors=["button was covered"],
            )
        ]
        if two_failures:
            results.append(
                TestOutcome(
                    "login rejects stale session",
                    False,
                    "timedOut",
                    4000,
                    file="REQ-2.spec.ts",
                    location="REQ-2.spec.ts:93",
                    message="session banner never appeared",
                )
            )
        return RunSummary(
            passed=0,
            total=len(results),
            results=results,
            stdout_tail="playwright terminal output",
            run_id="acceptance-0042",
            command="npx playwright test REQ-2.spec.ts",
            exit_code=1,
        )

    def test_should_persist_complete_failure_artifact_before_referencing_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = create_store(root)
            store.activate("REQ-2", "implement")

            capsule = store.record_verification(
                self.failed_summary("dashboard missing"),
                ["REQ-2.spec.ts"],
                "repair",
            )

            failure = capsule.active_failures[0]
            self.assertEqual(failure.artifact_ref, ".arc/evidence/acceptance-0042.json")
            artifact = store.output_dir / failure.artifact_ref
            data = artifact.read_bytes()
            self.assertEqual(failure.artifact_bytes, len(data))
            self.assertEqual(
                failure.artifact_sha256,
                "sha256:" + hashlib.sha256(data).hexdigest(),
            )
            payload = json.loads(data)
            self.assertEqual(payload["schema"], "octos.acceptance-evidence.v1")
            self.assertEqual(payload["summary"]["results"][0]["message"], "dashboard missing")
            self.assertEqual(payload["summary"]["stdout_tail"], "playwright terminal output")

    def test_should_reject_replay_when_referenced_artifact_is_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = create_store(root)
            store.activate("REQ-2", "implement")
            capsule = store.record_verification(
                self.failed_summary("dashboard missing"),
                ["REQ-2.spec.ts"],
                "repair",
            )
            (store.output_dir / capsule.active_failures[0].artifact_ref).unlink()

            with self.assertRaisesRegex(TaskEvidenceError, "artifact.*missing"):
                reopen_store(root)

    def test_should_reject_replay_when_artifact_hash_does_not_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = create_store(root)
            store.activate("REQ-2", "implement")
            capsule = store.record_verification(
                self.failed_summary("dashboard missing"),
                ["REQ-2.spec.ts"],
                "repair",
            )
            artifact = store.output_dir / capsule.active_failures[0].artifact_ref
            data = bytearray(artifact.read_bytes())
            data[-2] = ord(" ")
            artifact.write_bytes(data)

            with self.assertRaisesRegex(TaskEvidenceError, "hash mismatch"):
                reopen_store(root)

    def test_should_keep_long_utf8_logs_out_of_the_capsule(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = create_store(root)
            store.activate("REQ-2", "implement")
            marker = "数据库连接失败"
            message = marker * 20_000
            summary = self.failed_summary(message)
            summary.stdout_tail = "终端输出" * 10_000
            summary.results[0].action_errors = ["浏览器诊断" * 10_000]

            capsule = store.record_verification(
                summary,
                ["REQ-2.spec.ts"],
                "repair",
            )

            encoded_capsule = serialize_capsule(capsule)
            self.assertLess(len(encoded_capsule), 16 * 1024)
            self.assertNotIn(message.encode("utf-8"), encoded_capsule)
            artifact = store.output_dir / capsule.active_failures[0].artifact_ref
            artifact_text = artifact.read_text(encoding="utf-8")
            self.assertIn(message, artifact_text)
            self.assertIn(summary.stdout_tail, artifact_text)
            self.assertIn(summary.results[0].action_errors[0], artifact_text)

    def test_should_make_all_failures_in_one_run_share_one_artifact(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = create_store(root)
            store.activate("REQ-2", "implement")

            capsule = store.record_verification(
                self.failed_summary("dashboard missing", two_failures=True),
                ["REQ-2.spec.ts"],
                "repair",
            )

            self.assertEqual(len(capsule.active_failures), 2)
            self.assertEqual(
                {failure.artifact_ref for failure in capsule.active_failures},
                {".arc/evidence/acceptance-0042.json"},
            )
            self.assertEqual(
                len({failure.artifact_sha256 for failure in capsule.active_failures}),
                1,
            )

    def test_should_replay_valid_artifact_metadata_byte_stably(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = create_store(root)
            store.activate("REQ-2", "implement")
            capsule = store.record_verification(
                self.failed_summary("dashboard missing"),
                ["REQ-2.spec.ts"],
                "repair",
            )
            before = serialize_capsule(capsule)

            replayed = reopen_store(root)

            self.assertIsNotNone(replayed.capsule)
            self.assertEqual(serialize_capsule(replayed.capsule), before)
            self.assertEqual(
                replayed.capsule.active_failures[0].artifact_ref,
                ".arc/evidence/acceptance-0042.json",
            )

    def test_should_not_overwrite_an_older_artifact_when_run_id_is_reused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = create_store(root)
            store.activate("REQ-2", "implement")
            first = store.record_verification(
                self.failed_summary("first failure"),
                ["REQ-2.spec.ts"],
                "repair",
            )
            first_failure = first.active_failures[0]
            first_path = store.output_dir / first_failure.artifact_ref
            first_data = first_path.read_bytes()

            second = store.record_verification(
                self.failed_summary("different failure"),
                ["REQ-2.spec.ts"],
                "repair",
            )

            second_failure = second.active_failures[0]
            self.assertNotEqual(second_failure.artifact_ref, first_failure.artifact_ref)
            self.assertTrue(
                second_failure.artifact_ref.startswith(
                    ".arc/evidence/acceptance-0042-"
                )
            )
            self.assertEqual(first_path.read_bytes(), first_data)
            self.assertTrue((store.output_dir / second_failure.artifact_ref).is_file())

    def test_should_observe_artifact_write_and_replay_without_raw_content(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            observations = []
            observer = lambda kind, fields: observations.append((kind, fields))
            store = create_store(root, observer=observer)
            store.activate("REQ-2", "implement")
            capsule = store.record_verification(
                self.failed_summary("secret raw failure"),
                ["REQ-2.spec.ts"],
                "repair",
            )

            write = next(
                fields
                for kind, fields in observations
                if kind == "artifact" and fields["operation"] == "write"
            )
            self.assertEqual(write["status"], "stored")
            self.assertEqual(write["artifact_ref"], capsule.active_failures[0].artifact_ref)
            self.assertNotIn("secret raw failure", repr(observations))

            observations.clear()
            replayed = reopen_store(root, observer=observer)
            self.assertIsNotNone(replayed.capsule)
            replay = next(
                fields
                for kind, fields in observations
                if kind == "artifact" and fields["operation"] == "replay"
            )
            self.assertEqual(replay["status"], "found")
            self.assertNotIn("secret raw failure", repr(observations))


if __name__ == "__main__":
    unittest.main()

"""bundle 必须能带上 `public-tests/`，否则「随包 spec」这条兜底永远不可能命中。

`main.locate_acceptance_tests` 的第三个候选是 `bundle_dir/public-tests`，按需求树
根名匹配 `manifest.json`。但 `pack.py` 的 `ENTRIES` 里从来没有 `public-tests`，
所以平台不挂 `/workspace/tests` 的题目（官方 hackathon 就是）在容器里一条 spec
都找不到：`acceptance_loop` 对每个节点直接返回 None，修复回路全程禁用。

云端 hackathon--sheet `986f71047724` 就是这样跑的：0 条 `[acceptance]` 日志，
24/24 节点无判定，186 分钟盲写，32/100。
"""
import json
import unittest
from pathlib import Path

import pack

ARC_DIR = Path(pack.__file__).resolve().parent


class BundleContentTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.names = [name for _, name in pack.packaged_entries()]

    def test_should_package_public_tests_manifest(self):
        self.assertIn("public-tests/manifest.json", self.names)

    def test_should_package_specs_for_every_manifest_task(self):
        """manifest 里有条目却没有 spec，会让按根名匹配的兜底挑中一个空目录。"""
        manifest = json.loads((ARC_DIR / "public-tests" / "manifest.json").read_text(encoding="utf-8"))
        for task_id in manifest:
            specs = [n for n in self.names if n.startswith(f"public-tests/{task_id}/") and n.endswith(".spec.ts")]
            with self.subTest(task=task_id):
                self.assertTrue(specs, f"{task_id} is in the manifest but ships no spec")

    def test_should_not_package_pycache(self):
        self.assertFalse([n for n in self.names if "__pycache__" in n or n.endswith(".pyc")])


if __name__ == "__main__":
    unittest.main()

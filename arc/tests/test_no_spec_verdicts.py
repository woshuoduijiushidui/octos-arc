"""无验收 spec 时的判定必须是「未验证」，而不是「通过」。

云端 hackathon--sheet `986f71047724`：100 条官方 spec 一条都没挂进容器，
`acceptance_loop` 因 `not specs` 直接返回 None，24 个节点全部 `verdict=None`。
收尾时一句 LLM 终检 + 一次启动演练就把**全部 24 个节点**写成 `test_passed`
（`main.py` 的 final check 收尾块），平台上 `node_states` 因此显示 42/42 全绿，
而实际得分 32/100。

一次能构建能启动，不等于任何一条需求被满足。没有 spec 就没有关于需求的证据，
所以这里断言：无 spec 时既不写 `test_passed` 也不写 `test_failed`，节点停在
`IMPLEMENTED`——已实现、未验证。
"""
import unittest
from unittest.mock import Mock

import main


def _flow(tests_dir, spec_map=None):
    flow = object.__new__(main.Flow)
    flow.tests_dir = tests_dir
    # `spec_map[node]` is what decides whether this node has a spec of its own.
    flow.spec_map = spec_map if spec_map is not None else {None: []}
    return flow


NODE = "REQ-1-1-1"
COVERED = {"REQ-1-1-1": ["REQ-1-1-1.spec.ts"], None: []}


class FinalVerdictTests(unittest.TestCase):
    def test_should_record_no_verdict_when_no_acceptance_specs(self):
        """没有 spec：终检与演练都不是需求证据，不得因此判定通过。"""
        flow = _flow(None)
        self.assertIsNone(
            flow.final_verdict_for_undecided(NODE, rehearsed=True, final_ok=True))

    def test_should_not_flip_to_failed_when_no_acceptance_specs(self):
        """同样是「未验证」：没有 spec 时也不能把没跑过的节点判成失败。"""
        flow = _flow(None)
        self.assertIsNone(
            flow.final_verdict_for_undecided(NODE, rehearsed=False, final_ok=False))

    def test_should_not_pass_a_node_that_has_no_spec_of_its_own(self):
        """部分覆盖：套件整体有 spec，但这个节点没有。

        终检轮判的是整个应用，不是这条需求。平台 100 条用例按 scenario 分布，
        节点规格与用例数并不一一对应，这种部分覆盖正是 hackathon 的常态。"""
        flow = _flow(object(), spec_map={"REQ-1-1-1": ["REQ-1-1-1.spec.ts"], "REQ-9-9-9": [], None: []})
        self.assertIsNone(
            flow.final_verdict_for_undecided("REQ-9-9-9", rehearsed=True, final_ok=True))

    def test_should_keep_llm_verdict_when_the_node_has_a_spec(self):
        """有 spec 时行为不变：终检 + 演练仍作为未判定节点的兜底判据。"""
        flow = _flow(object(), spec_map=COVERED)
        self.assertTrue(flow.final_verdict_for_undecided(NODE, rehearsed=True, final_ok=True))
        self.assertFalse(flow.final_verdict_for_undecided(NODE, rehearsed=True, final_ok=False))
        self.assertFalse(flow.final_verdict_for_undecided(NODE, rehearsed=False, final_ok=True))


class FolderStateTests(unittest.TestCase):
    """FOLDER 节点的状态是子节点推导出来的，同样不能在无 spec 时凭空「通过」。"""

    def _foldered(self, tests_dir):
        flow = _flow(tests_dir)
        flow.folder_children = {"REQ-1": ["REQ-1-1-1", "REQ-1-1-2"]}
        flow.test_verdict = {"REQ-1-1-1": None, "REQ-1-1-2": None}
        flow.impl_failed = []
        flow.events = Mock()
        return flow

    def test_should_not_emit_test_state_for_folders_when_no_specs(self):
        flow = self._foldered(None)
        flow.mark_folders()
        flow.events.mark_test_passed.assert_not_called()
        flow.events.mark_test_failed.assert_not_called()

    def test_should_still_report_folders_implemented_when_no_specs(self):
        """不写测试状态，但「已设计 / 已实现」仍然是真的，必须照常上报。

        没有 spec 时不会再有 verdict，若仍等 verdict 才宣布 FOLDER 实现完成，
        它就会永远停在 IMPLEMENTING。"""
        flow = self._foldered(None)
        flow.mark_folders()
        flow.events.mark_design_done.assert_called_once()
        flow.events.mark_implementation_done.assert_called_once()

    def test_should_emit_test_state_for_folders_when_specs_exist(self):
        flow = self._foldered(object())
        flow.mark_folders()
        flow.events.mark_test_failed.assert_called_once()
        flow.events.mark_test_passed.assert_not_called()

    def test_should_report_passing_folders_when_specs_exist(self):
        flow = self._foldered(object())
        flow.test_verdict = {"REQ-1-1-1": True, "REQ-1-1-2": True}
        flow.mark_folders()
        flow.events.mark_test_passed.assert_called_once()
        flow.events.mark_test_failed.assert_not_called()


if __name__ == "__main__":
    unittest.main()

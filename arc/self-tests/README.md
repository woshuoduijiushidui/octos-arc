# arc/self-tests/ — 我们写给初赛两题的验收套件，**不参与判分**

这两个目录（`hackathon--sheet` 89 条 / `hackathon--github` 164 条）是我按 `arc/tasks/hackathon--*/requirements.yaml`
的 `description` 原文**自建**的 Playwright 套件。它们**故意**放在 `public-tests/` 之外：

- `public-tests/` 的语义是「平台发布的验收测试」（`arc/fetch-tests.py` 从
  `/api/requirements/<id>/tests?catalog=competition` 拉下来的就是那些，和 `grade-local.py` 用的是同一批）。
  这两个题平台不下发测试（official 赛事服务端就返回 404），套件是我们自己的，放在那个目录里会误导后来人。
- `pack.py` 只打包 `public-tests/`，所以放在这里 = **不会进提交包**，平台的生成过程回到「无 spec」的盲写基线，
  只保留 `main.py` 里那几处诚实化修复（无 spec 不写 `test_passed`，开局给醒目 WARNING）。

## 为什么摘掉判据权（2026-10-02）

云端 `101-C2-R23`（第一次带这套 spec 跑）用真实数字说明了问题：

| 同一份应用、同一次运行 | 我们的套件 | 平台 |
|---|---|---|
| 模块 | **23 / 24 满分**（1 个 2/3） | **1 / 24**（4.2%） |
| 用例 | 89 条基本全过 | **4 / 100** |

代价是确凿的：**90 轮修复、61.2M token、5.9 小时**，全部在追我们自己编的断言。

但**不能**说「spec 让分数变差了」。盲写四次的 sheet 分数是 `[0, 1, 1, 32]`（中位 1），带 spec 是 4 —— 落在盲写极差之内。
可以确定的是：这两个题**看不到平台的判据**，所以自建套件携带的信息量是零；拿零信息量的信号驱动修复，就是把预算花错地方。

## 这套东西的已知弱点（当初就记着，现在有了证据）

`helpers.ts` 的定位风格是照 `public-tests/arc-bench-web--keep/helpers.ts` 抄的 —— 但那套容错是**针对已知正确的应用**
写的。当裁判用就几乎不可反驳：`resolveNamed` 会依次试 11 种 role、大小写不敏感、空白正则化，再串 `or()` 兜底和 `.first()`，
控件名字或 role 错了照样能命中和判过。github 那 47 个里另有 **10 个带条件守卫**（`if (await count())` / `test.skip`），
它们连"控件不存在"都不算失败。涉及文件：`REQ-2-2-2 / 2-3 / 4-1 / 4-2-3 / 4-3-3 / 5-2-3 / 5-3-1 / 5-3-2 / 6-2-4 / 6-5`。

## 留着它们的用途

1. 需求文本的一份**可执行解读**，给人 review 用（比读 22KB description 快）。
2. 我们学到的两件事的证据：`importCsv` 死绑 `role=dialog` + `input[type=file]` 会把一个节点按在 0/4 上八轮；
   `openOrganization` 走 account menu 对未登录的 visitor 必然失败 —— 两者在
   `101-C2-R23` 的都修好了（`REQ-1-3-1` 4/4、`REQ-2-1-1` 4/4）。
3. 若将来平台公开这两题的测试，**「我们的断言 vs 他们的断言」的差集就是最便宜的纠错材料**。

## 想要这份信号但不想要它的代价

不要在 `acceptance_loop` 里跑它。若要跑，只作为**事后诊断**：跑、记日志、不进 `test_verdict`、不触发修复轮。
那需要在 `acceptance_loop` 加一条 advisory 路径 —— 注意那个循环对 `arc-bench-web` 等题是**真正的判据**
（那些 spec 就是评分器），所以任何改动都必须按题区分，不能全局削弱。

## 一条已被撤掉的连带改动

套件按 `role="gridcell"` 的 accessible name 定位单元格（`helpers.ts` 的 `cell()`）。为此我曾在
`prompts/ui-contract-data.md`（以及 `arc/main.py` 里那份同文常量）加过一条「用 `role="grid"`/`role="gridcell"`
暴露网格、单元用坐标作 accessible name」的约定。**这条已在 2026-10-02 连同套件的判据权一起撤除** —— 因为它是为了让
我们的断言能寻址而加给模型的指令，属于「我的解释在影响模型」。

后果要记清楚：现在应用**没有被告知**这个形状，单元格寻址只是我们的假设。若将来真要拿这套东西对着真实应用跑，
先决定要不要把那条约定加回去。

## 怎么手动跑

```sh
# 需要 local-grader 的 node_modules 才能解析 @playwright/test，所以拷进去跑
cd arc/local-grader && mkdir -p pw/tests && cp ../self-tests/hackathon--sheet/*.ts pw/tests/
printf "import { defineConfig } from '@playwright/test';\nexport default defineConfig({ testDir: './pw/tests', reporter: [['list']], use: { baseURL: 'http://127.0.0.1:43316' } });\n" > pw.config.ts
./node_modules/.bin/playwright test -c pw.config.ts --list      # 或去掉 --list 对着运行中的应用跑
```

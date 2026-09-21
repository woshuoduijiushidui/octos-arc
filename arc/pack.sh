#!/bin/sh
# 把 arc/ 打成 ARC 平台要的提交包（main.py 必须在 zip 根目录）
#
# 实现与包内容清单都在 pack.py：`zip` / `shasum` 在 Windows/MSYS 上不存在，
# 原来那两个依赖会让本机打不出包。这里只负责挑一个能用的解释器转发过去。
set -e
cd "$(dirname "$0")"
<<<<<<< New base: Merge branch 'octos-org:main' into main
if [ -n "$ROUTES" ]; then
    python3 -c 'import sys; from pathlib import Path; from llm_proxy import model_routes; model_routes(Path(sys.argv[1]).read_text())' "$ROUTES"
fi
rm -f ../octos-arc-bundle.zip
# public-tests/ is deliberately NOT shipped. The runner mounts the public specs at
# /workspace/tests, which locate_acceptance_tests() prefers anyway: across 13 completed
# cloud runs (smoke, smoke-evolution, ticket-booking, arc-bench-web, 2026-09-17) every
# single one logged "[tests] N spec files at /workspace/tests" and none ever reached the
# bundled copy. Shipping it would only put a per-task, task-title-keyed set of spec files
# in the submission -- dead weight in the cloud, and indistinguishable from pre-loading
# task data for anyone auditing the bundle. Local runs are unaffected: run-task-local.py
# resolves BUNDLE_DIR to arc/ and reads arc/public-tests straight from the repo.
zip -qr ../octos-arc-bundle.zip main.py rust_engine.py arc-policy.toml prompts octos_stdio.py requirement_order.py acceptance.py verify_app.py action_errors.cjs page_errors.ts guard.py llm_proxy.py codegen.py hooks requirements.txt arcbench_agent_runtime -x '*/__pycache__/*' '*.pyc'
if [ -n "$ROUTES" ]; then
    python3 -c 'import sys; from zipfile import ZipFile; z=ZipFile("../octos-arc-bundle.zip", "a"); z.write(sys.argv[1], "model-routes.json"); z.close()' "$ROUTES"
||||||| Common ancestor
if [ -n "$ROUTES" ]; then
    python3 -c 'import sys; from pathlib import Path; from llm_proxy import model_routes; model_routes(Path(sys.argv[1]).read_text())' "$ROUTES"
fi
rm -f ../octos-arc-bundle.zip
zip -qr ../octos-arc-bundle.zip main.py rust_engine.py arc-policy.toml prompts octos_stdio.py requirement_order.py acceptance.py verify_app.py action_errors.cjs page_errors.ts guard.py llm_proxy.py codegen.py hooks requirements.txt arcbench_agent_runtime public-tests -x '*/__pycache__/*' '*.pyc'
if [ -n "$ROUTES" ]; then
    python3 -c 'import sys; from zipfile import ZipFile; z=ZipFile("../octos-arc-bundle.zip", "a"); z.write(sys.argv[1], "model-routes.json"); z.close()' "$ROUTES"
=======

# `python3` 在 Windows 上可能是 Microsoft Store 的占位程序：存在但跑不了，
# 所以不能只看 `command -v`，得真跑一次。
if command -v python3 >/dev/null 2>&1 && python3 -c 'pass' >/dev/null 2>&1; then
    PY=python3
elif command -v python >/dev/null 2>&1 && python -c 'pass' >/dev/null 2>&1; then
    PY=python
else
    echo "python3 (or python) is required to pack the bundle" >&2
    exit 1
>>>>>>> Current commit: feat(pack): add ARC bundle packer and packaging rulesAdd arc/pack.py, a pure-Pyt
fi

exec "$PY" pack.py "$@"

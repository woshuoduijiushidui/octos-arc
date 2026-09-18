#!/bin/sh
# 把 arc/ 打成 ARC 平台要的提交包（main.py 必须在 zip 根目录）
#
# 实现与包内容清单都在 pack.py：`zip` / `shasum` 在 Windows/MSYS 上不存在，
# 原来那两个依赖会让本机打不出包。这里只负责挑一个能用的解释器转发过去。
set -e
cd "$(dirname "$0")"

# `python3` 在 Windows 上可能是 Microsoft Store 的占位程序：存在但跑不了，
# 所以不能只看 `command -v`，得真跑一次。
if command -v python3 >/dev/null 2>&1 && python3 -c 'pass' >/dev/null 2>&1; then
    PY=python3
elif command -v python >/dev/null 2>&1 && python -c 'pass' >/dev/null 2>&1; then
    PY=python
else
    echo "python3 (or python) is required to pack the bundle" >&2
    exit 1
fi

exec "$PY" pack.py "$@"

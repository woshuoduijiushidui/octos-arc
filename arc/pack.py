#!/usr/bin/env python3
"""把 arc/ 打成 ARC 平台要的提交包（main.py 必须在 zip 根目录）。

`pack.sh` 的等价实现，但只用标准库：Windows/MSYS 上没有 `zip` 和 `shasum`，
原来那两个依赖会让本机根本打不出包。包内容清单只在 `ENTRIES` 维护一份，
`pack.sh` 只是转发到这里，避免两个脚本各说各话。

Usage:
    python3 arc/pack.py [model-routes.json]

可选参数是部署策略（同题内按步骤分档选模型），不是生成的应用内容。给了就
先校验再作为 `model-routes.json` 放进包根；不给则包里不含任何模型规则。
"""

from __future__ import annotations

import hashlib
import sys
import zipfile
from pathlib import Path

ARC_DIR = Path(__file__).resolve().parent
OUTPUT = ARC_DIR.parent / "octos-arc-bundle.zip"

# 平台要的包内容。main.py 必须在根，其余带目录结构。
ENTRIES = (
    "main.py",
    "rust_engine.py",
    "arc-policy.toml",
    "prompts",
    "octos_stdio.py",
    "requirement_order.py",
    "acceptance.py",
    "verify_app.py",
    "action_errors.cjs",
    "page_errors.ts",
    "guard.py",
    "llm_proxy.py",
    "codegen.py",
    "hooks",
    "requirements.txt",
    "arcbench_agent_runtime",
    "public-tests",
)

# 目录条目也写进包（`zip -r` 的行为），与平台已接收过的包保持同一形状。
# 缓存不是包内容：平台会自己编译，带上只会变大且可能带进本机路径。
EXCLUDED_NAMES = frozenset({"__pycache__"})


def packaged_entries() -> list[tuple[Path, str]]:
    """(磁盘路径, 包内名) 列表，按 `ENTRIES` 顺序，目录内排序。"""
    entries: list[tuple[Path, str]] = []
    for name in ENTRIES:
        path = ARC_DIR / name
        if not path.exists():
            raise SystemExit(f"缺少打包内容：{path}")
        if path.is_file():
            entries.append((path, name))
            continue
        directories: list[Path] = [path]
        files: list[Path] = []
        for child in path.rglob("*"):
            relative = child.relative_to(ARC_DIR)
            if EXCLUDED_NAMES & set(relative.parts) or child.name.endswith(".pyc"):
                continue
            if child.is_dir():
                directories.append(child)
            else:
                files.append(child)
        entries.extend(
            (directory, directory.relative_to(ARC_DIR).as_posix() + "/")
            for directory in sorted(directories)
        )
        entries.extend(
            (file, file.relative_to(ARC_DIR).as_posix()) for file in sorted(files)
        )
    return entries


def main(argv: list[str]) -> int:
    routes = None
    if len(argv) > 1:
        sys.path.insert(0, str(ARC_DIR))
        from llm_proxy import model_routes

        routes = Path(argv[1]).resolve()
        # 坏规则不进包，也不动上一次打好的包。
        model_routes(routes.read_text(encoding="utf-8"))

    entries = packaged_entries()
    OUTPUT.unlink(missing_ok=True)
    with zipfile.ZipFile(OUTPUT, "w", zipfile.ZIP_DEFLATED) as archive:
        for path, name in entries:
            archive.write(path, name)
        if routes is not None:
            archive.write(routes, "model-routes.json")

    files = sum(1 for _, name in entries if not name.endswith("/"))
    print(f"bundled {OUTPUT} ({files} files)")
    print(f"sha256: {hashlib.sha256(OUTPUT.read_bytes()).hexdigest()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))

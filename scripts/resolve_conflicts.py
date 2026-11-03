#!/usr/bin/env python3
"""在 rebase 冲突暂停时，按 Fork 的固定规则解决冲突。

只处理两类冲突，其余一律报错交给人处理——自动同步宁可停下，也不能猜着合并。

1. `CHANGELOG.md`
   Fork 的条目必须始终排在所有上游条目之上（上游条目本身保持上游顺序）。
   做法：取上游（HEAD 侧）的正文，再把 `REBASE_HEAD`（正在重放的那个 Fork 提交）
   里的 Fork 条目整块插到它前面。

2. 版本号单行冲突（`version = "x.y.z"` / `"version": "x.y.z"`）
   取上游侧。真正的 Fork 版本号由调用方在 rebase 结束后统一重写一次，
   所以这里只要不把上游的版本号弄丢即可。

用法：python scripts/resolve_conflicts.py
退出码 0 = 全部解决；1 = 有需要人工处理的冲突（已把处理不了的文件列出来）。
"""

from __future__ import annotations

import re
import subprocess
import sys

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")
if hasattr(sys.stderr, "reconfigure"):
    sys.stderr.reconfigure(encoding="utf-8")

FORK_NOTICE = "<!-- FORK-NOTICE"
# 上游条目形如 `## [4.0.25] - 2026-11-02`；Fork 条目带 `+tailnet.N` 后缀，不会命中。
UPSTREAM_HEADING = re.compile(r"^## \[(\d+\.\d+\.\d+)\]", re.MULTILINE)
VERSION_LINE = re.compile(
    r'^\s*(?:version\s*=\s*"|"version"\s*:\s*")[^"]*"(?:\s*,)?\s*$'
)


def git(*args: str) -> str:
    # 输出一律按 UTF-8 解码：上游 CHANGELOG 是纯英文+中文混排，Windows 默认 GBK 会炸
    return subprocess.run(
        ["git", *args],
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    ).stdout


def split_changelog(text: str) -> tuple[str, str]:
    """拆成 (头部+上游正文, Fork 条目块)。没有 Fork 块时第二项为空串。"""
    notice = text.find(FORK_NOTICE)
    if notice == -1:
        return text, ""

    head = text[:notice]
    rest = text[notice:]

    # 第一个上游条目标题就是 Fork 块的结束位置
    match = UPSTREAM_HEADING.search(rest)
    if match is None:
        return head, rest

    return head + rest[match.start():], rest[: match.start()]


def resolve_changelog(path: str) -> None:
    upstream_text = git("show", "HEAD:" + path)
    try:
        incoming_text = git("show", "REBASE_HEAD:" + path)
    except subprocess.CalledProcessError:
        # 这一提交没有改 CHANGELOG，就用工作区里的 Fork 侧内容兜底
        incoming_text = open(path, encoding="utf-8").read()

    upstream_head, upstream_fork_block = split_changelog(upstream_text)
    _, incoming_fork_block = split_changelog(incoming_text)

    # 上游自己的 CHANGELOG 不该带 Fork 块；带上说明解析错了，直接报错
    if upstream_fork_block:
        raise RuntimeError("上游 CHANGELOG 里出现了 FORK-NOTICE，无法安全自动合并")

    body = upstream_head
    block = incoming_fork_block.strip()
    if block:
        # 把 Fork 块插在头部之后、第一条上游条目之前
        match = UPSTREAM_HEADING.search(body)
        if match is None:
            merged = body.rstrip("\n") + "\n\n" + block + "\n"
        else:
            merged = body[: match.start()].rstrip("\n") + "\n\n" + block + "\n\n" + body[match.start():]
    else:
        merged = body

    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(merged)


HUNK = re.compile(
    r"^<<<<<<< [^\n]*\n(.*?)^=======\n(.*?)^>>>>>>> [^\n]*\n",
    re.MULTILINE | re.DOTALL,
)


def resolve_version_hunks(path: str) -> bool:
    """只解决纯版本号冲突。返回 True 表示该文件已全部解决。"""
    text = open(path, encoding="utf-8").read()
    if "<<<<<<<" not in text:
        return True

    unresolved = False

    def repl(match: re.Match[str]) -> str:
        nonlocal unresolved
        ours, theirs = match.group(1), match.group(2)
        ours_lines = [l for l in ours.splitlines() if l.strip()]
        theirs_lines = [l for l in theirs.splitlines() if l.strip()]
        all_lines = ours_lines + theirs_lines
        if all_lines and all(VERSION_LINE.match(l) for l in all_lines):
            # 取上游侧（rebase 里 HEAD 就是新基线）
            return "\n".join(ours_lines) + "\n"
        unresolved = True
        return match.group(0)

    new_text = HUNK.sub(repl, text)
    if unresolved:
        return False

    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(new_text)
    return True


def resolve_all_conflicts() -> tuple[bool, list[str]]:
    """检测当前冲突文件并尝试自动解决。
    返回 (是否全部成功, 失败的文件列表)。
    """
    conflicted = [
        p
        for p in git("diff", "--name-only", "--diff-filter=U").splitlines()
        if p
    ]
    if not conflicted:
        return True, []

    failed: list[str] = []
    for path in conflicted:
        try:
            if path.endswith("CHANGELOG.md"):
                resolve_changelog(path)
                print(f"已合并  {path}（Fork 条目置于上游条目之上）")
            elif resolve_version_hunks(path):
                print(f"已取上游 {path}（版本号行）")
            else:
                failed.append(path)
        except Exception as exc:  # noqa: BLE001
            print(f"失败    {path}: {exc}")
            failed.append(path)

    return len(failed) == 0, failed


def main() -> int:
    success, failed = resolve_all_conflicts()
    if not success:
        print("\n以下文件需要人工处理：" + ", ".join(failed), file=sys.stderr)
        return 1
    print("全部冲突已按 Fork 规则自动解决。")
    return 0


if __name__ == "__main__":
    sys.exit(main())

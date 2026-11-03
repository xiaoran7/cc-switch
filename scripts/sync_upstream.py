#!/usr/bin/env python3
"""CC Switch 自动化日常上游同步脚本。

功能：
1. 检查工作区状态（必须干净）。
2. fetch 上游与 fork 远端。
3. 检查 upstream/main 是否有新提交。
4. 若有新提交：
   - 自动建立 backup/<branch>-pre-rebase-<timestamp> 本地备份分支。
   - 执行 git rebase upstream/main。
   - 遇冲突时自动调用 resolve_conflicts 逻辑解决 CHANGELOG 与版本号冲突并 continue。
   - 若遇到无法自动处理的代码冲突，自动 abort rebase 并回滚至备份，安全退出并告警。
   - 校验 src-tauri/tauri.conf.json 中 plugins.updater.endpoints 必须为空数组 []。
   - 对齐更新 Fork 版本号 (X.Y.Z+tailnet.N)。
   - 运行前端类型检查 (pnpm typecheck) 与 Rust 单元测试 (cargo test --lib)。
   - 将更新推送到 fork 远端 (git push --force-with-lease)。
5. 若无新提交：输出并正常退出。
"""

from __future__ import annotations

import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

# 导入同目录下的冲突解决器
SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_DIR = SCRIPT_DIR.parent
sys.path.insert(0, str(SCRIPT_DIR))

from resolve_conflicts import resolve_all_conflicts  # noqa: E402

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")
if hasattr(sys.stderr, "reconfigure"):
    sys.stderr.reconfigure(encoding="utf-8")


def run_cmd(
    args: list[str],
    cwd: Path | str | None = None,
    check: bool = True,
    capture: bool = True,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    merged_env = os.environ.copy()
    if env:
        merged_env.update(env)

    exec_args = list(args)
    use_shell = False
    if sys.platform == "win32":
        resolved = shutil.which(exec_args[0])
        if resolved:
            exec_args[0] = resolved
            if resolved.lower().endswith((".cmd", ".bat")):
                use_shell = True

    return subprocess.run(
        exec_args,
        cwd=cwd or PROJECT_DIR,
        check=check,
        capture_output=capture,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=merged_env,
        shell=use_shell,
    )


def git(*args: str, check: bool = True) -> str:
    res = run_cmd(["git", *args], check=check)
    return res.stdout.strip()


def is_worktree_clean() -> bool:
    # 检查暂存区与未暂存区是否有修改
    staged = run_cmd(["git", "diff", "--cached", "--quiet"], check=False).returncode == 0
    unstaged = run_cmd(["git", "diff", "--quiet"], check=False).returncode == 0
    return staged and unstaged


def get_current_branch() -> str:
    return git("rev-parse", "--abbrev-ref", "HEAD")


def check_is_rebasing() -> bool:
    git_dir = Path(git("rev-parse", "--git-dir"))
    return (git_dir / "rebase-merge").exists() or (git_dir / "rebase-apply").exists()


def enforce_disabled_updater() -> bool:
    """确保 tauri.conf.json 中的 updater.endpoints 为空数组 []。"""
    conf_path = PROJECT_DIR / "src-tauri" / "tauri.conf.json"
    with open(conf_path, "r", encoding="utf-8") as f:
        data = json.load(f)

    plugins = data.setdefault("plugins", {})
    updater = plugins.setdefault("updater", {})
    endpoints = updater.get("endpoints")

    if endpoints != []:
        updater["endpoints"] = []
        with open(conf_path, "w", encoding="utf-8", newline="\n") as f:
            json.dump(data, f, indent=2, ensure_ascii=False)
            f.write("\n")
        print("已修正 tauri.conf.json：清空 plugins.updater.endpoints 防止官方更新覆盖。")
        return True
    return False


def get_upstream_base_version(target_ref: str) -> str:
    """读取上游目标 ref 的 package.json 版本号。"""
    pkg_raw = git("show", f"{target_ref}:package.json")
    try:
        pkg = json.loads(pkg_raw)
        return str(pkg.get("version", "")).split("+")[0].strip()
    except Exception:
        # 兜底从 Cargo.toml 取
        cargo_raw = git("show", f"{target_ref}:src-tauri/Cargo.toml")
        m = re.search(r'^\s*version\s*=\s*"([^"]+)"', cargo_raw, re.MULTILINE)
        if m:
            return m.group(1).split("+")[0].strip()
    return ""


def get_current_fork_version() -> tuple[str, int]:
    """读取当前 package.json 版本号与 tailnet 构建序号。"""
    pkg_path = PROJECT_DIR / "package.json"
    with open(pkg_path, "r", encoding="utf-8") as f:
        data = json.load(f)
    ver = data.get("version", "")
    if "+tailnet." in ver:
        base, num_str = ver.split("+tailnet.")
        try:
            return base, int(num_str)
        except ValueError:
            return base, 1
    return ver.split("+")[0], 1


def update_fork_version(new_version: str, upstream_baseline: str) -> None:
    """同步修改 package.json, tauri.conf.json, Cargo.toml, FORK.md 中的版本号。"""
    # 1. package.json
    pkg_path = PROJECT_DIR / "package.json"
    with open(pkg_path, "r", encoding="utf-8") as f:
        pkg = json.load(f)
    pkg["version"] = new_version
    with open(pkg_path, "w", encoding="utf-8", newline="\n") as f:
        json.dump(pkg, f, indent=2, ensure_ascii=False)
        f.write("\n")

    # 2. tauri.conf.json
    tauri_path = PROJECT_DIR / "src-tauri" / "tauri.conf.json"
    with open(tauri_path, "r", encoding="utf-8") as f:
        conf = json.load(f)
    conf["version"] = new_version
    conf.setdefault("plugins", {}).setdefault("updater", {})["endpoints"] = []
    with open(tauri_path, "w", encoding="utf-8", newline="\n") as f:
        json.dump(conf, f, indent=2, ensure_ascii=False)
        f.write("\n")

    # 3. Cargo.toml
    cargo_path = PROJECT_DIR / "src-tauri" / "Cargo.toml"
    with open(cargo_path, "r", encoding="utf-8") as f:
        cargo_text = f.read()
    cargo_text = re.sub(
        r'^(\s*version\s*=\s*")[^"]*(")',
        rf"\g<1>{new_version}\g<2>",
        cargo_text,
        count=1,
        flags=re.MULTILINE,
    )
    with open(cargo_path, "w", encoding="utf-8", newline="\n") as f:
        f.write(cargo_text)

    # 4. Cargo.lock
    cargo_lock = PROJECT_DIR / "src-tauri" / "Cargo.lock"
    if cargo_lock.exists():
        with open(cargo_lock, "r", encoding="utf-8") as f:
            lock_text = f.read()
        lock_text = re.sub(
            r'(name\s*=\s*"cc-switch"\s*\n\s*version\s*=\s*")[^"]*(")',
            rf"\g<1>{new_version}\g<2>",
            lock_text,
            count=1,
        )
        with open(cargo_lock, "w", encoding="utf-8", newline="\n") as f:
            f.write(lock_text)

    # 5. FORK.md
    fork_md = PROJECT_DIR / "FORK.md"
    if fork_md.exists():
        with open(fork_md, "r", encoding="utf-8") as f:
            md_text = f.read()
        md_text = re.sub(
            r'(\|\s*上游版本基线\s*\|\s*`)[^`]*(`\s*\|)',
            rf"\g<1>v{upstream_baseline}\g<2>",
            md_text,
        )
        md_text = re.sub(
            r'(\|\s*本 Fork 版本号\s*\|\s*`)[^`]*(`\s*\|)',
            rf"\g<1>{new_version}\g<2>",
            md_text,
        )
        with open(fork_md, "w", encoding="utf-8", newline="\n") as f:
            f.write(md_text)

    print(f"已同步更新 Fork 版本号至 {new_version}（上游基线 v{upstream_baseline}）。")


def run_tests() -> bool:
    print("正在执行验证检查...")
    # 1. pnpm typecheck
    print("-> 执行 corepack pnpm typecheck")
    try:
        run_cmd(["corepack", "pnpm", "typecheck"], capture=False)
    except subprocess.CalledProcessError:
        print("❌ 前端 typecheck 失败！", file=sys.stderr)
        return False

    # 2. cargo test --lib
    print("-> 执行 cargo test --manifest-path src-tauri/Cargo.toml --lib")
    try:
        run_cmd(
            [
                "cargo",
                "test",
                "--manifest-path",
                "src-tauri/Cargo.toml",
                "--lib",
                "--",
                "--test-threads=8",
            ],
            capture=False,
            env={"CARGO_HTTP_PROXY": ""},
        )
    except subprocess.CalledProcessError:
        print("❌ Rust 单元测试失败！", file=sys.stderr)
        return False

    print("✅ 全部验证检查通过！")
    return True


def cleanup_build_artifacts() -> None:
    """收尾清理编译缓存，避免 target 目录膨胀占用数十 GB 空间。"""
    print("-> 正在收尾清理构建缓存 (cargo clean / dist)...")
    run_cmd(["cargo", "clean", "--manifest-path", "src-tauri/Cargo.toml"], check=False)
    dist_path = PROJECT_DIR / "dist"
    if dist_path.exists():
        shutil.rmtree(dist_path, ignore_errors=True)
    print("🧹 构建缓存与临时文件已清理完毕。")


def sync_upstream(
    target: str = "upstream/main",
    fork_remote: str = "fork",
    fork_branch: str = "feat/tailnet-https",
    upstream_remote: str = "upstream",
    do_push: bool = True,
    do_tests: bool = True,
    check_only: bool = False,
    do_clean: bool = True,
) -> int:
    print(f"=== CC Switch 上游同步开始 [{datetime.now().strftime('%Y-%m-%d %H:%M:%S')}] ===")

    # 1. 检查工作区干净度
    if not is_worktree_clean():
        print("❌ 工作区有未提交的改动，请先提交或 stash 后再运行同步。", file=sys.stderr)
        return 1

    # 2. 检查分支
    curr_branch = get_current_branch()
    if curr_branch != fork_branch:
        print(f"切换分支：{curr_branch} -> {fork_branch}")
        git("checkout", fork_branch)

    # 3. 拉取远端信息
    print(f"-> 拉取上游远端 {upstream_remote} 与 fork 远端 {fork_remote}...")
    run_cmd(["git", "fetch", upstream_remote, "--tags"], capture=False)
    run_cmd(["git", "fetch", fork_remote], capture=False)

    # 4. 判断是否需要同步
    target_hash = git("rev-parse", target)
    merge_base = git("merge-base", "HEAD", target)

    if merge_base == target_hash:
        print(f"✅ 当前分支已包含 {target} ({target_hash[:8]}) 的全部更新，无需同步。")
        return 0

    new_commits = git("rev-list", "--oneline", f"HEAD..{target}").splitlines()
    print(f"📢 发现上游 {target} 有 {len(new_commits)} 个新提交：")
    for c in new_commits[:5]:
        print(f"   {c}")
    if len(new_commits) > 5:
        print(f"   ... 共 {len(new_commits)} 个提交")

    if check_only:
        print("(--check-only 模式，退出)")
        return 0

    # 5. 创建安全备份分支
    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    backup_branch = f"backup/{fork_branch.replace('/', '-')}-pre-rebase-{timestamp}"
    git("branch", backup_branch, "HEAD")
    print(f"🛡️  已创建安全备份分支：{backup_branch}")

    # 6. 开始 rebase
    print(f"-> 开始执行 git rebase {target}...")
    rebase_proc = run_cmd(["git", "rebase", target], check=False)

    rebase_success = False
    if rebase_proc.returncode == 0:
        rebase_success = True
    else:
        # 进入冲突解决循环
        while check_is_rebasing():
            print("⚠️  Rebase 暂停，正在检查冲突...")
            resolved, failed = resolve_all_conflicts()
            if not resolved:
                print(f"❌ 遇到无法自动处理的冲突文件：{failed}", file=sys.stderr)
                print(f"-> 正在中止 rebase 并恢复至备份分支 {backup_branch}...")
                run_cmd(["git", "rebase", "--abort"], check=False)
                run_cmd(["git", "reset", "--hard", backup_branch], check=False)
                print("已安全还原现场，请人工排障。", file=sys.stderr)
                return 1

            # 标记已解决文件并 continue
            run_cmd(["git", "add", "-u"], check=True)
            cont_proc = run_cmd(
                ["git", "-c", "core.editor=true", "rebase", "--continue"],
                check=False,
            )
            if cont_proc.returncode == 0 and not check_is_rebasing():
                rebase_success = True
                break

    if not rebase_success:
        print("❌ Rebase 过程异常未完成，恢复备份...", file=sys.stderr)
        run_cmd(["git", "rebase", "--abort"], check=False)
        run_cmd(["git", "reset", "--hard", backup_branch], check=False)
        return 1

    print("🎉 Rebase 成功！")

    # 7. 检查与固化配置
    enforce_disabled_updater()

    # 8. 检查上游基线版本号是否变更
    upstream_ver = get_upstream_base_version(target)
    curr_base, curr_tailnet = get_current_fork_version()

    if upstream_ver and upstream_ver != curr_base:
        new_fork_ver = f"{upstream_ver}+tailnet.1"
        print(f"检测到上游基线版本变动：{curr_base} -> {upstream_ver}")
        update_fork_version(new_fork_ver, upstream_ver)
        run_cmd(["git", "commit", "-am", f"chore(release): rebase onto upstream v{upstream_ver} and bump fork to {new_fork_ver}"], check=False)
    else:
        # 版本未变，但若 tauri.conf.json 有修改，提交它
        if run_cmd(["git", "diff", "--quiet"], check=False).returncode != 0:
            run_cmd(["git", "commit", "-am", "fix(updater): keep official updater endpoints disabled"], check=False)

    # 9. 运行验证
    if do_tests:
        if not run_tests():
            print(f"❌ 测试失败，正在回滚至 {backup_branch}...", file=sys.stderr)
            run_cmd(["git", "reset", "--hard", backup_branch], check=False)
            return 1

    # 10. 推送到 fork
    if do_push:
        print(f"-> 正在推送到 {fork_remote}/{fork_branch}...")
        try:
            run_cmd(
                ["git", "push", "--force-with-lease", fork_remote, fork_branch],
                capture=False,
            )
            print("🚀 已成功推送到个人 fork 远端！")
        except subprocess.CalledProcessError as err:
            print(f"❌ 推送失败：{err}", file=sys.stderr)
            return 1

    # 11. 收尾清理缓存
    if do_clean:
        cleanup_build_artifacts()

    print(f"=== CC Switch 上游同步完成 [{datetime.now().strftime('%Y-%m-%d %H:%M:%S')}] ===")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="CC Switch upstream synchronization tool")
    parser.add_argument("--target", default="upstream/main", help="Target upstream ref (default: upstream/main)")
    parser.add_argument("--fork-remote", default="fork", help="Remote name for fork (default: fork)")
    parser.add_argument("--fork-branch", default="feat/tailnet-https", help="Branch name for fork (default: feat/tailnet-https)")
    parser.add_argument("--upstream-remote", default="upstream", help="Remote name for upstream (default: upstream)")
    parser.add_argument("--no-push", action="store_true", help="Do not push to fork")
    parser.add_argument("--no-tests", action="store_true", help="Skip running tests")
    parser.add_argument("--no-clean", action="store_true", help="Do not clean up build cache (target/dist)")
    parser.add_argument("--check-only", action="store_true", help="Only check for updates")

    args = parser.parse_args()

    return sync_upstream(
        target=args.target,
        fork_remote=args.fork_remote,
        fork_branch=args.fork_branch,
        upstream_remote=args.upstream_remote,
        do_push=not args.no_push,
        do_tests=not args.no_tests,
        check_only=args.check_only,
        do_clean=not args.no_clean,
    )


if __name__ == "__main__":
    sys.exit(main())

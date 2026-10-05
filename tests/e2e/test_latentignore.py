""".latentignore(gitignore 语法):AI 检索忽略的唯一配置入口——全局
~/.latent/.latentignore 在前、项目 <workdir>/.latentignore 在后拼接
(gitignore 语义 last-match-wins),项目可用 `!` 反选全局规则;@ 文件弹窗
候选随规则过滤。

夹具:测试自建 workdir 并自行清理;全局规则写进显式传入的隔离 HOME
(必须在 latent 启动前就位——配置启动时读一次,中途修改不生效)。
"""

import os
import shutil
import tempfile
import time

from harness import LatentApp


def make_workdir() -> str:
    workdir = tempfile.mkdtemp(prefix="latent_e2e_ignore_")
    os.makedirs(f"{workdir}/src")
    os.makedirs(f"{workdir}/node_modules")
    os.makedirs(f"{workdir}/secrets")
    with open(f"{workdir}/README.md", "w", encoding="utf-8") as f:
        f.write("hello from readme")
    with open(f"{workdir}/src/main.rs", "w", encoding="utf-8") as f:
        f.write("fn main() {}")
    with open(f"{workdir}/node_modules/x.js", "w", encoding="utf-8") as f:
        f.write("noise")
    with open(f"{workdir}/secrets/key.txt", "w", encoding="utf-8") as f:
        f.write("secret")
    return workdir


def popup_screen_after_at(app: LatentApp) -> str:
    """输入 @ 打开文件弹窗,返回渲染出的候选屏幕文本。"""
    app.type_text("@")
    app.expect_visible("README.md")
    app.expect_visible("src/")
    time.sleep(0.3)
    return app.visible_text()


def test_project_latentignore_filters_popup_candidates():
    workdir = make_workdir()
    with open(f"{workdir}/.latentignore", "w", encoding="utf-8") as f:
        f.write("# AI 忽略规则\nnode_modules/\nsecrets\n")
    app = LatentApp(turns=[], workdir=workdir)
    try:
        app.wait_ready()
        screen = popup_screen_after_at(app)
        # node_modules/ 与 secrets 被忽略,子项一并剪枝
        assert "node_modules" not in screen, screen
        assert "secrets" not in screen, screen
        assert "key.txt" not in screen, f"被忽略目录的子项不应出现: {screen}"
        # 未被忽略的条目照常在列(.latentignore 自身也是候选之一)
        assert "main.rs" in screen, screen
    finally:
        app.close()
        shutil.rmtree(workdir, ignore_errors=True)


def test_global_latentignore_then_project_negation_restores():
    workdir = make_workdir()
    os.makedirs(f"{workdir}/logs")
    with open(f"{workdir}/logs/run.log", "w", encoding="utf-8") as f:
        f.write("log")
    with open(f"{workdir}/logs/keep.log", "w", encoding="utf-8") as f:
        f.write("keep")
    # 全局在前:node_modules/ 与 *.log;项目在后:!keep.log 反选全局规则
    home = tempfile.mkdtemp(prefix="latent_e2e_ignore_home_")
    os.makedirs(f"{home}/.latent")
    with open(f"{home}/.latent/.latentignore", "w", encoding="utf-8") as f:
        f.write("node_modules/\n*.log\n")
    with open(f"{workdir}/.latentignore", "w", encoding="utf-8") as f:
        f.write("!keep.log\n")
    app = LatentApp(turns=[], workdir=workdir, home=home)
    try:
        app.wait_ready()
        screen = popup_screen_after_at(app)
        # 全局规则生效
        assert "node_modules" not in screen, screen
        assert "run.log" not in screen, screen
        # 项目 `!` 反选:keep.log 恢复可见
        assert "keep.log" in screen, f"项目反选应恢复 keep.log: {screen}"
    finally:
        app.close()
        shutil.rmtree(workdir, ignore_errors=True)
        shutil.rmtree(home, ignore_errors=True)

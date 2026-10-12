"""E2E:/init 命令——内置提示词作为 user 消息提交,模型分析代码库写 AGENTS.md。

验证:提交的请求体携带内置 init 提示词(目录索引/原地更新条款);附加参数
拼在提示词尾部;AGENTS.md 落在工作目录。
"""

import os

from harness import LatentApp, load_scenario

PROMPT_MARK = "improve it in place"


def test_init_writes_agents_md():
    app = LatentApp(turns=load_scenario("init"))
    try:
        app.wait_ready()
        app.sendline("/init")
        app.expect_text(r"新会话生效")

        # AGENTS.md 落在工作目录
        with open(os.path.join(app.workdir, "AGENTS.md"), encoding="utf-8") as f:
            content = f.read()
        assert "guidance to latent" in content

        # 反向断言:首个请求体携带内置提示词(架构/目录索引条款)
        bodies = app.wait_for_requests(2)
        first = str(bodies[0])
        assert "High-level architecture" in first
        assert "Directory index" in first
        assert PROMPT_MARK in first
    finally:
        app.close()


def test_init_appends_user_args():
    app = LatentApp(turns=load_scenario("init"))
    try:
        app.wait_ready()
        app.sendline("/init 重点是测试命令")
        app.expect_text(r"新会话生效")

        bodies = app.wait_for_requests(2)
        first = str(bodies[0])
        assert "重点是测试命令" in first, "附加要求应拼在提示词尾部"
        assert PROMPT_MARK in first
    finally:
        app.close()

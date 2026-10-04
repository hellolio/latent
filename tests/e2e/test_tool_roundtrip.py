"""E2E 场景 3:工具往返——LLM 请求调用工具,latent 执行后把结果发回第二轮。

这是 agent 软件最核心的闭环:mock 第一轮返回 tool_calls(read note.txt),
latent 在本地执行工具,第二轮请求里必须带上工具结果,最终回复上屏。
"""

import os

from harness import LatentApp, load_scenario

FILE_MARK = "HELLO FROM LATENT E2E"


def test_tool_roundtrip():
    app = LatentApp(turns=load_scenario("tool_roundtrip"))
    try:
        # 工作目录里放一个待读取的文件
        with open(os.path.join(app.workdir, "note.txt"), "w", encoding="utf-8") as f:
            f.write(FILE_MARK + "\n")

        app.wait_ready()
        app.sendline("请读取 note.txt 并告诉我内容")
        # 第二轮最终回复上屏
        app.expect_text(r"内容确认完毕")

        # mock 侧:第二轮请求必须带上工具执行结果(文件内容)
        bodies = app.wait_for_requests(2)
        second = str(bodies[1])
        assert FILE_MARK in second, "第二轮请求应包含工具执行结果(文件内容)"
    finally:
        app.close()

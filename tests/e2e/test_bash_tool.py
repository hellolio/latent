"""T16:bash 工具——stdout 上屏、退出码;超长输出截断显示行范围提示。"""

from harness import RpiApp, load_scenario


def test_bash_output_and_truncation_hint():
    app = RpiApp(turns=load_scenario("bash_tool"))
    try:
        app.wait_ready()
        app.sendline("跑个命令")
        app.expect_text("bash-e2e-ok")
        app.expect_text("bash 执行完毕")
        bodies = app.wait_for_requests(2)
        assert "bash-e2e-ok" in str(bodies[1]), "bash 输出应进入第二轮请求"
    finally:
        app.close()


def test_bash_long_output_truncated_with_range_hint():
    import json
    app = RpiApp(turns=load_scenario("bash_truncate"), timeout=20.0)
    try:
        app.wait_ready()
        app.sendline("输出 5000 行")
        # 卡片折叠:提示可展开
        app.expect_text(r"ctrl\+o to expand")
        app.expect_text("长输出已截断展示")
        # 行数截断提示随 toolResult 发给 LLM(保末尾 2000 行)
        bodies = app.wait_for_requests(2)
        assert "Showing lines" in json.dumps(bodies[1], ensure_ascii=False), \
            "截断续读提示应进入 toolResult"
    finally:
        app.close()

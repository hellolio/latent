"""T12:Esc 中断后同一会话继续对话(test_abort 的补集)。"""

from harness import LatentApp, load_scenario


def test_conversation_continues_after_abort():
    app = LatentApp(turns=load_scenario("abort_continue"), timeout=20.0)
    try:
        app.wait_ready()
        app.sendline("开始一个很慢的回复")
        app.wait_for_requests(1, timeout=10.0)
        app.send_key("esc")
        app.expect_text("aborted", timeout=10.0)

        # 中断后继续对话:正常走完一轮
        app.sendline("换一个问题")
        app.expect_text("中断后的新回复")
        bodies = app.wait_for_requests(2)
        assert "换一个问题" in str(bodies[1]), "中断后的新请求应带新问题"
    finally:
        app.close()

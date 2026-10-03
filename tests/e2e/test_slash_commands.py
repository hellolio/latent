"""T9:斜杠命令全集——/help /session /thinking /model 未知命令本地处理、/quit 退出。

本地命令不消耗场景 turn、不产生 LLM 请求(零请求反向断言)。
"""

import time

import pexpect

from harness import RpiApp


def test_local_slash_commands_without_llm_requests():
    app = RpiApp(turns=[])
    try:
        app.wait_ready()
        # /help:命令表
        app.sendline("/help")
        app.expect_text("命令:")
        app.expect_text("/quit")
        # /session info:会话信息(id/file);裸 /session 打开历史会话选择器
        app.sendline("/session info")
        app.expect_text("id:")
        app.expect_text("file:")
        # /thinking high:footer 级别指示更新
        app.sendline("/thinking high")
        app.expect_text("thinking:high")
        # /model 带参数:直接切换(成功路径无确认消息;同一模型切回不报错)
        app.sendline("/model e2e/e2e-model")
        time.sleep(0.5)
        assert "切换失败" not in app.transcript()
        # 未知命令:本地警告,不发给模型
        app.sendline("/nope")
        app.expect_text("Unknown command: /nope")
        time.sleep(0.6)
        assert len(app.request_bodies()) == 0, "本地斜杠命令不应产生 LLM 请求"
    finally:
        app.close()


def test_slash_quit_exits_cleanly():
    app = RpiApp(turns=[])
    try:
        app.wait_ready()
        app.sendline("/quit")
        app.child.expect(pexpect.EOF, timeout=5)
        app.child.wait()
        assert app.exit_status == 0, f"/quit 退出码应为 0,实际 {app.exit_status}"
    finally:
        app.close()

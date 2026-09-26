"""E2E 场景 6:Ctrl+C 语义。

1. 流式回复进行中按 Ctrl+C,应中断并显示 aborted;
2. 空闲时按一次 Ctrl+C,应提示再按一次退出;500ms 内再按,进程干净退出。
"""

import time

import pexpect

from harness import RpiApp, load_scenario


def test_ctrl_c_aborts_streaming_reply():
    # 两个 turn:第二个验证中断后还能正常继续对话
    turns = load_scenario("abort") + [{"text": "在的"}]
    app = RpiApp(turns=turns, timeout=20.0)
    try:
        app.wait_ready()
        app.sendline("开始一个很慢的回复")
        # 确认请求已发出(mock 已收到)、回复还在进行中(delay 8s)
        app.wait_for_requests(1, timeout=10.0)
        time.sleep(0.3)
        app.send_key("ctrl+c")
        # 中断提示上屏
        app.expect_text(r"aborted", timeout=10.0)
        time.sleep(1.0)
        assert len(app.request_bodies()) == 1, "中断后不应再发起新的 LLM 请求"
        # 中断后回到可输入状态:再次提问仍可用,并拿到第二轮回复
        app.sendline("还在吗")
        app.wait_for_requests(2, timeout=10.0)
        app.expect_text(r"在的", timeout=10.0)
    finally:
        app.close()


def test_double_ctrl_c_exits_at_idle():
    app = RpiApp(turns=[], timeout=10.0)
    try:
        app.wait_ready()
        # 第一次:提示再按一次退出(不退出)
        app.send_key("ctrl+c")
        app.expect_text(r"press Ctrl\+C again to exit")
        assert app.child.isalive()
        # 500ms 窗口内第二次:进程干净退出
        app.send_key("ctrl+c")
        app.child.expect(pexpect.EOF, timeout=5.0)
        app.child.wait()
        assert app.exit_status == 0, f"退出码应为 0,实际 {app.exit_status}"
    finally:
        app.close()


def test_double_ctrl_c_outside_window_does_not_exit():
    app = RpiApp(turns=[], timeout=10.0)
    try:
        app.wait_ready()
        # 两次 Ctrl+C 间隔超过 500ms:只是提示,不退出
        app.send_key("ctrl+c")
        app.expect_text(r"press Ctrl\+C again to exit")
        time.sleep(0.8)
        app.send_key("ctrl+c")
        app.expect_text(r"press Ctrl\+C again to exit")
        time.sleep(0.5)
        assert app.child.isalive(), "超过双击窗口后不应退出"
    finally:
        app.close()

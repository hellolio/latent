"""E2E 场景 4:流式回复进行中按 Esc,应中断并在屏幕上显示 aborted。"""

import time

from harness import RpiApp, load_scenario


def test_esc_aborts_streaming_reply():
    app = RpiApp(turns=load_scenario("abort"), timeout=20.0)
    try:
        app.wait_ready()
        app.sendline("开始一个很慢的回复")
        # 确认请求已发出(mock 已收到)、回复还在进行中(delay 8s)
        app.wait_for_requests(1, timeout=10.0)
        time.sleep(0.3)
        app.send_key("esc")
        # 中断提示上屏
        app.expect_text(r"aborted", timeout=10.0)
        # mock 不应收到第二轮请求
        time.sleep(1.0)
        assert len(app.request_bodies()) == 1, "中断后不应再发起新的 LLM 请求"
    finally:
        app.close()

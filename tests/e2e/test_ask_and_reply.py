"""E2E 场景 2:核心用户流程——输入问题、收到流式回复、回到可输入状态。

同时反向断言 mock LLM 收到的请求(rpi 发出的 payload 带上了用户消息)。
"""

import pytest

from harness import RpiApp, load_scenario


def test_ask_and_reply_end_to_end():
    app = RpiApp(turns=load_scenario("ask_and_reply"))
    try:
        app.wait_ready()
        app.sendline("你好,请介绍一下你自己")
        # mock 的固定回复流式上屏
        app.expect_text(r"这是 mock LLM 的固定回复")
        # 回复结束后回到可输入状态(编辑器重新出现,无 busy 状态)
        app.expect_text(r"你好,请介绍一下你自己")

        # mock 侧:收到的请求里应带系统提示词与用户消息
        bodies = app.wait_for_requests(1)
        assert len(bodies) == 1
        payload = bodies[0]
        payload_text = str(payload)
        assert "你好,请介绍一下你自己" in payload_text, "用户消息应随请求发出"
    finally:
        app.close()

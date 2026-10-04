"""T10:steering——run 进行中输入自动转为 steering 注入。

反向断言第二轮请求体同时包含首问与注入消息(注入发生在首条回复之后)。
"""

import json
import time

from harness import LatentApp, load_scenario


def test_input_during_run_becomes_steering():
    app = LatentApp(turns=load_scenario("steering"), timeout=20.0)
    try:
        app.wait_ready()
        app.sendline("第一个问题")
        # 首个回复还在 delay 中(mock 3s),此时输入应被路由为 steering
        time.sleep(0.5)
        app.sendline("中途补充")
        app.expect_text("第一段回复")
        app.expect_text("第二段回复")

        bodies = app.wait_for_requests(2)
        second = json.dumps(bodies[1], ensure_ascii=False)
        assert "第一个问题" in second, "第二轮应包含首问"
        assert "中途补充" in second, "第二轮应包含 steering 注入消息"
        # 注入消息必须在首条回复之后(先回复,后注入)
        assert second.index("第一段回复") < second.index("中途补充")
    finally:
        app.close()

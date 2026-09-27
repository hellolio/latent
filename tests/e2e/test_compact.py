"""T19:/compact——摘要请求发出、压缩完成后下一轮请求携带摘要;带参数出警告。"""

import time

from harness import RpiApp, load_scenario


def test_compact_short_conversation_no_empty_summarizer_request():
    """短会话 /compact:无内容可摘要,不得发出空对话的摘要请求(回归钉)。

    此前 bug:切点落在首个 user 消息,待摘要范围只有 system 元数据,
    序列化为空 → LLM 在看不到对话的情况下编造"摘要"入库。
    """
    app = RpiApp(turns=load_scenario("compact"), timeout=20.0)
    try:
        app.wait_ready()
        app.sendline("第一问")
        app.expect_text("第一轮的回复")

        app.sendline("/compact")
        app.expect_text("compacted")
        time.sleep(0.8)

        # 只有首轮一个请求:没有空对话的摘要请求
        bodies = app.request_bodies()
        assert len(bodies) == 1, f"短会话 /compact 不应产生摘要请求,实际 {len(bodies)} 个"

        # 压缩(无操作)后仍可继续对话
        app.sendline("压缩后还能聊吗")
        app.expect_text("压缩后仍可对话")
        bodies = app.wait_for_requests(2)
        assert "压缩后还能聊吗" in str(bodies[1])
    finally:
        app.close()


def test_compact_with_arg_warns_but_still_compacts():
    app = RpiApp(turns=load_scenario("compact"), timeout=20.0)
    try:
        app.wait_ready()
        app.sendline("第一问")
        app.expect_text("第一轮的回复")
        app.sendline("/compact 保留近期消息")
        # 明确警告:参数不生效
        app.expect_text("自定义压缩指令暂不支持")
        app.expect_text("compacted")
    finally:
        app.close()

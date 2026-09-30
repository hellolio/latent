"""sanitize 参数:bash 输出默认剥离 ANSI 码进转录,sanitize:false 保留原文。

断言面向 mock LLM 服务端收到的**真实请求体**(12 文档 L3 反向断言):
转录里的 toolResult 文本第一轮无 ESC 码、第二轮带 ESC 码,即证明
净化发生在"进对话历史"这一层,而非仅是显示。
"""

from harness import RpiApp, load_scenario


def _tool_result_texts(body) -> list[str]:
    """从 anthropic-messages 请求体抽取全部 tool_result 的文本内容。"""
    texts: list[str] = []
    for message in body.get("messages", []):
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict) or block.get("type") != "tool_result":
                continue
            inner = block.get("content")
            items = inner if isinstance(inner, list) else [{"type": "text", "text": inner}]
            for item in items:
                if isinstance(item, dict) and item.get("type") == "text":
                    texts.append(item.get("text", ""))
    return texts


def test_bash_output_sanitized_by_default_and_raw_with_sanitize_false():
    app = RpiApp(turns=load_scenario("bash_sanitize"), timeout=20.0)
    try:
        app.wait_ready()
        app.sendline("跑两条命令")
        app.expect_text("RED-ok")
        app.expect_text("sanitize e2e 完成")

        bodies = app.wait_for_requests(3)
        # 第 2 个请求体:只有第 1 轮的 toolResult(默认剥离 ANSI)
        sanitized = "\n".join(_tool_result_texts(bodies[1]))
        assert "RED-ok" in sanitized, f"输出文本应进转录: {sanitized!r}"
        assert "\x1b" not in sanitized, f"默认应剥离 ANSI 码: {sanitized!r}"

        # 第 3 个请求体携带全量转录(两条 toolResult):
        # 第 1 条保持净化原文;第 2 条(sanitize:false)保留 ANSI 码
        texts = _tool_result_texts(bodies[2])
        first = next(t for t in texts if "RED-ok" in t)
        second = next(t for t in texts if "GREEN" in t)
        assert "\x1b" not in first, f"第 1 轮应保持净化: {first!r}"
        assert "\x1b[32m" in second, f"sanitize:false 应保留 ANSI 码: {second!r}"
    finally:
        app.close()

"""净化路径对齐 pi:模型工具结果字节级保真(不剥 ANSI),`!` 裸命令输出净化。

- bash 工具结果:请求体 toolResult 里保留 ANSI 原文(字节级保真,pi 的
  core/tools/bash.ts 行为);
- `!` 裸命令:输出经执行器净化(stripAnsi → 控制字符 → 去 \\r)后注入转录,
  请求体 <bash_execution> 内无 ESC 码(pi 的 bash-executor.ts:82 行为)。

断言面向 mock LLM 服务端收到的真实请求体(12 文档 L3 反向断言)。
"""

from harness import LatentApp, load_scenario


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


def test_tool_result_raw_and_bang_output_sanitized():
    app = LatentApp(turns=load_scenario("bash_sanitize"), timeout=20.0)
    try:
        app.wait_ready()
        # 第 1 轮:模型调用 bash,输出带 ANSI 码(屏幕上 pyte 会把 ESC 当
        # 颜色控制消费,可见文本形如 "[31mRED[0m-ok" —— 保真的直接体现)
        app.sendline("跑一条命令")
        app.expect_text("第一轮完成")

        # 工具结果字节级保真:请求体 toolResult 保留 ANSI 原文
        bodies = app.wait_for_requests(2)
        tool_texts = "\n".join(_tool_result_texts(bodies[1]))
        assert "\x1b[31m" in tool_texts, f"工具结果应保留 ANSI 原文: {tool_texts!r}"
        assert "-ok" in tool_texts, f"输出文本应进转录: {tool_texts!r}"

        # 第 2 轮:`!` 裸命令(输出含 ANSI 码),随后一个 prompt 触发第 3 个请求
        app.sendline("!printf '\\033[31mBANG\\033[0m-marker'")
        app.expect_text("BANG-marker")
        app.sendline("看看输出")
        app.expect_text("第二轮完成")

        bodies = app.wait_for_requests(3)
        body = str(bodies[2])
        assert "<bash_execution" in body, "BashExecution 消息应进入转录"
        assert "BANG-marker" in body, f"! 输出应注入上下文: {body[:500]}"
        assert "\x1b" not in body, f"! 路径的上下文不应含 ESC 码: {body[:500]}"
    finally:
        app.close()

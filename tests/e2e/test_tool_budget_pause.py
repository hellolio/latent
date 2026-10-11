"""E2E 场景:Plan 模式工具调用预算——软上限收敛提示 + 超限暂停等用户确认。

maxToolCalls = 4:第 3 轮(3/4 上限)后注入模型可见的收敛提示(上屏不可见);
第 5 轮(> 4)执行完后 run 暂停,屏幕出现暗色「是否继续」提示且不再发请求;
用户回复任意消息开启新 run——held 工具结果留在转录并紧邻新用户消息之前,
模型恢复文本作答,预算重新计满。
"""

import json

from harness import LatentApp, load_scenario


def _msg_text(message: dict) -> str:
    """anthropic wire 格式的 user 消息文本(content 为字符串或块数组)。"""
    content = message["content"]
    if isinstance(content, str):
        return content
    return " ".join(block.get("text", "") for block in content)


def test_tool_budget_pause_and_resume():
    app = LatentApp(
        turns=load_scenario("tool_budget_pause"),
        settings={"maxToolCalls": 4},
        session_mode=None,  # 新会话默认 Plan:护栏仅在 Plan 模式生效
    )
    try:
        app.wait_ready()
        app.sendline("连续调用工具,不要停")
        # 5 个工具轮(每轮一次请求):第 5 轮执行完后暂停,不再有第 6 次请求
        bodies = app.wait_for_requests(5)
        # 软上限(3/4 = 3 轮)后:第 4 次请求应携带模型可见的收敛提示
        assert (
            "consecutive tool rounds" in str(bodies[3])
        ), "达到 3/4 上限后的请求应携带收敛提示"

        # 暂停提示上屏(暗色行):模型不可见,等用户确认
        app.expect_text(r"工具调用已经超过最大次数\(4 轮\),是否继续\?回复任意消息将继续。")

        # 用户回复任意消息 → 新 run:排队的第 6 个 turn 被消费
        app.sendline("继续")
        bodies = app.wait_for_requests(6)
        app.expect_text(r"已恢复,汇报结论。")

        # 恢复请求:held 工具结果(round-5 输出)紧邻新用户消息之前
        messages = json.loads(json.dumps(bodies[5]))["messages"]
        assert messages[-1]["role"] == "user", "恢复请求最后一条应为用户新消息"
        assert "继续" in _msg_text(messages[-1]), f"新用户消息应为「继续」: {messages[-1]!r}"
        prev = messages[-2]
        assert prev["role"] == "user" and any(
            block.get("type") == "tool_result" for block in prev["content"]
        ), "held 工具结果应紧邻新用户消息之前"
        prev_text = json.dumps(prev, ensure_ascii=False)
        assert "round-5" in prev_text, f"最后一个工具结果应为第 5 轮输出: {prev_text!r}"
    finally:
        app.close()

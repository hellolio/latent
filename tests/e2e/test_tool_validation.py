"""T14:工具参数 schema 校验——非法 arguments 转错误 toolResult 反馈给 LLM。"""

import json

from harness import RpiApp, load_scenario


def test_invalid_tool_arguments_become_error_tool_result():
    app = RpiApp(turns=load_scenario("tool_invalid_args"))
    try:
        app.wait_ready()
        # mock 发的 read 调用缺 required 参数 `path`
        app.sendline("读一个文件")
        app.expect_text("参数错误已反馈")

        bodies = app.wait_for_requests(2)
        last = bodies[1]["messages"][-1]
        assert last["role"] == "user", "错误 toolResult 应作为 user 消息回传"
        assert "Invalid arguments" in json.dumps(last, ensure_ascii=False), \
            "第二轮应包含参数校验错误 toolResult"
    finally:
        app.close()

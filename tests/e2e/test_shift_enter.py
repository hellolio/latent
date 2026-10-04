"""E2E 场景 3:多行输入——Shift+Enter(CSI u)与 Ctrl+J 在编辑器内换行不提交。

模拟 kitty keyboard protocol 终端:直接向 PTY 注入 `\x1b[13;2u`
(crossterm 解析为 Enter+SHIFT → Key::ShiftEnter),反向断言 mock LLM
收到的请求体包含换行合并后的完整消息。
"""

import pytest

from harness import LatentApp, load_scenario


def last_user_text(body: dict) -> str:
    """取请求体最后一条 user 消息的文本(兼容 block 列表与纯字符串)。"""
    msgs = [m for m in body["messages"] if m.get("role") == "user"]
    content = msgs[-1]["content"]
    if isinstance(content, list):
        return "".join(
            b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"
        )
    return content


def test_shift_enter_inserts_newline_without_submitting():
    app = LatentApp(turns=load_scenario("shift_enter"))
    try:
        app.wait_ready()
        # Shift+Enter = CSI 13;2u(协议终端上报形式)
        app.type_text("first")
        app.child.send("\x1b[13;2u")
        app.type_text("second")
        # 两行都应上屏,且此时未提交(mock 尚未收到请求)
        app.expect_text(r"first")
        app.expect_text(r"second")
        app.expect_absent("已收到两行消息", after_idle=0.8)
        # Enter 提交,两行合并为一条消息
        app.child.send("\r")
        app.expect_text(r"已收到两行消息")
        bodies = app.wait_for_requests(1)
        assert last_user_text(bodies[0]) == "first\nsecond", "请求体应包含换行合并的消息"
    finally:
        app.close()


def test_ctrl_j_inserts_newline_without_submitting():
    app = LatentApp(turns=load_scenario("shift_enter"))
    try:
        app.wait_ready()
        # Ctrl+J = \x0a(任意终端可用的换行兜底)
        app.type_text("alpha")
        app.child.send("\x0a")
        app.type_text("beta")
        app.expect_text(r"alpha")
        app.expect_text(r"beta")
        app.expect_absent("已收到两行消息", after_idle=0.8)
        app.child.send("\r")
        app.expect_text(r"已收到两行消息")
        bodies = app.wait_for_requests(1)
        assert last_user_text(bodies[0]) == "alpha\nbeta", "请求体应包含换行合并的消息"
    finally:
        app.close()

"""E2E 场景 7:模型输出显示完整性。

流式回复结束后:正文定稿上屏、用量行出现、footer token 段(含 cache 段)
右下角可见、编辑器回到可输入状态;提交进 scrollback 的 CJK 文本不注入
字间空格(ratatui insert_before 宽字符空位 bug 的回归测试)。
"""

import re
import time

from harness import RpiApp, load_scenario, strip_ansi


def test_reply_commits_with_usage_and_footer():
    app = RpiApp(turns=load_scenario("ask_and_reply"))
    try:
        app.wait_ready()
        app.sendline("你好")
        # 流式回复上屏
        app.expect_text(r"这是 mock LLM 的固定回复")
        # 回合结束后:用量行 + footer token 段(cache 0% 也要显示)
        app.expect_text(r"\[tokens\]")
        app.expect_text(r"cache 0%")
        app.expect_text(r"↑")
        # 编辑器回到可输入状态
        app.expect_text(r"❯")
    finally:
        app.close()


def test_cjk_committed_without_injected_spaces():
    app = RpiApp(turns=[{"text": "AB中文CD测试"}], timeout=15.0)
    try:
        app.wait_ready()
        app.sendline("测试中文消息")
        app.expect_text("AB中文CD测试")
        time.sleep(0.8)
        # 定位带背景色的用户块原始字节,去掉 ANSI 后必须是连续文本。
        # 此前的 bug:insert_before 把宽字符后的空位 cell 打成真实空格,
        # 提交内容变成 "测 试 中 文 消 息"。
        raw = "".join(app._raw)
        match = re.search(r"48;2;\d+;\d+;\d+", raw)
        assert match, "用户消息块应带背景色"
        segment = raw[match.start() : match.start() + 800]
        visible = strip_ansi(segment)
        assert "测试中文消息" in visible, f"提交的 CJK 文本不应有字间空格: {visible!r}"
    finally:
        app.close()

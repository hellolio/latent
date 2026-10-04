"""E2E 场景 7:模型输出显示完整性。

流式回复结束后:正文定稿上屏、用量行出现、footer token 段(含 cache 段)
右下角可见、编辑器回到可输入状态;提交进 scrollback 的 CJK 文本不注入
字间空格(全帧差分渲染路径的宽字符回归);输出与输入框之间恒定两行间隔;
工具命令本身完整显示(超长命令折多行,不受 ctrl+o 折叠影响)。
"""

import re
import time

from harness import LatentApp, load_scenario, strip_ansi


def test_reply_commits_with_usage_and_footer():
    app = LatentApp(turns=load_scenario("ask_and_reply"))
    try:
        app.wait_ready()
        app.sendline("你好")
        # 流式回复上屏
        app.expect_text(r"这是 mock LLM 的固定回复")
        # 回合结束后:用量行(裸行无外框;mock 无缓存读写,↑ = 裸 input)
        # + footer token 段
        app.expect_text(r"↓7")
        app.expect_text(r"↑10")
        app.expect_text(r"↑")
        # 编辑器回到可输入状态
        app.expect_text("Ask latent to do anything")
    finally:
        app.close()


def _screen_rows(app) -> list[str]:
    return [
        "".join(app.screen.buffer[y][x].data for x in range(app.screen.columns))
        for y in range(app.screen.lines)
    ]


def _row_has_bg(app, y: int) -> bool:
    return any(
        app.screen.buffer[y][x].bg not in (None, "default")
        for x in range(app.screen.columns)
    )


def _row_is_blank(app, y: int) -> bool:
    return all(
        app.screen.buffer[y][x].data in ("", " ")
        and app.screen.buffer[y][x].bg in (None, "default")
        for x in range(app.screen.columns)
    )


def test_two_blank_lines_between_output_and_editor():
    """回合结束后,最近的模型输出(用量行)与输入框之间有间隔空行。

    用量行上下各带一行间隔(避免与其他内容挤在一起),加上输出→输入框
    的固定两行空行,用量行下方共 3 行纯空行。
    """
    app = LatentApp(turns=load_scenario("ask_and_reply"))
    try:
        app.wait_ready()
        app.sendline("你好")
        app.expect_text("这是 mock LLM 的固定回复")
        app.expect_text("↓7")
        app.expect_text("Ask latent to do anything")
        time.sleep(0.5)
        rows = _screen_rows(app)
        editor = max(i for i, row in enumerate(rows) if "Ask latent to do anything" in row)
        # 占位文本行上方是编辑器背景内边距行,再往上是固定两行空行 + 用量行
        # 自带的下方间隔空行(共 3 行纯空行),空行之上就是用量行
        assert _row_has_bg(app, editor - 1), "占位文本上方应是编辑器内边距行"
        assert _row_is_blank(app, editor - 2), "间隔第 1 行应为纯空行"
        assert _row_is_blank(app, editor - 3), "间隔第 2 行应为纯空行"
        assert _row_is_blank(app, editor - 4), "用量行下方间隔应为纯空行"
        assert not _row_is_blank(app, editor - 5), "间隔之上应紧贴用量行"
    finally:
        app.close()


def test_tool_command_fully_displayed_in_card():
    """超长工具命令完整折行显示(多行,不因收起而截断)。"""
    command = (
        "true # AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKKLLLLMMMMNNNNOOOO"
        "PPPPQQQQRRRRSSSSTTTTUUUUVVVVWWWWXXXXYYYYZZZZ"
    )
    app = LatentApp(turns=load_scenario("long_command"))
    try:
        app.wait_ready()
        app.sendline("执行一下")
        app.expect_text("⏺ bash")
        app.expect_text("命令执行完毕")
        time.sleep(0.5)
        rows = [row.rstrip() for row in _screen_rows(app)]
        # 卡片命令折成多行:完整命令可在屏幕上找到,且尾部落在 ⏺ 标题行
        # 之外的续行上(单行截断的话尾部不会出现)
        title = next(i for i, row in enumerate(rows) if "⏺ bash" in row)
        squeezed = "".join(rows).replace(" ", "")
        assert squeezed.count(command.replace(" ", "")) >= 1, f"命令应完整显示: {rows}"
        continuation = "".join(rows[title + 1 : title + 3]).replace(" ", "")
        assert "YYYYZZZZ" in continuation, f"命令应折到标题行之外的续行: {rows}"
    finally:
        app.close()


def test_cjk_committed_without_injected_spaces():
    app = LatentApp(turns=[{"text": "AB中文CD测试"}], timeout=15.0)
    try:
        app.wait_ready()
        app.sendline("测试中文消息")
        app.expect_text("AB中文CD测试")
        time.sleep(0.8)
        # 定位带背景色的用户块原始字节,去掉 ANSI 后必须是连续文本。
        # 此前的 bug:insert_before 把宽字符后的空位 cell 打成真实空格,
        # 提交内容变成 "测 试 中 文 消 息"。
        # 布局调整后编辑器自身也带背景色,逐个背景色段检查,
        # 任一段里提交文本保持连续即通过。
        raw = "".join(app._raw)
        matches = list(re.finditer(r"48;2;\d+;\d+;\d+", raw))
        assert matches, "用户消息块应带背景色"
        found = False
        last_visible = ""
        for match in matches:
            visible = strip_ansi(raw[match.start() : match.start() + 800])
            if "测试中文消息" in visible:
                found = True
                break
            last_visible = visible
        assert found, f"提交的 CJK 文本不应有字间空格: {last_visible!r}"
    finally:
        app.close()

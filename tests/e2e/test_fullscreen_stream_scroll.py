"""全屏模式流式滚动——模型正在输出时上滚,正在输出的内容随滚动移动。

修复的行为:此前滚动只作用于已提交历史,流式预览钉在输入框上方且只显示
最新几行(更早的流式输出在流式期间根本看不到);现在非 follow 视口覆盖
committed + 流式预览全量内容,输入框以上除 chrome(状态行/编辑器/footer)
外全部跟随滚动,视口冻结语义不变(新输出在视口下方堆积,不拉走屏幕)。
"""

import time

from harness import LatentApp, load_scenario


def test_fullscreen_scroll_during_streaming():
    app = LatentApp(
        turns=load_scenario("fullscreen_stream_scroll"), timeout=30.0, rows=30, cols=100
    )
    try:
        app.wait_ready()
        # mock 按 90ms/delta 节奏流式输出 40 行(约 18s),留足断言窗口
        app.sendline("边流式边滚动")
        app.expect_text("LN20")
        time.sleep(0.3)
        visible = app.visible_text()
        assert "LN20" in visible, f"最新流式内容应可见:\n{visible[-800:]}"
        assert "LN01" not in visible, "早期流式内容应仍在预览窗口之外"

        # 流式进行中 PageUp:正在输出的内容并入滚动视口,早期输出行可见
        app.send_key("pageup")
        app.expect_visible("LN01")
        assert "Ask latent to do anything" in app.visible_text(), "输入框应保持钉底"

        # 视口冻结:断言窗口内流式继续推进(LN30 之后的内容陆续到达),
        # 视口不被新内容拉走,最新输出不可见
        app.expect_absent("LN30")

        # End:恢复 follow,最新内容回到视口(流式尾窗继续刷新至定稿)
        app.send_key("end")
        app.expect_text("LN40")
    finally:
        app.close()

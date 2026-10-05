"""T26:全屏模式(alternate screen,默认)——输入区钉底、屏幕内滚动、/fullscreen 切换。

pi 同款行为:
- 默认进入全屏:历史内容在屏幕内滚动,编辑器/footer 始终钉在底部;
- PageUp 翻页查看早期内容,End 回到最新(follow);
- 上滚后新回复到达不回跳底部(视口冻结);
- /fullscreen 切回 regular(内容重打进主屏 scrollback),再切回全屏。
"""

import time

from harness import LatentApp, load_scenario


def test_fullscreen_scroll_and_pinned_editor():
    app = LatentApp(turns=load_scenario("fullscreen"), timeout=20.0, rows=30, cols=100)
    try:
        app.wait_ready()
        # 第一轮:40 行长回复,超出可视历史窗口(30 行屏)
        app.sendline("长回复")
        app.expect_text("scroll-line-40")
        time.sleep(1.0)
        visible = app.visible_text()
        assert "scroll-line-40" in visible, f"最新回复应可见:\n{visible[-800:]}"
        assert "scroll-line-01" not in visible, "早期行应滚出可视窗口"
        # 编辑器/footer 钉底:占位文本仍在屏
        assert "Ask latent to do anything" in visible

        # PageUp:翻回上一页,早期行进入屏幕;输入区仍钉在底部
        app.send_key("pageup")
        app.expect_visible("scroll-line-05")
        assert "Ask latent to do anything" in app.visible_text()

        # End:回到最新内容(follow 恢复)
        app.send_key("end")
        time.sleep(0.5)
        visible = app.visible_text()
        assert "scroll-line-40" in visible, f"End 应回到最新:\n{visible[-800:]}"

        # 上滚后新回复到达:视口冻结,不回跳底部。流式预览已并入滚动视口
        # (不再钉在输入框上方),新回复在视口下方堆积、暂不渲染到屏幕
        app.send_key("pageup")
        app.expect_visible("scroll-line-05")
        app.sendline("第二条")
        time.sleep(2.0)
        visible = app.visible_text()
        assert "scroll-line-05" in visible, "上滚位置应保持,不被新内容拉走"
        assert "second-marker" not in visible, "新回复不应拉走视口(应在视口下方堆积)"

        # 回到底部:新回复随 follow 恢复回到视口
        app.send_key("end")
        app.expect_visible("second-marker")
    finally:
        app.close()


def test_fullscreen_toggle_to_regular_and_back():
    app = LatentApp(turns=load_scenario("fullscreen"), timeout=20.0, rows=30, cols=100)
    try:
        app.wait_ready()
        app.sendline("长回复")
        app.expect_text("scroll-line-40")
        time.sleep(1.0)

        # /setting:设置选择器(全屏模式 / 复制快捷键 / 鼠标选中复制)
        app.sendline("/setting")
        app.expect_text("全屏模式")
        assert "复制快捷键" in app.transcript()
        assert "鼠标选中复制" in app.transcript()
        # 第一项即全屏模式,Enter 切到 regular
        app.send_key("enter")
        app.expect_text("TUI → regular")
        time.sleep(1.0)
        visible = app.visible_text()
        assert "scroll-line-40" in visible, f"切回 regular 后内容应重打:\n{visible[-800:]}"

        # 再次 /setting → Enter:切回全屏,恢复屏幕内滚动,输入区钉底
        app.sendline("/setting")
        app.expect_text("全屏模式")
        app.send_key("enter")
        app.expect_text("TUI → fullscreen")
        time.sleep(1.0)
        assert "Ask latent to do anything" in app.visible_text(), "切回全屏后编辑器应钉底"
    finally:
        app.close()

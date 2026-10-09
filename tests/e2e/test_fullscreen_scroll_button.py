"""T27:全屏「跳到底部」药丸(pi scrollToEndIndicator 对应)——上滚出现、
点击恢复 follow、回底消失。

pi 同款行为:
- 全屏 follow 态无药丸;
- PageUp 上滚后,历史视口末行(输入区上沿)居中出现
  "↓ Jump to latest message · End" 药丸;
- 鼠标点击药丸(SGR 序列注入)恢复 follow:视口回底、药丸从屏幕消失;
- regular 模式(终端原生 scrollback)无药丸。
"""

import time

from harness import LatentApp, load_scenario


def find_button(app) -> tuple[int, int] | None:
    """在当前屏幕网格上定位药丸,返回 ↓ 字符的 1 基 SGR 坐标 (col, row)。"""
    for row, line in enumerate(app.screen.display):
        idx = line.find("↓ Jump to latest")
        if idx >= 0:
            return idx + 1, row + 1
    return None


def visible_has(app, text: str) -> bool:
    return text in app.visible_text()


def test_fullscreen_scroll_to_bottom_button():
    app = LatentApp(turns=load_scenario("fullscreen"), timeout=20.0, rows=30, cols=100)
    try:
        app.wait_ready()
        # 长回复铺满历史窗口(30 行屏)
        app.sendline("长回复")
        app.expect_text("scroll-line-40")
        time.sleep(1.0)
        # follow 态屏幕上无药丸
        assert not visible_has(app, "Jump to latest"), "follow 态不应显示药丸"

        # PageUp 上滚:药丸出现在视口末行
        app.send_key("pageup")
        app.expect_visible("Jump to latest")
        assert "Ask latent to do anything" in app.visible_text(), "输入区应仍钉底"

        # 点击药丸(注入 SGR 鼠标按下/抬起;按下即恢复 follow)
        hit = find_button(app)
        assert hit, f"药丸应在屏幕上:\n{app.visible_text()[-800:]}"
        col, row = hit
        app.send(f"\x1b[<0;{col};{row}M")
        time.sleep(0.3)
        app.send(f"\x1b[<0;{col};{row}m")
        # 回到底部:药丸从当前屏幕消失、最新内容可见(注意:只能断言当前
        # 屏幕,药丸出现过的痕迹仍留在累计输出里)
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline and visible_has(app, "Jump to latest"):
            time.sleep(0.1)
        assert not visible_has(app, "Jump to latest"), f"点击后药丸应消失:\n{app.visible_text()[-800:]}"
        assert visible_has(app, "scroll-line-40"), f"点击后应回到最新:\n{app.visible_text()[-800:]}"
    finally:
        app.close()


def test_fullscreen_scroll_button_absent_in_regular_mode():
    app = LatentApp(turns=load_scenario("fullscreen"), timeout=20.0, rows=30, cols=100)
    try:
        app.wait_ready()
        # 切 regular(/setting 第一项 = 全屏模式):终端 scrollback 接管,
        # 无屏幕内滚动也就无药丸
        app.sendline("/setting")
        app.expect_text("全屏模式")
        app.send_key("enter")
        app.expect_text("TUI → regular")
        # 面板保持打开(Enter 切换 · Esc 关闭):先关面板再输入
        app.send_key("esc")
        time.sleep(0.5)
        app.sendline("长回复")
        app.expect_text("scroll-line-40")
        time.sleep(1.0)
        assert not visible_has(app, "Jump to latest"), f"regular 模式不应显示药丸:\n{app.visible_text()[-800:]}"
    finally:
        app.close()

"""T18:Ctrl+O 折叠/展开工具卡片——折叠态隐藏早期输出行,展开后可见。"""

import time

from harness import RpiApp, load_scenario


def test_ctrl_o_expands_and_collapses_tool_card():
    app = RpiApp(turns=load_scenario("ctrl_o"), timeout=20.0, rows=40, cols=110)
    try:
        app.wait_ready()
        app.sendline("输出 20 行")
        # 工具卡片已出现(折叠态)
        app.expect_text("for i in")
        time.sleep(1.0)
        # 折叠态:尾部行不可见(当前屏,非累计转录)
        assert "output-line-20" not in app.visible_text(), "折叠态不应显示输出尾部"

        # 展开:Ctrl+O 全文重绘,尾部行可见
        app.send_key("ctrl+o")
        app.expect_text("output-line-20")
        assert "output-line-20" in app.visible_text()

        # 再按折叠:全文重绘回折叠态
        app.send_key("ctrl+o")
        time.sleep(1.0)
        assert "output-line-20" not in app.visible_text(), "再次 Ctrl+O 应回到折叠态"
        app.expect_text("折叠测试完成")
    finally:
        app.close()

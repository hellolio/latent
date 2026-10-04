"""E2E 场景 6:主题系统——/theme 列表、/theme 切换、切换后 UI 仍正常工作。

断言面向"人读到的文本"(颜色变化不可靠断言,这里验证命令行为与
交互不回归;配色本身由 Rust 单测覆盖)。
"""

from harness import LatentApp, load_scenario


def test_theme_command_lists_available_themes():
    app = LatentApp(turns=[])
    try:
        app.wait_ready()
        app.sendline("/theme")
        # 无参数:打开选择列表(预览区),应能看到主题名与提示
        app.expect_text("选择主题")
        app.expect_text("Tokyo Night")
        app.expect_text("Dracula")
        app.expect_text("Catppuccin Mocha")
        # Esc 关闭列表,输入通道恢复:带参切换仍然可用
        app.send_key("esc")
        app.sendline("/theme nord")
        app.expect_text("theme → Nord")
    finally:
        app.close()


def test_theme_switch_by_name_and_ui_keeps_working():
    app = LatentApp(turns=load_scenario("ask_and_reply"))
    try:
        app.wait_ready()
        app.sendline("/theme nord")
        # 确认行上屏(进入转录,ctrl+o 重绘不丢)
        app.expect_text("theme → Nord")
        # 未知名字:本地报错,不崩溃
        app.sendline("/theme no-such-theme")
        app.expect_text("未知主题: no-such-theme")
        # 切换后核心用户流程不受影响
        app.sendline("你好,请介绍一下你自己")
        app.expect_text(r"这是 mock LLM 的固定回复")
        app.wait_for_requests(1)
    finally:
        app.close()


def test_theme_flag_selects_startup_theme():
    app = LatentApp(turns=[], extra_args=["--theme", "nord"])
    try:
        app.wait_ready()
        # 启动即应用主题:无报错、UI 正常
        app.expect_absent("未知主题")
        app.expect_text("Ask latent to do anything")
        app.sendline("/theme")
        app.expect_text("选择主题")
    finally:
        app.close()

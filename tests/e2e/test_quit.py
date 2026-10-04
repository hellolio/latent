"""E2E 场景 5:空输入按 Ctrl+D 退出,进程应干净结束(退出码 0)。"""

from harness import LatentApp


def test_ctrl_d_exits_cleanly():
    app = LatentApp(turns=[])
    try:
        app.wait_ready()
        app.quit()
        assert app.exit_status == 0, f"退出码应为 0,实际 {app.exit_status}"
    finally:
        app.close()

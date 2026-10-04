"""E2E 场景 1:启动 interactive 模式,用户应该看到横幅、快捷键提示与 footer。

验证的是"人打开 latent 第一眼"的体验,不涉及 LLM 请求。
"""

from harness import LatentApp


def test_startup_shows_banner_and_footer():
    app = LatentApp(turns=[])
    try:
        app.expect_text(r"latent v\d+\.\d+")
        # 快捷键提示行(折叠态 header)
        app.expect_text(r"interrupt")
        app.expect_text(r"ctrl\+o")
        # footer 第一行显示 cwd(workdir 尾段)
        tail = app.workdir.rstrip("/").split("/")[-1]
        app.expect_text(tail)
        # footer 第三行右对齐显示模型(provider/model)
        app.expect_text("e2e-model")
    finally:
        app.close()


def test_startup_makes_no_llm_request():
    app = LatentApp(turns=[])
    try:
        app.wait_ready()
        assert len(app.request_bodies()) == 0, "启动阶段不应请求 LLM"
    finally:
        app.close()
